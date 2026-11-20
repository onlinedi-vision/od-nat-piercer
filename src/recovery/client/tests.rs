#![allow(clippy::disallowed_methods)]

use super::*;

fn address(port: u16) -> SocketAddr {
    ([127, 0, 0, 1], port).into()
}
fn client(now: Instant) -> Client {
    let join = Join {
        server: "s".into(),
        channel: "c".into(),
        user: "a".into(),
        attempt: Attempt::fresh().unwrap(),
        symmetric: false,
    };
    let binding = Binding {
        join: join.clone(),
        session: SessionId {
            channel: 100,
            peer: 1,
        },
    };
    let mut client = Client::new(join, address(2131), now);
    client.receive(
        Datagram::Welcome {
            binding: binding.clone(),
        },
        address(2131),
        now,
    );
    client
        .apply_snapshot(
            &binding,
            Snapshot {
                session: binding.session,
                revision: 1,
                relay: false,
                via_server: false,
                neighbours: vec![Neighbour {
                    peer: 2,
                    user: "relay".into(),
                    address: address(4002),
                    server_relay: false,
                    relay: true,
                }],
            },
            now,
        )
        .unwrap();
    client
}

#[test]
fn retries_are_finite_and_skip_missed_slots_without_burst() {
    let now = Instant::now();
    let mut cycle = RetryCycle::new(now);
    assert!(cycle.due(now));
    assert!(!cycle.due(now));
    assert!(cycle.due(now + Duration::from_secs(20)));
    assert!(!cycle.due(now + Duration::from_secs(20)));
    assert!(cycle.due(now + Duration::from_secs(23)));
    assert!(!cycle.due(now + Duration::from_secs(29)));
    assert!(cycle.expired(now + Duration::from_secs(30)));
    assert!(!cycle.due(now + Duration::from_secs(30)));
}

#[test]
fn unsent_duplicate_or_old_session_acks_cannot_extend_liveness() {
    let now = Instant::now();
    let mut client = client(now);
    let old = client.binding.clone().unwrap();
    let output = client.tick(now);
    let heartbeat = output
        .iter()
        .find(|(_, message)| matches!(message, Datagram::Hb { .. }))
        .unwrap()
        .1
        .clone();
    client.receive(
        Datagram::HbAck {
            session: old.session,
            sequence: 1,
        },
        address(2131),
        now + Duration::from_secs(1),
    );
    assert_eq!(client.last_ack, Some(now));
    client.sent(&heartbeat);
    client.receive(
        Datagram::HbAck {
            session: old.session,
            sequence: 1,
        },
        address(2131),
        now + Duration::from_secs(1),
    );
    client.receive(
        Datagram::HbAck {
            session: old.session,
            sequence: 1,
        },
        address(2131),
        now + Duration::from_secs(5),
    );
    assert_eq!(client.last_ack, Some(now + Duration::from_secs(1)));
    client.tick(now + Duration::from_secs(7));
    assert!(client.binding.is_none());
    assert_ne!(client.join.attempt, old.join.attempt);
    client.receive(
        Datagram::Welcome {
            binding: old.clone(),
        },
        address(2131),
        now + Duration::from_secs(8),
    );
    client.receive(
        Datagram::HbAck {
            session: old.session,
            sequence: 2,
        },
        address(2131),
        now + Duration::from_secs(8),
    );
    assert!(client.binding.is_none());
    assert_eq!(client.last_ack, None);
}

#[test]
fn punching_spacing_stop_on_observation_and_fallback_has_exact_address() {
    let now = Instant::now();
    let mut client = client(now);
    let punches = |output: Vec<(SocketAddr, Datagram)>| {
        output
            .iter()
            .filter(|(_, message)| matches!(message, Datagram::Punch { .. }))
            .count()
    };
    assert_eq!(punches(client.tick(now)), 1);
    assert_eq!(punches(client.tick(now + Duration::from_millis(149))), 0);
    assert_eq!(punches(client.tick(now + Duration::from_millis(150))), 1);
    let peer = SessionId {
        channel: 100,
        peer: 2,
    };
    client.receive(
        Datagram::Pong {
            session: peer,
            target: 1,
        },
        address(4002),
        now + Duration::from_millis(151),
    );
    assert_eq!(punches(client.tick(now + Duration::from_secs(1))), 0);
    client.last_ack = Some(now + Duration::from_secs(5));
    let output = client.tick(now + Duration::from_millis(5151));
    assert!(
        output
            .iter()
            .any(|(_, message)| matches!(message, Datagram::Fallback { target: 1, .. }))
    );
    assert_eq!(client.paths[&2].neighbour.address, address(4002));
}

#[test]
fn snapshot_replaces_old_binding_and_preserves_unchanged_health() {
    let now = Instant::now();
    let mut client = client(now);
    let binding = client.binding.clone().unwrap();
    client.paths.get_mut(&2).unwrap().observed = Some(now);
    let mut snapshot = Snapshot {
        session: binding.session,
        revision: 2,
        relay: false,
        via_server: false,
        neighbours: vec![client.paths[&2].neighbour.clone()],
    };
    client
        .apply_snapshot(&binding, snapshot.clone(), now)
        .unwrap();
    assert_eq!(client.paths[&2].observed, Some(now));
    snapshot.revision = 3;
    snapshot.neighbours[0].peer = 3;
    client.apply_snapshot(&binding, snapshot, now).unwrap();
    assert!(!client.paths.contains_key(&2));
    client.receive(
        Datagram::Data {
            session: SessionId {
                channel: 100,
                peer: 2,
            },
            target: 1,
            payload: "stale".into(),
        },
        address(4002),
        now,
    );
    assert_eq!(client.paths[&3].observed, None);
    let output = client.tick(now);
    assert!(
        !output
            .iter()
            .any(|(_, message)| matches!(message, Datagram::Fallback { .. }))
    );
}

