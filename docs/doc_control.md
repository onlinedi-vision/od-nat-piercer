# CONTROL guide

The active CLI uses plaintext WebSocket CONTROL at `ws://<server>:2133` for
session-bound route synchronization and fallback requests. It is not an
authenticated application membership service or a media carrier.

After UDP registration, `SessionJoin {request_id, binding}` must match the current
UDP channel/peer/attempt/name registration. `JoinAccepted` precedes the complete
`RouteSnapshot`. A snapshot is processed atomically before `ControlAck`; malformed
or wrong-session state closes the connection. Rebinding replays the current
snapshot without replacing healthy UDP state. An old worker's close cannot
remove its replacement, and replacement of the UDP session retires old CONTROL.

`Fallback {request_id, session, target}` is accepted only from the current bound
CONTROL connection and an allowed reporting role. `FallbackResult` confirms
request processing. Route activation still comes from `RouteSnapshot`. If the
connection fails before confirmation, rebind and reconcile the authoritative
snapshot; no separate UDP notification resend mechanism exists.

Messages use JSON text and have a complete 8 KiB limit. The existing bounded
one-send machinery applies: 32 slots including queued and unconfirmed messages,
5-second registration/confirmation deadlines, and 2-second write/close bounds.
Unknown, malformed or wrong-direction messages fail explicitly. A slow recipient
is closed; no unbounded retry or queue is added. TCP/WebSocket provides ordering;
application confirmation proves processing, not durable delivery.

Generic unscoped JoinControl/broadcast helpers are retained for regression tests.
The executable server exposes the session-bound handler, which rejects an
unscoped join. No CLI demonstration broadcast is sent.

A CONTROL-only failure starts bounded rebind attempts while valid UDP heartbeats
and direct checks continue. The recovery slots and 30-second exhaustion policy
are shared with registration recovery; the runtime owns one CONTROL worker at
a time and cancels it when its registration is replaced. Routine probes and
heartbeats remain silent. This lifecycle is transport-local and will be reviewed
against authenticated application membership during M2.

See [protocol](protocol.md), [state machines](state-machines.md), and
[ADR-017](decisions/ADR-017-prototype-session-recovery.md).
