use std::thread;

use futures_util::{Sink, SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::control::messages::ControlMessage;

const TEST_PAYLOAD_LEN: usize = 100 * 1024;

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
                eprintln!("Failed to create control client runtime: {e}");
                return;
            }
        };

        rt.block_on(async move {
            if let Err(e) =
                run_control_client(signaling_ip, server_id, channel, user, peer_id).await
            {
                eprintln!("Control client error: {e}");
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
    let ws_url = format!("ws://{signaling_ip}:2133");

    let (ws_stream, _) = connect_async(&ws_url).await?;
    println!("Connected to control WebSocket server at {ws_url}");

    let (mut write, mut read) = ws_stream.split();

    let join = ControlMessage::JoinControl {
        server_id,
        channel,
        user,
        peer_id,
    };

    send_control_message(&mut write, &join).await?;
    println!("Sent JoinControl on reliable control plane.");

    let test_payload = "A".repeat(TEST_PAYLOAD_LEN);
    let payload_len = test_payload.len();

    let control_msg = ControlMessage::ControlMsg {
        message_id: 1,
        payload: test_payload,
    };

    send_control_message(&mut write, &control_msg).await?;
    println!("Sent ControlMsg message_id=1 payload_len={payload_len}");

    while let Some(msg) = read.next().await {
        let msg = msg?;

        if let Message::Text(text) = msg {
            let parsed: ControlMessage = serde_json::from_str(&text)?;

            match parsed {
                ControlMessage::ControlAck { message_id } => {
                    println!("Received ControlAck for message_id={message_id}");
                }
                other => {
                    println!("Received other control message: {other:?}");
                }
            }
        }
    }

    Ok(())
}

pub(super) async fn send_control_message<S>(
    write: &mut S,
    message: &ControlMessage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let text = serde_json::to_string(message)?;
    write.send(Message::Text(text)).await?;
    Ok(())
}
