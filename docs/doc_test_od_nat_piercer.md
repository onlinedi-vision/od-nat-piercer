# Local testing guide

This guide exercises the current connectivity and CONTROL prototype on one
computer. It does not test a real NAT, audio, encryption, or production capacity.

Use a controlled development environment: the standalone server binds on all
IPv4 interfaces and the current transports are plaintext.

## Build

Install Rust/Cargo and a native toolchain. From the repository root:

~~~bash
cargo build --release --bin signaling_server --bin client
~~~

## Start the server

In the first terminal:

~~~bash
./target/release/signaling_server
~~~

Check for both listener messages:

~~~text
Signaling server listening on 0.0.0.0:2131
Control WebSocket server listening on 0.0.0.0:2133
~~~

The server also listens for NAT probes on UDP port `2132`. See the
[server guide](doc_signaling_server.md) for listener details.

## Start two clients

In a second terminal:

~~~bash
./target/release/client 127.0.0.1 server1 channel1 user1 4000
~~~

In a third terminal:

~~~bash
./target/release/client 127.0.0.1 server1 channel1 user2 4001
~~~

The arguments are `<signaling_ip> <server_id> <channel> <user> <local_port>`.
Both clients use the same logical server and channel, with distinct usernames
and local UDP ports. See the [client guide](doc_client.md) for each argument.

## What to check

Log ordering, numeric channel IDs, and assigned peer IDs can vary. Look for
these events rather than an exact transcript:

1. Each client receives WELCOME containing a channel ID and a nonzero peer ID.
2. Each opens a WebSocket connection to `ws://127.0.0.1:2133` and sends
   `JoinControl`, then waits for `JoinAccepted`.
3. Each waits for server CONTROL messages after registration. Two idle clients
   do not generate CONTROL broadcasts or application ACKs. Submitted messages
   and incoming ACK behavior are exercised by the automated tests below.
   A later join does not replay earlier messages.
4. On a typical localhost run, the first client starts with `MODE RELAY`.
   After the second joins, peer address messages, UDP punches, and
   `CONNECTED (direct)` messages show the attempted direct path.
5. Enter a short line such as `hello from user1` in the first client terminal
   and check that the second displays it. Repeat in the other direction.

Example CONTROL log lines are:

~~~text
Connected to control WebSocket server at ws://127.0.0.1:2133
CONTROL registration accepted.
~~~

Recipients log `Processed server CONTROL` when a server message arrives.
The automated CONTROL suite uses explicit test messages to exercise broadcast,
queue-admission results, processing ACKs, and finite unconfirmed failure.

Registration checks the WebSocket connection; typed terminal text uses legacy
UDP traffic. Neither demonstrates an audio call or authenticated communication.

A third client can be started with:

~~~bash
./target/release/client 127.0.0.1 server1 channel1 user3 4002
~~~

This lets you inspect additional peer/relay announcements. It does not establish
a tested channel capacity or prove all multi-participant routing behavior.

## Stop the prototype

Stop each client and the server with `Ctrl+C`. The client has no coordinated
graceful departure, so a stopped client's membership can remain until the server
removes it through timeout processing.

## Automated tests

Run the suite from the repository root:

~~~bash
cargo test
~~~

To focus on CONTROL:

~~~bash
cargo test --lib control::
~~~

The CONTROL suite covers accepted/refused/idempotent/conflicting registration,
client registration waits, typed result matching, strict request/notification
IDs, unknown/duplicate confirmations, handler failure without ACK, missing
results/ACKs without retransmission, exact escaped size limits, stalled writes,
recipient capacity, partial admission and healthy-peer progress, cancellation
and cleanup. The production CLI session test verifies registration and processing
ACKs without an automatic demo broadcast.

Tests use real localhost WebSockets on ephemeral ports, explicit message/Pong
barriers, a controlled monotonic clock and a ten-second case bound. State tests
also exercise counter exhaustion and combined queue/pending capacity. No separate
server process, external service or arbitrary sleep is required. Fixture teardown
joins or aborts connection tasks and releases sockets.

## Troubleshooting and scope

| Observation | Check |
| --- | --- |
| Address already in use | Stop the previous process or choose a different client UDP port. Server ports are fixed in the current executable. |
| No NAT probe responses | Check that both UDP server ports are reachable; the client continues with an unknown classification after the probe deadline. |
| No CONTROL connection | Check the separate TCP `2133` listener and whether the client received a nonzero peer ID. |
| CONTROL result received but no peer text | Inspect peer addresses, route messages, and UDP reachability separately from WebSocket CONTROL. |
| Local socket permission errors in tests | Check whether the test environment permits localhost TCP/UDP listeners. |

Real NAT coverage, protected signaling/media, audio behavior, recovery under
network changes, and performance measurements remain separate validation work.
