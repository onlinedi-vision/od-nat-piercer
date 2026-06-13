use crate::control::client::start_control_client;

pub fn start_control_client_after_welcome(
    control_client_started: &mut bool,
    signaling_ip: &str,
    server_id: &str,
    channel: &str,
    user: &str,
    peer_id: u32,
) {
    if *control_client_started {
        return;
    }

    if peer_id == 0 {
        return;
    }

    start_control_client(
        signaling_ip.to_string(),
        server_id.to_string(),
        channel.to_string(),
        user.to_string(),
        peer_id,
    );

    *control_client_started = true;

    println!("Started control WebSocket client with peer_id={}", peer_id);
}
