#![allow(clippy::disallowed_methods)] // Socket barriers and assertions in tests.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use super::{
    client::ClientEvent,
    integration_tests::{PAYLOAD, Peer, run_case},
    messages::{BroadcastReport, ControlMessage, RequestRejection},
    reliability::{
        ACK_TIMEOUT, ControlResult, MAX_MESSAGE_BYTES, ReliabilityError, WINDOW, encode,
    },
    server::channel_key,
};

async fn report(
    peer: &mut Peer,
    request_id: u64,
    queued: usize,
    failed: usize,
) -> ControlResult<()> {
    assert_eq!(
        peer.event().await?,
        ClientEvent::Broadcast {
            request_id,
            report: BroadcastReport { queued, failed },
        }
    );
    Ok(())
}

#[tokio::test]
async fn broadcast_is_scoped_to_exact_pair_and_excludes_source_and_unregistered()
-> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut source = h.connect().await?;
            let mut recipient = h.connect().await?;
            let mut second = h.connect().await?;
            let mut other_channel = h.connect().await?;
            let mut other_server = h.connect().await?;
            let mut unjoined = h.connect().await?;
            source.join("a:b", "c").await?;
            recipient.join("a:b", "c").await?;
            second.join("a:b", "c").await?;
            other_channel.join("a:b", "different").await?;
            other_server.join("a", "b:c").await?;
            assert_eq!(source.send(PAYLOAD).await?, 2);
            report(&mut source, 2, 2, 0).await?;
            for peer in [&mut recipient, &mut second] {
                assert_eq!(peer.event().await?, ClientEvent::Received { message_id: 1 });
                assert_eq!(peer.processed, vec![(1, PAYLOAD.into())]);
                peer.barrier().await?;
            }
            for peer in [
                &mut source,
                &mut other_channel,
                &mut other_server,
                &mut unjoined,
            ] {
                peer.barrier().await?;
            }
            // A different origin uses request 2; recipient IDs continue independently.
            assert_eq!(recipient.send(PAYLOAD).await?, 2);
            report(&mut recipient, 2, 2, 0).await?;
            assert_eq!(
                source.event().await?,
                ClientEvent::Received { message_id: 1 }
            );
            assert_eq!(
                second.event().await?,
                ClientEvent::Received { message_id: 2 }
            );
            for peer in [
                &mut source,
                &mut recipient,
                &mut second,
                &mut other_channel,
                &mut other_server,
                &mut unjoined,
            ] {
                peer.finish().await?;
                h.result(peer.index).await?;
            }
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn duplicate_unknown_results_and_acks_release_only_matching_pending_work() -> ControlResult<()>
{
    run_case(|h| {
        Box::pin(async move {
            let mut source = h.connect().await?;
            let mut recipient = h.connect().await?;
            source.join("server", "channel").await?;
            recipient.join("server", "channel").await?;
            source.send(PAYLOAD).await?;
            report(&mut source, 2, 1, 0).await?;
            let handle = h.registry.lock().await[&channel_key("server", "channel")][0].clone();
            for message in [
                ControlMessage::BroadcastResult {
                    request_id: 2,
                    report: BroadcastReport {
                        queued: 90,
                        failed: 90,
                    },
                },
                ControlMessage::JoinAccepted { request_id: 200 },
                ControlMessage::RequestRejected {
                    request_id: 0,
                    reason: RequestRejection::InvalidRegistration,
                },
            ] {
                handle.send(Message::Text(encode(&message)?))?;
                assert_eq!(source.event().await?, ClientEvent::Other);
                assert_eq!(source.state.sender.pending_len(), 0);
            }
            assert!(matches!(
                recipient.message().await?,
                ControlMessage::ControlMsg { message_id: 1, .. }
            ));
            for id in [0, 200, 1, 1] {
                recipient
                    .raw(&ControlMessage::ControlAck { message_id: id })
                    .await?;
            }
            recipient.barrier().await?;
            source.send(PAYLOAD).await?;
            report(&mut source, 3, 1, 0).await?;
            assert!(matches!(
                recipient.message().await?,
                ControlMessage::ControlMsg { message_id: 2, .. }
            ));
            recipient
                .raw(&ControlMessage::ControlAck { message_id: 1 })
                .await?;
            recipient.barrier().await?;
            let expired = *recipient.server_time.borrow() + ACK_TIMEOUT;
            recipient.server_time.send_replace(expired);
            let error = h
                .result(recipient.index)
                .await
                .expect_err("old ACK cannot clear ID 2");
            assert_eq!(
                error.downcast_ref::<ReliabilityError>(),
                Some(&ReliabilityError::Unconfirmed { message_id: 2 })
            );
            // A result received no extra ACK; a following valid request still works.
            source.send(PAYLOAD).await?;
            report(&mut source, 4, 0, 0).await?;
            source.finish().await?;
            h.result(source.index).await
        })
    })
    .await
}

