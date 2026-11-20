use super::*;
use crate::signaling::{
    handle_message,
    structures::{Channel, User},
};
use std::{collections::HashMap, io, net::Ipv4Addr, time::Duration};

async fn receive(socket: &UdpSocket) -> io::Result<Vec<u8>> {
    let mut buffer = [0; 512];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .map_err(io::Error::other)??;
    Ok(buffer[..len].to_vec())
}

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn repeat_connect_replays_current_route_without_reselecting_or_notifying_others()
-> io::Result<()> {
    for (selected_relay, fallback, reconnecting) in [
        (Some("relay"), false, 1),
        (Some("relay"), true, 1),
        (Some("relay"), true, 0),
        (Some("relay"), false, 0),
        (None, true, 1),
    ] {
        let server = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let clients = [
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?,
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?,
        ];
        let mut channel = Channel {
            channel_id: 7,
            next_peer_id: 3,
            users: vec![
                User::new("relay", clients[0].local_addr()?, NatKind::Cone, 1),
                User::new("peer", clients[1].local_addr()?, NatKind::Cone, 2),
            ],
            relay: selected_relay.map(str::to_owned),
        };
        channel.users[1].needs_server_relay = fallback;
        let name = channel.users[reconnecting].name.clone();
        let expected = if reconnecting == 0 {
            vec![
                "MODE RELAY\n".to_owned(),
                if fallback {
                    "MODE SERVER_RELAY peer\n".to_owned()
                } else {
                    format!("MODE DIRECT peer {}\n", clients[1].local_addr()?)
                },
            ]
        } else if fallback {
            vec!["MODE SERVER_RELAY peer\n".to_owned()]
        } else {
            vec![format!("MODE DIRECT relay {}\n", clients[0].local_addr()?)]
        };
        let state = Arc::new(Mutex::new(HashMap::from([(
            "s".into(),
            HashMap::from([("c".into(), channel.clone())]),
        )])));
        // A fresh process has lost local state, but its server registration
        // survives: no DISCONNECT or registration expiry is injected.
        handle_connect_message(
            &["CONNECT", "s", "c", &name, "CONE"],
            clients[reconnecting].local_addr()?,
            Arc::clone(&server),
            Arc::clone(&state),
        )
        .await;
        let welcome = receive(&clients[reconnecting]).await?;
        let (header, _) = packet::decode(&welcome)
            .ok_or_else(|| io::Error::other("repeat CONNECT omitted WELCOME"))?;
        assert_eq!(header.dst_peer_id, channel.users[reconnecting].peer_id);
        assert_eq!(header.channel_id, channel.channel_id);
        for notice in expected {
            assert_eq!(receive(&clients[reconnecting]).await?, notice.as_bytes());
        }
        for client in &clients {
            // A response barrier detects any extra/replayed notifications.
            handle_message(
                "NAT_PROBE 1".into(),
                client.local_addr()?,
                Arc::clone(&server),
                Arc::clone(&state),
            )
            .await;
            assert_eq!(
                receive(client).await?,
                format!("NAT_SEEN {}\n", client.local_addr()?).as_bytes()
            );
        }
        let registry = state.lock().await;
        let after = &registry["s"]["c"];
        assert_eq!(after.relay, channel.relay);
        assert_eq!(after.next_peer_id, channel.next_peer_id);
        assert_eq!(after.users.len(), channel.users.len());
        for (old, current) in channel.users.iter().zip(&after.users) {
            assert_eq!(old.peer_id, current.peer_id);
            assert_eq!(old.addr, current.addr);
            assert_eq!(old.needs_server_relay, current.needs_server_relay);
            assert_eq!(old.nat_kind, current.nat_kind);
        }
    }
    Ok(())
}
