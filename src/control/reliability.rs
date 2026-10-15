//! Connection-scoped CONTROL reliability

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fmt,
    future::pending,
    time::Duration,
};

use futures_util::{Sink, SinkExt};
use openssl::sha::sha256;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::{Error as WsError, Message, protocol::WebSocketConfig};

use super::messages::ControlMessage;

pub(super) type ControlResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

// Initial policy, not measured capacity or latency guarantees. Both endpoints
// must agree on the ID window; it is not negotiated by the current wire format.
pub(super) const WINDOW: usize = 32;
pub(super) const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub(super) const ACK_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReliabilityError {
    WindowFull,
    PendingBytesFull,
    MessageTooLarge,
    IdExhausted,
    Closed,
    WriteTimedOut,
    RetryExhausted {
        message_id: u64,
        attempts: u8,
    },
    ConnectionClosed {
        pending: usize,
    },
    InvalidSequence {
        message_id: u64,
        expected: Option<u64>,
    },
    ConflictingDuplicate {
        message_id: u64,
    },
    RetiredMessage {
        message_id: u64,
    },
}

impl fmt::Display for ReliabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowFull => f.write_str("CONTROL ID window is full; wait for acknowledgment"),
            Self::PendingBytesFull => f.write_str("CONTROL pending serialized-data budget is full"),
            Self::MessageTooLarge => f.write_str("serialized CONTROL message exceeds 1 MiB"),
            Self::IdExhausted => f.write_str("CONTROL message IDs exhausted; IDs cannot wrap"),
            Self::Closed => f.write_str("CONTROL reliability state is closed"),
            Self::WriteTimedOut => f.write_str("CONTROL write timed out; delivery is unconfirmed"),
            Self::RetryExhausted {
                message_id,
                attempts,
            } => write!(
                f,
                "CONTROL {message_id} delivery unconfirmed after {attempts} attempts"
            ),
            Self::ConnectionClosed { pending } => write!(
                f,
                "CONTROL connection closed with {pending} unconfirmed messages"
            ),
            Self::InvalidSequence {
                message_id,
                expected,
            } => write!(f, "invalid CONTROL ID {message_id}; expected {expected:?}"),
            Self::ConflictingDuplicate { message_id } => {
                write!(f, "CONTROL ID {message_id} reused with different content")
            }
            Self::RetiredMessage { message_id } => {
                write!(f, "CONTROL ID {message_id} is outside retained history")
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

pub(super) fn encode(message: &ControlMessage) -> ControlResult<String> {
    let text = serde_json::to_string(message)?;
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(ReliabilityError::MessageTooLarge.into());
    }
    Ok(text)
}

pub(super) async fn write_text<S>(write: &mut S, text: String) -> ControlResult<()>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    timeout(WRITE_TIMEOUT, write.send(Message::Text(text)))
        .await
        .map_err(|_| ReliabilityError::WriteTimedOut)??;
    Ok(())
}

/// Tests can advance ACK deadlines explicitly, including over real `WebSockets`.
/// Write timeout tests use Tokio's paused time separately.
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

struct PendingMessage {
    text: String,
    attempts: u8,
    deadline: Option<Instant>,
}

pub(super) struct ReliableSender {
    next_id: Option<u64>,
    pending: BTreeMap<u64, PendingMessage>,
    pending_bytes: usize,
    closed: bool,
}

