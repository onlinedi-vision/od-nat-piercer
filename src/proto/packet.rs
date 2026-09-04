pub const MAGIC: [u8; 4] = *b"ODNP";
pub const VERSION: u8 = 1;
pub const BROADCAST: u32 = 0xFFFF_FFFF;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Control = 1,
    Dtls = 2,
    Srtp = 3,
}

impl Kind {
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Kind::Control),
            2 => Some(Kind::Dtls),
            3 => Some(Kind::Srtp),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub kind: Kind,
    pub flags: u16,
    pub channel_id: u64,
    pub src_peer_id: u32,
    pub dst_peer_id: u32,
    pub stream_id: u32,
    pub payload_len: u16,
}

pub const HEADER_LEN: usize = 30;

impl Header {
    #[must_use]
    pub fn control(channel_id: u64, src_peer_id: u32, dst_peer_id: u32, payload_len: u16) -> Self {
        Self {
            kind: Kind::Control,
            flags: 0,
            channel_id,
            src_peer_id,
            dst_peer_id,
            stream_id: 0,
            payload_len,
        }
    }

    #[must_use]
    pub fn welcome(channel_id: u64, dst_peer_id: u32, payload_len: u16) -> Self {
        Self::control(channel_id, 0, dst_peer_id, payload_len)
    }
}

#[must_use]
pub fn encode(h: Header, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(h.kind as u8);
    out.extend_from_slice(&h.flags.to_le_bytes());
    out.extend_from_slice(&h.channel_id.to_le_bytes());
    out.extend_from_slice(&h.src_peer_id.to_le_bytes());
    out.extend_from_slice(&h.dst_peer_id.to_le_bytes());
    out.extend_from_slice(&h.stream_id.to_le_bytes());
    let len: u16 = payload.len().try_into().unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload[..len as usize]);
    out
}

#[must_use]
pub fn decode(buf: &[u8]) -> Option<(Header, &[u8])> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    if buf[0..4] != MAGIC {
        return None;
    }
    if buf[4] != VERSION {
        return None;
    }
    let kind = Kind::from_u8(buf[5])?;
    let flags = u16::from_le_bytes([buf[6], buf[7]]);
    let channel_id = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    let src_peer_id = u32::from_le_bytes(buf[16..20].try_into().ok()?);
    let dst_peer_id = u32::from_le_bytes(buf[20..24].try_into().ok()?);
    let stream_id = u32::from_le_bytes(buf[24..28].try_into().ok()?);

    let payload_len_u16 = u16::from_le_bytes([buf[28], buf[29]]);
    let payload_len = usize::from(payload_len_u16);

    let payload_start = HEADER_LEN;
    let payload_end = payload_start.checked_add(payload_len)?;
    if payload_end > buf.len() {
        return None;
    }

    let hdr = Header {
        kind,
        flags,
        channel_id,
        src_peer_id,
        dst_peer_id,
        stream_id,
        payload_len: payload_len_u16,
    };

    Some((hdr, &buf[payload_start..payload_end]))
}
