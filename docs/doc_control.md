# Reliable CONTROL guide

CONTROL carries coordination messages between a client and the signaling server
over a separate WebSocket connection. Its role is to support control operations
whose processing needs to be confirmed. Voice media will use the UDP transport;
the current implementation does not carry audio.

The present endpoint is `ws://<signaling_ip>:2133`. It is plaintext and does not
authenticate participants or authorize channel access. Production signaling
protection remains future work.

Some UDP packets also use the CONTROL label, including WELCOME. The ACK and
retry rules in this guide apply to WebSocket `ControlMsg` messages only.

## What works today

The normal client sends a channel registration followed by one 100 KiB
demonstration payload. The server logs each new message's ID and payload length,
then ACKs it. The reliability logic tracks pending messages, retries missing
ACKs, and prevents a retried message from being processed twice on the same
connection.

Current CONTROL processing does not broadcast to participants, implement voice
commands, or persist results. An ACK confirms that the server reached its
processing branch for that connection. It does not confirm recipient delivery,
durable storage, or a completed application operation.

## Messages

Each message is a JSON object carried in a WebSocket text message. The `type`
field selects one of three variants.

A normal client first registers its claimed channel and identity:

~~~json
{"type":"JoinControl","server_id":"server1","channel":"channel1","user":"user1","peer_id":1}
~~~

`peer_id` normally comes from the UDP WELCOME response. The server currently
accepts these registration fields without verifying them against UDP membership
or an authenticated application session. JoinControl has no application ACK or
reliability sequence ID.

The client sends a payload with a numbered message ID:

~~~json
{"type":"ControlMsg","message_id":1,"payload":"example control data"}
~~~

The server responds with the same ID:

~~~json
{"type":"ControlAck","message_id":1}
~~~

Only an ACK matching a currently pending ID releases that message's pending
state. Unknown, repeated, or stale ACKs leave other pending messages unchanged.
The server does not yet track reliable outgoing messages, and the normal client
does not implement application processing for server-to-client CONTROL payloads.

## Why an application ACK is useful

WebSocket runs over TCP, which handles ordered byte delivery while the
connection remains usable. Completing a socket write does not prove that the
remote application parsed or processed a control operation.

The CONTROL ACK supplies that application-level confirmation. A missing ACK
leaves delivery unconfirmed: the server might have processed the message before
the connection failed. Retry and duplicate tracking address that uncertainty
within a live connection; they do not create durable delivery across restarts.

## IDs and duplicate handling

New messages use consecutive `u64` IDs beginning at `1` on each connection.
IDs keep increasing past `32`; they do not restart when the window fills.
IDs never wrap around.

For each connection, the server retains the highest processed ID and SHA-256
fingerprints for the most recent 32 processed payloads:

| Incoming message | Server behavior |
| --- | --- |
| Next consecutive ID | Process once, record its ID and fingerprint, then ACK. |
| Recent ID with the same decoded payload | Skip processing and send the ACK again. |
| Recent ID with a different payload | Reject the conflicting reuse and end the connection. |
| Zero, a gap in new IDs, or an ID older than retained history | Reject and end the connection. |

Fingerprints compare the decoded payload's bytes, so equivalent JSON escaping
does not turn a retry into a different message. SHA-256 here supports content
comparison; it is not participant authentication or a message signature.

The highest processed ID remains recorded for the connection's lifetime even
as old fingerprints are removed. This prevents an old ID from being treated as
a fresh operation after history rolls forward.

A new connection begins new sequence and duplicate state. There is no automatic
reconnect/resume or duplicate suppression across connections and process
restarts.

## Pending window and size limits

The sender limits both the number of pending messages and the distance from
the oldest unacknowledged ID. A new ID must be fewer than 32 positions ahead of
that oldest pending ID. This keeps possible retries inside receiver history.

For example, after sending IDs `1` through `32`, ID `33` waits while ID `1`
is still pending, even if ACKs for `2` through `32` have arrived. Once ID `1`
is acknowledged, new sends can continue. A full window rejects a new send
attempt; it does not silently discard or queue the operation.

This is a limit on outstanding CONTROL work per connection, not on the number
of people who can join a voice channel. Channel capacity has not been measured.

| Setting | Current policy |
| --- | --- |
| Pending message count / permitted ID span | 32 positions per connection. |
| Retained pending serialized data | 4 MiB (4,194,304 bytes). |
| Complete serialized message and individual WebSocket frame | 1 MiB (1,048,576 bytes). |
| Receiver fingerprint history | Most recent 32 processed payloads, plus the highest processed ID. |
| ACK deadline | 2 seconds after a completed write. |
| Write timeout | 2 seconds per CONTROL write. |
| Total send attempts | 4: initial send plus at most 3 retransmissions. |

MiB means 1,024 × 1,024 bytes. The message limit includes JSON fields and escaping,
so the allowed payload can be smaller than 1 MiB. The pending-data budget counts
retained serialized messages; it is not a cap on total process memory, socket
buffers, or every server queue.

These values are initial policies, not measured latency or capacity guarantees.
They are currently fixed in the implementation, without wire negotiation.

## Retries and terminal failures

Each retry reuses the original ID and serialized message. After a completed
retry write, its ACK deadline starts again.

If the final attempt's ACK deadline expires, delivery is reported as unconfirmed
and pending sender state is cleared and closed. A failed or timed-out write,
malformed incoming JSON, a connection ending with pending messages, or
cancellation of the send/receive transition also prevents continued use of that
sender state.

The server drops connection-local duplicate history when handling ends. Normal
completion and message-handling errors also run registration cleanup. This is
connection cleanup, not a complete channel membership or reconnect lifecycle.

## Tests and source

The [testing guide](doc_test_od_nat_piercer.md) explains local runs and automated
coverage. Tests exercise real localhost WebSocket I/O, including suppressed ACKs,
retries, conflicting duplicates, sequence rollover, size limits, and cleanup.

- [JSON message definitions](../src/control/messages.rs)
- [Pending messages, retries, and duplicate tracking](../src/control/reliability.rs)
- [Client send and receive loop](../src/control/client.rs)
- [Server connection handling](../src/control/server.rs)
