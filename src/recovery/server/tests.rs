#![allow(clippy::disallowed_methods)]

use super::*;

fn address(port: u16) -> SocketAddr {
    ([127, 0, 0, 1], port).into()
}
fn join(name: &str) -> Join {
    Join {
        server: "s".into(),
        channel: "c".into(),
        user: name.into(),
        attempt: Attempt::fresh().unwrap(),
        symmetric: false,
    }
}
fn register(registry: &mut Registry, join: Join, port: u16, now: Instant) -> (Binding, Attempt) {
    let Datagram::Challenge { challenge, .. } = registry.connect(join.clone(), address(port), now)
    else {
        panic!("challenge missing");
    };
    let Datagram::Welcome { binding } = registry.confirm(join, &challenge, address(port), now)
    else {
        panic!("welcome missing");
    };
    (binding, challenge)
}

#[test]
fn fresh_same_address_restart_retires_ids_and_rebuilds_routes() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let (relay, _) = register(&mut registry, join("a"), 4001, now);
    let (old, _) = register(&mut registry, join("b"), 4002, now);
    registry.fallback(old.session, old.session.peer, address(4002));
    let (fresh, _) = register(&mut registry, join("b"), 4002, now);
    assert_ne!(fresh.session, old.session);
    assert!(!registry.valid_binding(&old));
    let channel = &registry.channels[&("s".into(), "c".into())];
    assert_eq!(channel.relay, Some(relay.session.peer));
    assert_eq!(channel.users.len(), 2);
    let user = channel
        .users
        .iter()
        .find(|user| user.binding == fresh)
        .unwrap();
    let snapshot = channel.snapshot(user);
    assert!(!snapshot.via_server);
    assert_eq!(snapshot.neighbours[0].peer, relay.session.peer);
    let revision = channel.revision;
    registry.fallback(old.session, old.session.peer, address(4002));
    registry.handle(
        Datagram::Disconnect {
            session: old.session,
        },
        address(4002),
        now,
    );
    assert_eq!(
        registry.channels[&("s".into(), "c".into())].revision,
        revision
    );
    assert!(registry.valid_binding(&fresh));
}

#[test]
fn delayed_connect_and_confirmation_cannot_evict_committed_replacement() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let (old, old_challenge) = register(&mut registry, join("a"), 4001, now);
    let (new, _) = register(&mut registry, join("a"), 4009, now);
    registry.connect(old.join.clone(), address(4001), now);
    assert!(registry.valid_binding(&new));
    assert!(matches!(
        registry.confirm(old.join, &old_challenge, address(4001), now),
        Datagram::Rejected { .. }
    ));
    assert!(registry.valid_binding(&new));
}

#[test]
fn repeats_preserve_ids_routes_and_do_not_refresh_expiry() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let mut request = join("a");
    request.symmetric = true;
    let (binding, challenge) = register(&mut registry, request, 4001, now);
    let key = ("s".into(), "c".into());
    let revision = registry.channels[&key].revision;
    assert!(matches!(
        registry.connect(
            binding.join.clone(),
            address(4001),
            now + Duration::from_secs(5)
        ),
        Datagram::Welcome { .. }
    ));
    assert!(matches!(
        registry.confirm(
            binding.join.clone(),
            &challenge,
            address(4001),
            now + Duration::from_secs(5)
        ),
        Datagram::Welcome { .. }
    ));
    assert_eq!(registry.channels[&key].revision, revision);
    assert!(
        registry.channels[&key]
            .snapshot(&registry.channels[&key].users[0])
            .via_server
    );
    registry.expire(now + REGISTRATION_TIMEOUT);
    assert!(!registry.valid_binding(&binding));
}

#[test]
fn heartbeat_sequences_and_source_are_scoped_and_quiet() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let (binding, _) = register(&mut registry, join("a"), 4001, now);
    let heartbeat = |sequence| Datagram::Hb {
        session: binding.session,
        sequence,
    };
    assert_eq!(registry.handle(heartbeat(1), address(4001), now).len(), 1);
    assert!(
        registry
            .handle(heartbeat(1), address(4001), now + Duration::from_secs(5))
            .is_empty()
    );
    assert!(matches!(
        registry.handle(heartbeat(2), address(4999), now)[0].1,
        Datagram::SessionRejected { .. }
    ));
    registry.expire(now + REGISTRATION_TIMEOUT);
    assert!(!registry.valid_binding(&binding));
}

