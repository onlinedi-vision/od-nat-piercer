pub const MAGIC: [u8; 4] = *b"ODNP";
pub const VERSION: u8 = 1;
pub const BROADCAST: u32 = 0xFFFF_FFFF;

#[repr(u8)]
// this is a rust attribute: when this enum is reprezented as a number (in memory/when we write is as bytes), use u8
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Control = 1,
    Dtls = 2,
    Srtp = 3,
}

impl Kind {
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
    pub channel_id: u32,
    pub src_peer_id: u32,
    pub dst_peer_id: u32,
    pub stream_id: u32,
    pub payload_len: u16,
}

pub const HEADER_LEN: usize = 26;

pub fn encode(h: Header, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(h.kind as u8);
    out.extend_from_slice(&h.flags.to_le_bytes()); //to_le = to_little_endian - stores the least significant byte first, at the lowest memory address
    out.extend_from_slice(&h.channel_id.to_le_bytes());
    out.extend_from_slice(&h.src_peer_id.to_le_bytes());
    out.extend_from_slice(&h.dst_peer_id.to_le_bytes());
    out.extend_from_slice(&h.stream_id.to_le_bytes());
    let len: u16 = payload.len().try_into().unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload[..len as usize]);
    out
}

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
    let channel_id = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    let src_peer_id = u32::from_le_bytes(buf[12..16].try_into().ok()?);
    let dst_peer_id = u32::from_le_bytes(buf[16..20].try_into().ok()?);
    let stream_id = u32::from_le_bytes(buf[20..24].try_into().ok()?);
    let payload_len = u16::from_le_bytes([buf[24], buf[25]]);
    let payload_len = payload_len as usize;

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
        payload_len: payload_len as u16,
    };

    Some((hdr, &buf[payload_start..payload_end]))
}
