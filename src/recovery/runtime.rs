//! CLI adapters. All UDP timers share one loop; CONTROL has one cancellable owner.

use std::{
    io::{self, BufRead},
    net::{SocketAddr, UdpSocket},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::{
    client::Client,
    wire::{Binding, Datagram, Join, MAX_DATAGRAM},
};
use crate::{
    control::client::{ControlWorker, session_worker},
    recovery::nat::{NatKind, detect_nat_kind},
};

struct Runtime {
    state: Arc<Mutex<Client>>,
    socket: Option<UdpSocket>,
    local_port: u16,
    control: Option<ControlWorker>,
    control_binding: Option<Binding>,
    discovery_attempt: Option<super::wire::Attempt>,
    last_error: Option<Instant>,
}

impl Runtime {
    fn state(&self) -> io::Result<std::sync::MutexGuard<'_, Client>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("transport state poisoned"))
    }

    fn report_error(&mut self, error: &dyn std::fmt::Display) {
        let now = Instant::now();
        if self
            .last_error
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
        {
            eprintln!("Transport I/O error: {error}");
            self.last_error = Some(now);
        }
    }

    fn discover(&mut self) -> io::Result<()> {
        let (attempt, server) = {
            let state = self.state()?;
            (state.join.attempt.clone(), state.server)
        };
        if self.discovery_attempt.as_ref() == Some(&attempt) {
            return Ok(());
        }
        self.control.take();
        self.control_binding = None;
        if let Some(socket) = &self.socket {
            let kind = detect_nat_kind(socket, server);
            self.state()?.join.symmetric = kind == NatKind::Symmetric;
            self.discovery_attempt = Some(attempt);
        }
        Ok(())
    }

    fn maintain_control(&mut self, now: Instant) -> io::Result<()> {
        let binding = self.state()?.binding.clone();
        if binding != self.control_binding {
            self.control.take();
            self.control_binding.clone_from(&binding);
        }
        let Some(binding) = binding else {
            return Ok(());
        };
        if self
            .control
            .as_ref()
            .is_some_and(ControlWorker::is_finished)
        {
            self.control.take();
            self.state()?.control_lost(now);
        }
        let due = self.control.is_none()
            && self
                .state()?
                .control_retry
                .as_mut()
                .is_some_and(|retry| retry.due(now));
        if due {
            let mut address = self.state()?.server;
            address.set_port(2133);
            self.control = Some(session_worker(
                format!("ws://{address}"),
                binding,
                self.state.clone(),
            ));
        }
        Ok(())
    }

    fn send(&mut self, output: Vec<(SocketAddr, Datagram)>) -> io::Result<()> {
        if output.iter().any(|(_, message)| {
            !matches!(message, Datagram::Fallback { .. }) && message.encode().is_err()
        }) {
            self.report_error(
                &"datagram exceeds forwarding budget; fanout rejected before sending",
            );
            return Ok(());
        }
        for (address, message) in output {
            if let Datagram::Fallback { session, target } = message {
                if !self
                    .control
                    .as_ref()
                    .is_some_and(|control| control.fallback(session, target))
                {
                    self.state()?.control_lost(Instant::now());
                }
                continue;
            }
            let bytes = match message.encode() {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.report_error(&error);
                    continue;
                }
            };
            let Some(socket) = &self.socket else {
                break;
            };
            match socket.send_to(&bytes, address) {
                Ok(_) => self.state()?.sent(&message),
                Err(error) => self.report_error(&error),
            }
        }
        Ok(())
    }

    fn receive(&mut self) -> io::Result<()> {
        let mut bytes = [0; MAX_DATAGRAM + 1];
        // A busy sender cannot starve lifecycle deadlines or user input.
        for _ in 0..64 {
            let Some(socket) = &self.socket else {
                break;
            };
            match socket.recv_from(&mut bytes) {
                Ok((len, source)) => {
                    if let Some(message) = Datagram::decode(&bytes[..len]) {
                        let output = self.state()?.receive(message, source, Instant::now());
                        self.send(output)?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.report_error(&error);
                    self.socket.take();
                    self.discovery_attempt = None;
                    if self.state()?.binding.is_some() {
                        self.state()?.registration_lost(Instant::now());
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self) -> io::Result<()> {
        let output = self.state()?.tick(Instant::now());
        if self.socket.is_none() && !output.is_empty() {
            match bind(self.local_port) {
                Ok(socket) => self.socket = Some(socket),
                Err(error) => self.report_error(&error),
            }
        }
        self.discover()?;
        // Discovery may have refined NAT classification since the timer fired.
        let output = output
            .into_iter()
            .map(|(address, message)| {
                if matches!(message, Datagram::Connect { .. }) {
                    self.state().map(|state| {
                        (
                            address,
                            Datagram::Connect {
                                join: state.join.clone(),
                            },
                        )
                    })
                } else {
                    Ok((address, message))
                }
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.send(output)?;
        self.maintain_control(Instant::now())?;
        self.receive()
    }

    fn input(&mut self, line: &str) -> io::Result<bool> {
        if line == "/quit" {
            return Ok(false);
        }
        if line == "/retry" {
            self.state()?.recover(Instant::now());
            return Ok(true);
        }
        let result = self.state()?.user_message(line);
        match result {
            Ok(output) => self.send(output)?,
            Err(error) => eprintln!("Cannot send: {error}"),
        }
        Ok(true)
    }
}

fn bind(port: u16) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(("0.0.0.0", port))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// Run the development CLI. Input is bounded and never queued for recovery.
/// # Errors
/// Invalid local binding or poisoned transport state ends the runtime explicitly.
pub fn run_client(join: Join, server: SocketAddr, local_port: u16) -> io::Result<()> {
    if !join.valid() || local_port == 0 {
        return Err(io::Error::other("invalid join or fixed local port"));
    }
    let state = Arc::new(Mutex::new(Client::new(join, server, Instant::now())));
    let mut runtime = Runtime {
        state,
        socket: Some(bind(local_port)?),
        local_port,
        control: None,
        control_binding: None,
        discovery_attempt: None,
        last_error: None,
    };
    let (tx, input) = mpsc::sync_channel(1);
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if line.len() > MAX_DATAGRAM {
                eprintln!("Input exceeds UDP limit");
                continue;
            }
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        runtime.tick()?;
        match input.try_recv() {
            Ok(line) => {
                if !runtime.input(line.trim())? {
                    break;
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        thread::sleep(Duration::from_millis(50));
    }
    let session = runtime
        .state()?
        .binding
        .as_ref()
        .map(|binding| binding.session);
    if let Some(session) = session {
        runtime.send(vec![(server, Datagram::Disconnect { session })])?;
    }
    Ok(())
}
