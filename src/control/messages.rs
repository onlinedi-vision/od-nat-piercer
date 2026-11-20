use serde::{Deserialize, Serialize};

/// Admission to recipient queues, not confirmation of recipient processing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroadcastReport {
    pub queued: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestRejection {
    NotRegistered,
    InvalidRegistration,
    ConflictingRegistration,
    MessageTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ControlMessage {
    Fallback {
        request_id: u64,
        session: crate::recovery::wire::SessionId,
        target: u32,
    },
    FallbackResult {
        request_id: u64,
    },
    SessionJoin {
        request_id: u64,
        binding: crate::recovery::wire::Binding,
    },
    RouteSnapshot {
        message_id: u64,
        snapshot: crate::recovery::wire::Snapshot,
    },
    JoinControl {
        request_id: u64,
        server_id: String,
        channel: String,
        user: String,
        peer_id: u32,
    },
    JoinAccepted {
        request_id: u64,
    },
    Broadcast {
        request_id: u64,
        payload: String,
    },
    BroadcastResult {
        request_id: u64,
        report: BroadcastReport,
    },
    RequestRejected {
        request_id: u64,
        reason: RequestRejection,
    },
    ControlMsg {
        message_id: u64,
        payload: String,
    },
    ControlAck {
        message_id: u64,
    },
}
