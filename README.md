# Online Division NAT Piercer

A **UDP NAT traversal protocol** and Rust implementation being built for Online
Division voice channels, with video and screen sharing as longer-term goals.

It uses NAT probing and UDP hole punching to attempt direct connections between
peers behind routers, with relay fallback when a direct path is unavailable.

## The goal

> Make voice channels fast, secure, and dependable across real networks, using
> direct UDP connections when possible and deliberate relay fallback when needed.

The intended product is a reusable Rust transport and security runtime embedded
in the Online Division client. It should establish connections, recover from
path failures, and preserve participant identity, logical streams, and the
required protection level as routes change.

In the intended design, the client application owns audio capture, codecs,
playback, and the user experience. The backend owns application identity and
channel authorization. Piercer provides connectivity and transport protection
between those boundaries.

## Main components

| Component | Responsibility |
| --- | --- |
| **Client / endpoint** | Runs on a participant's device, communicates with the signaling server, and sends and receives peer traffic over UDP. |
| **Signaling server** | Groups participants into servers and channels, shares peer addresses, and coordinates relay selection. |
| **UDP NAT traversal** | Uses NAT probes and UDP hole punching to attempt direct connections between peers behind routers. |
| **Reliable CONTROL** | Carries client-to-server JSON control messages over WebSocket, with acknowledgments, bounded retries, and duplicate suppression per connection. |
| **Relay fallback** | Forwards traffic through a participant or server when a direct connection is unavailable. |

CONTROL supports coordination between endpoints and the server. The UDP path
is the foundation for future media traffic. Today, the prototype exchanges
demonstration CONTROL payloads and terminal text; audio integration and transport
security remain to be implemented.

## NAT detection and relay fallback

NAT Piercer estimates NAT behavior by comparing addresses reported by UDP
probes sent to two ports on the signaling server. The current prototype
classifies the results as cone-like, symmetric-like, or unknown.

When a client is classified as symmetric, the prototype skips UDP hole punching
and relies on server relaying, so traffic can follow an indirect path instead.

These classifications are heuristic: they do not identify every NAT behavior or
guarantee direct connectivity. A symmetric classification guides the current
fallback policy; it is not a universal proof that a direct connection is
impossible.

## Current status

**Early prototype: connectivity and CONTROL are under active development.**
The current implementation does not yet provide voice calls or production
security. Low latency and performance are goals that still require measurement.

Implemented so far:

- UDP signaling, channel membership tracking, and peer discovery.
- Heuristic NAT probing, UDP hole punching, and basic direct peer communication.
- Prototype participant and server relay paths for terminal text traffic.
- JSON CONTROL messages over WebSocket, with pending-message tracking, matching
  acknowledgments, bounded retries, and duplicate suppression per connection.
- Unit tests and real localhost WebSocket integration coverage for CONTROL
  delivery, failure handling, message limits, and cleanup.

The current client sends a demonstration CONTROL payload to the server. Its ACK
confirms processing by that server on the current connection; it does not
confirm delivery to other participants. CONTROL broadcast is still to be
implemented.

Signaling currently uses plaintext UDP and `ws://` without participant
authentication. Production signaling protection, peer verification, and media
encryption remain to be implemented.

## What remains to build

- Complete CONTROL broadcast and consistent membership, routing, disconnect,
  and shutdown behavior.
- Add binary media transport and expose a reusable session runtime for the
  client application.
- Integrate audio from `od-client` for the first controlled development voice
  calls.
- Improve call behavior during participant changes, reconnects, packet loss,
  and route migration.
- Secure production CONTROL/signaling and media, including identity binding,
  authorization, replay protection, and rejection of security downgrades.
- Evaluate optional group key management and end-to-end media encryption.
- Validate security, latency, resource usage, recovery, and multi-user behavior
  before production readiness.

Secure signaling and protected media are requirements for production calling.
Implementation details will be refined as the working system evolves. Video
and screen sharing are future uses of the same transport foundation.

## Try the local prototype

Use a controlled development environment. Building requires Rust/Cargo and the
native build tools needed by the vendored OpenSSL dependency.

From the repository root, build the server and client:

```bash
cargo build --release --bin signaling_server --bin client
```

Start the signaling server in one terminal:

```bash
./target/release/signaling_server
```

It binds UDP ports `2131` and `2132`, plus TCP port `2133` for WebSocket CONTROL,
on all interfaces. The following clients connect through localhost.

Start the first client in another terminal:

```bash
./target/release/client 127.0.0.1 server1 channel1 user1 4000
```

Start a second client in a third terminal:

```bash
./target/release/client 127.0.0.1 server1 channel1 user2 4001
```

The arguments are `<signaling_ip> <server_id> <channel> <user> <local_port>`.

| Argument | First client example | Meaning |
| --- | --- | --- |
| `signaling_ip` | `127.0.0.1` | Signaling server IP address; `127.0.0.1` means this computer. |
| `server_id` | `server1` | Logical server identifier used to group channels. |
| `channel` | `channel1` | Name of the channel to join within that server. |
| `user` | `user1` | Username used by this prototype client. |
| `local_port` | `4000` | Local UDP port this client binds to. |

Use the same server and channel values, with distinct users and local ports.

The clients receive peer IDs, open CONTROL connections, and exchange UDP
connectivity messages. This example exercises the current connectivity and
CONTROL prototype. Stop each process with `Ctrl+C` when finished.

## Documentation

- [Client guide](docs/doc_client.md): arguments, connection setup, NAT probing,
  terminal messages, and current limitations.
- [Signaling server guide](docs/doc_signaling_server.md): listeners, membership,
  relay selection, and UDP commands.
- [Reliable CONTROL guide](docs/doc_control.md): JSON messages, ACKs, retries,
  duplicate handling, and connection limits.
- [Local testing guide](docs/doc_test_od_nat_piercer.md): a two-client example,
  expected events, automated tests, and troubleshooting.

## Tests

Run the test suite:

```bash
cargo test
```

To run only the CONTROL tests:

```bash
cargo test --lib control::
```

The CONTROL integration tests use short-lived localhost connections and
controlled retry deadlines. A localhost run does not establish behavior across
real NATs or measure voice quality or production capacity.
