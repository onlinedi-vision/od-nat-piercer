#![allow(clippy::disallowed_methods)] // Tokio test runtime and explicit test assertions.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use super::*;

#[derive(Default)]
struct TestSink {
    frames: Vec<Message>,
    stall: bool,
    fail: bool,
}

impl Sink<Message> for TestSink {
    type Error = WsError;
    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        Poll::Ready(if self.fail {
            Err(WsError::ConnectionClosed)
        } else {
            Ok(())
        })
    }
    fn start_send(mut self: Pin<&mut Self>, frame: Message) -> Result<(), WsError> {
        self.frames.push(frame);
        Ok(())
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        if self.fail {
            Poll::Ready(Err(WsError::ConnectionClosed))
        } else if self.stall {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        self.poll_flush(cx)
    }
}

fn notification(id: u64, payload: &str) -> ControlMessage {
    ControlMessage::ControlMsg {
        message_id: id,
        payload: payload.into(),
    }
}

async fn send(
    sender: &mut ReliableSender,
    sink: &mut TestSink,
    payload: &str,
    clock: &Clock,
) -> ControlResult<u64> {
    sender
        .send(
            sink,
            |id| notification(id, payload),
            Confirmation::Ack,
            None,
            clock,
        )
        .await
}

fn assert_error<T>(result: ControlResult<T>, expected: ReliabilityError) {
    match result {
        Err(error) => assert_eq!(error.downcast_ref::<ReliabilityError>(), Some(&expected)),
        Ok(_) => panic!("expected {expected}"),
    }
}

#[tokio::test]
async fn pending_count_allows_progress_while_oldest_stays_pending() -> ControlResult<()> {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let clock = Clock::default();
    for id in 1..=WINDOW as u64 {
        assert_eq!(send(&mut sender, &mut sink, "opaque", &clock).await?, id);
    }
    assert_error(
        send(&mut sender, &mut sink, "full", &clock).await,
        ReliabilityError::WindowFull,
    );
    for id in 2..=WINDOW as u64 {
        assert!(sender.confirm(id, Confirmation::Ack)?);
    }
    for id in WINDOW as u64 + 1..=100 {
        assert_eq!(send(&mut sender, &mut sink, "progress", &clock).await?, id);
        assert!(sender.confirm(id, Confirmation::Ack)?);
    }
    assert_eq!(sender.pending_len(), 1);
    assert!(!sender.confirm(0, Confirmation::Ack)?);
    assert!(!sender.confirm(100, Confirmation::Ack)?);
    assert!(!sender.confirm(500, Confirmation::Ack)?);
    assert!(sender.confirm(1, Confirmation::Ack)?);
    assert_eq!(sender.next_deadline(), None);
    Ok(())
}

#[tokio::test]
async fn deadline_fails_without_retransmission() -> ControlResult<()> {
    let (time, ticks) = tokio::sync::watch::channel(Instant::now());
    let clock = Clock::Manual(ticks);
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    send(&mut sender, &mut sink, "opaque", &clock).await?;
    let start = clock.now();
    time.send_replace(start + ACK_TIMEOUT - Duration::from_nanos(1));
    sender.check_deadlines(&clock)?;
    time.send_replace(start + ACK_TIMEOUT);
    assert_error(
        sender.check_deadlines(&clock),
        ReliabilityError::Unconfirmed { message_id: 1 },
    );
    assert_eq!(sink.frames.len(), 1);
    assert_eq!(sender.pending_len(), 0);
    assert!(sender.closed);
    Ok(())
}

#[tokio::test]
async fn exact_serialized_limits_and_local_errors_preserve_next_id() -> ControlResult<()> {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let clock = Clock::default();
    let overhead = encode(&notification(u64::MAX, ""))?.len();
    let body = MAX_MESSAGE_BYTES - overhead;
    let escaped = format!("{}{}", "\0".repeat(body / 6), "a".repeat(body % 6));
    assert_eq!(
        encode(&notification(u64::MAX, &escaped))?.len(),
        MAX_MESSAGE_BYTES
    );
    assert_error(
        encode(&notification(u64::MAX, &(escaped + "a"))),
        ReliabilityError::MessageTooLarge,
    );
    for payload in [
        "a".repeat(MAX_MESSAGE_BYTES + 1),
        "\0".repeat(MAX_MESSAGE_BYTES / 6),
    ] {
        assert_error(
            send(&mut sender, &mut sink, &payload, &clock).await,
            ReliabilityError::MessageTooLarge,
        );
        assert_eq!(sender.next_id, Some(1));
        assert_eq!(sink.frames.len(), 0);
    }
    assert_eq!(send(&mut sender, &mut sink, "accepted", &clock).await?, 1);
    Ok(())
}

#[test]
fn sequence_rejects_zero_gaps_reuse_and_exhaustion() {
    let mut sequence = Sequence::default();
    for id in [0, 2, 42] {
        assert!(sequence.accept(id).is_err());
    }
    assert_eq!(sequence.accept(1), Ok(()));
    assert!(sequence.accept(1).is_err());
    sequence.highest = u64::MAX - 1;
    assert_eq!(sequence.accept(u64::MAX), Ok(()));
    assert_eq!(sequence.accept(0), Err(ReliabilityError::IdExhausted));
}

