use crate::signaling::structures::ServerMap;
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tokio::sync::Mutex;

pub async fn handle_pong(src: SocketAddr, state: Arc<Mutex<ServerMap>>) {
    let mut st = state.lock().await;
    for (_sid, channels) in st.iter_mut() {
        for (_cname, channel) in channels.iter_mut() {
            if let Some(user) = channel.users.iter_mut().find(|u| u.addr == src) {
                user.last_pong = Instant::now();
            }
        }
    }
}

pub async fn handle_heartbeat(parts: &[&str], src: SocketAddr, state: Arc<Mutex<ServerMap>>) {
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
}
