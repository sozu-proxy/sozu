use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    str::from_utf8,
    time::{Duration, Instant},
};

use crate::BUFFER_SIZE;

/// HTTP/TCP mock client
/// Wrapper over a TCP connection
pub struct Client {
    pub name: String,
    pub address: SocketAddr,
    pub stream: Option<TcpStream>,
    pub request: String,
    pub responses_received: usize,
    pub requests_sent: usize,
}

impl Client {
    pub fn new<S1: Into<String>, S2: Into<String>>(
        name: S1,
        address: SocketAddr,
        request: S2,
    ) -> Self {
        let name = name.into();
        let request = request.into();
        Self {
            name,
            address,
            stream: None,
            request,
            requests_sent: 0,
            responses_received: 0,
        }
    }

    /// Establish a TCP connection with its address,
    /// register the yielded TCP stream, apply timeouts
    pub fn connect(&mut self) {
        let stream = TcpStream::connect(self.address).expect("could not connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("could not set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_millis(100)))
            .expect("could not set write timeout");
        self.stream = Some(stream);
    }

    pub fn disconnect(&mut self) {
        self.stream = None;
    }

    pub fn is_connected(&self) -> bool {
        match &self.stream {
            None => false,
            Some(stream) => match stream.peek(&mut [0]) {
                Ok(1) => {
                    println!("{} still connected", self.name);
                    true
                }
                Ok(_) => {
                    println!("{} disconnected", self.name);
                    false
                }
                Err(e) => {
                    println!("{} check_connection: {e:?}", self.name);
                    true
                }
            },
        }
    }

    /// Write its own request on the TcpStream, returns the number of bytes written
    pub fn send(&mut self) -> Option<usize> {
        match &mut self.stream {
            Some(stream) => match stream.write(self.request.as_bytes()) {
                Ok(0) => {
                    println!("{} sent nothing", self.name);
                    return Some(0);
                }
                Ok(n) => {
                    println!("{} sent {}", self.name, n);
                    self.requests_sent += 1;
                    return Some(n);
                }
                Err(error) => {
                    println!("{} could not send: {}", self.name, error);
                }
            },
            None => {
                println!("{} is not connected", self.name);
            }
        }
        None
    }

    /// Reads data arriving on the TcpStream, parses a UTF-8 string from it
    pub fn receive(&mut self) -> Option<String> {
        match &mut self.stream {
            Some(stream) => {
                let mut buf = [0u8; BUFFER_SIZE];
                match stream.read(&mut buf) {
                    Ok(0) => {
                        println!("{} received nothing", self.name);
                    }
                    Ok(n) => {
                        println!("{} received {}", self.name, n);
                        self.responses_received += 1;
                        return Some(from_utf8(&buf[..n]).unwrap().to_string());
                    }
                    Err(error) => {
                        println!("{} could not receive: {}", self.name, error);
                    }
                }
            }
            None => {
                println!("{} is not connected", self.name);
            }
        }
        None
    }

    /// Loop-read until either the upstream half-closes the TCP stream
    /// (read returns 0 — the canonical end-of-response on a
    /// `Connection: close` default-answer like the redirect templates)
    /// or `deadline` elapses.
    ///
    /// Replaces the single-`receive()` pattern for assertions on small
    /// default-answer payloads. CLAUDE.md: "Always `loop_read_*` when
    /// asserting on TCP responses. A single `read()` sees one segment
    /// under load." Returns the accumulated UTF-8 bytes, or `None` if
    /// the deadline elapsed before any byte was read.
    pub fn receive_until_eof(&mut self, deadline: Duration) -> Option<String> {
        let stream = self.stream.as_mut()?;
        let started = Instant::now();
        let mut acc: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
        loop {
            let mut buf = [0u8; BUFFER_SIZE];
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if started.elapsed() >= deadline {
                        break;
                    }
                    continue;
                }
                Err(_) => break,
            }
            if started.elapsed() >= deadline {
                break;
            }
        }
        if acc.is_empty() {
            return None;
        }
        self.responses_received += 1;
        Some(from_utf8(&acc).ok()?.to_string())
    }

    pub fn set_request<S1: Into<String>>(&mut self, request: S1) {
        self.request = request.into();
    }
}
