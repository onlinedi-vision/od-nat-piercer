use std::{
    env,
    net::{ToSocketAddrs, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use od_nat_piercer::{
    client::{
        control_plane::start_control_client_after_welcome,
        handlers::{
            IncomingMessageContext, handle_peer_message, process_incoming_message,
            try_handle_welcome,
        },
        networking::{
            detect_nat_kind, start_heartbeat, start_hole_punching, start_relay_keepalive,
            start_user_input,
        },
        structures::{NatKind, PeerInfo, PunchState, PunchSync, RelayState, RelaySync},
    },
    proto::{
        control_text::{
            MSG_CONNECT, MSG_CONTROL, MSG_MODE, MSG_RELAY, MSG_SERVER_RELAY, NAT_TYPE_CONE,
            NAT_TYPE_PUBLIC, NAT_TYPE_SYMMETRIC, NAT_TYPE_UNKNOWN,
        },
        packet::Kind,
    },
};

const SETUP_POLL_SLEEP_MS: u64 = 20;
const MAIN_POLL_SLEEP_MS: u64 = 50;
const SETUP_DEADLINE_MS: u64 = 1500;

struct ClientReceiveContext<'a> {
    incoming: IncomingMessageContext<'a>,
    send_via_server: &'a Arc<AtomicBool>,
    server_socketaddr: &'a std::net::SocketAddr,
    punch_sync: &'a PunchSync,
    relay_sync: &'a RelaySync,
    channel_id: &'a Arc<AtomicU64>,
    my_peer_id: &'a Arc<AtomicU32>,
}

fn parse_arguments(args: &[String]) -> std::io::Result<(String, String, String, String, u16)> {
    if args.len() < 6 {
        eprintln!("Usage: client <signaling_ip> <server_id> <channel> <user> <local_port>");
        std::process::exit(1);
    }

    let signaling_ip = args[1].clone();
    let server_id = args[2].clone();
    let channel = args[3].clone();
    let user = args[4].clone();
    let local_port: u16 = args[5].parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Invalid local port: {e}"),
        )
    })?;

    Ok((signaling_ip, server_id, channel, user, local_port))
}

fn setup_socket(local_port: u16) -> std::io::Result<UdpSocket> {
    //Socket UDP local
    let socket = UdpSocket::bind(("0.0.0.0", local_port))?;
    socket.set_nonblocking(true)?;

    Ok(socket)
}

fn send_connect_message(
    socket: &UdpSocket,
    signaling_addr: &str,
    server_id: &str,
    channel: &str,
    user: &str,
    my_nat: NatKind,
) -> std::io::Result<()> {
    let nat_type = match my_nat {
        NatKind::Symmetric => NAT_TYPE_SYMMETRIC,
        NatKind::Cone => NAT_TYPE_CONE,
        NatKind::Public => NAT_TYPE_PUBLIC,
        NatKind::Unknown => NAT_TYPE_UNKNOWN,
    };

    let connect_msg = format!("{MSG_CONNECT} {server_id} {channel} {user} {nat_type}");
    socket.send_to(connect_msg.as_bytes(), signaling_addr)?;
    println!("Sent {MSG_CONNECT} to signaling server");
    Ok(())
}

fn update_relay_is_active(
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    relay_sync: &RelaySync,
) -> std::io::Result<()> {
    let relay = *is_relay.lock().map_err(|_| {
        std::io::Error::other("Cannot update relay activity: relay role mutex is poisoned")
    })?;
    let active = relay || channel_has_server_relays.load(Ordering::Acquire);
    let (lock, cvar) = &**relay_sync;
    let mut st = lock.lock().map_err(|_| {
        std::io::Error::other("Cannot update relay activity: relay state mutex is poisoned")
    })?;
    if st.is_active != active {
        st.is_active = active;
        cvar.notify_all(); //starts/stops relay loop instantly
    }
    Ok(())
}

