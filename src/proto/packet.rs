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

impl Header {
    pub fn control(channel_id: u32, src_peer_id: u32, dst_peer_id: u32, payload_len: u16) -> Self {
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

    pub fn welcome(channel_id: u32, dst_peer_id: u32, payload_len: u16) -> Self {
        Self::control(channel_id, 0, dst_peer_id, payload_len)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let payload = b"hello";
        let h = Header {
            kind: Kind::Control,
            flags: 0x1224,
            channel_id: 1,
            src_peer_id: 2,
            dst_peer_id: BROADCAST,
            stream_id: 3,
            payload_len: payload.len() as u16,
        };

        let buf = encode(h, payload);
        let (h2, p2) = decode(&buf).expect("decode failed");
        assert_eq!(h2.kind as u8, h.kind as u8);
        assert_eq!(h2.flags, h.flags);
        assert_eq!(h2.channel_id, h.channel_id);
        assert_eq!(h2.src_peer_id, h.src_peer_id);
        assert_eq!(h2.stream_id, h.stream_id);
        assert_eq!(p2, payload);
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(b"XXXX");
        assert!(decode(&buf).is_none());
    }

    #[test]
    fn decode_rejects_bad_version() {
        let payload = b"a";
        let h = Header {
            kind: Kind::Control,
            flags: 0x1224,
            channel_id: 1,
            src_peer_id: 2,
            dst_peer_id: BROADCAST,
            stream_id: 3,
            payload_len: payload.len() as u16,
        };
        let mut buf = encode(h, payload);
        buf[4] = 99;
        assert!(decode(&buf).is_none());
    }

    #[test]
    fn kind_constants_table() {
        assert_eq!(Kind::Control as u8, 1);
        assert_eq!(Kind::Dtls as u8, 2);
        assert_eq!(Kind::Srtp as u8, 3);
    }

    #[test]
    fn decode_rejects_truncated_payload() {
        let payload = b"abcd";
        let h = Header {
            kind: Kind::Control,
            flags: 0,
            channel_id: 0,
            src_peer_id: 0,
            dst_peer_id: BROADCAST,
            stream_id: 0,
            payload_len: payload.len() as u16,
        };
        let buf = encode(h, payload);
        let truncated = &buf[..buf.len() - 1];
        assert!(decode(truncated).is_none());
    }
}
