#![allow(clippy::disallowed_methods)] // Explicit assertions and controlled timestamps.

use std::{collections::HashMap, net::Ipv4Addr, time::Instant};

use super::*;
use crate::signaling::structures::{Channel, NatKind, User};

fn state(users: Vec<User>, relay: Option<&str>) -> Arc<Mutex<ServerMap>> {
    Arc::new(Mutex::new(HashMap::from([(
        "s".into(),
        HashMap::from([(
            "c".into(),
            Channel {
                channel_id: 7,
                next_peer_id: 3,
                users,
                relay: relay.map(str::to_owned),
            },
        )]),
    )])))
}

fn user(name: &str, id: u32) -> User {
    let port = 30000 + u16::try_from(id).expect("fixture peer ID");
    User::new(name, (Ipv4Addr::LOCALHOST, port).into(), NatKind::Cone, id)
}

#[tokio::test]
async fn expired_relay_does_not_remove_healthy_neighbours() {
    let mut relay = user("relay", 1);
    let relay_addr = relay.addr;
    // 41s without a PONG exceeds the server's 40s heartbeat timeout.
    relay.last_pong = Instant::now()
        .checked_sub(Duration::from_secs(41))
        .expect("fixture timestamp");
    let peer = user("peer", 2);
    let state = state(vec![relay, peer.clone()], Some("relay"));
    let (_, cleanup, notices) = collect_heartbeat_data(Arc::clone(&state)).await;
    let registry = state.lock().await;
    let channel = &registry["s"]["c"];
    assert_eq!(channel.users.len(), 1);
    assert_eq!(channel.users[0].peer_id, peer.peer_id);
    assert_eq!(channel.users[0].last_pong, peer.last_pong);
    // The remaining peer replaces the expired relay.
    assert_eq!(channel.relay.as_deref(), Some("peer"));
    assert_eq!(cleanup.len(), 0);
    let departures: Vec<_> = notices
        .iter()
        .filter(|(_, bytes)| bytes.starts_with(b"USER_LEFT "))
        .collect();
    assert_eq!(departures.len(), 1);
    assert_eq!(
        departures[0].1,
        format!("USER_LEFT relay {relay_addr}\n").as_bytes()
    );
}

#[tokio::test]
async fn server_relay_peer_is_removed_after_heartbeat_timeout() {
    let mut peer = user("peer", 2);
    peer.needs_server_relay = true;
    // 41s without a PONG exceeds the server's 40s heartbeat timeout.
    peer.last_pong = Instant::now()
        .checked_sub(Duration::from_secs(41))
        .expect("fixture timestamp");
    let state = state(vec![user("relay", 1), peer], Some("relay"));
    let (_, _, notices) = collect_heartbeat_data(Arc::clone(&state)).await;
    let registry = state.lock().await;
    assert_eq!(registry["s"]["c"].users.len(), 1);
    assert_eq!(registry["s"]["c"].users[0].name, "relay");
    assert!(
        notices
            .iter()
            .any(|(_, bytes)| bytes.starts_with(b"USER_LEFT peer "))
    );
}

#[tokio::test]
async fn cleanup_preserves_a_channel_populated_after_the_empty_snapshot() {
    let state = state(Vec::new(), None);
    let (_, cleanup, _) = collect_heartbeat_data(Arc::clone(&state)).await;
    assert_eq!(cleanup, [("s".into(), "c".into())]);
    {
        let mut registry = state.lock().await;
        let channel = registry
            .get_mut("s")
            .expect("server")
            .get_mut("c")
            .expect("channel");
        channel.users.push(user("new-peer", 2));
        channel.relay = Some("new-peer".into());
    }
    cleanup_empty_channels(Arc::clone(&state), cleanup).await;
    let registry = state.lock().await;
    assert_eq!(registry["s"]["c"].channel_id, 7);
    assert_eq!(registry["s"]["c"].users[0].name, "new-peer");
}

#[tokio::test]
async fn cleanup_still_removes_a_channel_that_remains_empty() {
    let state = state(Vec::new(), None);
    let (_, cleanup, _) = collect_heartbeat_data(Arc::clone(&state)).await;
    cleanup_empty_channels(Arc::clone(&state), cleanup).await;
    assert!(state.lock().await.is_empty());
}

#[tokio::test]
async fn expiry_never_promotes_an_ineligible_or_already_expired_participant() {
    let now = Instant::now();
    let mut expired = user("expired", 1);
    expired.last_pong = now
        .checked_sub(Duration::from_secs(41))
        .expect("fixture timestamp");
    for nat_kind in [NatKind::Cone, NatKind::Symmetric] {
        let mut remaining = user("remaining", 2);
        remaining.nat_kind = nat_kind;
        remaining.needs_server_relay = true;
        remaining.last_pong = now;
        let state = state(vec![expired.clone(), remaining], None);
        let (_, _, notices) = collect_heartbeat_data_at(Arc::clone(&state), now).await;
        assert_eq!(state.lock().await["s"]["c"].relay, None);
        assert!(notices.iter().all(|(_, bytes)| bytes != b"MODE RELAY\n"));
        assert!(
            notices
                .iter()
                .any(|(_, bytes)| bytes.starts_with(b"MODE SERVER_RELAY remaining"))
        );
    }
    let mut another = user("another", 2);
    another.last_pong = expired.last_pong;
    let state = state(vec![expired, another], Some("expired"));
    let (_, cleanup, notices) = collect_heartbeat_data_at(Arc::clone(&state), now).await;
    assert!(state.lock().await["s"]["c"].users.is_empty());
    assert_eq!(state.lock().await["s"]["c"].relay, None);
    assert_eq!(cleanup, [("s".into(), "c".into())]);
    assert_eq!(notices.len(), 0);
}

#[tokio::test]
async fn registration_expiry_keeps_its_own_threshold_and_scan_cadence() {
    let start = Instant::now();
    let mut member = user("member", 1);
    member.last_pong = start;
    let state = state(vec![member], Some("member"));
    for scan in [
        start,
        start + SERVER_HEARTBEAT_INTERVAL,
        start + REGISTRATION_EXPIRY,
    ] {
        collect_heartbeat_data_at(Arc::clone(&state), scan).await;
        assert_eq!(state.lock().await["s"]["c"].users.len(), 1);
    }
    let (_, cleanup, _) = collect_heartbeat_data_at(
        Arc::clone(&state),
        start + REGISTRATION_EXPIRY + SERVER_HEARTBEAT_INTERVAL,
    )
    .await;
    assert!(state.lock().await["s"]["c"].users.is_empty());
    assert_eq!(cleanup, [("s".into(), "c".into())]);
}
