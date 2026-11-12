//! End-to-end regression guards for the backend connection of a request a
//! timeout answered.
//!
//! On a listener whose `back_timeout` is longer than its `front_timeout`, a
//! request whose backend is slow to answer meets the frontend timeout first:
//! the frontend-timeout branch of `Mux::timeout_inner`
//! (`lib/src/protocol/mux/mod.rs`) answers it 504 and, on an H2 frontend, the
//! connection lives on. Before the fix that branch never ended the request on
//! its backend: the H1 backend connection kept naming the stream slot, the
//! client's next request reused the slot, and the answer the first backend
//! sent at last was delivered on the new stream.
//!
//! On a default listener the backend timeout comes first, and the
//! backend-token branch of the same function answers. Before the fix it
//! installed the 504 and only then ended the request on its backend, so
//! `ConnectionH1::end_stream` (`lib/src/protocol/mux/h1.rs`) read the 504 as
//! the backend's own complete response and pooled the connection: the
//! client's next request was written to it, behind a request the backend had
//! not answered yet, and read that request's answer.
//!
//! * [`test_h2_late_backend_answer_after_frontend_timeout_stays_off_the_next_request`]
//! * [`test_h2_late_backend_answer_after_backend_timeout_stays_off_the_next_request`]

use std::{
    collections::{HashSet, VecDeque},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{Duration, Instant},
};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, AddCertificate, CertificateAndKey, ListenerType, RequestHttpFrontend,
        SocketAddress, request::RequestType,
    },
};

use super::h2_utils::{
    H2_FLAG_END_STREAM, H2_FRAME_DATA, H2_FRAME_GOAWAY, H2_FRAME_HEADERS, H2_FRAME_RST_STREAM,
    H2Frame, advance_one_frame, decode_status, h2_handshake, raw_h2_connection, verify_sozu_alive,
};
use crate::{
    sozu::worker::Worker,
    tests::{State, provide_port, provide_unbound_port, repeat_until_error_or},
};

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
type Frame = (u8, u8, u32, Vec<u8>);

/// How long one wait on the frontend or on the backend may last.
const DEADLINE: Duration = Duration::from_secs(10);

/// What the scripted backend reports to the test.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendEvent {
    /// Connection `conn` received a whole request head for `path`.
    Received { conn: usize, path: String },
    /// Connection `conn` answered the request for `path`.
    Answered { conn: usize, path: String },
    /// Sōzu closed connection `conn`.
    Closed { conn: usize },
}

/// An HTTP/1.1 backend that answers a request only once the test released
/// its path, in arrival order on each connection, with the body
/// `answer:<path>`.
struct ScriptedBackend {
    released: Arc<Mutex<HashSet<String>>>,
    events: Receiver<BackendEvent>,
    /// Every event received so far: a wait never drops the events it was
    /// not waiting for.
    seen: Vec<BackendEvent>,
}

impl ScriptedBackend {
    fn spawn(address: SocketAddr) -> Self {
        let listener = TcpListener::bind(address).expect("bind the scripted backend");
        let released = Arc::new(Mutex::new(HashSet::new()));
        let (sender, events) = channel();
        let shared = released.clone();
        thread::spawn(move || {
            for (conn, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { break };
                let released = shared.clone();
                let sender = sender.clone();
                thread::spawn(move || serve_connection(conn, stream, released, sender));
            }
        });
        Self {
            released,
            events,
            seen: Vec::new(),
        }
    }

    /// Let the backend answer the request for `path`.
    fn release(&self, path: &str) {
        self.released
            .lock()
            .expect("the release set is never poisoned")
            .insert(path.to_owned());
    }

    /// The first event matching `wanted`, already seen or received within
    /// [`DEADLINE`].
    fn wait_for(&mut self, wanted: impl Fn(&BackendEvent) -> bool) -> Option<BackendEvent> {
        let start = Instant::now();
        loop {
            if let Some(index) = self.seen.iter().position(&wanted) {
                return self.seen.get(index).cloned();
            }
            let left = DEADLINE.checked_sub(start.elapsed())?;
            self.seen.push(self.events.recv_timeout(left).ok()?);
        }
    }
}

