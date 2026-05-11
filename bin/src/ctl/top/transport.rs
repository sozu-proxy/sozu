//! Transport layer for `sozu top` — synchronous threads over the existing
//! unix command socket. No async runtime in v1 by design.
//!
//! Four `Channel` connections to the master, each owned by its own thread:
//!
//! 1. **Snapshot collector** (`spawn_collector`): polls `RequestType::
//!    QueryMetrics` on the configurable `--refresh-ms` ticker and pushes
//!    each `AggregatedMetrics` (plus a wall-clock sample anchor) into a
//!    `crossbeam_channel::bounded::<Snapshot>(1)`.
//! 2. **Events stream** (`spawn_events`): opens `RequestType::
//!    SubscribeEvents` once and forwards every inbound `Event` into a
//!    `crossbeam_channel::bounded::<TopEvent>(64)`. The unix `Channel<W,R>`
//!    is a single framed socket without message-id correlation, so
//!    multiplexing this stream with the discrete `QueryMetrics` round-trip
//!    on one socket is unsafe — we open a separate connection.
//! 3. **Listeners collector** (`spawn_listeners`): polls
//!    `RequestType::ListListeners` every 5 s into a `bounded(1)` channel.
//! 4. **Certs collector** (`spawn_certs`): polls
//!    `RequestType::QueryCertificatesFromTheState` every 30 s into a
//!    `bounded(1)` channel.
//!
//! All snapshot threads use **publish-or-skip on backpressure**: when the
//! `bounded(1)` channel is already populated (the UI hasn't drained yet),
//! the fresh snapshot is dropped rather than the thread blocking or dying.
//! The next poll produces a fresher snapshot anyway, so dropping an
//! in-flight one is correct: it preserves "newest-wins" without needing
//! the sender to peek into the receiver's slot. The events thread uses the
//! same shape, just with a `bounded(64)` buffer for burst tolerance.
//!
//! All threads exit cleanly when their `crossbeam_channel` peer is dropped
//! (the UI thread owns the rx ends; tearing down the App drops the senders).
//! Errors are logged via `eprintln!` and the thread shuts down — we never
//! crash the UI because of a transient socket error.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use sozu_command_lib::{
    config::Config,
    proto::command::{
        AggregatedMetrics, Event, ListListeners, ListOfCertificatesByAddress, ListenersList,
        QueryCertificatesFilters, QueryMetricsOptions, Request, Response, ResponseStatus,
        SubscribeEvents, request::RequestType, response_content::ContentType,
    },
};

use crate::ctl::create_channel;

use super::CtlError;

/// Bundle published by the collector thread on every successful poll.
/// Owns `AggregatedMetrics` outright so the UI can rebuild ring buffers
/// without holding any other lock. The `received_at` field anchors rate
/// calculation across ticks.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub metrics: AggregatedMetrics,
    pub received_at: Instant,
}

/// Wrapper around an inbound `Event` so we can later attach metadata
/// (received_at, source-worker tag) without breaking the channel shape.
#[derive(Debug, Clone)]
pub struct TopEvent {
    pub event: Event,
    pub received_at: Instant,
}

/// Listener inventory snapshot pushed by the listeners-collector thread.
/// Refreshed at a slower cadence than metrics (5 s default) because listener
/// state changes are operator-paced — adds, removes, activates, deactivates
/// all flow via control-plane mutations that the EVENTS pane already shows.
///
/// Unlike `Snapshot`, there is no `received_at` anchor: the listeners pane
/// renders the absolute set, never per-tick rates, so the wall-clock would
/// have nothing to discriminate against.
#[derive(Debug, Clone)]
pub struct ListenersSnapshot {
    pub list: ListenersList,
}

/// Certificate inventory snapshot pushed by the certs-collector thread.
/// Polled at 30 s — even slower than listeners because cert lifecycle is
/// operator-driven (add, remove, replace via the master's state) and every
/// transition already lands as a CERTIFICATE_* event on the EVENTS pane.
///
/// Same shape as `ListenersSnapshot`: no `received_at` because the certs
/// pane renders the absolute set without per-tick rates.
#[derive(Debug, Clone)]
pub struct CertsSnapshot {
    pub list: ListOfCertificatesByAddress,
}

