use od_nat_piercer::{
    control::server::serve_sessions,
    recovery::{
        CHECK_INTERVAL,
        server::{Registry, SharedRegistry, dispatch},
        wire::MAX_DATAGRAM,
    },
};
use std::{io, sync::Arc, time::Instant};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::Mutex,
    time::{MissedTickBehavior, interval},
};

#[allow(clippy::disallowed_methods)] // tokio::main builds the runtime.
#[tokio::main]
async fn main() -> io::Result<()> {
    let main = UdpSocket::bind("0.0.0.0:2131").await?;
    let probe = UdpSocket::bind("0.0.0.0:2132").await?;
    let control = TcpListener::bind("0.0.0.0:2133").await?;
    let registry = Arc::new(Mutex::new(Registry::default()));
    println!("Signaling UDP 2131/2132 and CONTROL 2133 listening");
    tokio::select! {
        result = serve_sessions(control, registry.clone()) => result,
        result = run_udp(main, probe, registry) => result,
    }
}

async fn run_udp(main: UdpSocket, probe: UdpSocket, registry: SharedRegistry) -> io::Result<()> {
    let mut primary = [0; MAX_DATAGRAM + 1];
    let mut secondary = [0; MAX_DATAGRAM + 1];
    let mut tick = interval(CHECK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_error = None;
    loop {
        let result = tokio::select! {
            _ = tick.tick() => { registry.lock().await.expire(Instant::now()); Ok(()) },
            result = main.recv_from(&mut primary) => {
                let (len, source) = result?;
                dispatch(&main, &registry, &primary[..len], source, true).await
            }
            result = probe.recv_from(&mut secondary) => {
                let (len, source) = result?;
                dispatch(&probe, &registry, &secondary[..len], source, false).await
            }
        };
        if let Err(error) = result {
            let now = Instant::now();
            if last_error
                .is_none_or(|last| now.duration_since(last) >= std::time::Duration::from_secs(5))
            {
                eprintln!("UDP send error: {error}");
                last_error = Some(now);
            }
        }
    }
}
