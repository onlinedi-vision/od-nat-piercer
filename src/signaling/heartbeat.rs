use crate::{
    proto::control_text::{MSG_MODE, MSG_PING, MSG_SERVER_RELAY, MSG_USER_LEFT},
    signaling::{handlers::utils::pick_eligible_relay, structures::ServerMap},
};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::Mutex};

const SERVER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const REGISTRATION_EXPIRY: Duration = Duration::from_secs(40);

fn handle_relay_timeout(
    channel: &mut crate::signaling::structures::Channel,
    notifications: &mut Vec<(Vec<SocketAddr>, Vec<u8>)>,
) {
    notifications.extend(crate::signaling::handlers::notifications::relay_handover(
        channel,
    ));
}

fn process_channel_heartbeat(
    server_id: &str,
    channel_name: &str,
    channel: &mut crate::signaling::structures::Channel,
    pings: &mut Vec<SocketAddr>,
    cleanup: &mut Vec<(String, String)>,
    notifications: &mut Vec<(Vec<SocketAddr>, Vec<u8>)>,
    now: Instant,
) {
    let mut expired = Vec::new();
    channel.users.retain(|user| {
        if now.saturating_duration_since(user.last_pong) > REGISTRATION_EXPIRY {
            expired.push(user.clone());
            false
        } else {
            true
        }
    });
    let relay_expired = expired
        .iter()
        .any(|user| channel.relay.as_deref() == Some(&user.name));
    for user in &expired {
        println!(
            "User {} timed out from {server_id}-{channel_name}",
            user.name
        );
        let recipients = channel
            .users
            .iter()
            .map(|peer| peer.addr)
            .collect::<Vec<_>>();
        if !recipients.is_empty() {
            notifications.push((
                recipients,
                format!("{MSG_USER_LEFT} {} {}\n", user.name, user.addr).into_bytes(),
            ));
        }
    }
    // Remove the whole expired batch before selecting anyone as a relay.
    if !expired.is_empty() && (relay_expired || channel.users.len() == 1) {
        handle_relay_timeout(channel, notifications);
    }
    if channel.users.len() == 1 {
        if let Some(solo) = pick_eligible_relay(channel) {
            pings.push(solo.addr);
            channel.relay = Some(solo.name);
        } else {
            channel.relay = None;
            let solo = &channel.users[0];
            notifications.push((
                vec![solo.addr],
                format!("{MSG_MODE} {MSG_SERVER_RELAY} {}", solo.name).into_bytes(),
            ));
        }
    }
    if channel.users.is_empty() {
        cleanup.push((server_id.to_owned(), channel_name.to_owned()));
    }
}

async fn collect_heartbeat_data(
    state: Arc<Mutex<ServerMap>>,
) -> (
    Vec<SocketAddr>,
    Vec<(String, String)>,
    Vec<(Vec<SocketAddr>, Vec<u8>)>,
) {
    collect_heartbeat_data_at(state, Instant::now()).await
}

async fn collect_heartbeat_data_at(
    state: Arc<Mutex<ServerMap>>,
    now: Instant,
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
    for sid in &server_ids {
        if let Some(channels) = st.get_mut(sid) {
            let channel_names: Vec<String> = channels.keys().cloned().collect();
            for cname in &channel_names {
                if let Some(channel) = channels.get_mut(cname) {
                    process_channel_heartbeat(
                        sid,
                        cname,
                        channel,
                        &mut pings,
                        &mut cleanup,
                        &mut notifications,
                        now,
                    );
                }
            }
        }
    }
    (pings, cleanup, notifications)
}

async fn send_pings(socket: Arc<UdpSocket>, to_ping: Vec<SocketAddr>) {
    for addr in to_ping {
        if let Err(e) = socket.send_to(MSG_PING.as_bytes(), addr).await {
            eprintln!("Failed to send {MSG_PING}: {e}");
        }
    }
}

async fn send_notifications(socket: Arc<UdpSocket>, notify_msgs: Vec<(Vec<SocketAddr>, Vec<u8>)>) {
    //send notifications (USER_LEFT, MODE RELAY, MODE DIRECT messages}
    for (peers_to_notify, payload) in notify_msgs {
        for addr in peers_to_notify {
            if let Err(e) = socket.send_to(&payload, addr).await {
                eprintln!("Failed to send heartbeat notification to {addr}: {e}");
            }
        }
    }
}

async fn cleanup_empty_channels(state: Arc<Mutex<ServerMap>>, to_cleanup: Vec<(String, String)>) {
    if !to_cleanup.is_empty() {
        let mut st = state.lock().await;
        for (sid, cname) in to_cleanup {
            if let Some(chans) = st.get_mut(&sid) {
                // A participant may have joined after the empty snapshot.
                if chans
                    .get(&cname)
                    .is_some_and(|channel| channel.users.is_empty())
                {
                    chans.remove(&cname);
                }
                if chans.is_empty() {
                    st.remove(&sid);
                }
            }
        }
    }
}

pub fn start_heartbeat(socket: Arc<UdpSocket>, state: Arc<Mutex<ServerMap>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SERVER_HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let (to_ping, to_cleanup, notify_msgs) = collect_heartbeat_data(state.clone()).await;

            send_pings(socket.clone(), to_ping).await;

            send_notifications(socket.clone(), notify_msgs).await;

            cleanup_empty_channels(state.clone(), to_cleanup).await;
        }
    });
}

#[cfg(test)]
mod tests;

#[test]
fn relay_handover_sends_one_direct_notice_to_each_recipient() {
    use crate::signaling::structures::{Channel, NatKind, User};
    use std::net::Ipv4Addr;

    let mut channel = Channel {
        users: vec![
            User::new(
                "new-relay",
                (Ipv4Addr::LOCALHOST, 30001).into(),
                NatKind::Cone,
                1,
            ),
            User::new(
                "peer-a",
                (Ipv4Addr::LOCALHOST, 30002).into(),
                NatKind::Cone,
                2,
            ),
            User::new(
                "peer-b",
                (Ipv4Addr::LOCALHOST, 30003).into(),
                NatKind::Cone,
                3,
            ),
        ],
        ..Channel::default()
    };
    let mut notifications = Vec::new();
    handle_relay_timeout(&mut channel, &mut notifications);
    let expected = b"MODE DIRECT new-relay 127.0.0.1:30001\n";
    for peer in &channel.users[1..] {
        let notices: Vec<_> = notifications
            .iter()
            .filter(|(recipients, _)| recipients.contains(&peer.addr))
            .map(|(_, payload)| payload.as_slice())
            .collect();
        assert_eq!(notices, [expected.as_slice()]);
    }
    assert_eq!(channel.relay.as_deref(), Some("new-relay"));
}
