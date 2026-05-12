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
//! # Single-owner topology
//!
//! One owner thread owns the `Channel<Request, Response>`. Every write —
//! initial apply, periodic renew, and final clear — flows through that
//! thread via a `crossbeam_channel::Sender<DetailRequest>`. The renewer
//! thread holds only the sender clone and a stop flag; it owns no
//! `Channel` of its own.
//!
//! Why: the master stamps a fresh `peer_session_ulid` on every
//! `ClientSession` it accepts. The earlier design opened one `Channel` for
//! the apply path and a separate `Channel` for the renewer thread, so the
//! renewer's writes carried a *different* session ulid than the apply.
//! The worker's `PeerBinding` table was rebound on each renewal, with two
//! consequences this module structurally prevents now:
//!
//! 1. After the first renewal (~ttl/2 seconds), the apply channel's own
//!    `clear`-on-Drop failed as `Unauthorized` because the lease's bound
//!    `peer_session_ulid` no longer matched the apply channel. The TUI
//!    could not revoke its own lease; the worker had to wait for TTL
//!    expiry on every exit.
//! 2. A same-UID actor who guessed the `client_id` format
//!    (`top:<pid>:<8 hex>`; PID observable, 8 hex needs ~4 billion
//!    attempts) could send their own renewal from a separate session,
//!    overwrite the binding, and then clear the lease. The peer-credential
//!    binding's stated purpose is exactly to stop same-UID actors from
//!    clearing another lease; the renewal path defeated that promise.
//!
//! Routing every write through one owner thread means the worker always
//! sees the same `peer_session_ulid` for a given guard, so binding
//! overwrite from the legitimate path is structurally impossible.

use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
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
pub(super) fn publish_status(slot: &StatusSlot, msg: String) {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    };
    *g = Some(msg);
}

/// Owner-thread mailbox messages. The owner thread holds the one
/// `Channel<Request, Response>` for this guard; every write goes
/// through one of these variants. See module docs for the rationale.
enum DetailRequest {
    /// Initial apply. The reply oneshot carries the outcome so
    /// `DetailGuard::apply` can fail fast and surface the error to the
    /// caller (worker too old, transport rejected the verb, …) before
    /// returning the guard.
    Apply { reply: Sender<Result<(), CtlError>> },
    /// Periodic renew, sent by the renewer thread once per `ttl/2`.
    /// The owner thread reconstructs the request from its cached
    /// `client_id` / `detail` / `ttl_seconds` / `reason`. On failure
    /// the owner publishes a status note and exits; that breaks the
    /// renewer's send loop too (its sender clone fails).
    Renew,
    /// Best-effort revoke, sent by `Drop`. The owner attempts the
    /// write and ignores the outcome — TTL expiry is the backstop.
    Clear,
    /// Graceful owner-thread exit, sent by `Drop` after `Clear`.
    Stop,
}

/// RAII guard that holds a runtime cardinality lease while the TUI runs.
/// Drop clears the lease (best-effort) so the worker drops back to its
/// configured floor. Crash-safe: even if Drop never runs, the lease
/// self-expires after `ttl_seconds`.
pub struct DetailGuard {
    /// Mailbox to the owner thread. `Apply` / `Renew` / `Clear` / `Stop`
    /// all flow through this one sender so every wire-level write is
    /// emitted on a single `Channel` connection (and therefore a single
    /// master-assigned `peer_session_ulid`). See module docs.
    tx: Sender<DetailRequest>,
    /// Owner-thread join handle. Joined on Drop after `Stop` is sent.
    owner_handle: Option<JoinHandle<()>>,
    /// Renewer-thread stop flag. The renewer checks this before each
    /// send so a Drop racing with the timer's wake-up does not emit a
    /// stale `Renew`.
    renewer_stop: Arc<AtomicBool>,
    /// Fast-wake signal for the renewer thread. The renewer waits on
    /// `select!(after(ttl/2), recv(renewer_wake_rx))`; dropping the
    /// sender wakes it immediately so Drop does not block for `ttl/2`.
    renewer_wake_tx: Option<Sender<()>>,
    /// Renewer-thread join handle.
    renewer_handle: Option<JoinHandle<()>>,
    /// Stable identifier for this `sozu top` instance, of the shape
    /// `top:<pid>:<random>`. Kept for any debug surface; the owner
    /// thread caches its own copy for request construction.
    #[allow(dead_code)]
    client_id: String,
    /// Shared status slot the renewer thread publishes degraded-mode
    /// messages into. Stored on the guard so it stays alive for the
    /// background threads' lifetime; the render loop drains it via the
    /// free `take_status` function once per tick.
    #[allow(dead_code)]
    status: StatusSlot,
}

