use std::{collections::HashMap, sync::Arc};

use futures_util::{
    StreamExt,
    stream::{SplitSink, SplitStream},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc, watch},
    time::Instant,
};
use tokio_tungstenite::{WebSocketStream, accept_async_with_config, tungstenite::Message};

pub use super::recipient::ControlPeer;
use super::{
    client::send_control_message,
    messages::{BroadcastReport, ControlMessage, RequestRejection},
    recipient::Outbound,
    reliability::{
        Clock, Confirmation, ControlResult, REGISTRATION_TIMEOUT, ReliabilityError, ReliableSender,
        Sequence, check_raw_size, close_socket, encode, flush_socket, websocket_config,
    },
};

pub type ControlPeers = Arc<Mutex<HashMap<(String, String), Vec<ControlPeer>>>>;

pub(crate) fn channel_key(server_id: &str, channel: &str) -> (String, String) {
    (server_id.into(), channel.into())
}

#[derive(PartialEq, Eq)]
struct Registration {
    key: (String, String),
    user: String,
    peer_id: u32,
}

impl Registration {
    fn valid(&self) -> bool {
        !self.key.0.is_empty()
            && !self.key.1.is_empty()
            && !self.user.is_empty()
            && self.peer_id != 0
    }
}

/// Starts the control WebSocket server.
///
/// # Errors
/// Returns an error if the server cannot listen or accept a connection.
pub async fn start_control_server(addr: &str, peers: ControlPeers) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    println!("Control WebSocket server listening on {addr}");
    loop {
        let (stream, _) = listener.accept().await?;
        let peers = Arc::clone(&peers);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peers).await {
                eprintln!("control ws connection error: {e}");
            }
        });
    }
}

pub(super) async fn handle_connection(stream: TcpStream, peers: ControlPeers) -> ControlResult<()> {
    handle_connection_with_clock(stream, peers, Clock::default()).await
}

// The production handler and real localhost tests differ only in their clock.
pub(super) async fn handle_connection_with_clock(
    stream: TcpStream,
    peers: ControlPeers,
    clock: Clock,
) -> ControlResult<()> {
    let socket = accept_async_with_config(stream, Some(websocket_config())).await?;
    let mut connection = Connection::new(socket, peers, clock);
    let result = connection.run().await;
    connection.finish(&result).await;
    result
}

struct Connection {
    write: SplitSink<WebSocketStream<TcpStream>, Message>,
    read: SplitStream<WebSocketStream<TcpStream>>,
    recipient: ControlPeer,
    queue: mpsc::Receiver<Outbound>,
    stopping: watch::Receiver<Option<ReliabilityError>>,
    peers: ControlPeers,
    registration: Option<Registration>,
    registration_deadline: Instant,
    sender: ReliableSender,
    requests: Sequence,
    clock: Clock,
}

impl Connection {
    fn new(socket: WebSocketStream<TcpStream>, peers: ControlPeers, clock: Clock) -> Self {
        let (write, read) = socket.split();
        let (recipient, queue, stopping) = ControlPeer::new();
        Self {
            write,
            read,
            recipient,
            queue,
            stopping,
            peers,
            registration: None,
            registration_deadline: clock.now() + REGISTRATION_TIMEOUT,
            sender: ReliableSender::default(),
            requests: Sequence::default(),
            clock,
        }
    }

    async fn run(&mut self) -> ControlResult<()> {
        while let Some(message) = self.next_message().await? {
            match message {
                ControlMessage::JoinControl {
                    request_id,
                    server_id,
                    channel,
                    user,
                    peer_id,
                } => {
                    self.requests.accept(request_id)?;
                    self.register(
                        request_id,
                        Registration {
                            key: channel_key(&server_id, &channel),
                            user,
                            peer_id,
                        },
                    )
                    .await?;
                }
                ControlMessage::Broadcast {
                    request_id,
                    payload,
                } => {
                    self.requests.accept(request_id)?;
                    self.broadcast(request_id, &payload).await?;
                }
                ControlMessage::ControlAck { message_id } => {
                    self.sender.confirm(message_id, Confirmation::Ack)?;
                }
                _ => return Err(ReliabilityError::Protocol("message in wrong direction").into()),
            }
        }
        let pending = self.sender.pending_len() + self.queue.len();
        if pending != 0 {
            return Err(ReliabilityError::ConnectionClosed { pending }.into());
        }
        Ok(())
    }

    fn check_deadlines(&mut self) -> ControlResult<()> {
        if let Some(reason) = *self.stopping.borrow() {
            return Err(reason.into());
        }
        self.sender.check_deadlines(&self.clock)?;
        if self.registration.is_none() && self.clock.now() >= self.registration_deadline {
            return Err(ReliabilityError::RegistrationTimedOut.into());
        }
        Ok(())
    }

