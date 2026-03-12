use crate::{
    proto::control_text::{MSG_WELCOME, NAT_TYPE_CONE, NAT_TYPE_PUBLIC, NAT_TYPE_SYMMETRIC},
    signaling::{
        handlers::utils::make_channel_id,
        structures::{NatKind, ServerMap},
    },
};

use crate::proto::packet::{self, Header, Kind};
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::Mutex};

use super::{
    notifications::handle_connect_notifications,
    utils::{add_new_user, remove_user_from_other_channels, update_existing_user},
};

async fn send_welcome(socket: &Arc<UdpSocket>, dst: SocketAddr, channel_id: u32, peer_id: u32) {
    let payload = format!("{MSG_WELCOME} to cid:{channel_id} with pid:{peer_id}\n");
    let hdr = Header::welcome(channel_id, peer_id, payload.len() as u16);

    let pkt = packet::encode(hdr, payload.as_bytes());
    let _ = socket.send_to(&pkt, dst).await;
}

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

    let nat_kind = if parts.len() >= 5 {
        match parts[4] {
            NAT_TYPE_SYMMETRIC => NatKind::Symmetric,
            NAT_TYPE_CONE => NatKind::Cone,
            NAT_TYPE_PUBLIC => NatKind::Public,
            _ => NatKind::Unknown,
        }
    } else {
        NatKind::Unknown
    };

    let (users_to_notify, is_new_user, peer_id, channel_id) = {
        let mut st = state.lock().await;

        remove_user_from_other_channels(&mut st, &server_id, &channel_name, &user_name, src_addr)
            .await;

        let channels = st.entry(server_id.clone()).or_default();
        let channel = channels.entry(channel_name.clone()).or_default();

        if channel.channel_id == 0 {
            channel.channel_id = make_channel_id(&server_id, &channel_name);
        }

        let channel_id = channel.channel_id;

        if let Some((updated_channel, is_new, peer_id)) =
            update_existing_user(channel, &user_name, src_addr).await
        {
            (updated_channel, is_new, peer_id, channel_id)
        } else {
            let (update_channel, peer_id) =
                add_new_user(channel, &user_name, src_addr, &socket, nat_kind).await;
            (update_channel, true, peer_id, channel_id)
        }
    };

    send_welcome(&socket, src_addr, channel_id, peer_id).await;

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
