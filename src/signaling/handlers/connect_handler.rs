use crate::signaling::structures::ServerMap;
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

use super::{
    notifications_handler::handle_connect_notifications,
    utils_handler::{add_new_user, update_existing_user},
};

pub async fn handle_connect_message(
    parts: &[&str],
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let server_id = parts[1].to_string();
    let channel_name = parts[2].to_string();
    let user_name = parts[3].to_string();
    let src_addr = src;

    let (users_to_notify, is_new_user) = {
        let mut st = state.lock().await;
        let channels = st.entry(server_id.clone()).or_default();
        let channel = channels.entry(channel_name.clone()).or_default();

        if let Some(result) = update_existing_user(channel, &user_name, src_addr).await {
            result
        } else {
            let update_channel = add_new_user(channel, &user_name, src_addr, &socket).await;
            (update_channel, true)
        }
    };

    if !is_new_user {
        return;
    }

    handle_connect_notifications(
        &server_id,
        &channel_name,
        &user_name,
        src_addr,
        users_to_notify,
        socket,
        state,
    )
    .await;
}
