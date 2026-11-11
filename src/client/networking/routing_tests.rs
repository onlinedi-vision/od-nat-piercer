#![allow(clippy::disallowed_methods)] // Explicit assertions in bounded localhost tests.

use std::{
    collections::HashMap,
    io,
    net::{Ipv4Addr, SocketAddr},
};

use tokio::{net::UdpSocket as AsyncUdpSocket, sync::Mutex as AsyncMutex, time::timeout};

use super::*;
use crate::{
    client::handlers::{IncomingMessageContext, process_incoming_message},
    signaling::{
        handle_message,
        structures::{Channel, NatKind as ServerNatKind, ServerMap, User},
    },
};

// These sockets have no workers. Each production handler completes before the
// next queue is inspected; all sends are synchronous loopback or awaited sends.
// Nonblocking reads and finite drains bound the fixture without quiet sleeps.
struct Fixture {
    server: UdpSocket,
    server_sender: Arc<AsyncUdpSocket>,
    state: Arc<AsyncMutex<ServerMap>>,
    relay: UdpSocket,
    direct: UdpSocket,
    server_peer: UdpSocket,
    server_other: UdpSocket,
    peers: Arc<Mutex<Vec<PeerInfo>>>,
    is_relay: Arc<Mutex<bool>>,
    has_server_peers: Arc<AtomicBool>,
}

fn socket() -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn drain(socket: &UdpSocket) -> io::Result<Vec<(SocketAddr, String)>> {
    Ok(drain_raw(socket)?
        .into_iter()
        .map(|(source, packet)| (source, packet.trim_end().to_owned()))
        .collect())
}

fn drain_raw(socket: &UdpSocket) -> io::Result<Vec<(SocketAddr, String)>> {
    let mut packets = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        match socket.recv_from(&mut buffer) {
            Ok((len, source)) => {
                assert!(packets.len() < 16, "unexpected forwarding loop/fanout");
                let packet = std::str::from_utf8(&buffer[..len]).map_err(io::Error::other)?;
                packets.push((source, packet.to_owned()));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(packets),
            Err(error) => return Err(error),
        }
    }
}

impl Fixture {
    fn new() -> io::Result<Self> {
        let server = socket()?;
        let server_sender = Arc::new(AsyncUdpSocket::from_std(server.try_clone()?)?);
        let relay = socket()?;
        let direct = socket()?;
        let server_peer = socket()?;
        let server_other = socket()?;
        let mut users = Vec::new();
        for (name, endpoint, nat) in [
            ("relay", &relay, ServerNatKind::Cone),
            ("direct", &direct, ServerNatKind::Cone),
            ("server-peer", &server_peer, ServerNatKind::Symmetric),
            ("server-other", &server_other, ServerNatKind::Symmetric),
        ] {
            let id = u32::try_from(users.len() + 1).expect("four fixture users");
            users.push(User::new(name, endpoint.local_addr()?, nat, id));
        }
        let peers = Arc::new(Mutex::new(vec![PeerInfo {
            addr: direct.local_addr()?,
            last_pong: Instant::now(),
            username: "direct".into(),
            connected: true,
            created_at: Instant::now(),
            use_server_relay: false,
            relay_requested: false,
        }]));
        let channel = Channel {
            channel_id: 7,
            next_peer_id: 5,
            users,
            relay: Some("relay".into()),
        };
        Ok(Self {
            server,
            server_sender,
            state: Arc::new(AsyncMutex::new(HashMap::from([(
                "s".into(),
                HashMap::from([("c".into(), channel)]),
            )]))),
            relay,
            direct,
            server_peer,
            server_other,
            peers,
            is_relay: Arc::new(Mutex::new(true)),
            has_server_peers: Arc::new(AtomicBool::new(true)),
        })
    }

