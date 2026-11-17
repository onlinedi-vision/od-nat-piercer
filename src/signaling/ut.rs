use crate::signaling::handlers::utils::{add_new_user, update_existing_user};

use crate::signaling::structures::{Channel, NatKind, User};
use std::sync::Arc;

#[cfg(test)]
mod tests {

    use super::*;
    use std::{io, net::Ipv4Addr};

    #[test]
    fn user_new_sets_fields_correctly() {
        let addr = (Ipv4Addr::LOCALHOST, 5000).into();

        let user = User::new("name", addr, NatKind::Cone, 7);

        assert_eq!(user.name, "name");
        assert_eq!(user.addr, addr);
        assert_eq!(user.peer_id, 7);
        assert!(!user.needs_server_relay);
        assert_eq!(user.nat_kind, NatKind::Cone);
    }

    #[test]
    fn user_new_marks_symmetric_as_server_relay() {
        let addr = (Ipv4Addr::LOCALHOST, 5001).into();

        let user = User::new("name", addr, NatKind::Symmetric, 9);

        assert_eq!(user.peer_id, 9);
        assert!(user.needs_server_relay);
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // tokio::test generates expect when creating its runtime.
    async fn create_new_user_sets_fields_correctly() {
        let addr = (Ipv4Addr::LOCALHOST, 5000).into();

        let user = User::new("name", addr, NatKind::Cone, 7);

        assert_eq!(user.name, "name");
        assert_eq!(user.addr, addr);
        assert_eq!(user.peer_id, 7);
        assert!(!user.needs_server_relay);
        assert_eq!(user.nat_kind, NatKind::Cone);
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // tokio::test generates expect when creating its runtime.
    async fn create_new_user_marks_symmetric_as_server_relay() {
        let addr = (Ipv4Addr::LOCALHOST, 5000).into();

        let user = User::new("name", addr, NatKind::Symmetric, 9);

        assert_eq!(user.peer_id, 9);
        assert!(user.needs_server_relay);
        assert_eq!(user.nat_kind, NatKind::Symmetric);
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // tokio::test generates expect when creating its runtime.
    async fn add_new_user_assigns_incrementing_peer_ids() -> io::Result<()> {
        let mut channel = Channel {
            channel_id: 123,
            next_peer_id: 1,
            users: Vec::new(),
            relay: None,
        };

        let socket = Arc::new(tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);

        let addr1 = (Ipv4Addr::LOCALHOST, 6001).into();
        let (updated_channel, peer_id1) =
            add_new_user(&mut channel, "name1", addr1, &socket, NatKind::Cone).await;

        assert_eq!(peer_id1, 1);
        assert_eq!(updated_channel.users.len(), 1);
        assert_eq!(updated_channel.users[0].peer_id, 1);
        assert_eq!(updated_channel.next_peer_id, 2);

        let addr2 = (Ipv4Addr::LOCALHOST, 6002).into();
        let (updated_channel, peer_id2) =
            add_new_user(&mut channel, "name2", addr2, &socket, NatKind::Cone).await;

        assert_eq!(peer_id2, 2);
        assert_eq!(updated_channel.users.len(), 2);
        assert_eq!(updated_channel.users[1].peer_id, 2);
        assert_eq!(updated_channel.next_peer_id, 3);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // tokio::test generates expect when creating its runtime.
    async fn update_existing_user_returns_existing_peer_id() -> io::Result<()> {
        let addr = (Ipv4Addr::LOCALHOST, 7001).into();

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

        let (updated_channel, is_new, peer_id) =
            result.ok_or_else(|| io::Error::other("existing fixture user not found"))?;
        assert!(!is_new);
        assert_eq!(peer_id, 1);
        assert_eq!(updated_channel.users.len(), 1);
        assert_eq!(updated_channel.users[0].peer_id, 1);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // tokio::test generates expect when creating its runtime.
    async fn update_existing_user_returns_none_for_missing_user() {
        let addr = (Ipv4Addr::LOCALHOST, 7002).into();

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
