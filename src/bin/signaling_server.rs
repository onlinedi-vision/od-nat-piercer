use od_nat_piercer::signaling::{
    handlers::handle_message, heartbeat::start_heartbeat, structures::ServerMap,
};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind("127.0.0.1:5000").await?;
    println!("Signaling server listening on 0.0.0.0:5000");

    let socket = Arc::new(socket);
    let state = Arc::new(Mutex::new(ServerMap::new()));

    start_heartbeat(Arc::clone(&socket), Arc::clone(&state));

    let mut buf = [0u8; 1024];

    loop {
        let (len, src) = socket.recv_from(&mut buf).await?;
        let msg = String::from_utf8_lossy(&buf[..len]).to_string();
        handle_message(msg, src, Arc::clone(&socket), Arc::clone(&state)).await;
    }
}