    async fn dispatch_server_queue(&self) -> io::Result<()> {
        for (source, packet) in drain_raw(&self.server)? {
            timeout(
                Duration::from_secs(2),
                handle_message(
                    packet,
                    source,
                    Arc::clone(&self.server_sender),
                    Arc::clone(&self.state),
                ),
            )
            .await
            .map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn dispatch_relay_queue(&self) -> io::Result<usize> {
        let packets = drain_raw(&self.relay)?;
        for (source, packet) in &packets {
            self.receive_at_relay(packet, *source)?;
        }
        Ok(packets.len())
    }

    fn receive_at_relay(&self, packet: &str, source: SocketAddr) -> io::Result<()> {
        let signaling_addr = self.server.local_addr()?.to_string();
        let context = IncomingMessageContext {
            socket: &self.relay,
            peers: &self.peers,
            user: "relay",
            is_relay: &self.is_relay,
            channel_has_server_relays: &self.has_server_peers,
            signaling_addr: &signaling_addr,
            server_addr: self.server.local_addr()?,
        };
        process_incoming_message(&context, packet, source)
    }

    async fn request_fallback(&self) -> io::Result<()> {
        self.send_to_server(&self.relay, "REQUEST_RELAY s c direct")
            .await?;
        for recipient in [
            &self.relay,
            &self.direct,
            &self.server_peer,
            &self.server_other,
        ] {
            expect_packet(
                recipient,
                self.server.local_addr()?,
                "MODE SERVER_RELAY direct",
            )?;
        }
        Ok(())
    }

    fn apply_fallback(&self) -> io::Result<()> {
        self.receive_at_relay("MODE SERVER_RELAY direct", self.server.local_addr()?)
    }

    async fn send_to_server(&self, sender: &UdpSocket, packet: &str) -> io::Result<()> {
        sender.send_to(packet.as_bytes(), self.server.local_addr()?)?;
        self.dispatch_server_queue().await
    }

    fn outgoing(
        &self,
        endpoint: &UdpSocket,
        user: &str,
        via_server: bool,
    ) -> io::Result<UserMessageContext> {
        Ok(UserMessageContext {
            socket: endpoint.try_clone()?,
            peers: Arc::clone(&self.peers),
            username: user.into(),
            send_via_server: Arc::new(AtomicBool::new(via_server)),
            signaling_addr: self.server.local_addr()?.to_string(),
            is_relay: Arc::clone(&self.is_relay),
            channel_has_server_relays: Arc::clone(&self.has_server_peers),
        })
    }
}

fn expect_packet(endpoint: &UdpSocket, source: SocketAddr, packet: &str) -> io::Result<()> {
    assert_eq!(drain(endpoint)?, [(source, packet.to_owned())]);
    Ok(())
}

fn expect_empty(endpoint: &UdpSocket) -> io::Result<()> {
    assert_eq!(drain(endpoint)?, Vec::new());
    Ok(())
}

fn peer(name: &str, addr: SocketAddr, via_server: bool) -> PeerInfo {
    PeerInfo {
        addr,
        last_pong: Instant::now(),
        username: name.into(),
        connected: true,
        created_at: Instant::now(),
        use_server_relay: via_server,
        relay_requested: false,
    }
}

fn assert_channel_unchanged(before: &Channel, after: &Channel) {
    assert_eq!(before.channel_id, after.channel_id);
    assert_eq!(before.next_peer_id, after.next_peer_id);
    assert_eq!(before.relay, after.relay);
    assert_eq!(before.users.len(), after.users.len());
    for (before, after) in before.users.iter().zip(&after.users) {
        assert_eq!(before.peer_id, after.peer_id);
        assert_eq!(before.name, after.name);
        assert_eq!(before.addr, after.addr);
        assert_eq!(before.last_pong, after.last_pong);
        assert_eq!(before.needs_server_relay, after.needs_server_relay);
        assert_eq!(before.nat_kind, after.nat_kind);
    }
}

#[tokio::test]
async fn server_fanout_is_not_forwarded_again_by_participant_relay() -> io::Result<()> {
    let fixture = Fixture::new()?;
    // DATA is only a carrier for this opaque deterministic payload.
    let packet = "DATA server-peer 0001a5ff";
    fixture.send_to_server(&fixture.server_peer, packet).await?;
    assert_eq!(fixture.dispatch_relay_queue()?, 1);
    fixture.dispatch_server_queue().await?;

    let direct = drain(&fixture.direct)?;
    let server_other = drain(&fixture.server_other)?;
    assert_eq!(
        (direct.len(), server_other.len()),
        (1, 1),
        "overlapping participant/server forwards"
    );
    assert_eq!(direct[0], (fixture.server.local_addr()?, packet.into()));
    assert_eq!(
        server_other[0],
        (fixture.server.local_addr()?, packet.into())
    );
    assert!(drain(&fixture.server_peer)?.is_empty(), "sender echo");
    assert!(drain(&fixture.server)?.is_empty(), "forwarding loop");
    Ok(())
}

#[tokio::test]
async fn fallback_with_delayed_relay_notice_does_not_duplicate_a_payload() -> io::Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .send_to_server(&fixture.relay, "REQUEST_RELAY s c direct")
        .await?;
    for recipient in [
        &fixture.relay,
        &fixture.direct,
        &fixture.server_peer,
        &fixture.server_other,
    ] {
        assert_eq!(
            drain(recipient)?,
            [(
                fixture.server.local_addr()?,
                "MODE SERVER_RELAY direct".into()
            )]
        );
    }
    // Deliberately delay applying the notice at the relay. The old direct UDP
    // path still delivers while the server already considers this target relayed.
    handle_user_message(
        &fixture.outgoing(&fixture.relay, "relay", false)?,
        "0002b6ff",
    )?;
    fixture.dispatch_server_queue().await?;
    let direct = drain(&fixture.direct)?;
    assert_eq!(direct.len(), 1, "old direct and new server route overlap");
    assert_eq!(direct[0].1, "DATA relay 0002b6ff");
    Ok(())
}

#[tokio::test]
async fn relay_originated_and_forwarded_payloads_use_the_same_partition() -> io::Result<()> {
    for forwarded in [false, true] {
        let fixture = Fixture::new()?;
        fixture.peers.lock().expect("peers").push(peer(
            "server-peer",
            fixture.server_peer.local_addr()?,
            true,
        ));
        let packet = if forwarded {
            // Sender is already listed as server-relayed: never echo to it.
            fixture.receive_at_relay("DATA server-peer 00a1", fixture.server_peer.local_addr()?)?;
            "DATA server-peer 00a1"
        } else {
            handle_user_message(&fixture.outgoing(&fixture.relay, "relay", false)?, "00a1")?;
            "DATA relay 00a1"
        };
        fixture.dispatch_server_queue().await?;
        expect_packet(&fixture.direct, fixture.relay.local_addr()?, packet)?;
        expect_packet(&fixture.server_other, fixture.server.local_addr()?, packet)?;
        if forwarded {
            assert!(drain(&fixture.server_peer)?.is_empty(), "sender echo");
        } else {
            expect_packet(&fixture.server_peer, fixture.server.local_addr()?, packet)?;
        }
        assert!(drain(&fixture.relay)?.is_empty(), "relay echo");
        assert!(drain(&fixture.server)?.is_empty(), "forwarding loop");
    }
    Ok(())
}

#[tokio::test]
async fn in_flight_mirror_survives_fallback_and_repeated_notices() -> io::Result<()> {
    for forwarded in [false, true] {
        let fixture = Fixture::new()?;
        let sender = if forwarded { "server-peer" } else { "relay" };
        let first = format!("DATA {sender} 00b1");
        if forwarded {
            fixture.receive_at_relay(&first, fixture.server_peer.local_addr()?)?;
        } else {
            handle_user_message(&fixture.outgoing(&fixture.relay, sender, false)?, "00b1")?;
        }
        let mirrors = drain_raw(&fixture.server)?;
        assert_eq!(mirrors.len(), 1);
        expect_packet(&fixture.direct, fixture.relay.local_addr()?, &first)?;

        let mut expected = fixture.state.lock().await["s"]["c"].clone();
        expected
            .users
            .iter_mut()
            .find(|user| user.name == "direct")
            .expect("target")
            .needs_server_relay = true;
        fixture.request_fallback().await?;
        fixture.apply_fallback()?;
        fixture.request_fallback().await?;
        fixture.apply_fallback()?;
        assert_channel_unchanged(&expected, &fixture.state.lock().await["s"]["c"]);

        // Release the old mirror after the control transition. The exclusion
        // must come from this packet, not the relay's or server's current flags.
        fixture
            .send_to_server(&fixture.relay, &mirrors[0].1)
            .await?;
        assert!(drain(&fixture.direct)?.is_empty(), "in-flight duplicate");
        expect_packet(&fixture.server_other, fixture.server.local_addr()?, &first)?;
        if !forwarded {
            expect_packet(&fixture.server_peer, fixture.server.local_addr()?, &first)?;
        }

        let second = format!("DATA {sender} 00b2");
        if forwarded {
            fixture.receive_at_relay(&second, fixture.server_peer.local_addr()?)?;
        } else {
            handle_user_message(&fixture.outgoing(&fixture.relay, sender, false)?, "00b2")?;
        }
        fixture.dispatch_server_queue().await?;
        expect_packet(&fixture.direct, fixture.server.local_addr()?, &second)?;
        expect_packet(&fixture.server_other, fixture.server.local_addr()?, &second)?;
        if forwarded {
            expect_empty(&fixture.server_peer)?;
        } else {
            expect_packet(&fixture.server_peer, fixture.server.local_addr()?, &second)?;
        }
        expect_empty(&fixture.relay)?;
        expect_empty(&fixture.server)?;
    }
    Ok(())
}

#[tokio::test]
async fn forwarded_payload_with_delayed_notice_keeps_direct_delivery_once() -> io::Result<()> {
    let fixture = Fixture::new()?;
    fixture.request_fallback().await?;
    fixture.receive_at_relay("DATA server-peer 00c1", fixture.server_peer.local_addr()?)?;
    fixture.dispatch_server_queue().await?;
    expect_packet(
        &fixture.direct,
        fixture.relay.local_addr()?,
        "DATA server-peer 00c1",
    )?;
    expect_packet(
        &fixture.server_other,
        fixture.server.local_addr()?,
        "DATA server-peer 00c1",
    )?;
    expect_empty(&fixture.server_peer)?;
    Ok(())
}

#[tokio::test]
async fn server_only_submissions_work_for_every_registered_role() -> io::Result<()> {
    for sender in ["relay", "direct", "server-peer"] {
        let fixture = Fixture::new()?;
        let endpoints = [
            ("relay", &fixture.relay),
            ("direct", &fixture.direct),
            ("server-peer", &fixture.server_peer),
            ("server-other", &fixture.server_other),
        ];
        let endpoint = endpoints
            .iter()
            .find(|(name, _)| *name == sender)
            .expect("sender")
            .1;
        handle_user_message(&fixture.outgoing(endpoint, sender, true)?, "00d1")?;
        fixture.dispatch_server_queue().await?;
        if sender != "relay" {
            assert_eq!(fixture.dispatch_relay_queue()?, 1);
        }
        for (name, endpoint) in endpoints {
            if name == sender || name == "relay" {
                assert!(drain(endpoint)?.is_empty(), "sender/relay echo");
            } else {
                expect_packet(
                    endpoint,
                    fixture.server.local_addr()?,
                    &format!("DATA {sender} 00d1"),
                )?;
            }
        }
        assert!(
            drain(&fixture.server)?.is_empty(),
            "server delivery was mirrored"
        );
    }
    Ok(())
}

#[tokio::test]
async fn no_participant_relay_still_allows_server_distribution() -> io::Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .state
        .lock()
        .await
        .get_mut("s")
        .expect("server")
        .get_mut("c")
        .expect("channel")
        .relay = None;
    *fixture.is_relay.lock().expect("role") = false;
    fixture
        .send_to_server(&fixture.server_peer, "DATA server-peer 00e1")
        .await?;
    assert_eq!(fixture.dispatch_relay_queue()?, 1);
    expect_packet(
        &fixture.direct,
        fixture.server.local_addr()?,
        "DATA server-peer 00e1",
    )?;
    expect_packet(
        &fixture.server_other,
        fixture.server.local_addr()?,
        "DATA server-peer 00e1",
    )?;
    expect_empty(&fixture.server_peer)?;
    expect_empty(&fixture.server)?;
    Ok(())
}

