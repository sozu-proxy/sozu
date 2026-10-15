//! End-to-end tests for backends reached over UNIX domain sockets
//! (`AddBackend::unix_path`, sozu-proxy/sozu#152).
//!
//! Each backend's identity `address` is a reserved loopback port nobody
//! listens on: a proxy that dialed it over TCP would be refused, so a
//! response proves the bytes went through the UNIX socket.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::net::UnixListener,
    path::PathBuf,
    thread::{self, JoinHandle},
    time::Duration,
};

use sozu_command_lib::proto::command::{AddBackend, request::RequestType};

use crate::{
    http_utils::{http_ok_response, http_request},
    mock::client::Client,
    sozu::worker::Worker,
    tests::{
        State, repeat_until_error_or, setup_test,
        tcp_tests::setup_tcp_test,
        tests::{create_local_address, create_unbound_local_address},
    },
};

const DEADLINE: Duration = Duration::from_secs(5);

/// A filesystem socket path no other test uses, removed when dropped.
struct SocketPath(PathBuf);

impl SocketPath {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sozu-e2e-{tag}-{}-{}.sock",
            std::process::id(),
            create_unbound_local_address().port()
        ));
        let _ = std::fs::remove_file(&path);
        SocketPath(path)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
/// An abstract-namespace name no other test uses.
fn abstract_name(tag: &str) -> String {
    format!(
        "sozu-e2e-{tag}-{}-{}",
        std::process::id(),
        create_unbound_local_address().port()
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_abstract(name: &str) -> UnixListener {
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .expect("a short abstract name fits a sockaddr_un");
    UnixListener::bind_addr(&address).expect("bind the abstract backend socket")
}

/// Accept one connection on `listener`, read one HTTP/1 request head, answer
/// `response`, and hand back what was read.
fn serve_one_http(listener: UnixListener, response: String) -> JoinHandle<String> {
    thread::spawn(move || {
        let (mut stream, _) = listener
            .accept()
            .expect("sozu connects to the unix backend");
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&request).into_owned()
    })
}

/// Accept one connection on `listener` and echo `len` bytes back.
fn echo_one(listener: UnixListener, len: usize) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let (mut stream, _) = listener
            .accept()
            .expect("sozu connects to the unix backend");
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        let mut received = vec![0u8; len];
        stream
            .read_exact(&mut received)
            .expect("read the relayed bytes");
        stream.write_all(&received).unwrap();
        received
    })
}

fn unix_backend(address: SocketAddr, unix_path: String) -> AddBackend {
    AddBackend {
        unix_path: Some(unix_path),
        ..Worker::default_backend("cluster_0", "cluster_0-unix", address, None)
    }
}

/// An HTTP request reaches the backend listening on `listener`, dialed
/// through `unix_path`, and its response comes back to the client.
fn try_http_unix_backend(name: &str, listener: UnixListener, unix_path: String) -> State {
    let front_address = create_local_address();
    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, _) = setup_test(name, config, listeners, state, front_address, 0, false);
    worker.send_proxy_request_type(RequestType::AddBackend(unix_backend(
        create_unbound_local_address(),
        unix_path,
    )));
    worker.read_to_last();

    let backend = serve_one_http(listener, http_ok_response("pong-unix"));
    let mut client = Client::new(
        "client",
        front_address,
        http_request("GET", "/api", "ping", "localhost"),
    );
    client.connect();
    client.send();
    let response = client.receive_response(DEADLINE);
    let request = backend.join().expect("the backend thread completes");

    worker.soft_stop();
    worker.wait_for_server_stop();

    println!("backend read: {request:?}, client read: {response:?}");
    match response {
        Some(response) if response.contains("pong-unix") && request.starts_with("GET /api") => {
            State::Success
        }
        _ => State::Fail,
    }
}

#[test]
fn test_http_unix_path_backend() {
    assert_eq!(
        repeat_until_error_or(3, "HTTP over a filesystem UNIX backend", || {
            let path = SocketPath::new("http");
            let listener = UnixListener::bind(&path.0).expect("bind the backend socket");
            try_http_unix_backend("UNIX-HTTP", listener, path.0.to_str().unwrap().to_owned())
        }),
        State::Success
    );
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn test_http_abstract_unix_backend() {
    assert_eq!(
        repeat_until_error_or(3, "HTTP over an abstract UNIX backend", || {
            let name = abstract_name("http");
            let listener = bind_abstract(&name);
            try_http_unix_backend("UNIX-HTTP-ABSTRACT", listener, format!("@{name}"))
        }),
        State::Success
    );
}

/// TCP passthrough relays both directions through a UNIX backend.
fn try_tcp_unix_backend() -> State {
    let path = SocketPath::new("tcp");
    let listener = UnixListener::bind(&path.0).expect("bind the backend socket");
    let (mut worker, _, front_address) = setup_tcp_test("UNIX-TCP", 0);
    worker.send_proxy_request_type(RequestType::AddBackend(unix_backend(
        create_unbound_local_address(),
        path.0.to_str().unwrap().to_owned(),
    )));
    worker.read_to_last();

    let payload = b"raw bytes through a unix socket".to_vec();
    let backend = echo_one(listener, payload.len());
    let mut client = TcpStream::connect(front_address).expect("connect to sozu");
    client.set_read_timeout(Some(DEADLINE)).unwrap();
    client.write_all(&payload).unwrap();
    let mut echoed = vec![0u8; payload.len()];
    let read = client.read_exact(&mut echoed);
    let received = backend.join().expect("the backend thread completes");

    worker.soft_stop();
    worker.wait_for_server_stop();

    if read.is_ok() && echoed == payload && received == payload {
        State::Success
    } else {
        println!("read: {read:?}, echoed: {echoed:?}, backend received: {received:?}");
        State::Fail
    }
}

#[test]
fn test_tcp_unix_path_backend() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "TCP over a filesystem UNIX backend",
            try_tcp_unix_backend
        ),
        State::Success
    );
}