impl Default for ReliableSender {
    fn default() -> Self {
        Self {
            next_id: Some(1),
            pending: BTreeMap::new(),
            pending_bytes: 0,
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

    fn admit(&mut self, payload: &str) -> ControlResult<u64> {
        self.ensure_open()?;
        let id = self.next_id.ok_or(ReliabilityError::IdExhausted)?;
        if self.pending.len() == WINDOW
            || self
                .pending
                .first_key_value()
                .is_some_and(|(&oldest, _)| id - oldest >= WINDOW as u64)
        {
            return Err(ReliabilityError::WindowFull.into());
        }
        // Avoid serializing arbitrarily large local input just to reject it.
        if payload.len() > MAX_MESSAGE_BYTES {
            return Err(ReliabilityError::MessageTooLarge.into());
        }
        let text = encode(&ControlMessage::ControlMsg {
            message_id: id,
            payload: payload.into(),
        })?;
        if text.len() > MAX_PENDING_BYTES - self.pending_bytes {
            return Err(ReliabilityError::PendingBytesFull.into());
        }
        self.pending_bytes += text.len();
        self.pending.insert(
            id,
            PendingMessage {
                text,
                attempts: 0,
                deadline: None,
            },
        );
        self.next_id = id.checked_add(1);
        Ok(id)
    }

    pub(super) async fn send<S>(
        &mut self,
        write: &mut S,
        payload: &str,
        clock: &Clock,
    ) -> ControlResult<u64>
    where
        S: Sink<Message, Error = WsError> + Unpin,
    {
        let id = self.admit(payload)?;
        self.transmit(write, id, clock).await?;
        Ok(id)
    }

    async fn transmit<S>(&mut self, write: &mut S, id: u64, clock: &Clock) -> ControlResult<()>
    where
        S: Sink<Message, Error = WsError> + Unpin,
    {
        let mut guard = SenderGuard::new(self);
        let message = guard
            .sender
            .pending
            .get_mut(&id)
            .ok_or(ReliabilityError::Closed)?;
        write_text(write, message.text.clone()).await?;
        message.attempts += 1;
        // Start the ACK deadline after the write completes, not during a stall.
        message.deadline = Some(clock.now() + ACK_TIMEOUT);
        guard.complete();
        Ok(())
    }

    pub(super) async fn retry_due<S>(
        &mut self,
        write: &mut S,
        clock: &Clock,
    ) -> ControlResult<usize>
    where
        S: Sink<Message, Error = WsError> + Unpin,
    {
        self.ensure_open()?;
        let now = clock.now();
        let due: Vec<u64> = self
            .pending
            .iter()
            .filter_map(|(&id, message)| {
                message
                    .deadline
                    .filter(|&deadline| deadline <= now)
                    .map(|_| id)
            })
            .collect();
        for &id in &due {
            if self
                .pending
                .get(&id)
                .is_some_and(|message| message.attempts == MAX_ATTEMPTS)
            {
                self.close();
                return Err(ReliabilityError::RetryExhausted {
                    message_id: id,
                    attempts: MAX_ATTEMPTS,
                }
                .into());
            }
        }
        for &id in &due {
            self.transmit(write, id, clock).await?;
        }
        Ok(due.len())
    }

    pub(super) fn acknowledge(&mut self, id: u64) -> bool {
        if let Some(message) = self.pending.remove(&id) {
            self.pending_bytes -= message.text.len();
            true
        } else {
            false
        }
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter_map(|message| message.deadline)
            .min()
    }

    pub(super) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn close(&mut self) {
        self.pending.clear();
        self.pending_bytes = 0;
        self.closed = true;
    }
}

/// Dropping an in-progress write/read operation terminates its reliability
/// state: a cancelled write may have partially reached the WebSocket sink.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Receipt {
    New,
    Duplicate,
}

#[derive(Default)]
pub(super) struct ReliableReceiver {
    highest: u64,
    recent: VecDeque<(u64, [u8; 32])>,
}

impl ReliableReceiver {
    // Processing and committing a New receipt must be synchronous in the
    // connection owner before awaiting an ACK write. This is not durable state.
    pub(super) fn classify(&self, id: u64, payload: &str) -> Result<Receipt, ReliabilityError> {
        let expected = self.highest.checked_add(1);
        if Some(id) == expected {
            return Ok(Receipt::New);
        }
        if id == 0 || id > self.highest {
            return Err(ReliabilityError::InvalidSequence {
                message_id: id,
                expected,
            });
        }
        let (_, fingerprint) = self
            .recent
            .iter()
            .find(|(seen, _)| *seen == id)
            .ok_or(ReliabilityError::RetiredMessage { message_id: id })?;
        if *fingerprint != sha256(payload.as_bytes()) {
            return Err(ReliabilityError::ConflictingDuplicate { message_id: id });
        }
        Ok(Receipt::Duplicate)
    }

    pub(super) fn commit(&mut self, id: u64, payload: &str) {
        self.highest = id;
        if self.recent.len() == WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back((id, sha256(payload.as_bytes())));
    }
}

#[cfg(test)]
mod tests;
