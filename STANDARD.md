# Historical port proposal

Port `1001` and the `NAT_PIERCER_PORT` environment variable belonged to an
unimplemented proposal. They do not configure the current executables.

The signaling server currently binds UDP ports `2131` and `2132` and WebSocket
TCP port `2133`. The executables do not read `NAT_PIERCER_PORT`. See the
[signaling server guide](docs/doc_signaling_server.md) for current usage.
