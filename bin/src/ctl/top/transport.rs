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
//! The three poll-driven threads exit cleanly when their `crossbeam_channel`
//! peer is dropped (the UI thread owns the rx ends; tearing down the App
//! drops the senders so `try_send` returns `Disconnected`). The events
//! thread does NOT see receiver-drop — its read blocks on the unix socket
//! and dropping the crossbeam `Receiver<TopEvent>` cannot propagate across
//! the socket. It exits on an `Arc<AtomicBool>` shutdown flag owned by
//! `run_top` and a bounded `EVENTS_READ_TIMEOUT` per read.
//!
//! Transient errors are surfaced via a shared `StatusSlot` (the same
//! mailbox the lease renewer uses); the render loop drains it once per
//! tick and shows the message in the status bar. The threads continue
//! running — a single transient socket error never crashes the UI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use sozu_command_lib::{
    channel::ChannelError,
    config::Config,
    proto::command::{
        AggregatedMetrics, Event, ListListeners, ListOfCertificatesByAddress, ListenersList,
        QueryCertificatesFilters, QueryMetricsOptions, Request, Response, ResponseStatus,
        SubscribeEvents, request::RequestType, response_content::ContentType,
    },
};

use crate::ctl::create_channel;

use super::CtlError;
use super::cardinality::{StatusSlot, publish_status};

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
/// METRIC_DETAIL_CHANGED audit). Bursts above 64 follow the publish-or-skip
/// contract used by every other snapshot channel: `try_send` on a full
/// bounded channel drops the newest sample (the 64 oldest stay queued for
/// the UI). The bound keeps memory bounded if the UI freezes momentarily.
const EVENTS_CAP: usize = 64;

/// Per-read deadline for the events loop. We do NOT want an unbounded
/// blocking read here: the only signal that the UI is gone is the
/// `shutdown` flag flipped by `run_top` after `render::run` returns,
/// and dropping the crossbeam `Receiver<TopEvent>` does NOT propagate
/// across the unix socket. A 1 s deadline keeps the shutdown latency
/// bounded by a single round-trip without burning CPU on idle traffic
/// (the master is event-pace; quiet seconds are the common case).
const EVENTS_READ_TIMEOUT: Duration = Duration::from_secs(1);

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
    status: StatusSlot,
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
                publish_status(&status, format!("{label} poll error: {err}"));
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
    status: StatusSlot,
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
            poll_loop("snapshot", interval, tx, status, channel, |ch| {
                poll_metrics(ch).map(|metrics| Snapshot {
                    metrics,
                    received_at: Instant::now(),
                })
            })
        })
        .map_err(|source| CtlError::SpawnFailed {
            label: "sozu-top-collector",
            source,
        })?;
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
                            "unexpected content variant for QueryMetrics: {}",
                            content_type_name(other.as_ref()),
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
    status: StatusSlot,
) -> Result<(Receiver<ListenersSnapshot>, std::thread::JoinHandle<()>), CtlError> {
    let channel = create_channel(&config)?;
    let (tx, rx) = bounded::<ListenersSnapshot>(SNAPSHOT_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-listeners".into())
        .spawn(move || {
            poll_loop("listeners", LISTENERS_INTERVAL, tx, status, channel, |ch| {
                poll_listeners(ch).map(|list| ListenersSnapshot { list })
            })
        })
        .map_err(|source| CtlError::SpawnFailed {
            label: "sozu-top-listeners",
            source,
        })?;
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
                            "unexpected content variant for ListListeners: {}",
                            content_type_name(other.as_ref()),
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
    status: StatusSlot,
) -> Result<(Receiver<CertsSnapshot>, std::thread::JoinHandle<()>), CtlError> {
    let channel = create_channel(&config)?;
    let (tx, rx) = bounded::<CertsSnapshot>(SNAPSHOT_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-certs".into())
        .spawn(move || {
            poll_loop("certs", CERTS_INTERVAL, tx, status, channel, |ch| {
                poll_certs(ch).map(|list| CertsSnapshot { list })
            })
        })
        .map_err(|source| CtlError::SpawnFailed {
            label: "sozu-top-certs",
            source,
        })?;
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
                    Some(ContentType::CertificatesWithFingerprints(map)) => {
                        // `query_certificates_from_main` answers with the
                        // fingerprint-keyed map (the same shape `sozu
                        // certificate query` consumes). The CERTS pane
                        // wants per-address rows; synthesise them here,
                        // dropping the PEM + private-key fields
                        // immediately because the TUI only needs the
                        // (domain, fingerprint) pair. NEVER let the key
                        // material flow further than this conversion —
                        // an `eprintln!` / log line downstream would
                        // otherwise leak the operator's private key to
                        // the renderer's alt-screen or scrollback.
                        return Ok(certs_from_fingerprint_map(map));
                    }
                    other => {
                        return Err(format!(
                            "unexpected content variant for QueryCertificatesFromTheState: {}",
                            content_type_name(other.as_ref()),
                        ));
                    }
                },
                None => return Err("QueryCertificatesFromTheState OK with no content".into()),
            },
        }
    }
}

