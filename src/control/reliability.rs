//! Send once, match a bounded confirmation, or fail explicitly.

use std::{collections::BTreeMap, error::Error, fmt, future::pending, time::Duration};

use futures_util::{Sink, SinkExt};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::{Error as WsError, Message, protocol::WebSocketConfig};

use super::{messages::ControlMessage, recipient::CapacityReservation};

pub(super) type ControlResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

// Provisional development policy, not measured production capacity.
pub(super) const WINDOW: usize = 32;
pub(super) const MAX_MESSAGE_BYTES: usize = 8 * 1024;
pub(super) const ACK_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReliabilityError {
    WindowFull,
    MessageTooLarge,
    IdExhausted,
    Closed,
    WriteTimedOut,
    Unconfirmed {
        message_id: u64,
    },
    ConnectionClosed {
        pending: usize,
    },
    InvalidSequence {
        message_id: u64,
        expected: Option<u64>,
    },
    RegistrationTimedOut,
    SlowConsumer,
    Protocol(&'static str),
    HandlerFailed,
}

impl fmt::Display for ReliabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowFull => f.write_str("CONTROL pending confirmation limit reached"),
            Self::MessageTooLarge => f.write_str("serialized CONTROL message exceeds 8 KiB"),
            Self::IdExhausted => f.write_str("CONTROL IDs exhausted; IDs cannot wrap"),
            Self::Closed => f.write_str("CONTROL connection state is closed"),
            Self::WriteTimedOut => f.write_str("CONTROL write timed out; delivery unconfirmed"),
            Self::Unconfirmed { message_id } => {
                write!(
                    f,
                    "CONTROL {message_id} delivery unconfirmed: confirmation timed out"
                )
            }
            Self::ConnectionClosed { pending } => {
                write!(
                    f,
                    "CONTROL connection closed; delivery unconfirmed for {pending} messages"
                )
            }
            Self::InvalidSequence {
                message_id,
                expected,
            } => {
                write!(f, "invalid CONTROL ID {message_id}; expected {expected:?}")
            }
            Self::RegistrationTimedOut => f.write_str("CONTROL registration timed out"),
            Self::SlowConsumer => {
                f.write_str("CONTROL recipient capacity exhausted; delivery unconfirmed")
            }
            Self::Protocol(reason) => write!(f, "CONTROL protocol error: {reason}"),
            Self::HandlerFailed => {
                f.write_str("CONTROL local handler failed; delivery unconfirmed")
            }
        }
    }
}

impl Error for ReliabilityError {}

pub(super) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_MESSAGE_BYTES),
        max_frame_size: Some(MAX_MESSAGE_BYTES),
        ..WebSocketConfig::default()
    }
}

pub(super) fn check_raw_size(bytes: usize) -> Result<(), ReliabilityError> {
    if bytes > MAX_MESSAGE_BYTES {
        Err(ReliabilityError::MessageTooLarge)
    } else {
        Ok(())
    }
}

pub(super) fn encode(message: &ControlMessage) -> ControlResult<String> {
    match message {
        ControlMessage::JoinControl {
            server_id,
            channel,
            user,
            ..
        } => {
            check_raw_size(
                server_id
                    .len()
                    .saturating_add(channel.len())
                    .saturating_add(user.len()),
            )?;
        }
        ControlMessage::Broadcast { payload, .. } | ControlMessage::ControlMsg { payload, .. } => {
            check_raw_size(payload.len())?;
        }
        _ => {}
    }
    let text = serde_json::to_string(message)?;
    check_raw_size(text.len())?;
    Ok(text)
}

pub(super) async fn write_frame<S>(write: &mut S, frame: Message) -> ControlResult<()>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    timeout(WRITE_TIMEOUT, write.send(frame))
        .await
        .map_err(|_| ReliabilityError::WriteTimedOut)??;
    Ok(())
}

pub(super) async fn write_text<S>(write: &mut S, text: String) -> ControlResult<()>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    write_frame(write, Message::Text(text)).await
}

// Tungstenite queues Pong when reading Ping; only its flush is our responsibility.
pub(super) async fn flush_socket<S>(write: &mut S) -> ControlResult<()>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    timeout(WRITE_TIMEOUT, write.flush())
        .await
        .map_err(|_| ReliabilityError::WriteTimedOut)??;
    Ok(())
}

pub(super) async fn close_socket<S>(
    write: &mut S,
    failure: Option<&(dyn Error + Send + Sync + 'static)>,
) -> ControlResult<()>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
    let close = failure.map(|error| CloseFrame {
        code: CloseCode::Policy,
        reason: match error.downcast_ref::<ReliabilityError>() {
            Some(ReliabilityError::HandlerFailed) => "CONTROL handler failed; delivery unconfirmed",
            Some(ReliabilityError::SlowConsumer) => {
                "CONTROL recipient capacity exhausted; delivery unconfirmed"
            }
            Some(ReliabilityError::RegistrationTimedOut) => "CONTROL registration timed out",
            Some(ReliabilityError::Protocol(_) | ReliabilityError::InvalidSequence { .. }) => {
                "CONTROL protocol error; delivery unconfirmed"
            }
            _ => "CONTROL connection failed; delivery unconfirmed",
        }
        .into(),
    });
    timeout(WRITE_TIMEOUT, async {
        // Reading Close may already have queued tungstenite's reply. Even if
        // another Close is rejected, flush that reply before dropping the sink.
        let _ = write.send(Message::Close(close)).await;
        write.close().await
    })
    .await
    .map_err(|_| ReliabilityError::WriteTimedOut)??;
    Ok(())
}

