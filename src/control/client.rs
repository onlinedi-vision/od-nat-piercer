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

pub(super) fn control_worker(url: String, join: ControlMessage) -> ControlWorker {
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
                result = run_control_client(url, join) => {
                    if let Err(error) = result { eprintln!("Control client error: {error}"); }
                }
            }
        });
    });
    ControlWorker {
        stopping,
        thread: Some(thread),
    }
}

async fn run_control_client(url: String, join: ControlMessage) -> ControlResult<()> {
    let (socket, _) = connect_async_with_config(&url, Some(websocket_config()), false).await?;
    run_control_session(socket, join).await
}

// Shared by the CLI and localhost tests. No synthetic startup broadcast.
pub(super) async fn run_control_session<S>(
    mut socket: WebSocketStream<S>,
    join: ControlMessage,
) -> ControlResult<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let result = async {
        let (mut write, mut read) = (&mut socket).split();
        let mut state = ClientState::default();
        let mut clock = Clock::default();
        state.join(&mut write, &join, &clock).await?;
        loop {
            match receive_event(
                &mut write,
                &mut read,
                &mut state,
                &mut clock,
                &mut |id, payload| {
                    println!(
                        "Processed server CONTROL message_id={id} payload_len={}",
                        payload.len()
                    );
                    Ok(())
                },
            )
            .await?
            {
                ClientEvent::Closed => return Ok(()),
                ClientEvent::Rejected { reason, .. } => {
                    return Err(format!("CONTROL request rejected: {reason:?}").into());
                }
                ClientEvent::Joined { .. } => println!("CONTROL registration accepted."),
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
        let ControlMessage::JoinControl {
            server_id,
            channel,
            user,
            ..
        } = join
        else {
            return Err(ReliabilityError::Protocol("expected local registration").into());
        };
        check_raw_size(
            server_id
                .len()
                .saturating_add(channel.len())
                .saturating_add(user.len()),
        )?;
        self.sender
            .send(
                write,
                |request_id| {
                    let mut message = join.clone();
                    if let ControlMessage::JoinControl { request_id: id, .. } = &mut message {
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
