use crate::{
    proto::{
        control_text::{MSG_DATA, MSG_MODE, MSG_SERVER_RELAY},
        routed_data::{Data, RelayData},
    },
    signaling::structures::ServerMap,
};
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

pub async fn handle_relay_request(
    parts: &[&str],
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    // Both path-failure reports and initial fallback requests use this policy.
    if parts.len() < 4 {
        return;
    }
    let server_id = parts[1];
    let channel_name = parts[2];
    let username = parts[3];

    let recipients: Vec<SocketAddr> = {
        let mut st = state.lock().await;
        let Some(channel) = st
            .get_mut(server_id)
            .and_then(|channels| channels.get_mut(channel_name))
        else {
            return;
        };
        let relay_name = channel.relay.as_deref();
        let relay_report = relay_name.is_some_and(|relay| {
            relay != username
                && channel
                    .users
                    .iter()
                    .any(|user| user.name == relay && user.addr == src)
        });
        let self_request = parts[0] == crate::proto::control_text::MSG_REQUEST_RELAY
            && relay_name != Some(username)
            && channel
                .users
                .iter()
                .any(|user| user.name == username && user.addr == src);
        if !relay_report && !self_request {
            return;
        }
        let Some(target) = channel.users.iter_mut().find(|user| user.name == username) else {
            return;
        };
        target.needs_server_relay = true;
        channel.users.iter().map(|user| user.addr).collect()
    };

    let notify = format!("{MSG_MODE} {MSG_SERVER_RELAY} {username}\n");
    for recipient in recipients {
        if let Err(error) = socket.send_to(notify.as_bytes(), recipient).await {
            eprintln!("Failed to send server-relay notification to {recipient}: {error}");
        }
    }
}

pub async fn handle_data_from_client(
    raw: &str,
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let Some(data) = Data::parse(raw) else {
        return;
    };
    let recipients = {
        let state = state.lock().await;
        state
            .values()
            .flat_map(|channels| channels.values())
            .find_map(|channel| {
                channel
                    .users
                    .iter()
                    .any(|user| user.addr == src && user.name == data.sender)
                    .then(|| {
                        channel
                            .users
                            .iter()
                            .filter(|user| user.name != data.sender)
                            .map(|user| user.addr)
                            .collect::<Vec<_>>()
                    })
            })
            .unwrap_or_default()
    };
    // Ordinary submissions give the server full distribution ownership,
    // including a direct member or the relay choosing server-only submission.
    send_data_to_recipients(raw, &recipients, &socket).await;
}

pub async fn handle_relay_data(
    raw: &str,
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let Some(mirror) = RelayData::parse(raw) else {
        return;
    };
    let recipients = {
        let state = state.lock().await;
        state
            .values()
            .flat_map(|channels| channels.values())
            .find_map(|channel| {
                let relay_name = channel.relay.as_deref()?;
                if !channel
                    .users
                    .iter()
                    .any(|user| user.name == relay_name && user.addr == src)
                    || !channel
                        .users
                        .iter()
                        .any(|user| user.name == mirror.data.sender)
                {
                    return None;
                }
                Some(
                    channel
                        .users
                        .iter()
                        .filter(|user| {
                            user.needs_server_relay
                                && user.name != mirror.data.sender
                                && user.addr != src
                                && !mirror.direct.contains(&user.name.as_str())
                        })
                        .map(|user| user.addr)
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default()
    };
    let delivery = format!("{MSG_DATA} {} {}", mirror.data.sender, mirror.data.payload);
    send_data_to_recipients(&delivery, &recipients, &socket).await;
}

async fn send_data_to_recipients(raw: &str, recipients: &[SocketAddr], socket: &UdpSocket) {
    for recipient in recipients {
        if let Err(error) = socket.send_to(raw.as_bytes(), recipient).await {
            eprintln!("Failed to forward DATA to {recipient}: {error}");
        }
    }
}
