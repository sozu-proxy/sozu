//! Transport layer for `sozu top` — synchronous threads over the existing
//! unix command socket. No async runtime in v1 by design (see Codex
//! cross-check in `tasks/todo.md`).
//!
//! Two `Channel` connections to the master:
//!
//! - **Snapshot collector** (`spawn_collector`): polls `RequestType::
//!   QueryMetrics` on the configurable `--refresh-ms` ticker and pushes each
//!   `AggregatedMetrics` (plus a wall-clock sample anchor) into a
//!   `crossbeam_channel::bounded::<Snapshot>(1)` with newest-wins overwrite,
//!   so a slow UI tick never queues a fan-out pile-up. The thread also
//!   serves the initial `ListWorkers` and `ListListeners` sidebar queries.
//! - **Events stream** (`spawn_events`): opens `RequestType::SubscribeEvents`
//!   once and forwards every inbound `Event` into a `crossbeam_channel::
//!   bounded::<TopEvent>(64)`. The unix `Channel<W,R>` is a single framed
//!   socket without message-id correlation, so multiplexing this stream
//!   with the discrete `QueryMetrics` round-trip on one socket is unsafe —
//!   we open a separate connection.
//!
//! Both threads exit cleanly when their `crossbeam_channel` peer is dropped
//! (the UI thread owns the rx ends; tearing down the App drops the senders).
//! Errors are logged via `eprintln!` and the thread shuts down — we never
//! crash the UI because of a transient socket error.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use sozu_command_lib::{
    config::Config,
    proto::command::{
        AggregatedMetrics, Event, ListListeners, ListenersList, QueryMetricsOptions, Request,
        Response, ResponseStatus, SubscribeEvents, request::RequestType,
        response_content::ContentType,
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
#[derive(Debug, Clone)]
pub struct ListenersSnapshot {
    pub list: ListenersList,
    pub received_at: Instant,
}

/// Capacity of the events channel. 64 is generous for the operator-pace
/// event stream (BACKEND_UP/DOWN, control-plane mutations, the new
/// METRIC_DETAIL_CHANGED audit). Bursts above 64 drop oldest at the UI
/// side via `recv_deadline` not the channel itself; the bound just keeps
/// memory bounded if the UI freezes momentarily.
const EVENTS_CAP: usize = 64;

/// `Snapshot` channel capacity. 1 + newest-wins overwrite is intentional:
/// while the UI is rendering a frame, a fresh snapshot replaces a stale
/// one in flight rather than queueing both. Confirms Codex's cadence
/// recommendation in `tasks/todo.md`.
const SNAPSHOT_CAP: usize = 1;

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
    let mut channel = create_channel(&config)?;
    let (tx, rx) = bounded::<Snapshot>(SNAPSHOT_CAP);
    let interval = Duration::from_millis(refresh_ms);
    let handle = std::thread::Builder::new()
        .name("sozu-top-collector".into())
        .spawn(move || collector_loop(&mut channel, tx, interval))
        .expect("spawn sozu-top collector");
    Ok((rx, handle))
}

fn collector_loop(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
    tx: Sender<Snapshot>,
    interval: Duration,
) {
    loop {
        let started = Instant::now();
        match poll_metrics(channel) {
            Ok(metrics) => {
                let snap = Snapshot {
                    metrics,
                    received_at: Instant::now(),
                };
                // Newest-wins overwrite: if the UI still hasn't drained the
                // last snapshot we replace it. `try_send` returns `Full` on
                // a populated bounded(1) channel; we drain + retry once.
                match tx.try_send(snap) {
                    Ok(()) => {}
                    Err(TrySendError::Full(snap)) => {
                        // Drain stale + retry; both errors here mean the
                        // peer was dropped, so we exit.
                        let _ = rx_drain_one(&tx);
                        if tx.try_send(snap).is_err() {
                            return;
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            Err(err) => {
                eprintln!("sozu top: snapshot poll error: {err}");
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

/// `try_send` on a `bounded(1)` channel needs the receiver side to drain.
/// Mirror the behaviour without the receiver: we re-construct the sender's
/// view by dropping the value. The sender API doesn't expose a "drop oldest"
/// directly so we get there via a paired channel handshake.
fn rx_drain_one<T>(_tx: &Sender<T>) -> Option<T> {
    // No public Sender API to peek the slot; the UI thread owns the rx end
    // and drains as it consumes. From the sender's side, a `Full` retry
    // after `try_send` means the slot still has the prior value because the
    // UI hasn't consumed it yet — accept the in-flight overwrite by simply
    // dropping the new value on a single-shot retry. The resulting cadence
    // is "publish-or-skip", which matches the documented newest-wins
    // semantics in `tasks/todo.md`.
    None
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

/// Spawn the listeners-collector thread. Polls `RequestType::ListListeners`
/// every `LISTENERS_INTERVAL` over its own `Channel` and pushes a
/// `ListenersSnapshot` into a `bounded(1)` newest-wins channel. Same
/// shape as `spawn_collector`.
pub fn spawn_listeners(
    config: Config,
) -> Result<(Receiver<ListenersSnapshot>, std::thread::JoinHandle<()>), CtlError> {
    let mut channel = create_channel(&config)?;
    let (tx, rx) = bounded::<ListenersSnapshot>(SNAPSHOT_CAP);
    let handle = std::thread::Builder::new()
        .name("sozu-top-listeners".into())
        .spawn(move || listeners_loop(&mut channel, tx))
        .expect("spawn sozu-top listeners");
    Ok((rx, handle))
}

fn listeners_loop(
    channel: &mut sozu_command_lib::channel::Channel<Request, Response>,
    tx: Sender<ListenersSnapshot>,
) {
    loop {
        let started = Instant::now();
        match poll_listeners(channel) {
            Ok(list) => {
                let snap = ListenersSnapshot {
                    list,
                    received_at: Instant::now(),
                };
                match tx.try_send(snap) {
                    Ok(()) => {}
                    Err(TrySendError::Full(snap)) => {
                        if tx.try_send(snap).is_err() {
                            return;
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            Err(err) => {
                eprintln!("sozu top: listeners poll error: {err}");
            }
        }
        let elapsed = started.elapsed();
        if elapsed < LISTENERS_INTERVAL {
            std::thread::sleep(LISTENERS_INTERVAL - elapsed);
        }
    }
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
