use super::control_text::{MSG_DATA, MSG_RELAY_DATA};

// Keep one byte spare in the existing 2,048-byte UDP receive buffers: a raw
// oversized/truncated datagram must not look like a valid complete carrier.
pub(crate) const MAX_ROUTED_DATA_BYTES: usize = 2047;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Data<'a> {
    pub sender: &'a str,
    pub payload: &'a str,
}

fn token(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_whitespace)
}

fn carrier(raw: &str) -> Option<&str> {
    if raw.len() > MAX_ROUTED_DATA_BYTES {
        return None;
    }
    let line = raw.strip_suffix('\n').unwrap_or(raw);
    (!line.contains(['\r', '\n'])).then_some(line)
}

fn field<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let (value, tail) = rest.split_once(' ')?;
    if !token(value) {
        return None;
    }
    *rest = tail;
    Some(value)
}

impl<'a> Data<'a> {
    pub(crate) fn parse(raw: &'a str) -> Option<Self> {
        let mut rest = carrier(raw)?;
        if field(&mut rest)? != MSG_DATA {
            return None;
        }
        let sender = field(&mut rest)?;
        (!rest.is_empty()).then_some(Self {
            sender,
            payload: rest,
        })
    }
}

pub(crate) struct RelayData<'a> {
    pub data: Data<'a>,
    pub direct: Vec<&'a str>,
}

impl<'a> RelayData<'a> {
    pub(crate) fn parse(raw: &'a str) -> Option<Self> {
        let mut rest = carrier(raw)?;
        if field(&mut rest)? != MSG_RELAY_DATA {
            return None;
        }
        let sender = field(&mut rest)?;
        let count = field(&mut rest)?;
        if !count.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let count = count.parse::<usize>().ok()?;
        // Every listed name requires at least a byte and a separator. Never
        // reserve attacker-supplied capacity or iterate beyond the datagram.
        if count > rest.len() / 2 {
            return None;
        }
        let mut direct = Vec::new();
        for _ in 0..count {
            direct.push(field(&mut rest)?);
        }
        (!rest.is_empty()).then_some(Self {
            data: Data {
                sender,
                payload: rest,
            },
            direct,
        })
    }

    pub(crate) fn encode(&self) -> Option<String> {
        if !token(self.data.sender)
            || self.data.payload.is_empty()
            || self.data.payload.contains(['\r', '\n'])
            || self.direct.iter().any(|name| !token(name))
        {
            return None;
        }
        let mut raw = format!(
            "{MSG_RELAY_DATA} {} {}",
            self.data.sender,
            self.direct.len()
        );
        for name in &self.direct {
            if raw.len().checked_add(name.len())?.checked_add(1)? > MAX_ROUTED_DATA_BYTES {
                return None;
            }
            raw.push(' ');
            raw.push_str(name);
        }
        raw.push(' ');
        // Check before allocating/copying the payload. No truncation or split.
        if raw.len().checked_add(self.data.payload.len())? > MAX_ROUTED_DATA_BYTES {
            return None;
        }
        raw.push_str(self.data.payload);
        Some(raw)
    }
}

#[cfg(test)]
mod tests;
