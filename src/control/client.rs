use std::thread;

use futures_util::{Sink, Stream, StreamExt};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Error as WsError, Message},
};

use crate::control::messages::ControlMessage;

use super::reliability::{
    Clock, ControlResult, ReliabilityError, ReliableSender, SenderGuard, encode, websocket_config,
    write_text,
};

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
) -> ControlResult<()> {
    let ws_url = format!("ws://{signaling_ip}:2133");

    let (ws_stream, _) =
        connect_async_with_config(&ws_url, Some(websocket_config()), false).await?;
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

    let mut sender = ReliableSender::default();
    let mut clock = Clock::default();
    let message_id = sender.send(&mut write, &test_payload, &clock).await?;
    println!("Sent ControlMsg message_id={message_id} payload_len={payload_len}");

    loop {
        match receive_or_retry(&mut write, &mut read, &mut sender, &mut clock).await? {
            ClientEvent::Acknowledged {
                message_id,
                cleared: true,
            } => {
                println!("Received ControlAck for pending message_id={message_id}");
            }
            ClientEvent::Closed => break,
            ClientEvent::Acknowledged { cleared: false, .. }
            | ClientEvent::Retried { .. }
            | ClientEvent::Other => {}
        }
    }

    Ok(())
}

pub(super) async fn send_control_message<S>(
    write: &mut S,
    message: &ControlMessage,
) -> ControlResult<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    write_text(write, encode(message)?).await
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClientEvent {
    Acknowledged { message_id: u64, cleared: bool },
    Retried { count: usize },
    Other,
    Closed,
}

/// One production receive/timer transition. The caller owns the socket and
/// reliability state; cancellation of a transition clears and closes that state.
pub(super) async fn receive_or_retry<S, R>(
    write: &mut S,
    read: &mut R,
    sender: &mut ReliableSender,
    clock: &mut Clock,
) -> ControlResult<ClientEvent>
where
    S: Sink<Message, Error = WsError> + Unpin,
    R: Stream<Item = Result<Message, WsError>> + Unpin,
{
    sender.ensure_open()?;
    let mut guard = SenderGuard::new(sender);
    let deadline = guard.sender.next_deadline();
    let event = tokio::select! {
        frame = read.next() => {
            match frame.transpose()? {
                None | Some(Message::Close(_)) => {
                    let pending = guard.sender.pending_len();
                    guard.sender.close();
                    if pending != 0 {
                        return Err(ReliabilityError::ConnectionClosed { pending }.into());
                    }
                    ClientEvent::Closed
                }
                Some(Message::Text(text)) => {
                    match serde_json::from_str::<ControlMessage>(&text)? {
                        ControlMessage::ControlAck { message_id } => ClientEvent::Acknowledged {
                            message_id,
                            cleared: guard.sender.acknowledge(message_id),
                        },
                        _ => ClientEvent::Other,
                    }
                }
                Some(_) => ClientEvent::Other,
            }
        }
        () = clock.wait_until(deadline) => {
            ClientEvent::Retried { count: guard.sender.retry_due(write, clock).await? }
        }
    };
    guard.complete();
    Ok(event)
}
