//! Deterministic transport lifecycle. The runtime owns sockets and worker cancellation.

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    time::{Duration, Instant},
};

use super::{
    HEARTBEAT_INTERVAL, RECOVERY_TIMEOUT, REGISTRATION_TIMEOUT, RETRY_SECONDS,
    wire::{Attempt, Binding, Datagram, Join, Neighbour, SessionId, Snapshot},
};

#[derive(Clone, Debug)]
pub struct RetryCycle {
    started: Instant,
    next: usize,
}

impl RetryCycle {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            next: 0,
        }
    }

    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= RECOVERY_TIMEOUT
    }

    pub fn due(&mut self, now: Instant) -> bool {
        if self.expired(now) {
            return false;
        }
        let elapsed = now.duration_since(self.started).as_secs();
        let old = self.next;
        while self.next < RETRY_SECONDS.len() && RETRY_SECONDS[self.next] <= elapsed {
            self.next += 1;
        }
        self.next != old
    }
}

struct Path {
    neighbour: Neighbour,
    started: Instant,
    observed: Option<Instant>,
    last_probe: Option<Instant>,
    fallback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Direct,
    ParticipantRelay,
    Server,
}

pub struct Client {
    pub join: Join,
    pub server: SocketAddr,
    pub binding: Option<Binding>,
    pub ready: bool,
    pub failed: bool,
    pub control_retry: Option<RetryCycle>,
    retry: Option<RetryCycle>,
    recovery_started: Option<Instant>,
    revision: u64,
    route: Route,
    paths: HashMap<u32, Path>,
    last_ack: Option<Instant>,
    ack_sequence: u64,
    heartbeat_sequence: u64,
    last_heartbeat: Option<Instant>,
    sent: VecDeque<u64>,
}

impl Client {
    #[must_use]
    pub fn new(join: Join, server: SocketAddr, now: Instant) -> Self {
        Self {
            join,
            server,
            binding: None,
            ready: false,
            failed: false,
            control_retry: None,
            retry: Some(RetryCycle::new(now)),
            recovery_started: Some(now),
            revision: 0,
            route: Route::Server,
            paths: HashMap::new(),
            last_ack: None,
            ack_sequence: 0,
            heartbeat_sequence: 0,
            last_heartbeat: None,
            sent: VecDeque::new(),
        }
    }

    pub fn recover(&mut self, now: Instant) {
        let mut join = self.join.clone();
        let Ok(attempt) = Attempt::fresh() else {
            self.failed = true;
            self.ready = false;
            self.binding = None;
            self.paths.clear();
            eprintln!("Transport recovery failed: operating-system randomness unavailable");
            return;
        };
        join.attempt = attempt;
        *self = Self::new(join, self.server, now);
    }

    /// Resume UDP registration without extending an in-progress recovery budget.
    pub fn registration_lost(&mut self, now: Instant) {
        let started = self.recovery_started.unwrap_or(now);
        self.recover(now);
        self.recovery_started = Some(started);
    }

    pub fn control_lost(&mut self, now: Instant) {
        self.recovery_started.get_or_insert(now);
        self.ready = false;
        // Reconcile an unconfirmed fallback against the next authoritative snapshot.
        for path in self.paths.values_mut() {
            path.fallback = false;
        }
        self.control_retry
            .get_or_insert_with(|| RetryCycle::new(now));
    }

