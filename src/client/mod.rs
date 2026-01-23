pub mod api;
pub mod handlers;
pub mod networking;
pub mod structures;

use crate::client::{
    handlers::*,
    structures::{NatKind, PeerInfo, PunchSync, RelaySync},
};

use std::net::UdpSocket;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const SETUP_POLL_SLEEP_MS: u64 = 20;
const MAIN_POLL_SLEEP_MS: u64 = 50;
const SETUP_DEADLINE_MS: u64 = 1500;

pub fn setup_socket(local_port: u16) -> UdpSocket {
    //Socket UDP local
    let socket = UdpSocket::bind(("0.0.0.0", local_port)).expect("Failed to bind socket");
    socket
        .set_nonblocking(true)
        .expect("Failed to set non-blocking");

    socket
}

pub fn send_connect_message(
    socket: &UdpSocket,
    signaling_addr: &str,
    server_id: &str,
    channel: &str,
    user: &str,
    my_nat: NatKind,
) {
    let nat_type = match my_nat {
        NatKind::Symmetric => "SYMMETRIC",
        NatKind::Cone => "CONE",
        NatKind::Public => "PUBLIC",
        NatKind::Unknown => "UNKNOWN",
    };

    let connect_msg = format!("CONNECT {} {} {} {}", server_id, channel, user, nat_type);
    socket
        .send_to(connect_msg.as_bytes(), &signaling_addr)
        .expect("Failed to send CONNECT");
    println!("Sent CONNECT to signaling server");
}

fn update_relay_is_active(
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    relay_sync: &RelaySync,
) {
    let active = *is_relay.lock().unwrap() || channel_has_server_relays.load(Ordering::Acquire);
    let (lock, cvar) = &**relay_sync;
    let mut st = lock.lock().unwrap();
    if st.is_active != active {
        st.is_active = active;
        cvar.notify_all(); //starts/stops relay loop instantly
    }
}

fn handle_recv_result(
    result: std::io::Result<(usize, std::net::SocketAddr)>,
    buf: &mut [u8],
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    send_via_server: &Arc<AtomicBool>,
    server_socketaddr: &std::net::SocketAddr,
    signaling_addr: &str,
    saw_mode: &mut bool,
    punch_sync: &PunchSync,
    relay_sync: &RelaySync,
) -> bool {
    match result {
        Ok((len, src)) => {
            let resp = String::from_utf8_lossy(&buf[..len]).to_string();
            if &src == server_socketaddr {
                if resp.lines().any(|l| l.trim_start().starts_with("MODE ")) {
                    *saw_mode = true;
                }

                // 1) Normal processing: MODE / DATA / USER_LEFT, etc
                process_incoming_message(
                    socket,
                    &resp,
                    src,
                    peers,
                    &user,
                    is_relay,
                    channel_has_server_relays,
                    signaling_addr,
                );

                // 2) Local mode + send_via_server / punching behaviors
                process_server_response(
                    &resp,
                    user,
                    is_relay,
                    channel_has_server_relays,
                    send_via_server,
                    punch_sync,
                );

                update_relay_is_active(is_relay, channel_has_server_relays, relay_sync);
            } else {
                // peer traffic (arrived during setup)
                handle_peer_message(peers, src);

                process_incoming_message(
                    socket,
                    &resp,
                    src,
                    peers,
                    &user,
                    is_relay,
                    channel_has_server_relays,
                    signaling_addr,
                );
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(SETUP_POLL_SLEEP_MS));
            true
        }
        Err(e) => {
            eprintln!("recv error during setup: {}", e);
            false
        }
    }
}

pub fn server_responses_during_setup(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    send_via_server: &Arc<AtomicBool>,
    server_socketaddr: &std::net::SocketAddr,
    signaling_addr: &str,
    punch_sync: &PunchSync,
    relay_sync: &RelaySync,
) {
    let mut buf = [0u8; 1024];
    let setup_deadline = Instant::now() + Duration::from_millis(SETUP_DEADLINE_MS);
    let mut saw_mode = false;

    while Instant::now() < setup_deadline && !saw_mode {
        let res = socket.recv_from(&mut buf);
        if !handle_recv_result(
            res,
            &mut buf,
            socket,
            peers,
            user,
            is_relay,
            channel_has_server_relays,
            send_via_server,
            server_socketaddr,
            signaling_addr,
            &mut saw_mode,
            punch_sync,
            relay_sync,
        ) {
            break;
        }
    }
}

fn process_server_response(
    response: &str,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    send_via_server: &Arc<AtomicBool>,
    punch_sync: &PunchSync,
) {
    if !response
        .lines()
        .any(|l| l.trim_start().starts_with("MODE "))
    {
        return;
    }
    println!("Server response:\n{}", response);
    for line in response.lines() {
        let line = line.trim();

        if line == "MODE RELAY" {
            if send_via_server.load(Ordering::Acquire) {
                send_via_server.store(false, Ordering::Release);
                println!("I am relay now - stop sending via server.");
            }

            //resume punching if it was paused
            let (lock, cvar) = &**punch_sync;
            let mut st = lock.lock().unwrap();
            if st.paused {
                st.paused = false;
                cvar.notify_all();
            }
        }

        if line.starts_with("MODE SERVER_RELAY ") {
            // MODE SERVER_RELAY <username>
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let who = parts[2];

                if who == user {
                    //i am symmetric (or lone) => send via server
                    if !send_via_server.load(Ordering::Acquire) {
                        send_via_server.store(true, Ordering::Release);
                        println!("I will send via server from now on.");
                    }

                    // stop the thread of punching
                    let (lock, cvar) = &**punch_sync;
                    let mut st = lock.lock().unwrap();
                    if !st.paused {
                        st.paused = true;
                        cvar.notify_all();
                    }
                } else if *is_relay.lock().unwrap() {
                    // i am the relay -> note that channel has server-relayed peers
                    if !channel_has_server_relays.load(Ordering::Acquire) {
                        channel_has_server_relays.store(true, Ordering::Release);
                        println!("Relay will mirror traffic to server.");
                    }
                }
            }
        }
    }
}

pub fn main_loop(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: String,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    signaling_addr: &str,
    server_socketaddr: &std::net::SocketAddr,
    punch_sync: &PunchSync,
    relay_sync: &RelaySync,
    send_via_server: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];

    println!("Starting main message loop...");
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let message = String::from_utf8_lossy(&buf[..len]).to_string();

                if &src == server_socketaddr {
                    // 1) Process MODE / DATA / USER_LEFT etc.
                    process_incoming_message(
                        socket,
                        &message,
                        src,
                        peers,
                        &user,
                        is_relay,
                        channel_has_server_relays,
                        signaling_addr,
                    );

                    // 2) Update send_via_server + punching according to MODE lines
                    process_server_response(
                        &message,
                        &user,
                        is_relay,
                        channel_has_server_relays,
                        send_via_server,
                        punch_sync,
                    );
                } else {
                    // Peer traffic
                    handle_peer_message(&peers, src);

                    process_incoming_message(
                        socket,
                        &message,
                        src,
                        peers,
                        &user,
                        is_relay,
                        channel_has_server_relays,
                        signaling_addr,
                    );
                }

                update_relay_is_active(is_relay, channel_has_server_relays, relay_sync);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(MAIN_POLL_SLEEP_MS));
            }
            Err(e) => {
                eprintln!("Error receiving: {}", e);
                return Err(e);
            }
        }
    }
}
