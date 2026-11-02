#![allow(clippy::disallowed_methods)] // Tokio test runtime and explicit test assertions.

use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{
    accept_async_with_config, connect_async_with_config, tungstenite::Message,
};

use super::{
    client::{ClientEvent, ClientState, receive_event, run_control_session, send_control_message},
    integration_tests::{PAYLOAD, Socket, registration, run_case},
    messages::{BroadcastReport, ControlMessage, RequestRejection},
    reliability::{
        ACK_TIMEOUT, Clock, ControlResult, REGISTRATION_TIMEOUT, ReliabilityError, close_socket,
        websocket_config,
    },
    server::channel_key,
};

async fn socket_pair() -> ControlResult<(
    Socket,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
)> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let connect = connect_async_with_config(
        format!("ws://{}", listener.local_addr()?),
        Some(websocket_config()),
        false,
    );
    let accept = async {
        let (stream, _) = listener.accept().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
            accept_async_with_config(stream, Some(websocket_config())).await?,
        )
    };
    let (client, server) = tokio::join!(connect, accept);
    Ok((client?.0, server?))
}

#[tokio::test]
async fn registration_is_required_and_semantic_rejection_consumes_id() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            assert!(peer.send(PAYLOAD).await.is_err());
            peer.raw(&ControlMessage::Broadcast {
                request_id: 1,
                payload: PAYLOAD.into(),
            })
            .await?;
            assert_eq!(
                peer.message().await?,
                ControlMessage::RequestRejected {
                    request_id: 1,
                    reason: RequestRejection::NotRegistered,
                }
            );
            let mut join = registration("server", "channel", 7);
            if let ControlMessage::JoinControl { request_id, .. } = &mut join {
                *request_id = 2;
            }
            peer.raw(&join).await?;
            assert_eq!(
                peer.message().await?,
                ControlMessage::JoinAccepted { request_id: 2 }
            );
            peer.barrier().await?;
            peer.finish().await?;
            h.result(peer.index).await
        })
    })
    .await
}

