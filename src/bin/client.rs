use std::{env, thread, time::Duration};

use od_nat_piercer::{Piercer, PiercerConfig};

fn parse_arguments(args: Vec<String>) -> (String, String, String, String, u16) {
    if args.len() < 6 {
        eprintln!("Usage: client <signaling_ip> <server_id> <channel> <user> <local_port>");
        std::process::exit(1);
    }

    let signaling_ip = args[1].clone();
    let server_id = args[2].clone();
    let channel = args[3].clone();
    let user = args[4].clone();
    let local_port: u16 = args[5].parse().expect("Invalid port number");

    (signaling_ip, server_id, channel, user, local_port)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let (signaling_ip, server_id, channel, user, local_port) = parse_arguments(args);

    let cfg = PiercerConfig {
        signaling_ip,
        server_id,
        channel,
        username: user,
        local_port,
    };

    let _piercer = Piercer::connect(cfg);

    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