#[tokio::test]
async fn direct_only_delivery_needs_no_server_mirror() -> io::Result<()> {
    let fixture = Fixture::new()?;
    fixture.has_server_peers.store(false, Ordering::Release);
    handle_user_message(&fixture.outgoing(&fixture.relay, "relay", false)?, "00f1")?;
    expect_packet(
        &fixture.direct,
        fixture.relay.local_addr()?,
        "DATA relay 00f1",
    )?;
    fixture.receive_at_relay("DATA direct 00f2", fixture.direct.local_addr()?)?;
    assert!(drain(&fixture.direct)?.is_empty(), "direct sender echo");
    expect_empty(&fixture.server)?;
    expect_empty(&fixture.server_peer)?;
    expect_empty(&fixture.server_other)?;
    Ok(())
}

#[tokio::test]
async fn ordinary_sender_uses_one_upstream_and_never_forwards_received_data() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let mut outgoing = fixture.outgoing(&fixture.direct, "direct", false)?;
    outgoing.is_relay = Arc::new(Mutex::new(false));
    outgoing.peers = Arc::new(Mutex::new(vec![
        peer("old-relay", fixture.server_other.local_addr()?, false),
        peer("relay", fixture.relay.local_addr()?, false),
    ]));
    handle_user_message(&outgoing, "0101")?;
    assert_eq!(fixture.dispatch_relay_queue()?, 1);
    fixture.dispatch_server_queue().await?;
    expect_packet(
        &fixture.server_peer,
        fixture.server.local_addr()?,
        "DATA direct 0101",
    )?;
    expect_packet(
        &fixture.server_other,
        fixture.server.local_addr()?,
        "DATA direct 0101",
    )?;
    expect_empty(&fixture.direct)?;

    *fixture.is_relay.lock().expect("role") = false;
    fixture.receive_at_relay("DATA server-peer 0102", fixture.server_peer.local_addr()?)?;
    expect_empty(&fixture.direct)?;
    expect_empty(&fixture.server)?;
    Ok(())
}

