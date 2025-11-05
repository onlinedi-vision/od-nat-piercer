use crate::signaling::{
    structures::{Channel, ServerMap, User},
    utils::cleanup_and_notify_iter,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::UdpSocket, sync::Mutex};

fn pick_eligible_relay(channel: &Channel) -> Option<User> {
    // First user that is NOT in need_server_relay (meaning he doesn't have Symmetric NAT)
    channel
        .users
        .iter()
        .find(|u| !u.needs_server_relay)
        .cloned()
}

fn handle_relay_timeout(
    channel: &mut crate::signaling::structures::Channel,
    notifications: &mut Vec<(Vec<SocketAddr>, Vec<u8>)>,
) {
    if let Some(new_relay_user) = pick_eligible_relay(channel) {
        //setting the new relay in channel
        channel.relay = Some(new_relay_user.name.clone());

        //1) Send MODE RELAY just to the new relay
        notifications.push((vec![new_relay_user.addr], b"MODE RELAY\n".to_vec()));

        //2) Send to every peer: "MODE DIRECT <relay_name> <relay_addr>"
        let peers_addrs: Vec<SocketAddr> = channel
            .users
            .iter()
            .filter(|u| u.name != new_relay_user.name)
            .map(|u| u.addr)
            .collect();
        if !peers_addrs.is_empty() {
            let mut payload = Vec::new();
            for _peer in channel
                .users
                .iter()
                .filter(|u| u.name != new_relay_user.name)
            {
                let m = format!(
                    "MODE DIRECT {} {}\n",
                    new_relay_user.name, new_relay_user.addr
                );
                payload.extend_from_slice(m.as_bytes());
            }
            notifications.push((peers_addrs, payload));
        }

        // 3) Send new relay info about all other peers: "MODE DIRECT <peer> <addr>"
        let mut peers_to_new_msg = Vec::new();
        for peer in channel
            .users
            .iter()
            .filter(|u| u.name != new_relay_user.name)
        {
            let m = format!("MODE DIRECT {} {}\n", peer.name, peer.addr);
            peers_to_new_msg.extend_from_slice(m.as_bytes());
        }
        if !peers_to_new_msg.is_empty() {
            notifications.push((vec![new_relay_user.addr], peers_to_new_msg));
        }
    } else {
        //No eligible user -> no RELAY
        // we just keep the server as relay (send MODE SERVER_RELAY <user> to all users)
        channel.relay = None;
        if !channel.users.is_empty() {
            let mut payload = Vec::new();
            for u in &channel.users {
                let m = format!("MODE SERVER_RELAY {}\n", u.name);
                payload.extend_from_slice(m.as_bytes());
            }

            //send to all users from channel
            let all_addrs: Vec<SocketAddr> = channel.users.iter().map(|u| u.addr).collect();
            notifications.push((all_addrs, payload));
        }
    }
}

fn handle_timed_out_user(
    server_id: &str,
    channel_name: &str,
    channel: &mut crate::signaling::structures::Channel,
    user_index: usize,
    user_name: &str,
    user_addr: SocketAddr,
    notifications: &mut Vec<(Vec<SocketAddr>, Vec<u8>)>,
) {
    println!(
        "User {} timed out from {}-{}",
        user_name, server_id, channel_name
    );

    let msg = format!("USER_LEFT {} {}\n", user_name, user_addr);
    let peers_to_notify: Vec<SocketAddr> = channel
        .users
        .iter()
        .filter(|u| u.addr != user_addr)
        .map(|u| u.addr)
        .collect();

    if !peers_to_notify.is_empty() {
        notifications.push((peers_to_notify, msg.as_bytes().to_vec()));
    }

    let was_relay = channel
        .relay
        .as_ref()
        .map(|r| r == &user_name)
        .unwrap_or(false);

    channel.users.remove(user_index);

    if was_relay {
        handle_relay_timeout(channel, notifications);
    }

    if channel.users.len() == 1 {
        channel.relay = Some(channel.users[0].name.clone());
        notifications.push((vec![channel.users[0].addr], b"MODE RELAY\n".to_vec()));
    }
}

