use crate::signaling::structures::{Channel, NatKind, ServerMap, User};
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

fn pick_eligible_relay(channel: &Channel) -> Option<User> {
    // Primul user care NU e in need_server_relay (adica nu are NAT symmetric)
    channel
        .users
        .iter()
        .find(|u| !u.needs_server_relay && !matches!(u.nat_kind, NatKind::Symmetric))
        .cloned()
}

pub async fn mark_relay_in_channel(
    state: &Arc<Mutex<ServerMap>>,
    server_id: &str,
    channel_name: &str,
    relay_user: &User,
) {
    let mut st = state.lock().await;
    if let Some(channels) = st.get_mut(server_id) {
        if let Some(channel) = channels.get_mut(channel_name) {
            channel.relay = Some(relay_user.name.clone());
        }
    }
}

pub async fn notify_relay_about_peers(socket: &Arc<UdpSocket>, relay_user: &User, peers: &[User]) {
    for peer in peers {
        let msg = format!("MODE DIRECT {} {}\n", peer.name, peer.addr);
        if let Err(e) = socket.send_to(msg.as_bytes(), relay_user.addr).await {
            eprintln!(
                "Failed to notify relay {} about {}: {}",
                relay_user.name, peer.name, e
            );
        }
    }
}

pub async fn notify_peers_about_relay(socket: &Arc<UdpSocket>, relay_user: &User, peers: &[User]) {
    for peer in peers {
        let msg = format!("MODE DIRECT {} {}\n", relay_user.name, relay_user.addr);
        if let Err(e) = socket.send_to(msg.as_bytes(), peer.addr).await {
            eprintln!(
                "Failed to notify {} about new relay{}: {}",
                peer.name, relay_user.name, e
            );
        }
    }
}

pub async fn send_relay_mode_to_relay(socket: &Arc<UdpSocket>, relay_user: &User) {
    let reply = "MODE RELAY\n";
    if let Err(e) = socket.send_to(reply.as_bytes(), relay_user.addr).await {
        eprintln!("Failed to send RELAY mode to {}: {}", relay_user.name, e);
    }
}

pub async fn notify_existing_users_about_new_user(
    socket: &Arc<UdpSocket>,
    users: &[User],
    new_user_name: &str,
    new_user_addr: SocketAddr,
) {
    for user in users.iter() {
        if user.addr != new_user_addr {
            let msg_to_existing = format!("MODE DIRECT {} {}\n", new_user_name, new_user_addr);
            if let Err(e) = socket.send_to(msg_to_existing.as_bytes(), user.addr).await {
                eprintln!("Failed to notify {}: {}", user.name, e);
            }

            let msg_to_new = format!("MODE DIRECT {} {}\n", user.name, user.addr);
            if let Err(e) = socket.send_to(msg_to_new.as_bytes(), new_user_addr).await {
                eprintln!("Failed to notify new user: {}", e);
            }
        }
    }
}

pub async fn promote_new_relay(socket: &Arc<UdpSocket>, new_relay: &User) {
    if let Err(e) = socket
        .send_to("MODE RELAY\n".as_bytes(), new_relay.addr)
        .await
    {
        eprintln!(
            "Failed to notify {} about promotion to relay: {}",
            new_relay.name, e
        );
    }
}

pub async fn notify_lone_user(socket: &Arc<UdpSocket>, lone_user_addr: Option<SocketAddr>) {
    if let Some(lone_user_addr) = lone_user_addr {
        let reply = "MODE RELAY\n";
        if let Err(e) = socket.send_to(reply.as_bytes(), lone_user_addr).await {
            eprintln!("Failed to notify lone user: {}", e);
        }
    }
}