#[tokio::test]
async fn duplicate_peer_entries_select_one_destination_and_one_exclusion() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let old_endpoint = socket()?;
    {
        let mut peers = fixture.peers.lock().expect("peers");
        peers.insert(0, peer("direct", old_endpoint.local_addr()?, false));
        let duplicate = peers.last().expect("direct peer").clone();
        peers.push(duplicate);
    }
    fixture.request_fallback().await?; // Deliberately do not apply the notice.
    handle_user_message(&fixture.outgoing(&fixture.relay, "relay", false)?, "0111")?;
    fixture.dispatch_server_queue().await?;
    expect_packet(
        &fixture.direct,
        fixture.relay.local_addr()?,
        "DATA relay 0111",
    )?;
    expect_empty(&old_endpoint)?;
    fixture.apply_fallback()?;
    assert!(
        fixture
            .peers
            .lock()
            .expect("peers")
            .iter()
            .all(|peer| peer.use_server_relay)
    );
    Ok(())
}

#[tokio::test]
async fn invalid_or_wrong_role_mirrors_have_no_forwarding_or_state_effect() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let outsider = socket()?;
    let before = fixture.state.lock().await["s"]["c"].clone();
    for packet in [
        "RELAY_DATA relay",
        "RELAY_DATA relay x 0121",
        "RELAY_DATA relay -1 0121",
        "RELAY_DATA relay +1 direct 0121",
        "RELAY_DATA relay 2 direct",
        "RELAY_DATA relay 1 direct ",
        "RELAY_DATA relay 999999999999999999999999 0121",
        "RELAY_DATA relay 1000 direct 0121",
        "RELAY_DATA relay 0 0121\nMODE RELAY",
        "RELAY_DATA absent 0 0121",
        "DATA absent 0121",
    ] {
        fixture.send_to_server(&fixture.relay, packet).await?;
    }
    for source in [&fixture.direct, &fixture.server_peer, &outsider] {
        fixture
            .send_to_server(source, "RELAY_DATA relay 0 0121")
            .await?;
        fixture.send_to_server(source, "DATA relay 0121").await?;
    }
    assert_channel_unchanged(&before, &fixture.state.lock().await["s"]["c"]);
    for endpoint in [
        &fixture.relay,
        &fixture.direct,
        &fixture.server_peer,
        &fixture.server_other,
    ] {
        expect_empty(endpoint)?;
    }
    // A former relay's source address grants no mirror authority.
    fixture
        .state
        .lock()
        .await
        .get_mut("s")
        .expect("server")
        .get_mut("c")
        .expect("channel")
        .relay = Some("direct".into());
    fixture
        .send_to_server(&fixture.relay, "RELAY_DATA relay 0 0122")
        .await?;
    expect_empty(&fixture.server_peer)?;
    expect_empty(&fixture.server_other)?;
    Ok(())
}