#[test]
fn control_only_loss_keeps_udp_session_and_exhaustion_is_explicit() {
    let now = Instant::now();
    let mut client = client(now);
    let binding = client.binding.clone();
    client.control_lost(now);
    assert_eq!(client.binding, binding);
    assert!(client.user_message("no queue").is_err());
    for second in 0..30 {
        let time = now + Duration::from_secs(second);
        let output = client.tick(time);
        for (_, message) in output {
            if let Datagram::Hb { session, sequence } = message {
                client.sent(&Datagram::Hb { session, sequence });
                client.receive(Datagram::HbAck { session, sequence }, address(2131), time);
            }
        }
    }
    assert_eq!(client.binding, binding);
    client.tick(now + Duration::from_secs(30));
    assert!(client.failed);
    assert!(client.binding.is_none());
    client.recover(now + Duration::from_secs(31));
    assert!(!client.failed);
    assert_ne!(client.join.attempt, binding.unwrap().join.attempt);
}

#[test]
fn invalid_or_wrong_session_snapshot_is_atomic() {
    let now = Instant::now();
    let mut client = client(now);
    let binding = client.binding.clone().unwrap();
    let neighbour = client.paths[&2].neighbour.clone();
    let mut snapshot = Snapshot {
        session: binding.session,
        revision: 2,
        relay: true,
        via_server: false,
        neighbours: vec![neighbour.clone(), neighbour],
    };
    assert!(
        client
            .apply_snapshot(&binding, snapshot.clone(), now)
            .is_err()
    );
    assert_eq!(client.revision, 1);
    assert_eq!(client.paths.len(), 1);
    snapshot.neighbours.clear();
    snapshot.session.peer = 999;
    assert!(client.apply_snapshot(&binding, snapshot, now).is_err());
    assert_eq!(client.revision, 1);
}

#[test]
fn wire_rejects_oversize_unknown_fields_and_zero_attempt() {
    assert!(!Attempt("0".repeat(32)).valid());
    assert!(Attempt::fresh().unwrap().valid());
    assert!(Datagram::decode(&vec![b'a'; super::super::wire::MAX_DATAGRAM + 1]).is_none());
    assert!(
        Datagram::decode(
            br#"{"type":"HB","session":{"channel":1,"peer":1},"sequence":1,"extra":1}"#
        )
        .is_none()
    );
}

#[test]
fn promotion_starts_new_adjacencies_without_inheriting_old_setup_timeouts() {
    let now = Instant::now();
    let mut client = client(now);
    let binding = client.binding.clone().unwrap();
    let other = Neighbour {
        peer: 3,
        user: "other".into(),
        address: address(4003),
        server_relay: false,
        relay: false,
    };
    client
        .apply_snapshot(
            &binding,
            Snapshot {
                session: binding.session,
                revision: 2,
                relay: false,
                via_server: false,
                neighbours: vec![client.paths[&2].neighbour.clone(), other.clone()],
            },
            now,
        )
        .unwrap();
    let promoted = now + Duration::from_secs(20);
    client.last_ack = Some(promoted);
    client
        .apply_snapshot(
            &binding,
            Snapshot {
                session: binding.session,
                revision: 3,
                relay: true,
                via_server: false,
                neighbours: vec![other],
            },
            promoted,
        )
        .unwrap();
    let output = client.tick(promoted);
    assert!(
        output
            .iter()
            .any(|(_, message)| matches!(message, Datagram::Punch { target: 3, .. }))
    );
    assert!(
        !output
            .iter()
            .any(|(_, message)| matches!(message, Datagram::Fallback { .. }))
    );
}

#[test]
fn control_recovery_continues_health_checks_without_changing_the_udp_registration() {
    let now = Instant::now();
    let mut client = client(now);
    let binding = client.binding.clone();
    client.control_lost(now);
    assert!(!client.ready);
    let output = client.tick(now);
    assert!(
        output
            .iter()
            .any(|(_, message)| matches!(message, Datagram::Punch { .. }))
    );
    client.receive(
        Datagram::Pong {
            session: SessionId {
                channel: 100,
                peer: 2,
            },
            target: 1,
        },
        address(4002),
        now,
    );
    assert_eq!(client.paths[&2].observed, Some(now));
    assert_eq!(client.binding, binding);
}

#[test]
fn changing_recovery_layers_does_not_extend_the_deadline() {
    let now = Instant::now();
    let mut client = client(now);
    client.control_lost(now);
    client.registration_lost(now + Duration::from_secs(20));
    let binding = Binding {
        session: SessionId {
            channel: 100,
            peer: 4,
        },
        join: client.join.clone(),
    };
    client.receive(
        Datagram::Welcome {
            binding: binding.clone(),
        },
        address(2131),
        now + Duration::from_secs(29),
    );
    assert_eq!(client.binding.as_ref(), Some(&binding));
    assert!(
        client
            .apply_snapshot(
                &binding,
                Snapshot {
                    session: binding.session,
                    revision: 1,
                    relay: true,
                    via_server: false,
                    neighbours: Vec::new()
                },
                now + Duration::from_secs(30),
            )
            .is_err()
    );
    let output = client.tick(now + Duration::from_secs(30));
    assert!(output.is_empty());
    assert!(client.failed);
}
