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
        handlers::{IncomingMessageContext, process_incoming_message, try_handle_welcome},
        networking::{
            UserMessageContext, detect_nat_kind, start_heartbeat, start_hole_punching,
            start_user_input, stop_workers, workers_stopping,
        },
        path_health::{PathContext, PathMonitor},
        structures::{NatKind, PeerInfo, PunchState, PunchSync},
    },
    proto::{
        control_text::{
            MSG_CONNECT, MSG_DATA, MSG_MODE, MSG_RELAY, MSG_SERVER_RELAY, NAT_TYPE_CONE,
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
    for line in response.lines() {
        let line = line.trim();

        if line == format!("{MSG_MODE} {MSG_RELAY}") {
            let (lock, cvar) = &**punch_sync;
            let mut st = lock.lock().map_err(|_| {
                std::io::Error::other(
                    "Cannot process server response: punch state mutex is poisoned",
                )
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
                    *is_relay.lock().map_err(|_| {
                        std::io::Error::other("Cannot clear relay role: mutex poisoned")
                    })? = false;
                    channel_has_server_relays.store(false, Ordering::Release);
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

fn handle_datagram(
    context: &ClientReceiveContext<'_>,
    bytes: &[u8],
    src: std::net::SocketAddr,
) -> std::io::Result<bool> {
    let payload = if let Some((header, payload)) = od_nat_piercer::proto::packet::decode(bytes) {
        if header.kind != Kind::Control {
            return Ok(false);
        }
        payload
    } else {
        bytes
    };
    let Ok(message) = std::str::from_utf8(payload) else {
        return Ok(false);
    };
    let incoming = &context.incoming;
    process_incoming_message(incoming, message, src)?;
    if src != *context.server_socketaddr || message.split_whitespace().next() == Some(MSG_DATA) {
        return Ok(false);
    }
    try_handle_welcome(message, context.channel_id, context.my_peer_id);
    process_server_response(
        message,
        incoming.user,
        incoming.is_relay,
        incoming.channel_has_server_relays,
        context.send_via_server,
        context.punch_sync,
    )?;
    Ok(message.lines().any(|line| line.starts_with("MODE ")))
}

fn server_responses_during_setup(context: &ClientReceiveContext<'_>) -> std::io::Result<()> {
    let mut buffer = [0; 2048];
    let deadline = Instant::now() + Duration::from_millis(SETUP_DEADLINE_MS);
    while Instant::now() < deadline {
        match context.incoming.socket.recv_from(&mut buffer) {
            Ok((len, src)) => {
                if handle_datagram(context, &buffer[..len], src)? {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(SETUP_POLL_SLEEP_MS));
            }
            Err(error) => return Err(error),
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
    let mut buffer = [0; 2048];
    let mut monitor = PathMonitor::new(Instant::now());
    let incoming = &context.incoming;
    let mut control_worker = None;
    while !workers_stopping(context.punch_sync) {
        let mut control_started = control_worker.is_some();
        if let Some(worker) = start_control_client_after_welcome(
            &mut control_started,
            signaling_ip,
            server_id,
            channel,
            incoming.user,
            context.my_peer_id.load(Ordering::Acquire),
        ) {
            control_worker = Some(worker);
        }
        let idle = match incoming.socket.recv_from(&mut buffer) {
            Ok((len, src)) => {
                handle_datagram(context, &buffer[..len], src)?;
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(error) => return Err(error),
        };
        let relay = *incoming
            .is_relay
            .lock()
            .map_err(|_| std::io::Error::other("Cannot check paths: relay mutex is poisoned"))?;
        monitor.tick(
            &PathContext {
                socket: incoming.socket,
                peers: incoming.peers,
                server_id,
                channel,
                user: incoming.user,
                signaling_addr: incoming.signaling_addr,
                is_relay: relay,
                send_via_server: context.send_via_server.load(Ordering::Acquire),
            },
            Instant::now(),
        )?;
        if idle {
            thread::sleep(
                Duration::from_millis(MAIN_POLL_SLEEP_MS)
                    .min(monitor.time_until_check(Instant::now())),
            );
        }
    }
    Ok(())
}

struct Workers {
    sync: PunchSync,
    heartbeat: Option<thread::JoinHandle<()>>,
    punching: Option<thread::JoinHandle<()>>,
}

impl Drop for Workers {
    fn drop(&mut self) {
        stop_workers(&self.sync);
        for worker in [self.heartbeat.take(), self.punching.take()]
            .into_iter()
            .flatten()
        {
            if worker.join().is_err() {
                eprintln!("Network worker panicked during shutdown");
            }
        }
    }
}

struct Departure<'a> {
    socket: &'a UdpSocket,
    address: &'a str,
    server: &'a str,
    channel: &'a str,
    user: &'a str,
}

impl Drop for Departure<'_> {
    fn drop(&mut self) {
        let message = format!("DISCONNECT {} {} {}", self.server, self.channel, self.user);
        if let Err(error) = self.socket.send_to(message.as_bytes(), self.address) {
            eprintln!("Failed explicit departure: {error}");
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
    let server_socketaddr: std::net::SocketAddr =
        signaling_addr.to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "No address found for signaling server",
            )
        })?;

    // Workers reuse the resolved numeric address; shutdown never waits on DNS.
    let signaling_addr = server_socketaddr.to_string();
    send_connect_message(
        &socket,
        &signaling_addr,
        &server_id,
        &channel,
        &user,
        my_nat,
    )?;

    let _departure = Departure {
        socket: &socket,
        address: &signaling_addr,
        server: &server_id,
        channel: &channel,
        user: &user,
    };
    let peers: Vec<PeerInfo> = Vec::new();
    let peers = Arc::new(Mutex::new(peers));

    let is_relay = Arc::new(Mutex::new(false));

    let send_via_server = Arc::new(AtomicBool::new(false)); //this client must send via server
    let channel_has_server_relays = Arc::new(AtomicBool::new(false)); //as relay, i should also mirror to serveri

    let punch_sync: PunchSync = Arc::new((
        Mutex::new(PunchState {
            paused: false,
            stopping: false,
        }),
        std::sync::Condvar::new(),
    ));

    let mut workers = Workers {
        sync: Arc::clone(&punch_sync),
        heartbeat: None,
        punching: None,
    };
    workers.heartbeat = Some(start_heartbeat(
        socket.try_clone()?,
        server_id.clone(),
        channel.clone(),
        user.clone(),
        signaling_addr.clone(),
        Arc::clone(&punch_sync),
    ));

    let incoming = IncomingMessageContext {
        socket: &socket,
        peers: &peers,
        user: &user,
        is_relay: &is_relay,
        channel_has_server_relays: &channel_has_server_relays,
        signaling_addr: &signaling_addr,
        server_addr: server_socketaddr,
    };

    let context = ClientReceiveContext {
        incoming,
        send_via_server: &send_via_server,
        server_socketaddr: &server_socketaddr,
        punch_sync: &punch_sync,
        channel_id: &channel_id,
        my_peer_id: &my_peer_id,
    };

    server_responses_during_setup(&context)?;

    // start punching ONLY if NAT is not symmetric
    if my_nat == NatKind::Symmetric {
        println!("Symmetric NAT detected - skipping hole punching, relying on server relay.");
    } else {
        workers.punching = Some(start_hole_punching(
            socket.try_clone()?,
            Arc::clone(&peers),
            Arc::clone(&punch_sync),
        ));
    }

    //Thread for sending messages
    start_user_input(UserMessageContext {
        socket: socket.try_clone()?,
        peers: Arc::clone(&peers),
        username: user.clone(),
        send_via_server: Arc::clone(&send_via_server),
        signaling_addr: signaling_addr.clone(),
        is_relay: Arc::clone(&is_relay),
        channel_has_server_relays: Arc::clone(&channel_has_server_relays),
        sync: Arc::clone(&punch_sync),
    });

    main_loop(
        &context,
        &server_socketaddr.ip().to_string(),
        &server_id,
        &channel,
    )
}

#[cfg(test)]
#[path = "client/tests.rs"]
mod tests;
