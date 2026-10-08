use std::{collections::HashMap, sync::Arc};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use futures_util::{SinkExt, StreamExt};

use crate::control::messages::ControlMessage;

pub type ControlPeers =
    Arc<Mutex<HashMap<String, Vec<tokio::sync::mpsc::UnboundedSender<Message>>>>>;

pub(crate) fn channel_key(server_id: &str, channel: &str) -> String {
    format!("{server_id}:{channel}")
}

/// Starts the control WebSocket server.
///
/// # Errors
///
/// Returns an error if the TCP listener cannot bind to `addr` or if accepting
/// an incoming TCP connection fails.
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

pub(super) async fn handle_connection(
    stream: TcpStream,
    peers: ControlPeers,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_stream = accept_async(stream).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Owned tasks are aborted if the connection future is cancelled.
    let mut write_tasks = JoinSet::new();
    write_tasks.spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut joined_channel: Option<String> = None;

    // Capture read/parse errors so they take the same teardown path as EOF.
    let read_result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        while let Some(msg) = ws_read.next().await {
            let msg = msg?;

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
                        println!(
                            "Received ControlMsg message_id={} payload_len={}",
                            message_id,
                            payload.len(),
                        );
                        let ack = ControlMessage::ControlAck { message_id };
                        let ack_text = serde_json::to_string(&ack)?;
                        let _ = tx.send(Message::Text(ack_text));
                    }

                    ControlMessage::ControlAck { .. } => {
                        //for part1 we don't have anything for server
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    // Await cancellation before is_closed(): abort alone does not guarantee the
    // writer has dropped its receiver when the registry is pruned.
    write_tasks.shutdown().await;

    if let Some(key) = joined_channel {
        let mut guard = peers.lock().await;
        if let Some(list) = guard.get_mut(&key) {
            list.retain(|sender| !sender.is_closed());
        }
    }

    read_result
}
