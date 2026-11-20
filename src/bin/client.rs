use od_nat_piercer::recovery::{
    runtime::run_client,
    wire::{Attempt, Join},
};
use std::{env, io, net::ToSocketAddrs};

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().collect();
    if args.len() != 6 {
        return Err(io::Error::other(
            "Usage: client <signaling_host> <server_id> <channel> <user> <local_port>",
        ));
    }
    let server = (args[1].as_str(), 2131)
        .to_socket_addrs()?
        .find(std::net::SocketAddr::is_ipv4)
        .ok_or_else(|| io::Error::other("no IPv4 signaling address"))?;
    let port = args[5]
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let join = Join {
        server: args[2].clone(),
        channel: args[3].clone(),
        user: args[4].clone(),
        attempt: Attempt::fresh()?,
        symmetric: false,
    };
    run_client(join, server, port)
}
