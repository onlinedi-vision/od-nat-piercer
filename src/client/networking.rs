use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::client::structures::PeerInfo;

fn heartbeat_loop(
    socket: UdpSocket,
    server_id: String,
    channel: String,
    user: String,
    signaling_addr: String,
) {
    loop {
        let hb = format!("HB {} {} {}", server_id, channel, user);
        let _ = socket.send_to(hb.as_bytes(), &signaling_addr);
        thread::sleep(Duration::from_secs(20));
    }
}

pub fn start_heartbeat(
    socket: UdpSocket,
    server_id: String,
    channel: String,
    user: String,
    signaling_addr: String,
) {
    thread::spawn(move || {
        heartbeat_loop(socket, server_id, channel, user, signaling_addr);
    });
}

fn hole_punching_loop(socket: UdpSocket, peers: Arc<Mutex<Vec<PeerInfo>>>) {
    loop {
        {
            let guard = peers.lock().unwrap();
            for p in guard.iter() {
                if !p.connected {
                    if let Err(e) = socket.send_to(b"HOLE_PUNCH", p.addr) {
                        eprintln!("Failed to send punch to{}: {}", p.addr, e);
                    } else {
                        println!("Sent UDP punch to {} ({})", p.username, p.addr);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
}

pub fn start_hole_punching(socket: UdpSocket, peers: Arc<Mutex<Vec<PeerInfo>>>) {
    thread::spawn(move || {
        //We'll keep punching until peer.connected == true or timeout
        hole_punching_loop(socket, peers);
    });
}

fn handle_peer_timeout(
    socket: &UdpSocket,
    server_id: &str,
    channel: &str,
    peer: &PeerInfo,
    signaling_addr: &str,
) {
    println!("Peer {} timeout - reporting to server", peer.username);
    let _ = socket.send_to(
        format!("PEER_TIMEOUT {} {} {}", server_id, channel, peer.username).as_bytes(),
        &signaling_addr,
    );
}

fn relay_main_loop(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    is_relay: &Arc<Mutex<bool>>,
    relay_started: &Arc<Mutex<bool>>,
    server_id: &str,
    channel: &str,
    signaling_addr: &str,
    server_relay_enabled: &Arc<Mutex<bool>>,
) {
    loop {
        let mut to_remove = Vec::new();
        {
            let mut guard = peers.lock().unwrap();
            for (i, peer) in guard.iter_mut().enumerate() {
                if peer.last_pong.elapsed() > Duration::from_secs(60) {
                    handle_peer_timeout(socket, server_id, channel, peer, signaling_addr);
                    to_remove.push(i);
                } else {
                    if let Err(e) = socket.send_to(b"PING", peer.addr) {
                        eprintln!("Failed to send PING to {}: {}", peer.addr, e);
                    }

                    if !peer.connected && peer.created_at.elapsed() > Duration::from_secs(5) {
                        println!(
                            "Peer {} not connected after 5s - requesting server relay",
                            peer.username
                        );
                        let _ = socket.send_to(
                            format!(
                                "REQUEST_RELAY {} {} {}\n",
                                server_id, channel, peer.username
                            )
                            .as_bytes(),
                            signaling_addr,
                        );
                    }
                }
            }

            for &idx in to_remove.iter().rev() {
                guard.remove(idx);
            }
        }

        if !*is_relay.lock().unwrap() && !*server_relay_enabled.lock().unwrap() {
            println!("Relay role lost, stopping relay loop.");
            *relay_started.lock().unwrap() = false;
            break;
        }

        thread::sleep(Duration::from_secs(15));
    }
}

fn relay_keepalive_loop(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    is_relay: Arc<Mutex<bool>>,
    relay_started: Arc<Mutex<bool>>,
    server_id: String,
    channel: String,
    signaling_addr: String,
    server_relay_enabled: Arc<Mutex<bool>>,
) {
    loop {
        if *is_relay.lock().unwrap() || *server_relay_enabled.lock().unwrap() {
            let mut started = relay_started.lock().unwrap();
            //Only start one relay loop
            if *started {
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            *started = true;
            drop(started);

            println!("Starting relay keepalive thread");
            relay_main_loop(
                &socket,
                &peers,
                &is_relay,
                &relay_started,
                &server_id,
                &channel,
                &signaling_addr,
                &server_relay_enabled,
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub fn start_relay_keepalive(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    is_relay: Arc<Mutex<bool>>,
    relay_started: Arc<Mutex<bool>>,
    server_id: String,
    channel: String,
    signaling_addr: String,
    server_relay_enabled: Arc<Mutex<bool>>,
) {
    thread::spawn(move || {
        relay_keepalive_loop(
            socket,
            peers,
            is_relay,
            relay_started,
            server_id,
            channel,
            signaling_addr,
            server_relay_enabled,
        );
    });
}

fn handle_user_message(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    username: &str,
    message: &str,
    server_relay_enabled: bool,
    signaling_addr: &str,
) {
    let payload = format!("DATA {} {}\n", username, message);
    if server_relay_enabled {
        println!("Sending DATA via server relay: {}", message);
        let _ = socket.send_to(payload.as_bytes(), signaling_addr);
    } else {
        let peers_guard = peers.lock().unwrap();
        for peer in peers_guard.iter() {
            let _ = socket.send_to(payload.as_bytes(), peer.addr);
        }
    }
}

fn user_input_loop(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    username: String,
    server_relay_enabled: Arc<Mutex<bool>>,
    signaling_addr: String,
) {
    use std::io::{self, BufRead};
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        if let Ok(msg) = line {
            let msg = msg.trim();
            if msg.is_empty() {
                continue;
            }
            let relay_flag = *server_relay_enabled.lock().unwrap();
            handle_user_message(&socket, &peers, &username, msg, relay_flag, &signaling_addr);
        }
    }
}

pub fn start_user_input(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    username: String,
    server_relay_enabled: Arc<Mutex<bool>>,
    signaling_addr: String,
) {
    thread::spawn(move || {
        user_input_loop(
            socket,
            peers,
            username,
            server_relay_enabled,
            signaling_addr,
        );
    });
}
