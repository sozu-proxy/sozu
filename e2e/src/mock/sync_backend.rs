use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::{FromRawFd, IntoRawFd},
    str::from_utf8,
    time::Duration,
};

use libc::setsockopt;

use crate::BUFFER_SIZE;

/// A mock backend whose actions are all synchronous (accepting, receiving, responding...)
/// this should help reproductibility by enforcing a strict order on those actions
pub struct Backend {
    pub name: String,
    pub address: SocketAddr,
    pub listener: Option<TcpListener>,
    pub clients: HashMap<usize, TcpStream>,
    pub response: String,
    pub requests_received: usize,
    pub responses_sent: usize,
}

impl Backend {
    pub fn new<S1: Into<String>, S2: Into<String>>(
        name: S1,
        address: SocketAddr,
        response: S2,
    ) -> Self {
        let name = name.into();
        let response = response.into();
        Self {
            name,
            address,
            listener: None,
            clients: HashMap::new(),
            response,
            responses_sent: 0,
            requests_received: 0,
        }
    }

    /// Binds itself to its address, stores the yielded TCP listener
    pub fn connect(&mut self) {
        let listener = TcpListener::bind(self.address).expect("could not bind");
        let timeout = Duration::from_millis(100);
        let timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: timeout.subsec_micros().try_into().unwrap_or(0),
        };
        let listener = unsafe {
            let fd = listener.into_raw_fd();
            setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const libc::timeval as *const _,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
            TcpListener::from_raw_fd(fd)
        };
        self.listener = Some(listener);
        self.clients = HashMap::new();
    }

    pub fn disconnect(&mut self) {
        self.listener = None;
        self.clients = HashMap::new();
    }

    pub fn is_connected(&self, client_id: usize) -> bool {
        match self.clients.get(&client_id) {
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

    /// Tries to accept one connection on a TcpSocket
    /// and registers the resulting TcpStream as a client
    pub fn accept(&mut self, client_id: usize) -> bool {
        if let Some(listener) = &self.listener {
            let stream = listener.accept();
            match stream {
                Ok((stream, _)) => {
                    println!("{} accepted {}", self.name, client_id);
                    stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .expect("could not set read timeout");
                    stream
                        .set_write_timeout(Some(Duration::from_millis(100)))
                        .expect("could not set write timeout");
                    self.clients.insert(client_id, stream);
                    return true;
                }
                Err(error) => {
                    println!("{} accept error: {:?}", self.name, error);
                }
            }
        }
        false
    }

    /// Writes its own response as bytes on the TcpStream corresponding to a specific client,
    /// returns the number of bytes written
    pub fn send(&mut self, client_id: usize) -> Option<usize> {
        match self.clients.get_mut(&client_id) {
            Some(stream) => match stream.write(self.response.as_bytes()) {
                Ok(0) => {
                    println!("{} sent nothing", self.name);
                    return Some(0);
                }
                Ok(n) => {
                    println!("{} sent {} to {}", self.name, n, client_id);
                    self.responses_sent += 1;
                    return Some(n);
                }
                Err(error) => {
                    println!(
                        "{} could not respond to {}: {}",
                        self.name, client_id, error
                    );
                }
            },
            None => {
                println!("no client with id {} on backend {}", client_id, self.name);
            }
        }
        None
    }

    /// Reads data arriving on the TcpStream of a specifc client, parses a UTF-8 string from it
    pub fn receive(&mut self, client_id: usize) -> Option<String> {
        match self.clients.get_mut(&client_id) {
            Some(stream) => {
                let mut buf = [0u8; BUFFER_SIZE];
                match stream.read(&mut buf) {
                    Ok(0) => {
                        println!("{} received nothing from {}", self.name, client_id);
                    }
                    Ok(n) => {
                        println!("{} received {} from {}", self.name, n, client_id);
                        self.requests_received += 1;
                        return Some(from_utf8(&buf[..n]).unwrap().to_string());
                    }
                    Err(error) => {
                        println!(
                            "{} could not receive for {}: {}",
                            self.name, client_id, error
                        );
                    }
                }
            }
            None => {
                println!("no client with id {} on backend {}", client_id, self.name);
            }
        }
        None
    }

    /// Shutdown the connection to a client
    pub fn close(&mut self, client_id: usize) -> bool {
        match self.clients.get_mut(&client_id) {
            Some(stream) => match stream.shutdown(std::net::Shutdown::Both) {
                Ok(()) => {
                    println!("{} closed connection with {}", self.name, client_id);
                    return true;
                }
                Err(error) => {
                    println!(
                        "{} could not close connection with {}: {}",
                        self.name, client_id, error
                    );
                }
            },
            None => {
                println!("no client with id {} on backend {}", client_id, self.name);
            }
        }
        false
    }

    pub fn set_response<S1: Into<String>>(&mut self, response: S1) {
        self.response = response.into();
    }
}
