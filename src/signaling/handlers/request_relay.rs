use crate::signaling::structures::ServerMap;
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

pub async fn handle_relay_request(
    parts: &[&str],
    _src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    // Relay request <server_id> <channel> <username>
    if parts.len() < 4 {
        return;
    }
    let server_id = parts[1];
    let channel_name = parts[2];
    let username = parts[3];

    let mut st = state.lock().await;
    if let Some(channels) = st.get_mut(server_id) {
        if let Some(channel) = channels.get_mut(channel_name) {
            // mark this user that he needs server relay
            if let Some(user) = channel.users.iter_mut().find(|u| u.name == username) {
                user.needs_server_relay = true;
            }
            // notify all users that the server will forward for the user that needs a relay
            let notify = format!("MODE SERVER_RELAY {}\n", username);
            for u in channel.users.iter() {
                let _ = socket.send_to(notify.as_bytes(), u.addr).await;
            }
        }
    }
}

pub async fn handle_data_from_client(
    raw: &str,
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    // expect "DATA <sender> <payload>"
    let parts: Vec<&str> = raw.splitn(3, ' ').collect();
    if parts.len() < 3 {
        return;
    }
    let sender_name = parts[1];

    let mut st = state.lock().await;

    for (_sid, channels) in st.iter_mut() {
        for (_cname, channel) in channels.iter_mut() {
            // mirrored DATA from RELAY -> deliver to peers that need server relay
            if let Some(relay_name) = &channel.relay {
                if let Some(relay_user) = channel.users.iter().find(|u| &u.name == relay_name) {
                    if relay_user.addr == src {
                        for peer in channel.users.iter() {
                            if peer.name != sender_name && peer.needs_server_relay {
                                let _ = socket.send_to(raw.as_bytes(), peer.addr).await;
                            }
                        }
                        return;
                    }
                }
            }

            //find the sender in this channel by (addr, name)
            if let Some(sender_index) = channel
                .users
                .iter()
                .position(|u| u.addr == src && u.name == sender_name)
            {
                //who is relay
                let sender_is_relay = channel
                    .relay
                    .as_deref()
                    .map(|r| r == sender_name)
                    .unwrap_or(false);

                let sender_needs_server_relay = channel.users[sender_index].needs_server_relay;

                //if sender is relay -> deliver only to need_server_relay users
                if sender_is_relay {
                    for peer in channel.users.iter() {
                        if peer.name != sender_name && peer.needs_server_relay {
                            let _ = socket.send_to(raw.as_bytes(), peer.addr).await;
                        }
                    }
                    return;
                }

                // If sender is symmetric (needs server relay) -> deliver to everyone else
                if sender_needs_server_relay {
                    for peer in channel.users.iter() {
                        if peer.addr != src {
                            let _ = socket.send_to(raw.as_bytes(), peer.addr).await;
                        }
                    }
                    return;
                }

                //otherwise (unexpected normal client sent to server?)
                // forward to the relay only
                if let Some(relay_name) = &channel.relay {
                    if let Some(relay_user) = channel.users.iter().find(|u| &u.name == relay_name) {
                        if relay_user.addr != src {
                            let _ = socket.send_to(raw.as_bytes(), relay_user.addr).await;
                        }
                    }
                }
                return;
            }
        }
    }
}
