use super::structures::PeerInfo;
use crate::proto::routed_data::{Data, RelayData};
use std::{
    io,
    net::{SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
};

fn direct_targets(
    peers: &Mutex<Vec<PeerInfo>>,
    sender: &str,
    relay: bool,
) -> io::Result<Vec<(String, SocketAddr)>> {
    let peers = peers
        .lock()
        .map_err(|_| io::Error::other("Cannot route data: peers mutex is poisoned"))?;
    let mut targets: Vec<(String, SocketAddr)> = Vec::new();
    for peer in peers.iter().rev() {
        if peer.username == sender
            || peer.use_server_relay
            || targets.iter().any(|(name, _)| name == &peer.username)
        {
            continue;
        }
        if peer.relay_requested {
            if !relay {
                break; // Do not revive an older upstream after this one failed.
            }
            continue;
        }
        targets.push((peer.username.clone(), peer.addr));
        if !relay {
            break; // An ordinary sender submits to one distribution owner.
        }
    }
    Ok(targets)
}

// Shared by relay-originated and relay-forwarded DATA. Only this immutable
// snapshot determines both direct attempts and the server mirror's exclusions.
pub(crate) fn send_data(
    socket: &UdpSocket,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    raw: &str,
    relay: bool,
    mirror_to: Option<&str>,
) -> io::Result<()> {
    let data = Data::parse(raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid routed DATA"))?;
    let targets = direct_targets(peers, data.sender, relay)?;
    // Preflight the complete mirror before any send. An invalid/oversized
    // envelope must never be replaced with an unqualified DATA broadcast.
    let mirror = if mirror_to.is_some() {
        Some(
            RelayData {
                data,
                direct: targets.iter().map(|(name, _)| name.as_str()).collect(),
            }
            .encode()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "RELAY_DATA exceeds carrier bounds",
                )
            })?,
        )
    } else {
        None
    };

    for (name, addr) in &targets {
        if let Err(error) = socket.send_to(raw.as_bytes(), addr) {
            eprintln!("Failed direct DATA attempt to {name} at {addr}: {error}");
        }
    }
    // Failed direct attempts are still excluded. No retry or second route is
    // introduced for the same packet, and no locks are held during sends.
    if let (Some(addr), Some(packet)) = (mirror_to, mirror)
        && let Err(error) = socket.send_to(packet.as_bytes(), addr)
    {
        eprintln!("Failed RELAY_DATA mirror to {addr}: {error}");
    }
    Ok(())
}
