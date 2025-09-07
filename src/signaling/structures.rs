use std::{collections::HashMap, net::SocketAddr, time::Instant};

#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
    pub addr: SocketAddr,
    pub last_pong: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct Channel {
    pub users: Vec<User>,
    pub relay: Option<String>,
}

pub type ServerMap = HashMap<String, HashMap<String, Channel>>;
