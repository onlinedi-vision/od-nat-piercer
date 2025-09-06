use crate::signaling::structures::{ServerMap, User};
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tokio::{net::UdpSocket, sync::Mutex};

pub async fn handle_message(
    msg: String,
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let parts: Vec<&str> = msg.trim().split_whitespace().collect();

    // Handle PONG messages first (relay PING <-> client PONG)
    if msg.trim() == "PONG" {
        let mut st = state.lock().await;
        for (_sid, channels) in st.iter_mut() {
            for (_cname, channel) in channels.iter_mut() {
                if let Some(user) = channel.users.iter_mut().find(|u| u.addr == src) {
                    user.last_pong = Instant::now();
                }
            }
        }
        return;
    }

    //HB messages from clients to inform server they are alive
    if parts.len() >= 4 && parts[0] == "HB" {
        let server_id = parts[1];
        let channel_name = parts[2];
        let user_name = parts[3];
        let mut st = state.lock().await;
        if let Some(channels) = st.get_mut(server_id) {
            if let Some(channel) = channels.get_mut(channel_name) {
                if let Some(u) = channel
                    .users
                    .iter_mut()
                    .find(|u| u.name == user_name && u.addr == src)
                {
                    u.last_pong = Instant::now();
                }
            }
        }
        return;
    }

    if parts.len() >= 1 {
        match parts[0] {
            "CONNECT" if parts.len() >= 4 => {
                let server_id = parts[1].to_string();
                let channel_name = parts[2].to_string();
                let user_name = parts[3].to_string();

                //Use src as address (UDP observer addr)
                let src_addr = src;

                //Add or update user under lock, but collect notifications to send after releasing the lock
                let (users_to_notify, is_new_user) = {
                    let mut st = state.lock().await;
                    let channels = st.entry(server_id.clone()).or_default();
                    let channel = channels.entry(channel_name.clone()).or_default();

                    //check if user already present with same addr
                    if let Some(existing) = channel
                        .users
                        .iter_mut()
                        .find(|u| u.name == user_name && u.addr == src_addr)
                    {
                        existing.last_pong = Instant::now();
                        (channel.clone(), false)
                    } else {
                        //if name exists but with different addr, remove old one
                        channel
                            .users
                            .retain(|u| !(u.name == user_name && u.addr != src_addr));

                        channel.users.push(User {
                            name: user_name.clone(),
                            addr: src_addr,
                            last_pong: Instant::now(),
                        });

                        if channel.relay.is_none() && channel.users.len() == 1 {
                            //server maintains connection via ping
                            let reply = "MODE SERVER_RELAY\n";
                            if let Err(e) = socket
                                .send_to(reply.as_bytes(), channel.users[0].addr)
                                .await
                            {
                                eprintln!("Failed to notify lone user about server relay: {}", e);
                            }

                            //mark channel.relay to None (server is special relay)
                            channel.relay = Some(channel.users[0].name.clone());
                        }

                        println!(
                            "User {} joined {}-{} from {}",
                            user_name, server_id, channel_name, src_addr
                        );
                        (channel.clone(), true)
                    }
                }; //lock dropped

                if !is_new_user {
                    //nothing to notify
                    return;
                }

                // if more than 1 user and relay wasn't set previously, promote first user
                if users_to_notify.users.len() > 1 {
                    let relay_user = &users_to_notify.users[0];

                    //Mark relay in the channel
                    {
                        let mut st = state.lock().await;
                        if let Some(channels) = st.get_mut(&server_id) {
                            if let Some(channel) = channels.get_mut(&channel_name) {
                                channel.relay = Some(relay_user.name.clone());
                            }
                        }
                    }

                    for peer in users_to_notify.users.iter().skip(1) {
                        let msg = format!("MODE DIRECT {} {}\n", peer.name, peer.addr);
                        if let Err(e) = socket.send_to(msg.as_bytes(), relay_user.addr).await {
                            eprintln!(
                                "Failed to notify relay {} about {}: {}",
                                relay_user.name, peer.name, e
                            );
                        }
                    }

                    for peer in users_to_notify.users.iter().skip(1) {
                        let msg = format!("MODE DIRECT {} {}\n", relay_user.name, relay_user.addr);
                        if let Err(e) = socket.send_to(msg.as_bytes(), peer.addr).await {
                            eprintln!(
                                "Failed to notify {} about new relay{}: {}",
                                peer.name, relay_user.name, e
                            );
                        }
                    }

                    //Notify the relay itself that it should act as relay
                    let reply = "MODE RELAY\n";
                    if let Err(e) = socket.send_to(reply.as_bytes(), relay_user.addr).await {
                        eprintln!("Failed to send RELAY mode to {}: {}", relay_user.name, e);
                    }
                } else {
                    //Notify existing users about the new one, and the new one about existing users
                    for user in users_to_notify.users.iter() {
                        if user.addr != src_addr {
                            let msg_to_existing =
                                format!("MODE DIRECT {} {}\n", user_name, src_addr);
                            if let Err(e) =
                                socket.send_to(msg_to_existing.as_bytes(), user.addr).await
                            {
                                eprintln!("Failed to notify {}: {}", user.name, e);
                            }

                            let msg_to_new = format!("MODE DIRECT {} {}\n", user.name, user.addr);
                            if let Err(e) = socket.send_to(msg_to_new.as_bytes(), src_addr).await {
                                eprintln!("Failed to notify new user: {}", e);
                            }
                        }
                    }
                }
            }

            "DISCONNECT" if parts.len() >= 4 => {
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

                    if let Some(channels) = st.get_mut(&server_id) {
                        if let Some(channel) = channels.get_mut(&channel_name) {
                            //find index of user with matching name and addr
                            if let Some(pos) = channel
                                .users
                                .iter()
                                .position(|u| u.name == user_name && u.addr == src_addr)
                            {
                                //check if leaving user was relay
                                was_relay = channel
                                    .relay
                                    .as_ref()
                                    .map(|r| r == &user_name)
                                    .unwrap_or(false);
                                leaving_user_addr = Some(channel.users[pos].addr);
                                channel.users.remove(pos);

                                //update relay if needed
                                if was_relay {
                                    if !channel.users.is_empty() {
                                        let new_relay = channel.users[0].name.clone();
                                        channel.relay = Some(new_relay);
                                    } else {
                                        channel.relay = None;
                                    }
                                }

                                //If only one user left, make sure they are marked relay
                                if channel.users.len() == 1 {
                                    channel.relay = Some(channel.users[0].name.clone());
                                    lone_user_addr = Some(channel.users[0].addr);
                                }

                                println!("User {} left {}-{}", user_name, server_id, channel_name);
                            } else {
                                //unknown session, so ignore
                                println!(
                                    "Ignoring DISCONNECT for {} from {} (no matching session)",
                                    user_name, src_addr
                                );
                            }
                        }
                    }

                    let remaining_users = st
                        .get(&server_id)
                        .and_then(|channels| channels.get(&channel_name))
                        .map(|channel| channel.clone())
                        .unwrap_or_default();

                    (
                        remaining_users.users,
                        was_relay,
                        leaving_user_addr,
                        lone_user_addr,
                    )
                }; //lock dropped

                //Notify lone user they are now relay
                if let Some(lone_user_addr) = lone_user_addr {
                    let reply = "MODE RELAY\n";
                    if let Err(e) = socket.send_to(reply.as_bytes(), lone_user_addr).await {
                        eprintln!("Failed to notify lone user: {}", e);
                    }
                }

                //If the leaving user was relay and there are still users, promote the first and notify
                if was_relay && remaining_users.len() > 1 {
                    let new_relay = &remaining_users[0];
                    if let Err(e) = socket
                        .send_to("MODE RELAY\n".as_bytes(), new_relay.addr)
                        .await
                    {
                        eprintln!(
                            "Failed to notify {} about promotion to relay: {}",
                            new_relay.name, e
                        );
                    }

                    for user in remaining_users.iter().skip(1) {
                        let msg = format!("MODE DIRECT {} {}\n", new_relay.name, new_relay.addr);
                        if let Err(e) = socket.send_to(msg.as_bytes(), user.addr).await {
                            eprintln!("Failed to notify {} about new relay: {}", user.name, e);
                        }
                    }

                    for user in remaining_users.iter().skip(1) {
                        let msg = format!("MODE DIRECT {} {}\n", user.name, user.addr);
                        if let Err(e) = socket.send_to(msg.as_bytes(), new_relay.addr).await {
                            eprintln!(
                                "Failed to notify {} about connected users: {}",
                                new_relay.name, e
                            );
                        }
                    }
                }

                //Notify remaining users about the departure
                for user in remaining_users {
                    let departure_msg = format!(
                        "USER_LEFT {} {}\n",
                        user_name,
                        leaving_user_addr
                            .map(|a| a.to_string())
                            .unwrap_or_else(|| "0.0.0.0:0".into())
                    );
                    if let Err(e) = socket.send_to(departure_msg.as_bytes(), user.addr).await {
                        eprintln!("Failed to notify {} about departure: {}", user.name, e);
                    }
                }
            }

            "PEER_TIMEOUT" if parts.len() >= 4 => {
                let server_id = parts[1].to_string();
                let channel_name = parts[2].to_string();
                let peer_user = parts[3].to_string();

                let remaining = {
                    let mut st = state.lock().await;
                    if let Some(channels) = st.get_mut(&server_id) {
                        if let Some(channel) = channels.get_mut(&channel_name) {
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

                if let Some(channel) = remaining {
                    for u in channel.users {
                        let _ = socket
                            .send_to(
                                format!("USER_LEFT {} 0.0.0.0:0\n", peer_user).as_bytes(),
                                u.addr,
                            )
                            .await;
                    }
                }
            }

            _ => {
                println!("Unknown/Bad packet from {}: {}", src, msg.trim());
            }
        }
    }
}