/// Serve one backend connection until Sōzu closes it or [`DEADLINE`] passes
/// with nothing to do.
fn serve_connection(
    conn: usize,
    mut stream: TcpStream,
    released: Arc<Mutex<HashSet<String>>>,
    events: Sender<BackendEvent>,
) {
    // Each read waits on the socket at most this long, so the loop waits on
    // Sōzu or on the test's release, never on a sleep.
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set the backend read timeout");
    let mut carry: Vec<u8> = Vec::new();
    let mut pending: VecDeque<String> = VecDeque::new();
    let mut buf = [0u8; 4096];
    let mut idle_since = Instant::now();
    while idle_since.elapsed() < 3 * DEADLINE {
        while let Some(end) = carry.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&carry[..end]).into_owned();
            carry.drain(..end + 4);
            let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
            let _ = events.send(BackendEvent::Received {
                conn,
                path: path.clone(),
            });
            pending.push_back(path);
        }
        let next_released = pending.front().is_some_and(|path| {
            released
                .lock()
                .expect("the release set is never poisoned")
                .contains(path)
        });
        if next_released && let Some(path) = pending.pop_front() {
            let body = format!("answer:{path}");
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            if stream.write_all(answer.as_bytes()).is_ok() {
                let _ = events.send(BackendEvent::Answered { conn, path });
            }
            idle_since = Instant::now();
            continue;
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                let _ = events.send(BackendEvent::Closed { conn });
                return;
            }
            Ok(size) => {
                carry.extend_from_slice(&buf[..size]);
                idle_since = Instant::now();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                let _ = events.send(BackendEvent::Closed { conn });
                return;
            }
        }
    }
}

/// An HTTPS listener with the given timeouts, one cluster, and one backend
/// address the caller binds.
fn setup_listener_with_timeouts(
    name: &str,
    front_timeout: u32,
    back_timeout: u32,
) -> (Worker, u16, SocketAddr) {
    let front_port = provide_port();
    let front_address = SocketAddress::new_v4(127, 0, 0, 1, front_port);

    let (config, listeners, state) = Worker::empty_https_config(front_address.clone().into());
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    let mut listener_builder = ListenerBuilder::new_https(front_address.clone());
    listener_builder.with_front_timeout(Some(front_timeout));
    listener_builder.with_back_timeout(Some(back_timeout));
    worker.send_proxy_request_type(RequestType::AddHttpsListener(
        listener_builder.to_tls(None).unwrap(),
    ));
    worker.send_proxy_request_type(RequestType::ActivateListener(ActivateListener {
        interface: None,
        address: front_address.clone(),
        proxy: ListenerType::Https.into(),
        from_scm: false,
    }));
    worker.send_proxy_request_type(RequestType::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request_type(RequestType::AddHttpsFrontend(RequestHttpFrontend {
        hostname: String::from("localhost"),
        ..Worker::default_http_frontend("cluster_0", front_address.clone().into())
    }));
    worker.send_proxy_request_type(RequestType::AddCertificate(AddCertificate {
        address: front_address,
        certificate: CertificateAndKey {
            certificate: String::from(include_str!("../../../lib/assets/local-certificate.pem")),
            key: String::from(include_str!("../../../lib/assets/local-key.pem")),
            certificate_chain: vec![],
            versions: vec![],
            names: vec![],
        },
        expired_at: None,
    }));

    let back_port = provide_unbound_port();
    let back_address: SocketAddr = format!("127.0.0.1:{back_port}").parse().unwrap();
    worker.send_proxy_request_type(RequestType::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0".to_owned(),
        back_address,
        None,
    )));
    worker.read_to_last();
    (worker, front_port, back_address)
}

/// `:method GET`, `:scheme https`, `:path path` and `:authority localhost`,
/// the literals without indexing: no HPACK dynamic table state.
fn get_block(path: &str) -> Vec<u8> {
    let mut block = vec![0x82, 0x87, 0x04, path.len() as u8];
    block.extend_from_slice(path.as_bytes());
    block.extend_from_slice(&[0x01, 0x09]);
    block.extend_from_slice(b"localhost");
    block
}

