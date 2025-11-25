use std::{
    env,
    net::{ToSocketAddrs, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use od_nat_piercer::client::{
    handlers::*,
    networking::*,
    structures::{NatKind, PeerInfo, PunchState, PunchSync, RelayState, RelaySync},
};

const SETUP_POLL_SLEEP_MS: u64 = 20;
const MAIN_POLL_SLEEP_MS: u64 = 50;
const SETUP_DEADLINE_MS: u64 = 1500;

fn parse_arguments(args: Vec<String>) -> (String, String, String, String, u16) {
    if args.len() < 6 {
        eprintln!("Usage: client <signaling_ip> <server_id> <channel> <user> <local_port>");
        std::process::exit(1);
    }

    let signaling_ip = args[1].clone();
    let server_id = args[2].clone();
    let channel = args[3].clone();
    let user = args[4].clone();
    let local_port: u16 = args[5].parse().expect("Invalid port number");

    (signaling_ip, server_id, channel, user, local_port)
}

fn setup_socket(local_port: u16) -> UdpSocket {
    //Socket UDP local
    let socket = UdpSocket::bind(("0.0.0.0", local_port)).expect("Failed to bind socket");
    socket
        .set_nonblocking(true)
        .expect("Failed to set non-blocking");

    socket
}

fn send_connect_message(
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
        NatKind::Unknown => "UNKOWN",
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

fn server_responses_during_setup(
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

fn main_loop(
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

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let (signaling_ip, server_id, channel, user, local_port) = parse_arguments(args);

    let socket = setup_socket(local_port);

    // NAT detection before CONNECT
    let my_nat = detect_nat_kind(&socket, &signaling_ip);
    println!("My NAT kind: {:?}", my_nat);

    //Address for the signalization server (UDP on port 2131)
    let signaling_addr = format!("{}:2131", signaling_ip);
    let server_socketaddr: std::net::SocketAddr = signaling_addr
        .to_socket_addrs()
        .expect("resolve signaling server")
        .next()
        .expect("no addr for signaling server");

    send_connect_message(
        &socket,
        &signaling_addr,
        &server_id,
        &channel,
        &user,
        my_nat,
    );

    let peers: Vec<PeerInfo> = Vec::new();
    let peers = Arc::new(Mutex::new(peers));

    let is_relay = Arc::new(Mutex::new(false));
    let relay_started = Arc::new(Mutex::new(false));

    let send_via_server = Arc::new(AtomicBool::new(false)); //this client must send via server
    let channel_has_server_relays = Arc::new(AtomicBool::new(false)); //as relay, i should also mirror to serveri

    let punch_sync: PunchSync = Arc::new((
        Mutex::new(PunchState { paused: false }),
        std::sync::Condvar::new(),
    ));

    let relay_sync: RelaySync = Arc::new((
        Mutex::new(RelayState { is_active: false }),
        std::sync::Condvar::new(),
    ));

    start_heartbeat(
        socket.try_clone()?,
        server_id.to_string(),
        channel.to_string(),
        user.to_string(),
        signaling_addr.clone(),
    );

    server_responses_during_setup(
        &socket,
        &peers,
        &user,
        &is_relay,
        &channel_has_server_relays,
        &send_via_server,
        &server_socketaddr,
        &signaling_addr,
        &punch_sync,
        &relay_sync,
    );

    // start punching ONLY if NAT is not symmetric
    if my_nat != NatKind::Symmetric {
        start_hole_punching(
            socket.try_clone()?,
            Arc::clone(&peers),
            Arc::clone(&punch_sync),
        );
    } else {
        println!("Symmetric NAT detected - skipping hole punching, relying on server relay.");
    }

    //Relay keepalive thread starter
    start_relay_keepalive(
        socket.try_clone()?,
        Arc::clone(&peers),
        Arc::clone(&relay_started),
        server_id.to_string(),
        channel.to_string(),
        signaling_addr.clone(),
        Arc::clone(&relay_sync),
    );

    //Thread for sending messages
    start_user_input(
        socket.try_clone()?,
        Arc::clone(&peers),
        user.to_string(),
        Arc::clone(&send_via_server),
        signaling_addr.clone(),
        Arc::clone(&is_relay),
        Arc::clone(&channel_has_server_relays),
    );

    main_loop(
        &socket,
        &peers,
        user,
        &is_relay,
        &channel_has_server_relays,
        &signaling_addr,
        &server_socketaddr,
        &punch_sync,
        &relay_sync,
        &send_via_server,
    )
}
