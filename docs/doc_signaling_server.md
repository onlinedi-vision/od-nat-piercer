# Signaling Server Phase 1

## Purpose

This UDP server act as signaling server for a peer-to-peer voice chat that uses UDP hole punching to connect clients/users directly.

It keeps track of users grouped into channels with different server IDs and shares their network address so they can connect to each other. (this will probably be modified a little)
If a user is alone in a channel, the server will set him into a **RELAY** mode; if the user is not alone, he is placed into **DIRECT** mode with the addresses of their peers.

---

## Data structures
### User
Represents a connected user:
  - `name` - username (`String`)
  - `addr` - UDP socket address of the user (`SocketAddr`)

### Channel
A vector of `User` - the list of all users in the same channel

### ServerMap
A `HashMap` that represents the current state of all users grouped by server and channel.
`HashMap<String, HashMap<String, Channel>>`
(`server_id` -> `channel_name` -> `Vec<User>`)

---


## How it works:
### 1. Server startup
The server binds to UDP port `5000` on all interfaces (`0.0.0.0:5000`) and starts listening for incoming UDP packets async.

### 2. Receiving messages
The server waits for incoming UDP datagrams. Each datagram is expected to be a UTF-8 encoded string command

### 3. Parsing messages
The server splits the received message by whitespace into tokens. It expects commands that follow one of the patterns:
- `CONNECT <server_id> <channel> <user> <user_addr>`
- `DISCONNECT <server_id> <channel> <user>`

### 4. Handling `CONNECT`:
- Extract the `server ID`, `channel name`, `user name`, and the UDP socket address of the user (derived from the source of the UDP packet)
- Lock the shared state (`ServerMap`) to safely add or update the user list.
- Check if the user is already present in the channel by matching both username and address:
  - If not present, add the user to the channel.
  - Print a message about a new user joining.
- Determine the mode we are going to operate on, based on the number of users in the channel:
  - **Only one user in the channel** -> Send back a `MODE RELAY` message, indicating the user is alone and should operate in relay mode.
  - **Multiple users in the channel**
    -> For the newly connected user, send one `MODE DIRECT` message per existing user in the channel, providing each peer's username and address (THIS MIGHT BE CHANGED SOON)
    -> For all existing users send a `MODE DIRECT` message with the new user's information. This informs all clients about the new user peer they can connect to directly.

### 5. Handling DISCONNECT:
- Extract `server ID`, `channel` and `user name` from the message.
- Lock the shared state and remove the user from the channel.
- Print a message about the user leaving the channel.
- If after removal there is exactly one user left in the channel:
  -> Send a `MODE RELAY` message to that user, notifying them they are alone and should switch to relay mode.

### 6. Loop:
The server returns to listening for more UDP messages for an unlimited period of time.

---

## Notes
- The server assumes clients maintain their connection by sending `CONNECT` and `DISCONNECT` messages (This might be changed soon)
- The current way the server operates may be updated/changed (future **Phase 2**)
