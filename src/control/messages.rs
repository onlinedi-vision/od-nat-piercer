use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlMessage {
    JoinControl {
        server_id: String,
        channel: String,
        user: String,
        peer_id: u32,
    },

    ControlMsg {
        message_id: u64,
        payload: String,
    },

    ControlAck {
        message_id: u64,
    },
}