#[tokio::test]
async fn sender_never_wraps_or_reuses_exhausted_counter() -> ControlResult<()> {
    let mut sender = ReliableSender {
        next_id: Some(u64::MAX),
        ..ReliableSender::default()
    };
    let mut sink = TestSink::default();
    let clock = Clock::default();
    assert_eq!(
        send(&mut sender, &mut sink, "last", &clock).await?,
        u64::MAX
    );
    assert_error(
        send(&mut sender, &mut sink, "wrap", &clock).await,
        ReliabilityError::IdExhausted,
    );
    assert_eq!(sink.frames.len(), 1);
    Ok(())
}

#[tokio::test]
async fn failed_cancelled_and_closed_writes_release_reservations() -> ControlResult<()> {
    for fail in [true, false] {
        let (peer, mut queue, _) = super::super::recipient::ControlPeer::new();
        assert!(peer.enqueue(&std::sync::Arc::from("opaque")));
        let Some(super::super::recipient::Outbound::Control { reservation, .. }) =
            queue.recv().await
        else {
            return Err("expected a queued CONTROL notification".into());
        };
        let mut sender = ReliableSender::default();
        let mut sink = TestSink {
            fail,
            stall: !fail,
            ..TestSink::default()
        };
        let result = timeout(
            Duration::ZERO,
            sender.send(
                &mut sink,
                |id| notification(id, "opaque"),
                Confirmation::Ack,
                Some(reservation),
                &Clock::default(),
            ),
        )
        .await;
        assert!(matches!(result, Err(_) | Ok(Err(_))));
        assert!(sender.closed);
        assert_eq!(sender.pending_len(), 0);
        // All 32 slots are available again, including the cancelled write's slot.
        for _ in 0..WINDOW {
            assert!(peer.enqueue(&std::sync::Arc::from("next")));
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failed_or_stalled_ping_flush_closes_client_pending_requests() -> ControlResult<()> {
    use super::super::{
        client::{ClientState, receive_event},
        integration_tests::registration,
    };

    for fail in [false, true] {
        let mut state = ClientState::default();
        let mut sink = TestSink::default();
        let mut clock = Clock::default();
        state
            .join(&mut sink, &registration("server", "channel", 7), &clock)
            .await?;
        assert_eq!(state.sender.pending_len(), 1);

        sink.fail = fail;
        sink.stall = !fail;
        let mut read = futures_util::stream::iter([Ok(Message::Ping(vec![1]))]);
        let error = receive_event(&mut sink, &mut read, &mut state, &mut clock, &mut |_, _| {
            Ok(())
        })
        .await
        .expect_err("failed Ping flush must terminate pending delivery");

        if fail {
            assert!(matches!(
                error.downcast_ref::<WsError>(),
                Some(WsError::ConnectionClosed)
            ));
        } else {
            assert_eq!(
                error.downcast_ref::<ReliabilityError>(),
                Some(&ReliabilityError::WriteTimedOut)
            );
        }
        assert_eq!(state.sender.pending_len(), 0);
        assert!(state.sender.ensure_open().is_err());
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stalled_write_and_close_are_bounded() {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink {
        stall: true,
        ..TestSink::default()
    };
    assert_error(
        send(&mut sender, &mut sink, "opaque", &Clock::default()).await,
        ReliabilityError::WriteTimedOut,
    );
    assert!(sender.closed);
    assert_eq!(sender.pending_len(), 0);
    assert_error(
        close_socket(&mut sink, Some(&ReliabilityError::Closed)).await,
        ReliabilityError::WriteTimedOut,
    );
}

#[tokio::test]
async fn confirmation_deadline_starts_after_completed_write() -> ControlResult<()> {
    struct Delayed {
        time: tokio::sync::watch::Sender<Instant>,
        completed: Instant,
    }
    impl Sink<Message> for Delayed {
        type Error = WsError;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), WsError> {
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.time.send_replace(self.completed);
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            Poll::Ready(Ok(()))
        }
    }
    let start = Instant::now();
    let (time, ticks) = tokio::sync::watch::channel(start);
    let clock = Clock::Manual(ticks);
    let completed = start + WRITE_TIMEOUT / 2;
    let mut sink = Delayed { time, completed };
    let mut sender = ReliableSender::default();
    sender
        .send(
            &mut sink,
            |id| notification(id, "opaque"),
            Confirmation::Ack,
            None,
            &clock,
        )
        .await?;
    assert_eq!(sender.next_deadline(), Some(completed + ACK_TIMEOUT));
    Ok(())
}

#[tokio::test]
async fn queue_and_pending_share_capacity_and_release_on_ack_or_termination() -> ControlResult<()> {
    use super::super::recipient::{ControlPeer, Outbound};
    let (peer, mut queue, _) = ControlPeer::new();
    let payload = std::sync::Arc::from("opaque");
    for _ in 0..WINDOW {
        assert!(peer.enqueue(&payload));
    }
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let clock = Clock::default();
    for _ in 0..WINDOW / 2 {
        let Some(Outbound::Control { reservation, .. }) = queue.recv().await else {
            return Err("expected a queued CONTROL notification".into());
        };
        sender
            .send(
                &mut sink,
                |id| notification(id, "opaque"),
                Confirmation::Ack,
                Some(reservation),
                &clock,
            )
            .await?;
    }
    assert_eq!(queue.len() + sender.pending_len(), WINDOW);
    assert!(sender.confirm(1, Confirmation::Ack)?);
    assert!(peer.enqueue(&payload)); // ACK frees exactly one combined slot
    assert_eq!(queue.len() + sender.pending_len(), WINDOW);
    assert!(!peer.enqueue(&payload)); // moving to pending did not create capacity
    assert!(peer.is_closed());
    sender.close();
    drop(queue);
    assert_eq!(sender.pending_len(), 0);
    Ok(())
}