#[tokio::test]
async fn server_mirror_is_channel_scoped_and_ignores_retired_exclusions() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let foreign = socket()?;
    let mut channel = Channel {
        channel_id: 9,
        relay: Some("relay".into()),
        ..Channel::default()
    };
    channel.users = vec![User::new(
        "relay",
        foreign.local_addr()?,
        ServerNatKind::Symmetric,
        1,
    )];
    fixture
        .state
        .lock()
        .await
        .insert("foreign".into(), HashMap::from([("c".into(), channel)]));
    // The server owns its current fallback set; stale/repeated exclusions do
    // not add destinations or require inventing a complete roster at the relay.
    fixture
        .send_to_server(&fixture.relay, "RELAY_DATA relay 2 retired retired 0131")
        .await?;
    expect_packet(
        &fixture.server_peer,
        fixture.server.local_addr()?,
        "DATA relay 0131",
    )?;
    expect_packet(
        &fixture.server_other,
        fixture.server.local_addr()?,
        "DATA relay 0131",
    )?;
    expect_empty(&fixture.direct)?;
    expect_empty(&fixture.relay)?;
    assert!(drain(&foreign)?.is_empty(), "cross-channel delivery");
    Ok(())
}

#[tokio::test]
async fn oversize_or_multiline_carriers_never_partially_forward() -> io::Result<()> {
    use crate::proto::routed_data::MAX_ROUTED_DATA_BYTES;

    let fixture = Fixture::new()?;
    // Plain DATA fits exactly; adding routing metadata would exceed the bound.
    let payload = "a".repeat(MAX_ROUTED_DATA_BYTES - "DATA relay ".len() - 1);
    let error = handle_user_message(&fixture.outgoing(&fixture.relay, "relay", false)?, &payload)
        .expect_err("oversized mirror");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let oversized_data = format!("DATA direct {}", "a".repeat(MAX_ROUTED_DATA_BYTES));
    fixture.receive_at_relay(&oversized_data, fixture.direct.local_addr()?)?;
    let boundary_with_lf = format!(
        "DATA direct {}\n",
        "a".repeat(MAX_ROUTED_DATA_BYTES - "DATA direct ".len())
    );
    fixture.receive_at_relay(&boundary_with_lf, fixture.direct.local_addr()?)?;
    let valid_data_oversized_mirror = format!(
        "DATA direct {}",
        "a".repeat(MAX_ROUTED_DATA_BYTES - "DATA direct ".len())
    );
    fixture.receive_at_relay(&valid_data_oversized_mirror, fixture.direct.local_addr()?)?;
    fixture.receive_at_relay(
        "DATA direct 0141\nMODE SERVER_RELAY direct",
        fixture.direct.local_addr()?,
    )?;
    assert!(!fixture.peers.lock().expect("peers")[0].use_server_relay);
    for raw in [
        oversized_data,
        format!("RELAY_DATA relay 0 {}", "a".repeat(MAX_ROUTED_DATA_BYTES)),
    ] {
        fixture.send_to_server(&fixture.relay, &raw).await?;
    }
    for endpoint in [
        &fixture.server,
        &fixture.relay,
        &fixture.direct,
        &fixture.server_peer,
        &fixture.server_other,
    ] {
        assert!(
            drain(endpoint)?.is_empty(),
            "partial or unqualified fallback send"
        );
    }
    Ok(())
}