fn process_server_response(
    response: &str,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
    send_via_server: &Arc<AtomicBool>,
    punch_sync: &PunchSync,
) -> std::io::Result<()> {
    if !response
        .lines()
        .any(|l| l.trim_start().starts_with(&format!("{MSG_MODE} ")))
    {
        return Ok(());
    }
    println!("Server response:\n{response}");
    for line in response.lines() {
        let line = line.trim();

        if line == format!("{MSG_MODE} {MSG_RELAY}") {
            let (lock, cvar) = &**punch_sync;
            let mut st = lock.lock().map_err(|_| {
                std::io::Error::other("Cannot process server response: punch state mutex is poisoned")
            })?;
            if send_via_server.load(Ordering::Acquire) {
                send_via_server.store(false, Ordering::Release);
                println!("I am relay now - stop sending via server.");
            }

            //resume punching if it was paused
            if st.paused {
                st.paused = false;
                cvar.notify_all();
            }
        }

        if line.starts_with(&format!("{MSG_MODE} {MSG_SERVER_RELAY} ")) {
            // MODE SERVER_RELAY <username>
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let who = parts[2];

                if who == user {
                    let (lock, cvar) = &**punch_sync;
                    let mut st = lock.lock().map_err(|_| {
                        std::io::Error::other(
                            "Cannot process server response: punch state mutex is poisoned",
                        )
                    })?;
                    //i am symmetric (or lone) => send via server
                    if !send_via_server.load(Ordering::Acquire) {
                        send_via_server.store(true, Ordering::Release);
                        println!("I will send via server from now on.");
                    }

                    // stop the thread of punching
                    if !st.paused {
                        st.paused = true;
                        cvar.notify_all();
                    }
                } else {
                    let relay = *is_relay.lock().map_err(|_| {
                        std::io::Error::other(
                            "Cannot process server response: relay role mutex is poisoned",
                        )
                    })?;
                    // i am the relay -> note that channel has server-relayed peers
                    if relay && !channel_has_server_relays.load(Ordering::Acquire) {
                        channel_has_server_relays.store(true, Ordering::Release);
                        println!("Relay will mirror traffic to server.");
                    }
                }
            }
        }
    }
    Ok(())
}

fn handle_recv_result(
    context: &ClientReceiveContext<'_>,
    result: std::io::Result<(usize, std::net::SocketAddr)>,
    buf: &[u8],
    saw_mode: &mut bool,
) -> std::io::Result<bool> {
    let incoming = &context.incoming;

    match result {
        Ok((len, src)) => {
            if let Some((hdr, payload)) = od_nat_piercer::proto::packet::decode(&buf[..len]) {
                match hdr.kind {
                    Kind::Control => {
                        println!(
                            "(setup) Got {MSG_CONTROL} {} bytes from {src}",
                            payload.len()
                        );

                        //temporarly: if payload is text, process as before
                        if let Ok(s) = std::str::from_utf8(payload) {
                            println!("(setup) {MSG_CONTROL} payload: {}", s.trim());
                            try_handle_welcome(s, context.channel_id, context.my_peer_id);
                            if &src == context.server_socketaddr {
                                if s.lines()
                                    .any(|l| l.trim_start().starts_with(&format!("{MSG_MODE} ")))
                                {
                                    *saw_mode = true;
                                }

                                // 1) Normal processing: MODE / DATA / USER_LEFT, etc
                                process_incoming_message(incoming, s, src)?;

                                // 2) Local mode + send_via_server / punching behaviors
                                process_server_response(
                                    s,
                                    incoming.user,
                                    incoming.is_relay,
                                    incoming.channel_has_server_relays,
                                    context.send_via_server,
                                    context.punch_sync,
                                )?;

                                update_relay_is_active(
                                    incoming.is_relay,
                                    incoming.channel_has_server_relays,
                                    context.relay_sync,
                                )?;
                            } else {
                                // peer traffic (arrived during setup)
                                handle_peer_message(incoming.peers, src)?;

                                process_incoming_message(incoming, s, src)?;
                            }
                        }
                    }
                    Kind::Dtls => {
                        println!("(setup) Got DTLS {} bytes from {}", payload.len(), src);
                    }
                    Kind::Srtp => {
                        println!("(setup) Got SRTP {} bytes from {}", payload.len(), src);
                    }
                }
                return Ok(true);
            }
            let resp = String::from_utf8_lossy(&buf[..len]).to_string();
            if &src == context.server_socketaddr {
                if resp
                    .lines()
                    .any(|l| l.trim_start().starts_with(&format!("{MSG_MODE} ")))
                {
                    *saw_mode = true;
                }

                // 1) Normal processing: MODE / DATA / USER_LEFT, etc
                process_incoming_message(incoming, &resp, src)?;

                // 2) Local mode + send_via_server / punching behaviors
                process_server_response(
                    &resp,
                    incoming.user,
                    incoming.is_relay,
                    incoming.channel_has_server_relays,
                    context.send_via_server,
                    context.punch_sync,
                )?;

                update_relay_is_active(
                    incoming.is_relay,
                    incoming.channel_has_server_relays,
                    context.relay_sync,
                )?;
            } else {
                // peer traffic (arrived during setup)
                handle_peer_message(incoming.peers, src)?;

                process_incoming_message(incoming, &resp, src)?;
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(SETUP_POLL_SLEEP_MS));
            Ok(true)
        }
        Err(e) => {
            eprintln!("recv error during setup: {e}");
            Ok(false)
        }
    }
}

fn server_responses_during_setup(
    context: &ClientReceiveContext<'_>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    let setup_deadline = Instant::now() + Duration::from_millis(SETUP_DEADLINE_MS);
    let mut saw_mode = false;

    while Instant::now() < setup_deadline && !saw_mode {
        let res = context.incoming.socket.recv_from(&mut buf);
        if !handle_recv_result(context, res, &buf, &mut saw_mode)? {
            break;
        }
    }
    Ok(())
}

