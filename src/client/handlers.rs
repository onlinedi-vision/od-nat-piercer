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
                    created_at: Instant::now(),
                    use_server_relay: false,
                    relay_requested: false,
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
    channel_has_server_relays: &Arc<Mutex<bool>>,
) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "MODE" if parts.len() >= 2 => match parts[1] {
            "RELAY" => handle_mode_relay(is_relay),

            "SERVER_RELAY" => {
                if parts.len() >= 3 {
                    let username = parts[2];

                    // if the server is relaying for 'me', i must send to server
                    if username == me {
                        // we'll set a global flag in bin/process_server_response
                        println!("Server will relay my traffic now: {}", me);
                    } else {
                        // if i am the RELAY, note that the channel has server-relayed peers
                        if *is_relay.lock().unwrap() {
                            let mut f = channel_has_server_relays.lock().unwrap();
                            if !*f {
                                *f = true;
                                println!("Relay: channel has server-relayed peers.");
                            }
                        } else {
                            println!("Server will relay for user: {}", username);
                        }

                        //mark that peer as server-relayed to stop punching it
                        let mut guard = peers.lock().unwrap();
                        if let Some(peer) = guard.iter_mut().find(|p| p.username == username) {
                            peer.use_server_relay = true;
                            peer.connected = true; // stop punching
                            peer.relay_requested = true;
                        }
                    }
                } else {
                    // just a lone user :(
                    println!("Server is currently being relay for the lone user.");
                }
            }

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
        let was_connected = peer.connected;
        peer.connected = true;
        peer.last_pong = Instant::now();
        if !was_connected {
            println!(
                "Peer {} is now CONNECTED (punch) at {}.",
                peer.username, peer.addr
            );
        }
    } else {
        println!("Hole punch received from unknown peer: {}", src);
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
    channel_has_server_relays: &Arc<Mutex<bool>>,
    signaling_addr: &str,
) {
    if line.starts_with("DATA ") {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            let sender = parts[1];
            let text = parts[2];
            println!("[{}]: {}", sender, text);

            //NO MATTER THE SOURCE, we've observed sender activity
            mark_peer_connected_by_name(peers, sender);

            if *is_relay.lock().unwrap() {
                // 1) transmit to peers
                handle_relay_message_to_peers(socket, peers, sender, line);

                // 2) mirror to server only if the channel has server-relayed users
                if *channel_has_server_relays.lock().unwrap() {
                    let _ = socket.send_to(line.as_bytes(), signaling_addr);
                }
            } else {
                // non-relay receiving data does not forward further
                // if this non-relay is symmetric and has to send via server, that's handled in networking.rs
            }
        }
    }
}

pub fn handle_peer_message(peers: &Arc<Mutex<Vec<PeerInfo>>>, src: std::net::SocketAddr) {
    let mut guard = peers.lock().unwrap();
    if let Some(p) = guard.iter_mut().find(|p| p.addr == src) {
        let was_connected = p.connected;
        p.last_pong = Instant::now();
        p.connected = true;

        if !was_connected {
            println!(
                "Peer {} is now CONNECTED (direct) at {}.",
                p.username, p.addr
            );
        }
    }
}

pub fn handle_mode_server_relay_for(
    username: &str,
    me: &str,
    server_relay_flag: &Arc<Mutex<bool>>,
) {
    if username == me {
        let mut flag = server_relay_flag.lock().unwrap();
        *flag = true;
        println!("Server will relay my traffic now: {}", me);
    } else {
        println!("Server will relay for user {}", username);
    }
}

// we will call this function when DATA sender... is seen (even received from the server)
fn mark_peer_connected_by_name(peers: &Arc<Mutex<Vec<PeerInfo>>>, name: &str) {
    let mut guard = peers.lock().unwrap();
    if let Some(peer) = guard.iter_mut().find(|p| p.username == name) {
        let was_connected = peer.connected;
        peer.connected = true;
        peer.last_pong = Instant::now();
        if !was_connected {
            println!(
                "Peer {} is now CONNECTED (via server) at {}.",
                peer.username, peer.addr
            );
        }
    }
}

pub fn process_incoming_message(
    socket: &UdpSocket,
    message: &str,
    src: std::net::SocketAddr,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    server_relay_enabled: &Arc<Mutex<bool>>,
    signaling_addr: &str,
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
                    handle_data_message(
                        socket,
                        line,
                        peers,
                        is_relay,
                        server_relay_enabled,
                        signaling_addr,
                    );
                } else {
                    //Handle control messages: MODE / USER_LEFT
                    handle_mode_line(line, peers, user, is_relay, server_relay_enabled);
                }
            }
        }
    }
}
