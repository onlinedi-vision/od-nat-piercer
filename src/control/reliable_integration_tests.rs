use std::{
    collections::HashMap,
    future::Future,
    net::Ipv4Addr,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, watch},
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Error as WsError, Message},
};

use super::{
    client::{ClientEvent, receive_or_retry, send_control_message},
    messages::ControlMessage,
    reliability::{
        ACK_TIMEOUT, Clock, ControlResult, MAX_MESSAGE_BYTES, ReliabilityError, ReliableSender,
        WINDOW, WRITE_TIMEOUT, encode, websocket_config,
    },
    server::{ControlPeers, channel_key, handle_connection_observed},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type CaseFuture<'a> = Pin<Box<dyn Future<Output = ControlResult<()>> + 'a>>;
const TEST_DEADLINE: Duration = Duration::from_secs(5);
const PAYLOAD: &str = "\0\u{1}\u{7f}\"\\";

#[derive(Clone, Copy, Default)]
struct Observation {
    received: usize,
    applied: usize,
}

struct ControlTestConnection {
    write: SplitSink<Socket, Message>,
    read: SplitStream<Socket>,
    sender: ReliableSender,
    clock: Clock,
    time: watch::Sender<Instant>,
    observations: watch::Receiver<Observation>,
    connections: JoinSet<ControlResult<()>>,
    peers: ControlPeers,
    handler_lifetime: Weak<AtomicUsize>,
}

impl ControlTestConnection {
    async fn start<A>(mut ack: A) -> ControlResult<Self>
    where
        A: FnMut(u64, usize) -> Option<u64> + Send + 'static,
    {
        let peers: ControlPeers = Arc::new(Mutex::new(HashMap::new()));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let server_peers = Arc::clone(&peers);
        let applied = Arc::new(AtomicUsize::new(0));
        let applied_for_ack = Arc::clone(&applied);
        let (observe, observations) = watch::channel(Observation::default());
        let handler_lifetime = Arc::downgrade(&applied);
        let mut received = 0;
        let mut connections = JoinSet::new();
        connections.spawn(async move {
            let (stream, _) = listener.accept().await?;
            drop(listener);
            handle_connection_observed(
                stream,
                server_peers,
                move |_, _| {
                    applied.fetch_add(1, Ordering::Relaxed);
                },
                move |id| {
                    received += 1;
                    observe.send_replace(Observation {
                        received,
                        applied: applied_for_ack.load(Ordering::Relaxed),
                    });
                    ack(id, received)
                },
            )
            .await
        });
        let (socket, _) = timeout(
            TEST_DEADLINE,
            connect_async_with_config(format!("ws://{addr}"), Some(websocket_config()), false),
        )
        .await??;
        let (mut write, read) = socket.split();
        send_control_message(
            &mut write,
            &ControlMessage::JoinControl {
                server_id: "reliable-server".into(),
                channel: "reliable-channel".into(),
                user: "reliable-peer".into(),
                peer_id: 7,
            },
        )
        .await?;
        let (time, ticks) = watch::channel(Instant::now());
        Ok(Self {
            write,
            read,
            sender: ReliableSender::default(),
            clock: Clock::Manual(ticks),
            time,
            observations,
            connections,
            peers,
            handler_lifetime,
        })
    }

    async fn send(&mut self, payload: &str) -> ControlResult<u64> {
        self.sender
            .send(&mut self.write, payload, &self.clock)
            .await
    }

    async fn event(&mut self) -> ControlResult<ClientEvent> {
        receive_or_retry(
            &mut self.write,
            &mut self.read,
            &mut self.sender,
            &mut self.clock,
        )
        .await
    }

    async fn wait_received(&mut self, count: usize) -> ControlResult<Observation> {
        while self.observations.borrow().received < count {
            self.observations.changed().await?;
        }
        Ok(*self.observations.borrow())
    }

    fn expire_ack_deadline(&self) {
        self.time.send_replace(self.clock.now() + ACK_TIMEOUT);
    }

    async fn inject_ack(&self, id: u64) -> ControlResult<()> {
        let registry = self.peers.lock().await;
        let senders = registry
            .get(&channel_key("reliable-server", "reliable-channel"))
            .expect("JoinControl must register this connection");
        senders[0].send(Message::Text(encode(&ControlMessage::ControlAck {
            message_id: id,
        })?))?;
        Ok(())
    }

    async fn assert_registry_empty(&self) {
        let registry = self.peers.lock().await;
        assert!(
            registry.values().all(Vec::is_empty),
            "normal/error teardown must prune the sender"
        );
        assert!(
            self.handler_lifetime.upgrade().is_none(),
            "connection-owned callbacks must be released"
        );
    }

    async fn finish(&mut self) -> ControlResult<()> {
        self.write.close().await?;
        let result = self
            .connections
            .join_next()
            .await
            .expect("connection task must exist")?;
        result?;
        self.assert_registry_empty().await;
        Ok(())
    }

    async fn server_error(&mut self) -> ControlResult<Box<dyn std::error::Error + Send + Sync>> {
        let result = self
            .connections
            .join_next()
            .await
            .expect("connection task must exist")?;
        let Err(error) = result else {
            panic!("expected connection failure")
        };
        self.assert_registry_empty().await;
        Ok(error)
    }
}

// Every case owns bounded observations, sockets, and its connection task. Always
// join cancellation after a failed assertion result or deadline. JoinSet also
// aborts on drop during a panic. No application server process is launched.
async fn run_case<A, F>(ack: A, case: F) -> ControlResult<()>
where
    A: FnMut(u64, usize) -> Option<u64> + Send + 'static,
    F: for<'a> FnOnce(&'a mut ControlTestConnection) -> CaseFuture<'a>,
{
    let mut fixture = ControlTestConnection::start(ack).await?;
    let result = timeout(TEST_DEADLINE, case(&mut fixture)).await;
    fixture.sender.close();
    let _ = timeout(WRITE_TIMEOUT, fixture.write.close()).await;
    fixture.connections.shutdown().await;
    result??;
    Ok(())
}

