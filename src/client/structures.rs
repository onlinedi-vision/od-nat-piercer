use std::net::SocketAddr;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    pub last_pong: Instant,
    pub username: String,
    pub connected: bool,

    pub created_at: Instant,
}
