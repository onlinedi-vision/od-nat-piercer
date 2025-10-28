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
            channel.need_server_relay.insert(username.to_string());

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
    let sender = parts[1];

    let mut st = state.lock().await;

    for (_sid, channels) in st.iter_mut() {
        for (_cname, channel) in channels.iter_mut() {
            //find the sender in this channel by (addr, name)
            if let Some(_user) = channel
                .users
                .iter()
                .position(|u| u.addr == src && u.name == sender)
            {
                //who is relay
                let relay_name = channel.relay.clone();

                //if sender is relay -> deliver only to need_server_relay
                if relay_name.as_deref() == Some(sender) {
                    if channel.need_server_relay.is_empty() {
                        //nothing to mirror
                        return;
                    }

                    for peer in channel.users.iter() {
                        if channel.need_server_relay.contains(&peer.name) && peer.name != sender {
                            let _ = socket.send_to(raw.as_bytes(), peer.addr).await;
                        }
                    }
                    return;
                }

                // If sender is symmetric (needs server relay) -> deliver to everyone else
                if channel.need_server_relay.contains(sender) {
                    for peer in channel.users.iter() {
                        if peer.addr != src {
                            let _ = socket.send_to(raw.as_bytes(), peer.addr).await;
                        }
                    }
                }

                //otherwise (unexpected normal client sent to server?)
                // forward to the relay only
                if let Some(relay_name) = relay_name {
                    if let Some(relay) = channel.users.iter().find(|u| u.name == relay_name) {
                        if relay.addr != src {
                            let _ = socket.send_to(raw.as_bytes(), relay.addr).await;
                        }
                    }
                }
                return;
            }
        }
    }
}
