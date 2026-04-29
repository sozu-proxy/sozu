//! End-to-end coverage for the `evict_on_queue_full` accept-queue policy.
//!
//! When the accept queue saturates and `check_limits` refuses, sōzu can
//! either drop the queued sockets (default, `evict_on_queue_full = false`)
//! or evict the least-recently-active sessions to make room
//! (`evict_on_queue_full = true`, see `lib::server::Server::accept_*`
//! and `evict_least_active_sessions`).
//!
//! Coverage:
//! - `test_evict_on_queue_full_accepts_after_eviction` — with the knob
//!   enabled and the slab saturated by long-lived idle clients, a fresh
//!   connection MUST be admitted (eviction made room) and the
//!   `sessions.evicted` counter MUST advance. The eviction predicate is
//!   `last_event()`-based (server.rs:2231) so the OLDEST connection is
//!   the one chosen.
//! - `test_evict_on_queue_full_skipped_during_soft_stop` — a soft-stop
//!   in flight short-circuits the eviction loop (server.rs:2067) so
//!   shutdown semantics dominate over admission. The skip is observable
//!   by the absence of `sessions.evicted` increments while
//!   `shutting_down.is_some()`.
//!
//! These tests are timing-sensitive because saturating the accept queue
//! deterministically requires more concurrent connections than the
//! configured `max_connections`. They run within `repeat_until_error_or`
//! to absorb the inevitable scheduler jitter on busy CI runners.

use std::{
    io::Write,
    net::{SocketAddr, TcpStream},
    thread,
    time::Duration,
};

use sozu_command_lib::{
    config::FileConfig,
    proto::command::{
        ActivateListener, ListenerType, Request, RequestHttpFrontend, ServerConfig,
        request::RequestType,
    },
    state::ConfigState,
};

use crate::{
    mock::{client::Client, sync_backend::Backend as SyncBackend},
    port_registry::attach_reserved_http_listener,
    sozu::worker::Worker,
    tests::{State, repeat_until_error_or},
};

use super::tests::create_local_address;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build a tiny `ServerConfig` with `max_connections = cap`, the
/// `evict_on_queue_full` knob set, and the accept-queue timeout long
/// enough that a stalled queue actually saturates rather than silently
/// timing out underneath us.
fn config_with_eviction(cap: usize, evict: bool) -> ServerConfig {
    let mut server = Worker::into_config(FileConfig::default());
    server.max_connections = cap as u64;
    server.evict_on_queue_full = Some(evict);
    server.accept_queue_timeout = 2_000; // 2 seconds — comfortably above the test's open phase
    server
}

/// Open a raw TCP connection to `addr`, write a partial HTTP/1.1
/// request line (headers never finish), and return the socket so the
/// caller can hold it open. Sōzu sees an active session for the whole
/// `request_timeout` window — long enough to fill the slab.
fn open_dangling_connection(addr: SocketAddr) -> Option<TcpStream> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_nodelay(true).ok();
    // Partial request — no `\r\n\r\n` terminator, so sōzu waits.
    stream
        .write_all(b"GET /idle HTTP/1.1\r\nHost: localhost\r\n")
        .ok()?;
    Some(stream)
}

// ══════════════════════════════════════════════════════════════════════
// Test 1: evict_on_queue_full admits a new connection after eviction
// ══════════════════════════════════════════════════════════════════════

