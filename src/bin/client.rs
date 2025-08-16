use std::{
    env,
    net::{SocketAddr, UdpSocket},
    str::FromStr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn main() -> std::io::Result<()> {
    //CLI arguments: signaling_ip, server, channel, user, local_port
    let args: Vec<String> = env::args().collect();
    if args.len() < 7 {
        eprintln!(
            "Usage: client <signaling_ip> <client_ip> <server_id> <channel> <user> <local_port>"
        );
        std::process::exit(1);
    }

    let signaling_ip = &args[1];
    let client_ip = &args[2];
    let server_id = &args[3];
    let channel = &args[4];
    let user = &args[5];
    let local_port: u16 = args[6].parse().expect("Invalid port number");

    //Socket UDP local
    let socket = UdpSocket::bind(("0.0.0.0", local_port))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;

    //Address for the signalization server (UDP on port 5000)
    let signaling_addr = format!("{}:5000", signaling_ip);

    //Address of the client for sending to the server (here 127.0.0.1:local_port)
    let addr_str = format!("{}:{}", client_ip, local_port);

    //1. Send request to signaling server
    let connect_msg = format!("CONNECT {} {} {} {}", server_id, channel, user, addr_str);
    socket.send_to(connect_msg.as_bytes(), signaling_addr)?;

    //2. Receiving server response
    let mut buf = [0u8; 1024];
    let (len, _) = socket.recv_from(&mut buf)?;
    let resp = String::from_utf8_lossy(&buf[..len]);
    println!("Server response:\n {}", resp);

    let mut peers: Vec<SocketAddr> = Vec::new();
    let mut relay_mode = false;

    //Parsing server respone
    for line in resp.lines() {
        if line.trim() == "MODE RELAY" {
            relay_mode = true;
            println!("You are in RELAY mode (only user in channel)");
        } else if line.starts_with("MODE DIRECT") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 4 {
                // parts[2] -> user of the received peer
                if parts[2] != user {
                    if let Ok(addr) = SocketAddr::from_str(parts[3]) {
                        peers.push(addr);
                        println!("Added peer {} with addr {}", parts[2], parts[3]);
                    } else {
                        eprintln!("Invalid peer address format: {}", parts[3]);
                    }
                } else {
                    println!("Skipping self user {}", parts[2]);
                }
            }
        }
    }

    let socket = Arc::new(socket);
    let peers = Arc::new(peers);

    //Hole punching duration
    let hole_punching_duration = Duration::from_secs(5);
    let start = Instant::now();

    // 2. If we are not in relay -> Hole punching -> sending UDP messages to every peer
    if !relay_mode {
        // Thread for sending UDP punch repeatedly
        let socket_clone = Arc::clone(&socket);
        let peers_clone = Arc::clone(&peers);

        thread::spawn(move || {
            while Instant::now() - start < hole_punching_duration {
                for peer in peers_clone.iter() {
                    if let Err(e) = socket_clone.send_to(b"punch", peer) {
                        eprintln!("Failed to send punch to {}: {}", peer, e);
                    } else {
                        println!("Sent UDP punch to {}", peer);
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
    } else {
        println!("Relay mode active: waiting for incoming messages...");
    }

    //Main thread: listen for incoming UDP packets until timeout
    while Instant::now() - start < hole_punching_duration {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                println!(
                    "Received from {}: {}",
                    src,
                    String::from_utf8_lossy(&buf[..len])
                );
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                //Timeout, continue listening...
            }
            Err(e) => {
                eprintln!("Error receiving UDP packet: {}", e);
                break;
            }
        }
    }

    println!("Finished hole punching / listening phase.");

    Ok(())
}
