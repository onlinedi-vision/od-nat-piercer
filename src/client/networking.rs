use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::structures::{NatKind, PeerInfo, PunchSync, RelaySync};

const PUNCH_INITIAL_SLEEP_MS: u64 = 150; //initial sleep between punches
const PUNCH_MAX_SLEEP_MS: u64 = 1500; // max sleep value between punches
const HEARTBEAT_SLEEP_SEC: u64 = 20; // sleep between heartbeats
const RELAY_TICK_SEC: u64 = 15; // how often the relay does keepalive work
const PEER_TIMEOUT_SEC: u64 = 60; //peer timeout if no PONG message in this time
const CONNECT_GRACE_SEC: u64 = 12; // wait for connection for this time, after this, ask server for relay
const NAT_DETECT_TOTAL_TIMEOUT_MS: u64 = 600; // maximum waiting time for server to respond to both probes
const NAT_DETECT_POLL_SLEEP_MS: u64 = 20; // sleep between polls when socket is WouldBlock

pub fn detect_nat_kind(socket: &UdpSocket, signaling_ip: &str) -> NatKind {
    let addr1 = format!("{}:2131", signaling_ip);
    let addr2 = format!("{}:2132", signaling_ip);

    let _ = socket.send_to(b"NAT_PROBE 1\n", &addr1);
    let _ = socket.send_to(b"NAT_PROBE 2\n", &addr2);

    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_millis(NAT_DETECT_TOTAL_TIMEOUT_MS);

    while seen.len() < 2 && Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((len, _src)) => {
                let msg = String::from_utf8_lossy(&buf[..len]).to_string();
                if msg.starts_with("NAT_SEEN ") {
                    if let Some(addr_str) = msg.split_whitespace().nth(1) {
                        if let Ok(observed) = addr_str.parse::<std::net::SocketAddr>() {
                            seen.push(observed);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(NAT_DETECT_POLL_SLEEP_MS));
            }
            Err(_) => break,
        }
    }

    if seen.len() < 2 {
        println!("NAT detection: insufficient responses, treating as Unknown");
        return NatKind::Unknown;
    }

    let a = seen[0];
    let b = seen[1];

    if a.ip() != b.ip() {
        println!("NAT detection: different public IPs, treating as Symmetric");
        return NatKind::Symmetric;
    }

    if a.port() == b.port() {
        println!("NAT detection: CONE(same addr on both ports): {}", a);
        NatKind::Cone
    } else {
        println!("NAT detection: SYMMETRIC (different ports): {} vs {}", a, b);
        NatKind::Symmetric
    }
}

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
        thread::sleep(Duration::from_secs(HEARTBEAT_SLEEP_SEC));
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

fn hole_punching_loop(socket: UdpSocket, peers: Arc<Mutex<Vec<PeerInfo>>>, sync: PunchSync) {
    let mut backoff: HashMap<String, u64> = HashMap::new(); //username -> ms

    loop {
        // block if we are paused (sending via server) -> block here
        {
            let (lock, cvar) = &*sync;
            let mut state = lock.lock().unwrap();

            while state.paused {
                //here the thread doesn't consume CPU
                state = cvar.wait(state).unwrap();
            }

            //here state.paused == false, we can punch
        }

        {
            let guard = peers.lock().unwrap();
            for p in guard.iter() {
                if p.connected || p.use_server_relay {
                    continue; //don't punch server-relayed peers
                }

                if let Err(e) = socket.send_to(b"HOLE_PUNCH", p.addr) {
                    eprintln!("Failed to send punch to {}: {}", p.addr, e);
                } else {
                    println!("Sent UDP punch to {} ({})", p.username, p.addr);
                }

                let entry = backoff
                    .entry(p.username.clone())
                    .or_insert(PUNCH_INITIAL_SLEEP_MS);
                *entry = (*entry).saturating_mul(2).min(PUNCH_MAX_SLEEP_MS);
            }
        }

        let sleep_ms = backoff
            .values()
            .min()
            .copied()
            .unwrap_or(PUNCH_INITIAL_SLEEP_MS);
        thread::sleep(Duration::from_millis(sleep_ms));
    }
}

