#![deny(clippy::disallowed_methods)]

use super::*;
use crate::client::handlers::{IncomingMessageContext, process_incoming_message_at};
use std::{
    net::Ipv4Addr,
    sync::{MutexGuard, atomic::AtomicBool},
};

struct Fixture {
    socket: UdpSocket,
    server: UdpSocket,
    peer: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    relay: Arc<Mutex<bool>>,
    mirrors: Arc<AtomicBool>,
    server_addr: String,
}

impl Fixture {
    fn new(now: Instant) -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        for recipient in [&server, &peer] {
            recipient.set_read_timeout(Some(Duration::from_secs(2)))?;
        }
        let peers = Arc::new(Mutex::new(vec![PeerInfo {
            addr: peer.local_addr()?,
            username: "upstream".into(),
            last_direct_activity: Some(now),
            connected: true,
            created_at: now,
            use_server_relay: false,
            relay_requested: false,
        }]));
        let server_addr = server.local_addr()?.to_string();
        Ok(Self {
            socket,
            server,
            peer,
            peers,
            server_addr,
            relay: Arc::new(Mutex::new(false)),
            mirrors: Arc::new(AtomicBool::new(false)),
        })
    }

    fn context(&self, is_relay: bool) -> PathContext<'_> {
        PathContext {
            socket: &self.socket,
            peers: &self.peers,
            server_id: "s",
            channel: "c",
            user: "me",
            signaling_addr: &self.server_addr,
            is_relay,
            send_via_server: false,
        }
    }

    fn lock_peers(&self) -> io::Result<MutexGuard<'_, Vec<PeerInfo>>> {
        self.peers
            .lock()
            .map_err(|_| io::Error::other("test peer-state mutex is poisoned"))
    }

    fn incoming(&self) -> io::Result<IncomingMessageContext<'_>> {
        Ok(IncomingMessageContext {
            socket: &self.socket,
            peers: &self.peers,
            user: "me",
            is_relay: &self.relay,
            channel_has_server_relays: &self.mirrors,
            signaling_addr: &self.server_addr,
            server_addr: self.server.local_addr()?,
        })
    }

    fn receive(&self, socket: &UdpSocket) -> io::Result<String> {
        let mut bytes = [0; 256];
        let (len, source) = socket.recv_from(&mut bytes)?;
        assert_eq!(source, self.socket.local_addr()?);
        String::from_utf8(bytes[..len].to_vec()).map_err(io::Error::other)
    }
}

#[test]
fn established_failure_obeys_threshold_and_check_cadence_once() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let mut monitor = PathMonitor::new(start);
    monitor.tick(&fixture.context(false), start + Duration::from_millis(4900))?;
    monitor.tick(&fixture.context(false), start + ESTABLISHED_PATH_TIMEOUT)?;
    assert!(!fixture.lock_peers()?[0].relay_requested);
    monitor.tick(&fixture.context(false), start + Duration::from_millis(5150))?;
    assert_eq!(fixture.receive(&fixture.server)?, "REQUEST_RELAY s c me");
    assert!(fixture.lock_peers()?[0].relay_requested);
    monitor.tick(&fixture.context(false), start + Duration::from_secs(30))?;
    fixture.server.set_nonblocking(true)?;
    assert!(matches!(
        fixture.server.recv_from(&mut [0; 256]),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    Ok(())
}

#[test]
fn relay_probes_once_per_second_and_tolerates_missing_responses() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let mut monitor = PathMonitor::new(start);
    monitor.tick(&fixture.context(true), start)?;
    assert_eq!(fixture.receive(&fixture.peer)?, "PING");
    monitor.tick(&fixture.context(true), start + PATH_CHECK_INTERVAL)?;
    monitor.tick(&fixture.context(true), start + PATH_PROBE_INTERVAL)?;
    assert_eq!(fixture.receive(&fixture.peer)?, "PING");
    let response = start + Duration::from_millis(4500);
    process_incoming_message_at(
        &fixture.incoming()?,
        "PONG",
        fixture.peer.local_addr()?,
        response,
    )?;
    monitor.tick(&fixture.context(true), start + ESTABLISHED_PATH_TIMEOUT)?;
    assert!(!fixture.lock_peers()?[0].relay_requested);
    assert_eq!(fixture.receive(&fixture.peer)?, "PING");
    monitor.tick(&fixture.context(true), response + ESTABLISHED_PATH_TIMEOUT)?;
    assert_eq!(
        fixture.receive(&fixture.server)?,
        "PEER_TIMEOUT s c upstream"
    );
    let retired = fixture.lock_peers()?[0].last_direct_activity;
    process_incoming_message_at(
        &fixture.incoming()?,
        "PONG",
        fixture.peer.local_addr()?,
        response + Duration::from_secs(6),
    )?;
    assert_eq!(fixture.lock_peers()?[0].last_direct_activity, retired);
    assert!(!fixture.lock_peers()?[0].connected);
    Ok(())
}

#[test]
fn initial_setup_uses_separate_grace_and_never_proves_direct_health() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    fixture.lock_peers()?[0].last_direct_activity = None;
    let mut monitor = PathMonitor::new(start);
    monitor.tick(
        &fixture.context(false),
        start + Duration::from_millis(11_750),
    )?;
    assert!(!fixture.lock_peers()?[0].relay_requested);
    monitor.tick(&fixture.context(false), start + INITIAL_PATH_TIMEOUT)?;
    assert_eq!(fixture.receive(&fixture.server)?, "REQUEST_RELAY s c me");
    assert_eq!(fixture.lock_peers()?[0].last_direct_activity, None);
    Ok(())
}

