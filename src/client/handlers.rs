use crate::{
    client::routing::send_data,
    client::structures::PeerInfo,
    proto::control_text::{
        MSG_DATA, MSG_DIRECT, MSG_HOLE_PUNCH, MSG_MODE, MSG_PING, MSG_PONG, MSG_RELAY,
        MSG_SERVER_RELAY, MSG_USER_LEFT, MSG_WELCOME,
    },
    proto::routed_data::Data,
};
use std::{
    net::{SocketAddr, UdpSocket},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};

pub fn try_handle_welcome(s: &str, channel_id: &Arc<AtomicU64>, my_peer_id: &Arc<AtomicU32>) {
    for line in s.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 3
            && parts[0] == MSG_WELCOME
            && let (Ok(cid), Ok(pid)) = (parts[1].parse::<u64>(), parts[2].parse::<u32>())
        {
            channel_id.store(cid, Ordering::Release);
            my_peer_id.store(pid, Ordering::Release);
            println!("{MSG_WELCOME} received: channel_id={cid}, my_peer_id={pid}");
        }
    }
}

fn handle_mode_relay(is_relay: &Arc<Mutex<bool>>) -> std::io::Result<()> {
    let mut r = is_relay.lock().map_err(|_| {
        std::io::Error::other("Cannot process message: relay state mutex is poisoned")
    })?;
    if !*r {
        *r = true;
        println!("You are in RELAY MODE");
    }
    Ok(())
}

fn handle_mode_direct(
    parts: &[&str],
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    me: &str,
) -> std::io::Result<()> {
    let username = parts[2];
    let addr_str = parts[3];
    if let Ok(addr) = SocketAddr::from_str(addr_str) {
        let mut guard = peers.lock().map_err(|_| {
            std::io::Error::other("Cannot process message: peers mutex is poisoned")
        })?;
        if username == me {
            println!("Server confirms you ({me}) are relay for {addr_str}");
        } else if !guard.iter().any(|p| p.addr == addr) {
            guard.push(PeerInfo {
                addr,
                last_direct_activity: None,
                username: username.to_string(),
                connected: false,
                created_at: Instant::now(),
                use_server_relay: false,
                relay_requested: false,
            });
            println!("Added peer {username} with addr {addr_str}");
        }
    }
    Ok(())
}

fn handle_user_left(parts: &[&str], peers: &Arc<Mutex<Vec<PeerInfo>>>) -> std::io::Result<()> {
    let username = parts[1];
    let mut guard = peers
        .lock()
        .map_err(|_| std::io::Error::other("Cannot process message: peers mutex is poisoned"))?;
    guard.retain(|p| p.username != username);
    println!("[CLIENT:user] {username} left, removed from list");
    Ok(())
}

/// Handles a mode-related control message.
///
/// # Errors
///
/// Returns an error if a required peer or relay-state mutex is poisoned.
pub fn handle_mode_line(
    line: &str,
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    me: &str,
    is_relay: &Arc<Mutex<bool>>,
    channel_has_server_relays: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(());
    }

    match parts[0] {
        MSG_MODE if parts.len() >= 2 => match parts[1] {
            MSG_RELAY => handle_mode_relay(is_relay)?,

            MSG_SERVER_RELAY => {
                if parts.len() >= 3 {
                    let username = parts[2];

                    // if the server is relaying for 'me', i must send to server
                    if username == me {
                        return Ok(());
                    }
                    let relay = *is_relay.lock().map_err(|_| {
                        std::io::Error::other(
                            "Cannot process message: relay state mutex is poisoned",
                        )
                    })?;
                    let mut guard = peers.lock().map_err(|_| {
                        std::io::Error::other("Cannot process message: peers mutex is poisoned")
                    })?;
                    // if i am the RELAY, note that the channel has server-relayed peers
                    if relay {
                        if !channel_has_server_relays.load(Ordering::Acquire) {
                            channel_has_server_relays.store(true, Ordering::Release);
                            println!("Relay: channel has server-relayed peers.");
                        }
                    } else {
                        println!("Server will relay for user: {username}");
                    }

                    //mark that peer as server-relayed to stop punching it
                    for peer in guard.iter_mut().filter(|p| p.username == username) {
                        peer.use_server_relay = true;
                        peer.connected = false; // server routing is not direct connectivity
                        peer.relay_requested = true;
                    }
                } else {
                    // just a lone user :(
                    println!("Server is currently being relay for the lone user.");
                }
            }

            MSG_DIRECT if parts.len() >= 4 => handle_mode_direct(&parts, peers, me)?,
            _ => {}
        },
        MSG_USER_LEFT if parts.len() >= 2 => handle_user_left(&parts, peers)?,
        _ => {}
    }
    Ok(())
}

