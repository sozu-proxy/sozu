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

/// Shared status slot used by background threads (renewer, transport
/// collectors) to surface error / degraded-mode notes to the operator.
/// The render loop drains it once per tick and copies the most recent
/// message into `App::status` so the F-key bar shows it on the next
/// frame. Wrapped in `Arc<Mutex<...>>` because the writers (background
/// threads) and the reader (render-loop thread) live in different
/// scheduling contexts; contention is rare (one write on error, one
/// read per tick) so the lock is uncontended in practice.
pub type StatusSlot = Arc<Mutex<Option<String>>>;

/// Build an empty shared status slot.
pub fn new_status_slot() -> StatusSlot {
    Arc::new(Mutex::new(None))
}

/// Atomically take the latest pending status message, if any. Used by
/// the render loop's per-tick drain. Returns `None` when no background
/// thread has published since the last drain. Silently passes a poisoned
/// lock through `into_inner` — a poisoned mutex here means a background
/// thread panicked while holding it, and we want to surface the residual
/// message rather than swallow it.
pub fn take_status(slot: &StatusSlot) -> Option<String> {
    match slot.lock() {
        Ok(mut g) => g.take(),
        Err(poison) => poison.into_inner().take(),
    }
}

/// Publish a status message from a background thread. Drops the
/// previous message if it had not yet been drained — render-loop
/// cadence (~30 fps) is much faster than realistic background-thread
/// error rates so overwriting is the right policy. A poisoned lock is
/// recovered the same way as `take_status`.
fn publish_status(slot: &StatusSlot, msg: String) {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    };
    *g = Some(msg);
}

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
    /// Pre-opened channel reserved for the final `clear` request. The
    /// renewer thread keeps its own dedicated channel and never touches
    /// this slot, so contention is impossible by construction: `Drop`
    /// is the sole consumer, `apply` is the sole producer. A direct
    /// `Option<...>` is therefore enough — no Arc, no Mutex, no
    /// silently-swallowed lock-poison branch.
    final_channel: Option<Channel<Request, Response>>,
    /// Reason text echoed in the audit `EventKind::MetricDetailChanged`
    /// trail. Defaults to `"sozu top"`.
    reason: String,
    /// Shared status slot the renewer thread publishes degraded-mode
    /// messages into. The caller (`crate::ctl::top::mod`) holds the
    /// other `Arc<...>` end and threads it through `RenderConfig` so
    /// the render loop drains it via the free `take_status` function
    /// once per tick. Stored on the guard so it stays alive for the
    /// renewer thread's lifetime; read accesses live in the render
    /// loop, not on this struct (hence the `dead_code`-style read
    /// pattern). Without this slot the renewer's `eprintln!` errors
    /// land on a wiped alt-screen and the operator never sees them.
    #[allow(dead_code)]
    status: StatusSlot,
}

