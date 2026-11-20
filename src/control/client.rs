use std::thread;

use futures_util::{Sink, Stream, StreamExt};
use tokio_tungstenite::{
    WebSocketStream, connect_async_with_config,
    tungstenite::{Error as WsError, Message},
};

use super::{
    messages::{BroadcastReport, ControlMessage, RequestRejection},
    reliability::{
        Clock, Confirmation, ControlResult, ReliabilityError, ReliableSender, SenderGuard,
        Sequence, check_raw_size, close_socket, encode, flush_socket, websocket_config, write_text,
    },
};

#[must_use]
pub struct ControlWorker {
    stopping: tokio::sync::watch::Sender<bool>,
    thread: Option<thread::JoinHandle<()>>,
    commands: tokio::sync::mpsc::Sender<(crate::recovery::wire::SessionId, u32)>,
}

impl ControlWorker {
    #[must_use]
    pub fn fallback(&self, session: crate::recovery::wire::SessionId, target: u32) -> bool {
        if self.commands.try_send((session, target)).is_err() {
            self.stopping.send_replace(true);
            return false;
        }
        true
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }
}

impl Drop for ControlWorker {
    fn drop(&mut self) {
        self.stopping.send_replace(true);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("CONTROL worker panicked during shutdown");
        }
    }
}

#[cfg(test)]
pub fn start_control_client(
    signaling_ip: &str,
    server_id: String,
    channel: String,
    user: String,
    peer_id: u32,
) -> ControlWorker {
    control_worker(
        format!("ws://{signaling_ip}:2133"),
        ControlMessage::JoinControl {
            request_id: 1,
            server_id,
            channel,
            user,
            peer_id,
        },
    )
}

#[cfg(test)]
pub(super) fn control_worker(url: String, join: ControlMessage) -> ControlWorker {
    worker_with_processor(url, join, |_, _| Ok(()))
}

pub fn session_worker(
    url: String,
    binding: crate::recovery::wire::Binding,
    state: std::sync::Arc<std::sync::Mutex<crate::recovery::client::Client>>,
) -> ControlWorker {
    worker_with_processor(
        url,
        ControlMessage::SessionJoin {
            request_id: 1,
            binding: binding.clone(),
        },
        move |_, payload| {
            let snapshot = serde_json::from_str(payload)?;
            let mut client = state.lock().map_err(|_| "transport state poisoned")?;
            client
                .apply_snapshot(&binding, snapshot, std::time::Instant::now())
                .map_err(Into::into)
        },
    )
}

fn worker_with_processor<F>(url: String, join: ControlMessage, process: F) -> ControlWorker
where
    F: FnMut(u64, &str) -> ControlResult<()> + Send + 'static,
{
    let (commands, pending) = tokio::sync::mpsc::channel(super::reliability::WINDOW);
    let (stopping, mut stopped) = tokio::sync::watch::channel(false);
    let thread = thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("Failed to create control client runtime: {e}");
                return;
            }
        };
        rt.block_on(async move {
            tokio::select! {
                biased;
                _ = stopped.wait_for(|stopped| *stopped) => {},
                result = run_control_client(url, join, process, pending) => {
                    if let Err(error) = result { eprintln!("Control client error: {error}"); }
                }
            }
        });
    });
    ControlWorker {
        stopping,
        thread: Some(thread),
        commands,
    }
}

async fn run_control_client<F>(
    url: String,
    join: ControlMessage,
    process: F,
    pending: tokio::sync::mpsc::Receiver<(crate::recovery::wire::SessionId, u32)>,
) -> ControlResult<()>
where
    F: FnMut(u64, &str) -> ControlResult<()>,
{
    let (socket, _) = tokio::time::timeout(
        super::reliability::REGISTRATION_TIMEOUT,
        connect_async_with_config(&url, Some(websocket_config()), false),
    )
    .await??;
    session_with_processor(socket, join, process, Some(pending)).await
}

// Generic transport fixture; production supplies its session-bound handler.
#[cfg(test)]
pub(super) async fn run_control_session<S>(
    socket: WebSocketStream<S>,
    join: ControlMessage,
) -> ControlResult<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    session_with_processor(socket, join, |_, _| Ok(()), None).await
}