#[test]
fn pending_deadline_is_not_refreshed_and_capacity_is_bounded() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let request = join("a");
    let Datagram::Challenge { challenge, .. } =
        registry.connect(request.clone(), address(4001), now)
    else {
        panic!();
    };
    registry.connect(request.clone(), address(4001), now + Duration::from_secs(4));
    assert!(matches!(
        registry.confirm(request, &challenge, address(4001), now + PENDING_TTL),
        Datagram::Rejected { .. }
    ));
    for i in 0..MAX_PENDING {
        registry.connect(join(&format!("user{i}")), address(4001), now + PENDING_TTL);
    }
    assert_eq!(registry.pending.len(), MAX_PENDING);
    assert!(matches!(
        registry.connect(join("overflow"), address(4001), now + PENDING_TTL),
        Datagram::Rejected { .. }
    ));
    assert!(registry.channels.is_empty());
}

#[test]
fn oversized_snapshot_rejects_join_before_mutation() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let mut rejected = false;
    for i in 0..100 {
        let request = join(&format!("{i:0128}"));
        let Datagram::Challenge { challenge, .. } =
            registry.connect(request.clone(), address(4001), now)
        else {
            panic!();
        };
        let before = registry
            .channels
            .get(&("s".into(), "c".into()))
            .map(|channel| (channel.revision, channel.next_peer, channel.users.len()));
        if matches!(
            registry.confirm(request, &challenge, address(4001), now),
            Datagram::Rejected { .. }
        ) {
            let after = registry
                .channels
                .get(&("s".into(), "c".into()))
                .map(|channel| (channel.revision, channel.next_peer, channel.users.len()));
            assert_eq!(before, after);
            rejected = true;
            break;
        }
    }
    assert!(rejected);
}

#[test]
fn server_forwarding_excludes_direct_copies_and_rejects_retired_origin() {
    let now = Instant::now();
    let mut registry = Registry::default();
    let (relay, _) = register(&mut registry, join("relay"), 4001, now);
    let (a, _) = register(&mut registry, join("a"), 4002, now);
    let (b, _) = register(&mut registry, join("b"), 4003, now);
    let (c, _) = register(&mut registry, join("c"), 4004, now);
    register(&mut registry, join("direct-only"), 4005, now);
    // Fallback was applied at the server, but b is still in the relay's send plan.
    assert!(registry.fallback(relay.session, b.session.peer, address(4001)));
    assert!(registry.fallback(relay.session, c.session.peer, address(4001)));
    let recipients = registry.deliver(
        relay.session,
        a.session.peer,
        &[b.session.peer],
        "hello",
        address(4001),
        true,
    );
    assert_eq!(recipients.len(), 1);
    assert_eq!(recipients[0].0, address(4004));
    register(&mut registry, join("a"), 4002, now);
    assert!(
        registry
            .deliver(
                relay.session,
                a.session.peer,
                &[],
                "old",
                address(4001),
                true
            )
            .is_empty()
    );
    assert!(
        registry
            .deliver(
                c.session,
                c.session.peer,
                &[],
                "bad relay",
                address(4004),
                true
            )
            .is_empty()
    );
}

