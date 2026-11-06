#![allow(clippy::disallowed_methods)] // Explicit assertions in bounded localhost tests.

use std::{collections::HashMap, io, net::Ipv4Addr, sync::Arc, time::Duration};

use tokio::{net::UdpSocket, sync::Mutex, time::timeout};

use super::{
    handlers::handle_message,
    structures::{Channel, NatKind, ServerMap, User},
};

struct Fixture {
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
    relay: UdpSocket,
    peer: UdpSocket,
    other: UdpSocket,
}

impl Fixture {
    async fn new() -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let other = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let mut users = Vec::new();
        for (name, source) in [("relay", &relay), ("peer", &peer), ("other", &other)] {
            let peer_id = u32::try_from(users.len() + 1).expect("three fixture users");
            users.push(User::new(
                name,
                source.local_addr()?,
                NatKind::Cone,
                peer_id,
            ));
        }
        let channel = Channel {
            channel_id: 7,
            next_peer_id: 4,
            users,
            relay: Some("relay".into()),
        };
        Ok(Self {
            socket,
            state: Arc::new(Mutex::new(HashMap::from([(
                "s".into(),
                HashMap::from([("c".into(), channel)]),
            )]))),
            relay,
            peer,
            other,
        })
    }

    async fn command(&self, source: &UdpSocket, message: &str) -> io::Result<()> {
        timeout(Duration::from_secs(2), async {
            source
                .send_to(message.as_bytes(), self.socket.local_addr()?)
                .await?;
            let mut buffer = [0; 256];
            let (len, address) = self.socket.recv_from(&mut buffer).await?;
            let received = std::str::from_utf8(&buffer[..len]).map_err(io::Error::other)?;
            assert_eq!(received, message);
            handle_message(
                received.into(),
                address,
                Arc::clone(&self.socket),
                Arc::clone(&self.state),
            )
            .await;
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn receive(&self, recipient: &UdpSocket) -> io::Result<String> {
        let mut buffer = [0; 256];
        let (len, source) = timeout(Duration::from_secs(2), recipient.recv_from(&mut buffer))
            .await
            .map_err(io::Error::other)??;
        assert_eq!(source, self.socket.local_addr()?);
        String::from_utf8(buffer[..len].to_vec()).map_err(io::Error::other)
    }

    async fn assert_next_message_is_probe_response(&self, recipient: &UdpSocket) -> io::Result<()> {
        // Checks only the next datagram, not later notifications.
        self.command(recipient, "NAT_PROBE 1").await?;
        assert_eq!(
            self.receive(recipient).await?,
            format!("NAT_SEEN {}\n", recipient.local_addr()?)
        );
        Ok(())
    }

    async fn channel(&self) -> Channel {
        self.state.lock().await["s"]["c"].clone()
    }
}

fn same_membership(before: &Channel, after: &Channel) {
    assert_eq!(after.channel_id, before.channel_id);
    assert_eq!(after.next_peer_id, before.next_peer_id);
    assert_eq!(after.relay, before.relay);
    assert_eq!(after.users.len(), before.users.len());
    for (previous, current) in before.users.iter().zip(&after.users) {
        assert_eq!(current.peer_id, previous.peer_id);
        assert_eq!(current.name, previous.name);
        assert_eq!(current.addr, previous.addr);
        assert_eq!(current.nat_kind, previous.nat_kind);
        assert_eq!(current.last_pong, previous.last_pong);
    }
}

#[tokio::test]
async fn path_reports_preserve_healthy_membership_and_request_fallback() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.command(&fixture.peer, "HB s c peer").await?;
    let before = fixture.channel().await;
    for command in ["PEER_TIMEOUT", "REQUEST_RELAY", "PEER_TIMEOUT"] {
        fixture
            .command(&fixture.relay, &format!("{command} s c peer"))
            .await?;
        let after = fixture.channel().await;
        same_membership(&before, &after);
        assert!(after.users[1].needs_server_relay);
        assert!(!after.users[0].needs_server_relay);
        assert!(!after.users[2].needs_server_relay);
        for recipient in [&fixture.relay, &fixture.peer, &fixture.other] {
            assert_eq!(
                fixture.receive(recipient).await?,
                "MODE SERVER_RELAY peer\n"
            );
            fixture
                .assert_next_message_is_probe_response(recipient)
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_path_reports_have_no_membership_route_or_notification_effects() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let outsider = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let before = fixture.channel().await;
    for command in ["PEER_TIMEOUT", "REQUEST_RELAY"] {
        for (source, target) in [
            (&outsider, "s c peer"),
            (&fixture.peer, "s c other"),
            (&fixture.relay, "s c absent"),
            (&fixture.relay, "s c relay"),
            (&fixture.relay, "wrong c peer"),
            (&fixture.relay, "s wrong peer"),
        ] {
            fixture
                .command(source, &format!("{command} {target}"))
                .await?;
            let after = fixture.channel().await;
            same_membership(&before, &after);
            assert!(after.users.iter().all(|user| !user.needs_server_relay));
            for recipient in [&fixture.relay, &fixture.peer, &fixture.other, &outsider] {
                fixture
                    .assert_next_message_is_probe_response(recipient)
                    .await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn former_relay_cannot_request_fallback_for_another_participant() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .state
        .lock()
        .await
        .get_mut("s")
        .expect("server")
        .get_mut("c")
        .expect("channel")
        .relay = Some("other".into());
    let before = fixture.channel().await;
    for command in ["PEER_TIMEOUT", "REQUEST_RELAY"] {
        fixture
            .command(&fixture.relay, &format!("{command} s c peer"))
            .await?;
        let after = fixture.channel().await;
        same_membership(&before, &after);
        assert!(after.users.iter().all(|user| !user.needs_server_relay));
        for recipient in [&fixture.relay, &fixture.peer, &fixture.other] {
            fixture
                .assert_next_message_is_probe_response(recipient)
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn missing_or_unregistered_selected_relay_cannot_authorize_reports() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    for relay in [None, Some("removed-relay".into())] {
        fixture
            .state
            .lock()
            .await
            .get_mut("s")
            .expect("server")
            .get_mut("c")
            .expect("channel")
            .relay = relay;
        let before = fixture.channel().await;
        for command in ["PEER_TIMEOUT", "REQUEST_RELAY"] {
            fixture
                .command(&fixture.relay, &format!("{command} s c peer"))
                .await?;
            let after = fixture.channel().await;
            same_membership(&before, &after);
            assert!(after.users.iter().all(|user| !user.needs_server_relay));
            for recipient in [&fixture.relay, &fixture.peer, &fixture.other] {
                fixture
                    .assert_next_message_is_probe_response(recipient)
                    .await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn unmatched_disconnect_has_no_departure_or_route_effects() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let before = fixture.channel().await;
    for (source, command) in [
        (&fixture.peer, "DISCONNECT s c relay"),
        (&fixture.relay, "DISCONNECT s c absent"),
        (&fixture.peer, "DISCONNECT wrong c peer"),
        (&fixture.peer, "DISCONNECT s wrong peer"),
    ] {
        fixture.command(source, command).await?;
        same_membership(&before, &fixture.channel().await);
        for recipient in [&fixture.relay, &fixture.peer, &fixture.other] {
            fixture
                .assert_next_message_is_probe_response(recipient)
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn matched_disconnect_removes_once_and_late_reports_cannot_restore_it() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .command(&fixture.peer, "DISCONNECT s c peer")
        .await?;
    let departed = fixture.channel().await;
    assert_eq!(departed.users.len(), 2);
    assert!(departed.users.iter().all(|user| user.name != "peer"));
    let expected = format!("USER_LEFT peer {}\n", fixture.peer.local_addr()?);
    for recipient in [&fixture.relay, &fixture.other] {
        assert_eq!(fixture.receive(recipient).await?, expected);
    }
    for (source, command) in [
        (&fixture.peer, "DISCONNECT s c peer"),
        (&fixture.relay, "PEER_TIMEOUT s c peer"),
        (&fixture.relay, "REQUEST_RELAY s c peer"),
    ] {
        fixture.command(source, command).await?;
        same_membership(&departed, &fixture.channel().await);
        for recipient in [&fixture.relay, &fixture.peer, &fixture.other] {
            fixture
                .assert_next_message_is_probe_response(recipient)
                .await?;
        }
    }
    Ok(())
}
