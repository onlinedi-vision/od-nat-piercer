use super::*;
use crate::client::path_health::INITIAL_PATH_TIMEOUT;
use std::net::Ipv4Addr;

fn peer(now: Instant, port: u16) -> PeerInfo {
    PeerInfo {
        addr: (Ipv4Addr::LOCALHOST, port).into(),
        username: "peer".into(),
        last_direct_activity: None,
        connected: false,
        created_at: now,
        use_server_relay: false,
        relay_requested: false,
    }
}

#[test]
fn initial_punches_are_fast_but_bounded_by_spacing_and_setup_deadline() {
    let start = Instant::now();
    let peers = [peer(start, 10001)];
    let mut schedule = PunchSchedule::new();
    let mut sent_at = Vec::new();
    for milliseconds in 0..=12_000 {
        let now = start + Duration::from_millis(milliseconds);
        if !collect_punches(&peers, &mut schedule, now).is_empty() {
            sent_at.push(milliseconds);
        }
    }
    assert_eq!(&sent_at[..8], &[0, 150, 300, 450, 600, 750, 900, 1050]);
    assert!(
        sent_at[7..]
            .windows(2)
            .all(|pair| pair[1] - pair[0] == 1500)
    );
    assert_eq!(sent_at.len(), 15);
    assert!(schedule.is_empty());
    assert_eq!(
        collect_punches(&peers, &mut schedule, start + INITIAL_PATH_TIMEOUT),
        []
    );
}

#[test]
fn delayed_worker_never_catches_up_and_new_peer_does_not_accelerate_old_peer() {
    let start = Instant::now();
    let mut peers = vec![peer(start, 10001)];
    let mut schedule = PunchSchedule::new();
    assert_eq!(collect_punches(&peers, &mut schedule, start).len(), 1);
    let delayed = start + Duration::from_secs(3);
    assert_eq!(collect_punches(&peers, &mut schedule, delayed).len(), 1);
    assert_eq!(collect_punches(&peers, &mut schedule, delayed), []);
    peers.push(peer(delayed, 10002));
    let targets = collect_punches(&peers, &mut schedule, delayed);
    assert_eq!(targets, [("peer".into(), peers[1].addr)]);
    let targets = collect_punches(&peers, &mut schedule, delayed + PUNCH_INITIAL_INTERVAL);
    assert_eq!(targets, [("peer".into(), peers[1].addr)]);
}

#[test]
fn success_fallback_removal_and_replacement_retire_the_previous_schedule() {
    let start = Instant::now();
    for reason in 0..4 {
        let mut peers = vec![peer(start, 10001)];
        let mut schedule = PunchSchedule::new();
        collect_punches(&peers, &mut schedule, start);
        match reason {
            0 => peers[0].connected = true,
            1 => peers[0].use_server_relay = true,
            2 => peers[0].relay_requested = true,
            _ => peers.clear(),
        }
        assert_eq!(
            collect_punches(&peers, &mut schedule, start + PUNCH_INITIAL_INTERVAL),
            []
        );
        assert!(schedule.is_empty());
    }

    let mut schedule = PunchSchedule::new();
    collect_punches(&[peer(start, 10001)], &mut schedule, start);
    let replaced_at = start + Duration::from_millis(50);
    let replacement = peer(replaced_at, 10002);
    assert_eq!(
        collect_punches(&[replacement], &mut schedule, replaced_at).len(),
        1
    );
    assert_eq!(schedule.len(), 1);
    assert!(!schedule.contains_key(&((Ipv4Addr::LOCALHOST, 10001).into(), start)));
}
