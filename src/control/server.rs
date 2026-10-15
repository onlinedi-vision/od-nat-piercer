use std::{collections::HashMap, sync::Arc};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::{accept_async_with_config, tungstenite::Message};

use futures_util::{SinkExt, StreamExt};

use crate::control::messages::ControlMessage;

use super::reliability::{
    ControlResult, MAX_MESSAGE_BYTES, Receipt, ReliabilityError, ReliableReceiver, WRITE_TIMEOUT,
    encode, websocket_config, write_text,
};

pub type ControlPeers =
    Arc<Mutex<HashMap<String, Vec<tokio::sync::mpsc::UnboundedSender<Message>>>>>;

pub(crate) fn channel_key(server_id: &str, channel: &str) -> String {
    format!("{server_id}:{channel}")
}

/// Starts the control WebSocket server.
///
/// # Errors
///
/// Returns an error if the server cannot listen on `addr` or accept a connection.
pub async fn start_control_server(addr: &str, peers: ControlPeers) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    println!("Control WebSocket server listening on {addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        let peers = Arc::clone(&peers);

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peers).await {
                eprintln!("control ws connection error: {e}");
            }
        });
    }
}

pub(super) async fn handle_connection(stream: TcpStream, peers: ControlPeers) -> ControlResult<()> {
    handle_connection_observed(
        stream,
        peers,
        |message_id, payload| {
            println!(
                "Received ControlMsg message_id={message_id} payload_len={}",
                payload.len()
            );
        },
        Some,
    )
    .await
}

// Handles a connection using two functions supplied by the caller.
// `process` is called once for each new `ControlMsg`.
// `acknowledge` returns an ID to ACK, or None to skip that ACK.
// Tests count processed messages and skip selected ACKs to trigger retries.
// They use real WebSocket connections and the same reliability code.
pub(super) async fn handle_connection_observed<F, A>(
    stream: TcpStream,
    peers: ControlPeers,
    mut process: F,
    mut acknowledge: A,
) -> ControlResult<()>
where
    F: FnMut(u64, &str) + Send,
    A: FnMut(u64) -> Option<u64> + Send,
{
    let ws_stream = accept_async_with_config(stream, Some(websocket_config())).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    let mut joined_channel: Option<String> = None;
    let mut receiver = ReliableReceiver::default();

    // Save the result of reading and handling messages so cleanup below also
    // runs if reading, JSON parsing, or message handling fails.
    let read_result: ControlResult<()> = async {
        loop {
            let msg = tokio::select! {
                frame = ws_read.next() => {
                    let Some(frame) = frame else { break; };
                    frame?
                }
                queued = rx.recv() => {
                    if let Some(message) = queued {
                        // This queue accepts messages from the senders stored in `peers`.
                        // ACKs are sent directly below, and each write must finish
                        // before the next incoming message is handled.
                        if message.len() > MAX_MESSAGE_BYTES {
                            return Err(ReliabilityError::MessageTooLarge.into());
                        }
                        timeout(WRITE_TIMEOUT, ws_write.send(message)).await
                            .map_err(|_| ReliabilityError::WriteTimedOut)??;
                    }
                    continue;
                }
            };

            if matches!(msg, Message::Close(_)) { break; }

            if let Message::Text(text) = msg {
                let parsed: ControlMessage = serde_json::from_str(&text)?;

                match parsed {
                    ControlMessage::JoinControl {
                        server_id,
                        channel,
                        user,
                        peer_id,
                    } => {
                        let key = channel_key(&server_id, &channel);

                        println!(
                            "Control client joined: server_id={server_id} channel={channel}, user={user}, peer_id={peer_id}"
                        );

                        joined_channel = Some(key.clone());

                        let mut guard = peers.lock().await;
                        guard.entry(key).or_default().push(tx.clone());
                    }

                    ControlMessage::ControlMsg {
                        message_id,
                        payload,
                    } => {
                        if receiver.classify(message_id, &payload)? == Receipt::New {
                            process(message_id, &payload);
                            receiver.commit(message_id, &payload);
                        }
                        if let Some(ack_id) = acknowledge(message_id) {
                            // The normal server ACKs the ID it received.
                            let ack = ControlMessage::ControlAck {
                                message_id: ack_id,
                            };
                            write_text(&mut ws_write, encode(&ack)?).await?;
                        }
                    }

                    ControlMessage::ControlAck { .. } => {
                        // The server does not track outgoing reliable CONTROL
                        // messages yet, so it has no pending messages to clear.
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    // This connection handler owns the socket and performs all writes.
    // Close the outgoing queue before removing its senders from `peers`.
    // If this handler is cancelled, its socket, queue receiver, and duplicate
    // history are dropped automatically.
    rx.close();
    drop(rx);
    drop(receiver);
    // Give the WebSocket up to WRITE_TIMEOUT to close.
    // Return the earlier message-handling result even if closing fails.
    let _ = timeout(WRITE_TIMEOUT, ws_write.close()).await;

    if let Some(key) = joined_channel {
        let mut guard = peers.lock().await;
        if let Some(list) = guard.get_mut(&key) {
            list.retain(|sender| !sender.is_closed());
        }
    }

    read_result
}
