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

    /// Reset the connection: `SO_LINGER` zero, then close, so the peer sees
    /// a client that went away rather than one that half-closed after its
    /// request (which sozu still answers).
    pub fn reset(&mut self) {
        let Some(stream) = self.stream.take() else {
            return;
        };
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: `linger` is a valid `libc::linger` and `stream` owns the
        // descriptor for the duration of the call.
        let rc = unsafe {
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&stream),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &linger as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "SO_LINGER zero must set");
        drop(stream);
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

    /// Read exactly ONE `Content-Length`-framed HTTP/1 response, however many
    /// socket reads it takes, and count it only once it is whole.
    ///
    /// `receive` counts one response per `read()`, which holds only when the
    /// whole response lands in a single segment within the 100 ms socket
    /// timeout `connect` installs. Under load neither is guaranteed: a
    /// response later than 100 ms is not counted at all, and one split across
    /// two segments is counted twice. Returns `None`, and counts nothing, when
    /// `deadline` elapses, the peer closes, or the bytes read are not exactly
    /// one response.
    pub fn receive_response(&mut self, deadline: Duration) -> Option<String> {
        let stream = self.stream.as_mut()?;
        let response = read_one_http_response(stream, deadline)?;
        self.responses_received += 1;
        Some(response)
    }

    pub fn set_request<S1: Into<String>>(&mut self, request: S1) {
        self.request = request.into();
    }
}

/// Total length of the first response in `bytes`, once its head is complete:
/// the head, the blank line and `Content-Length` body bytes. `None` while the
/// head is still arriving or when it carries no parsable `Content-Length`.
fn http_response_len(bytes: &[u8]) -> Option<usize> {
    let head_end = bytes.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let head = from_utf8(&bytes[..head_end]).ok()?;
    let body_len = head.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })?;
    Some(head_end + body_len)
}

/// Read from `reader` until it holds one complete `Content-Length`-framed
/// response, the peer closes, or `deadline` elapses. The caller has exactly
/// one request outstanding, so any byte past that response is a framing error
/// and fails the read rather than being folded into it.
fn read_one_http_response(reader: &mut impl Read, deadline: Duration) -> Option<String> {
    let started = Instant::now();
    let mut acc: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
    let mut buf = [0u8; BUFFER_SIZE];
    loop {
        if let Some(total) = http_response_len(&acc) {
            if acc.len() == total {
                return from_utf8(&acc).ok().map(str::to_owned);
            }
            if acc.len() > total {
                println!(
                    "read {} bytes, past the {total}-byte response: not exactly one response",
                    acc.len()
                );
                return None;
            }
        }
        if started.elapsed() >= deadline {
            println!(
                "no complete response within {deadline:?} ({} bytes read)",
                acc.len()
            );
            return None;
        }
        match reader.read(&mut buf) {
            Ok(0) => {
                println!(
                    "peer closed after {} bytes, before a complete response",
                    acc.len()
                );
                return None;
            }
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => {
                println!("could not receive: {error}");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io};

    use super::*;

    /// A reader that replays a fixed script of read outcomes, one per call,
    /// then reports `WouldBlock` forever: the shape of a socket under a read
    /// timeout, without a socket or a clock to race.
    struct ScriptedReader(VecDeque<io::Result<Vec<u8>>>);

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Some(Err(error)) => Err(error),
                None => Err(io::ErrorKind::WouldBlock.into()),
            }
        }
    }

    const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\npong0";

    #[test]
    fn a_response_split_across_reads_and_a_timeout_is_read_whole_once() {
        let (head, body) = RESPONSE.split_at(RESPONSE.len() - 3);
        let mut reader = ScriptedReader(VecDeque::from([
            Err(io::ErrorKind::WouldBlock.into()),
            Ok(head.as_bytes().to_vec()),
            Err(io::ErrorKind::TimedOut.into()),
            Ok(body.as_bytes().to_vec()),
        ]));
        assert_eq!(
            read_one_http_response(&mut reader, Duration::from_secs(5)).as_deref(),
            Some(RESPONSE)
        );
    }

    #[test]
    fn bytes_past_one_response_are_not_counted_as_it() {
        let two = format!("{RESPONSE}{RESPONSE}");
        let mut reader = ScriptedReader(VecDeque::from([Ok(two.into_bytes())]));
        assert_eq!(
            read_one_http_response(&mut reader, Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn an_incomplete_response_is_not_counted() {
        let mut reader = ScriptedReader(VecDeque::from([
            Ok(RESPONSE.as_bytes()[..RESPONSE.len() - 1].to_vec()),
            Ok(Vec::new()),
        ]));
        assert_eq!(
            read_one_http_response(&mut reader, Duration::from_secs(5)),
            None
        );
    }
}