    async fn next_message(&mut self) -> ControlResult<Option<ControlMessage>> {
        loop {
            self.check_deadlines()?;
            let deadline = self
                .sender
                .next_deadline()
                .into_iter()
                .chain(
                    self.registration
                        .is_none()
                        .then_some(self.registration_deadline),
                )
                .min();
            tokio::select! {
                _ = self.stopping.changed() => {},
                () = self.clock.wait_until(deadline) => {},
                frame = self.read.next() => {
                    self.check_deadlines()?;
                    match frame.transpose()? {
                        None | Some(Message::Close(_)) => return Ok(None),
                        Some(Message::Text(text)) => {
                            check_raw_size(text.len())?;
                            return Ok(Some(serde_json::from_str(&text)?));
                        }
                        Some(Message::Ping(_)) => {
                            flush_socket(&mut self.write).await?;
                        }
                        Some(Message::Pong(_)) => {},
                        Some(_) => return Err(ReliabilityError::Protocol("expected JSON text").into()),
                    }
                }
                outgoing = self.queue.recv() => {
                    self.check_deadlines()?;
                    let Some(message) = outgoing else { return Ok(None); };
                    self.send_queued(message).await?;
                }
            }
        }
    }

    async fn send_queued(&mut self, message: Outbound) -> ControlResult<()> {
        match message {
            Outbound::Control {
                payload,
                reservation,
            } => {
                self.sender
                    .send(
                        &mut self.write,
                        |message_id| ControlMessage::ControlMsg {
                            message_id,
                            payload: payload.to_string(),
                        },
                        Confirmation::Ack,
                        Some(reservation),
                        &self.clock,
                    )
                    .await?;
            }
            #[cfg(test)]
            Outbound::Inject(message) => {
                super::reliability::write_frame(&mut self.write, message).await?;
            }
        }
        Ok(())
    }

    async fn reject(&mut self, request_id: u64, reason: RequestRejection) -> ControlResult<()> {
        send_control_message(
            &mut self.write,
            &ControlMessage::RequestRejected { request_id, reason },
        )
        .await
    }

    async fn register(&mut self, request_id: u64, next: Registration) -> ControlResult<()> {
        let rejection = if !next.valid() {
            Some(RequestRejection::InvalidRegistration)
        } else if self
            .registration
            .as_ref()
            .is_some_and(|current| current != &next)
        {
            Some(RequestRejection::ConflictingRegistration)
        } else {
            None
        };
        if let Some(reason) = rejection {
            self.reject(request_id, reason).await?;
            return Err(ReliabilityError::Protocol("invalid or conflicting registration").into());
        }
        // Publish only after the result write: notifications cannot overtake it.
        send_control_message(
            &mut self.write,
            &ControlMessage::JoinAccepted { request_id },
        )
        .await?;
        if self.registration.is_none() {
            let mut registry = self.peers.lock().await;
            let list = registry.entry(next.key.clone()).or_default();
            list.retain(|peer| !peer.is_closed());
            list.push(self.recipient.clone());
            self.registration = Some(next);
        }
        Ok(())
    }

    async fn broadcast(&mut self, request_id: u64, payload: &str) -> ControlResult<()> {
        let Some(joined) = &self.registration else {
            return self
                .reject(request_id, RequestRejection::NotRegistered)
                .await;
        };
        // Validate the largest possible recipient envelope before any fanout.
        check_raw_size(payload.len())?;
        if let Err(error) = encode(&ControlMessage::ControlMsg {
            message_id: u64::MAX,
            payload: payload.into(),
        }) {
            if error.downcast_ref::<ReliabilityError>() == Some(&ReliabilityError::MessageTooLarge)
            {
                return self
                    .reject(request_id, RequestRejection::MessageTooLarge)
                    .await;
            }
            return Err(error);
        }
        let report = broadcast(&self.peers, &joined.key, &self.recipient, payload).await;
        send_control_message(
            &mut self.write,
            &ControlMessage::BroadcastResult { request_id, report },
        )
        .await
    }

    async fn finish(self, result: &ControlResult<()>) {
        let Self {
            mut write,
            recipient,
            mut queue,
            peers,
            registration,
            mut sender,
            ..
        } = self;
        recipient.close(ReliabilityError::Closed);
        queue.close();
        drop(queue);
        sender.close();
        if let Some(joined) = registration {
            let mut registry = peers.lock().await;
            if let Some(list) = registry.get_mut(&joined.key) {
                list.retain(|peer| !peer.same_connection(&recipient) && !peer.is_closed());
                if list.is_empty() {
                    registry.remove(&joined.key);
                }
            }
        }
        // No UDP/application membership is referenced here. On cancellation the
        // dropped receiver makes any remaining registry handle inactive.
        let _ = close_socket(&mut write, result.as_ref().err().map(AsRef::as_ref)).await;
    }
}

async fn broadcast(
    peers: &ControlPeers,
    key: &(String, String),
    source: &ControlPeer,
    payload: &str,
) -> BroadcastReport {
    let payload: Arc<str> = payload.into();
    let mut registry = peers.lock().await;
    let mut report = BroadcastReport::default();
    if let Some(list) = registry.get_mut(key) {
        list.retain(|peer| !peer.is_closed());
        for peer in list.iter().filter(|peer| !peer.same_connection(source)) {
            if peer.enqueue(&payload) {
                report.queued += 1;
            } else {
                report.failed += 1;
            }
        }
        list.retain(|peer| !peer.is_closed());
    }
    report
}