#[allow(clippy::disallowed_methods)] // tokio::test creates its runtime with expect.
#[tokio::test]
async fn reliable_send_receives_real_ack_and_clears_pending() -> ControlResult<()> {
    run_case(
        |id, _| Some(id),
        |fixture| {
            Box::pin(async move {
                assert_eq!(fixture.send(PAYLOAD).await?, 1);
                assert_eq!(fixture.sender.pending_len(), 1);
                assert_eq!(
                    fixture.event().await?,
                    ClientEvent::Acknowledged {
                        message_id: 1,
                        cleared: true
                    }
                );
                assert_eq!(fixture.sender.pending_len(), 0);
                assert_eq!(fixture.sender.next_deadline(), None);
                assert_eq!(fixture.wait_received(1).await?.applied, 1);
                fixture.finish().await
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn real_unknown_duplicate_and_stale_acks_only_clear_the_matching_send() -> ControlResult<()> {
    run_case(
        |_, _| None,
        |fixture| {
            Box::pin(async move {
                fixture.send("first").await?;
                fixture.send("second").await?;
                fixture.wait_received(2).await?;
                for (id, cleared, remaining) in [
                    (999, false, 2),
                    (0, false, 2),
                    (2, true, 1),
                    (2, false, 1),
                    (1, true, 0),
                    (1, false, 0),
                ] {
                    fixture.inject_ack(id).await?;
                    assert_eq!(
                        fixture.event().await?,
                        ClientEvent::Acknowledged {
                            message_id: id,
                            cleared
                        }
                    );
                    assert_eq!(fixture.sender.pending_len(), remaining);
                }
                fixture.finish().await
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn missing_ack_retries_over_websocket_without_reapplying_and_is_reacked() -> ControlResult<()>
{
    run_case(
        |id, receipt| if receipt == 1 { None } else { Some(id) },
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                assert_eq!(fixture.wait_received(1).await?.applied, 1);
                fixture.expire_ack_deadline();
                assert_eq!(fixture.event().await?, ClientEvent::Retried { count: 1 });
                assert_eq!(fixture.wait_received(2).await?.applied, 1);
                assert_eq!(
                    fixture.event().await?,
                    ClientEvent::Acknowledged {
                        message_id: 1,
                        cleared: true
                    }
                );
                assert_eq!(fixture.sender.pending_len(), 0);
                fixture.finish().await
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn delayed_ack_before_deadline_does_not_trigger_a_retry() -> ControlResult<()> {
    run_case(
        |_, _| None,
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.wait_received(1).await?;
                fixture
                    .time
                    .send_replace(fixture.clock.now() + ACK_TIMEOUT - Duration::from_millis(1));
                fixture.inject_ack(1).await?;
                assert_eq!(
                    fixture.event().await?,
                    ClientEvent::Acknowledged {
                        message_id: 1,
                        cleared: true
                    }
                );
                assert_eq!(fixture.observations.borrow().received, 1);
                fixture.finish().await
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn retry_exhaustion_is_explicit_and_clears_connection_state() -> ControlResult<()> {
    run_case(
        |_, _| None,
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.wait_received(1).await?;
                for receipt in 2..=4 {
                    fixture.expire_ack_deadline();
                    assert_eq!(fixture.event().await?, ClientEvent::Retried { count: 1 });
                    assert_eq!(fixture.wait_received(receipt).await?.applied, 1);
                }
                fixture.expire_ack_deadline();
                let error = match fixture.event().await {
                    Err(error) => error,
                    Ok(event) => panic!("expected exhaustion, got {event:?}"),
                };
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::RetryExhausted {
                        message_id: 1,
                        attempts: 4
                    })
                );
                assert_eq!(fixture.sender.pending_len(), 0);
                assert_eq!(fixture.sender.next_deadline(), None);
                let Err(error) = fixture.send("after failure").await else {
                    panic!("closed sender accepted a message")
                };
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::Closed)
                );
                fixture.finish().await
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn conflicting_duplicate_is_rejected_before_a_second_application() -> ControlResult<()> {
    run_case(
        |id, _| Some(id),
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.event().await?;
                send_control_message(
                    &mut fixture.write,
                    &ControlMessage::ControlMsg {
                        message_id: 1,
                        payload: "changed".into(),
                    },
                )
                .await?;
                let error = fixture.server_error().await?;
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::ConflictingDuplicate { message_id: 1 })
                );
                assert_eq!(fixture.observations.borrow().applied, 1);
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn sequence_gap_is_rejected_before_processing() -> ControlResult<()> {
    run_case(
        |id, _| Some(id),
        |fixture| {
            Box::pin(async move {
                send_control_message(
                    &mut fixture.write,
                    &ControlMessage::ControlMsg {
                        message_id: 2,
                        payload: PAYLOAD.into(),
                    },
                )
                .await?;
                let error = fixture.server_error().await?;
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::InvalidSequence {
                        message_id: 2,
                        expected: Some(1)
                    })
                );
                assert_eq!(fixture.observations.borrow().applied, 0);
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn connection_continues_after_window_rollover_and_rejects_retired_ids() -> ControlResult<()> {
    run_case(
        |id, _| Some(id),
        |fixture| {
            Box::pin(async move {
                for id in 1..=WINDOW as u64 + 1 {
                    assert_eq!(fixture.send(PAYLOAD).await?, id);
                    assert_eq!(
                        fixture.event().await?,
                        ClientEvent::Acknowledged {
                            message_id: id,
                            cleared: true
                        }
                    );
                }
                assert_eq!(fixture.observations.borrow().applied, WINDOW + 1);
                send_control_message(
                    &mut fixture.write,
                    &ControlMessage::ControlMsg {
                        message_id: 1,
                        payload: PAYLOAD.into(),
                    },
                )
                .await?;
                let error = fixture.server_error().await?;
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::RetiredMessage { message_id: 1 })
                );
                assert_eq!(fixture.observations.borrow().applied, WINDOW + 1);
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn close_with_pending_messages_reports_unconfirmed_and_cleans_up() -> ControlResult<()> {
    run_case(
        |_, _| None,
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.wait_received(1).await?;
                fixture.write.close().await?;
                let error = match fixture.event().await {
                    Err(error) => error,
                    Ok(event) => panic!("expected unconfirmed close, got {event:?}"),
                };
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::ConnectionClosed { pending: 1 })
                );
                assert_eq!(fixture.sender.pending_len(), 0);
                assert_eq!(fixture.sender.next_deadline(), None);
                fixture
                    .connections
                    .join_next()
                    .await
                    .expect("connection task must exist")??;
                fixture.assert_registry_empty().await;
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn cancelled_receive_clears_pending_and_server_shutdown_releases_handler() -> ControlResult<()>
{
    run_case(
        |_, _| None,
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.wait_received(1).await?;
                assert!(timeout(Duration::ZERO, fixture.event()).await.is_err());
                assert_eq!(fixture.sender.pending_len(), 0);
                assert_eq!(fixture.sender.next_deadline(), None);
                fixture.connections.shutdown().await;
                assert!(fixture.handler_lifetime.upgrade().is_none());
                let registry = fixture.peers.lock().await;
                // Cancellation drops reliability state and the queue receiver. Async
                // registry pruning remains the existing handler's normal/error cleanup.
                assert!(
                    registry
                        .values()
                        .flatten()
                        .all(tokio::sync::mpsc::UnboundedSender::is_closed)
                );
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn oversized_network_message_is_rejected_before_processing() -> ControlResult<()> {
    run_case(
        |id, _| Some(id),
        |fixture| {
            Box::pin(async move {
                let text = serde_json::to_string(&ControlMessage::ControlMsg {
                    message_id: 1,
                    payload: "A".repeat(MAX_MESSAGE_BYTES),
                })?;
                assert!(text.len() > MAX_MESSAGE_BYTES);
                fixture.write.send(Message::Text(text)).await?;
                let error = fixture.server_error().await?;
                assert!(matches!(
                    error.downcast_ref::<WsError>(),
                    Some(WsError::Capacity(_))
                ));
                assert_eq!(fixture.observations.borrow().applied, 0);
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn graceful_close_after_ack_ends_the_production_receive_loop() -> ControlResult<()> {
    run_case(
        |id, _| Some(id),
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.event().await?;
                fixture.write.close().await?;
                assert_eq!(fixture.event().await?, ClientEvent::Closed);
                let Err(error) = fixture.event().await else {
                    panic!("closed connection must not be reused")
                };
                assert_eq!(
                    error.downcast_ref::<ReliabilityError>(),
                    Some(&ReliabilityError::Closed)
                );
                fixture
                    .connections
                    .join_next()
                    .await
                    .expect("connection task must exist")??;
                fixture.assert_registry_empty().await;
                Ok(())
            })
        },
    )
    .await
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn malformed_ack_clears_pending_before_returning_the_parse_error() -> ControlResult<()> {
    run_case(
        |_, _| None,
        |fixture| {
            Box::pin(async move {
                fixture.send(PAYLOAD).await?;
                fixture.wait_received(1).await?;
                {
                    let registry = fixture.peers.lock().await;
                    let senders = registry
                        .get(&channel_key("reliable-server", "reliable-channel"))
                        .expect("JoinControl must register this connection");
                    senders[0].send(Message::Text("{".into()))?;
                }
                let Err(error) = fixture.event().await else {
                    panic!("malformed ACK must fail")
                };
                assert!(error.downcast_ref::<serde_json::Error>().is_some());
                assert_eq!(fixture.sender.pending_len(), 0);
                assert_eq!(fixture.sender.next_deadline(), None);
                fixture.finish().await
            })
        },
    )
    .await
}
