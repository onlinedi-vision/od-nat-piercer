//! Single transport registry; mutations preflight complete CONTROL snapshots.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use rand::{RngCore, rngs::OsRng};
use tokio::sync::Mutex;

use crate::control::{messages::ControlMessage, reliability::encode, server::ControlPeer};

use super::{
    REGISTRATION_TIMEOUT,
    wire::{Attempt, Binding, Datagram, Join, Neighbour, SessionId, Snapshot},
};

pub type SharedRegistry = Arc<Mutex<Registry>>;
pub type Replies = Vec<(SocketAddr, Datagram)>;
type Key = (String, String);
const PENDING_TTL: Duration = Duration::from_secs(5);
const MAX_PENDING: usize = 256;

struct Pending {
    join: Join,
    address: SocketAddr,
    challenge: Attempt,
    expires: Instant,
}

#[derive(Clone)]
struct User {
    binding: Binding,
    address: SocketAddr,
    challenge: Attempt,
    heartbeat: Instant,
    sequence: u64,
    via_server: bool,
    control: Option<ControlPeer>,
}

#[derive(Clone)]
struct Channel {
    id: u64,
    next_peer: u32,
    revision: u64,
    relay: Option<u32>,
    users: Vec<User>,
}

impl Channel {
    fn normalize(&mut self) {
        if !self
            .users
            .iter()
            .any(|user| Some(user.binding.session.peer) == self.relay && !user.via_server)
        {
            self.relay = self
                .users
                .iter()
                .find(|user| !user.via_server)
                .map(|user| user.binding.session.peer);
        }
    }

    fn snapshot(&self, user: &User) -> Snapshot {
        Snapshot {
            session: user.binding.session,
            revision: self.revision,
            relay: Some(user.binding.session.peer) == self.relay,
            via_server: user.via_server || self.relay.is_none(),
            neighbours: self
                .users
                .iter()
                .filter(|other| other.binding.session != user.binding.session)
                .map(|other| Neighbour {
                    peer: other.binding.session.peer,
                    user: other.binding.join.user.clone(),
                    address: other.address,
                    server_relay: other.via_server || self.relay.is_none(),
                    relay: Some(other.binding.session.peer) == self.relay,
                })
                .collect(),
        }
    }

    fn prepare(&mut self) -> Result<(), &'static str> {
        self.normalize();
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("route revision exhausted")?;
        for user in &self.users {
            encode(&ControlMessage::RouteSnapshot {
                message_id: u64::MAX,
                snapshot: self.snapshot(user),
            })
            .map_err(|_| "route snapshot exceeds CONTROL limit")?;
        }
        Ok(())
    }

    fn publish(&self) {
        for user in &self.users {
            if let Some(control) = &user.control {
                control.enqueue_snapshot(self.snapshot(user));
            }
        }
    }
}

#[derive(Default)]
pub struct Registry {
    channels: HashMap<Key, Channel>,
    pending: HashMap<(String, String, String), Pending>,
}

impl Registry {
    fn current(&self, join: &Join, address: SocketAddr) -> Option<&User> {
        self.channels
            .get(&(join.server.clone(), join.channel.clone()))?
            .users
            .iter()
            .find(|user| user.binding.join == *join && user.address == address)
    }

    fn connect(&mut self, join: Join, address: SocketAddr, now: Instant) -> Datagram {
        if !join.valid() {
            return reject(join.attempt, "invalid registration");
        }
        if let Some(user) = self.current(&join, address) {
            return Datagram::Welcome {
                binding: user.binding.clone(),
            };
        }
        self.pending.retain(|_, pending| now < pending.expires);
        let key = (join.server.clone(), join.channel.clone(), join.user.clone());
        if let Some(pending) = self.pending.get(&key)
            && pending.join == join
            && pending.address == address
        {
            return Datagram::Challenge {
                attempt: join.attempt,
                challenge: pending.challenge.clone(),
            };
        }
        if !self.pending.contains_key(&key) && self.pending.len() >= MAX_PENDING {
            return reject(join.attempt, "pending registration capacity exhausted");
        }
        let Ok(challenge) = Attempt::fresh() else {
            return reject(join.attempt, "operating-system randomness unavailable");
        };
        let reply = Datagram::Challenge {
            attempt: join.attempt.clone(),
            challenge: challenge.clone(),
        };
        self.pending.insert(
            key,
            Pending {
                join,
                address,
                challenge,
                expires: now + PENDING_TTL,
            },
        );
        reply
    }

