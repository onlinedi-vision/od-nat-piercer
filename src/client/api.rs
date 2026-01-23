use crate::client::{networking, setup_socket};

use std::{
    net::ToSocketAddrs,
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
};

use crate::client::{
    networking::*,
    structures::{NatKind, PeerInfo, PunchState, PunchSync, RelayState, RelaySync},
};

#[derive(Clone, Debug)]
pub struct PiercerConfig {
    pub signaling_ip: String,
    pub server_id: String,
    pub channel: String,
    pub username: String,
    pub local_port: u16,
}

pub struct Piercer {}

impl Piercer {
    pub fn connect(cfg: PiercerConfig) -> std::io::Result<Self> {
        let socket = setup_socket(cfg.local_port);

        let my_nat = networking::detect_nat_kind(&socket, &cfg.signaling_ip);

        let signaling_addr = format!("{}:2131", cfg.signaling_ip);
        let server_socketaddr: std::net::SocketAddr = signaling_addr
            .to_socket_addrs()
            .expect("resolve signaling server")
            .next()
            .expect("no addr for signaling server");

        crate::client::send_connect_message(
            &socket,
            &signaling_addr,
            &cfg.server_id,
            &cfg.channel,
            &cfg.username,
            my_nat,
        );

        let peers: Vec<PeerInfo> = Vec::new();
        let peers = Arc::new(Mutex::new(peers));

        let is_relay = Arc::new(Mutex::new(false));
        let relay_started = Arc::new(Mutex::new(false));

        let send_via_server = Arc::new(AtomicBool::new(false)); //this client must send via server
        let channel_has_server_relays = Arc::new(AtomicBool::new(false)); //as relay, i should also mirror to serveri

        let punch_sync: PunchSync = Arc::new((
            Mutex::new(PunchState { paused: false }),
            std::sync::Condvar::new(),
        ));

        let relay_sync: RelaySync = Arc::new((
            Mutex::new(RelayState { is_active: false }),
            std::sync::Condvar::new(),
        ));

        networking::start_heartbeat(
            socket.try_clone()?,
            cfg.server_id.clone(),
            cfg.channel.clone(),
            cfg.username.clone(),
            signaling_addr.clone(),
        );

        crate::client::server_responses_during_setup(
            &socket,
            &peers,
            &cfg.username,
            &is_relay,
            &channel_has_server_relays,
            &send_via_server,
            &server_socketaddr,
            &signaling_addr,
            &punch_sync,
            &relay_sync,
        );

        // start punching ONLY if NAT is not symmetric
        if my_nat != NatKind::Symmetric {
            start_hole_punching(
                socket.try_clone()?,
                Arc::clone(&peers),
                Arc::clone(&punch_sync),
            );
        } else {
            println!("Symmetric NAT detected - skipping hole punching, relying on server relay.");
        }

        //Relay keepalive thread starter
        networking::start_relay_keepalive(
            socket.try_clone()?,
            Arc::clone(&peers),
            Arc::clone(&relay_started),
            cfg.server_id.clone(),
            cfg.channel.clone(),
            signaling_addr.clone(),
            Arc::clone(&relay_sync),
        );

        networking::start_user_input(
            socket.try_clone()?,
            Arc::clone(&peers),
            cfg.username.clone(),
            Arc::clone(&send_via_server),
            signaling_addr.clone(),
            Arc::clone(&is_relay),
            Arc::clone(&channel_has_server_relays),
        );

        let main_socket = socket.try_clone()?;
        let peers_cl = Arc::clone(&peers);
        let user_cl = cfg.username.clone();
        let is_relay_cl = Arc::clone(&is_relay);
        let channel_srv_cl = Arc::clone(&channel_has_server_relays);
        let punch_sync_cl = Arc::clone(&punch_sync);
        let relay_sync_cl = Arc::clone(&relay_sync);
        let send_via_server_cl = Arc::clone(&send_via_server);
        let signaling_addr_cl = signaling_addr.clone();

        thread::spawn(move || {
            if let Err(e) = crate::client::main_loop(
                &main_socket,
                &peers_cl,
                user_cl,
                &is_relay_cl,
                &channel_srv_cl,
                &signaling_addr_cl,
                &server_socketaddr,
                &punch_sync_cl,
                &relay_sync_cl,
                &send_via_server_cl,
            ) {
                eprintln!("piercer main_loop exited with error: {}", e);
            }
        });

        Ok(Piercer {})
    }
}
