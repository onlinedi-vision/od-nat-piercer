use crate::control::client::{ControlWorker, start_control_client};

pub(crate) fn should_start_control_client(control_client_started: bool, peer_id: u32) -> bool {
    !control_client_started && peer_id != 0
}

pub fn start_control_client_after_welcome(
    control_client_started: &mut bool,
    signaling_ip: &str,
    server_id: &str,
    channel: &str,
    user: &str,
    peer_id: u32,
) -> Option<ControlWorker> {
    if !should_start_control_client(*control_client_started, peer_id) {
        return None;
    }

    let worker = start_control_client(
        signaling_ip,
        server_id.to_string(),
        channel.to_string(),
        user.to_string(),
        peer_id,
    );

    *control_client_started = true;

    println!("Started control WebSocket client with peer_id={peer_id}");
    Some(worker)
}
