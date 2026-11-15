use std::{
    io,
    net::UdpSocket,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::structures::PeerInfo;
use crate::proto::control_text::{MSG_PEER_TIMEOUT, MSG_PING, MSG_REQUEST_RELAY};

pub const PATH_CHECK_INTERVAL: Duration = Duration::from_millis(250);
pub const PATH_PROBE_INTERVAL: Duration = Duration::from_secs(1);
pub const ESTABLISHED_PATH_TIMEOUT: Duration = Duration::from_secs(5);
pub const INITIAL_PATH_TIMEOUT: Duration = Duration::from_secs(12);

/// Schedules path checks and relay probes from the client's receive loop.
/// Checks run even when no media is sent.
/// A path failure requests server fallback without removing the participant.
pub struct PathMonitor {
    next_check: Instant,
    next_probe: Instant,
}

struct PathActions {
    probes: Vec<std::net::SocketAddr>,
    reports: Vec<(&'static str, String)>,
}

pub struct PathContext<'a> {
    pub socket: &'a UdpSocket,
    pub peers: &'a Arc<Mutex<Vec<PeerInfo>>>,
    pub server_id: &'a str,
    pub channel: &'a str,
    pub user: &'a str,
    pub signaling_addr: &'a str,
    pub is_relay: bool,
    pub send_via_server: bool,
}

impl PathMonitor {
    #[must_use]
    pub fn time_until_check(&self, now: Instant) -> Duration {
        self.next_check.saturating_duration_since(now)
    }

    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            next_check: now,
            next_probe: now,
        }
    }

    /// Checks local path state and attempts probes/fallback outside its lock.
    ///
    /// # Errors
    /// Returns an error if the peer-state mutex is poisoned.
    pub fn tick(&mut self, context: &PathContext<'_>, now: Instant) -> io::Result<()> {
        if now < self.next_check {
            return Ok(());
        }
        self.next_check = now + PATH_CHECK_INTERVAL;
        let probe_due = now >= self.next_probe;
        if probe_due {
            self.next_probe = now + PATH_PROBE_INTERVAL;
        }
        if !context.is_relay && context.send_via_server {
            return Ok(());
        }
        let PathActions { probes, reports } = Self::collect(context, now, probe_due)?;
        for address in probes {
            if let Err(error) = context.socket.send_to(MSG_PING.as_bytes(), address) {
                eprintln!("Failed path probe to {address}: {error}");
            }
        }
        for (command, target) in reports {
            eprintln!("Requesting server fallback for {target}; route activation is unconfirmed");
            let report = format!(
                "{command} {} {} {target}",
                context.server_id, context.channel
            );
            if let Err(error) = context
                .socket
                .send_to(report.as_bytes(), context.signaling_addr)
            {
                eprintln!("Failed fallback request for {target}: {error}");
            }
        }
        Ok(())
    }

    fn collect(
        context: &PathContext<'_>,
        now: Instant,
        probe_due: bool,
    ) -> io::Result<PathActions> {
        let mut peers = context
            .peers
            .lock()
            .map_err(|_| io::Error::other("Cannot check paths: peers mutex is poisoned"))?;
        let upstream = peers.iter().rposition(|peer| !peer.use_server_relay);
        let mut probes = Vec::new();
        let mut reports = Vec::new();
        for (index, peer) in peers.iter_mut().enumerate() {
            if peer.use_server_relay
                || peer.relay_requested
                || (!context.is_relay && Some(index) != upstream)
            {
                continue;
            }
            let (observed, threshold) = peer
                .last_direct_activity
                .map_or((peer.created_at, INITIAL_PATH_TIMEOUT), |observed| {
                    (observed, ESTABLISHED_PATH_TIMEOUT)
                });
            if now.saturating_duration_since(observed) >= threshold {
                peer.connected = false;
                peer.relay_requested = true;
                let report = if context.is_relay {
                    let command = if peer.last_direct_activity.is_some() {
                        MSG_PEER_TIMEOUT
                    } else {
                        MSG_REQUEST_RELAY
                    };
                    (command, peer.username.clone())
                } else {
                    (MSG_REQUEST_RELAY, context.user.to_owned())
                };
                reports.push(report);
            } else if context.is_relay && probe_due {
                probes.push(peer.addr);
            }
        }
        Ok(PathActions { probes, reports })
    }
}

#[cfg(test)]
mod tests;
