use od_nat_piercer::signaling::{
    handlers::handle_message, heartbeat::start_heartbeat, structures::ServerMap,
};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_main = UdpSocket::bind("0.0.0.0:2131").await?;
    let socket_probe = UdpSocket::bind("0.0.0.0:2132").await?;

    println!("Signaling server listening on 0.0.0.0:2131");

    let socket_main = Arc::new(socket_main);
    let socket_probe = Arc::new(socket_probe);

    let state = Arc::new(Mutex::new(ServerMap::new()));

    start_heartbeat(Arc::clone(&socket_main), Arc::clone(&state));

    run_server(socket_main, socket_probe, state).await;
    Ok(())
}

async fn run_server(
    socket_main: Arc<UdpSocket>,
    socket_probe: Arc<UdpSocket>,
    state: Arc<Mutex<ServerMap>>,
) {
    let mut buf_main = [0u8; 1024];
    let mut buf_probe = [0u8; 1024];

    loop {
        tokio::select! {
            res = socket_main.recv_from(&mut buf_main) => {
                if let Ok((len,src)) = res{
                    let msg = String::from_utf8_lossy(&buf_main[..len]).to_string();
                    handle_message(msg, src, Arc::clone(&socket_main), Arc::clone(&state)).await;
                }
            }

            res = socket_probe.recv_from(&mut buf_probe) => {
                if let Ok((len,src)) = res{
                    let msg = String::from_utf8_lossy(&buf_probe[..len]).to_string();
                    handle_message(msg, src, Arc::clone(&socket_probe), Arc::clone(&state)).await;
                }
            }
        }
    }
}
