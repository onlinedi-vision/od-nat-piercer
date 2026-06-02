use crate::proto::packet::{BROADCAST, HEADER_LEN, Header, Kind, decode, encode};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let payload = b"hello";
        let h = Header {
            kind: Kind::Control,
            flags: 0x1224,
            channel_id: 1u64,
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
        assert_eq!(h2.dst_peer_id, h.dst_peer_id);
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