    /// # Errors
    /// Rejects inactive sessions and malformed or stale complete snapshots.
    pub fn apply_snapshot(
        &mut self,
        binding: &Binding,
        snapshot: Snapshot,
        now: Instant,
    ) -> Result<(), &'static str> {
        if self.binding.as_ref() != Some(binding)
            || snapshot.session != binding.session
            || self.failed
            || self.recovery_expired(now)
        {
            return Err("snapshot for inactive session");
        }
        validate_snapshot(&snapshot)?;
        let previous_route = self.route;
        if snapshot.revision < self.revision {
            return Err("stale route snapshot");
        }
        if snapshot.revision > self.revision {
            let mut paths = HashMap::new();
            for neighbour in snapshot.neighbours {
                let was_active = self.route != Route::Server
                    && (self.route == Route::ParticipantRelay || neighbour.relay);
                let active = !snapshot.via_server && (snapshot.relay || neighbour.relay);
                let path = self
                    .paths
                    .remove(&neighbour.peer)
                    .filter(|path| path.neighbour == neighbour && (was_active || !active))
                    .unwrap_or(Path {
                        neighbour,
                        started: now,
                        observed: None,
                        last_probe: None,
                        fallback: false,
                    });
                paths.insert(path.neighbour.peer, path);
            }
            self.paths = paths;
            self.route = if snapshot.relay {
                Route::ParticipantRelay
            } else if snapshot.via_server {
                Route::Server
            } else {
                Route::Direct
            };
            self.revision = snapshot.revision;
        }
        if !self.ready || previous_route != self.route {
            println!("CONTROL route synchronized: {:?}", self.route);
        }
        self.ready = true;
        self.control_retry = None;
        self.recovery_started = None;
        Ok(())
    }

    fn recovery_expired(&self, now: Instant) -> bool {
        self.recovery_started
            .is_some_and(|started| now.duration_since(started) >= RECOVERY_TIMEOUT)
    }

    /// Generate at most one heartbeat and one probe per relevant path per tick.
    pub fn tick(&mut self, now: Instant) -> Vec<(SocketAddr, Datagram)> {
        if self.failed {
            return Vec::new();
        }
        if self.recovery_expired(now) {
            self.failed = true;
            self.ready = false;
            self.binding = None;
            self.paths.clear();
            eprintln!("Transport recovery exhausted after 30 s; explicit retry required");
            return Vec::new();
        }
        if self
            .last_ack
            .is_some_and(|ack| now.duration_since(ack) >= REGISTRATION_TIMEOUT)
        {
            println!("Server registration lost; starting bounded recovery");
            self.registration_lost(now);
        }
        if self.binding.is_none() {
            return self.registration_tick(now);
        }
        let Some(session) = self.binding.as_ref().map(|binding| binding.session) else {
            return Vec::new();
        };
        let mut output = Vec::new();
        if self
            .last_heartbeat
            .is_none_or(|last| now.duration_since(last) >= HEARTBEAT_INTERVAL)
        {
            let Some(sequence) = self.heartbeat_sequence.checked_add(1) else {
                self.registration_lost(now);
                return Vec::new();
            };
            self.heartbeat_sequence = sequence;
            self.last_heartbeat = Some(now);
            output.push((self.server, Datagram::Hb { session, sequence }));
        }
        if self.revision > 0 {
            self.probe_paths(session, now, &mut output);
        }
        output
    }

    fn registration_tick(&mut self, now: Instant) -> Vec<(SocketAddr, Datagram)> {
        if !self.retry.as_mut().is_some_and(|retry| retry.due(now)) {
            return Vec::new();
        }
        // CONNECT obtains a fresh challenge after pending state expires; confirming
        // the previous challenge alone would otherwise strand a lossy handshake.
        vec![(
            self.server,
            Datagram::Connect {
                join: self.join.clone(),
            },
        )]
    }

    fn probe_paths(
        &mut self,
        session: SessionId,
        now: Instant,
        output: &mut Vec<(SocketAddr, Datagram)>,
    ) {
        for path in self
            .paths
            .values_mut()
            .filter(|path| !path.neighbour.server_relay && !path.fallback)
        {
            if self.route == Route::Server
                || (self.route != Route::ParticipantRelay && !path.neighbour.relay)
            {
                continue;
            }
            let elapsed = now.duration_since(path.started);
            let expired = path
                .observed
                .map_or(elapsed >= Duration::from_secs(12), |observed| {
                    now.duration_since(observed) >= Duration::from_secs(5)
                });
            if expired {
                if !self.ready {
                    continue;
                }
                path.fallback = true;
                let target = if self.route == Route::ParticipantRelay {
                    path.neighbour.peer
                } else {
                    session.peer
                };
                println!(
                    "Requesting server fallback: peer={} address={} session={:?}",
                    path.neighbour.user, path.neighbour.address, session
                );
                output.push((self.server, Datagram::Fallback { session, target }));
                continue;
            }
            let interval = if path.observed.is_some() {
                HEARTBEAT_INTERVAL
            } else if elapsed < Duration::from_secs(1) {
                Duration::from_millis(150)
            } else {
                Duration::from_millis(1500)
            };
            if path
                .last_probe
                .is_none_or(|last| now.duration_since(last) >= interval)
            {
                path.last_probe = Some(now);
                let message = if path.observed.is_some() {
                    Datagram::Ping {
                        session,
                        target: path.neighbour.peer,
                    }
                } else {
                    Datagram::Punch {
                        session,
                        target: path.neighbour.peer,
                    }
                };
                output.push((path.neighbour.address, message));
            }
        }
    }

    /// Only successful socket sends can authorize heartbeat acknowledgments.
    pub fn sent(&mut self, message: &Datagram) {
        if let Datagram::Hb { session, sequence } = message
            && self
                .binding
                .as_ref()
                .is_some_and(|binding| binding.session == *session)
        {
            if self.sent.len() == 8 {
                self.sent.pop_front();
            }
            self.sent.push_back(*sequence);
        }
    }

    pub fn receive(
        &mut self,
        message: Datagram,
        source: SocketAddr,
        now: Instant,
    ) -> Vec<(SocketAddr, Datagram)> {
        if self.failed || self.recovery_expired(now) {
            return Vec::new();
        }
        if source == self.server {
            return self.receive_server(message, now);
        }
        self.receive_peer(message, source, now)
    }

    fn receive_server(&mut self, message: Datagram, now: Instant) -> Vec<(SocketAddr, Datagram)> {
        match message {
            Datagram::Challenge { attempt, challenge }
                if self.binding.is_none() && attempt == self.join.attempt && challenge.valid() =>
            {
                return vec![(
                    self.server,
                    Datagram::Confirm {
                        join: self.join.clone(),
                        challenge,
                    },
                )];
            }
            Datagram::Welcome { binding }
                if self.binding.is_none()
                    && binding.join == self.join
                    && binding.session.valid() =>
            {
                println!(
                    "Transport registered: channel={} peer={}",
                    binding.session.channel, binding.session.peer
                );
                self.binding = Some(binding);
                self.retry = None;
                self.last_ack = Some(now);
                self.control_retry = Some(RetryCycle::new(now));
            }
            Datagram::HbAck { session, sequence }
                if self.current(session)
                    && sequence > self.ack_sequence
                    && self.sent.contains(&sequence) =>
            {
                self.ack_sequence = sequence;
                self.last_ack = Some(now);
                self.sent.retain(|sent| *sent > sequence);
            }
            Datagram::SessionRejected { session } if self.current(session) => {
                println!("Server rejected retired registration; recovering");
                self.registration_lost(now);
            }
            Datagram::Rejected { attempt, reason }
                if attempt == self.join.attempt && self.binding.is_none() =>
            {
                // At most six requests per finite recovery cycle.
                eprintln!("Registration attempt rejected: {reason}");
            }
            Datagram::Delivery {
                session,
                target,
                payload,
            } if self.valid_target(session.channel, target)
                && self.paths.contains_key(&session.peer) =>
            {
                self.display(session.peer, &payload);
            }
            _ => {}
        }
        Vec::new()
    }

    fn current(&self, session: SessionId) -> bool {
        self.binding
            .as_ref()
            .is_some_and(|binding| binding.session == session)
    }

    fn valid_target(&self, channel: u64, target: u32) -> bool {
        self.revision > 0
            && self.current(SessionId {
                channel,
                peer: target,
            })
    }

    fn receive_peer(
        &mut self,
        message: Datagram,
        source: SocketAddr,
        now: Instant,
    ) -> Vec<(SocketAddr, Datagram)> {
        let (session, target) = match &message {
            Datagram::Punch { session, target }
            | Datagram::Ping { session, target }
            | Datagram::Pong { session, target }
            | Datagram::Data {
                session, target, ..
            }
            | Datagram::Forward {
                session, target, ..
            } => (*session, *target),
            _ => return Vec::new(),
        };
        if !self.valid_target(session.channel, target) {
            return Vec::new();
        }
        let Some(path) = self.paths.get_mut(&session.peer) else {
            return Vec::new();
        };
        if path.fallback
            || path.neighbour.address != source
            || path.neighbour.server_relay
            || self.route == Route::Server
            || (self.route != Route::ParticipantRelay && !path.neighbour.relay)
        {
            return Vec::new();
        }
        if let Datagram::Forward { origin, .. } = &message
            && (!path.neighbour.relay || !self.paths.contains_key(origin) || *origin == target)
        {
            return Vec::new();
        }
        let Some(path) = self.paths.get_mut(&session.peer) else {
            return Vec::new();
        };
        if path.observed.is_none() {
            println!(
                "Direct path observed: peer={} address={}",
                path.neighbour.user, source
            );
        }
        path.observed = Some(now);
        let own = SessionId {
            channel: session.channel,
            peer: target,
        };
        match message {
            Datagram::Punch { .. } | Datagram::Ping { .. } => vec![(
                source,
                Datagram::Pong {
                    session: own,
                    target: session.peer,
                },
            )],
            Datagram::Data { payload, .. } => {
                self.display(session.peer, &payload);
                if self.route == Route::ParticipantRelay {
                    self.fanout(own, session.peer, &payload)
                } else {
                    Vec::new()
                }
            }
            Datagram::Forward {
                origin, payload, ..
            } => {
                self.display(origin, &payload);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn display(&self, origin: u32, payload: &str) {
        if let Some(path) = self.paths.get(&origin) {
            println!("[{}]: {payload}", path.neighbour.user);
        }
    }

    /// # Errors
    /// Reports not-ready routing or a datagram exceeding the bounded wire budget.
    pub fn user_message(&self, payload: &str) -> Result<Vec<(SocketAddr, Datagram)>, &'static str> {
        let Some(session) = self
            .binding
            .as_ref()
            .filter(|_| self.ready)
            .map(|binding| binding.session)
        else {
            return Err("transport not ready");
        };
        // Include forwarding overhead before admitting a local submission.
        let largest = SessionId {
            channel: u64::MAX,
            peer: u32::MAX,
        };
        for message in [
            Datagram::Forward {
                session: largest,
                target: u32::MAX,
                origin: u32::MAX,
                payload: payload.into(),
            },
            Datagram::RelayData {
                session: largest,
                origin: u32::MAX,
                direct: vec![u32::MAX; self.paths.len()],
                payload: payload.into(),
            },
        ] {
            if message.encode().is_err() {
                return Err("message exceeds forwarding budget");
            }
        }
        let output = if self.route == Route::Server {
            vec![(
                self.server,
                Datagram::Data {
                    session,
                    target: 0,
                    payload: payload.into(),
                },
            )]
        } else if self.route == Route::ParticipantRelay {
            self.fanout(session, session.peer, payload)
        } else {
            let relay = self
                .paths
                .values()
                .find(|path| path.neighbour.relay && path.observed.is_some() && !path.fallback)
                .ok_or("direct route not ready; waiting for confirmed server fallback")?;
            vec![(
                relay.neighbour.address,
                Datagram::Data {
                    session,
                    target: relay.neighbour.peer,
                    payload: payload.into(),
                },
            )]
        };
        if output.iter().any(|(_, message)| message.encode().is_err()) {
            return Err("message exceeds UDP limit");
        }
        Ok(output)
    }

    fn fanout(
        &self,
        session: SessionId,
        origin: u32,
        payload: &str,
    ) -> Vec<(SocketAddr, Datagram)> {
        let mut output = Vec::new();
        let mut direct = Vec::new();
        for path in self.paths.values().filter(|path| {
            path.neighbour.peer != origin
                && !path.neighbour.server_relay
                && path.observed.is_some()
                && !path.fallback
        }) {
            direct.push(path.neighbour.peer);
            output.push((
                path.neighbour.address,
                Datagram::Forward {
                    session,
                    target: path.neighbour.peer,
                    origin,
                    payload: payload.into(),
                },
            ));
        }
        if self
            .paths
            .values()
            .any(|path| path.neighbour.server_relay && path.neighbour.peer != origin)
        {
            output.push((
                self.server,
                Datagram::RelayData {
                    session,
                    origin,
                    direct,
                    payload: payload.into(),
                },
            ));
        }
        output
    }
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), &'static str> {
    if !snapshot.session.valid()
        || snapshot.revision == 0
        || (snapshot.relay && snapshot.via_server)
        || snapshot.neighbours.iter().filter(|peer| peer.relay).count()
            > usize::from(!snapshot.relay)
    {
        return Err("invalid snapshot metadata");
    }
    for (index, peer) in snapshot.neighbours.iter().enumerate() {
        if peer.peer == snapshot.session.peer
            || peer.peer == 0
            || peer.peer == u32::MAX
            || peer.user.is_empty()
            || peer.user.len() > 128
            || peer.address.port() == 0
            || peer.address.ip().is_unspecified()
            || peer.address.ip().is_multicast()
            || (peer.relay && peer.server_relay)
            || snapshot.neighbours[..index]
                .iter()
                .any(|other| other.peer == peer.peer || other.user == peer.user)
        {
            return Err("invalid snapshot neighbour");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