#[tokio::test]
async fn failed_direct_attempt_is_not_retried_through_the_server() -> io::Result<()> {
    for forwarded in [false, true] {
        let fixture = Fixture::new()?;
        fixture.request_fallback().await?;
        // An IPv4-only socket cannot send to this IPv6 destination. Failure is
        // logged, but must neither abort processing nor remove the exclusion.
        fixture.peers.lock().expect("peers")[0].addr = SocketAddr::new(
            std::net::Ipv6Addr::LOCALHOST.into(),
            fixture.direct.local_addr()?.port(),
        );
        let sender = if forwarded { "server-peer" } else { "relay" };
        if forwarded {
            fixture.receive_at_relay("DATA server-peer 0151", fixture.server_peer.local_addr()?)?;
        } else {
            handle_user_message(&fixture.outgoing(&fixture.relay, sender, false)?, "0151")?;
        }
        fixture.dispatch_server_queue().await?;
        assert!(
            drain(&fixture.direct)?.is_empty(),
            "unexpected second routing attempt"
        );
        expect_packet(
            &fixture.server_other,
            fixture.server.local_addr()?,
            &format!("DATA {sender} 0151"),
        )?;
        if !forwarded {
            expect_packet(
                &fixture.server_peer,
                fixture.server.local_addr()?,
                "DATA relay 0151",
            )?;
        }
        fixture.apply_fallback()?;
        handle_user_message(&fixture.outgoing(&fixture.relay, "relay", false)?, "0152")?;
        fixture.dispatch_server_queue().await?;
        expect_packet(
            &fixture.direct,
            fixture.server.local_addr()?,
            "DATA relay 0152",
        )?;
    }
    Ok(())
}
