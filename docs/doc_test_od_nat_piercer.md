# Testing commands and expected outputs for **Phase 1** `od_nat_piercer`

## 1. Start signaling server

### Command
```bash
cargo run --bin signaling_server
```
### Expected output
Signaling server listening on 0.0.0.0:5000


## 2. Start the first client (**user1**)
**Open a new terminal.**
In the command line you need to add the right arguments for every user. The command follows the next pattern:

```bash
cargo run --bin client -- <signaling_ip> <client_ip> <server_id> <channel_name> <user_name> <local_port>
```
One example to run this command can be:

```bash
cargo run --bin <your_machine_ip> <your_machine_ip> server1 channel1 user1 4000
````

You can discover your machine's ip by running the next command in a terminal (on linux): `ip a`

## Expected Output for the first client

Server response:
 MODE RELAY

You are in RELAY mode (only user in channel)
Relay mode active: waiting for incoming messages...


## 3. Start a second client
**Open a new terminal.**

```bash
cargo run --bin client -- <your_machine_ip> <your_machine_ip> server1 channel1 user2 4002
```

## 4. Start a third client
**Open a new terminal.**

```bash
cargo run --bin client -- <your_machine_ip> <your_machine_ip> server1 channel1 user3 4003
```

## Expected Outputs:
### Output for 3rd client:
Server response:
 MODE DIRECT user1 <your_machine_ip>:4000
Added peer user1 with addr <your_machine_ip>:4000
Received from <your_machine_ip>:5000: MODE DIRECT user2 <your_machine_ip>:4002
Received from <your_machine_ip>:5000: MODE DIRECT user3 <your_machine_ip>:4003
Received from <your_machine_ip>:5000: MODE DIRECT user3 <your_machine_ip>:4003
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Finished hole punching / listening phase.


### Output for 2nd client
Server response:
 MODE DIRECT user1 <your_machine_ip>:4000
Added peer user1 with addr <your_machine_ip>:4000
Received from <your_machine_ip>:5000: MODE DIRECT user2 <your_machine_ip>:4002
Received from <your_machine_ip>:5000: MODE DIRECT user2 <your_machine_ip>:4002
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Received from <your_machine_ip>:5000: MODE DIRECT user3 <your_machine_ip>:4003
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Sent UDP punch to <your_machine_ip>:4000
Finished hole punching / listening phase.


### Output for 1st client
Server response:
 MODE RELAY

You are in RELAY mode (only user in channel)
Relay mode active: waiting for incoming messages...
Received from <your_machine_ip>:5000: MODE DIRECT user2 <your_machine_ip>:4002
Received from <your_machine_ip>:4002: punch
Received from <your_machine_ip>:4002: punch
Received from <your_machine_ip>:4002: punch
Received from <your_machine_ip>:4002: punch
Finished hole punching / listening phase.


## And now, you're server's output should look something like this:
Signaling server listening on 0.0.0.0:5000
User user1 joined server1-channel1 from <your_machine_ip>:4000
User user2 joined server1-channel1 from <your_machine_ip>:4002
User user3 joined server1-channel1 from <your_machine_ip>:4003


#For Phase 1 of this protocol, if you're receiving the same outputs, the server and client are working properly (for now)




