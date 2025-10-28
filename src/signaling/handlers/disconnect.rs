use crate::signaling::structures::ServerMap;
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

use super::{notifications::handle_disconnect_notifications, utils::handle_user_removal};

pub async fn handle_disconnect_message(
    parts: &[&str],
    src: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let server_id = parts[1].to_string();
    let channel_name = parts[2].to_string();
    let user_name = parts[3].to_string();
    let src_addr = src;

    //We'll only remove the user if the src matches the stored addr for that username
    let (remaining_users, was_relay, leaving_user_addr, lone_user_addr) =
        handle_user_removal(&state, &server_id, &channel_name, &user_name, src_addr).await;

    handle_disconnect_notifications(
        remaining_users,
        was_relay,
        leaving_user_addr,
        lone_user_addr,
        &user_name,
        socket,
        &Arc::clone(&state),
        server_id,
        channel_name,
    )
    .await;
}