fn process_channel_heartbeat(
    server_id: &str,
    channel_name: &str,
    channel: &mut crate::signaling::structures::Channel,
    pings: &mut Vec<SocketAddr>,
    cleanup: &mut Vec<(String, String)>,
    notifications: &mut Vec<(Vec<SocketAddr>, Vec<u8>)>,
) {
    //Process timeout users
    let mut i = 0;
    while i < channel.users.len() {
        let user_clone = &channel.users[i].clone();

        if user_clone.last_pong.elapsed() > Duration::from_secs(40) {
            handle_timed_out_user(
                server_id,
                channel_name,
                channel,
                i,
                &user_clone.name,
                user_clone.addr,
                notifications,
            );
        } else {
            i += 1;
        }
    }

    if channel.users.len() == 1 {
        //one lone user, if he's eligible, we keep him as relay, otherwise the server remains relay
        let solo = channel.users[0].clone();
        let is_eligible = !solo.needs_server_relay;

        if is_eligible {
            pings.push(solo.addr);
            channel.relay = Some(solo.name.clone());
        } else {
            //not eligible, don't promote him as relay, keep server as relay
            channel.relay = None;

            //announce it
            notifications.push((
                vec![solo.addr],
                format!("MODE SERVER_RELAY {}\n", solo.name).into_bytes(),
            ));
        }
    }

    if channel.users.is_empty() {
        cleanup.push((server_id.to_string(), channel_name.to_string()));
    }
}

async fn collect_heartbeat_data(
    state: Arc<Mutex<ServerMap>>,
) -> (
    Vec<SocketAddr>,
    Vec<(String, String)>,
    Vec<(Vec<SocketAddr>, Vec<u8>)>,
) {
    let mut st = state.lock().await;
    let mut pings = Vec::new();
    let mut cleanup = Vec::new();
    let mut notifications: Vec<(Vec<SocketAddr>, Vec<u8>)> = Vec::new();

    let server_ids: Vec<String> = st.keys().cloned().collect();
    for sid in server_ids.iter() {
        if let Some(channels) = st.get_mut(sid) {
            let channel_names: Vec<String> = channels.keys().cloned().collect();
            for cname in channel_names.iter() {
                if let Some(channel) = channels.get_mut(cname) {
                    process_channel_heartbeat(
                        sid,
                        cname,
                        channel,
                        &mut pings,
                        &mut cleanup,
                        &mut notifications,
                    );
                }
            }
        }
    }
    (pings, cleanup, notifications)
}

async fn send_pings(socket: Arc<UdpSocket>, to_ping: Vec<SocketAddr>) {
    for addr in to_ping {
        if let Err(e) = socket.send_to(b"PING", addr).await {
            eprintln!("Failed to send PING: {}", e);
        }
    }
}

async fn send_notifications(socket: Arc<UdpSocket>, notify_msgs: Vec<(Vec<SocketAddr>, Vec<u8>)>) {
    //send notifications (USER_LEFT, MODE RELAY, MODE DIRECT messages}
    for (peers_to_notify, payload) in cleanup_and_notify_iter(notify_msgs.into_iter()) {
        for addr in peers_to_notify {
            if let Err(e) = socket.send_to(&payload, addr).await {
                eprintln!("Failed to send heartbeat notification to {}: {}", addr, e);
            }
        }
    }
}

async fn cleanup_empty_channels(state: Arc<Mutex<ServerMap>>, to_cleanup: Vec<(String, String)>) {
    if !to_cleanup.is_empty() {
        let mut st = state.lock().await;
        for (sid, cname) in to_cleanup.into_iter() {
            if let Some(chans) = st.get_mut(&sid) {
                chans.remove(&cname);
                if chans.is_empty() {
                    st.remove(&sid);
                }
            }
        }
    }
}

pub fn start_heartbeat(socket: Arc<UdpSocket>, state: Arc<Mutex<ServerMap>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        loop {
            interval.tick().await;

            let (to_ping, to_cleanup, notify_msgs) = collect_heartbeat_data(state.clone()).await;

            send_pings(socket.clone(), to_ping).await;

            send_notifications(socket.clone(), notify_msgs).await;

            cleanup_empty_channels(state.clone(), to_cleanup).await;
        }
    });
}