/// Capacity of the events channel. 64 is generous for the operator-pace
/// event stream (BACKEND_UP/DOWN, control-plane mutations, the new
/// METRIC_DETAIL_CHANGED audit). Bursts above 64 drop oldest at the UI
/// side via `recv_deadline` not the channel itself; the bound just keeps
/// memory bounded if the UI freezes momentarily.
const EVENTS_CAP: usize = 64;

/// `Snapshot` channel capacity. 1 with publish-or-skip on backpressure is
/// intentional: while the UI is rendering a frame, a fresh snapshot is
/// dropped rather than queueing behind the stale one. The next poll
/// produces a newer snapshot anyway, so the cadence stays "as fresh as
/// the UI can consume" without the sender having to peek into the
/// receiver's slot.
const SNAPSHOT_CAP: usize = 1;

/// Shared polling skeleton for the three `bounded(1)` collector threads
/// (`spawn_collector`, `spawn_listeners`, `spawn_certs`).
///
/// The threading topology (one OS thread per pane, owning its own `Channel`,
/// `bounded(1)` publish-or-skip on backpressure) is locked by design and is
/// not abstracted here — only the per-tick polling shape is shared:
///
/// 1. record `Instant::now()`
/// 2. call the per-thread `poll` closure
/// 3. on `Ok(v)`: `try_send(v)` — `Full` is dropped (publish-or-skip),
///    `Disconnected` exits the thread cleanly when the UI drops `rx`
/// 4. on `Err(_)`: `eprintln!` and keep going (a transient socket error
///    never kills the thread; the next tick reconnects via the shared
///    `Channel` retry path inside `poll`)
/// 5. sleep the remainder of `interval` if the round-trip was faster
///
/// The events thread (`spawn_events`) has a different shape (single
/// `SubscribeEvents` write + open-ended drain loop) and intentionally does
/// not reuse this helper.
fn poll_loop<T, F>(
    label: &'static str,
    interval: Duration,
    tx: Sender<T>,
    mut channel: sozu_command_lib::channel::Channel<Request, Response>,
    mut poll: F,
) where
    F: FnMut(&mut sozu_command_lib::channel::Channel<Request, Response>) -> Result<T, String>,
{
    loop {
        let started = Instant::now();
        match poll(&mut channel) {
            Ok(v) => match tx.try_send(v) {
                Ok(()) => {}
                // Publish-or-skip: if the UI hasn't drained the previous
                // value, skip this one rather than killing the thread. The
                // next poll produces a fresher value anyway.
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return,
            },
            Err(err) => {
                eprintln!("sozu top: {label} poll error: {err}");
            }
        }
        // Sleep the remaining slice of the configured interval so we don't
        // hammer the master after a slow round-trip. If a poll took longer
        // than `interval`, fire the next one immediately.
        let elapsed = started.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

/// Spawn the snapshot-collector thread. Returns the `Snapshot` receiver and
/// a join handle. Thread exits when the receiver is dropped or the channel
/// returns a permanent socket error.
pub fn spawn_collector(
    config: Config,
    refresh_ms: u64,
) -> Result<(Receiver<Snapshot>, std::thread::JoinHandle<()>), CtlError> {
    // Open the dedicated polling channel up-front so a connection failure
    // surfaces synchronously (operator gets `CtlError::CreateChannel`)
    // rather than silently spinning behind the spawned thread.
    let channel = create_channel(&config)?;
    let (tx, rx) = bounded::<Snapshot>(SNAPSHOT_CAP);
    let interval = Duration::from_millis(refresh_ms);
    let handle = std::thread::Builder::new()
        .name("sozu-top-collector".into())
        .spawn(move || {
            poll_loop("snapshot", interval, tx, channel, |ch| {
                poll_metrics(ch).map(|metrics| Snapshot {
                    metrics,
                    received_at: Instant::now(),
                })
            })
        })
        .expect("spawn sozu-top collector");
    Ok((rx, handle))
}

fn poll_metrics(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
) -> Result<AggregatedMetrics, String> {
    let req = Request {
        request_type: Some(RequestType::QueryMetrics(QueryMetricsOptions {
            list: false,
            cluster_ids: vec![],
            backend_ids: vec![],
            metric_names: vec![],
            no_clusters: false,
            workers: false,
        })),
    };
    channel
        .write_message(&req)
        .map_err(|e| format!("write QueryMetrics: {e}"))?;

    // The protocol shape is `0..N Response{PROCESSING}` then exactly one
    // terminal `Response{OK|FAILURE}`. We poll until the terminal arrives
    // (or a per-message read timeout pushes us back). Matches the existing
    // `bin/src/ctl/command.rs::get_metrics` loop.
    loop {
        let resp = channel
            .read_message_blocking_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("read QueryMetrics response: {e}"))?;
        match resp.status() {
            ResponseStatus::Processing => continue,
            ResponseStatus::Failure => {
                return Err(format!("QueryMetrics failed: {}", resp.message));
            }
            ResponseStatus::Ok => match resp.content {
                Some(content) => match content.content_type {
                    Some(ContentType::Metrics(m)) => return Ok(m),
                    other => {
                        return Err(format!(
                            "unexpected content variant for QueryMetrics: {other:?}"
                        ));
                    }
                },
                None => return Err("QueryMetrics OK with no content".into()),
            },
        }
    }
}

/// Cadence of the listeners poll. Operator-paced; 5 s matches the brief's
/// "cold subjects" tier and HAProxy hatop's documented `show stat` cadence.
const LISTENERS_INTERVAL: Duration = Duration::from_secs(5);

/// Cadence of the certs poll. Operator-paced and lower-priority than
/// listeners; cert mutations also flow through the EVENTS pane in
/// real-time, so the 30 s refresh is enough to keep the table fresh.
const CERTS_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the listeners-collector thread. Polls `RequestType::ListListeners`
/// every `LISTENERS_INTERVAL` over its own `Channel` and pushes a
/// `ListenersSnapshot` into a `bounded(1)` newest-wins channel. Same
/// shape as `spawn_collector`.
pub fn spawn_listeners(
    config: Config,
) -> Result<(Receiver<ListenersSnapshot>, std::thread::JoinHandle<()>), CtlError> {
    let channel = create_channel(&config)?;
    let (tx, rx) = bounded::<ListenersSnapshot>(SNAPSHOT_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-listeners".into())
        .spawn(move || {
            poll_loop("listeners", LISTENERS_INTERVAL, tx, channel, |ch| {
                poll_listeners(ch).map(|list| ListenersSnapshot { list })
            })
        })
        .expect("spawn sozu-top listeners");
    Ok((rx, handle))
}

fn poll_listeners(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
) -> Result<ListenersList, String> {
    let req = Request {
        request_type: Some(RequestType::ListListeners(ListListeners {})),
    };
    channel
        .write_message(&req)
        .map_err(|e| format!("write ListListeners: {e}"))?;
    loop {
        let resp = channel
            .read_message_blocking_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("read ListListeners response: {e}"))?;
        match resp.status() {
            ResponseStatus::Processing => continue,
            ResponseStatus::Failure => {
                return Err(format!("ListListeners failed: {}", resp.message));
            }
            ResponseStatus::Ok => match resp.content {
                Some(content) => match content.content_type {
                    Some(ContentType::ListenersList(l)) => return Ok(l),
                    other => {
                        return Err(format!(
                            "unexpected content variant for ListListeners: {other:?}"
                        ));
                    }
                },
                None => return Err("ListListeners OK with no content".into()),
            },
        }
    }
}

/// Spawn the certs-collector thread. Polls `RequestType::QueryCertificates
/// FromTheState` every `CERTS_INTERVAL` over its own `Channel` and pushes a
/// `CertsSnapshot` into a `bounded(1)` newest-wins channel. The "from the
/// state" variant (vs `QueryCertificatesFromWorkers`) reads the master's
/// `ConfigState` — the canonical cert inventory — without paying the
/// worker-fan-out cost on every poll.
pub fn spawn_certs(
    config: Config,
) -> Result<(Receiver<CertsSnapshot>, std::thread::JoinHandle<()>), CtlError> {
    let channel = create_channel(&config)?;
    let (tx, rx) = bounded::<CertsSnapshot>(SNAPSHOT_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-certs".into())
        .spawn(move || {
            poll_loop("certs", CERTS_INTERVAL, tx, channel, |ch| {
                poll_certs(ch).map(|list| CertsSnapshot { list })
            })
        })
        .expect("spawn sozu-top certs");
    Ok((rx, handle))
}

fn poll_certs(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
) -> Result<ListOfCertificatesByAddress, String> {
    let req = Request {
        request_type: Some(RequestType::QueryCertificatesFromTheState(
            QueryCertificatesFilters {
                domain: None,
                fingerprint: None,
            },
        )),
    };
    channel
        .write_message(&req)
        .map_err(|e| format!("write QueryCertificatesFromTheState: {e}"))?;
    loop {
        let resp = channel
            .read_message_blocking_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("read QueryCertificatesFromTheState response: {e}"))?;
        match resp.status() {
            ResponseStatus::Processing => continue,
            ResponseStatus::Failure => {
                return Err(format!(
                    "QueryCertificatesFromTheState failed: {}",
                    resp.message
                ));
            }
            ResponseStatus::Ok => match resp.content {
                Some(content) => match content.content_type {
                    Some(ContentType::CertificatesByAddress(l)) => return Ok(l),
                    other => {
                        return Err(format!(
                            "unexpected content variant for QueryCertificatesFromTheState: {other:?}"
                        ));
                    }
                },
                None => return Err("QueryCertificatesFromTheState OK with no content".into()),
            },
        }
    }
}

/// Spawn the events-stream thread. Returns the `TopEvent` receiver and
/// a join handle. Thread exits when the receiver is dropped or the
/// SubscribeEvents stream errors out.
pub fn spawn_events(
    config: Config,
) -> Result<(Receiver<TopEvent>, std::thread::JoinHandle<()>), CtlError> {
    let mut channel = create_channel(&config)?;
    let (tx, rx) = bounded::<TopEvent>(EVENTS_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-events".into())
        .spawn(move || events_loop(&mut channel, tx))
        .expect("spawn sozu-top events");
    Ok((rx, handle))
}

fn events_loop(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
    tx: Sender<TopEvent>,
) {
    let req = Request {
        request_type: Some(RequestType::SubscribeEvents(SubscribeEvents {})),
    };
    if let Err(e) = channel.write_message(&req) {
        eprintln!("sozu top: SubscribeEvents write error: {e}");
        return;
    }
    loop {
        let resp = match channel.read_message_blocking_timeout(None) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("sozu top: events read error: {e}");
                return;
            }
        };
        match resp.status() {
            ResponseStatus::Processing => {
                if let Some(ev) = unwrap_event(resp) {
                    let topev = TopEvent {
                        event: ev,
                        received_at: Instant::now(),
                    };
                    // Drop oldest semantics on overflow: we never block the
                    // events thread on a stuck UI. A `try_send` failure on a
                    // populated bounded channel is acceptable (the UI is
                    // already showing 64 recent events; one more drops).
                    let _ = tx.try_send(topev);
                }
            }
            // Some servers may close the subscription with an explicit
            // terminal Ok/Failure; surface it then exit.
            ResponseStatus::Ok | ResponseStatus::Failure => return,
        }
    }
}

fn unwrap_event(resp: Response) -> Option<Event> {
    let content = resp.content?;
    match content.content_type? {
        ContentType::Event(ev) => Some(ev),
        _ => None,
    }
}