impl DetailGuard {
    /// Open a fresh `Channel` to the master, send the initial
    /// `SetMetricDetail` apply, and spawn the renewer. Returns `Ok` once
    /// the master acknowledges; if the master rejects (e.g. mixed-version
    /// fleet without the verb) `Err` is returned and the caller shows the
    /// "lease unsupported" warning in the status bar. The `status` slot
    /// is shared with the render loop so the renewer can surface
    /// degraded-mode messages without writing to the wiped alt-screen.
    pub fn apply(
        config: &Config,
        detail: TopDetail,
        ttl_seconds: u32,
        reason: impl Into<String>,
        status: StatusSlot,
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

        let (shutdown_tx, shutdown_rx) = bounded::<()>(0);
        let renewer = spawn_renewer(
            config.clone(),
            client_id.clone(),
            proto_detail,
            ttl_seconds,
            reason.clone(),
            shutdown_rx,
            Arc::clone(&status),
        )?;
        Ok(Self {
            client_id,
            shutdown_tx: Some(shutdown_tx),
            renewer: Some(renewer),
            final_channel: Some(channel),
            reason,
            status,
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
        if let Some(mut channel) = self.final_channel.take() {
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

fn spawn_renewer(
    config: Config,
    client_id: String,
    detail: MetricDetail,
    ttl_seconds: u32,
    reason: String,
    shutdown_rx: Receiver<()>,
    status: StatusSlot,
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
                    publish_status(
                        &status,
                        format!("renewer channel open failed: {e}; cardinality will lapse at TTL"),
                    );
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
                            publish_status(
                                &status,
                                format!(
                                    "renewer dropped: {e}; cardinality lapses in ≤ {ttl_seconds}s"
                                ),
                            );
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

/// 8 hex chars used as the random portion of the lease `client_id`. On
/// Linux uses the `getrandom(2)` syscall directly via the `libc` crate
/// (already in the workspace), which is non-blocking, has no fs
/// dependency, and surfaces failure modes (`EAGAIN` while the entropy
/// pool is uninitialised, `ENOSYS` on ancient kernels) as a `-1` return.
/// On non-Linux Unix targets we fall back to a `/dev/urandom` read;
/// `getrandom`'s shape is OS-specific (FreeBSD: `getrandom(2)`, OpenBSD:
/// `getentropy(2)`, macOS: `SecRandomCopyBytes`) and the fs path is the
/// portable lowest common denominator.
///
/// Endianness: we use `u32::from_le_bytes` for cross-arch reproducibility
/// of the rendered hex, independent of which source actually delivered
/// the bytes.
///
/// On total entropy failure (`getrandom` returned `-1` AND the
/// `/dev/urandom` read failed) the function falls back to
/// `SystemTime::now().subsec_nanos()` and the caller observes the
/// degraded mode via the `app.status` line surfaced by `DetailGuard`.
/// Cryptographic strength is not required — the value only needs to be
/// unguessable enough to avoid lease-id collisions across concurrent
/// `sozu top` instances on the same host.
fn short_random_suffix() -> String {
    let mut buf = [0u8; 4];
    if read_csprng_bytes(&mut buf) {
        let n = u32::from_le_bytes(buf);
        return format!("{n:08x}");
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:08x}")
}

/// Fill `buf` from the kernel CSPRNG. Returns `true` on success, `false`
/// on any error so the caller can fall through to the `subsec_nanos`
/// fallback.
///
/// Linux: `libc::getrandom(buf, len, GRND_NONBLOCK)`. The flag asks the
/// kernel to return `EAGAIN` rather than block when the entropy pool is
/// not yet initialised — extraordinarily rare on real hosts but matters
/// inside fresh containers and at boot. We treat any short read or
/// negative return as failure and fall through.
///
/// Non-Linux Unix targets (macOS / *BSD): `getrandom(2)` exists under
/// different ABIs (e.g. OpenBSD's `getentropy(2)` caps at 256 bytes;
/// FreeBSD's `getrandom(2)` has the same signature as Linux's but
/// belongs to `<sys/random.h>` rather than `<linux/random.h>`). For
/// portability across the platforms Sōzu builds on, fall back to a
/// `/dev/urandom` read — present and readable on every supported
/// non-Linux Unix target.
fn read_csprng_bytes(buf: &mut [u8]) -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `libc::getrandom` accepts a mutable byte pointer + length
        // and writes up to `len` bytes. We pass our owned `buf`'s pointer
        // and full length; both are valid for the duration of the call.
        // The `GRND_NONBLOCK` flag is `0x0001`, well-defined on Linux.
        let ret = unsafe {
            libc::getrandom(
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                libc::GRND_NONBLOCK,
            )
        };
        if ret as usize == buf.len() {
            return true;
        }
        // fall through to `/dev/urandom` below; some kernels (very old
        // 3.x or seccomp-restricted sandboxes) refuse the syscall.
    }
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(buf).is_ok()
    {
        return true;
    }
    false
}