pub async fn notify_peers_about_new_relay(
    socket: &Arc<UdpSocket>,
    new_relay: &User,
    peers: &[User],
) {
    for user in peers {
        let msg = format!("MODE DIRECT {} {}\n", new_relay.name, new_relay.addr);
        if let Err(e) = socket.send_to(msg.as_bytes(), user.addr).await {
            eprintln!("Failed to notify {} about new relay: {}", user.name, e);
        }
    }
}

pub async fn notify_new_relay_about_peers(
    socket: &Arc<UdpSocket>,
    new_relay: &User,
    peers: &[User],
) {
    for user in peers {
        let msg = format!("MODE DIRECT {} {}\n", user.name, user.addr);
        if let Err(e) = socket.send_to(msg.as_bytes(), new_relay.addr).await {
            eprintln!(
                "Failed to notify {} about connected users: {}",
                new_relay.name, e
            );
        }
    }
}

pub async fn notify_all_about_departure(
    socket: &Arc<UdpSocket>,
    remaining_users: Vec<User>,
    user_name: &str,
    leaving_user_addr: Option<SocketAddr>,
) {
    for user in remaining_users {
        let departure_msg = format!(
            "USER_LEFT {} {}\n",
            user_name,
            leaving_user_addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| "0.0.0.0:0".into())
        );
        if let Err(e) = socket.send_to(departure_msg.as_bytes(), user.addr).await {
            eprintln!("Failed to notify {} about departure: {}", user.name, e);
        }
    }
}

pub async fn handle_connect_notifications(
    server_id: &str,
    channel_name: &str,
    user_name: &str,
    src_addr: SocketAddr,
    users_to_notify: Channel,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    if users_to_notify.users.len() > 1 {
        handle_multiple_users_scenario(server_id, channel_name, &users_to_notify, &socket, &state)
            .await;
    } else {
        handle_single_user_scenario(&socket, &users_to_notify, user_name, src_addr).await;
    }
}

pub async fn handle_multiple_users_scenario(
    server_id: &str,
    channel_name: &str,
    users_to_notify: &Channel,
    socket: &Arc<UdpSocket>,
    state: &Arc<Mutex<ServerMap>>,
) {
    if let Some(relay_user) = pick_eligible_relay(users_to_notify) {
        let mut relay_peers: Vec<User> = Vec::new();
        let mut symmetric_peers: Vec<User> = Vec::new();

        for u in users_to_notify.users.iter() {
            if u.name == relay_user.name {
                continue;
            }

            if u.needs_server_relay {
                symmetric_peers.push(u.clone());
            } else {
                relay_peers.push(u.clone());
            }
        }

        // 1) mark relay for channel
        mark_relay_in_channel(state, server_id, channel_name, &relay_user).await;

        // 2) notify relay about DIRECT peers
        notify_relay_about_peers(socket, &relay_user, &relay_peers).await;

        // 3) notify DIRECT peers about relay
        notify_peers_about_relay(socket, &relay_user, &relay_peers).await;

        // 4) relay receives MODE RELAY
        send_relay_mode_to_relay(socket, &relay_user).await;

        // 5) SYMMETRIC peers: only get MODE SERVER_RELAY
        for symmetric in symmetric_peers.iter() {
            let msg = format!("MODE SERVER_RELAY {}\n", symmetric.name);

            //1) send to symmetric user (to send via server)
            let _ = socket.send_to(msg.as_bytes(), symmetric.addr).await;

            //2) send to relay (to mark peer as peer.use_server_relay)
            if relay_user.addr != symmetric.addr {
                let _ = socket.send_to(msg.as_bytes(), relay_user.addr).await;
            }
        }
    } else {
        // no eligible user -> no user gets promoted to relay, let server as relay
        // announce that server will be relay for every user in the channel
        for u in &users_to_notify.users {
            let notify = format!("MODE SERVER_RELAY {}\n", u.name);
            for usr in &users_to_notify.users {
                let _ = socket.send_to(notify.as_bytes(), usr.addr).await;
            }
        }

        // mark in state that there is no user relay
        let mut st = state.lock().await;
        if let Some(chans) = st.get_mut(server_id) {
            if let Some(ch) = chans.get_mut(channel_name) {
                ch.relay = None;
            }
        }
    }
}

