use crate::client::control_plane::should_start_control_client;
use crate::client::handlers::try_handle_welcome;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_handle_welcome_sets_channel_id_and_peer_id() {
        let channel_id = Arc::new(AtomicU64::new(0));
        let my_peer_id = Arc::new(AtomicU32::new(0));

        try_handle_welcome("WELCOME 123456789 42\n", &channel_id, &my_peer_id);

        assert_eq!(channel_id.load(Ordering::Acquire), 123456789);
        assert_eq!(my_peer_id.load(Ordering::Acquire), 42);
    }

    #[test]
    fn try_handle_welcome_ignores_invalid_format() {
        let channel_id = Arc::new(AtomicU64::new(0));
        let my_peer_id = Arc::new(AtomicU32::new(0));

        try_handle_welcome(
            "WELCOME to cid:123456789 with pid:42\n",
            &channel_id,
            &my_peer_id,
        );

        assert_eq!(channel_id.load(Ordering::Acquire), 0);
        assert_eq!(my_peer_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn try_handle_welcome_ignores_invalid_numeric_values() {
        let channel_id = Arc::new(AtomicU64::new(0));
        let my_peer_id = Arc::new(AtomicU32::new(0));

        try_handle_welcome("WELCOME abc 42\n", &channel_id, &my_peer_id);

        assert_eq!(channel_id.load(Ordering::Acquire), 0);
        assert_eq!(my_peer_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn try_handle_welcome_ignores_invalid_peer_id() {
        let channel_id = Arc::new(AtomicU64::new(0));
        let my_peer_id = Arc::new(AtomicU32::new(0));

        try_handle_welcome("WELCOME 123456789 abc\n", &channel_id, &my_peer_id);

        assert_eq!(channel_id.load(Ordering::Acquire), 0);
        assert_eq!(my_peer_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn try_handle_welcome_ignores_non_welcome_messages() {
        let channel_id = Arc::new(AtomicU64::new(0));
        let my_peer_id = Arc::new(AtomicU32::new(0));

        try_handle_welcome("MODE RELAY\n", &channel_id, &my_peer_id);

        assert_eq!(channel_id.load(Ordering::Acquire), 0);
        assert_eq!(my_peer_id.load(Ordering::Acquire), 0);
    }

    #[test]
    fn should_start_control_client_when_not_started_and_peer_id_is_valid() {
        assert!(should_start_control_client(false, 1));
    }

    #[test]
    fn should_not_start_control_client_when_already_started() {
        assert!(!should_start_control_client(true, 1));
    }

    #[test]
    fn should_not_start_control_client_without_peer_id() {
        assert!(!should_start_control_client(false, 0));
    }
}
