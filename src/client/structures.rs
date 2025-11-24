use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatKind {
    Unknown,
    Public,
    Cone,
    Symmetric,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    pub last_pong: Instant,
    pub username: String,
    pub connected: bool,
    pub created_at: Instant,
    pub use_server_relay: bool, //server will carry traffic for this peer
    pub relay_requested: bool,  //we asked server once
    pub nat_kind: NatKind,
}

#[derive(Debug)]
pub struct PunchState {
    pub paused: bool,
}

pub type PunchSync = Arc<(Mutex<PunchState>, Condvar)>;

#[derive(Debug)]
pub struct RelayState {
    pub is_active: bool, // = is_relay || channel_has_server_relays
}

pub type RelaySync = Arc<(Mutex<RelayState>, Condvar)>;
