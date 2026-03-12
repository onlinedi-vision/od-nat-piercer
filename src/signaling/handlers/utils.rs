use crate::{
    proto::control_text::{MSG_MODE, MSG_RELAY, MSG_SERVER_RELAY},
    signaling::structures::{Channel, NatKind, ServerMap, User},
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};
use tokio::{net::UdpSocket, sync::Mutex};

pub fn make_channel_id(server_id: &str, channel_name: &str) -> u32 {
    let mut h = DefaultHasher::new();
    server_id.hash(&mut h);
    channel_name.hash(&mut h);
    (h.finish() & 0xFFFF_FFFF) as u32
}

pub async fn get_remaining_users(
    state: &Arc<Mutex<ServerMap>>,
    server_id: &str,
    channel_name: &str,
) -> Vec<User> {
    let st = state.lock().await;
    st.get(server_id)
        .and_then(|channels| channels.get(channel_name))
        .map(|channel| channel.users.clone())
        .unwrap_or_default()
}

pub async fn update_existing_user(
    channel: &mut Channel,
    user_name: &str,
    src_addr: SocketAddr,
) -> Option<(Channel, bool, u32)> {
    //check if user already present with same addr
    if let Some(existing) = channel
        .users
        .iter_mut()
        .find(|u| u.name == user_name && u.addr == src_addr)
    {
        existing.last_pong = Instant::now();
        let peer_id = existing.peer_id;
        Some((channel.clone(), false, peer_id))
    } else {
        None
    }
}

pub async fn remove_old_user_sessions(
    channel: &mut Channel,
    user_name: &str,
    src_addr: SocketAddr,
) {
    channel
        .users
        .retain(|u| !(u.name == user_name && u.addr != src_addr));
}

pub async fn create_new_user(
    user_name: &str,
    src_addr: SocketAddr,
    nat_kind: NatKind,
    peer_id: u32,
) -> User {
    User {
        peer_id,
        name: user_name.to_string(),
        addr: src_addr,
        last_pong: Instant::now(),
        needs_server_relay: matches!(nat_kind, NatKind::Symmetric),
        nat_kind,
    }
}

pub async fn handle_lone_user_scenario(channel: &mut Channel, socket: &Arc<UdpSocket>) {
    if channel.relay.is_none() && channel.users.len() == 1 {
        let u = &channel.users[0];
        match u.nat_kind {
            NatKind::Symmetric => {
                let reply = format!(
                    "{} {} {}\n",
                    MSG_MODE, MSG_SERVER_RELAY, channel.users[0].name
                );
                if let Err(e) = socket.send_to(reply.as_bytes(), u.addr).await {
                    eprintln!("Failed to notify lone user about server relay: {}", e);
                }
                channel.relay = None;
            }

            _ => {
                if let Err(e) = socket
                    .send_to(format!("{MSG_MODE} {MSG_RELAY}\n").as_bytes(), u.addr)
                    .await
                {
                    eprintln!("Failed to notify lone user about relay mode: {}", e);
                }
                channel.relay = Some(u.name.clone());
            }
        }
    }
}

pub async fn add_new_user(
    channel: &mut Channel,
    user_name: &str,
    src_addr: SocketAddr,
    socket: &Arc<UdpSocket>,
    nat_kind: NatKind,
) -> (Channel, u32) {
    //Remove old user sessions with same name but different address
    remove_old_user_sessions(channel, user_name, src_addr).await;

    let peer_id = channel.next_peer_id;
    channel.next_peer_id += 1;

    //Create and add new user
    let new_user = create_new_user(user_name, src_addr, nat_kind, peer_id).await;
    channel.users.push(new_user);

    //Handle lone user scenario
    handle_lone_user_scenario(channel, socket).await;

    println!("User {} joined from {}", user_name, src_addr);

    (channel.clone(), peer_id)
}

pub async fn find_and_remove_user(
    channel: &mut Channel,
    user_name: &str,
    src_addr: SocketAddr,
) -> Option<(bool, SocketAddr)> {
    if let Some(pos) = channel
        .users
        .iter()
        .position(|u| u.name == user_name && u.addr == src_addr)
    {
        //check if leaving user was relay
        let was_relay = channel
            .relay
            .as_ref()
            .map(|r| r == &user_name)
            .unwrap_or(false);

        let leaving_user_addr = channel.users[pos].addr;
        channel.users.remove(pos);

        Some((was_relay, leaving_user_addr))
    } else {
        None
    }
}

pub async fn update_relay_after_departure(
    channel: &mut Channel,
    was_relay: bool,
) -> Option<SocketAddr> {
    let mut lone_user_addr = None;

    if was_relay {
        if !channel.users.is_empty() {
            let new_relay = channel.users[0].name.clone();
            channel.relay = Some(new_relay);
        } else {
            channel.relay = None;
        }

        if channel.users.len() == 1 {
            channel.relay = Some(channel.users[0].name.clone());
            lone_user_addr = Some(channel.users[0].addr);
        }
    }

    lone_user_addr
}

pub async fn handle_user_removal(
    state: &Arc<Mutex<ServerMap>>,
    server_id: &str,
    channel_name: &str,
    user_name: &str,
    src_addr: SocketAddr,
) -> (Vec<User>, bool, Option<SocketAddr>, Option<SocketAddr>) {
    let mut st = state.lock().await;
    let mut was_relay = false;
    let mut leaving_user_addr = None;
    let mut lone_user_addr = None;

    if let Some(channels) = st.get_mut(server_id) {
        if let Some(channel) = channels.get_mut(channel_name) {
            if let Some((found_was_relay, found_leaving_addr)) =
                find_and_remove_user(channel, user_name, src_addr).await
            {
                was_relay = found_was_relay;
                leaving_user_addr = Some(found_leaving_addr);

                lone_user_addr = update_relay_after_departure(channel, was_relay).await;

                println!("User {} left {}-{}", user_name, server_id, channel_name);
            } else {
                println!(
                    "Ignoring DISCONNECT for {} from {} (no matching session)",
                    user_name, src_addr
                );
            }
        }
    }

    let remaining_users = get_remaining_users(&state, server_id, channel_name).await;
    (
        remaining_users,
        was_relay,
        leaving_user_addr,
        lone_user_addr,
    )
}

pub async fn remove_user_from_other_channels(
    st: &mut ServerMap,
    server_id: &str,
    channel_name: &str,
    user_name: &str,
    src_addr: SocketAddr,
) {
    if let Some(channels) = st.get_mut(server_id) {
        for (cname, ch) in channels.iter_mut() {
            if cname == channel_name {
                continue;
            }

            ch.users
                .retain(|u| !(u.name == user_name && u.addr == src_addr));
        }
    }
}

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