async fn session_with_processor<S, F>(
    mut socket: WebSocketStream<S>,
    join: ControlMessage,
    mut process: F,
    pending: Option<tokio::sync::mpsc::Receiver<(crate::recovery::wire::SessionId, u32)>>,
) -> ControlResult<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnMut(u64, &str) -> ControlResult<()>,
{
    let result = async {
        let (mut write, mut read) = (&mut socket).split();
        let mut state = ClientState {
            commands: pending,
            ..ClientState::default()
        };
        let mut clock = Clock::default();
        state.join(&mut write, &join, &clock).await?;
        loop {
            match receive_event(&mut write, &mut read, &mut state, &mut clock, &mut process).await?
            {
                ClientEvent::Closed => return Ok(()),
                ClientEvent::Rejected { reason, .. } => {
                    return Err(format!("CONTROL request rejected: {reason:?}").into());
                }
                _ => {}
            }
        }
    }
    .await;
    let _ = close_socket(&mut socket, result.as_ref().err().map(AsRef::as_ref)).await;
    result
}

#[derive(Default)]
pub(super) struct ClientState {
    pub(super) sender: ReliableSender,
    receiver: Sequence,
    registered: bool,
    commands: Option<tokio::sync::mpsc::Receiver<(crate::recovery::wire::SessionId, u32)>>,
}

impl ClientState {
    pub(super) async fn join<S>(
        &mut self,
        write: &mut S,
        join: &ControlMessage,
        clock: &Clock,
    ) -> ControlResult<u64>
    where
        S: Sink<Message, Error = WsError> + Unpin,
    {
        if !matches!(
            join,
            ControlMessage::JoinControl { .. } | ControlMessage::SessionJoin { .. }
        ) {
            return Err(ReliabilityError::Protocol("expected local registration").into());
        }
        encode(join)?;
        self.sender
            .send(
                write,
                |request_id| {
                    let mut message = join.clone();
                    if let ControlMessage::JoinControl { request_id: id, .. }
                    | ControlMessage::SessionJoin { request_id: id, .. } = &mut message
                    {
                        *id = request_id;
                    }
                    message
                },
                Confirmation::Join,
                None,
                clock,
            )
            .await
    }

    // Used by real WebSocket tests; the CLI currently has no broadcast producer.
    #[allow(dead_code)]
    pub(super) async fn broadcast<S>(
        &mut self,
        write: &mut S,
        payload: &str,
        clock: &Clock,
    ) -> ControlResult<u64>
    where
        S: Sink<Message, Error = WsError> + Unpin,
    {
        self.sender.check_deadlines(clock)?;
        if !self.registered {
            return Err(
                ReliabilityError::Protocol("wait for JoinAccepted before broadcast").into(),
            );
        }
        check_raw_size(payload.len())?;
        self.sender
            .send(
                write,
                |request_id| ControlMessage::Broadcast {
                    request_id,
                    payload: payload.into(),
                },
                Confirmation::Broadcast,
                None,
                clock,
            )
            .await
    }
}

pub(super) async fn send_control_message<S>(
    write: &mut S,
    message: &ControlMessage,
) -> ControlResult<()>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    write_text(write, encode(message)?).await
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClientEvent {
    Joined {
        request_id: u64,
    },
    Broadcast {
        request_id: u64,
        report: BroadcastReport,
    },
    Rejected {
        request_id: u64,
        reason: RequestRejection,
    },
    Received {
        message_id: u64,
    },
    Other,
    Closed,
}

