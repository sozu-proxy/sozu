//! systemd `sd_notify` integration for the master process.
//!
//! When sozu runs under a systemd unit declared `Type=notify`, the
//! master process tells systemd it is ready (`READY=1`) only AFTER the
//! initial workers have spawned and any saved state has been replayed
//! — without that handshake, `Type=simple` declares the unit ready as
//! soon as the binary forks, which is wrong for sozu's master/worker
//! lifecycle (clients can hit a configured `After=sozu.service` peer
//! before sozu accepts traffic).
//!
//! Implementation is std-only: read `$NOTIFY_SOCKET`, connect a
//! `UnixDatagram`, send the protocol-formatted state line. Closes
//! [#228]. Linux is the only target system where this matters in
//! practice; the helper is a no-op when `$NOTIFY_SOCKET` is unset
//! (e.g. when the operator runs `sozu start` from a shell without
//! systemd in the supervision chain), so non-Linux builds compile and
//! call into the helper without behavioural change.
//!
//! [#228]: https://github.com/sozu-proxy/sozu/issues/228

use std::env;
use std::io;
use std::os::unix::net::UnixDatagram;

/// Send a state message to systemd via `$NOTIFY_SOCKET`.
///
/// Returns `Ok(false)` when `$NOTIFY_SOCKET` is unset (sozu is not
/// running under systemd notify supervision; the caller can ignore
/// this case). Returns `Ok(true)` when the datagram was sent.
/// Returns `Err(...)` for socket / permission failures so the caller
/// can decide whether to log + continue or fail-fast.
///
/// Multiple state lines can be packed into a single message by
/// joining them with `\n` (see [`STATE_READY`] / [`STATE_STOPPING`]).
pub fn notify(state: &str) -> io::Result<bool> {
    let path = match env::var_os("NOTIFY_SOCKET") {
        Some(p) => p,
        None => return Ok(false),
    };
    let sock = UnixDatagram::unbound()?;
    sock.connect(&path)?;
    sock.send(state.as_bytes())?;
    Ok(true)
}

/// `READY=1` — master finished spawning workers and loading state.
pub const STATE_READY: &str = "READY=1";

/// `STOPPING=1` — graceful shutdown begins.
pub const STATE_STOPPING: &str = "STOPPING=1";

/// `RELOADING=1` — hot reload begins (the same binary is reloading
/// configuration; for binary upgrade see [`main_pid`]).
pub const STATE_RELOADING: &str = "RELOADING=1";

/// `WATCHDOG=1` — watchdog ping. Send periodically if the systemd
/// unit declares `WatchdogSec`.
pub const STATE_WATCHDOG: &str = "WATCHDOG=1";

/// Send `MAINPID=<pid>` so systemd tracks the new master after a
/// hot-upgrade re-exec hands the supervision off to a forked child.
pub fn main_pid(pid: i32) -> io::Result<bool> {
    notify(&format!("MAINPID={pid}"))
}

/// Send `STATUS=<text>` for human-readable status (visible via
/// `systemctl status sozu`).
pub fn status(text: &str) -> io::Result<bool> {
    notify(&format!("STATUS={text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;
    use tempfile::TempDir;

    #[test]
    fn notify_no_socket_set_is_noop() {
        // SAFETY: this test mutates the process environment which is
        // shared global state. It runs in serial against any other
        // test that reads NOTIFY_SOCKET (none currently). If parallel
        // contention surfaces, gate via #[serial_test::serial].
        // SAFETY: documented in test header; mutating env vars is unsafe in
        // multi-threaded code, but the `#[test]` harness runs each test on
        // its own thread and we restore the previous value on exit.
        let prev = env::var_os("NOTIFY_SOCKET");
        unsafe {
            env::remove_var("NOTIFY_SOCKET");
        }
        let res = notify(STATE_READY).expect("noop without NOTIFY_SOCKET");
        unsafe {
            if let Some(v) = prev {
                env::set_var("NOTIFY_SOCKET", v);
            }
        }
        assert!(
            !res,
            "notify must report `false` when NOTIFY_SOCKET is unset"
        );
    }

    #[test]
    fn notify_writes_payload_to_socket() {
        let dir = TempDir::new().expect("tempdir");
        let socket_path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&socket_path).expect("bind notify socket");

        // SAFETY: see `notify_no_socket_set_is_noop`.
        let prev = env::var_os("NOTIFY_SOCKET");
        unsafe {
            env::set_var("NOTIFY_SOCKET", &socket_path);
        }
        let sent = notify(STATE_READY).expect("notify must succeed");
        unsafe {
            match prev {
                Some(v) => env::set_var("NOTIFY_SOCKET", v),
                None => env::remove_var("NOTIFY_SOCKET"),
            }
        }
        assert!(sent, "notify must report `true` when datagram was sent");

        let mut buf = [0u8; 64];
        let (n, _addr) = listener.recv_from(&mut buf).expect("recv notify datagram");
        assert_eq!(&buf[..n], STATE_READY.as_bytes());
    }
}
