# Client **Phase 1** doc

## Purpose
This UDP client connects to the signaling server (soon to be [od-official-server]) to join the voice chat channel using UDP punching.
It handles communication with the signaling server to discover peers in the same channel, then attempts to establish direct UDP connections with those peers. (Possibly soon to be changed)

---

## How it works

### 1. Command-line Arguments
The client expects 6 arguments on startup: (**as for now, because i've tested it on a local machine and i needed these 6 parameters**)
client <signaling_ip> <client_ip> <server_id> <channel> <user> <local_port>

- `signaling_ip`: IP address of the signaling server (UDP port 5000) 
- `client_ip`: Local IP address of this client
- `server_id`: Identifier of the server instance
- `channel`: Channel name to join
- `user`: Username for this client
- `local_port`: UDP port to bind locally for sending/receiving messages

---

### 2. Socket Setup
- Binds a UDP socket locally on `0.0.0.0:<local_port>`.
- Sets a read timeout of 3 seconds on the socket to avoid blocking indefinitely.

---

### 3.Connect to Signaling Server
- Sends a `CONNECT` message to the signaling server at `signaling_ip:5000` with the following format:
CONNECT <server_id> <channel> <user> <client_ip:local_port>

- Wait for the server's response, which contains the **connection mode** and peer information

---

### 4. Parsing Server Response
- Reads the response and interprets each line:
- `MODE RELAY`: the client is the only user in the channel, so relay mode is active (no direct peers)
- `MODE DIRECT <peer_user> <peer_addr>`: Indicates a peer in the channel to connect to directly

- Populates a list of peer UDP addresses (**excluding itself**) for direct communication

---

### 5. Hole Punching Phase

- If in **DIRECT MODE** (peers available) -> spawns a background thread to send repeated UDP "punch" messages to each peer every 500ms for 5 seconds. This helps to open **NAT** bindings and establish direct communication.
- If in **RELAY MODE** (alone user) -> no punching happens and client simply waits for incoming messages. (This might be updated in the future)

---

### 6. Listening Phase
- On the main thread, listens for incoming UDP packets on the socket for 5 seconds (the hole punching duration)
- Prints any received messages with their source address

---
### 7. End of Hole Punching
- After 5 seconds, the client finishes the hole punching and listening phase and terminates.

---

## Notes
- The client relies on the signaling server to provide accurate peer information for hole punching.
- The use of `Arc` allows sharing the UDP socket and peer list safely between threads.
- Timeout and error handling are not the best, but present at the moment, to avoid deadlocks during communication.
