use crate::proto::packet::{BROADCAST, Header, Kind, encode};
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
    if msg.starts_with("NAT_PROBE") {
        let reply = format!("NAT_SEEN {}\n", src);
        let _ = socket.send_to(reply.as_bytes(), src).await;
        return;
    }

    let parts: Vec<&str> = msg.trim().split_whitespace().collect();

    if msg.trim() == "PONG" {
        handle_pong(src, state).await;
        return;
    }

    if parts.len() >= 4 && parts[0] == "HB" {
        handle_heartbeat(&parts, src, state).await;
        return;
    }

    if parts.len() >= 1 {
        match parts[0] {
            "CONNECT" if parts.len() >= 4 => {
                handle_connect_message(&parts, src, socket, state).await;
            }

            "DISCONNECT" if parts.len() >= 4 => {
                handle_disconnect_message(&parts, src, socket, state).await;
            }

            "PEER_TIMEOUT" if parts.len() >= 4 => {
                handle_peer_timeout(&parts, src, socket, state).await;
            }

            "REQUEST_RELAY" if parts.len() >= 4 => {
                handle_relay_request(&parts, src, socket, state).await;
            }

            "DATA" if parts.len() >= 3 => {
                handle_data_from_client(&msg, src, socket, state).await;
            }

            "CONTROL_TEST" => {
                println!("CONTROL_TEST from {}", src);

                let payload = b"CONTROL_TEST_OK\n";
                let hdr = Header {
                    kind: Kind::Control,
                    flags: 0,
                    channel_id: 0,
                    src_peer_id: 0,
                    dst_peer_id: BROADCAST,
                    stream_id: 0,
                    payload_len: payload.len() as u16,
                };
                let pkt = encode(hdr, payload);
                let _ = socket.send_to(&pkt, src).await;
            }

            _ => {
                println!("Unknown/Bad packet from {}: {}", src, msg.trim());
            }
        }
    }
}
