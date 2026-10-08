use std::{collections::HashMap, net::Ipv4Addr, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpListener, sync::Mutex, task::JoinSet, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::{
    client::send_control_message,
    messages::ControlMessage,
    server::{ControlPeers, channel_key, handle_connection},
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
const TEST_DEADLINE: Duration = Duration::from_secs(5);
const MESSAGE_ID: u64 = 42;

#[derive(Clone, Copy)]
enum ConnectionEnd {
    Close,
    MalformedJson,
}

// A single accepted socket uses the same handler as start_control_server. Binding
// before connecting provides readiness without sleeps or the application's port.
async fn control_exchange(connections: &mut JoinSet<TestResult>, end: ConnectionEnd) -> TestResult {
    let peers: ControlPeers = Arc::new(Mutex::new(HashMap::new()));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let server_peers = Arc::clone(&peers);
    connections.spawn(async move {
        let (stream, _) = listener.accept().await?;
        drop(listener);
        handle_connection(stream, server_peers).await
    });

    let (mut client, _) = connect_async(format!("ws://{addr}")).await?;
    send_control_message(
        &mut client,
        &ControlMessage::JoinControl {
            server_id: "integration-server".into(),
            channel: "integration-channel".into(),
            user: "integration-peer".into(),
            peer_id: 7,
        },
    )
    .await?;
    send_control_message(
        &mut client,
        &ControlMessage::ControlMsg {
            message_id: MESSAGE_ID,
            // Opaque deterministic control data, with JSON escaping exercised.
            payload: "\0\u{1}\u{7f}\"\\".into(),
        },
    )
    .await?;

    let frame = client.next().await.expect("server must return an ACK")?;
    let Message::Text(text) = frame else {
        panic!("expected a JSON ACK, got {frame:?}");
    };
    let ack: ControlMessage = serde_json::from_str(&text)?;
    assert!(
        matches!(ack, ControlMessage::ControlAck { message_id } if message_id == MESSAGE_ID),
        "expected ACK for message {MESSAGE_ID}, got {ack:?}"
    );

    // The ACK orders this observation after the production handler's join.
    let key = channel_key("integration-server", "integration-channel");
    {
        let registry = peers.lock().await;
        let senders = registry.get(&key).expect("join must register the socket");
        assert_eq!(senders.len(), 1);
        assert!(!senders[0].is_closed());
    }

    match end {
        ConnectionEnd::Close => {
            client.close(None).await?;
            let reply = client.next().await.transpose()?;
            assert!(
                matches!(reply, Some(Message::Close(_)) | None),
                "expected close response or EOF, got {reply:?}"
            );
        }
        ConnectionEnd::MalformedJson => {
            client.send(Message::Text("{".into())).await?;
        }
    }

    let server_result = connections
        .join_next()
        .await
        .expect("connection task must exist")?;
    match end {
        ConnectionEnd::Close => server_result?,
        ConnectionEnd::MalformedJson => {
            let error = server_result.expect_err("malformed JSON must end the connection");
            assert!(error.downcast_ref::<serde_json::Error>().is_some());
        }
    }
    let registry = peers.lock().await;
    assert!(
        registry.values().all(Vec::is_empty),
        "connection teardown must remove its registered sender"
    );
    Ok(())
}

async fn run_control_exchange(end: ConnectionEnd) -> TestResult {
    // JoinSet aborts on drop if an assertion panics; shutdown also joins any
    // unfinished connection after a timeout/error. All sockets are task-local.
    let mut connections = JoinSet::new();
    let result = timeout(TEST_DEADLINE, control_exchange(&mut connections, end)).await;
    connections.shutdown().await;
    result?
}

#[allow(clippy::disallowed_methods)] // tokio::test uses expect to create its runtime.
#[tokio::test]
async fn real_websocket_control_ack_and_close_cleanup() -> TestResult {
    run_control_exchange(ConnectionEnd::Close).await
}

#[allow(clippy::disallowed_methods)] // tokio::test uses expect to create its runtime.
#[tokio::test]
async fn real_websocket_malformed_json_cleans_up_connection() -> TestResult {
    run_control_exchange(ConnectionEnd::MalformedJson).await
}