impl DetailGuard {
    /// Open a fresh `Channel` to the master, hand it to a single owner
    /// thread, send the initial `SetMetricDetail` apply over that
    /// thread, and spawn the renewer. Returns `Ok` once the master
    /// acknowledges the apply; if the master rejects (e.g. mixed-version
    /// fleet without the verb) `Err` is returned and the caller shows
    /// the "lease unsupported" warning in the status bar. The `status`
    /// slot is shared with the render loop so the renewer can surface
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
        // One `Channel` connection for this guard's entire lifetime.
        // Handed off to the owner thread, which is the sole writer.
        let channel = create_channel(config)?;

        // Mailbox to the owner thread (apply / renew / clear / stop).
        // Unbounded: traffic is one apply at startup, one renew every
        // ttl/2, one clear + one stop at drop. The renewer never
        // out-paces the owner.
        let (tx, rx) = crossbeam_channel::unbounded::<DetailRequest>();

        let owner_handle = spawn_owner(
            channel,
            rx,
            client_id.clone(),
            proto_detail,
            ttl_seconds,
            reason.clone(),
            Arc::clone(&status),
        );

        // Initial apply: send through the mailbox and wait for the
        // owner's reply so the caller observes the master's verdict
        // before we return the guard.
        let (apply_reply_tx, apply_reply_rx) = bounded::<Result<(), CtlError>>(1);
        if tx
            .send(DetailRequest::Apply {
                reply: apply_reply_tx,
            })
            .is_err()
        {
            // Owner thread refused the message — it has already exited.
            // Wait for the handle so we surface its outcome cleanly,
            // then return a transport error.
            let _ = owner_handle.join();
            return Err(CtlError::WriteRequest(
                sozu_command_lib::channel::ChannelError::Connection(None),
            ));
        }
        let apply_result = match apply_reply_rx.recv() {
            Ok(r) => r,
            Err(_) => {
                let _ = owner_handle.join();
                return Err(CtlError::WriteRequest(
                    sozu_command_lib::channel::ChannelError::Connection(None),
                ));
            }
        };
        if let Err(e) = apply_result {
            // Owner thread exits on apply failure; reap it so we don't
            // leave a zombie behind.
            let _ = owner_handle.join();
            return Err(e);
        }

        // Spawn the renewer now that the apply succeeded.
        let renewer_stop = Arc::new(AtomicBool::new(false));
        let (renewer_wake_tx, renewer_wake_rx) = bounded::<()>(0);
        let renewer_handle = spawn_renewer(
            tx.clone(),
            ttl_seconds,
            Arc::clone(&renewer_stop),
            renewer_wake_rx,
        );

        Ok(Self {
            tx,
            owner_handle: Some(owner_handle),
            renewer_stop,
            renewer_wake_tx: Some(renewer_wake_tx),
            renewer_handle: Some(renewer_handle),
            client_id,
            status,
        })
    }
}

impl Drop for DetailGuard {
    fn drop(&mut self) {
        // 1. Stop the renewer before issuing the final write so its
        //    next tick cannot enqueue a `Renew` that would race the
        //    `Clear`. The atomic short-circuits the post-wake send;
        //    dropping the wake sender breaks the select! sleep so the
        //    renewer exits in <1 ms instead of waiting for ttl/2.
        self.renewer_stop.store(true, Ordering::Relaxed);
        drop(self.renewer_wake_tx.take());
        if let Some(handle) = self.renewer_handle.take() {
            let _ = handle.join();
        }

        // 2. Best-effort revoke over the same owner-thread channel
        //    that did the apply. Sender errors are ignored: if the
        //    owner has already exited (apply or renew failed before
        //    Drop), TTL expiry is the backstop.
        let _ = self.tx.send(DetailRequest::Clear);

        // 3. Graceful owner exit.
        let _ = self.tx.send(DetailRequest::Stop);
        if let Some(handle) = self.owner_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the owner thread. It owns the `Channel`, caches the lease
/// parameters, and dispatches `DetailRequest`s into wire-level
/// `SetMetricDetail` writes.
fn spawn_owner(
    mut channel: Channel<Request, Response>,
    rx: Receiver<DetailRequest>,
    client_id: String,
    detail: MetricDetail,
    ttl_seconds: u32,
    reason: String,
    status: StatusSlot,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("sozu-top-detail-owner".into())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    DetailRequest::Apply { reply } => {
                        let result = send_set_detail(
                            &mut channel,
                            &client_id,
                            Some(detail),
                            Some(ttl_seconds),
                            Some(&reason),
                            false,
                        );
                        let failed = result.is_err();
                        // Reply may be dropped if `apply` was cancelled
                        // (the caller already returned). Swallow.
                        let _ = reply.send(result);
                        if failed {
                            // The initial apply is load-bearing; if it
                            // failed there is no point keeping the
                            // owner alive. The renewer is not spawned
                            // yet (apply gates renewer spawn), so this
                            // is a clean exit.
                            return;
                        }
                    }
                    DetailRequest::Renew => {
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
                    DetailRequest::Clear => {
                        // Best-effort: server-side TTL expiry covers
                        // dropped revokes. We do not surface failures
                        // through the status slot because the TUI is
                        // already tearing down by this point.
                        let _ = send_set_detail(
                            &mut channel,
                            &client_id,
                            None,
                            None,
                            Some(&format!("{reason} (clear)")),
                            true,
                        );
                    }
                    DetailRequest::Stop => return,
                }
            }
            // Sender dropped without sending Stop: treat as implicit
            // shutdown.
        })
        .expect("spawn sozu-top owner")
}

