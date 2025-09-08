use crate::client::structures::PeerInfo;
use std::{
    net::{SocketAddr, UdpSocket},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Instant,
};

fn handle_mode_relay(is_relay: &Arc<Mutex<bool>>) {
    let mut r = is_relay.lock().unwrap();
    if !*r {
        *r = true;
        println!("You are in RELAY MODE");
    }
}

fn handle_mode_server_relay() {
    println!("Server is currently being relay for the lone user.");
}

fn handle_mode_direct(parts: &[&str], peers: &Arc<Mutex<Vec<PeerInfo>>>, me: &str) {
    let username = parts[2];
    let addr_str = parts[3];
    if let Ok(addr) = SocketAddr::from_str(addr_str) {
        let mut guard = peers.lock().unwrap();
        if username != me {
            if !guard.iter().any(|p| p.addr == addr) {
                guard.push(PeerInfo {
                    addr,
                    last_pong: Instant::now(),
                    username: username.to_string(),
                    connected: false,
                });
                println!("Added peer {} with addr {}", username, addr_str);
            }
        } else {
            println!("Server confirms you ({}) are relay for {}", me, addr_str);
        }
    }
}

fn handle_user_left(parts: &[&str], peers: &Arc<Mutex<Vec<PeerInfo>>>) {
    let username = parts[1];
    let mut guard = peers.lock().unwrap();
    guard.retain(|p| p.username != username.to_string());
    println!("[CLIENT:user] {} left, removed from list", username);
}

fn handle_unrecognized_command(line: &str) {
    if line != "PING" && line != "HOLE_PUNCH" {
        println!("Unhandled control line: {}", line);
    }
}

pub fn handle_mode_line(
    line: &str,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    me: &str,
    is_relay: &Arc<Mutex<bool>>,
) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "MODE" if parts.len() >= 2 => match parts[1] {
            "RELAY" => handle_mode_relay(is_relay),
            "SERVER_RELAY" => handle_mode_server_relay(),
            "DIRECT" if parts.len() >= 4 => handle_mode_direct(&parts, peers, me),
            _ => handle_unrecognized_command(line),
        },
        "USER_LEFT" if parts.len() >= 2 => handle_user_left(&parts, peers),
        _ => handle_unrecognized_command(line),
    }
}

pub fn handle_ping(socket: &UdpSocket, src: std::net::SocketAddr) {
    let _ = socket.send_to(b"PONG", src);
    println!("Received PING from {}, sent PONG", src);
}

pub fn handle_pong(peers: &Arc<Mutex<Vec<PeerInfo>>>, src: std::net::SocketAddr) {
    let mut peers_guard = peers.lock().unwrap();
    if let Some(peer) = peers_guard.iter_mut().find(|p| p.addr == src) {
        peer.last_pong = Instant::now();
        println!("Received PONG from {}", peer.username);
    }
}

pub fn handle_hole_punch(peers: &Arc<Mutex<Vec<PeerInfo>>>, src: std::net::SocketAddr) {
    println!("Received hole punch from {}", src);
    let mut peers_guard = peers.lock().unwrap();
    if let Some(peer) = peers_guard.iter_mut().find(|p| p.addr == src) {
        peer.connected = true;
        peer.last_pong = Instant::now();
    }
}

fn handle_relay_message_to_peers(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    sender: &str,
    message: &str,
) {
    let peers_guard = peers.lock().unwrap();
    for peer in peers_guard.iter() {
        if peer.username != sender {
            if let Err(e) = socket.send_to(message.as_bytes(), peer.addr) {
                eprintln!("Failed to send data to {}: {}", peer.addr, e);
            }
        }
    }
}

pub fn handle_data_message(
    socket: &UdpSocket,
    line: &str,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    is_relay: &Arc<Mutex<bool>>,
) {
    if line.starts_with("DATA ") {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            let sender = parts[1];
            let text = parts[2];
            println!("[{}]: {}", sender, text);

            if *is_relay.lock().unwrap() {
                handle_relay_message_to_peers(socket, peers, sender, line);
            }
        }
    }
}

pub fn handle_peer_message(peers: &Arc<Mutex<Vec<PeerInfo>>>, src: std::net::SocketAddr) {
    let mut guard = peers.lock().unwrap();
    if let Some(p) = guard.iter_mut().find(|p| p.addr == src) {
        p.last_pong = Instant::now();
        p.connected = true;
    }
}

pub fn process_incoming_message(
    socket: &UdpSocket,
    message: &str,
    src: std::net::SocketAddr,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
) {
    for line in message.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "PING" => handle_ping(socket, src),
            "PONG" => handle_pong(peers, src),
            "HOLE_PUNCH" => handle_hole_punch(peers, src),
            _ => {
                if line.starts_with("DATA ") {
                    handle_data_message(socket, line, peers, is_relay);
                } else {
                    //Handle control messages: MODE / USER_LEFT
                    handle_mode_line(line, peers, user, is_relay);
                }
            }
        }
    }
}