#[tokio::test]
async fn real_control_rebind_replays_snapshot_and_old_close_cannot_remove_new_binding() {
    use crate::{
        control::{client::session_worker, server::handle_session_connection},
        recovery::client::Client,
    };
    let now = Instant::now();
    let mut registry = Registry::default();
    let (binding, _) = register(&mut registry, join("a"), 4001, now);
    let (neighbour, _) = register(&mut registry, join("b"), 4002, now);
    let registry = Arc::new(Mutex::new(registry));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let registry_for_task = registry.clone();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            connections.spawn(handle_session_connection(stream, registry_for_task.clone()));
        }
        while connections.join_next().await.is_some() {}
    });
    let mut client = Client::new(binding.join.clone(), address(2131), now);
    client.receive(
        Datagram::Welcome {
            binding: binding.clone(),
        },
        address(2131),
        now,
    );
    let state = Arc::new(std::sync::Mutex::new(client));
    let old = session_worker(url.clone(), binding.clone(), state.clone());
    wait_ready(&state).await;
    state.lock().unwrap().control_lost(Instant::now());
    let new = session_worker(url, binding.clone(), state.clone());
    wait_ready(&state).await;
    drop(old);
    assert!(registry.lock().await.valid_binding(&binding));
    assert!(!new.is_finished());
    assert!(new.fallback(binding.session, neighbour.session.peer));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let applied = registry.lock().await.channels[&("s".into(), "c".into())]
                .users
                .iter()
                .any(|user| user.binding == neighbour && user.via_server);
            if applied {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(new);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

async fn wait_ready(state: &Arc<std::sync::Mutex<crate::recovery::client::Client>>) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.lock().unwrap().ready {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

fn pump(
    registry: &mut Registry,
    client: &mut crate::recovery::client::Client,
    port: u16,
    now: Instant,
    drop_message: Option<usize>,
) {
    let mut queue: std::collections::VecDeque<_> = client.tick(now).into();
    let mut index = 0;
    while let Some((destination, message)) = queue.pop_front() {
        assert!(index < 20, "handshake must terminate without a packet loop");
        let dropped = Some(index) == drop_message;
        index += 1;
        if dropped {
            continue;
        }
        if destination == address(2131) {
            client.sent(&message);
            queue.extend(registry.handle(message, address(port), now));
        } else {
            queue.extend(client.receive(message, address(2131), now));
        }
    }
    if let Some(binding) = client.binding.clone() {
        let channel = &registry.channels[&("s".into(), "c".into())];
        let user = channel
            .users
            .iter()
            .find(|user| user.binding == binding)
            .unwrap();
        client
            .apply_snapshot(&binding, channel.snapshot(user), now)
            .unwrap();
    }
}

#[test]
fn lost_handshake_steps_recover_with_same_attempt_and_one_committed_peer() {
    for lost in 0..4 {
        let now = Instant::now();
        let mut registry = Registry::default();
        let mut client = crate::recovery::client::Client::new(join("a"), address(2131), now);
        let attempt = client.join.attempt.clone();
        pump(&mut registry, &mut client, 4001, now, Some(lost));
        pump(
            &mut registry,
            &mut client,
            4001,
            now + Duration::from_secs(1),
            None,
        );
        assert!(client.ready);
        assert_eq!(client.join.attempt, attempt);
        assert_eq!(registry.channels[&("s".into(), "c".into())].users.len(), 1);
    }
}

#[test]
fn server_restart_network_change_and_simulated_suspend_get_fresh_sessions() {
    for scenario in 0..3 {
        let now = Instant::now();
        let mut registry = Registry::default();
        let mut client = crate::recovery::client::Client::new(join("a"), address(2131), now);
        pump(&mut registry, &mut client, 4001, now, None);
        let old = client.binding.clone().unwrap();
        let port = if scenario == 1 { 4009 } else { 4001 };
        let elapsed = if scenario == 2 { 20 } else { 1 };
        if scenario == 0 {
            registry = Registry::default();
        }
        if scenario == 2 {
            registry.expire(now + Duration::from_secs(elapsed));
        }
        pump(
            &mut registry,
            &mut client,
            port,
            now + Duration::from_secs(elapsed),
            None,
        );
        pump(
            &mut registry,
            &mut client,
            port,
            now + Duration::from_secs(elapsed + 1),
            None,
        );
        assert!(client.ready);
        let new = client.binding.as_ref().unwrap();
        assert_ne!(new.session, old.session);
        assert_ne!(new.join.attempt, old.join.attempt);
        assert!(!registry.valid_binding(&old));
    }
}

#[tokio::test]
async fn real_udp_restart_rejects_old_heartbeat_at_the_reused_address() {
    let server = tokio::net::UdpSocket::bind(address(0)).await.unwrap();
    let client = tokio::net::UdpSocket::bind(address(0)).await.unwrap();
    let registry = Arc::new(Mutex::new(Registry::default()));
    let mut old = None;
    for _ in 0..2 {
        let join = join("same-user");
        let reply = exchange(
            &server,
            &client,
            &registry,
            Datagram::Connect { join: join.clone() },
        )
        .await;
        let Datagram::Challenge { challenge, .. } = reply else {
            panic!("challenge missing");
        };
        let reply = exchange(
            &server,
            &client,
            &registry,
            Datagram::Confirm { join, challenge },
        )
        .await;
        let Datagram::Welcome { binding } = reply else {
            panic!("welcome missing");
        };
        let reply = exchange(
            &server,
            &client,
            &registry,
            Datagram::Hb {
                session: binding.session,
                sequence: 1,
            },
        )
        .await;
        assert!(matches!(reply, Datagram::HbAck { sequence: 1, .. }));
        if let Some(old) = old {
            let reply = exchange(
                &server,
                &client,
                &registry,
                Datagram::Hb {
                    session: old,
                    sequence: 99,
                },
            )
            .await;
            assert!(matches!(reply, Datagram::SessionRejected { session } if session == old));
        }
        old = Some(binding.session);
    }
}

async fn exchange(
    server: &tokio::net::UdpSocket,
    client: &tokio::net::UdpSocket,
    registry: &SharedRegistry,
    message: Datagram,
) -> Datagram {
    tokio::time::timeout(Duration::from_secs(2), async {
        client
            .send_to(&message.encode().unwrap(), server.local_addr().unwrap())
            .await
            .unwrap();
        let mut bytes = [0; 2048];
        let (len, source) = server.recv_from(&mut bytes).await.unwrap();
        dispatch(server, registry, &bytes[..len], source, true)
            .await
            .unwrap();
        let (len, source) = client.recv_from(&mut bytes).await.unwrap();
        assert_eq!(source, server.local_addr().unwrap());
        Datagram::decode(&bytes[..len]).unwrap()
    })
    .await
    .unwrap()
}