/// Spawn the renewer thread. It holds only a sender clone and a stop
/// flag — no `Channel` of its own. Every renewal write traverses the
/// owner thread, which preserves the apply-time `peer_session_ulid` and
/// therefore the worker's `PeerBinding`.
fn spawn_renewer(
    tx: Sender<DetailRequest>,
    ttl_seconds: u32,
    stop: Arc<AtomicBool>,
    wake_rx: Receiver<()>,
) -> JoinHandle<()> {
    let renew_after = Duration::from_secs((ttl_seconds.max(2) / 2) as u64);
    std::thread::Builder::new()
        .name("sozu-top-detail-renewer".into())
        .spawn(move || {
            loop {
                let timer = after(renew_after);
                select! {
                    recv(timer) -> _ => {
                        // Post-wake stop check: Drop may have set the
                        // flag between the timer's wake and our send.
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if tx.send(DetailRequest::Renew).is_err() {
                            // Owner thread has exited (apply / renew
                            // failure, or Drop completed). Nothing
                            // more to do here; status was already
                            // published by the owner.
                            return;
                        }
                    }
                    recv(wake_rx) -> _ => {
                        // Wake-channel sender dropped (Drop path).
                        // The atomic is already set; exit cleanly.
                        return;
                    }
                }
            }
        })
        .expect("spawn sozu-top renewer")
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

#[cfg(test)]
mod tests {
    //! Mailbox-level tests for the single-owner topology. We cannot run
    //! the owner thread end-to-end without a live master, so the tests
    //! here exercise the renewer cadence and the Drop wake-up shape
    //! against the public mailbox surface. The invariant "every wire
    //! write traverses one `Channel` connection" is enforced
    //! structurally — there is no second `create_channel` call site for
    //! the renewer to reach — so it does not require a behavioural test
    //! beyond reading the code at `spawn_renewer`.
    use super::*;
    use std::time::Instant;

    /// `ttl_seconds = 2` minimum (clamped by `ttl_seconds.max(2)`) yields
    /// a 1 s renewer cadence. The first `Renew` should land on the
    /// mailbox in roughly 1 s — we accept a wide upper bound (3 s) for
    /// CI scheduling noise.
    #[test]
    fn renewer_sends_renew_after_ttl_half() {
        let (tx, rx) = crossbeam_channel::unbounded::<DetailRequest>();
        let stop = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = bounded::<()>(0);

        let start = Instant::now();
        let handle = spawn_renewer(tx, 2, Arc::clone(&stop), wake_rx);

        // First renew should arrive within ttl/2 (=1 s) + slack.
        let msg = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("renewer produced no Renew within 3 s");
        assert!(
            matches!(msg, DetailRequest::Renew),
            "first mailbox message must be Renew"
        );
        assert!(
            start.elapsed() >= Duration::from_millis(900),
            "renewer fired too early: {:?}",
            start.elapsed()
        );

        // Tell the renewer to stop and verify it exits promptly.
        stop.store(true, Ordering::Relaxed);
        drop(wake_tx);
        handle.join().expect("renewer panicked");
    }

    /// Dropping the wake sender mid-sleep must wake the renewer in
    /// well under `ttl/2`. This guards the Drop-fast-path promise — if
    /// `select!` regresses to a pure timer, this assertion catches it.
    #[test]
    fn renewer_wakes_on_drop() {
        let (tx, _rx) = crossbeam_channel::unbounded::<DetailRequest>();
        let stop = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = bounded::<()>(0);

        // ttl_seconds = 60 → renewer would otherwise sleep 30 s.
        let handle = spawn_renewer(tx, 60, Arc::clone(&stop), wake_rx);

        // Give the renewer time to reach the select!.
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        drop(wake_tx);

        let start = Instant::now();
        handle.join().expect("renewer panicked");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "renewer did not wake on drop: {:?}",
            start.elapsed()
        );
    }
}
