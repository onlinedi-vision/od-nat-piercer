use std::{collections::HashMap, net::SocketAddr, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatKind {
    Unknown,
    Public,
    Cone,
    Symmetric,
}

#[derive(Clone, Debug)]
pub struct User {
    pub peer_id: u32,
    pub name: String,
    pub addr: SocketAddr,
    pub last_pong: Instant,
    pub needs_server_relay: bool,
    pub nat_kind: NatKind,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub channel_id: u32,
    pub next_peer_id: u32,
    pub users: Vec<User>,
    pub relay: Option<String>,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            channel_id: 0,
            next_peer_id: 1,
            users: Vec::new(),
            relay: None,
        }
    }
}

pub type ServerMap = HashMap<String, HashMap<String, Channel>>;
