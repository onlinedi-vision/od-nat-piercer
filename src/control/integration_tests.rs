#![allow(clippy::disallowed_methods)] // Tokio test runtime and explicit test assertions.

//! Shared bounded localhost fixture using the production connection and client.
use std::{
    collections::HashMap, future::Future, net::Ipv4Addr, pin::Pin, sync::Arc, time::Duration,
};

use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, watch},
    task::{AbortHandle, JoinSet},
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config, tungstenite::Message,
};

use super::{
    client::{ClientEvent, ClientState, receive_event, send_control_message},
    messages::ControlMessage,
    reliability::{Clock, ControlResult, websocket_config},
    server::{ControlPeers, handle_connection_with_clock},
};

pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub(super) type Case<'a> = Pin<Box<dyn Future<Output = ControlResult<()>> + 'a>>;
pub(super) const PAYLOAD: &str = "\0\u{1}\u{7f}\"\\";

pub(super) fn registration(server: &str, channel: &str, peer_id: u32) -> ControlMessage {
    ControlMessage::JoinControl {
        request_id: 1,
        server_id: server.into(),
        channel: channel.into(),
        user: format!("peer-{peer_id}"),
        peer_id,
    }
}

pub(super) struct Peer {
    pub(super) index: usize,
    pub(super) write: SplitSink<Socket, Message>,
    pub(super) read: SplitStream<Socket>,
    pub(super) state: ClientState,
    pub(super) clock: Clock,
    pub(super) time: watch::Sender<Instant>,
    pub(super) server_time: watch::Sender<Instant>,
    pub(super) processed: Vec<(u64, String)>,
}

impl Peer {
    pub(super) async fn raw(&mut self, message: &ControlMessage) -> ControlResult<()> {
        send_control_message(&mut self.write, message).await
    }

    pub(super) async fn event(&mut self) -> ControlResult<ClientEvent> {
        let processed = &mut self.processed;
        receive_event(
            &mut self.write,
            &mut self.read,
            &mut self.state,
            &mut self.clock,
            &mut |id, payload| {
                processed.push((id, payload.into()));
                Ok(())
            },
        )
        .await
    }

    pub(super) async fn message(&mut self) -> ControlResult<ControlMessage> {
        let frame = self.read.next().await.expect("server frame")?;
        let Message::Text(text) = frame else {
            panic!("expected JSON, got {frame:?}");
        };
        Ok(serde_json::from_str(&text)?)
    }

    pub(super) async fn join(&mut self, server: &str, channel: &str) -> ControlResult<()> {
        let id = self
            .state
            .join(
                &mut self.write,
                &registration(server, channel, 7),
                &self.clock,
            )
            .await?;
        assert_eq!(self.event().await?, ClientEvent::Joined { request_id: id });
        self.barrier().await
    }

    pub(super) async fn send(&mut self, payload: &str) -> ControlResult<u64> {
        self.state
            .broadcast(&mut self.write, payload, &self.clock)
            .await
    }

    // Ordered Ping/Pong proves all preceding frames were handled; also detects
    // unexpected notifications/results without an arbitrary negative timeout.
    pub(super) async fn barrier(&mut self) -> ControlResult<()> {
        self.write.send(Message::Ping(vec![42])).await?;
        assert_eq!(
            self.read.next().await.expect("barrier pong")?,
            Message::Pong(vec![42])
        );
        Ok(())
    }

    pub(super) async fn finish(&mut self) -> ControlResult<()> {
        self.write.close().await?;
        while let Some(frame) = self.read.next().await {
            assert!(matches!(frame?, Message::Close(_) | Message::Pong(_)));
        }
        Ok(())
    }
}

pub(super) struct Harness {
    listener: Arc<TcpListener>,
    pub(super) registry: ControlPeers,
    tasks: JoinSet<(usize, ControlResult<()>)>,
    pub(super) aborts: Vec<AbortHandle>,
    completed: HashMap<usize, ControlResult<()>>,
}

impl Harness {
    async fn new() -> ControlResult<Self> {
        Ok(Self {
            listener: Arc::new(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?),
            registry: Arc::new(Mutex::new(HashMap::new())),
            tasks: JoinSet::new(),
            aborts: Vec::new(),
            completed: HashMap::new(),
        })
    }

    pub(super) async fn connect(&mut self) -> ControlResult<Peer> {
        let index = self.aborts.len();
        let listener = Arc::clone(&self.listener);
        let peers = Arc::clone(&self.registry);
        let (server_time, ticks) = watch::channel(Instant::now());
        self.aborts.push(self.tasks.spawn(async move {
            let result = async {
                let (stream, _) = listener.accept().await?;
                handle_connection_with_clock(stream, peers, Clock::Manual(ticks)).await
            }
            .await;
            (index, result)
        }));
        let (socket, _) = connect_async_with_config(
            format!("ws://{}", self.listener.local_addr()?),
            Some(websocket_config()),
            false,
        )
        .await?;
        let (write, read) = socket.split();
        let (time, ticks) = watch::channel(Instant::now());
        Ok(Peer {
            index,
            write,
            read,
            state: ClientState::default(),
            clock: Clock::Manual(ticks),
            time,
            server_time,
            processed: Vec::new(),
        })
    }

    pub(super) async fn result(&mut self, index: usize) -> ControlResult<()> {
        loop {
            if let Some(result) = self.completed.remove(&index) {
                return result;
            }
            let (ended, result) = self.tasks.join_next().await.expect("connection task")?;
            self.completed.insert(ended, result);
        }
    }
}

pub(super) async fn run_case(
    case: impl for<'a> FnOnce(&'a mut Harness) -> Case<'a>,
) -> ControlResult<()> {
    let mut harness = Harness::new().await?;
    let result = timeout(Duration::from_secs(10), case(&mut harness)).await;
    harness.tasks.shutdown().await;
    result?
}

#[tokio::test]
async fn real_websocket_registration_broadcast_result_and_cleanup() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            peer.join("server", "channel").await?;
            assert_eq!(peer.send(PAYLOAD).await?, 2);
            assert_eq!(
                peer.event().await?,
                ClientEvent::Broadcast {
                    request_id: 2,
                    report: super::messages::BroadcastReport::default(),
                }
            );
            peer.barrier().await?;
            peer.finish().await?;
            h.result(peer.index).await?;
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn malformed_json_cleans_up_registration() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut peer = h.connect().await?;
            peer.join("server", "channel").await?;
            peer.write.send(Message::Text("{".into())).await?;
            let error = h.result(peer.index).await.expect_err("invalid JSON");
            assert!(error.downcast_ref::<serde_json::Error>().is_some());
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}
