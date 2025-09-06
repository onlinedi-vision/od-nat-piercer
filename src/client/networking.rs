use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::client::structures::PeerInfo;

pub fn start_heartbeat(
    socket_clone: UdpSocket,
    server_id: String,
    channel: String,
    user: String,
    signaling_addr: String,
) {
    thread::spawn(move || {
        loop {
            let hb = format!("HB {} {} {}", server_id, channel, user);
            let _ = socket_clone.send_to(hb.as_bytes(), &signaling_addr);
            thread::sleep(Duration::from_secs(20));
        }
    });
}

pub fn start_hole_punching(socket_clone: UdpSocket, peers_clone: Arc<Mutex<Vec<PeerInfo>>>) {
    thread::spawn(move || {
        //We'll keep punching until peer.connected == true or timeout
        loop {
            {
                let guard = peers_clone.lock().unwrap();
                for p in guard.iter() {
                    if !p.connected {
                        if let Err(e) = socket_clone.send_to(b"HOLE_PUNCH", p.addr) {
                            eprintln!("Failed to send punch to{}: {}", p.addr, e);
                        } else {
                            println!("Sent UDP punch to {} ({})", p.username, p.addr);
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

pub fn start_relay_keepalive(
    socket_clone: UdpSocket,
    peers_clone: Arc<Mutex<Vec<PeerInfo>>>,
    is_relay_clone: Arc<Mutex<bool>>,
    relay_started_clone: Arc<Mutex<bool>>,
    server_id: String,
    channel: String,
    signaling_addr: String,
) {
    thread::spawn(move || {
        loop {
            if *is_relay_clone.lock().unwrap() {
                let mut started = relay_started_clone.lock().unwrap();
                //Only start one relay loop
                if *started {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
                *started = true;
                drop(started);
                println!("Starting relay keepalive thread");

                loop {
                    let mut to_remove = Vec::new();
                    {
                        let mut guard = peers_clone.lock().unwrap();
                        for (i, peer) in guard.iter_mut().enumerate() {
                            if peer.last_pong.elapsed() > Duration::from_secs(60) {
                                println!("Peer {} timeout - reporting to server", peer.username);
                                let _ = socket_clone.send_to(
                                    format!(
                                        "PEER_TIMEOUT {} {} {}",
                                        server_id, channel, peer.username
                                    )
                                    .as_bytes(),
                                    &signaling_addr,
                                );
                                to_remove.push(i);
                            } else {
                                if let Err(e) = socket_clone.send_to(b"PING", peer.addr) {
                                    eprintln!("Failed to send PING to {}: {}", peer.addr, e);
                                }
                            }
                        }

                        for &idx in to_remove.iter().rev() {
                            guard.remove(idx);
                        }
                    }

                    if !*is_relay_clone.lock().unwrap() {
                        //we lost relay role
                        *relay_started_clone.lock().unwrap() = false;
                        break;
                    }

                    thread::sleep(Duration::from_secs(15));
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
}

pub fn start_user_input(
    socket_clone: UdpSocket,
    peers_clone: Arc<Mutex<Vec<PeerInfo>>>,
    username: String,
) {
    thread::spawn(move || {
        use std::io::{self, BufRead};
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            if let Ok(msg) = line {
                let msg = msg.trim();
                if msg.is_empty() {
                    continue;
                }
                let payload = format!("DATA {} {}\n", username, msg);
                let peers_guard = peers_clone.lock().unwrap();
                for peer in peers_guard.iter() {
                    let _ = socket_clone.send_to(payload.as_bytes(), peer.addr);
                }
            }
        }
    });
}
