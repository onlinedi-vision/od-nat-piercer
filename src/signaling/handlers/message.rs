use crate::proto::control_text::{
    MSG_CONNECT, MSG_DATA, MSG_DISCONNECT, MSG_HB, MSG_NAT_PROBE, MSG_NAT_SEEN, MSG_PEER_TIMEOUT,
    MSG_PONG, MSG_REQUEST_RELAY,
};
use crate::signaling::structures::ServerMap;
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

use super::{
    connect::handle_connect_message,
    disconnect::handle_disconnect_message,
    heartbeat::{handle_heartbeat, handle_pong},
    notifications::handle_peer_timeout,
    request_relay::{handle_data_from_client, handle_relay_request},
};

pub async fn handle_message(
    msg: String,
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    if msg.starts_with(MSG_NAT_PROBE) {
        let reply = format!("{MSG_NAT_SEEN} {src}\n");
        let _ = socket.send_to(reply.as_bytes(), src).await;
        return;
    }

    let parts: Vec<&str> = msg.trim().split_whitespace().collect();

    if msg.trim() == MSG_PONG {
        handle_pong(src, state).await;
        return;
    }

    if parts.len() >= 4 && parts[0] == MSG_HB {
        handle_heartbeat(&parts, src, state).await;
        return;
    }

    if parts.len() >= 1 {
        match parts[0] {
            MSG_CONNECT if parts.len() >= 4 => {
                handle_connect_message(&parts, src, socket, state).await;
            }

            MSG_DISCONNECT if parts.len() >= 4 => {
                handle_disconnect_message(&parts, src, socket, state).await;
            }

            MSG_PEER_TIMEOUT if parts.len() >= 4 => {
                handle_peer_timeout(&parts, src, socket, state).await;
            }

            MSG_REQUEST_RELAY if parts.len() >= 4 => {
                handle_relay_request(&parts, src, socket, state).await;
            }

            MSG_DATA if parts.len() >= 3 => {
                handle_data_from_client(&msg, src, socket, state).await;
            }

            _ => {
                println!("Unknown/Bad packet from {}: {}", src, msg.trim());
            }
        }
    }
}