pub fn handle_ping(socket: &UdpSocket, src: std::net::SocketAddr) {
    if let Err(error) = socket.send_to(MSG_PONG.as_bytes(), src) {
        eprintln!("Failed path response to {src}: {error}");
    }
}

fn record_direct_activity(
    peers: &Arc<Mutex<Vec<PeerInfo>>>,
    src: SocketAddr,
    now: Instant,
) -> std::io::Result<()> {
    let mut guard = peers
        .lock()
        .map_err(|_| std::io::Error::other("Cannot process message: peers mutex is poisoned"))?;
    if let Some(peer) = guard.iter_mut().find(|peer| peer.addr == src)
        && !peer.use_server_relay
        && !peer.relay_requested
    {
        peer.last_direct_activity = Some(now);
        peer.connected = true;
    }
    Ok(())
}

/// Handles a data message received from a peer or the signaling server.
///
/// # Errors
///
/// Returns an error if a required peer or relay-state mutex is poisoned.
pub fn handle_data_message(
    context: &IncomingMessageContext<'_>,
    line: &str,
    src: SocketAddr,
) -> std::io::Result<()> {
    let Some(data) = Data::parse(line) else {
        return Ok(());
    };
    println!("[{}]: {}", data.sender, data.payload);

    // The server already owns distribution of this packet. Its copy is final,
    // even when this endpoint also has the participant-relay role.
    if src == context.server_addr {
        return Ok(());
    }
    let relay = *context.is_relay.lock().map_err(|_| {
        std::io::Error::other("Cannot process message: relay state mutex is poisoned")
    })?;
    if !relay {
        return Ok(());
    }
    let mirror_to = context
        .channel_has_server_relays
        .load(Ordering::Acquire)
        .then_some(context.signaling_addr);
    match send_data(context.socket, context.peers, line, true, mirror_to) {
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            eprintln!("Cannot forward DATA: {error}");
            Ok(())
        }
        result => result,
    }
}

pub struct IncomingMessageContext<'a> {
    pub socket: &'a UdpSocket,
    pub peers: &'a Arc<Mutex<Vec<PeerInfo>>>,
    pub user: &'a str,
    pub is_relay: &'a Arc<Mutex<bool>>,
    pub channel_has_server_relays: &'a Arc<AtomicBool>,
    pub signaling_addr: &'a str,
    pub server_addr: SocketAddr,
}

/// Processes incoming lines, stopping at the first handler error.
///
/// # Errors
///
/// Returns an error if a handler encounters a poisoned peer or relay-state mutex.
pub fn process_incoming_message(
    context: &IncomingMessageContext<'_>,
    message: &str,
    src: SocketAddr,
) -> std::io::Result<()> {
    process_incoming_message_at(context, message, src, Instant::now())
}

pub(crate) fn process_incoming_message_at(
    context: &IncomingMessageContext<'_>,
    message: &str,
    src: SocketAddr,
    now: Instant,
) -> std::io::Result<()> {
    // DATA occupies one complete datagram. Validate size/newlines before the
    // multi-line MODE parser can trim a truncated carrier or interpret payload.
    if message.split_whitespace().next() == Some(MSG_DATA) {
        if src != context.server_addr && Data::parse(message).is_some() {
            record_direct_activity(context.peers, src, now)?;
        }
        return handle_data_message(context, message, src);
    }
    for line in message.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if src != context.server_addr && matches!(line, MSG_PING | MSG_PONG | MSG_HOLE_PUNCH) {
            record_direct_activity(context.peers, src, now)?;
        }
        match line {
            MSG_PING => handle_ping(context.socket, src),
            MSG_PONG | MSG_HOLE_PUNCH => {}
            _ if src == context.server_addr => {
                handle_mode_line(
                    line,
                    context.peers,
                    context.user,
                    context.is_relay,
                    context.channel_has_server_relays,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}
