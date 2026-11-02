//! Per-connection send queues with shared capacity limits and one socket owner.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

use super::reliability::{ReliabilityError, WINDOW};

/// Holds one message slot until ACK or connection cleanup.
pub(super) struct CapacityReservation {
    _slot: OwnedSemaphorePermit,
}

pub(super) enum Outbound {
    Control {
        payload: Arc<str>,
        reservation: CapacityReservation,
    },
    #[cfg(test)]
    Inject(tokio_tungstenite::tungstenite::Message),
}

/// A bounded recipient handle. It exposes neither a socket nor application
/// membership. Reservations live until ACK or teardown, including queued work.
#[derive(Clone)]
pub struct ControlPeer {
    tx: mpsc::Sender<Outbound>,
    slots: Arc<Semaphore>,
    stop: watch::Sender<Option<ReliabilityError>>,
}

impl ControlPeer {
    pub(super) fn new() -> (
        Self,
        mpsc::Receiver<Outbound>,
        watch::Receiver<Option<ReliabilityError>>,
    ) {
        let (tx, rx) = mpsc::channel(WINDOW);
        let (stop, stopping) = watch::channel(None);
        (
            Self {
                tx,
                slots: Arc::new(Semaphore::new(WINDOW)),
                stop,
            },
            rx,
            stopping,
        )
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed() || self.stop.borrow().is_some()
    }

    pub(super) fn same_connection(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }

    pub(super) fn enqueue(&self, payload: &Arc<str>) -> bool {
        if self.is_closed() {
            return false;
        }
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            self.close(ReliabilityError::SlowConsumer);
            return false;
        };
        let reservation = CapacityReservation { _slot: slot };
        if self
            .tx
            .try_send(Outbound::Control {
                payload: Arc::clone(payload),
                reservation,
            })
            .is_err()
        {
            self.close(ReliabilityError::SlowConsumer);
            return false;
        }
        true
    }

    pub(super) fn close(&self, reason: ReliabilityError) {
        // First failure wins. Closing admission does not mutate membership.
        self.stop.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
        self.slots.close();
    }

    // Existing reliability tests inject malformed/unknown ACKs through the
    // connection owner's queue. There is no production raw-message producer.
    #[cfg(test)]
    pub(super) fn send(
        &self,
        message: tokio_tungstenite::tungstenite::Message,
    ) -> super::reliability::ControlResult<()> {
        self.tx
            .try_send(Outbound::Inject(message))
            .map_err(|_| ReliabilityError::Closed)?;
        Ok(())
    }
}