pub fn start_hole_punching(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    punch_sync: PunchSync,
) {
    thread::spawn(move || {
        hole_punching_loop(socket, peers, punch_sync);
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
    server_id: &str,
    channel: &str,
    signaling_addr: &str,
    relay_sync: &RelaySync,
) {
    loop {
        // stop immediatly if deactivated
        {
            let (lock, _) = &**relay_sync;
            if !lock.lock().unwrap().is_active {
                break;
            }
        }

        let mut to_remove = Vec::new();
        {
            let mut guard = peers.lock().unwrap();
            for (i, peer) in guard.iter_mut().enumerate() {
                if peer.use_server_relay {
                    continue;
                }

                if peer.last_pong.elapsed() > Duration::from_secs(PEER_TIMEOUT_SEC) {
                    handle_peer_timeout(socket, server_id, channel, peer, signaling_addr);
                    to_remove.push(i);
                } else {
                    if let Err(e) = socket.send_to(b"PING", peer.addr) {
                        eprintln!("Failed to send PING to {}: {}", peer.addr, e);
                    }

                    if !peer.connected
                        && peer.created_at.elapsed() > Duration::from_secs(CONNECT_GRACE_SEC)
                        && !peer.relay_requested
                    {
                        println!(
                            "Peer {} not connected after 10s - requesting server relay",
                            peer.username
                        );
                        peer.relay_requested = true;
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
        // sleep up to 15s, but wake instantly if is_active flips
        let (lock, cvar) = &**relay_sync;
        let st = lock.lock().unwrap();
        let _ = cvar
            .wait_timeout(st, Duration::from_secs(RELAY_TICK_SEC))
            .unwrap();
    }
}

fn relay_keepalive_loop(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    relay_started: Arc<Mutex<bool>>,
    server_id: String,
    channel: String,
    signaling_addr: String,
    relay_sync: RelaySync,
) {
    loop {
        // wait until active
        {
            let (lock, cvar) = &*relay_sync;
            let mut st = lock.lock().unwrap();
            while !st.is_active {
                st = cvar.wait(st).unwrap();
            }
        }

        // mark started, only once
        {
            let mut started = relay_started.lock().unwrap();
            if !*started {
                *started = true;
                println!("Starting relay keepalive thread.")
            }
        }

        // run main loop until deactivated
        relay_main_loop(
            &socket,
            &peers,
            &server_id,
            &channel,
            &signaling_addr,
            &relay_sync,
        );

        // mark stopped
        {
            let mut started = relay_started.lock().unwrap();
            *started = false;
            println!("Relay loop stopped.");
        }

        //go back to waiting
    }
}

pub fn start_relay_keepalive(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    relay_started: Arc<Mutex<bool>>,
    server_id: String,
    channel: String,
    signaling_addr: String,
    relay_sync: RelaySync,
) {
    thread::spawn(move || {
        relay_keepalive_loop(
            socket,
            peers,
            relay_started,
            server_id,
            channel,
            signaling_addr,
            relay_sync,
        );
    });
}

fn handle_user_message(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    username: &str,
    message: &str,
    send_via_server: bool,
    signaling_addr: &str,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
) {
    let payload = format!("DATA {} {}\n", username, message);

    // if i am simmetric, send via server
    if send_via_server {
        println!("Sending DATA via server relay: {}", message);
        let _ = socket.send_to(payload.as_bytes(), signaling_addr);
        return;
    }

    // we on DIRECT, send directly only to peers that are not server relayed
    let peers_guard = peers.lock().unwrap();
    for peer in peers_guard.iter().filter(|p| !p.use_server_relay) {
        let _ = socket.send_to(payload.as_bytes(), peer.addr);
    }

    // if i am relay and channel has server relayed peers, mirror to server
    // otherwise symmetric users can not receive my message
    if *is_relay.lock().unwrap() && channel_has_server_relays.load(Ordering::Acquire) {
        let _ = socket.send_to(payload.as_bytes(), signaling_addr);
    }
}

fn user_input_loop(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    username: String,
    send_via_server: Arc<AtomicBool>,
    signaling_addr: String,
    is_relay: Arc<Mutex<bool>>,
    channel_has_server_relays: Arc<AtomicBool>,
) {
    use std::io::{self, BufRead};
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        if let Ok(msg) = line {
            let msg = msg.trim();
            if msg.is_empty() {
                continue;
            }
            let s = send_via_server.load(Ordering::Acquire);
            handle_user_message(
                &socket,
                &peers,
                &username,
                msg,
                s,
                &signaling_addr,
                &is_relay,
                &channel_has_server_relays,
            );
        }
    }
}

pub fn start_user_input(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    username: String,
    send_via_server: Arc<AtomicBool>,
    signaling_addr: String,
    is_relay: Arc<Mutex<bool>>,
    channel_has_server_relays: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        user_input_loop(
            socket,
            peers,
            username,
            send_via_server,
            signaling_addr,
            is_relay,
            channel_has_server_relays,
        );
    });
}