    fn confirm(
        &mut self,
        join: Join,
        challenge: &Attempt,
        address: SocketAddr,
        now: Instant,
    ) -> Datagram {
        if let Some(user) = self.current(&join, address)
            && user.challenge == *challenge
        {
            return Datagram::Welcome {
                binding: user.binding.clone(),
            };
        }
        let key = (join.server.clone(), join.channel.clone(), join.user.clone());
        let valid = self.pending.get(&key).is_some_and(|pending| {
            pending.join == join
                && pending.address == address
                && pending.challenge == *challenge
                && now < pending.expires
        });
        if !valid {
            return reject(join.attempt, "expired or superseded registration challenge");
        }
        self.pending.remove(&key);
        match self.commit(&join, challenge.clone(), address, now) {
            Ok(binding) => Datagram::Welcome { binding },
            Err(reason) => reject(join.attempt, reason),
        }
    }

    fn commit(
        &mut self,
        join: &Join,
        challenge: Attempt,
        address: SocketAddr,
        now: Instant,
    ) -> Result<Binding, &'static str> {
        let key = (join.server.clone(), join.channel.clone());
        let mut channel = if let Some(channel) = self.channels.get(&key) {
            channel.clone()
        } else {
            let mut id = 0;
            for _ in 0..32 {
                let mut bytes = [0; 8];
                OsRng
                    .try_fill_bytes(&mut bytes)
                    .map_err(|_| "operating-system randomness unavailable")?;
                let candidate = u64::from_le_bytes(bytes);
                if candidate != 0
                    && !self
                        .channels
                        .values()
                        .any(|channel| channel.id == candidate)
                {
                    id = candidate;
                    break;
                }
            }
            if id == 0 {
                return Err("cannot allocate channel ID");
            }
            Channel {
                id,
                next_peer: 1,
                revision: 0,
                relay: None,
                users: Vec::new(),
            }
        };
        let session = SessionId {
            channel: channel.id,
            peer: channel.next_peer,
        };
        if !session.valid() {
            return Err("peer IDs exhausted");
        }
        channel.next_peer = channel
            .next_peer
            .checked_add(1)
            .ok_or("peer IDs exhausted")?;
        let old = channel
            .users
            .iter()
            .find(|user| user.binding.join.user == join.user)
            .cloned();
        channel
            .users
            .retain(|user| user.binding.join.user != join.user);
        let binding = Binding {
            session,
            join: join.clone(),
        };
        channel.users.push(User {
            binding: binding.clone(),
            address,
            challenge,
            heartbeat: now,
            sequence: 0,
            via_server: join.symmetric,
            control: None,
        });
        channel.prepare()?;
        if let Some(control) = old.and_then(|user| user.control) {
            control.retire();
        }
        channel.publish();
        self.channels.insert(key, channel);
        Ok(binding)
    }

    fn locate(&self, session: SessionId, address: SocketAddr) -> Option<Key> {
        self.channels
            .iter()
            .find(|(_, channel)| {
                channel.id == session.channel
                    && channel
                        .users
                        .iter()
                        .any(|user| user.binding.session == session && user.address == address)
            })
            .map(|(key, _)| key.clone())
    }

    #[must_use]
    pub fn valid_binding(&self, binding: &Binding) -> bool {
        self.channels
            .get(&(binding.join.server.clone(), binding.join.channel.clone()))
            .is_some_and(|channel| channel.users.iter().any(|user| user.binding == *binding))
    }

    pub fn bind_control(&mut self, binding: &Binding, control: &ControlPeer) -> bool {
        let Some(channel) = self
            .channels
            .get_mut(&(binding.join.server.clone(), binding.join.channel.clone()))
        else {
            return false;
        };
        let Some(index) = channel
            .users
            .iter()
            .position(|user| user.binding == *binding)
        else {
            return false;
        };
        let snapshot = channel.snapshot(&channel.users[index]);
        if !control.enqueue_snapshot(snapshot) {
            return false;
        }
        if let Some(old) = channel.users[index].control.replace(control.clone())
            && !old.same_connection(control)
        {
            old.retire();
        }
        true
    }

    pub fn handle(&mut self, message: Datagram, address: SocketAddr, now: Instant) -> Replies {
        match message {
            Datagram::Connect { join } => vec![(address, self.connect(join, address, now))],
            Datagram::Confirm { join, challenge } => {
                vec![(address, self.confirm(join, &challenge, address, now))]
            }
            Datagram::Hb { session, sequence } => self.heartbeat(session, sequence, address, now),
            Datagram::Disconnect { session } => {
                if let Some(key) = self.locate(session, address) {
                    self.remove(&key, &[session.peer]);
                }
                Vec::new()
            }
            Datagram::Data {
                session,
                target: 0,
                payload,
            } => self.deliver(session, session.peer, &[], &payload, address, false),
            Datagram::RelayData {
                session,
                origin,
                direct,
                payload,
            } => self.deliver(session, origin, &direct, &payload, address, true),
            _ => Vec::new(),
        }
    }

    fn heartbeat(
        &mut self,
        session: SessionId,
        sequence: u64,
        address: SocketAddr,
        now: Instant,
    ) -> Replies {
        let Some(key) = self.locate(session, address) else {
            return vec![(address, Datagram::SessionRejected { session })];
        };
        if let Some(channel) = self.channels.get_mut(&key)
            && let Some(user) = channel
                .users
                .iter_mut()
                .find(|user| user.binding.session == session)
            && sequence > user.sequence
        {
            user.sequence = sequence;
            user.heartbeat = now;
            return vec![(address, Datagram::HbAck { session, sequence })];
        }
        Vec::new()
    }

    pub fn request_fallback(
        &mut self,
        binding: &Binding,
        target: u32,
        control: &ControlPeer,
    ) -> bool {
        let address = self
            .channels
            .get(&(binding.join.server.clone(), binding.join.channel.clone()))
            .and_then(|channel| {
                channel.users.iter().find(|user| {
                    user.binding == *binding
                        && user
                            .control
                            .as_ref()
                            .is_some_and(|active| active.same_connection(control))
                })
            })
            .map(|user| user.address);
        let Some(address) = address else {
            return false;
        };
        self.fallback(binding.session, target, address)
    }

    fn fallback(&mut self, session: SessionId, target: u32, address: SocketAddr) -> bool {
        let Some(key) = self.locate(session, address) else {
            return false;
        };
        let Some(current) = self.channels.get(&key) else {
            return false;
        };
        let is_relay = current.relay == Some(session.peer);
        if (!is_relay && target != session.peer) || (is_relay && target == session.peer) {
            return false;
        }
        let mut next = current.clone();
        let Some(user) = next
            .users
            .iter_mut()
            .find(|user| user.binding.session.peer == target)
        else {
            return false;
        };
        if user.via_server {
            return true;
        }
        user.via_server = true;
        if next.prepare().is_ok() {
            next.publish();
            self.channels.insert(key, next);
            return true;
        }
        false
    }

    fn deliver(
        &self,
        session: SessionId,
        origin: u32,
        direct: &[u32],
        payload: &str,
        address: SocketAddr,
        relay: bool,
    ) -> Replies {
        let Some(key) = self.locate(session, address) else {
            return Vec::new();
        };
        let channel = &self.channels[&key];
        if !channel
            .users
            .iter()
            .any(|user| user.binding.session.peer == origin)
        {
            return Vec::new();
        }
        if relay
            && (channel.relay != Some(session.peer)
                || direct.len() > channel.users.len()
                || direct.iter().enumerate().any(|(index, id)| {
                    *id == origin
                        || *id == session.peer
                        || direct[..index].contains(id)
                        || !channel
                            .users
                            .iter()
                            .any(|user| user.binding.session.peer == *id)
                }))
        {
            return Vec::new();
        }
        channel
            .users
            .iter()
            .filter(|user| {
                user.binding.session.peer != origin
                    && (!relay || (user.via_server && user.binding.session.peer != session.peer))
                    && !direct.contains(&user.binding.session.peer)
            })
            .map(|user| {
                (
                    user.address,
                    Datagram::Delivery {
                        session: SessionId {
                            channel: channel.id,
                            peer: origin,
                        },
                        target: user.binding.session.peer,
                        payload: payload.into(),
                    },
                )
            })
            .collect()
    }

    fn remove(&mut self, key: &Key, peers: &[u32]) {
        let Some(current) = self.channels.get(key) else {
            return;
        };
        let mut next = current.clone();
        next.users
            .retain(|user| !peers.contains(&user.binding.session.peer));
        if next.prepare().is_err() {
            // Exhausted revision cannot publish a partial topology: retire the channel.
            for user in &current.users {
                if let Some(control) = &user.control {
                    control.retire();
                }
            }
            self.channels.remove(key);
            return;
        }
        for user in current
            .users
            .iter()
            .filter(|user| peers.contains(&user.binding.session.peer))
        {
            if let Some(control) = &user.control {
                control.retire();
            }
        }
        next.publish();
        if next.users.is_empty() {
            self.channels.remove(key);
        } else {
            self.channels.insert(key.clone(), next);
        }
    }

    pub fn expire(&mut self, now: Instant) {
        self.pending.retain(|_, pending| now < pending.expires);
        let expired: Vec<_> = self
            .channels
            .iter()
            .filter_map(|(key, channel)| {
                let peers: Vec<_> = channel
                    .users
                    .iter()
                    .filter(|user| now.duration_since(user.heartbeat) >= REGISTRATION_TIMEOUT)
                    .map(|user| user.binding.session.peer)
                    .collect();
                (!peers.is_empty()).then(|| (key.clone(), peers))
            })
            .collect();
        for (key, peers) in expired {
            self.remove(&key, &peers);
        }
    }
}

fn reject(attempt: Attempt, reason: &str) -> Datagram {
    Datagram::Rejected {
        attempt,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests;

/// Dispatch one UDP datagram on the production server path.
/// # Errors
/// Returns serialization or socket-send failures.
pub async fn dispatch(
    socket: &tokio::net::UdpSocket,
    registry: &SharedRegistry,
    bytes: &[u8],
    source: std::net::SocketAddr,
    primary: bool,
) -> std::io::Result<()> {
    if bytes == b"NAT_PROBE 1\n" || bytes == b"NAT_PROBE 2\n" {
        socket
            .send_to(format!("NAT_SEEN {source}\n").as_bytes(), source)
            .await?;
    } else if primary && let Some(message) = Datagram::decode(bytes) {
        let output = registry
            .lock()
            .await
            .handle(message, source, Instant::now());
        let encoded = output
            .into_iter()
            .map(|(address, message)| message.encode().map(|bytes| (address, bytes)))
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut first_failure = None;
        for (address, bytes) in encoded {
            if let Err(error) = socket.send_to(&bytes, address).await {
                first_failure.get_or_insert(error);
            }
        }
        if let Some(error) = first_failure {
            return Err(error);
        }
    }
    Ok(())
}
