# Development client

The CLI exchanges plaintext terminal text through direct UDP, a participant
relay and explicit server fallback. It does not implement voice or encryption.

```sh
./target/release/client onlinedi.vision server1 channel1 user1 4001
```

Arguments are signaling host, server namespace, channel, username and fixed local
UDP port. Use the same namespace/channel and different usernames for participants.
Use distinct local ports for processes sharing one machine. Names and IDs are
not authenticated credentials. The client resolves an IPv4 signaling address.

## Joining and recovering

1. Bind the requested port and probe server UDP 2131/2132, waiting up to 1 second.
2. Register a fresh 128-bit attempt through CONNECT/CHALLENGE/CONFIRM/WELCOME.
3. Bind CONTROL at `ws://<server>:2133` to the committed transport session.
4. Apply a complete route snapshot, then probe the selected direct neighbours.
5. Exchange text when the selected route is ready. Otherwise report not-ready;
   input is not retained for a later successful reconnection.

A confirmed restart gets a fresh peer ID even with the same name and address.
An old timeout, heartbeat or direct packet cannot update the replacement by name.
The attempt ID is used during setup, not attached to every data packet.

Eligible direct paths are punched at least 150 ms apart during the first second,
then 1.5 seconds apart, for at most 12 seconds. Observing a direct packet stops
punching and starts 1-second health probes. Five seconds without valid direct
activity requests fallback over confirmed CONTROL. It does not remove the user
from the application or prove that a new route has already become active.

A server registration heartbeat runs every second. Routine heartbeat replies,
PING/PONG and successful punches do not print. Logs show direct observation,
fallback (including failed peer/address), registration/recovery and failures.
Repeated UDP errors are rate-limited.

Six seconds without a fresh matching heartbeat ACK starts registration recovery.
A CONTROL-only failure first retries CONTROL while UDP checks continue. Recovery
uses slots at 0, 1, 3, 7, 15 and 23 seconds, stopping after 30 seconds. A suspended
process does not emit a burst of missed retries on return. Socket recreation
keeps the configured local port; a bind error is reported explicitly.

Enter `/retry` for a fresh explicit cycle or `/quit` to exit. EOF also exits.
There is no special Ctrl+C handler. Crash/forced termination correctness depends
on scoped replacement and expiry, not on a departure message. `/quit` attempts a
best-effort scoped DISCONNECT; failed cleanup does not block a later join.

## Important limits

NAT classification is a heuristic. Unknown means insufficient observations and
still allows path attempts. Using the same public IP does not guarantee router
hairpinning; LAN discovery is not included. Server fallback remains available.

This is development transport: source and session checks are not authentication.
Text is plaintext, and no audio capture, codec, MEDIA or E2EE exists here.
Automated localhost/controlled-clock tests do not establish behavior of real
routers, iPhone network switching, OS suspension or production capacity.

See [protocol](protocol.md), [CONTROL](doc_control.md), and
[ADR-017](decisions/ADR-017-prototype-session-recovery.md).
