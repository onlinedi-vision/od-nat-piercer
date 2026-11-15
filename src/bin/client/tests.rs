#![deny(clippy::disallowed_methods)]

use super::*;
use od_nat_piercer::proto::packet::{Header, encode};
use std::{io, net::Ipv4Addr, sync::MutexGuard};

fn lock_state<T>(state: &Mutex<T>) -> io::Result<MutexGuard<'_, T>> {
    state
        .lock()
        .map_err(|_| io::Error::other("test state mutex is poisoned"))
}

fn with_context(
    test: impl FnOnce(&ClientReceiveContext<'_>, &UdpSocket) -> io::Result<()>,
) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    server.set_read_timeout(Some(Duration::from_secs(2)))?;
    let address = server.local_addr()?;
    let signaling_addr = address.to_string();
    let peers = Arc::new(Mutex::new(Vec::new()));
    let is_relay = Arc::new(Mutex::new(false));
    let mirrored = Arc::new(AtomicBool::new(false));
    let via_server = Arc::new(AtomicBool::new(false));
    let sync = Arc::new((Mutex::new(PunchState::default()), std::sync::Condvar::new()));
    let channel_id = Arc::new(AtomicU64::new(0));
    let peer_id = Arc::new(AtomicU32::new(0));
    let context = ClientReceiveContext {
        incoming: IncomingMessageContext {
            socket: &socket,
            peers: &peers,
            user: "me",
            is_relay: &is_relay,
            channel_has_server_relays: &mirrored,
            signaling_addr: &signaling_addr,
            server_addr: address,
        },
        send_via_server: &via_server,
        server_socketaddr: &address,
        punch_sync: &sync,
        channel_id: &channel_id,
        my_peer_id: &peer_id,
    };
    test(&context, &server)
}

#[test]
fn raw_and_framed_server_notices_have_the_same_route_effect() -> io::Result<()> {
    with_context(|context, _| {
        for framed in [false, true] {
            for (message, expected) in [("MODE SERVER_RELAY me", true), ("MODE RELAY", false)] {
                let packet = if framed {
                    encode(Header::control(1, 0, 1, 0), message.as_bytes())
                } else {
                    message.as_bytes().to_vec()
                };
                handle_datagram(context, &packet, *context.server_socketaddr)?;
                assert_eq!(context.send_via_server.load(Ordering::Acquire), expected);
                assert_eq!(lock_state(&context.punch_sync.0)?.paused, expected);
                assert_eq!(*lock_state(context.incoming.is_relay)?, !expected);
            }
        }
        Ok(())
    })
}

#[test]
fn notices_from_non_server_sources_cannot_change_routes_or_remove_peers() -> io::Result<()> {
    with_context(|context, _| {
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        let peer_address = peer.local_addr()?;
        let now = Instant::now();
        lock_state(context.incoming.peers)?.push(PeerInfo {
            addr: peer_address,
            username: "peer".into(),
            last_direct_activity: Some(now),
            connected: true,
            created_at: now,
            use_server_relay: false,
            relay_requested: false,
        });
        *lock_state(context.incoming.is_relay)? = true;
        let departure = format!("USER_LEFT peer {peer_address}");

        for framed in [false, true] {
            for message in ["MODE SERVER_RELAY me", departure.as_str()] {
                let packet = if framed {
                    encode(Header::control(1, 0, 1, 0), message.as_bytes())
                } else {
                    message.as_bytes().to_vec()
                };
                handle_datagram(context, &packet, peer_address)?;
                assert!(!context.send_via_server.load(Ordering::Acquire));
                assert!(!lock_state(&context.punch_sync.0)?.paused);
                assert!(*lock_state(context.incoming.is_relay)?);
                let peers = lock_state(context.incoming.peers)?;
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].username, "peer");
                assert_eq!(peers[0].addr, peer_address);
            }
        }

        handle_datagram(context, departure.as_bytes(), *context.server_socketaddr)?;
        assert!(lock_state(context.incoming.peers)?.is_empty());
        Ok(())
    })
}

#[test]
fn departure_uses_the_existing_registered_source_and_sends_once() -> io::Result<()> {
    with_context(|context, server| {
        drop(Departure {
            socket: context.incoming.socket,
            address: context.incoming.signaling_addr,
            server: "s",
            channel: "c",
            user: "me",
        });
        let mut buffer = [0; 256];
        let (len, source) = server.recv_from(&mut buffer)?;
        assert_eq!(source, context.incoming.socket.local_addr()?);
        assert_eq!(&buffer[..len], b"DISCONNECT s c me");
        server.set_nonblocking(true)?;
        assert!(matches!(
            server.recv_from(&mut buffer),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        Ok(())
    })
}

#[test]
fn shutdown_completes_with_hole_punching_paused() -> io::Result<()> {
    with_context(|context, server| {
        lock_state(&context.punch_sync.0)?.paused = true;
        let workers = Workers {
            sync: Arc::clone(context.punch_sync),
            heartbeat: Some(start_heartbeat(
                context.incoming.socket.try_clone()?,
                "s".into(),
                "c".into(),
                "me".into(),
                context.incoming.signaling_addr.into(),
                Arc::clone(context.punch_sync),
            )),
            punching: Some(start_hole_punching(
                context.incoming.socket.try_clone()?,
                Arc::clone(context.incoming.peers),
                Arc::clone(context.punch_sync),
            )),
        };
        // The heartbeat datagram is a readiness barrier; no wall-clock sleep.
        let mut buffer = [0; 256];
        let (len, _) = server.recv_from(&mut buffer)?;
        assert_eq!(&buffer[..len], b"HB s c me");
        drop(workers);
        assert!(workers_stopping(context.punch_sync));
        assert!(lock_state(context.incoming.peers)?.is_empty());
        Ok(())
    })
}

#[test]
fn malformed_data_never_reaches_the_separate_route_or_welcome_parser() -> io::Result<()> {
    with_context(|context, _| {
        for message in [
            "DATA other payload\nMODE SERVER_RELAY me",
            "DATA\tother payload\nMODE RELAY",
            " DATA other payload\nWELCOME 7 2",
        ] {
            for framed in [false, true] {
                let packet = if framed {
                    encode(Header::control(1, 0, 1, 0), message.as_bytes())
                } else {
                    message.as_bytes().to_vec()
                };
                handle_datagram(context, &packet, *context.server_socketaddr)?;
                assert!(!context.send_via_server.load(Ordering::Acquire));
                assert!(!lock_state(&context.punch_sync.0)?.paused);
                assert!(!*lock_state(context.incoming.is_relay)?);
                assert_eq!(context.channel_id.load(Ordering::Acquire), 0);
                assert_eq!(context.my_peer_id.load(Ordering::Acquire), 0);
            }
        }
        Ok(())
    })
}