/// One socket-owner transition. Failure/cancellation clears pending state.
pub(super) async fn receive_event<S, R, F>(
    write: &mut S,
    read: &mut R,
    state: &mut ClientState,
    clock: &mut Clock,
    process: &mut F,
) -> ControlResult<ClientEvent>
where
    S: Sink<Message, Error = WsError> + Unpin,
    R: Stream<Item = Result<Message, WsError>> + Unpin,
    F: FnMut(u64, &str) -> ControlResult<()>,
{
    state.sender.check_deadlines(clock)?;
    let mut guard = SenderGuard::new(&mut state.sender);
    let deadline = guard.sender.next_deadline();
    let frame = tokio::select! {
        biased;
        () = clock.wait_until(deadline) => {
            guard.sender.check_deadlines(clock)?;
            return Err(ReliabilityError::Protocol("confirmation timer woke early").into());
        }
        frame = read.next() => frame.transpose()?,
        command = async {
            match &mut state.commands {
                Some(commands) => commands.recv().await,
                None => std::future::pending().await,
            }
        }, if state.registered => {
            let Some((session, target)) = command else { return Err(ReliabilityError::Closed.into()); };
            guard.sender.send(write, |request_id| ControlMessage::Fallback { request_id, session, target },
                Confirmation::Fallback, None, clock).await?;
            guard.complete();
            return Ok(ClientEvent::Other);
        }
    };
    let event = match frame {
        None | Some(Message::Close(_)) => {
            let pending = guard.sender.pending_len();
            guard.sender.close();
            if pending != 0 {
                return Err(ReliabilityError::ConnectionClosed { pending }.into());
            }
            ClientEvent::Closed
        }
        Some(Message::Text(text)) => {
            check_raw_size(text.len())?;
            let message = serde_json::from_str(&text)?;
            handle_message(
                write,
                guard.sender,
                &mut state.receiver,
                &mut state.registered,
                message,
                process,
            )
            .await?
        }
        Some(Message::Ping(_)) => {
            flush_socket(write).await?;
            ClientEvent::Other
        }
        Some(Message::Pong(_)) => ClientEvent::Other,
        Some(_) => return Err(ReliabilityError::Protocol("expected JSON text").into()),
    };
    guard.complete();
    Ok(event)
}

async fn handle_message<S, F>(
    write: &mut S,
    sender: &mut ReliableSender,
    receiver: &mut Sequence,
    registered: &mut bool,
    message: ControlMessage,
    process: &mut F,
) -> ControlResult<ClientEvent>
where
    S: Sink<Message, Error = WsError> + Unpin,
    F: FnMut(u64, &str) -> ControlResult<()>,
{
    Ok(match message {
        ControlMessage::JoinAccepted { request_id } => {
            if !sender.confirm(request_id, Confirmation::Join)? {
                return Ok(ClientEvent::Other);
            }
            *registered = true;
            ClientEvent::Joined { request_id }
        }
        ControlMessage::FallbackResult { request_id } => {
            sender.confirm(request_id, Confirmation::Fallback)?;
            ClientEvent::Other
        }
        ControlMessage::BroadcastResult { request_id, report } => {
            if !sender.confirm(request_id, Confirmation::Broadcast)? {
                return Ok(ClientEvent::Other);
            }
            ClientEvent::Broadcast { request_id, report }
        }
        ControlMessage::RequestRejected { request_id, reason } => {
            if !sender.confirm(request_id, Confirmation::Rejection)? {
                return Ok(ClientEvent::Other);
            }
            ClientEvent::Rejected { request_id, reason }
        }
        ControlMessage::RouteSnapshot {
            message_id,
            snapshot,
        } => {
            if !*registered {
                return Err(ReliabilityError::Protocol("snapshot before JoinAccepted").into());
            }
            receiver.accept(message_id)?;
            process(message_id, &serde_json::to_string(&snapshot)?)
                .map_err(|_| ReliabilityError::HandlerFailed)?;
            send_control_message(write, &ControlMessage::ControlAck { message_id }).await?;
            ClientEvent::Received { message_id }
        }
        ControlMessage::ControlMsg {
            message_id,
            payload,
        } => {
            if !*registered {
                return Err(ReliabilityError::Protocol("notification before JoinAccepted").into());
            }
            receiver.accept(message_id)?;
            process(message_id, &payload).map_err(|_| ReliabilityError::HandlerFailed)?;
            send_control_message(write, &ControlMessage::ControlAck { message_id }).await?;
            ClientEvent::Received { message_id }
        }
        _ => return Err(ReliabilityError::Protocol("message in wrong direction").into()),
    })
}
