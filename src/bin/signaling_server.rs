use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use tokio::{net::UdpSocket, sync::Mutex};

#[derive(Clone, Debug)]
struct User {
    name: String,
    addr: SocketAddr,
}

type Channel = Vec<User>;
type ServerMap = HashMap<String, HashMap<String, Channel>>; //server_id -> channel_name -> Vec<User>

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind("0.0.0.0:5000").await?;
    println!("Signaling server listening on 0.0.0.0:5000");

    let state = Arc::new(Mutex::new(ServerMap::new()));
    let mut buf = [0u8; 1024];

    loop {
        let (len, src) = socket.recv_from(&mut buf).await?;
        let msg = String::from_utf8_lossy(&buf[..len]).to_string();
        let parts: Vec<&str> = msg.trim().split_whitespace().collect();

        if parts.len() >= 4 && parts[0] == "CONNECT" {
            let server_id = parts[1].to_string();
            let channel_name = parts[2].to_string();
            let user_name = parts[3].to_string();

            let mut st = state.lock().await;
            let channels = st.entry(server_id.clone()).or_default();
            let channel = channels.entry(channel_name.clone()).or_default();

            //Add the new user
            if !channel.iter().any(|u| u.name == user_name && u.addr == src) {
                channel.push(User {
                    name: user_name.clone(),
                    addr: src,
                });
                println!(
                    "User {} joined {}-{} from {}",
                    user_name, server_id, channel_name, src
                );
            } else {
                println!(
                    "User {} at {} already in {}-{}, skipping add",
                    user_name, src, server_id, channel_name
                );
            }

            //Decide the mode relaying on the number of users in the channel
            if channel.len() == 1 {
                //First user -> relay mode
                let reply = format!("MODE RELAY\n");
                socket.send_to(reply.as_bytes(), src).await?;
            } else {
                //Another user exists in the channel -> direct mode
                for u in channel.iter() {
                    //Send every user the address of the others
                    let msg_to_new = format!("MODE DIRECT {} {}", u.name, u.addr);
                    socket.send_to(msg_to_new.as_bytes(), src).await?;

                    let msg_to_existing = format!("MODE DIRECT {} {}", user_name, src);
                    socket.send_to(msg_to_existing.as_bytes(), u.addr).await?;
                }
            }
        } else if parts.len() >= 4 && parts[0] == "DISCONNECT" {
            let server_id = parts[1].to_string();
            let channel_name = parts[2].to_string();
            let user_name = parts[3].to_string();

            let mut st = state.lock().await;

            if let Some(channels) = st.get_mut(&server_id) {
                if let Some(channel) = channels.get_mut(&channel_name) {
                    channel.retain(|u| u.name != user_name);
                    println!("User {} left {}-{}", user_name, server_id, channel_name);

                    //If there's only one user left, we mark him as relay
                    if channel.len() == 1 {
                        let lone_user = &channel[0];
                        let reply = format!("MODE RELAY\n");
                        socket.send_to(reply.as_bytes(), lone_user.addr).await?;
                    }
                }
            }
        }
    }
}