pub async fn handle_single_user_scenario(
    socket: &Arc<UdpSocket>,
    users_to_notify: &Channel,
    user_name: &str,
    src_addr: SocketAddr,
) {
    notify_existing_users_about_new_user(socket, &users_to_notify.users, user_name, src_addr).await;
}

pub async fn handle_relay_transition(
    socket: &Arc<UdpSocket>,
    was_relay: bool,
    state: &Arc<Mutex<ServerMap>>,
    server_id: &str,
    channel_name: &str,
) {
    if !was_relay {
        return;
    }

    //luam canalul curent ca sa vedem users + need_server_relay
    let channel_opt = {
        let st = state.lock().await;
        st.get(server_id)
            .and_then(|chans| chans.get(channel_name))
            .cloned()
    };

    if let Some(channel) = channel_opt {
        //alegem un nou relay eligibil
        if let Some(new_relay) = pick_eligible_relay(&channel) {
            let peers: Vec<User> = channel
                .users
                .iter()
                .cloned()
                .filter(|u| u.name != new_relay.name)
                .collect();

            promote_new_relay(socket, &new_relay).await;
            notify_peers_about_new_relay(socket, &new_relay, &peers).await;
            notify_new_relay_about_peers(socket, &new_relay, &peers).await;

            //actualizam relay in state
            let mut st = state.lock().await;
            if let Some(chans) = st.get_mut(server_id) {
                if let Some(ch) = chans.get_mut(channel_name) {
                    ch.relay = Some(new_relay.name.clone());
                }
            }
        } else {
            //nici un user eligibil -> ramane serverul ca relay, anuntam SERVER_RELAY pentru toti
            for u in &channel.users {
                let notify = format!("MODE SERVER_RELAY {}\n", u.name);
                for usr in &channel.users {
                    let _ = socket.send_to(notify.as_bytes(), usr.addr).await;
                }
            }

            let mut st = state.lock().await;
            if let Some(chans) = st.get_mut(server_id) {
                if let Some(ch) = chans.get_mut(channel_name) {
                    ch.relay = None;
                }
            }
        }
    }
}

pub async fn handle_disconnect_notifications(
    remaining_users: Vec<User>,
    was_relay: bool,
    leaving_user_addr: Option<SocketAddr>,
    lone_user_addr: Option<SocketAddr>,
    user_name: &str,
    socket: Arc<UdpSocket>,
    state: &Arc<Mutex<ServerMap>>,
    server_id: String,
    channel_name: String,
) {
    notify_lone_user(&socket, lone_user_addr).await;
    handle_relay_transition(&socket, was_relay, &state, &server_id, &channel_name).await;
    notify_all_about_departure(&socket, remaining_users, user_name, leaving_user_addr).await;
}

pub async fn handle_peer_timeout(
    parts: &[&str],
    _src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let server_id = parts[1].to_string();
    let channel_name = parts[2].to_string();
    let peer_user = parts[3].to_string();

    let remaining = {
        let mut st = state.lock().await;
        if let Some(channels) = st.get_mut(&server_id) {
            if let Some(channel) = channels.get_mut(&channel_name) {
                //removing by name
                channel.users.retain(|u| u.name != peer_user);

                //update relay if needed
                if channel.users.len() == 1 {
                    channel.relay = Some(channel.users[0].name.clone());
                }
            }
        }

        st.get(&server_id)
            .and_then(|channels| channels.get(&channel_name))
            .cloned()
    };

    if let Some(channel) = remaining {
        for u in channel.users {
            let _ = socket
                .send_to(
                    format!("USER_LEFT {} 0.0.0.0:0\n", peer_user).as_bytes(),
                    u.addr,
                )
                .await;
        }
    }
}
