use std::thread;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::control::messages::ControlMessage;

pub fn start_control_client(
    signaling_ip: String,
    server_id: String,
    channel: String,
    user: String,
    peer_id: u32,
) {
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("Failed to create control client runtime: {}", e);
                return;
            }
        };

        rt.block_on(async move {
            if let Err(e) =
                run_control_client(signaling_ip, server_id, channel, user, peer_id).await
            {
                eprintln!("Control client error: {}", e);
            }
        });
    });
}

async fn run_control_client(
    signaling_ip: String,
    server_id: String,
    channel: String,
    user: String,
    peer_id: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_url = format!("ws://{}:2133", signaling_ip);

    let (ws_stream, _) = connect_async(&ws_url).await?;
    println!("Connected to control WebSocket server at {}", ws_url);

    let (mut write, mut read) = ws_stream.split();

    let join = ControlMessage::JoinControl {
        server_id,
        channel,
        user,
        peer_id,
    };

    let join_text = serde_json::to_string(&join)?;
    write.send(Message::Text(join_text)).await?;
    println!("Sent JoinControl on reliable control plane.");

    let test_payload = "control-plane-test".to_string();

    let control_msg = ControlMessage::ControlMsg {
        message_id: 1,
        payload: test_payload,
    };

    let msg_text = serde_json::to_string(&control_msg)?;
    write.send(Message::Text(msg_text)).await?;
    println!("Sent ControlMsg message_id=1");

    while let Some(msg) = read.next().await {
        let msg = msg?;

        if let Message::Text(text) = msg {
            let parsed: ControlMessage = serde_json::from_str(&text)?;

            match parsed {
                ControlMessage::ControlAck { message_id } => {
                    println!("Received ControlAck for message_id={}", message_id);
                }
                other => {
                    println!("Received other control message: {:?}", other);
                }
            }
        }
    }

    Ok(())
}
