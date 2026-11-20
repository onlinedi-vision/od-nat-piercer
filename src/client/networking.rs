use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::routing::send_data;
use crate::client::structures::{PeerInfo, PunchSync};
use crate::proto::control_text::{MSG_DATA, MSG_HB, MSG_HOLE_PUNCH};

const PUNCH_INITIAL_INTERVAL: Duration = Duration::from_millis(150);
const PUNCH_INITIAL_WINDOW: Duration = Duration::from_secs(1);
const PUNCH_LATER_INTERVAL: Duration = Duration::from_millis(1500);
const HEARTBEAT_SLEEP_SEC: u64 = 20; // sleep between heartbeats
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

// The local advertisement timestamp resets scheduling when an address is
// replaced/reintroduced. It is not a protocol session generation.
type PunchSchedule = HashMap<(SocketAddr, Instant), Instant>;

fn needs_punch(peer: &PeerInfo, now: Instant) -> bool {
    !peer.connected
        && !peer.use_server_relay
        && !peer.relay_requested
        && now.saturating_duration_since(peer.created_at)
            < crate::client::path_health::INITIAL_PATH_TIMEOUT
}

fn collect_punches(
    peers: &[PeerInfo],
    schedule: &mut PunchSchedule,
    now: Instant,
) -> Vec<(String, SocketAddr)> {
    schedule.retain(|&(addr, created_at), _| {
        peers.iter().any(|peer| {
            peer.addr == addr && peer.created_at == created_at && needs_punch(peer, now)
        })
    });
    let mut targets = Vec::new();
    for peer in peers.iter().filter(|peer| needs_punch(peer, now)) {
        let next = schedule.entry((peer.addr, peer.created_at)).or_insert(now);
        if now < *next {
            continue;
        }
        let interval = if now.saturating_duration_since(peer.created_at) < PUNCH_INITIAL_WINDOW {
            PUNCH_INITIAL_INTERVAL
        } else {
            PUNCH_LATER_INTERVAL
        };
        // A delayed worker sends once, never a catch-up burst.
        *next = now + interval;
        targets.push((peer.username.clone(), peer.addr));
    }
    targets
}

fn hole_punching_loop(socket: &UdpSocket, peers: &Arc<Mutex<Vec<PeerInfo>>>, sync: &PunchSync) {
    let mut schedule = PunchSchedule::new();

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

        let targets = {
            let Ok(guard) = peers.lock() else {
                eprintln!("Hole punching stopped: peers mutex is poisoned");
                return;
            };
            collect_punches(&guard, &mut schedule, Instant::now())
        };
        for (name, addr) in targets {
            if let Err(error) = socket.send_to(MSG_HOLE_PUNCH.as_bytes(), addr) {
                eprintln!("Failed to send punch to {name} ({addr}): {error}");
            }
        }

        let now = Instant::now();
        let sleep = schedule
            .values()
            .map(|next| next.saturating_duration_since(now))
            .min()
            .unwrap_or(PUNCH_INITIAL_INTERVAL)
            .min(PUNCH_INITIAL_INTERVAL);
        if !wait_for_worker_tick(sync, sleep) {
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

#[cfg(test)]
mod punching_tests;
