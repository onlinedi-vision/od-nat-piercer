use crate::client::structures::PeerInfo;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub fn handle_mode_line(
    line: &str,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    me: &str,
    is_relay: &Arc<Mutex<bool>>,
) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "MODE" if parts.len() >= 2 && parts[1] == "RELAY" => {
            let mut r = is_relay.lock().unwrap();
            if !*r {
                *r = true;
                println!("You are in RELAY MODE");
            }
        }
        "MODE" if parts.len() >= 2 && parts[1] == "SERVER_RELAY" => {
            println!("Server is currently being relay for the lone user.");
        }
        "MODE" if parts.len() >= 4 && parts[1] == "DIRECT" => {
            let username = parts[2];
            let addr_str = parts[3];
            if let Ok(addr) = SocketAddr::from_str(addr_str) {
                let mut guard = peers.lock().unwrap();
                if username != me {
                    if !guard.iter().any(|p| p.addr == addr) {
                        guard.push(PeerInfo {
                            addr,
                            last_pong: Instant::now(),
                            username: username.to_string(),
                            connected: false,
                        });
                        println!("Added peer {} with addr {}", username, addr_str);
                    }
                } else {
                    println!("Server confirms you ({}) are relay for {}", me, addr_str);
                }
            }
        }
        "USER_LEFT" if parts.len() >= 2 => {
            let username = parts[1];
            let mut guard = peers.lock().unwrap();
            guard.retain(|p| p.username != username.to_string());
            println!("[CLIENT:user] {} left, removed from list", username);
        }
        _ => {
            if line != "PING" && line != "HOLE_PUNCH" {
                println!("Unhandled control line: {}", line);
            }
        }
    }
}
