# CONTROL guide

CONTROL carries opaque coordination payloads over `ws://<signaling_ip>:2133`.
It currently uses plaintext, unauthenticated registration. It does not provide
audio, authorized channel membership, encryption, or durable delivery.
UDP WELCOME also uses the CONTROL label; the rules here apply only to WebSocket.

## Requests and results

The CLI sends registration, waits for acceptance, then receives server messages.
It sends no demonstration broadcast and has no command for voice operations.
Tests submit broadcasts through the same production client functions.

Messages are JSON text with a `type` tag. A normal exchange starts with:

```json
{"type":"JoinControl","request_id":1,"server_id":"server1","channel":"channel1","user":"user1","peer_id":1}
{"type":"JoinAccepted","request_id":1}
```

Registration is for one exact server/channel pair and claimed identity. Text
fields must be nonempty, and peer_id nonzero. These claims are not verified
against UDP membership or an authenticated application session. An identical
registration under a new request ID is idempotent. Invalid or conflicting
registration gets a rejection then closes; it cannot replace an existing join.
The acceptance result precedes any notification to that connection.

A registered source can submit:

```json
{"type":"Broadcast","request_id":2,"payload":"example control data"}
{"type":"BroadcastResult","request_id":2,"report":{"queued":2,"failed":0}}
```

The result means two copies entered recipient queues. It does not confirm
recipient processing. No recipients produces `queued:0, failed:0`. Results never
receive ACKs, are not revised later, and have no aggregate final confirmation.

A rejected request receives, for example:

```json
{"type":"RequestRejected","request_id":2,"reason":"NotRegistered"}
```

Rejection reasons are `NotRegistered`, `InvalidRegistration`,
`ConflictingRegistration`, and `MessageTooLarge`. Broadcast before registration
has no effect and does not extend the original registration deadline. An
oversized resulting notification is rejected before any fanout; the source
connection stays usable.

## Broadcast notifications

Recipients are the other registered connections in the exact server/channel
pair at processing time. Exclude the source, unregistered sockets and other
pairs. A new connection does not receive earlier broadcasts.

```json
{"type":"ControlMsg","message_id":1,"payload":"example control data"}
{"type":"ControlAck","message_id":1}
```

Each recipient uses its own notification sequence, independent of source IDs.
The recipient sends an ACK only after its local handler succeeds. A handler
failure closes the connection without ACK. Current processing logs only IDs and
payload lengths; it does not implement a voice or group-key operation. Received
notifications and ACKs are never rebroadcast by the client.

## IDs and failure

Client requests share consecutive `u64` IDs from 1 for registration and broadcast.
Each recipient's notification sequence also starts at 1. IDs never wrap. Zero,
reuse, gaps and exhausted counters fail explicitly before repeated processing.
A semantic rejection consumes a request ID; a local pre-send refusal does not.

Results match the pending request's ID and type. A wrong result type closes the
connection. Unknown, duplicate and stale results/ACKs are ignored and release
no other work. Invalid JSON, binary input and wrong-direction messages are
protocol failures. WebSocket Ping/Pong/Close retain their normal behavior.

Each message is sent once. There is no retransmission, receipt-history cache,
or reconnect replay. A missing result/ACK closes the affected connection with
`delivery unconfirmed`. The operation may already have executed; failure does
not prove otherwise. Socket writes alone cannot confirm application processing.

## Development limits

| Setting | Policy |
| --- | ---: |
| Complete JSON message and WebSocket frame | 8 KiB (8,192 bytes) |
| Sent client requests awaiting results | 32 |
| Per-recipient queued plus unacknowledged notifications | 32 |
| Result/ACK wait after completed write | 5 seconds |
| Registration after WebSocket upgrade | 5 seconds |
| WebSocket write or close | 2 seconds |

Size includes fields, IDs and JSON escaping. Before fanout, the server checks
the notification serialized with the largest `u64` ID. Thus a source request
may fit while its resulting notification is rejected. Local validation runs
before large copies and again after serialization.

A single message-slot reservation covers a notification from queue admission
through ACK or termination. Payloads are shared between recipient queues and
are not retained in pending state after writing. Acknowledging newer IDs allows
more sends even if an older ID remains pending. There is no ID-distance rule.
32 × 8 KiB gives a conservative 256 KiB content bound per recipient; it excludes
socket buffers, connection overhead and total process memory.

These provisional limits are not measured capacity or latency guarantees.

## Slow recipients and cleanup

Admission never waits for a recipient socket. Saturation immediately stops
admission and closes that recipient's CONTROL connection with delivery
unconfirmed. Healthy recipients and the source continue; the source receives
partial counts such as `queued:1, failed:1`. A queued copy can still fail later;
the earlier report stays a statement about queue admission.

Normal/error termination releases queued/pending work and removes registration.
Cancellation releases resources immediately; inactive closed registry handles
are pruned on subsequent activity. No detached writer or per-message task runs.
CONTROL closure does not remove UDP/application membership or alter routes.
There is no automatic reconnect or state recovery.

## Future coordination and security

The application backend owns authorized membership decisions. Future CONTROL
operations may describe sessions, connectivity, streams or public peer-security
information. Usable media keys and private endpoint keys belong at legitimate
recipient endpoints, not signaling or forwarding infrastructure. Production
voice requires end-to-end confidentiality and the planned security validation.
The prototype's separate listener does not settle the final application carrier.

## Tests and source

The [testing guide](doc_test_od_nat_piercer.md) describes the real localhost
suite, strict ID/size cases, finite missing-confirmation failures, recipient
isolation, handler errors and cleanup.

- [JSON messages](../src/control/messages.rs)
- [Pending confirmations and strict sequences](../src/control/reliability.rs)
- [Recipient capacity](../src/control/recipient.rs)
- [Client](../src/control/client.rs)
- [Server](../src/control/server.rs)