#[tokio::test]
async fn missing_recipient_ack_closes_without_retransmission() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut source = h.connect().await?;
            let mut recipient = h.connect().await?;
            source.join("server", "channel").await?;
            recipient.join("server", "channel").await?;
            source.send(PAYLOAD).await?;
            report(&mut source, 2, 1, 0).await?;
            assert!(matches!(
                recipient.message().await?,
                ControlMessage::ControlMsg { message_id: 1, .. }
            ));
            recipient.barrier().await?;
            let expired = *recipient.server_time.borrow() + ACK_TIMEOUT;
            recipient.server_time.send_replace(expired);
            let error = h.result(recipient.index).await.expect_err("missing ACK");
            assert_eq!(
                error.downcast_ref::<ReliabilityError>(),
                Some(&ReliabilityError::Unconfirmed { message_id: 1 })
            );
            assert!(matches!(
                recipient.read.next().await.unwrap()?,
                Message::Close(_)
            ));
            source.send(PAYLOAD).await?;
            report(&mut source, 3, 0, 0).await?;
            source.finish().await?;
            h.result(source.index).await
        })
    })
    .await
}

#[tokio::test]
async fn saturated_recipient_is_isolated_and_source_gets_partial_report() -> ControlResult<()> {
    run_case(|h| Box::pin(async move {
        let mut source = h.connect().await?;
        let mut slow = h.connect().await?;
        let mut healthy = h.connect().await?;
        for peer in [&mut source, &mut slow, &mut healthy] { peer.join("server", "channel").await?; }
        for id in 1..=WINDOW as u64 {
            assert_eq!(source.send(PAYLOAD).await?, id + 1);
            report(&mut source, id + 1, 2, 0).await?;
            assert_eq!(healthy.event().await?, ClientEvent::Received { message_id: id });
            healthy.barrier().await?;
            assert!(matches!(slow.message().await?, ControlMessage::ControlMsg { message_id, .. } if message_id == id));
        }
        slow.barrier().await?; // all 32 slots are now pending ACKs
        let id = source.send(PAYLOAD).await?;
        report(&mut source, id, 1, 1).await?;
        assert_eq!(healthy.event().await?, ClientEvent::Received { message_id: 33 });
        healthy.barrier().await?;
        let error = h.result(slow.index).await.expect_err("saturated recipient");
        assert_eq!(error.downcast_ref::<ReliabilityError>(), Some(&ReliabilityError::SlowConsumer));
        assert!(matches!(slow.read.next().await.unwrap()?, Message::Close(_)));
        let id = source.send(PAYLOAD).await?;
        report(&mut source, id, 1, 0).await?;
        assert_eq!(healthy.event().await?, ClientEvent::Received { message_id: 34 });
        for peer in [&mut source, &mut healthy] {
            peer.finish().await?;
            h.result(peer.index).await?;
        }
        Ok(())
    })).await
}

