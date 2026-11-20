//! Bounded NAT heuristic, independent of route readiness.
use crate::proto::control_text::{MSG_NAT_PROBE, MSG_NAT_SEEN, NAT_TYPE_CONE, NAT_TYPE_SYMMETRIC};
use std::{
    net::{SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatKind {
    Unknown,
    Cone,
    Symmetric,
}

const NAT_DETECT_TOTAL_TIMEOUT_MS: u64 = 1000; // maximum waiting time for server to respond to both probes
const NAT_DETECT_POLL_SLEEP_MS: u64 = 20; // sleep between polls when socket is WouldBlock

#[must_use]
pub fn detect_nat_kind(socket: &UdpSocket, signaling: SocketAddr) -> NatKind {
    let endpoints = [signaling, SocketAddr::new(signaling.ip(), 2132)];

    let _ = socket.send_to(format!("{MSG_NAT_PROBE} 1\n").as_bytes(), endpoints[0]);
    let _ = socket.send_to(format!("{MSG_NAT_PROBE} 2\n").as_bytes(), endpoints[1]);

    let mut seen = [None; 2];
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_millis(NAT_DETECT_TOTAL_TIMEOUT_MS);

    while seen.contains(&None) && Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((len, source)) => observe(&mut seen, &endpoints, source, &buf[..len]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(NAT_DETECT_POLL_SLEEP_MS));
            }
            Err(_) => break,
        }
    }

    let [Some(a), Some(b)] = seen else {
        println!("NAT detection: insufficient responses, treating as Unknown");
        return NatKind::Unknown;
    };

    if a.ip() != b.ip() {
        println!("NAT detection: different public IPs, treating as Symmetric");
        return NatKind::Symmetric;
    }

    if a.port() == b.port() {
        println!("NAT detection: {NAT_TYPE_CONE} (same addr on both ports): {a}");
        NatKind::Cone
    } else {
        println!("NAT detection: {NAT_TYPE_SYMMETRIC} (different ports): {a} vs {b}");
        NatKind::Symmetric
    }
}

fn observe(
    seen: &mut [Option<SocketAddr>; 2],
    endpoints: &[SocketAddr; 2],
    source: SocketAddr,
    bytes: &[u8],
) {
    if let Some(index) = endpoints.iter().position(|endpoint| *endpoint == source)
        && seen[index].is_none()
        && let Ok(reply) = std::str::from_utf8(bytes)
        && let Some(address) = reply
            .strip_prefix(MSG_NAT_SEEN)
            .and_then(|tail| tail.strip_prefix(' '))
        && let Ok(address) = address.trim_end().parse()
    {
        seen[index] = Some(address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_observation_per_expected_probe_endpoint_is_counted() {
        let endpoints = [([127, 0, 0, 1], 2131).into(), ([127, 0, 0, 1], 2132).into()];
        let mut seen = [None; 2];
        observe(
            &mut seen,
            &endpoints,
            ([127, 0, 0, 1], 2133).into(),
            b"NAT_SEEN 127.0.0.1:1\n",
        );
        assert_eq!(seen, [None; 2]);
        observe(
            &mut seen,
            &endpoints,
            endpoints[0],
            b"NAT_SEEN 127.0.0.1:2\n",
        );
        observe(
            &mut seen,
            &endpoints,
            endpoints[0],
            b"NAT_SEEN 127.0.0.1:3\n",
        );
        assert_eq!(seen, [Some(([127, 0, 0, 1], 2).into()), None]);
        observe(
            &mut seen,
            &endpoints,
            endpoints[1],
            b"NAT_SEEN 127.0.0.1:4\n",
        );
        assert_eq!(seen[1], Some(([127, 0, 0, 1], 4).into()));
    }
}