#[tokio::test]
async fn identical_join_is_idempotent_and_conflict_is_rejected() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            peer.join("server", "channel").await?;
            let same = registration("server", "channel", 7);
            assert_eq!(
                peer.state.join(&mut peer.write, &same, &peer.clock).await?,
                2
            );
            assert_eq!(peer.event().await?, ClientEvent::Joined { request_id: 2 });
            peer.barrier().await?;
            let original = h.registry.lock().await[&channel_key("server", "channel")][0].clone();
            assert_eq!(
                h.registry.lock().await[&channel_key("server", "channel")].len(),
                1
            );
            let conflict = registration("server", "other-channel", 7);
            peer.state
                .join(&mut peer.write, &conflict, &peer.clock)
                .await?;
            assert_eq!(
                peer.event().await?,
                ClientEvent::Rejected {
                    request_id: 3,
                    reason: RequestRejection::ConflictingRegistration,
                }
            );
            assert!(h.result(peer.index).await.is_err());
            assert!(original.is_closed());
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn invalid_registration_fields_get_rejection_then_close() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            for field in 0..4 {
                let mut peer = h.connect().await?;
                let mut join = registration("server", "channel", 7);
                if let ControlMessage::JoinControl {
                    server_id,
                    channel,
                    user,
                    peer_id,
                    ..
                } = &mut join
                {
                    match field {
                        0 => server_id.clear(),
                        1 => channel.clear(),
                        2 => user.clear(),
                        _ => *peer_id = 0,
                    }
                }
                peer.raw(&join).await?;
                assert_eq!(
                    peer.message().await?,
                    ControlMessage::RequestRejected {
                        request_id: 1,
                        reason: RequestRejection::InvalidRegistration,
                    }
                );
                assert!(h.result(peer.index).await.is_err());
            }
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn registration_timeout_is_not_extended_by_rejected_broadcast() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            peer.barrier().await?;
            let start = *peer.server_time.borrow();
            peer.server_time
                .send_replace(start + REGISTRATION_TIMEOUT / 2);
            peer.raw(&ControlMessage::Broadcast {
                request_id: 1,
                payload: PAYLOAD.into(),
            })
            .await?;
            assert!(matches!(
                peer.message().await?,
                ControlMessage::RequestRejected {
                    reason: RequestRejection::NotRegistered,
                    ..
                }
            ));
            peer.server_time.send_replace(start + REGISTRATION_TIMEOUT);
            let error = h
                .result(peer.index)
                .await
                .expect_err("registration deadline");
            assert_eq!(
                error.downcast_ref::<ReliabilityError>(),
                Some(&ReliabilityError::RegistrationTimedOut)
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn zero_gap_and_reused_request_ids_close_connection() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            for id in [0, 1, 3] {
                let mut peer = h.connect().await?;
                peer.join("server", "channel").await?;
                peer.raw(&ControlMessage::Broadcast {
                    request_id: id,
                    payload: PAYLOAD.into(),
                })
                .await?;
                let error = h.result(peer.index).await.expect_err("invalid request ID");
                assert!(matches!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(ReliabilityError::InvalidSequence { .. })
                ));
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn server_rejects_binary_and_wrong_direction_messages() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let messages = [
                ControlMessage::JoinAccepted { request_id: 1 },
                ControlMessage::BroadcastResult {
                    request_id: 1,
                    report: BroadcastReport::default(),
                },
                ControlMessage::RequestRejected {
                    request_id: 1,
                    reason: RequestRejection::NotRegistered,
                },
                ControlMessage::ControlMsg {
                    message_id: 1,
                    payload: PAYLOAD.into(),
                },
            ];
            for message in messages {
                let mut peer = h.connect().await?;
                peer.raw(&message).await?;
                assert!(h.result(peer.index).await.is_err());
            }
            let mut peer = h.connect().await?;
            peer.write.send(Message::Binary(vec![1])).await?;
            assert!(h.result(peer.index).await.is_err());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn client_matches_results_by_type_and_ignores_unknown_results() -> ControlResult<()> {
    timeout(std::time::Duration::from_secs(10), async {
        let (client, mut server) = socket_pair().await?;
        let (mut write, mut read) = client.split();
        let mut state = ClientState::default();
        let mut clock = Clock::default();
        state
            .join(&mut write, &registration("server", "channel", 7), &clock)
            .await?;
        let Some(message) = server.next().await else {
            return Err("connection closed before registration request".into());
        };
        assert!(matches!(message?, Message::Text(_)));
        for message in [
            ControlMessage::JoinAccepted { request_id: 99 },
            ControlMessage::RequestRejected {
                request_id: 0,
                reason: RequestRejection::NotRegistered,
            },
        ] {
            send_control_message(&mut server, &message).await?;
            assert_eq!(
                receive_event(
                    &mut write,
                    &mut read,
                    &mut state,
                    &mut clock,
                    &mut |_, _| Ok(())
                )
                .await?,
                ClientEvent::Other
            );
            assert_eq!(state.sender.pending_len(), 1);
            assert!(state.broadcast(&mut write, PAYLOAD, &clock).await.is_err());
        }
        send_control_message(
            &mut server,
            &ControlMessage::BroadcastResult {
                request_id: 1,
                report: BroadcastReport::default(),
            },
        )
        .await?;
        let error = receive_event(
            &mut write,
            &mut read,
            &mut state,
            &mut clock,
            &mut |_, _| Ok(()),
        )
        .await
        .expect_err("wrong result type");
        assert!(matches!(
            error.downcast_ref::<ReliabilityError>(),
            Some(ReliabilityError::Protocol(_))
        ));
        assert_eq!(state.sender.pending_len(), 0);
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn missing_result_times_out_without_retransmission() -> ControlResult<()> {
    timeout(std::time::Duration::from_secs(10), async {
        let (client, mut server) = socket_pair().await?;
        let (mut write, mut read) = client.split();
        let mut state = ClientState::default();
        let (time, ticks) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let mut clock = Clock::Manual(ticks);
        state
            .join(&mut write, &registration("server", "channel", 7), &clock)
            .await?;
        let Some(message) = server.next().await else {
            return Err("connection closed before registration request".into());
        };
        assert!(matches!(message?, Message::Text(_)));
        time.send_replace(clock.now() + ACK_TIMEOUT);
        let error = receive_event(
            &mut write,
            &mut read,
            &mut state,
            &mut clock,
            &mut |_, _| Ok(()),
        )
        .await
        .expect_err("missing result");
        assert_eq!(
            error.downcast_ref::<ReliabilityError>(),
            Some(&ReliabilityError::Unconfirmed { message_id: 1 })
        );
        close_socket(&mut write, Some(&ReliabilityError::HandlerFailed)).await?;
        // No retry frame precedes closure on the actual WebSocket.
        let Some(message) = server.next().await else {
            return Err("connection ended before close frame".into());
        };
        assert!(matches!(message?, Message::Close(_)));
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn handler_failure_sends_no_ack_and_clears_client_state() -> ControlResult<()> {
    timeout(std::time::Duration::from_secs(10), async {
        let (client, mut server) = socket_pair().await?;
        let (mut write, mut read) = client.split();
        let mut state = ClientState::default();
        let mut clock = Clock::default();
        state
            .join(&mut write, &registration("server", "channel", 7), &clock)
            .await?;
        let Some(message) = server.next().await else {
            return Err("connection closed before registration request".into());
        };
        let _ = message?;
        send_control_message(&mut server, &ControlMessage::JoinAccepted { request_id: 1 }).await?;
        receive_event(
            &mut write,
            &mut read,
            &mut state,
            &mut clock,
            &mut |_, _| Ok(()),
        )
        .await?;
        send_control_message(
            &mut server,
            &ControlMessage::ControlMsg {
                message_id: 1,
                payload: PAYLOAD.into(),
            },
        )
        .await?;
        let error = receive_event(
            &mut write,
            &mut read,
            &mut state,
            &mut clock,
            &mut |_, _| Err("local processing failed".into()),
        )
        .await
        .expect_err("handler failure");
        assert_eq!(
            error.downcast_ref::<ReliabilityError>(),
            Some(&ReliabilityError::HandlerFailed)
        );
        assert!(state.sender.ensure_open().is_err());
        close_socket(&mut write, Some(&ReliabilityError::HandlerFailed)).await?;
        let Some(message) = server.next().await else {
            return Err("connection ended before close frame".into());
        };
        assert!(matches!(message?, Message::Close(_)));
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn client_cli_registers_then_processes_and_acks_without_demo_broadcast() -> ControlResult<()>
{
    timeout(std::time::Duration::from_secs(10), async {
        let (client, mut server) = socket_pair().await?;
        let client = run_control_session(client, registration("server", "channel", 7));
        let server = async {
            let Some(message) = server.next().await else {
                return Err("connection closed before registration request".into());
            };
            let Message::Text(text) = message? else {
                return Err("expected registration JSON text".into());
            };
            assert_eq!(
                serde_json::from_str::<ControlMessage>(&text)?,
                registration("server", "channel", 7)
            );
            server.send(Message::Ping(vec![1])).await?;
            let Some(message) = server.next().await else {
                return Err("connection closed before Pong".into());
            };
            assert_eq!(message?, Message::Pong(vec![1]));
            send_control_message(&mut server, &ControlMessage::JoinAccepted { request_id: 1 })
                .await?;
            for message_id in 1..=2 {
                send_control_message(
                    &mut server,
                    &ControlMessage::ControlMsg {
                        message_id,
                        payload: PAYLOAD.into(),
                    },
                )
                .await?;
                let Some(message) = server.next().await else {
                    return Err("connection closed before notification ACK".into());
                };
                let Message::Text(text) = message? else {
                    return Err("expected notification ACK JSON text".into());
                };
                assert_eq!(
                    serde_json::from_str::<ControlMessage>(&text)?,
                    ControlMessage::ControlAck { message_id }
                );
            }
            server.close(None).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        };
        tokio::try_join!(client, server)?;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn client_rejects_invalid_input_and_reused_notifications_before_processing()
-> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let frames = [
                Message::Binary(vec![1]),
                Message::Text("{".into()),
                Message::Text(serde_json::to_string(&registration(
                    "server", "channel", 7,
                ))?),
                Message::Text(serde_json::to_string(&ControlMessage::Broadcast {
                    request_id: 1,
                    payload: PAYLOAD.into(),
                })?),
                Message::Text(serde_json::to_string(&ControlMessage::ControlAck {
                    message_id: 1,
                })?),
                Message::Text(serde_json::to_string(&ControlMessage::ControlMsg {
                    message_id: 0,
                    payload: PAYLOAD.into(),
                })?),
                Message::Text(serde_json::to_string(&ControlMessage::ControlMsg {
                    message_id: 2,
                    payload: PAYLOAD.into(),
                })?),
            ];
            for frame in frames {
                let mut peer = h.connect().await?;
                peer.join("server", "channel").await?;
                let handle = h.registry.lock().await[&channel_key("server", "channel")][0].clone();
                handle.send(frame)?;
                assert!(peer.event().await.is_err());
                assert_eq!(peer.processed.len(), 0);
                close_socket(&mut peer.write, Some(&ReliabilityError::Closed)).await?;
                h.result(peer.index).await?;
            }
            let mut peer = h.connect().await?;
            peer.join("server", "channel").await?;
            let handle = h.registry.lock().await[&channel_key("server", "channel")][0].clone();
            let message = Message::Text(serde_json::to_string(&ControlMessage::ControlMsg {
                message_id: 1,
                payload: PAYLOAD.into(),
            })?);
            handle.send(message.clone())?;
            assert_eq!(peer.event().await?, ClientEvent::Received { message_id: 1 });
            handle.send(message)?;
            assert!(peer.event().await.is_err());
            assert_eq!(peer.processed.len(), 1);
            close_socket(&mut peer.write, Some(&ReliabilityError::Closed)).await?;
            h.result(peer.index).await
        })
    })
    .await
}

#[tokio::test]
async fn ready_traffic_cannot_hide_an_expired_request_deadline() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            peer.join("server", "channel").await?;
            peer.send(PAYLOAD).await?;
            // Read an actual result from the socket, then leave it ready for the
            // production transition while advancing the confirmation deadline.
            let frame = peer.read.next().await.expect("broadcast result")?;
            let mut ready = futures_util::stream::iter([Ok(frame)]).chain(&mut peer.read);
            peer.time.send_replace(peer.clock.now() + ACK_TIMEOUT);
            let error = receive_event(
                &mut peer.write,
                &mut ready,
                &mut peer.state,
                &mut peer.clock,
                &mut |_, _| Ok(()),
            )
            .await
            .expect_err("expired request takes priority");
            assert_eq!(
                error.downcast_ref::<ReliabilityError>(),
                Some(&ReliabilityError::Unconfirmed { message_id: 2 })
            );
            close_socket(&mut peer.write, Some(&ReliabilityError::Closed)).await?;
            h.result(peer.index).await
        })
    })
    .await
}

#[tokio::test]
async fn websocket_enforces_real_frame_and_message_limits() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            peer.join("server", "channel").await?;
            peer.write
                .send(Message::Text(
                    "a".repeat(super::reliability::MAX_MESSAGE_BYTES + 1),
                ))
                .await?;
            let error = h.result(peer.index).await.expect_err("oversized frame");
            assert!(matches!(
                error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
                Some(tokio_tungstenite::tungstenite::Error::Capacity(_))
            ));
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn cancelling_client_receive_closes_state_and_releases_pending() -> ControlResult<()> {
    timeout(std::time::Duration::from_secs(10), async {
        let (client, mut server) = socket_pair().await?;
        let (mut write, mut read) = client.split();
        let mut state = ClientState::default();
        let mut clock = Clock::default();
        state
            .join(&mut write, &registration("server", "channel", 7), &clock)
            .await?;
        let _ = server.next().await.expect("registration request")?;
        assert!(
            timeout(
                std::time::Duration::ZERO,
                receive_event(
                    &mut write,
                    &mut read,
                    &mut state,
                    &mut clock,
                    &mut |_, _| Ok(()),
                )
            )
            .await
            .is_err()
        );
        assert_eq!(state.sender.pending_len(), 0);
        assert!(state.sender.ensure_open().is_err());
        close_socket(&mut write, Some(&ReliabilityError::Closed)).await?;
        assert!(matches!(
            server.next().await.expect("close frame")?,
            Message::Close(_)
        ));
        Ok(())
    })
    .await?
}
