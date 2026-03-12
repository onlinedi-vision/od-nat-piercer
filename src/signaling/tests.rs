use crate::signaling::handlers::utils::{
    add_new_user, create_new_user, make_channel_id, update_existing_user,
};

use crate::signaling::structures::{Channel, NatKind, User};
use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_channel_id_is_stable_for_same_input() {
        let a = make_channel_id("server1", "channel1");
        let b = make_channel_id("server1", "channel1");
        assert_eq!(a, b);
    }

    #[test]
    fn make_channel_id_differs_for_different_channels() {
        let a = make_channel_id("server1", "channel1");
        let b = make_channel_id("server1", "channel2");
        assert_ne!(a, b);
    }

    #[test]
    fn make_channel_id_differs_for_different_servers() {
        let a = make_channel_id("server1", "channel1");
        let b = make_channel_id("server2", "channel1");
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn create_new_user_sets_fields_correctly() {
        let addr: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();

        let user = create_new_user("name", addr, NatKind::Cone, 7).await;

        assert_eq!(user.name, "name");
        assert_eq!(user.addr, addr);
        assert_eq!(user.peer_id, 7);
        assert!(!user.needs_server_relay);
        assert_eq!(user.nat_kind, NatKind::Cone);
    }

    #[tokio::test]
    async fn create_new_user_marks_symmetric_as_server_relay() {
        let addr: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();

        let user = create_new_user("name", addr, NatKind::Symmetric, 9).await;

        assert_eq!(user.peer_id, 9);
        assert!(user.needs_server_relay);
        assert_eq!(user.nat_kind, NatKind::Symmetric);
    }

    #[tokio::test]
    async fn add_new_user_assigns_incrementing_peer_ids() {
        let mut channel = Channel {
            channel_id: 123,
            next_peer_id: 1,
            users: Vec::new(),
            relay: None,
        };

        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());

        let addr1: std::net::SocketAddr = "127.0.0.1:6001".parse().unwrap();
        let (updated_channel, peer_id1) =
            add_new_user(&mut channel, "name1", addr1, &socket, NatKind::Cone).await;

        assert_eq!(peer_id1, 1);
        assert_eq!(updated_channel.users.len(), 1);
        assert_eq!(updated_channel.users[0].peer_id, 1);
        assert_eq!(updated_channel.next_peer_id, 2);

        let addr2: std::net::SocketAddr = "127.0.0.1:6002".parse().unwrap();
        let (updated_channel, peer_id2) =
            add_new_user(&mut channel, "name2", addr2, &socket, NatKind::Cone).await;

        assert_eq!(peer_id2, 2);
        assert_eq!(updated_channel.users.len(), 2);
        assert_eq!(updated_channel.users[1].peer_id, 2);
        assert_eq!(updated_channel.next_peer_id, 3);
    }

    #[tokio::test]
    async fn update_existing_user_returns_existing_peer_id() {
        let addr: std::net::SocketAddr = "127.0.0.1:7001".parse().unwrap();

        let mut channel = Channel {
            channel_id: 123,
            next_peer_id: 2,
            users: vec![User {
                peer_id: 1,
                name: "name".to_string(),
                addr,
                last_pong: std::time::Instant::now(),
                needs_server_relay: false,
                nat_kind: NatKind::Cone,
            }],
            relay: None,
        };

        let result = update_existing_user(&mut channel, "name", addr).await;

        assert!(result.is_some());

        let (updated_channel, is_new, peer_id) = result.unwrap();
        assert!(!is_new);
        assert_eq!(peer_id, 1);
        assert_eq!(updated_channel.users.len(), 1);
        assert_eq!(updated_channel.users[0].peer_id, 1);
    }

    #[tokio::test]
    async fn update_existing_user_returns_none_for_missing_user() {
        let addr: std::net::SocketAddr = "127.0.0.1:7002".parse().unwrap();

        let mut channel = Channel {
            channel_id: 123,
            next_peer_id: 1,
            users: Vec::new(),
            relay: None,
        };

        let result = update_existing_user(&mut channel, "name", addr).await;

        assert!(result.is_none());
    }
}