/// Tests advance deadlines explicitly, including over real `WebSockets`.
#[derive(Default)]
pub(super) enum Clock {
    #[default]
    System,
    #[cfg(test)]
    Manual(tokio::sync::watch::Receiver<Instant>),
}

impl Clock {
    pub(super) fn now(&self) -> Instant {
        match self {
            Self::System => Instant::now(),
            #[cfg(test)]
            Self::Manual(time) => *time.borrow(),
        }
    }

    pub(super) async fn wait_until(&mut self, deadline: Option<Instant>) {
        let Some(deadline) = deadline else {
            return pending().await;
        };
        match self {
            Self::System => tokio::time::sleep_until(deadline).await,
            #[cfg(test)]
            Self::Manual(time) => loop {
                if *time.borrow() >= deadline {
                    return;
                }
                if time.changed().await.is_err() {
                    return pending().await;
                }
            },
        }
    }
}

#[derive(Default)]
pub(super) struct Sequence {
    highest: u64,
}

impl Sequence {
    pub(super) fn accept(&mut self, id: u64) -> Result<(), ReliabilityError> {
        let expected = self.highest.checked_add(1);
        if expected.is_none() {
            return Err(ReliabilityError::IdExhausted);
        }
        if Some(id) != expected {
            return Err(ReliabilityError::InvalidSequence {
                message_id: id,
                expected,
            });
        }
        self.highest = id;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Confirmation {
    Join,
    Broadcast,
    Ack,
    Rejection,
}

struct PendingMessage {
    expected: Confirmation,
    deadline: Instant,
    _reservation: Option<CapacityReservation>,
}

pub(super) struct ReliableSender {
    next_id: Option<u64>,
    pending: BTreeMap<u64, PendingMessage>,
    closed: bool,
}

impl Default for ReliableSender {
    fn default() -> Self {
        Self {
            next_id: Some(1),
            pending: BTreeMap::new(),
            closed: false,
        }
    }
}

impl ReliableSender {
    pub(super) fn ensure_open(&self) -> Result<(), ReliabilityError> {
        if self.closed {
            Err(ReliabilityError::Closed)
        } else {
            Ok(())
        }
    }

    pub(super) async fn send<S, F>(
        &mut self,
        write: &mut S,
        make_message: F,
        expected: Confirmation,
        reservation: Option<CapacityReservation>,
        clock: &Clock,
    ) -> ControlResult<u64>
    where
        S: Sink<Message, Error = WsError> + Unpin,
        F: FnOnce(u64) -> ControlMessage,
    {
        self.check_deadlines(clock)?;
        let id = self.next_id.ok_or(ReliabilityError::IdExhausted)?;
        if self.pending.len() == WINDOW {
            return Err(ReliabilityError::WindowFull.into());
        }
        // Local rejection happens before consuming an ID or closing the sender.
        let text = encode(&make_message(id))?;
        self.next_id = id.checked_add(1);
        let mut guard = SenderGuard::new(self);
        write_text(write, text).await?;
        guard.sender.pending.insert(
            id,
            PendingMessage {
                expected,
                deadline: clock.now() + ACK_TIMEOUT,
                _reservation: reservation,
            },
        );
        guard.complete();
        Ok(id)
    }

    pub(super) fn confirm(&mut self, id: u64, actual: Confirmation) -> ControlResult<bool> {
        let Some(message) = self.pending.get(&id) else {
            return Ok(false);
        };
        if actual != message.expected
            && !(actual == Confirmation::Rejection && message.expected != Confirmation::Ack)
        {
            return Err(
                ReliabilityError::Protocol("result type does not match pending request").into(),
            );
        }
        self.pending.remove(&id);
        Ok(true)
    }

    pub(super) fn check_deadlines(&mut self, clock: &Clock) -> ControlResult<()> {
        self.ensure_open()?;
        if let Some((&id, _)) = self.pending.iter().find(|(_, m)| m.deadline <= clock.now()) {
            self.close();
            return Err(ReliabilityError::Unconfirmed { message_id: id }.into());
        }
        Ok(())
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|message| message.deadline).min()
    }

    pub(super) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn close(&mut self) {
        self.pending.clear();
        self.closed = true;
    }
}

/// A cancelled/failed socket transition cannot safely resume the same sender.
pub(super) struct SenderGuard<'a> {
    pub(super) sender: &'a mut ReliableSender,
    completed: bool,
}

impl<'a> SenderGuard<'a> {
    pub(super) fn new(sender: &'a mut ReliableSender) -> Self {
        Self {
            sender,
            completed: false,
        }
    }
    pub(super) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for SenderGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.sender.close();
        }
    }
}

#[cfg(test)]
mod tests;
