use crate::control::messages::ControlMessage;
use crate::control::server::channel_key;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_key_is_stable_for_same_input() {
        let a = channel_key("Server1", "Channel1");
        let b = channel_key("Server1", "Channel1");
        assert_eq!(a, b);
    }

    #[test]
    fn control_key_differs_for_different_channels() {
        let a = channel_key("Server1", "Channel1");
        let b = channel_key("Server1", "Channel2");
        assert_ne!(a, b);
    }

    #[test]
    fn channel_key_differs_for_different_servers() {
        let a = channel_key("Server1", "Channel1");
        let b = channel_key("Server2", "Channel1");
        assert_ne!(a, b);
    }

    #[test]
    fn join_control_json_roundtrip() {
        let msg = ControlMessage::JoinControl {
            server_id: "Server1".to_string(),
            channel: "Channel1".to_string(),
            user: "User1".to_string(),
            peer_id: 7,
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMessage = serde_json::from_str(&json).unwrap();

        match parsed {
            ControlMessage::JoinControl {
                server_id,
                channel,
                user,
                peer_id,
            } => {
                assert_eq!(server_id, "Server1");
                assert_eq!(channel, "Channel1");
                assert_eq!(user, "User1");
                assert_eq!(peer_id, 7);
            }
            _ => panic!("expected JoinControl"),
        }
    }

    #[test]
    fn control_msg_json_roundtrip() {
        let msg = ControlMessage::ControlMsg {
            message_id: 42,
            payload: "hello".to_string(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMessage = serde_json::from_str(&json).unwrap();

        match parsed {
            ControlMessage::ControlMsg {
                message_id,
                payload,
            } => {
                assert_eq!(message_id, 42);
                assert_eq!(payload, "hello");
            }
            _ => panic!("expected ControlMsg"),
        }
    }

    #[test]
    fn control_ack_json_roundtrip() {
        let msg = ControlMessage::ControlAck { message_id: 99 };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMessage = serde_json::from_str(&json).unwrap();

        match parsed {
            ControlMessage::ControlAck { message_id } => {
                assert_eq!(message_id, 99);
            }
            _ => panic!("expected ControlAck"),
        }
    }

    #[test]
    fn control_msg_large_payload_json_roundtrip() {
        const PAYLOAD_LEN: usize = 100 * 1024;

        let payload = "A".repeat(PAYLOAD_LEN);

        let msg = ControlMessage::ControlMsg {
            message_id: 1,
            payload: payload.clone(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMessage = serde_json::from_str(&json).unwrap();

        match parsed {
            ControlMessage::ControlMsg {
                message_id,
                payload: parsed_payload,
            } => {
                assert_eq!(message_id, 1);
                assert_eq!(parsed_payload.len(), PAYLOAD_LEN);
                assert_eq!(parsed_payload, payload);
            }
            _ => panic!("expected ControlMsg"),
        }
    }
}
