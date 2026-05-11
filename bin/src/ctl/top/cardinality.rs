//! Runtime cardinality lease lifecycle for `sozu top`.
//!
//! On startup the TUI elevates the metrics drain to `MetricDetail::Backend`
//! via the `SetMetricDetail` proto verb. The lease is `client_id`-keyed
//! with a configurable TTL; a renewer thread re-sends every `ttl/2` seconds
//! so the lease stays alive while the TUI runs. On Drop (clean shutdown,
//! panic, SIGINT/SIGTERM via `ctrlc::set_handler` registered by the
//! renderer) we send a best-effort `clear: true` revoke. Crash safety: the
//! lease self-expires server-side after `ttl_seconds` so a dead `sozu top`
//! never permanently elevates cardinality.
//!
//! Two threads + one channel:
//!
//! - The renewer thread sleeps on a `crossbeam_channel::after(ttl/2)` and
//!   re-sends a renewal each tick. A "shutdown" sender wakes it early so
//!   `Drop` returns fast.
//! - The drop path owns its own pre-opened channel (`final_channel`)
//!   parked behind a `Mutex<Option<...>>`, used exclusively for the
//!   best-effort `clear` request. The renewer thread keeps a separate
//!   channel of its own and drops it on exit.

use std::process;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, after, bounded, select};
use sozu_command_lib::{
    channel::Channel,
    config::Config,
    proto::command::{
        MetricDetail, Request, Response, ResponseStatus, SetMetricDetail, request::RequestType,
    },
};

use crate::cli::TopDetail;
use crate::ctl::create_channel;

use super::CtlError;

/// RAII guard that holds a runtime cardinality lease while the TUI runs.
/// Drop clears the lease (best-effort) so the worker drops back to its
/// configured floor. Crash-safe: even if Drop never runs, the lease
/// self-expires after `ttl_seconds`.
pub struct DetailGuard {
    /// Stable identifier for this `sozu top` instance, of the shape
    /// `top:<pid>:<random>`. Required by `SetMetricDetail` so multiple TUIs
    /// can lease independently without colliding on each other's id.
    client_id: String,
    /// Shutdown signal for the renewer thread. The renewer drops out of its
    /// `select!` when this fires.
    shutdown_tx: Option<Sender<()>>,
    /// Renewer-thread join handle. Joined on Drop after the shutdown signal
    /// fires so we exit deterministically.
    renewer: Option<JoinHandle<()>>,
    /// Pre-opened channel reserved for the final `clear` request. Parked
    /// behind a `Mutex<Option<...>>` so `Drop` can take it without
    /// blocking on contention. The renewer thread is NOT involved with
    /// this slot — it keeps its own dedicated channel and drops it on
    /// shutdown — so a renewer crash never strands the revoke path.
    final_channel: Arc<Mutex<Option<Channel<Request, Response>>>>,
    /// Reason text echoed in the audit `EventKind::MetricDetailChanged`
    /// trail. Defaults to `"sozu top"`.
    reason: String,
}

impl DetailGuard {
    /// Open a fresh `Channel` to the master, send the initial
    /// `SetMetricDetail` apply, and spawn the renewer. Returns `Ok` once
    /// the master acknowledges; if the master rejects (e.g. mixed-version
    /// fleet without the verb) `Err` is returned and the caller shows the
    /// "lease unsupported" warning in the status bar.
    pub fn apply(
        config: &Config,
        detail: TopDetail,
        ttl_seconds: u32,
        reason: impl Into<String>,
    ) -> Result<Self, CtlError> {
        let client_id = format!("top:{}:{}", process::id(), short_random_suffix());
        let proto_detail = match detail {
            TopDetail::Process => MetricDetail::DetailProcess,
            TopDetail::Frontend => MetricDetail::DetailFrontend,
            TopDetail::Cluster => MetricDetail::DetailCluster,
            TopDetail::Backend => MetricDetail::DetailBackend,
        };
        let reason = reason.into();
        let mut channel = create_channel(config)?;
        send_set_detail(
            &mut channel,
            &client_id,
            Some(proto_detail),
            Some(ttl_seconds),
            Some(&reason),
            false,
        )?;

        let final_channel = Arc::new(Mutex::new(Some(channel)));
        let (shutdown_tx, shutdown_rx) = bounded::<()>(0);
        let renewer = spawn_renewer(
            config.clone(),
            client_id.clone(),
            proto_detail,
            ttl_seconds,
            reason.clone(),
            shutdown_rx,
        )?;
        Ok(Self {
            client_id,
            shutdown_tx: Some(shutdown_tx),
            renewer: Some(renewer),
            final_channel,
            reason,
        })
    }
}

