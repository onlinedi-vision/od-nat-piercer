# Signaling server guide

The standalone signaling server tracks prototype participants, shares UDP
addresses, coordinates relay choices, and hosts the WebSocket CONTROL endpoint.
It also forwards legacy UDP text where server relaying is selected.

This executable is a development prototype. Usernames, channel names, and peer
IDs are caller-supplied or server-assigned identifiers, not authenticated
application identities. UDP signaling and WebSocket CONTROL are currently
plaintext.

## Build and start

From the repository root:

~~~bash
cargo build --release --bin signaling_server
./target/release/signaling_server
~~~

The executable uses fixed ports and accepts no port configuration arguments:

| Listener | Bind address | Purpose |
| --- | --- | --- |
| UDP | `0.0.0.0:2131` | Primary signaling, NAT probes, and server relay traffic. |
| UDP | `0.0.0.0:2132` | Second port used by NAT probing. |
| TCP / WebSocket | `0.0.0.0:2133` | JSON CONTROL messages over `ws://`. |

The UDP listeners share the command handler. All listeners bind on all IPv4
interfaces; use a controlled development environment. The
[local testing guide](doc_test_od_nat_piercer.md) connects clients through
localhost.

Successful startup normally includes:

~~~text
Signaling server listening on 0.0.0.0:2131
Control WebSocket server listening on 0.0.0.0:2133
~~~

The WebSocket listener runs in a background task. If it fails, the UDP loop can
continue, so check for both startup messages.

## Joining a channel

The normal client sends this UDP command to port `2131`:

~~~text
CONNECT server1 channel1 user1 CONE
~~~

The fields identify a logical server namespace, channel name, username, and
client-reported NAT classification. The server takes the UDP address from the
packet's source, rather than accepting a client address field.

The server stores channels under their server namespace. Each channel contains
its users, a numeric channel ID, the next peer ID, and its selected participant
relay, if any. A new user receives a peer ID within that channel.

The server returns:

~~~text
WELCOME <channel_id> <peer_id>
~~~

WELCOME is text inside an ODNP CONTROL UDP packet. The client then uses its peer
ID when opening a separate WebSocket CONTROL connection. That join is not yet
authenticated or checked against the UDP membership record.

A repeated CONNECT from the same username and address refreshes the existing
entry and re-sends WELCOME. A matching username from a different address replaces
the previous entry. This behavior is not secure session resumption.

## NAT probing and relay choices

A `NAT_PROBE` received on either UDP listener produces a
`NAT_SEEN <observed_source_address>` response. The client compares responses
from the two ports to estimate NAT behavior.

The server currently marks clients reporting symmetric NAT for server relaying.
It selects an eligible participant relay when one exists and exchanges peer
addresses through `MODE DIRECT` messages. The selected relay receives
`MODE RELAY`. A lone eligible participant can also hold that role.

Participants requiring server relaying receive `MODE SERVER_RELAY <user>`.
If no eligible participant relay exists, the server selects server relaying for
the channel's participants. Later relay requests can mark a participant for
server forwarding.

These are prototype policies driven partly by client-reported classifications.
They do not guarantee connectivity across all NATs, successful migration, or
correct recovery from every participant change.

## UDP command overview

| Command | Current purpose |
| --- | --- |
| `NAT_PROBE <probe_id>` | Ask the server which source address it observes. |
| `CONNECT <server_id> <channel> <user> <nat_type>` | Join or refresh a prototype participant. |
| `HB <server_id> <channel> <user>` | Refresh liveness for a matching participant and source address. |
| `PING` / `PONG` | Probe liveness and refresh matching users on PONG. |
| `REQUEST_RELAY <server_id> <channel> <user>` | Request server relaying for a participant. |
| `PEER_TIMEOUT <server_id> <channel> <user>` | Report an unreachable peer; currently also affects membership. |
| `DISCONNECT <server_id> <channel> <user>` | Handle an explicit departure; the normal client does not send it on termination. |
| `DATA <user> <text>` | Forward legacy text according to current channel/relay state. |

These commands are not a production authorization boundary. In particular,
peer failure reporting currently affects the membership list, although a failed
network path should eventually be handled separately from application membership.

The heartbeat worker scans every 20 seconds. Users whose recorded liveness is
older than 40 seconds can be removed on a scan, with departure notifications and
relay reassignment where applicable. This is not an exact 40-second disconnect
deadline.

## WebSocket CONTROL

The endpoint is `ws://<signaling_ip>:2133`. A normal client sends:

~~~json
{"type":"JoinControl","server_id":"server1","channel":"channel1","user":"user1","peer_id":1}
~~~

It then sends numbered `ControlMsg` messages. The server handles a new ID once,
records it in connection-local duplicate tracking, and returns a matching
`ControlAck`. Current handling logs the ID and payload length; it does not
broadcast the payload to other participants.

See [reliable CONTROL](doc_control.md) for message examples, sequence rules,
limits, retries, and failure behavior.

## State and shutdown limits

Membership, relay selection, and CONTROL duplicate history are held in memory.
A process restart loses that state. The UDP membership map and WebSocket
registration map are separate; opening or closing one transport does not
establish a complete authenticated session lifecycle.

Stop the standalone server with `Ctrl+C`. Coordinated shutdown, authenticated
membership, protected signaling, and reliable route recovery remain future work.

## Source

- [Server executable](../src/bin/signaling_server.rs)
- [UDP command dispatch](../src/signaling/handlers/message.rs)
- [Membership and channel state](../src/signaling/structures.rs)
- [WebSocket connection handling](../src/control/server.rs)
