use std::net::SocketAddr;

//helper to avoid moving Vec in pattern
pub fn to_cleanup_and_notify_iter<I>(it: I) -> Vec<(Vec<SocketAddr>, Vec<u8>)>
where
    I: IntoIterator<Item = (Vec<SocketAddr>, Vec<u8>)>,
{
    it.into_iter().collect()
}