fn main_loop(
    context: &ClientReceiveContext<'_>,
    signaling_ip: &str,
    server_id: &str,
    channel: &str,
) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    let mut control_client_started = false;
    let incoming = &context.incoming;

    println!("Starting main message loop...");

    let pid = context.my_peer_id.load(Ordering::Acquire);

    start_control_client_after_welcome(
        &mut control_client_started,
        signaling_ip,
        server_id,
        channel,
        incoming.user,
        pid,
    );

    loop {
        match incoming.socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                if let Some((hdr, payload)) = od_nat_piercer::proto::packet::decode(&buf[..len]) {
                    match hdr.kind {
                        Kind::Control => {
                            println!("Got {MSG_CONTROL} {} bytes from {src}", payload.len());
                            if let Ok(s) = std::str::from_utf8(payload) {
                                println!("{MSG_CONTROL} payload: {}", s.trim());
                                try_handle_welcome(s, context.channel_id, context.my_peer_id);

                                process_incoming_message(incoming, s, src)?;
                            }
                        }
                        Kind::Dtls => println!("Got DTLS {} bytes from {}", payload.len(), src),
                        Kind::Srtp => println!("Got SRTP {} bytes from {}", payload.len(), src),
                    }
                    let pid = context.my_peer_id.load(Ordering::Acquire);

                    start_control_client_after_welcome(
                        &mut control_client_started,
                        signaling_ip,
                        server_id,
                        channel,
                        incoming.user,
                        pid,
                    );

                    continue;
                }

                let message = String::from_utf8_lossy(&buf[..len]).to_string();

                if &src == context.server_socketaddr {
                    // 1) Process MODE / DATA / USER_LEFT etc.
                    process_incoming_message(incoming, &message, src)?;

                    // 2) Update send_via_server + punching according to MODE lines
                    process_server_response(
                        &message,
                        incoming.user,
                        incoming.is_relay,
                        incoming.channel_has_server_relays,
                        context.send_via_server,
                        context.punch_sync,
                    )?;
                } else {
                    // Peer traffic
                    handle_peer_message(incoming.peers, src)?;

                    process_incoming_message(incoming, &message, src)?;
                }

                update_relay_is_active(
                    incoming.is_relay,
                    incoming.channel_has_server_relays,
                    context.relay_sync,
                )?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(MAIN_POLL_SLEEP_MS));
            }
            Err(e) => {
                eprintln!("Error receiving: {e}");
                return Err(e);
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let (signaling_ip, server_id, channel, user, local_port) = parse_arguments(&args)?;

    let socket = setup_socket(local_port)?;

    let my_peer_id = Arc::new(AtomicU32::new(0));
    let channel_id = Arc::new(AtomicU64::new(0));

    // NAT detection before CONNECT
    let my_nat = detect_nat_kind(&socket, &signaling_ip);
    println!("My NAT kind: {my_nat:?}");

    //Address for the signalization server (UDP on port 2131)
    let signaling_addr = format!("{signaling_ip}:2131");
    let server_socketaddr: std::net::SocketAddr = signaling_addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "No address found for signaling server",
            )
        })?;

    send_connect_message(
        &socket,
        &signaling_addr,
        &server_id,
        &channel,
        &user,
        my_nat,
    )?;

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
        server_id.clone(),
        channel.clone(),
        user.clone(),
        signaling_addr.clone(),
    );

    let incoming = IncomingMessageContext {
        socket: &socket,
        peers: &peers,
        user: &user,
        is_relay: &is_relay,
        channel_has_server_relays: &channel_has_server_relays,
        signaling_addr: &signaling_addr,
    };

    let context = ClientReceiveContext {
        incoming,
        send_via_server: &send_via_server,
        server_socketaddr: &server_socketaddr,
        punch_sync: &punch_sync,
        relay_sync: &relay_sync,
        channel_id: &channel_id,
        my_peer_id: &my_peer_id,
    };

    server_responses_during_setup(&context)?;

    // start punching ONLY if NAT is not symmetric
    if my_nat == NatKind::Symmetric {
        println!("Symmetric NAT detected - skipping hole punching, relying on server relay.");
    } else {
        start_hole_punching(
            socket.try_clone()?,
            Arc::clone(&peers),
            Arc::clone(&punch_sync),
        );
    }

    //Relay keepalive thread starter
    start_relay_keepalive(
        socket.try_clone()?,
        Arc::clone(&peers),
        Arc::clone(&relay_started),
        server_id.clone(),
        channel.clone(),
        signaling_addr.clone(),
        Arc::clone(&relay_sync),
    );

    //Thread for sending messages
    start_user_input(
        socket.try_clone()?,
        Arc::clone(&peers),
        user.clone(),
        Arc::clone(&send_via_server),
        signaling_addr.clone(),
        Arc::clone(&is_relay),
        Arc::clone(&channel_has_server_relays),
    );

    main_loop(&context, &signaling_ip, &server_id, &channel)
}
