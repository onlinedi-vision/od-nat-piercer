use std::{io, net::SocketAddr};

use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};

pub const MAX_DATAGRAM: usize = 2047;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Attempt(pub String);

impl Attempt {
    /// # Errors
    /// Fails explicitly if the operating system cannot supply randomness.
    pub fn fresh() -> io::Result<Self> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0; 16];
        OsRng.try_fill_bytes(&mut bytes).map_err(io::Error::other)?;
        // Zero is reserved, without an unbounded randomness retry.
        if bytes == [0; 16] {
            bytes[15] = 1;
        }
        let mut text = String::with_capacity(32);
        for byte in bytes {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 15)]));
        }
        Ok(Self(text))
    }

    #[must_use]
    pub fn valid(&self) -> bool {
        self.0.len() == 32
            && self
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && self.0.bytes().any(|byte| byte != b'0')
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionId {
    pub channel: u64,
    pub peer: u32,
}

impl SessionId {
    #[must_use]
    pub fn valid(self) -> bool {
        self.channel != 0 && self.peer != 0 && self.peer != u32::MAX
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    pub server: String,
    pub channel: String,
    pub user: String,
    pub attempt: Attempt,
    pub symmetric: bool,
}

impl Join {
    #[must_use]
    pub fn valid(&self) -> bool {
        self.attempt.valid()
            && [&self.server, &self.channel, &self.user]
                .iter()
                .all(|value| {
                    !value.is_empty()
                        && value.len() <= 128
                        && !value.chars().any(char::is_whitespace)
                })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub session: SessionId,
    pub join: Join,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Neighbour {
    pub peer: u32,
    pub user: String,
    pub address: SocketAddr,
    pub server_relay: bool,
    pub relay: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub session: SessionId,
    pub revision: u64,
    pub relay: bool,
    pub via_server: bool,
    pub neighbours: Vec<Neighbour>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Datagram {
    Connect {
        join: Join,
    },
    Challenge {
        attempt: Attempt,
        challenge: Attempt,
    },
    Confirm {
        join: Join,
        challenge: Attempt,
    },
    Welcome {
        binding: Binding,
    },
    Rejected {
        attempt: Attempt,
        reason: String,
    },
    Hb {
        session: SessionId,
        sequence: u64,
    },
    HbAck {
        session: SessionId,
        sequence: u64,
    },
    SessionRejected {
        session: SessionId,
    },
    Disconnect {
        session: SessionId,
    },
    // Local effect; the runtime submits this through confirmed CONTROL.
    #[serde(skip)]
    Fallback {
        session: SessionId,
        target: u32,
    },
    Punch {
        session: SessionId,
        target: u32,
    },
    Ping {
        session: SessionId,
        target: u32,
    },
    Pong {
        session: SessionId,
        target: u32,
    },
    Data {
        session: SessionId,
        target: u32,
        payload: String,
    },
    Forward {
        session: SessionId,
        target: u32,
        origin: u32,
        payload: String,
    },
    RelayData {
        session: SessionId,
        origin: u32,
        direct: Vec<u32>,
        payload: String,
    },
    Delivery {
        session: SessionId,
        target: u32,
        payload: String,
    },
}

impl Datagram {
    /// # Errors
    /// Rejects messages whose serialized datagram exceeds the prototype limit.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        if bytes.len() > MAX_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UDP message exceeds 2047 bytes",
            ));
        }
        Ok(bytes)
    }

    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        (bytes.len() <= MAX_DATAGRAM)
            .then(|| serde_json::from_slice(bytes).ok())
            .flatten()
    }
}
