# Client guide

The prototype client joins a channel, attempts direct UDP connectivity, and uses
participant or server relay paths where selected by the signaling server. It
also opens a WebSocket connection for demonstration CONTROL messages.

Audio integration is future work. The current client exchanges terminal text
and connectivity messages, without transport encryption or participant
authentication. Use it in a controlled development environment.

## Build and start

From the repository root:

~~~bash
cargo build --release --bin client
./target/release/client 127.0.0.1 server1 channel1 user1 4000
~~~

Start the [signaling server](doc_signaling_server.md) first.

The command takes five positional arguments:

~~~text
client <signaling_ip> <server_id> <channel> <user> <local_port>
~~~

| Argument | Example | Meaning |
| --- | --- | --- |
| `signaling_ip` | `127.0.0.1` | Signaling server IP address; localhost for this example. |
| `server_id` | `server1` | Logical namespace that groups channels on the server. |
| `channel` | `channel1` | Channel name within that namespace. |
| `user` | `user1` | Prototype username, supplied by the caller. |
| `local_port` | `4000` | Local UDP port; the client binds `0.0.0.0:4000`. |

Use the same server and channel for participants who should communicate.
Use distinct usernames and, when running on one computer, distinct local UDP
ports. These names and IDs are not authenticated credentials.

## Connection flow

1. Bind a nonblocking UDP socket on the requested local port.
2. Send NAT probes to the signaling server's UDP ports `2131` and `2132`.
   The client waits up to 600 ms for two observed-address responses.
3. Send `CONNECT <server_id> <channel> <user> <nat_type>` to UDP port `2131`.
4. Handle the server's `WELCOME <channel_id> <peer_id>` response and route
   instructions. WELCOME currently travels inside an ODNP CONTROL UDP packet.
5. Once a nonzero peer ID is available, open `ws://<signaling_ip>:2133`, send
   `JoinControl`, and send a 100 KiB demonstration CONTROL payload.
   A matching `ControlAck` clears that pending message.
6. Continue handling UDP traffic, peer information, heartbeat messages, and
   terminal input until the process is stopped or a fatal error ends the client.

WELCOME and route messages may arrive in either order. The WebSocket connection
can therefore start during the main receive loop rather than immediately after
CONNECT. See [reliable CONTROL](doc_control.md) for its separate ACK/retry rules.

## NAT probing and route selection

The NAT probe compares source addresses reported by the two server ports:

| Probe result | Client classification | Current behavior |
| --- | --- | --- |
| Same observed IP and port | Cone-like | Attempt UDP hole punching for eligible peers. |
| Different observed IP or port | Symmetric-like | Skip hole punching and rely on server relay selection. |
| Fewer than two responses | Unknown | Continue setup and permit hole-punch attempts. |

This is a heuristic, not a complete NAT discovery procedure or a guarantee of
reachability. Localhost commonly reports a cone-like result because both probes
use the same local address; that does not exercise a real router.

The server communicates route choices with these messages:

| Message | Meaning |
| --- | --- |
| `MODE DIRECT <peer_user> <peer_addr>` | Add a peer address for direct UDP communication. |
| `MODE RELAY` | Assign this client the participant relay role. A lone eligible client can receive it too. |
| `MODE SERVER_RELAY <user>` | Use the server relay path for the named participant. |

For eligible peers, the hole-punch worker retries with increasing intervals.
It skips connected or server-relayed peers and can pause when this client is
told to send through the server. The worker runs in the background while the
client process is active.

The client sends a server heartbeat every 20 seconds. When active, the relay
keepalive worker checks peer liveness and can request server relaying for a
peer that has not connected. Route recovery and membership handling are still
prototype behavior; they are not production guarantees.

## Terminal messages and stopping

Enter a short line in a client terminal to send legacy UDP text. The selected
route determines whether it goes to known peers or through the signaling server.
This traffic is separate from reliable WebSocket CONTROL and has no application
ACK/retry guarantee.

Stop each client with `Ctrl+C`. The executable currently has no coordinated
session shutdown or automatic CONTROL reconnect. Process termination does not
send a graceful DISCONNECT; server membership may remain until timeout cleanup.

For a complete two-client example, use the
[local testing guide](doc_test_od_nat_piercer.md).

## Source

- [Client executable](../src/bin/client.rs)
- [NAT probes and networking workers](../src/client/networking.rs)
- [UDP message handlers](../src/client/handlers.rs)
