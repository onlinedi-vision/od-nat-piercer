use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::{Duration, Instant}};
use tokio::{net::UdpSocket, sync::Mutex};

#[derive(Clone, Debug)]
struct User {
    name: String,
    addr: SocketAddr,
    last_pong: Instant,
}

#[derive(Clone, Debug, Default)]
struct Channel{
    users: Vec<User>,
    relay: Option<String>, //username of the relay
}

type ServerMap = HashMap<String, HashMap<String, Channel>>; //server_id -> channel_name -> Vec<User>

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind("0.0.0.0:5000").await?;
    println!("Signaling server listening on 0.0.0.0:5000");

    let state = Arc::new(Mutex::new(ServerMap::new()));
    let socket = Arc::new(socket);
    let mut buf = [0u8; 1024];

    // Start heartbeat task (collect addresses to ping while holding lock, then send without lock)
    let heartbeat_socket = Arc::clone(&socket);
    let heartbeat_state = Arc::clone(&state);
    tokio::spawn(async move{
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        loop{
            interval.tick().await;

            //Collect pings and removals while holding the lock
            let (to_ping, mut to_cleanup): (Vec<SocketAddr>, Vec<(String, String)>) ={
                let mut st = heartbeat_state.lock().await;
                let mut pings = Vec::new();
                let mut clean = Vec::new();

                let server_ids: Vec<String> = st.keys().cloned().collect();
                for sid in server_ids.iter(){
                    if let Some(channels) = st.get_mut(sid){
                        let channel_names: Vec<String> = channels.keys().cloned().collect();
                        for cname in channel_names.iter(){
                            if let Some(channel) = channels.get_mut(cname){
                                //check users for timeouts
                                let mut i = 0;
                                while i < channel.users.len(){
                                    let user = &channel.users[i];

                                    //Only apply timeout if there are other users in the channel
                                    let is_lone_user = channel.users.len() == 1;

                                    let is_relay_user = channel.relay.as_ref().map(|r| r == &user.name).unwrap_or(false);
                                    
                                    if !is_lone_user && !is_relay_user && user.last_pong.elapsed() > Duration::from_secs(60){
                                        println!("User {} disconnected (timeout) from {}-{}", user.name, sid, cname);
                                        channel.users.remove(i);
                                    }else{
                                        i += 1;
                                    }
                                }

                                //if channel has only one user, ping him to keep NAT connection
                                if channel.users.len() == 1{
                                    pings.push(channel.users[0].addr);
                                }

                                //if channel empty, mark for cleanup
                                if channel.users.is_empty(){
                                    clean.push((sid.clone(), cname.clone()));
                                }
                            }
                        }
                    }
                }
                (pings, clean)
            }; //lock dropped here
            // Send pings without holding the lock
            for addr in to_ping{
                if let Err(e) = heartbeat_socket.send_to(b"PING", addr).await{
                    eprintln!("Failed to send PING: {}", e);
                }
            }

            //Cleanup empty channels / servers
            if !to_cleanup.is_empty(){
                let mut st = heartbeat_state.lock().await;
                for(sid, cname) in to_cleanup.drain(..){
                    if let Some(chans) = st.get_mut(&sid){
                        chans.remove(&cname);
                        if chans.is_empty(){
                            st.remove(&sid);
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
        if msg.trim() == "PONG"{
            let mut st = state.lock().await;
            for(_, channels) in st.iter_mut(){
                for(_, channel) in channels.iter_mut(){
                    if let Some(user) = channel.users.iter_mut().find(|u| u.addr == src){
                        user.last_pong = Instant::now();
                    }
                }
            }
            continue;
        }
        
        if parts.len() >= 1 {
            match parts[0] {
                "CONNECT" if parts.len() >= 4 => {
                    //CONNECT server_id channel_name user_name [client_provided_addr]
                    let server_id = parts[1].to_string();
                    let channel_name = parts[2].to_string();
                    let user_name = parts[3].to_string();

                    //Use src as address, ignore client provided addr, because it might be different behind NAT than the UDP provided one
                    let src_addr = src;

                    //Add or update user under lock, but collect notifications to send after releasing the lock
                    let (users_to_notify, is_new_user) = {
                        let mut st = state.lock().await;
                        let channels = st.entry(server_id.clone()).or_default();
                        let channel = channels.entry(channel_name.clone()).or_default();

                        //check if user already present with same addr
                        if let Some(existing) = channel.users.iter_mut().find(|u| u.name == user_name && u.addr == src_addr){
                            existing.last_pong = Instant::now();
                            (channel.clone(), false)
                        }else{
                            //if name exists but with different addr, remove old one
                            channel.users.retain(|u| !(u.name == user_name && u.addr != src_addr));

                            channel.users.push(User{
                                name: user_name.clone(),
                                addr: src_addr,
                                last_pong: Instant::now()
                            });
                            
                            // promote user to relay if there's more than 1 user in the channel
                            if channel.relay.is_none() && channel.users.len() == 1{
                                //server maintains connection via ping
                                let reply = "MODE SERVER_RELAY\n";
                                if let Err(e) = socket.send_to(reply.as_bytes(), channel.users[0].addr).await{
                                    eprintln!("Failed to notify lone user about server relay: {}", e);
                                }
                            }
                        
                            println!("User {} joined {}-{} from {}", user_name, server_id, channel_name, src_addr);
                            (channel.clone(), true)
                        }
                    }; //lock dropped
                                        
                    if !is_new_user{
                        //nothing to notify
                        continue;
                    }

                    // Now decide messages to send based on cloned channel state
                    if users_to_notify.users.len() > 1 && users_to_notify.relay.is_none(){
                        //promote first user to relay
                        let relay_user = &users_to_notify.users[0];
                    
                        //Mark relay in the channel
                        {
                            let mut st = state.lock().await;
                            if let Some(channels) = st.get_mut(&server_id){
                                if let Some(channel) = channels.get_mut(&channel_name){
                                    channel.relay = Some(relay_user.name.clone());
                                }
                            }
                        }
                        
                        //Notify the new relay about all other users
                        for peer in users_to_notify.users.iter().skip(1){
                            let msg = format!("MODE DIRECT {} {}\n", peer.name, peer.addr);
                            if let Err(e) = socket.send_to(msg.as_bytes(), relay_user.addr).await{
                                eprintln!("Failed to notify relay {} about {}: {}", relay_user.name, peer.name, e);
                            }
                        }

                        //Notify other users about the new relay
                        for peer in users_to_notify.users.iter().skip(1){
                            let msg = format!("MODE DIRECT {} {}\n", relay_user.name, relay_user.addr);
                            if let Err(e) = socket.send_to(msg.as_bytes(), peer.addr).await{
                                eprintln!("Failed to notify {} about new relay {}: {}", peer.name, relay_user.name, e);
                            }
                        }
                        
                        //Finally notify the relay itself
                        let reply = "MODE RELAY\n";
                        if let Err(e) = socket.send_to(reply.as_bytes(), relay_user.addr).await{
                            eprintln!("Failed to send RELAY mode to {}: {}", relay_user.name, e);
                        }
                    } else {
                        //Notify existing users about the new user, and notify new user about existing users
                        for user in users_to_notify.users.iter(){
                            if user.addr != src_addr{
                                //notify existing user about the new one
                                let msg_to_existing = format!("MODE DIRECT {} {}\n", user_name, src_addr);
                                if let Err(e) = socket.send_to(msg_to_existing.as_bytes(), user.addr).await{
                                    eprintln!("Failed to notify {}: {}", user.name, e);
                                }

                                //notify new user about the existing user
                                let msg_to_new = format!("MODE DIRECT {} {}\n", user.name, user.addr);
                                if let Err(e) = socket.send_to(msg_to_new.as_bytes(), src_addr).await{
                                    eprintln!("Failed to notify new user: {}", e);
                                }
                            }
                        }
                    }
                }
                            
                "DISCONNECT" if parts.len() >= 4 => {
                    //DISCONNECT server_id channel_name user_name
                    let server_id = parts[1].to_string();
                    let channel_name = parts[2].to_string();
                    let user_name = parts[3].to_string();
                    let src_addr = src;

                    //We'll only remove the user if the src matches the stored addr for that username
                    let (remaining_users, was_relay, leaving_user_addr, lone_user_addr) = {
                        let mut st = state.lock().await;
                        let mut was_relay = false;
                        let mut leaving_user_addr = None;
                        let mut lone_user_addr = None;

                        if let Some(channels) = st.get_mut(&server_id){
                            if let Some(channel) = channels.get_mut(&channel_name){
                                //find index of user with matching name and addr
                                if let Some(pos) = channel.users.iter().position(|u| u.name == user_name && u.addr == src_addr){
                                    //check if leaving user was relay
                                    was_relay = channel.relay.as_ref().map(|r| r == &user_name).unwrap_or(false);
                                    leaving_user_addr = Some(channel.users[pos].addr);
                                    channel.users.remove(pos);

                                    //update relay if needed
                                    if was_relay{
                                        if !channel.users.is_empty(){
                                            //set first user in list as relay
                                            let new_relay = channel.users[0].name.clone();
                                            channel.relay = Some(new_relay);
                                        }else{
                                             channel.relay = None;
                                        }
                                    }

                                    //If only one user left, make sure they are marked relay
                                    if channel.users.len() == 1 {
                                        channel.relay = Some(channel.users[0].name.clone());
                                        lone_user_addr = Some(channel.users[0].addr);
                                    }

                                    println!("User {} left {}-{}", user_name, server_id, channel_name);
                                }else{
                                    //unknown session, so ignore
                                    println!("Ignoring DISCONNECT for {} from {} (no matching session)", user_name, src_addr);                               
                                }
                            }
                        }
                    
                        let remaining_users = st
                            .get(&server_id)
                            .and_then(|channels| channels.get(&channel_name))
                            .map(|channel| channel.clone())
                            .unwrap_or_default();

                        (remaining_users.users, was_relay, leaving_user_addr, lone_user_addr)
                    }; //lock dropped

                    //Notify lone user they are now relay
                    if let Some(lone_user_addr) = lone_user_addr{
                        let reply = "MODE RELAY\n";
                        if let Err(e) = socket.send_to(reply.as_bytes(), lone_user_addr).await{
                            eprintln!("Failed to notify lone user: {}", e);
                        }
                    }

                    //If the leaving user was the relay and there are still users, promote the first and notify
                    if was_relay && remaining_users.len() > 1{
                        let new_relay = &remaining_users[0];
                        if let Err(e) = socket.send_to("MODE RELAY\n".as_bytes(), new_relay.addr).await{
                            eprintln!("Failed to notify {} about new relay: {}", new_relay.name, e);
                        }

                        for user in remaining_users.iter().skip(1) {
                            let msg = format!("MODE DIRECT {} {}\n", new_relay.name, new_relay.addr);
                            if let Err(e) = socket.send_to(msg.as_bytes(), user.addr).await{
                                eprintln!("Failed to notify {} about new relay: {}", user.name, e);
                            }
                        }
                    }

                    //Notify remaining users about the departure
                    for user in remaining_users {
                        let departure_msg = format!("USER_LEFT {} {}\n", user_name, leaving_user_addr.map(|a| a.to_string()).unwrap_or_else(|| "0.0.0.0:0".into()));
                        if let Err(e) = socket.send_to(departure_msg.as_bytes(), user.addr).await{
                            eprintln!("Failed to notify {} about departure: {}", user.name, e);
                        }
                    }
                }

                "PEER_TIMEOUT" if parts.len() >= 4 => {
                    //PEER TIMEOUT server_id channel_name username
                    let server_id = parts[1].to_string();
                    let channel_name = parts[2].to_string();
                    let peer_user = parts[3].to_string();

                    let remaining = {
                        let mut st = state.lock().await;
                        if let Some(channels) = st.get_mut(&server_id){
                            if let Some(channel) = channels.get_mut(&channel_name){
                                //removing by name
                                channel.users.retain(|u| u.name != peer_user);

                                //update relay if needed
                                if channel.users.len() == 1 {
                                    channel.relay = Some(channel.users[0].name.clone());
                                }
                            }
                        }

                        st.get(&server_id)
                            .and_then(|channels| channels.get(&channel_name))
                            .cloned()
                    };

                    if let Some(channel) = remaining{
                        for u in channel.users{
                            let _ = socket.send_to(format!("USER_LEFT {} 0.0.0.0:0\n", peer_user).as_bytes(), u.addr).await;    
                        }
                    }
                }

                _ => {
                println!("Unknown/Bad packet from {}: {}", src, msg.trim());
                }
            }
        }
    }
}
            
        
   