#[tokio::test]
async fn acked_slots_allow_progress_with_an_old_notification_pending() -> ControlResult<()> {
    run_case(|h| Box::pin(async move {
        let mut source = h.connect().await?;
        let mut recipient = h.connect().await?;
        source.join("server", "channel").await?;
        recipient.join("server", "channel").await?;
        for id in 1..=70 {
            source.send(PAYLOAD).await?;
            report(&mut source, id + 1, 1, 0).await?;
            assert!(matches!(recipient.message().await?, ControlMessage::ControlMsg { message_id, .. } if message_id == id));
            if id != 1 {
                recipient.raw(&ControlMessage::ControlAck { message_id: id }).await?;
                recipient.barrier().await?;
            }
        }
        recipient.raw(&ControlMessage::ControlAck { message_id: 1 }).await?;
        for peer in [&mut source, &mut recipient] {
            peer.finish().await?;
            h.result(peer.index).await?;
        }
        Ok(())
    })).await
}

#[tokio::test]
async fn exact_escaped_notification_limit_is_validated_before_fanout() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut source = h.connect().await?;
            let mut recipient = h.connect().await?;
            source.join("server", "channel").await?;
            recipient.join("server", "channel").await?;
            let overhead = encode(&ControlMessage::ControlMsg {
                message_id: u64::MAX,
                payload: String::new(),
            })?
            .len();
            let body = MAX_MESSAGE_BYTES - overhead;
            let exact = format!("{}{}", "\0".repeat(body / 6), "a".repeat(body % 6));
            let too_large = exact.clone() + "a";
            assert_eq!(source.send(&too_large).await?, 2); // request fits, recipient envelope does not
            assert_eq!(
                source.event().await?,
                ClientEvent::Rejected {
                    request_id: 2,
                    reason: RequestRejection::MessageTooLarge
                }
            );
            recipient.barrier().await?; // no partial fanout before size rejection
            assert!(source.send(&"\0".repeat(MAX_MESSAGE_BYTES)).await.is_err());
            assert_eq!(source.send(&exact).await?, 3); // local rejection did not consume 3
            report(&mut source, 3, 1, 0).await?;
            assert_eq!(
                recipient.event().await?,
                ClientEvent::Received { message_id: 1 }
            );
            assert_eq!(recipient.processed, vec![(1, exact)]);
            for peer in [&mut source, &mut recipient] {
                peer.finish().await?;
                h.result(peer.index).await?;
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn cancellation_releases_work_and_closed_registry_handles_are_pruned() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut source = h.connect().await?;
            let mut recipient = h.connect().await?;
            source.join("server", "channel").await?;
            recipient.join("server", "channel").await?;
            let handle = h.registry.lock().await[&channel_key("server", "channel")][1].clone();
            source.send(PAYLOAD).await?;
            report(&mut source, 2, 1, 0).await?;
            assert!(matches!(
                recipient.message().await?,
                ControlMessage::ControlMsg { .. }
            ));
            recipient.barrier().await?;
            h.aborts[recipient.index].abort();
            // Joining the cancellation is an explicit resource-release barrier.
            assert!(h.result(recipient.index).await.is_err());
            assert!(handle.is_closed());
            assert!(!handle.enqueue(&Arc::from(PAYLOAD)));
            source.send(PAYLOAD).await?;
            report(&mut source, 3, 0, 0).await?;
            assert_eq!(
                h.registry.lock().await[&channel_key("server", "channel")].len(),
                1
            );
            source.finish().await?;
            h.result(source.index).await?;
            assert!(h.registry.lock().await.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn recipient_close_with_pending_delivery_reports_unconfirmed() -> ControlResult<()> {
    run_case(|h| {
        Box::pin(async move {
            let mut source = h.connect().await?;
            let mut recipient = h.connect().await?;
            source.join("server", "channel").await?;
            recipient.join("server", "channel").await?;
            source.send(PAYLOAD).await?;
            report(&mut source, 2, 1, 0).await?;
            assert!(matches!(
                recipient.message().await?,
                ControlMessage::ControlMsg { message_id: 1, .. }
            ));
            recipient.finish().await?;
            let error = h
                .result(recipient.index)
                .await
                .expect_err("unacknowledged close");
            assert_eq!(
                error.downcast_ref::<ReliabilityError>(),
                Some(&ReliabilityError::ConnectionClosed { pending: 1 })
            );
            source.send(PAYLOAD).await?;
            report(&mut source, 3, 0, 0).await?;
            source.finish().await?;
            h.result(source.index).await
        })
    })
    .await
}