#[test]
fn only_valid_input_from_the_observed_path_refreshes_health() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let later = start + Duration::from_secs(4);
    for message in [
        "DATA upstream relayed",
        "PONG",
        "MODE DIRECT upstream 127.0.0.1:1",
    ] {
        process_incoming_message_at(
            &fixture.incoming()?,
            message,
            fixture.server.local_addr()?,
            later,
        )?;
        assert_eq!(fixture.lock_peers()?[0].last_direct_activity, Some(start));
    }
    for message in ["garbage", "DATA upstream ", "DATA upstream a\nMODE RELAY"] {
        process_incoming_message_at(
            &fixture.incoming()?,
            message,
            fixture.peer.local_addr()?,
            later,
        )?;
        assert_eq!(fixture.lock_peers()?[0].last_direct_activity, Some(start));
    }
    process_incoming_message_at(
        &fixture.incoming()?,
        "DATA another forwarded",
        fixture.peer.local_addr()?,
        later,
    )?;
    assert_eq!(fixture.lock_peers()?[0].last_direct_activity, Some(later));
    Ok(())
}

#[test]
fn server_routes_do_not_trigger_fallback_requests() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let mut monitor = PathMonitor::new(start);
    let mut context = fixture.context(false);
    context.send_via_server = true;
    monitor.tick(&context, start + Duration::from_secs(60))?;
    assert!(!fixture.lock_peers()?[0].relay_requested);
    context.send_via_server = false;
    fixture.lock_peers()?[0].use_server_relay = true;
    monitor.tick(&context, start + Duration::from_secs(61))?;
    assert!(!fixture.lock_peers()?[0].relay_requested);
    Ok(())
}

#[test]
fn fallback_send_error_is_nonfatal_and_preserves_peer() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let mut monitor = PathMonitor::new(start);
    let mut context = fixture.context(false);
    // Force a local send error: the fixture socket is IPv4.
    context.signaling_addr = "[::1]:1";
    monitor.tick(&context, start + ESTABLISHED_PATH_TIMEOUT)?;
    monitor.tick(&context, start + Duration::from_secs(60))?;
    let peers = fixture.lock_peers()?;
    assert_eq!(peers.len(), 1);
    assert!(peers[0].relay_requested);
    Ok(())
}

#[test]
fn failed_upstream_does_not_revive_an_older_advertisement() -> io::Result<()> {
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let old_endpoint = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    old_endpoint.set_nonblocking(true)?;
    let mut old = fixture.lock_peers()?[0].clone();
    old.addr = old_endpoint.local_addr()?;
    old.username = "old-upstream".into();
    fixture.lock_peers()?.insert(0, old);
    let mut monitor = PathMonitor::new(start);
    monitor.tick(&fixture.context(false), start + ESTABLISHED_PATH_TIMEOUT)?;
    assert_eq!(fixture.receive(&fixture.server)?, "REQUEST_RELAY s c me");
    monitor.tick(&fixture.context(false), start + Duration::from_secs(20))?;
    assert!(!fixture.lock_peers()?[0].relay_requested);
    crate::client::routing::send_data(
        &fixture.socket,
        &fixture.peers,
        "DATA me payload",
        false,
        None,
    )?;
    assert!(matches!(
        old_endpoint.recv_from(&mut [0; 256]),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    Ok(())
}

#[tokio::test]
#[allow(clippy::disallowed_methods)] // tokio::test generates expect when creating its runtime.
async fn detected_relay_loss_requests_fallback_without_expiring_healthy_registrations()
-> io::Result<()> {
    use crate::signaling::{
        handlers::handle_message,
        structures::{Channel, NatKind, User},
    };
    use std::collections::HashMap;
    let start = Instant::now();
    let fixture = Fixture::new(start)?;
    let members = vec![
        User::new("me", fixture.socket.local_addr()?, NatKind::Cone, 1),
        User::new("upstream", fixture.peer.local_addr()?, NatKind::Cone, 2),
    ];
    let observed = members[0].last_pong;
    let state = Arc::new(tokio::sync::Mutex::new(HashMap::from([(
        "s".into(),
        HashMap::from([(
            "c".into(),
            Channel {
                channel_id: 7,
                next_peer_id: 3,
                users: members,
                relay: Some("upstream".into()),
            },
        )]),
    )])));
    let mut monitor = PathMonitor::new(start);
    monitor.tick(&fixture.context(false), start + ESTABLISHED_PATH_TIMEOUT)?;
    let report = fixture.receive(&fixture.server)?;
    fixture.server.set_nonblocking(true)?;
    let server = Arc::new(tokio::net::UdpSocket::from_std(
        fixture.server.try_clone()?,
    )?);
    handle_message(
        report,
        fixture.socket.local_addr()?,
        server,
        Arc::clone(&state),
    )
    .await;
    let registry = state.lock().await;
    let channel = &registry["s"]["c"];
    assert_eq!(channel.users.len(), 2);
    assert_eq!(channel.relay.as_deref(), Some("upstream"));
    assert_eq!(channel.users[0].peer_id, 1);
    assert_eq!(channel.users[0].last_pong, observed);
    assert!(channel.users[0].needs_server_relay);
    let mut packet = [0; 256];
    fixture
        .socket
        .set_read_timeout(Some(Duration::from_secs(2)))?;
    let (length, source) = fixture.socket.recv_from(&mut packet)?;
    assert_eq!(source, fixture.server.local_addr()?);
    assert_eq!(&packet[..length], b"MODE SERVER_RELAY me\n");
    Ok(())
}