/// Convert the fingerprint-keyed `CertificatesWithFingerprints` payload
/// (which carries cert PEM + private key) into the by-address
/// `ListOfCertificatesByAddress` shape the CERTS pane consumes (which
/// carries only the `(domain, fingerprint)` pair). Drops the key + cert
/// PEM fields IMMEDIATELY so private-key material never reaches the
/// renderer, the error log, the alt-screen scrollback, or any
/// downstream `eprintln!`. The address is synthesised because the
/// fingerprint-keyed response doesn't carry one; `0.0.0.0:0` renders
/// as `0.0.0.0:0` in the table and signals "no per-address grouping
/// available". A follow-up could plumb the actual listener address
/// from the state, but the inventory shape is correct.
fn certs_from_fingerprint_map(
    payload: sozu_command_lib::proto::command::CertificatesWithFingerprints,
) -> ListOfCertificatesByAddress {
    use sozu_command_lib::proto::command::{
        CertificateSummary, CertificatesByAddress, SocketAddress,
    };
    let mut summaries: Vec<CertificateSummary> = Vec::with_capacity(payload.certs.len());
    for (fingerprint, cert) in payload.certs {
        // The cert's first SNI is the operator-facing identifier. If
        // `names` is empty (legacy certs without an SNI override)
        // fall back to a `<unknown>` placeholder so the row still
        // shows up rather than disappearing silently.
        let domain = cert
            .names
            .into_iter()
            .next()
            .unwrap_or_else(|| "<unknown>".to_owned());
        summaries.push(CertificateSummary {
            fingerprint,
            domain,
        });
        // `cert.certificate`, `cert.certificate_chain`, `cert.key`
        // drop here as `cert` goes out of scope — never copied
        // forward, never logged.
    }
    ListOfCertificatesByAddress {
        certificates: vec![CertificatesByAddress {
            address: SocketAddress {
                ip: sozu_command_lib::proto::command::IpAddress {
                    inner: Some(sozu_command_lib::proto::command::ip_address::Inner::V4(0)),
                },
                port: 0,
            },
            certificate_summaries: summaries,
        }],
    }
}

/// Stable short name for a `ContentType` variant, used in error
/// messages to identify which variant arrived without `Debug`-printing
/// its payload (private keys, large blobs). Returns `<none>` for
/// `None` (no content_type set in the response).
fn content_type_name(ct: Option<&ContentType>) -> &'static str {
    match ct {
        None => "<none>",
        Some(ContentType::Workers(_)) => "Workers",
        Some(ContentType::Metrics(_)) => "Metrics",
        Some(ContentType::WorkerResponses(_)) => "WorkerResponses",
        Some(ContentType::Event(_)) => "Event",
        Some(ContentType::FrontendList(_)) => "FrontendList",
        Some(ContentType::ListenersList(_)) => "ListenersList",
        Some(ContentType::WorkerMetrics(_)) => "WorkerMetrics",
        Some(ContentType::AvailableMetrics(_)) => "AvailableMetrics",
        Some(ContentType::Clusters(_)) => "Clusters",
        Some(ContentType::ClusterHashes(_)) => "ClusterHashes",
        Some(ContentType::CertificatesByAddress(_)) => "CertificatesByAddress",
        Some(ContentType::CertificatesWithFingerprints(_)) => "CertificatesWithFingerprints",
        Some(ContentType::RequestCounts(_)) => "RequestCounts",
        Some(ContentType::MaxConnectionsPerIpLimit(_)) => "MaxConnectionsPerIpLimit",
        Some(ContentType::HealthChecksList(_)) => "HealthChecksList",
        Some(ContentType::MetricDetailStatus(_)) => "MetricDetailStatus",
        Some(ContentType::WorkerMetricDetailStatus(_)) => "WorkerMetricDetailStatus",
    }
}

/// Spawn the events-stream thread. Returns the `TopEvent` receiver and
/// a join handle. Thread exits when `shutdown` is set, when the
/// SubscribeEvents stream errors out, or when the master closes the
/// subscription with a terminal Ok/Failure.
///
/// The `shutdown` flag is the canonical wake-up: dropping the
/// `Receiver<TopEvent>` cannot propagate across the unix socket, so
/// without an explicit flag the thread sleeps forever on the next read.
/// `run_top` owns the `Arc<AtomicBool>` and flips it after the render
/// loop returns.
pub fn spawn_events(
    config: Config,
    shutdown: Arc<AtomicBool>,
    status: StatusSlot,
) -> Result<(Receiver<TopEvent>, std::thread::JoinHandle<()>), CtlError> {
    let mut channel = create_channel(&config)?;
    let (tx, rx) = bounded::<TopEvent>(EVENTS_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-events".into())
        .spawn(move || events_loop(&mut channel, tx, shutdown, status))
        .map_err(|source| CtlError::SpawnFailed {
            label: "sozu-top-events",
            source,
        })?;
    Ok((rx, handle))
}

fn events_loop(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
    tx: Sender<TopEvent>,
    shutdown: Arc<AtomicBool>,
    status: StatusSlot,
) {
    let req = Request {
        request_type: Some(RequestType::SubscribeEvents(SubscribeEvents {})),
    };
    if let Err(e) = channel.write_message(&req) {
        publish_status(&status, format!("events: SubscribeEvents write error: {e}"));
        return;
    }
    while !shutdown.load(Ordering::Relaxed) {
        let resp = match channel.read_message_blocking_timeout(Some(EVENTS_READ_TIMEOUT)) {
            Ok(r) => r,
            // Bounded read timeout fired with no payload; loop back to
            // re-check the shutdown flag. This is the steady-state path
            // on a quiet master, not an error.
            Err(ChannelError::TimeoutReached(_)) => continue,
            Err(e) => {
                publish_status(&status, format!("events: read error: {e}"));
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
                    // Publish-or-skip on overflow: we never block the events
                    // thread on a stuck UI. A `try_send` failure on a full
                    // bounded channel drops the newest sample (the 64
                    // oldest stay queued for the UI). Documented contract
                    // shared with the snapshot channels above.
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
    match resp.content?.content_type? {
        ContentType::Event(ev) => Some(ev),
        _ => None,
    }
}
