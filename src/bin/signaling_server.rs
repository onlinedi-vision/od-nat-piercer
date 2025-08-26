use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::{Duration, Instant}};

use tokio::{net::UdpSocket, sync::Mutex};

#[derive(Clone, Debug)]
struct User {
    name: String,
    addr: SocketAddr,
    last_pong: Instant,
}

type Channel = Vec<User>;
type ServerMap = HashMap<String, HashMap<String, Channel>>; //server_id -> channel_name -> Vec<User>

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind("0.0.0.0:5000").await?;
    println!("Signaling server listening on 0.0.0.0:5000");

    let state = Arc::new(Mutex::new(ServerMap::new()));
    let socket = Arc::new(socket);
    let mut buf = [0u8; 1024];

    // Start heartbeat task
    let heartbeat_socket = Arc::clone(&socket);
    let heartbeat_state = Arc::clone(&state);
    tokio::spawn(async move{
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        loop{
            interval.tick().await;
            let mut state = heartbeat_state.lock().await;

            for(_, channels) in state.iter_mut(){
                for(_, channel) in channels.iter_mut(){
                    //Check for lone users
                    if channel.len() == 1 {
                        let user = &mut channel[0];

                        //Check if user is responsive
                        if user.last_pong.elapsed() > Duration::from_secs(60){
                            println!("User {} disconnected (timeout)", user.name);
                            channel.clear(); //Remove user
                        }else{
                            //Send ping
                            if let Err(e) = heartbeat_socket.send_to(b"PING", user.addr).await{
                                eprintln!("Failed to send PING: {}", e);
                            }
                        }
                    }
                }
            }
        }
    });

    loop {
        let (len, src) = socket.recv_from(&mut buf).await?;
        let msg = String::from_utf8_lossy(&buf[..len]).to_string();
        let parts: Vec<&str> = msg.trim().split_whitespace().collect();

        // Handle PONG messages first
        if msg == "PONG"{
            let mut state = state.lock().await;
            for(_, channels) in state.iter_mut(){
                for(_, channel) in channels.iter_mut(){
                    if let Some(user) = channel.iter_mut().find(|u| u.addr == src){
                        user.last_pong = Instant::now();
                        break;
                    }
                }
            }
            continue;
        }
        
        if parts.len() >= 4 && parts[0] == "CONNECT" {
            let server_id = parts[1].to_string();
            let channel_name = parts[2].to_string();
            let user_name = parts[3].to_string();

            //Get data quickly and release lock
            let (users_to_notify, is_new_user) = {
                let mut st = state.lock().await;
                let channels = st.entry(server_id.clone()).or_default();
                let channel = channels.entry(channel_name.clone()).or_default();
            }

            let is_new_user = !channel.iter().any(|u| u.name == user_name && u.addr == src);

            if is_new_user{
                channel.push(User {
                    name: user_name.clone(),
                    addr: src,
                    last_pong: Instant::now(),
                });
                println!(
                    "User {} joined {}-{} from {}",
                    user_name, server_id, channel_name, src
                );
            }else{
                println!(
                    "User {} at {} already in {}-{}, updating last_pong",
                    user_name, src, server_id, channel_name
                )
                //Update last_pong for existing user
                if let Some(user) = channel.iter_mut().find(|u| u.name == user_name && u.addr == src){
                    user.last_pong = Instant::now();
                }
            }

            //Clone users for notification (release lock early)
            (channel.clone(), is_new_user)
        };

        if !is_new_user{
            continue; //Skip notification for existing users
        }

        //Handle notifications without holding the lock
        if users_to_notify.len() == 1 {
                //First user -> relay mode
                let reply = "MODE RELAY\n";
                if let Err(e) = socket.send_to(reply.as_bytes(), src).await{
                    eprintln!("Failed to send RELAY mode: {}", e);
                }
        }else{
            //Notify all users about the new user
            for user in users_to_notify.iter() {
                if user.addr != src { //Don't send self-notification
                    let msg_to_existing = format!("MODE DIRECT {} {}", user_name, src);
                    if let Err(e) = socket.send_to(msg_to_existing.as_bytes(), user.addr).await{
                        eprintln!("Failed to notify {}: {}", user.name, e);
                    }

                    // Notify new user about existing users
                    let msg_to_new = format!("MODE DIRECT {} {}", user.name, user.addr);
                    if let Err(e) = socket.send_to(msg_to_new.as_bytes(), src).await{
                        eprintln!("Failed to notify new user: {}", e);
                    }
                }
            }
        }
    } else if parts.len() >= 4 && parts[0] == "DISCONNECT" {
            let server_id = parts[1].to_string();
            let channel_name = parts[2].to_string();
            let user_name = parts[3].to_string();

            //Get data quickly and release lock
            let(remaining_users, lone_user_addr) = {
                let mut st = state.lock().await;
                let mut was_relay = false;
                let mut leaving_user_addr = None;

                if let Some(channels) = st.get_mut(&server_id){
                    if let Some(channel) = channels.get_mut(&channel_name){
                        //Checking if leaving user was the relay
                        was_relay = channel.first().map(|u| u.name == user_name).unwrap_or(false);
                        leaving_user_addr = channel.iter().find(|u| u.name == user_name).map(|u| u.addr);

                        //Remove the user
                        channel.retain(|u| u.name != user_name);
                        println!("User {} left {}-{}", user_name, server_id, channel_name);

                        //If there's only one user left, mark them as relay
                        if channel.len() == 1 {
                            lone_user_addr = Some(channel[0].addr);
                            (channel.clone(), was_relay, leaving_user_addr, lone_user_addr)
                        }else{
                            (channel.clone(), was_relay, leaving_user_addr, None)
                        }
                    }else{
                        (Vec::new(), false, None, None)
                    }
                }else{
                    (Vec::new(), false, None, None)
                }
            };

            //Handle notifications withoud holding the lock
            if let Some(lone_user_addr) = lone_user_addr {
                let reply = "MODE RELAY\n";
                if let Err(e) = socket.send_to(reply.as_bytes(), lone_user_addr).await{
                    eprintln!("Failed to notify lone user: {}", e);
                }
            }

            //Handle relay reassignment if the leaving user was the relay
            if was_relay && remaining_users.len() > 1 {
                //The first remaining user becomes the new relay
                let new_relay = &remaining_users[0];

                //Tell the new relay to swtich to relay mode
                if let Err(e) = socket.send_to("MODE RELAY\n".as_bytes(), new_relay.addr).await{
                    eprintln!("Failed to promote new relay: {}", e);
                }

                //Tell all other users to connect to the new relay
                for user in remaining_users.iter().skip(1){
                    let msg = format!("MODE DIRECT {} {}\n", new_relay.name, new_relay.addr);
                    if let Err(e) = socket.send_to(msg.as_bytes(), user.addr).await{
                        eprintln!("Failed to notify {} about new relay: {}", user.name, e);
                    }
                }
            }

            //Notify remaining users about the departure
            for user in remaining_users{
                let departure_msg = format!("USER_LEFT {} {}\n", user_name, leaving_user_addr.unwrap_or(src));
                if let Err(e) = socket.send_to(departure_msg.as_bytes(), user.addr).await{
                    eprintln!("Failed to notify {} about departure: {}", user.name, e);
                }
            }
        }
    }
}
