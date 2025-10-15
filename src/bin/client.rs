use std::{
    env,
    net::UdpSocket,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
    u8,
};

use od_nat_piercer::client::{handlers::*, networking::*, structures::PeerInfo};

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
) {
    let connect_msg = format!("CONNECT {} {} {}", server_id, channel, user);
    socket
        .send_to(connect_msg.as_bytes(), &signaling_addr)
        .expect("Failed to send CONNECT");
    println!("Sent CONNECT to signaling server");
}

fn process_server_response(
    response: &str,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    server_relay_enabled: &Arc<Mutex<bool>>,
) {
    println!("Server response:\n{}", response);
    for line in response.lines() {
        handle_mode_line(line.trim(), peers, user, is_relay, server_relay_enabled);
    }
}

fn handle_recv_result(
    result: std::io::Result<(usize, std::net::SocketAddr)>,
    buf: &mut [u8],
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    user: &str,
    is_relay: &Arc<Mutex<bool>>,
    server_relay_enabled: &Arc<Mutex<bool>>,
) -> bool {
    match result {
        Ok((len, _)) => {
            let resp = String::from_utf8_lossy(&buf[..len]).to_string();
            process_server_response(&resp, peers, user, is_relay, server_relay_enabled);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(20));
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
    server_relay_enabled: &Arc<Mutex<bool>>,
) {
    let mut buf = [0u8; 1024];
    let setup_deadline = Instant::now() + Duration::from_millis(1500);

    while Instant::now() < setup_deadline {
        let recv_result = socket.recv_from(&mut buf);

        if !handle_recv_result(
            recv_result,
            &mut buf,
            peers,
            user,
            is_relay,
            server_relay_enabled,
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
    server_relay_enabled: &Arc<Mutex<bool>>,
    signaling_addr: &str,
) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];

    println!("Starting main message loop...");
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let message = String::from_utf8_lossy(&buf[..len]).to_string();

                //If message comes from a peer addr, mark them as connected
                handle_peer_message(&peers, src);
                process_incoming_message(
                    socket,
                    &message,
                    src,
                    peers,
                    &user,
                    is_relay,
                    server_relay_enabled,
                    signaling_addr,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
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

    //Address for the signalization server (UDP on port 5000)
    let signaling_addr = format!("{}:5000", signaling_ip);

    send_connect_message(&socket, &signaling_addr, &server_id, &channel, &user);

    let peers: Vec<PeerInfo> = Vec::new();
    let peers = Arc::new(Mutex::new(peers));

    let is_relay = Arc::new(Mutex::new(false));
    let relay_started = Arc::new(Mutex::new(false));
    let server_relay_enabled = Arc::new(Mutex::new(false));

    start_heartbeat(
        socket.try_clone()?,
        server_id.to_string(),
        channel.to_string(),
        user.to_string(),
        signaling_addr.clone(),
    );

    server_responses_during_setup(&socket, &peers, &user, &is_relay, &server_relay_enabled);

    start_hole_punching(socket.try_clone()?, Arc::clone(&peers));

    //Relay keepalive thread starter
    start_relay_keepalive(
        socket.try_clone()?,
        Arc::clone(&peers),
        Arc::clone(&is_relay),
        Arc::clone(&relay_started),
        server_id.to_string(),
        channel.to_string(),
        signaling_addr.clone(),
        Arc::clone(&server_relay_enabled),
    );

    //Thread for sending messages
    start_user_input(
        socket.try_clone()?,
        Arc::clone(&peers),
        user.to_string(),
        Arc::clone(&server_relay_enabled),
        signaling_addr.clone(),
    );

    main_loop(
        &socket,
        &peers,
        user,
        &is_relay,
        &server_relay_enabled,
        &signaling_addr,
    )
}
