use crate::signaling::structures::{Channel, NatKind, ServerMap, User};
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tokio::{net::UdpSocket, sync::Mutex};

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
) -> Option<(Channel, bool)> {
    //check if user already present with same addr
    if let Some(existing) = channel
        .users
        .iter_mut()
        .find(|u| u.name == user_name && u.addr == src_addr)
    {
        existing.last_pong = Instant::now();
        Some((channel.clone(), false))
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

pub async fn create_new_user(user_name: &str, src_addr: SocketAddr, nat_kind: NatKind) -> User {
    User {
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
                let reply = format!("MODE SERVER_RELAY {}\n", channel.users[0].name);
                if let Err(e) = socket.send_to(reply.as_bytes(), u.addr).await {
                    eprintln!("Failed to notify lone user about server relay: {}", e);
                }
                channel.relay = None;
            }

            _ => {
                if let Err(e) = socket.send_to(b"MODE RELAY\n", u.addr).await {
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
) -> Channel {
    //Remove old user sessions with same name but different address
    remove_old_user_sessions(channel, user_name, src_addr).await;

    //Create and add new user
    let new_user = create_new_user(user_name, src_addr, nat_kind).await;
    channel.users.push(new_user);

    //Handle lone user scenario
    handle_lone_user_scenario(channel, socket).await;

    println!("User {} joined from {}", user_name, src_addr);

    channel.clone()
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
