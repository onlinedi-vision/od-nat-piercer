use std::{
    env,
    net::UdpSocket,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use od_nat_piercer::client::{handlers::handle_mode_line, networking::*, structures::PeerInfo};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 6 {
        eprintln!("Usage: client <signaling_ip> <server_id> <channel> <user> <local_port>");
        std::process::exit(1);
    }

    let signaling_ip = &args[1];
    let server_id = &args[2];
    let channel = &args[3];
    let user = &args[4];
    let local_port: u16 = args[5].parse().expect("Invalid port number");

    //Socket UDP local
    let socket = UdpSocket::bind(("0.0.0.0", local_port))?;
    socket.set_nonblocking(true)?;

    //Address for the signalization server (UDP on port 5000)
    let signaling_addr = format!("{}:5000", signaling_ip);

    //1. Send CONNECT
    let connect_msg = format!("CONNECT {} {} {}", server_id, channel, user);
    socket.send_to(connect_msg.as_bytes(), &signaling_addr)?;
    println!("Sent CONNECT to signaling server");

    let mut buf = [0u8; 1024];
    let peers: Vec<PeerInfo> = Vec::new();
    let peers = Arc::new(Mutex::new(peers));
    let is_relay = Arc::new(Mutex::new(false));
    let relay_started = Arc::new(Mutex::new(false));

    //Start heartbeat to server every 20s so server knows we're alive
    start_heartbeat(
        socket.try_clone()?,
        server_id.to_string(),
        channel.to_string(),
        user.to_string(),
        signaling_addr.clone(),
    );

    //Agregate server responses for a short window
    let setup_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < setup_deadline {
        match socket.recv_from(&mut buf) {
            Ok((len, _)) => {
                let resp = String::from_utf8_lossy(&buf[..len]).to_string();
                println!("Server response:\n{}", resp);
                for line in resp.lines() {
                    handle_mode_line(line.trim(), &peers, user, &is_relay);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                eprintln!("recv error during setup: {}", e);
                break;
            }
        }
    }

    //Start hole-punching tasks for knows peers
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
    );

    //Thread for sending messages
    start_user_input(socket.try_clone()?, Arc::clone(&peers), user.to_string());

    println!("Starting main message loop...");
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let message = String::from_utf8_lossy(&buf[..len]).to_string();
                //If message comes from a peer addr, mark them as connected
                {
                    let mut guard = peers.lock().unwrap();
                    if let Some(p) = guard.iter_mut().find(|p| p.addr == src) {
                        p.last_pong = Instant::now();
                        p.connected = true;
                    }
                }

                for line in message.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match line {
                        "PING" => {
                            let _ = socket.send_to(b"PONG", src);
                            println!("Received PING from {}, sent PONG", src);
                        }
                        "PONG" => {
                            //update last_pong if this is a known peer
                            let mut peers_guard = peers.lock().unwrap();
                            if let Some(peer) = peers_guard.iter_mut().find(|p| p.addr == src) {
                                peer.last_pong = Instant::now();
                                println!("Received PONG from {}", peer.username);
                            }
                        }
                        "HOLE_PUNCH" => {
                            println!("Received hole punch from {}", src);
                            let mut peers_guard = peers.lock().unwrap();
                            if let Some(peer) = peers_guard.iter_mut().find(|p| p.addr == src) {
                                peer.connected = true;
                                peer.last_pong = Instant::now();
                            }
                        }
                        _ => {
                            if line.starts_with("DATA ") {
                                let parts: Vec<&str> = line.splitn(3, ' ').collect();
                                if parts.len() >= 3 {
                                    let sender = parts[1];
                                    let text = parts[2];
                                    println!("[{}]: {}", sender, text);

                                    if *is_relay.lock().unwrap() {
                                        let peers_guard = peers.lock().unwrap();
                                        for peer in peers_guard.iter() {
                                            if peer.username != sender {
                                                if let Err(e) =
                                                    socket.send_to(line.as_bytes(), peer.addr)
                                                {
                                                    eprintln!(
                                                        "Failed to send data to {}: {}",
                                                        peer.addr, e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                //Handle control messages: MODE / USER_LEFT
                                handle_mode_line(line, &peers, user, &is_relay);
                            }
                        }
                    }
                }
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
