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
    pub name: String,
    pub addr: SocketAddr,
    pub last_pong: Instant,
    pub needs_server_relay: bool,
    pub nat_kind: NatKind,
}

#[derive(Clone, Debug, Default)]
pub struct Channel {
    pub users: Vec<User>,
    pub relay: Option<String>,
}

pub type ServerMap = HashMap<String, HashMap<String, Channel>>;
