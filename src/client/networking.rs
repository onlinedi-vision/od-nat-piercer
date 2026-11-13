use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::routing::send_data;
use crate::client::structures::{NatKind, PeerInfo, PunchSync};
use crate::proto::control_text::{
    MSG_DATA, MSG_HB, MSG_HOLE_PUNCH, MSG_NAT_PROBE, MSG_NAT_SEEN, NAT_TYPE_CONE,
    NAT_TYPE_SYMMETRIC,
};

const PUNCH_INITIAL_SLEEP_MS: u64 = 150; //initial sleep between punches
const PUNCH_MAX_SLEEP_MS: u64 = 1500; // max sleep value between punches
const HEARTBEAT_SLEEP_SEC: u64 = 20; // sleep between heartbeats
const NAT_DETECT_TOTAL_TIMEOUT_MS: u64 = 600; // maximum waiting time for server to respond to both probes
const NAT_DETECT_POLL_SLEEP_MS: u64 = 20; // sleep between polls when socket is WouldBlock

#[must_use]
pub fn detect_nat_kind(socket: &UdpSocket, signaling_ip: &str) -> NatKind {
    let addr1 = format!("{signaling_ip}:2131");
    let addr2 = format!("{signaling_ip}:2132");

    let _ = socket.send_to(format!("{MSG_NAT_PROBE} 1\n").as_bytes(), &addr1);
    let _ = socket.send_to(format!("{MSG_NAT_PROBE} 2\n").as_bytes(), &addr2);

    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_millis(NAT_DETECT_TOTAL_TIMEOUT_MS);

    while seen.len() < 2 && Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((len, _src)) => {
                let msg = String::from_utf8_lossy(&buf[..len]).to_string();
                if msg.starts_with(&format!("{MSG_NAT_SEEN} "))
                    && let Some(addr_str) = msg.split_whitespace().nth(1)
                    && let Ok(observed) = addr_str.parse::<std::net::SocketAddr>()
                {
                    seen.push(observed);
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
        println!("NAT detection: {NAT_TYPE_CONE} (same addr on both ports): {a}");
        NatKind::Cone
    } else {
        println!("NAT detection: {NAT_TYPE_SYMMETRIC} (different ports): {a} vs {b}");
        NatKind::Symmetric
    }
}

/// Requests local worker shutdown and wakes paused/sleeping workers.
pub fn stop_workers(sync: &PunchSync) {
    let (lock, wake) = &**sync;
    let mut state = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.stopping = true;
    wake.notify_all();
}

#[must_use]
pub fn workers_stopping(sync: &PunchSync) -> bool {
    sync.0.lock().map_or(true, |state| state.stopping)
}

fn wait_for_worker_tick(sync: &PunchSync, duration: Duration) -> bool {
    let (lock, wake) = &**sync;
    let Ok(state) = lock.lock() else {
        return false;
    };
    let Ok((state, _)) = wake.wait_timeout_while(state, duration, |state| !state.stopping) else {
        return false;
    };
    !state.stopping
}

pub fn start_heartbeat(
    socket: UdpSocket,
    server_id: String,
    channel: String,
    user: String,
    signaling_addr: String,
    sync: PunchSync,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let hb = format!("{MSG_HB} {server_id} {channel} {user}");
        while !workers_stopping(&sync) {
            if let Err(error) = socket.send_to(hb.as_bytes(), &signaling_addr) {
                eprintln!("Failed registration heartbeat: {error}");
            }
            if !wait_for_worker_tick(&sync, Duration::from_secs(HEARTBEAT_SLEEP_SEC)) {
                break;
            }
        }
    })
}

fn hole_punching_loop(socket: &UdpSocket, peers: &Arc<Mutex<Vec<PeerInfo>>>, sync: &PunchSync) {
    let mut backoff: HashMap<String, u64> = HashMap::new(); //username -> ms

    loop {
        // block if we are paused (sending via server) -> block here
        {
            let (lock, cvar) = &**sync;
            let Ok(mut state) = lock.lock() else {
                eprintln!("Hole punching stopped: punch state mutex is poisoned");
                return;
            };

            while state.paused && !state.stopping {
                //here the thread doesn't consume CPU
                let Ok(next_state) = cvar.wait(state) else {
                    eprintln!("Hole punching stopped: punch state mutex is poisoned");
                    return;
                };
                state = next_state;
            }

            if state.stopping {
                return;
            }
        }

        {
            let Ok(guard) = peers.lock() else {
                eprintln!("Hole punching stopped: peers mutex is poisoned");
                return;
            };
            backoff.retain(|name, _| {
                guard.iter().any(|peer| {
                    &peer.username == name
                        && !peer.connected
                        && !peer.use_server_relay
                        && !peer.relay_requested
                })
            });
            for p in guard.iter() {
                if p.connected || p.use_server_relay || p.relay_requested {
                    continue; //don't punch server-relayed peers
                }

                if let Err(e) = socket.send_to(MSG_HOLE_PUNCH.as_bytes(), p.addr) {
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
        if !wait_for_worker_tick(sync, Duration::from_millis(sleep_ms)) {
            return;
        }
    }
}

pub fn start_hole_punching(
    socket: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    punch_sync: PunchSync,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        hole_punching_loop(&socket, &peers, &punch_sync);
    })
}

pub struct UserMessageContext {
    pub socket: UdpSocket,
    pub peers: Arc<Mutex<Vec<PeerInfo>>>,
    pub username: String,
    pub send_via_server: Arc<AtomicBool>,
    pub signaling_addr: String,
    pub is_relay: Arc<Mutex<bool>>,
    pub channel_has_server_relays: Arc<AtomicBool>,
    pub sync: PunchSync,
}

fn handle_user_message(context: &UserMessageContext, message: &str) -> std::io::Result<()> {
    if workers_stopping(&context.sync) {
        return Ok(());
    }
    let payload = format!("{MSG_DATA} {} {message}\n", context.username);

    // The server is the sole distribution owner for this submission.
    if context.send_via_server.load(Ordering::Acquire) {
        if crate::proto::routed_data::Data::parse(&payload).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid routed DATA",
            ));
        }
        if let Err(error) = context
            .socket
            .send_to(payload.as_bytes(), &context.signaling_addr)
        {
            eprintln!("Failed DATA submission to server: {error}");
        }
        return Ok(());
    }

    let is_relay = *context
        .is_relay
        .lock()
        .map_err(|_| std::io::Error::other("Cannot send message: relay state mutex is poisoned"))?;

    let mirror_to = (is_relay && context.channel_has_server_relays.load(Ordering::Acquire))
        .then_some(context.signaling_addr.as_str());
    send_data(
        &context.socket,
        &context.peers,
        &payload,
        is_relay,
        mirror_to,
    )
}

fn user_input_loop(context: &UserMessageContext, input: impl std::io::BufRead) {
    for msg in input.lines().map_while(Result::ok) {
        let msg = msg.trim();
        if msg.is_empty() {
            continue;
        }
        if let Err(e) = handle_user_message(context, msg) {
            if e.kind() == std::io::ErrorKind::InvalidInput {
                eprintln!("Cannot send input: {e}");
                continue;
            }
            eprintln!("User input stopped: {e}");
            break;
        }
    }
    stop_workers(&context.sync);
}

pub fn start_user_input(context: UserMessageContext) {
    thread::spawn(move || {
        let input = std::io::stdin();
        user_input_loop(&context, input.lock());
    });
}

#[cfg(test)]
mod routing_tests;
