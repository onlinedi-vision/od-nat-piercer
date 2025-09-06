use crate::signaling::{structures::ServerMap, utils::to_cleanup_and_notify_iter};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::UdpSocket, sync::Mutex};

pub fn start_heartbeat(socket: Arc<UdpSocket>, state: Arc<Mutex<ServerMap>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        loop {
            interval.tick().await;

            let (to_ping, to_cleanup, notify_msgs): (
                Vec<SocketAddr>,
                Vec<(String, String)>,
                Vec<(Vec<SocketAddr>, Vec<u8>)>,
            ) = {
                let mut st = state.lock().await;
                let mut pings = Vec::new();
                let mut cleanup = Vec::new();
                let mut notifications: Vec<(Vec<SocketAddr>, Vec<u8>)> = Vec::new();

                let server_ids: Vec<String> = st.keys().cloned().collect();
                for sid in server_ids.iter() {
                    if let Some(channels) = st.get_mut(sid) {
                        let channel_names: Vec<String> = channels.keys().cloned().collect();
                        for cname in channel_names.iter() {
                            if let Some(channel) = channels.get_mut(cname) {
                                let mut i = 0;
                                while i < channel.users.len() {
                                    let user = &channel.users[i];

                                    if user.last_pong.elapsed() > Duration::from_secs(40) {
                                        println!(
                                            "User {} timed out from {}-{}",
                                            user.name, sid, cname
                                        );

                                        let msg =
                                            format!("USER_LEFT {} {}\n", user.name, user.addr);
                                        let peers_to_notify: Vec<SocketAddr> = channel
                                            .users
                                            .iter()
                                            .filter(|u| u.addr != user.addr)
                                            .map(|u| u.addr)
                                            .collect();

                                        if !peers_to_notify.is_empty() {
                                            notifications
                                                .push((peers_to_notify, msg.as_bytes().to_vec()));
                                        }

                                        let was_relay = channel
                                            .relay
                                            .as_ref()
                                            .map(|r| r == &user.name)
                                            .unwrap_or(false);

                                        channel.users.remove(i);

                                        if was_relay {
                                            if !channel.users.is_empty() {
                                                let new_relay = channel.users[0].name.clone();
                                                channel.relay = Some(new_relay.clone());

                                                let new_relay_addr = channel.users[0].addr;

                                                notifications.push((
                                                    vec![new_relay_addr],
                                                    b"MODE RELAY\n".to_vec(),
                                                ));

                                                let mut relinfo_msg = Vec::new();
                                                for peer in channel.users.iter().skip(1) {
                                                    let m = format!(
                                                        "MODE DIRECT {} {}\n",
                                                        new_relay, new_relay_addr
                                                    );
                                                    relinfo_msg.extend_from_slice(m.as_bytes());
                                                }
                                                if !relinfo_msg.is_empty() {
                                                    let others_addrs: Vec<SocketAddr> = channel
                                                        .users
                                                        .iter()
                                                        .skip(1)
                                                        .map(|u| u.addr)
                                                        .collect();
                                                    notifications.push((others_addrs, relinfo_msg));
                                                }

                                                let mut peers_to_new_msg = Vec::new();
                                                for peer in channel.users.iter().skip(1) {
                                                    let m = format!(
                                                        "MODE DIRECT {} {}\n",
                                                        peer.name, peer.addr
                                                    );
                                                    peers_to_new_msg
                                                        .extend_from_slice(m.as_bytes());
                                                }

                                                if !peers_to_new_msg.is_empty() {
                                                    notifications.push((
                                                        vec![new_relay_addr],
                                                        peers_to_new_msg,
                                                    ));
                                                }
                                            } else {
                                                channel.relay = None;
                                            }
                                        }

                                        if channel.users.len() == 1 {
                                            channel.relay = Some(channel.users[0].name.clone());
                                            notifications.push((
                                                vec![channel.users[0].addr],
                                                b"MODE RELAY\n".to_vec(),
                                            ));
                                        }
                                    } else {
                                        i += 1;
                                    }
                                }

                                //if channel has only one user, ping him to keep NAT connection
                                if channel.users.len() == 1 {
                                    pings.push(channel.users[0].addr);
                                }

                                //if channel empty, mark for cleanup
                                if channel.users.is_empty() {
                                    cleanup.push((sid.clone(), cname.clone()));
                                }
                            }
                        }
                    }
                }
                (pings, cleanup, notifications)
            };

            // Send pings without holding the lock
            for addr in to_ping {
                if let Err(e) = socket.send_to(b"PING", addr).await {
                    eprintln!("Failed to send PING: {}", e);
                }
            }

            //send notifications (USER_LEFT, MODE RELAY, MODE DIRECT messages}
            for (peers_to_notify, payload) in to_cleanup_and_notify_iter(notify_msgs.into_iter()) {
                for addr in peers_to_notify {
                    if let Err(e) = socket.send_to(&payload, addr).await {
                        eprintln!("Failed to send heartbeat notification to {}: {}", addr, e);
                    }
                }
            }

            //Cleanup empty channels
            if !to_cleanup.is_empty() {
                let mut st = state.lock().await;
                for (sid, cname) in to_cleanup.into_iter() {
                    if let Some(chans) = st.get_mut(&sid) {
                        chans.remove(&cname);
                        if chans.is_empty() {
                            st.remove(&sid);
                        }
                    }
                }
            }
        }
    });
}
