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
        if self.stall {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        self.poll_flush(cx)
    }
}

fn manual_clock() -> (Clock, tokio::sync::watch::Sender<Instant>) {
    let (time, ticks) = tokio::sync::watch::channel(Instant::now());
    (Clock::Manual(ticks), time)
}

fn assert_error<T>(result: ControlResult<T>, expected: ReliabilityError) {
    match result {
        Err(error) => assert_eq!(error.downcast_ref::<ReliabilityError>(), Some(&expected)),
        Ok(_) => panic!("expected {expected}"),
    }
}

#[allow(clippy::disallowed_methods)] // tokio::test creates its runtime with expect.
#[tokio::test]
async fn id_window_tracks_oldest_pending_not_just_pending_count() -> ControlResult<()> {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let clock = Clock::default();
    for id in 1..=WINDOW as u64 {
        assert_eq!(sender.send(&mut sink, "opaque", &clock).await?, id);
    }
    assert_error(
        sender.send(&mut sink, "blocked", &clock).await,
        ReliabilityError::WindowFull,
    );
    for id in 2..=WINDOW as u64 {
        assert!(sender.acknowledge(id));
    }
    assert_eq!(sender.pending_len(), 1);
    assert_error(
        sender.send(&mut sink, "still blocked", &clock).await,
        ReliabilityError::WindowFull,
    );
    assert!(sender.acknowledge(1));
    assert_eq!(
        sender.send(&mut sink, "next", &clock).await?,
        WINDOW as u64 + 1
    );
    assert_eq!(sender.pending_len(), 1);
    Ok(())
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn ack_clears_only_matching_message_once_and_releases_bytes() -> ControlResult<()> {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let clock = Clock::default();
    sender.send(&mut sink, "first", &clock).await?;
    sender.send(&mut sink, "second", &clock).await?;
    let before = sender.pending_bytes;
    assert!(!sender.acknowledge(0));
    assert!(!sender.acknowledge(500));
    assert_eq!(sender.pending_bytes, before);
    assert!(sender.acknowledge(2));
    assert_eq!(sender.pending_len(), 1);
    let after = sender.pending_bytes;
    assert!(!sender.acknowledge(2));
    assert_eq!(sender.pending_bytes, after);
    assert!(sender.acknowledge(1));
    assert_eq!(sender.pending_bytes, 0);
    assert_eq!(sender.next_deadline(), None);
    sender.close();
    assert!(!sender.acknowledge(1));
    assert_error(
        sender.send(&mut sink, "closed", &clock).await,
        ReliabilityError::Closed,
    );
    Ok(())
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn retry_deadlines_preserve_content_and_fail_after_four_attempts() -> ControlResult<()> {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let (clock, time) = manual_clock();
    let start = clock.now();
    sender.send(&mut sink, "\0\u{1}\"\\", &clock).await?;
    time.send_replace(start + ACK_TIMEOUT - Duration::from_millis(1));
    assert_eq!(sender.retry_due(&mut sink, &clock).await?, 0);
    for attempt in 1..MAX_ATTEMPTS {
        time.send_replace(start + ACK_TIMEOUT * u32::from(attempt));
        assert_eq!(sender.retry_due(&mut sink, &clock).await?, 1);
    }
    assert_eq!(sink.frames.len(), usize::from(MAX_ATTEMPTS));
    assert!(sink.frames.iter().all(|frame| frame == &sink.frames[0]));
    time.send_replace(start + ACK_TIMEOUT * u32::from(MAX_ATTEMPTS));
    assert_error(
        sender.retry_due(&mut sink, &clock).await,
        ReliabilityError::RetryExhausted {
            message_id: 1,
            attempts: MAX_ATTEMPTS,
        },
    );
    assert!(sender.closed);
    assert_eq!(sender.pending_len(), 0);
    assert_eq!(sender.pending_bytes, 0);
    assert_error(
        sender.retry_due(&mut sink, &clock).await,
        ReliabilityError::Closed,
    );
    Ok(())
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn serialized_size_and_byte_budget_are_enforced_without_consuming_ids() -> ControlResult<()> {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink::default();
    let clock = Clock::default();
    let escaped = "\0".repeat(MAX_MESSAGE_BYTES / 6);
    assert_error(
        sender.send(&mut sink, &escaped, &clock).await,
        ReliabilityError::MessageTooLarge,
    );
    assert_eq!(sender.next_id, Some(1));
    let overhead = encode(&ControlMessage::ControlMsg {
        message_id: 1,
        payload: String::new(),
    })?
    .len();
    let boundary = "A".repeat(MAX_MESSAGE_BYTES - overhead);
    for id in 1..=4 {
        assert_eq!(sender.send(&mut sink, &boundary, &clock).await?, id);
    }
    assert_eq!(sender.pending_bytes, MAX_PENDING_BYTES);
    assert_error(
        sender.send(&mut sink, "small", &clock).await,
        ReliabilityError::PendingBytesFull,
    );
    assert_eq!(sender.next_id, Some(5));
    assert!(sender.acknowledge(1));
    assert_eq!(sender.send(&mut sink, "small", &clock).await?, 5);
    sender.close();
    assert_eq!(sender.pending_bytes, 0);
    Ok(())
}

#[test]
fn receiver_rolls_history_without_forgetting_the_processed_sequence() -> ControlResult<()> {
    let mut receiver = ReliableReceiver::default();
    // Two windows replace every original history entry while IDs keep increasing.
    let last_id = (2 * WINDOW) as u64;
    for id in 1..=last_id {
        assert_eq!(receiver.classify(id, "opaque")?, Receipt::New);
        receiver.commit(id, "opaque");
        assert!(receiver.recent.len() <= WINDOW);
    }
    assert_eq!(receiver.highest, last_id);
    assert_eq!(receiver.recent.len(), WINDOW);
    assert_eq!(receiver.classify(last_id, "opaque")?, Receipt::Duplicate);
    assert_eq!(
        receiver.classify(last_id, "altered"),
        Err(ReliabilityError::ConflictingDuplicate {
            message_id: last_id
        })
    );
    assert_eq!(
        receiver.classify(1, "opaque"),
        Err(ReliabilityError::RetiredMessage { message_id: 1 })
    );
    assert_eq!(
        receiver.classify(last_id + 2, "opaque"),
        Err(ReliabilityError::InvalidSequence {
            message_id: last_id + 2,
            expected: Some(last_id + 1)
        })
    );
    Ok(())
}

#[test]
fn receiver_rejects_zero_and_initial_gaps_without_processing() {
    let receiver = ReliableReceiver::default();
    for id in [0, 2, 42] {
        assert_eq!(
            receiver.classify(id, "opaque"),
            Err(ReliabilityError::InvalidSequence {
                message_id: id,
                expected: Some(1)
            })
        );
    }
    assert!(receiver.recent.is_empty());
    assert_eq!(receiver.highest, 0);
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn message_ids_never_wrap_at_u64_boundary() -> ControlResult<()> {
    let mut sender = ReliableSender {
        next_id: Some(u64::MAX),
        ..ReliableSender::default()
    };
    let mut sink = TestSink::default();
    let clock = Clock::default();
    assert_eq!(sender.send(&mut sink, "last", &clock).await?, u64::MAX);
    assert_error(
        sender.send(&mut sink, "would wrap", &clock).await,
        ReliabilityError::IdExhausted,
    );
    assert!(sender.acknowledge(u64::MAX));
    let mut receiver = ReliableReceiver {
        highest: u64::MAX - 1,
        ..ReliableReceiver::default()
    };
    assert_eq!(receiver.classify(u64::MAX, "last")?, Receipt::New);
    receiver.commit(u64::MAX, "last");
    assert_eq!(receiver.classify(u64::MAX, "last")?, Receipt::Duplicate);
    assert_eq!(
        receiver.classify(0, "new"),
        Err(ReliabilityError::InvalidSequence {
            message_id: 0,
            expected: None
        })
    );
    Ok(())
}

#[allow(clippy::disallowed_methods)]
#[tokio::test]
async fn failed_or_cancelled_write_clears_pending_and_closes_sender() -> ControlResult<()> {
    let clock = Clock::default();
    let mut sender = ReliableSender::default();
    let mut sink = TestSink {
        fail: true,
        ..TestSink::default()
    };
    assert!(sender.send(&mut sink, "opaque", &clock).await.is_err());
    assert!(sender.closed);
    assert_eq!(sender.pending_len(), 0);
    let mut sender = ReliableSender::default();
    let mut sink = TestSink {
        stall: true,
        ..TestSink::default()
    };
    assert!(
        timeout(Duration::ZERO, sender.send(&mut sink, "opaque", &clock))
            .await
            .is_err()
    );
    assert!(sender.closed);
    assert_eq!(sender.pending_len(), 0);
    assert_eq!(sender.pending_bytes, 0);
    Ok(())
}

#[allow(clippy::disallowed_methods)]
#[tokio::test(start_paused = true)]
async fn stalled_write_reaches_explicit_timeout() {
    let mut sender = ReliableSender::default();
    let mut sink = TestSink {
        stall: true,
        ..TestSink::default()
    };
    assert_error(
        sender.send(&mut sink, "opaque", &Clock::default()).await,
        ReliabilityError::WriteTimedOut,
    );
    assert!(sender.closed);
    assert_eq!(sender.pending_len(), 0);
}
