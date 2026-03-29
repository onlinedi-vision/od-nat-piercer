use std::net::SocketAddr;

use rand::{RngCore, rngs::OsRng};

//helper to avoid moving Vec in pattern
pub fn cleanup_and_notify_iter<I>(it: I) -> Vec<(Vec<SocketAddr>, Vec<u8>)>
where
    I: IntoIterator<Item = (Vec<SocketAddr>, Vec<u8>)>,
{
    it.into_iter().collect()
}

pub fn generate_channel_id() -> u64 {
    OsRng.next_u64()
}
