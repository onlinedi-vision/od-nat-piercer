use std::{ 
    env, net::{SocketAddr, UdpSocket}, sync::{Arc,Mutex}, str::FromStr, thread, time::{Duration, Instant}
};


#[derive(Clone, Debug)]
struct PeerInfo{
    addr: SocketAddr,
    last_pong: Instant,
    username: String,
    connected: bool, //whether we've received any packet from them
}

fn main() -> std::io::Result<()> {
    //CLI arguments: signaling_ip, client_ip(ignored), server, channel, user, local_port
    let args: Vec<String> = env::args().collect();
    if args.len() < 7 {
        eprintln!(
            "Usage: client <signaling_ip> <client_ip(ignored)> <server_id> <channel> <user> <local_port>"
        );
        std::process::exit(1);
    }

    let signaling_ip = &args[1];
    //let client_ip = &args[2];
    let server_id = &args[3];
    let channel = &args[4];
    let user = &args[5];
    let local_port: u16 = args[6].parse().expect("Invalid port number");

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

    // 2. Aggregate server responses for a short window
    let setup_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < setup_deadline{
        match socket.recv_from(&mut buf){
            Ok((len, _)) => {
                let resp = String::from_utf8_lossy(&buf[..len]).to_string();
                println!("Server response:\n{}", resp);
                for line in resp.lines(){
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

    // Start hole punching tasks for known peers
    {
        let peers_clone = Arc::clone(&peers);
        let socket_clone = socket.try_clone()?;
        thread::spawn(move || {
            //We'll keep punching until peer.connected == true or timeout
            let punch_start = Instant::now();
            while punch_start.elapsed() < Duration::from_secs(15){
                let guard = peers_clone.lock().unwrap();
                for p in guard.iter(){
                    if !p.connected{
                        if let Err(e) = socket_clone.send_to(b"HOLE_PUNCH", p.addr){
                            eprintln!("Failed to send punch to{}: {}", p.addr, e);
                        }else{
                            println!("Sent UDP punch to {} ({})", p.username, p.addr);
                        }
                    }
                }
                drop(guard);
                thread::sleep(Duration::from_millis(500));
            }
        });
    }


    // 3. Relay keepalive thread starter
    {
        let socket_clone = socket.try_clone()?;
        let peers_clone = Arc::clone(&peers);
        let is_relay_clone = Arc::clone(&is_relay);
        let relay_started_clone = Arc::clone(&relay_started);

        //Clone the String so the thread own them, otherwise i get a warning
        let server_id = server_id.to_string();
        let channel = channel.to_string();
        let signaling_addr = signaling_addr.to_string();
        
        thread::spawn(move || {
            loop{
                if *is_relay_clone.lock().unwrap(){
                    let mut started = relay_started_clone.lock().unwrap();
                    //Only start one relay loop
                    if *started{
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    *started = true;
                    drop(started);
                    println!("Starting relay keepalive thread");

                    loop{
                        thread::sleep(Duration::from_secs(20));
                        let mut to_remove = Vec::new();
                        {
                            let mut guard = peers_clone.lock().unwrap();
                            for(i, peer) in guard.iter_mut().enumerate(){
                                if peer.last_pong.elapsed() > Duration::from_secs(60){
                                    println!("Peer {} timeout - reporting to server", peer.username);
                                    let _ = socket_clone.send_to(
                                        format!("PEER_TIMEOUT {} {} {}", server_id, channel, peer.username).as_bytes(),
                                        &signaling_addr,
                                    );
                                    to_remove.push(i);
                                }else{
                                    if let Err(e) = socket_clone.send_to(b"PING", peer.addr){
                                        eprintln!("Failed to send PING to {}: {}", peer.addr, e);
                                    }
                                }
                            }

                            for &idx in to_remove.iter().rev(){
                                guard.remove(idx);
                            }
                        }

                        if !*is_relay_clone.lock().unwrap(){
                            //we lost relay role
                            *relay_started_clone.lock().unwrap() = false;
                            break;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
    }

    println!("Starting main message loop...");
    loop{
        match socket.recv_from(&mut buf){
            Ok((len, src)) => {
                let message = String::from_utf8_lossy(&buf[..len]).to_string();
                //If message comes from a peer addr, mark them as connected
                {
                    let mut guard = peers.lock().unwrap();
                    if let Some(p) = guard.iter_mut().find(|p| p.addr == src){
                        p.last_pong = Instant::now();
                        p.connected = true;
                    }
                }

                for line in message.lines() {
                    let line = line.trim();
                    if line.is_empty() { continue; }
                    match line{
                        "PING" => {
                            let _ = socket.send_to(b"PONG", src);
                            println!("Received PING from {}, sent PONG", src);
                        },
                        "PONG" => {
                            //update last_pong if this is a known peer
                            let mut peers_guard = peers.lock().unwrap();
                            if let Some(peer) = peers_guard.iter_mut().find(|p| p.addr == src){
                                peer.last_pong = Instant::now();
                                println!("Received PONG from {}", peer.username);
                            }
                        },
                        "HOLE_PUNCH" => {
                            println!("Received hole punch from {}", src);
                            //mark peer connected
                            let mut peers_guard = peers.lock().unwrap();
                            if let Some(peer) = peers_guard.iter_mut().find(|p| p.addr == src){
                                peer.connected = true;
                                peer.last_pong = Instant::now();
                            }
                        },
                        _ => {
                            //Handle control messages: MODE / USER_LEFT
                            handle_mode_line(line, &peers, user, &is_relay);
                        }
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            },
            Err(e) => {
                eprintln!("Error receiving: {}", e);
                return Err(e);
            }
        }
    }

    // unreachable
   // Ok(())
}

fn handle_mode_line(line: &str, peers: &Arc<Mutex<Vec<PeerInfo>>>, me: &str, is_relay: &Arc<Mutex<bool>>){
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty(){ return; }

    match parts[0]{
        "MODE" if parts.len() >= 2 && parts[1] == "RELAY" => {
            let mut r = is_relay.lock().unwrap();
            if !*r {
                *r = true;
                println!("You are in RELAY MODE");
            }
        },
        "MODE" if parts.len() >= 2 && parts[1] == "SERVER_RELAY" => {
              println!("Server is currently being relay for the lone user.");
        },
        "MODE" if parts.len() >= 4 && parts[1] == "DIRECT" => {
            let username = parts[2];
            let addr_str = parts[3];
            if let Ok(addr) = SocketAddr::from_str(addr_str){
                let mut guard = peers.lock().unwrap();
                if username != me{
                    if !guard.iter().any(|p| p.addr == addr){
                        guard.push(PeerInfo {
                            addr,
                            last_pong: Instant::now(),
                            username: username.to_string(),
                            connected: false,
                        });
                        println!("Added peer {} with addr {}", username, addr_str);
                    }
                }else{
                    println!("Server confirms you ({}) are relay for {}", me, addr_str);
                }
            }
        },
        "USER_LEFT" if parts.len() >= 2 => {
            let username = parts[1];
            let mut guard = peers.lock().unwrap();
            guard.retain(|p| p.username != username.to_string());
            println!("Peer {} left, removed from list", username);
        },
        _ => {
            println!("Unhandled control line: {}", line);
        }
    }
}