impl Drop for DetailGuard {
    fn drop(&mut self) {
        // Wake the renewer so it exits before we re-use the saved channel
        // for the final `clear` request.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.renewer.take() {
            // Renewer is bounded by ttl/2; we wake it with the shutdown
            // signal so this join is fast in practice. Best-effort: if the
            // thread panicked we still want to issue the revoke.
            let _ = handle.join();
        }
        if let Ok(mut slot) = self.final_channel.lock() {
            if let Some(mut channel) = slot.take() {
                let _ = send_set_detail(
                    &mut channel,
                    &self.client_id,
                    None,
                    None,
                    Some(&format!("{} (clear)", self.reason)),
                    true,
                );
            }
        }
    }
}

fn spawn_renewer(
    config: Config,
    client_id: String,
    detail: MetricDetail,
    ttl_seconds: u32,
    reason: String,
    shutdown_rx: Receiver<()>,
) -> Result<JoinHandle<()>, CtlError> {
    let renew_after = Duration::from_secs((ttl_seconds.max(2) / 2) as u64);
    let handle = std::thread::Builder::new()
        .name("sozu-top-detail-renewer".into())
        .spawn(move || {
            // Open the renewer's own channel; the drop path keeps a
            // separate pre-opened one for its `clear` request. The
            // renewer's channel drops implicitly on thread exit.
            let mut channel = match create_channel(&config) {
                Ok(ch) => ch,
                Err(e) => {
                    eprintln!("sozu top: renewer channel: {e:?}");
                    return;
                }
            };
            loop {
                let timer = after(renew_after);
                select! {
                    recv(timer) -> _ => {
                        if let Err(e) = send_set_detail(
                            &mut channel,
                            &client_id,
                            Some(detail),
                            Some(ttl_seconds),
                            Some(&format!("{reason} (renew)")),
                            false,
                        ) {
                            eprintln!("sozu top: renewer send error: {e:?}");
                            return;
                        }
                    }
                    recv(shutdown_rx) -> _ => return,
                }
            }
        })
        .expect("spawn sozu-top renewer");
    Ok(handle)
}

fn send_set_detail(
    channel: &mut Channel<Request, Response>,
    client_id: &str,
    detail: Option<MetricDetail>,
    ttl_seconds: Option<u32>,
    reason: Option<&str>,
    clear: bool,
) -> Result<(), CtlError> {
    let req = Request {
        request_type: Some(RequestType::SetMetricDetail(SetMetricDetail {
            client_id: client_id.to_owned(),
            detail: detail.map(|d| d as i32),
            ttl_seconds,
            clear: Some(clear),
            reason: reason.map(|r| r.to_owned()),
            // Master-populated fields; clients leave them empty. The
            // master fills them in `worker_request` before fan-out from
            // the connecting `ClientSession`.
            peer_pid: None,
            peer_session_ulid: None,
        })),
    };
    channel
        .write_message(&req)
        .map_err(CtlError::WriteRequest)?;
    // Drain processing replies until the terminal Ok/Failure. SetMetricDetail
    // is a quick fan-out; 5 s gives enough headroom for a slow worker.
    loop {
        let resp = channel
            .read_message_blocking_timeout(Some(Duration::from_secs(5)))
            .map_err(CtlError::ReadBlocking)?;
        match resp.status() {
            ResponseStatus::Processing => continue,
            ResponseStatus::Failure => return Err(CtlError::WrongResponse(resp)),
            ResponseStatus::Ok => return Ok(()),
        }
    }
}

/// 8 hex chars used as the random portion of the lease `client_id`. Reads
/// 4 bytes from `/dev/urandom` (Linux/BSD/macOS) instead of the previous
/// `subsec_nanos()` seed: pid reuse on a busy host plus PID modulo
/// SystemTime collisions made that source weak under `sozu top` mass-launch
/// (e.g. tmux scripted dashboards). The kernel CSPRNG is non-blocking on
/// every platform Sōzu builds on after first boot; if the read fails for
/// any reason we fall back to the nanosecond seed so the TUI still starts.
/// Cryptographic strength is not required — the value only needs to be
/// unguessable enough to avoid lease-id collisions across concurrent
/// instances — so we don't pull `getrandom` as a new direct dependency.
fn short_random_suffix() -> String {
    use std::io::Read;
    let mut buf = [0u8; 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        let n = u32::from_ne_bytes(buf);
        return format!("{n:08x}");
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:08x}")
}