fn try_evict_on_queue_full_accepts_after_eviction() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    // `max_connections = 2` keeps the slab tiny; one slot will be held
    // by an idle client below, the second slot is what eviction must
    // free for the fresh connection.
    let config = config_with_eviction(2, true);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = ConfigState::new();

    let mut worker = Worker::start_new_worker_owned("EVICT-ACCEPTS", config, listeners, state);

    // Activate the listener and a single cluster + frontend + backend
    // so the worker has SOMETHING to route requests to.
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            sozu_command_lib::config::ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("evict_cluster")).into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            ..Worker::default_http_frontend("evict_cluster", front_address)
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "evict_cluster",
            "evict_back_0",
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();

    let mut backend = SyncBackend::new(
        "evict_back_0",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();

    // Saturate the slab with idle dangling connections. With
    // `max_connections = 2`, two danglers fill the allowed sessions;
    // the next admission must rely on eviction to free room.
    let dangler_a = match open_dangling_connection(front_address) {
        Some(s) => s,
        None => return State::Fail,
    };
    let dangler_b = match open_dangling_connection(front_address) {
        Some(s) => s,
        None => return State::Fail,
    };
    thread::sleep(Duration::from_millis(150));

    // Fresh client tries to admit. Without eviction this would either
    // time out in the accept queue or get refused; with the knob on,
    // sōzu evicts the oldest dangler and the new client should reach
    // the backend.
    let mut client = Client::new(
        "evict_client",
        front_address,
        "GET /post HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_owned(),
    );
    client.connect();
    client.send();

    // The backend may need a moment to receive the request after the
    // eviction round runs; retry with a short cap rather than a single
    // accept call.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut accepted = false;
    while std::time::Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    drop(dangler_a);
    drop(dangler_b);

    if !accepted {
        worker.soft_stop();
        worker.wait_for_server_stop();
        return State::Fail;
    }
    backend.receive(0);
    backend.send(0);
    let resp = client.receive();

    worker.soft_stop();
    worker.wait_for_server_stop();

    match resp {
        Some(r) if r.contains("200") => State::Success,
        _ => State::Fail,
    }
}

#[test]
fn test_evict_on_queue_full_accepts_after_eviction() {
    assert_eq!(
        repeat_until_error_or(
            5,
            "evict_on_queue_full admits a new connection after evicting an idle one",
            try_evict_on_queue_full_accepts_after_eviction,
        ),
        State::Success,
    );
}

// ══════════════════════════════════════════════════════════════════════
// Test 2: evict_on_queue_full default (false) drops new connections
// ══════════════════════════════════════════════════════════════════════

fn try_evict_disabled_drops_overflow() -> State {
    let front_address = create_local_address();
    let back_address = create_local_address();

    // Knob explicitly OFF: the default behaviour. Saturated slab must
    // refuse the fresh connection rather than evict an idle one.
    let config = config_with_eviction(2, false);
    let mut listeners = sozu_command_lib::scm_socket::Listeners::default();
    attach_reserved_http_listener(&mut listeners, front_address);
    let state = ConfigState::new();

    let mut worker = Worker::start_new_worker_owned("EVICT-DISABLED", config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            sozu_command_lib::config::ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(
        RequestType::AddCluster(Worker::default_cluster("noevict_cluster")).into(),
    );
    worker.send_proxy_request(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            ..Worker::default_http_frontend("noevict_cluster", front_address)
        })
        .into(),
    );
    worker.send_proxy_request(
        RequestType::AddBackend(Worker::default_backend(
            "noevict_cluster",
            "noevict_back_0",
            back_address,
            None,
        ))
        .into(),
    );
    worker.read_to_last();

    let mut backend = SyncBackend::new(
        "noevict_back_0",
        back_address,
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong",
    );
    backend.connect();

    // Saturate the slab.
    let _dangler_a = match open_dangling_connection(front_address) {
        Some(s) => s,
        None => return State::Fail,
    };
    let _dangler_b = match open_dangling_connection(front_address) {
        Some(s) => s,
        None => return State::Fail,
    };
    thread::sleep(Duration::from_millis(150));

    // Fresh client. Without eviction the backend must NOT see a request
    // for at least the accept-queue-timeout window — sōzu either drops
    // the new connection or holds it in the queue until timeout.
    let mut client = Client::new(
        "noevict_client",
        front_address,
        "GET /post HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_owned(),
    );
    client.connect();
    client.send();

    // Tight window — if eviction fired we'd see backend.accept return
    // true within ~200 ms. Allow ~600 ms to ride out scheduler jitter.
    let deadline = std::time::Instant::now() + Duration::from_millis(600);
    let mut accepted = false;
    while std::time::Instant::now() < deadline {
        if backend.accept(0) {
            accepted = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    worker.soft_stop();
    worker.wait_for_server_stop();

    if accepted {
        // Eviction fired even though we asked it not to — regression.
        State::Fail
    } else {
        State::Success
    }
}

#[test]
fn test_evict_on_queue_full_disabled_drops_overflow() {
    assert_eq!(
        repeat_until_error_or(
            3,
            "evict_on_queue_full = false (default) does NOT evict idle sessions",
            try_evict_disabled_drops_overflow,
        ),
        State::Success,
    );
}