/// Frames read from the client connection, with the bytes of a frame not
/// read whole yet.
struct FrameReader {
    carry: Vec<u8>,
    frames: Vec<Frame>,
}

impl FrameReader {
    fn new(tls: &mut TlsStream) -> Self {
        // Each read waits on the socket at most this long, so the loop below
        // waits on the frontend, never on a sleep.
        tls.sock
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set the client read timeout");
        Self {
            carry: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Read until `stream_id` ended, the connection ends, or `deadline`
    /// passes. Answers whether the stream ended.
    fn read_until_ended(
        &mut self,
        tls: &mut TlsStream,
        stream_id: u32,
        deadline: Duration,
    ) -> bool {
        let start = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];
        while !self.ended(stream_id) && start.elapsed() < deadline {
            match tls.read(&mut buf) {
                Ok(0) => break,
                Ok(size) => {
                    self.carry.extend_from_slice(&buf[..size]);
                    while let Some(frame) = advance_one_frame(&mut self.carry) {
                        self.frames.push(frame);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        self.ended(stream_id)
    }

    /// Whether `stream_id` ended, or the connection did.
    fn ended(&self, stream_id: u32) -> bool {
        self.frames.iter().any(|(kind, flags, id, _)| {
            *kind == H2_FRAME_GOAWAY
                || (*id == stream_id
                    && (*kind == H2_FRAME_RST_STREAM
                        || ((*kind == H2_FRAME_HEADERS || *kind == H2_FRAME_DATA)
                            && flags & H2_FLAG_END_STREAM != 0)))
        })
    }

    /// The `:status` of the first HEADERS frame on `stream_id`.
    fn status(&self, stream_id: u32) -> Option<u16> {
        self.frames
            .iter()
            .find(|(kind, _, id, _)| *kind == H2_FRAME_HEADERS && *id == stream_id)
            .and_then(|(_, _, _, payload)| decode_status(payload))
    }

    /// The DATA payload received on `stream_id`.
    fn body(&self, stream_id: u32) -> String {
        let bytes: Vec<u8> = self
            .frames
            .iter()
            .filter(|(kind, _, id, _)| *kind == H2_FRAME_DATA && *id == stream_id)
            .flat_map(|(_, _, _, payload)| payload.iter().copied())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// What the client read on its second stream, after its first request was
/// answered 504 by a timeout and the first backend answered late.
struct Outcome {
    first_status: Option<u16>,
    second_status: Option<u16>,
    second_body: String,
    infra_ok: bool,
}

/// One connection, two requests. `/slow` on stream 1 gets no backend answer
/// and is answered 504 by whichever timeout the listener reaches first.
/// `/held` follows on stream 3 once the 504 is read. Then the backend
/// answers `/slow`, and only after that `/held`.
fn late_answer_after_a_timeout(name: &str, front_timeout: u32, back_timeout: u32) -> Outcome {
    let (mut worker, front_port, back_address) =
        setup_listener_with_timeouts(name, front_timeout, back_timeout);
    let mut backend = ScriptedBackend::spawn(back_address);

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    let mut reader = FrameReader::new(&mut tls);

    tls.write_all(&H2Frame::headers(1, get_block("/slow"), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    let slow = backend
        .wait_for(|event| matches!(event, BackendEvent::Received { path, .. } if path == "/slow"));
    let slow_conn = match slow {
        Some(BackendEvent::Received { conn, .. }) => conn,
        other => panic!("premise: the backend received /slow, got {other:?}"),
    };
    // The timeout answers stream 1.
    reader.read_until_ended(&mut tls, 1, DEADLINE);
    let first_status = reader.status(1);

    tls.write_all(&H2Frame::headers(3, get_block("/held"), true, true).encode())
        .unwrap();
    tls.flush().unwrap();
    let held = backend
        .wait_for(|event| matches!(event, BackendEvent::Received { path, .. } if path == "/held"));
    assert!(
        held.is_some(),
        "premise: the backend received /held before any answer was released"
    );

    // The backend answers the first request at last, unless Sōzu already
    // closed the connection that carried it.
    backend.release("/slow");
    let late = backend.wait_for(|event| {
        matches!(event, BackendEvent::Answered { path, .. } if path == "/slow")
            || *event == BackendEvent::Closed { conn: slow_conn }
    });
    if matches!(late, Some(BackendEvent::Answered { .. })) {
        // The late answer is on the wire to Sōzu: give it the time to reach
        // the client, on whichever stream Sōzu puts it.
        reader.read_until_ended(&mut tls, 3, Duration::from_secs(2));
    }
    backend.release("/held");
    reader.read_until_ended(&mut tls, 3, DEADLINE);

    let outcome_status = reader.status(3);
    let second_body = reader.body(3);
    println!(
        "{name}: stream 1 status={first_status:?}, late={late:?}, stream 3 \
         status={outcome_status:?} body={second_body:?}"
    );

    drop(tls);
    let still_alive = verify_sozu_alive(front_port);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();
    Outcome {
        first_status,
        second_status: outcome_status,
        second_body,
        infra_ok: still_alive && stopped,
    }
}

fn try_late_backend_answer_after_frontend_timeout() -> State {
    // `front_timeout` 2 s, `back_timeout` 20 s: the frontend timeout answers
    // the first request, 18 s before its backend's own timeout.
    let outcome = late_answer_after_a_timeout("H2-FRONT-TIMEOUT-DETACH", 2, 20);
    if outcome.infra_ok
        && outcome.first_status == Some(504)
        && outcome.second_status == Some(200)
        && outcome.second_body == "answer:/held"
    {
        State::Success
    } else {
        println!(
            "FAIL: stream 1 status={:?} (want 504), stream 3 status={:?} body={:?} \
             (want 200, \"answer:/held\"), infra_ok={}",
            outcome.first_status, outcome.second_status, outcome.second_body, outcome.infra_ok
        );
        State::Fail
    }
}

/// A request answered 504 by the frontend timeout leaves nothing behind on
/// its backend: the client's next request, on the same H2 connection, reads
/// its own backend's answer and never the first request's late one.
///
/// TO SEE THIS RED: in the `StreamState::Linked` arm of the
/// frontend-timeout branch of `Mux::timeout_inner`
/// (`lib/src/protocol/mux/mod.rs`), drop the `end_stream` call that
/// precedes `set_default_answer`. Stream 3 then reads `answer:/slow`.
#[test]
fn test_h2_late_backend_answer_after_frontend_timeout_stays_off_the_next_request() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: a backend's late answer after a frontend-timeout 504 must not reach the next request",
            try_late_backend_answer_after_frontend_timeout
        ),
        State::Success
    );
}

fn try_late_backend_answer_after_backend_timeout() -> State {
    // `back_timeout` 2 s, `front_timeout` 60 s, the order of a default
    // listener: the backend timeout answers the first request.
    let outcome = late_answer_after_a_timeout("H2-BACK-TIMEOUT-DETACH", 60, 2);
    if outcome.infra_ok
        && outcome.first_status == Some(504)
        && outcome.second_status == Some(200)
        && outcome.second_body == "answer:/held"
    {
        State::Success
    } else {
        println!(
            "FAIL: stream 1 status={:?} (want 504), stream 3 status={:?} body={:?} \
             (want 200, \"answer:/held\"), infra_ok={}",
            outcome.first_status, outcome.second_status, outcome.second_body, outcome.infra_ok
        );
        State::Fail
    }
}

/// A request answered 504 by the backend timeout closes the backend
/// connection that still owes its answer: the client's next request, on the
/// same H2 connection, goes to a connection of its own and reads its own
/// answer, never the first request's late one.
///
/// TO SEE THIS RED: in the backend-token branch of `Mux::timeout_inner`
/// (`lib/src/protocol/mux/mod.rs`), move the `end_stream` call from ahead of
/// the arms of the loop to below them, after `set_default_answer`. Stream 3
/// then reads `answer:/slow`.
#[test]
fn test_h2_late_backend_answer_after_backend_timeout_stays_off_the_next_request() {
    assert_eq!(
        repeat_until_error_or(
            1,
            "H2: a backend's late answer after a backend-timeout 504 must not reach the next request",
            try_late_backend_answer_after_backend_timeout
        ),
        State::Success
    );
}
