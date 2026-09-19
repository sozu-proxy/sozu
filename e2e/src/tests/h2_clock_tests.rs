//! End-to-end tests for the H2 mux's two *connection-level* deadlines, driven
//! from a raw TLS+ALPN client against a real worker.
//!
//! These guard the behaviours whose only input is a clock reading, at the one
//! layer that can falsify them — the wire. They exist because the H2 core's
//! clock plumbing was reworked (`Mux` now samples `Context::now` once per
//! `ready()` pass and `ConnectionH2` mirrors it into `self.now`, so the flood
//! detector and every deadline weigh a burst against ONE instant). That rework
//! is deliberately not wire-visible, which is exactly why it needs tests that
//! assert the *behaviour* rather than the clock plumbing: reverting a
//! `self.now` read to `Instant::now()` reads a second clock and behaves
//! identically, so no end-to-end test can — or should — catch it. Every
//! `To SEE THIS RED:` below therefore names a mutation that breaks the
//! behaviour itself.
//!
//! ## Test list
//! 1. [`test_h2_flood_window_decays_between_bursts`] — the flood detector's
//!    sliding window really decays: 120 PINGs (threshold is 100) spread over
//!    three bursts one and a half seconds apart do NOT trip
//!    `GOAWAY(ENHANCE_YOUR_CALM)`, and the connection still serves a request
//!    afterwards. This is the negative space of the CVE-2019-9512 tests in
//!    `h2_tests.rs`, which only ever prove the threshold *trips*. A detector
//!    whose window never advances kills a legitimate slow client.
//! 2. [`test_h2_settings_ack_timeout_goaways_the_frontend`] — a client that
//!    never ACKs Sōzu's SETTINGS is met with `GOAWAY(SETTINGS_TIMEOUT = 0x4)`
//!    (RFC 9113 §6.5.3), and not before the budget has elapsed.
//!    `h2_tests.rs::test_h2_settings_ack_timeout` covers the *backend* side of
//!    the same watchdog but asserts only that the worker survives, so it stays
//!    green with the deadline deleted; this one asserts the GOAWAY and its
//!    error code.
//!
//! The graceful-shutdown forced-close deadline — the third clock-driven
//! behaviour the rework touches — is already covered end-to-end by
//! `h2_tests.rs::{test_h2_graceful_shutdown_timeout_forces_close,
//! test_h2_graceful_shutdown_deadline_configurable_short,
//! test_h2_graceful_shutdown_deadline_configurable_long}`, which assert the
//! stop latency against the configured `h2_graceful_shutdown_deadline_seconds`.
//! Nothing is added here for it.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};

use super::h2_utils::{
    H2_CLIENT_PREFACE, H2_FLAG_ACK, H2_FLAG_END_STREAM, H2_FRAME_DATA, H2_FRAME_PING,
    H2_FRAME_SETTINGS, H2Frame, build_chrome146_get_headers, contains_goaway, goaway_error_code,
    h2_handshake, log_frames, parse_h2_frames, raw_h2_connection, setup_h2_listener_only,
    setup_h2_test, teardown, verify_sozu_alive,
};
use crate::tests::{State, repeat_until_error_or};

/// `SETTINGS_TIMEOUT` (0x4) — RFC 9113 §7. Sōzu's answer to a peer that never
/// acknowledges its SETTINGS.
const H2_ERROR_SETTINGS_TIMEOUT: u32 = 0x4;

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// Short socket read timeout for the bounded read loops below, mirroring
/// `h2_priority_rearm_tests.rs`. `raw_h2_connection` installs a 2 s timeout,
/// which would make each drain of a quiet connection cost two seconds.
const POLL_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Read from `tls` into the caller-owned `carry` buffer until `done` is
/// satisfied by the frames accumulated SO FAR, or `budget` elapses. Returns
/// every frame seen on the connection since `carry` was created.
///
/// The carry buffer is the point: a GOAWAY that straddles two read calls would
/// be dropped by a helper that re-parses a fresh buffer each time, and the
/// tests below poll a deliberately quiet connection for tens of seconds.
fn pump_frames<F>(
    tls: &mut TlsStream,
    carry: &mut Vec<u8>,
    budget: Duration,
    done: F,
) -> Vec<(u8, u8, u32, Vec<u8>)>
where
    F: Fn(&[(u8, u8, u32, Vec<u8>)]) -> bool,
{
    let start = Instant::now();
    let mut buf = vec![0u8; 65536];
    loop {
        let frames = parse_h2_frames(carry);
        if done(&frames) || start.elapsed() >= budget {
            return frames;
        }
        match tls.read(&mut buf) {
            Ok(0) => return parse_h2_frames(carry),
            Ok(n) => carry.extend_from_slice(&buf[..n]),
            Err(ref error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => return parse_h2_frames(carry),
        }
    }
}

/// Count PING frames carrying the ACK flag — Sōzu's reply to each PING we
/// send. Used as proof that the frames were actually ingested, so "no GOAWAY"
/// cannot pass vacuously on a connection that was never read.
fn count_ping_acks(frames: &[(u8, u8, u32, Vec<u8>)]) -> usize {
    frames
        .iter()
        .filter(|(frame_type, flags, _, _)| {
            *frame_type == H2_FRAME_PING && (*flags & H2_FLAG_ACK) != 0
        })
        .count()
}

// ============================================================================
// Test 1: the flood detector's sliding window decays
// ============================================================================

/// `DEFAULT_MAX_PING_PER_WINDOW` in `lib/src/protocol/mux/h2.rs`. Not a knob
/// this test patches — the listener default is what the CVE-2019-9512 test in
/// `h2_tests.rs` trips, and this test is its negative space.
const PING_THRESHOLD: usize = 100;
/// PINGs per burst. Three bursts put 120 PINGs on the connection, 20 above
/// `PING_THRESHOLD`, so a detector whose window never advances trips.
///
/// With the window advancing, the counter half-decays at each window edge:
/// 40 → (20 + 40) = 60 → (30 + 40) = 70, well under the threshold. Two bursts
/// landing in ONE window still only reach 80, so the test only false-fails if
/// all three bursts are ingested in a single window — a three-second stall of
/// the worker's event loop.
const PINGS_PER_BURST: usize = 40;
const PING_BURSTS: usize = 3;
/// Interval between burst *starts*. `FLOOD_WINDOW_DURATION` is one second, so
/// this leaves half a second of slack before two bursts could share a window.
const BURST_PERIOD: Duration = Duration::from_millis(1500);

/// To SEE THIS RED: in `lib/src/protocol/mux/h2.rs`, change
/// `FLOOD_WINDOW_DURATION` from `from_secs(1)` to `from_secs(3600)` — the
/// sliding window then never advances, the three bursts accumulate to 120
/// PINGs against a threshold of 100, and the third burst is answered with
/// `GOAWAY(ENHANCE_YOUR_CALM)` instead of PING ACKs. Inverting
/// `maybe_reset_window`'s comparison, or making its body a no-op, reddens it
/// the same way.
///
/// What this does NOT guard: which clock the detector reads. Swapping
/// `check_flood(self.now)` back to an internal `Instant::now()` leaves this
/// test green, because a second clock reading of the same wall time decays the
/// window identically. Only the window's existence and its duration are
/// falsifiable from the wire; the single-snapshot property is a unit-test
/// concern (`h2.rs::tests`).
fn try_h2_flood_window_decays_between_bursts() -> State {
    let (worker, backends, front_port) = setup_h2_test("H2-FLOOD-WINDOW-DECAY", 1);
    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();

    let mut tls = raw_h2_connection(front_addr);
    h2_handshake(&mut tls);
    tls.sock.set_read_timeout(Some(POLL_READ_TIMEOUT)).ok();

    let mut carry = Vec::new();
    let mut goaway_during_bursts = false;

    for burst in 0..PING_BURSTS {
        let burst_started = Instant::now();

        let mut wire = Vec::with_capacity(PINGS_PER_BURST * 17);
        for index in 0..PINGS_PER_BURST {
            let mut payload = [0u8; 8];
            let tag = (burst * PINGS_PER_BURST + index) as u32;
            payload[0..4].copy_from_slice(&tag.to_be_bytes());
            wire.extend_from_slice(&H2Frame::ping(payload).encode());
        }
        if tls.write_all(&wire).and_then(|_| tls.flush()).is_err() {
            println!(
                "H2 flood window decay - burst {burst}: write failed, peer closed the connection"
            );
            goaway_during_bursts = true;
            break;
        }

        // Drain this burst's ACKs. Stop as soon as they are all in so the
        // inter-burst interval stays governed by BURST_PERIOD.
        let expected_acks = (burst + 1) * PINGS_PER_BURST;
        let frames = pump_frames(&mut tls, &mut carry, Duration::from_millis(900), |frames| {
            count_ping_acks(frames) >= expected_acks || contains_goaway(frames)
        });
        if contains_goaway(&frames) {
            log_frames("H2 flood window decay", &frames);
            goaway_during_bursts = true;
            break;
        }

        let elapsed = burst_started.elapsed();
        if burst + 1 < PING_BURSTS && elapsed < BURST_PERIOD {
            thread::sleep(BURST_PERIOD - elapsed);
        }
    }

    // A request on the SAME connection: "no GOAWAY" alone would also hold on a
    // connection Sōzu had quietly stopped serving.
    let mut served = false;
    if !goaway_during_bursts {
        let block = build_chrome146_get_headers("localhost", "/api/decay", None);
        if tls
            .write_all(&H2Frame::headers(1, block, true, true).encode())
            .and_then(|_| tls.flush())
            .is_ok()
        {
            let frames = pump_frames(&mut tls, &mut carry, Duration::from_secs(5), |frames| {
                frames.iter().any(|(_, flags, stream_id, _)| {
                    *stream_id == 1 && (*flags & H2_FLAG_END_STREAM) != 0
                })
            });
            served = !contains_goaway(&frames)
                && frames.iter().any(|(frame_type, _, stream_id, payload)| {
                    *frame_type == H2_FRAME_DATA
                        && *stream_id == 1
                        && payload.windows(5).any(|window| window == b"pong0")
                });
            if !served {
                log_frames("H2 flood window decay - final request", &frames);
            }
        }
    }

    let all_frames = parse_h2_frames(&carry);
    let ping_acks = count_ping_acks(&all_frames);
    println!(
        "H2 flood window decay - sent {} PINGs in {PING_BURSTS} bursts (threshold {PING_THRESHOLD}), \
         ping_acks={ping_acks}, goaway={goaway_during_bursts}, served={served}",
        PING_BURSTS * PINGS_PER_BURST
    );

    let infra_ok = teardown(tls, front_port, worker, backends);

    if infra_ok && !goaway_during_bursts && served && ping_acks >= PINGS_PER_BURST {
        State::Success
    } else {
        println!(
            "FAIL: infra_ok={infra_ok} goaway_during_bursts={goaway_during_bursts} \
             served={served} ping_acks={ping_acks}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_flood_window_decays_between_bursts() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 flood detector: the sliding window decays, so a sustained sub-threshold \
             PING rate is not a flood",
            try_h2_flood_window_decays_between_bursts,
        ),
        State::Success,
    );
}

// ============================================================================
// Test 2: the SETTINGS-ACK deadline GOAWAYs an un-ACKing frontend
// ============================================================================

/// Mirror of `SETTINGS_ACK_TIMEOUT` in `lib/src/protocol/mux/h2.rs`.
const SETTINGS_ACK_BUDGET: Duration = Duration::from_secs(5);
/// When the "it has NOT fired yet" probe runs, measured from the moment our
/// SETTINGS reaches the wire. Sōzu arms its watchdog when it serialises its
/// own SETTINGS in reply, so its elapsed time can only be SHORTER than ours —
/// the probe cannot race the budget from below.
const EARLY_PROBE_AT: Duration = Duration::from_millis(2500);
/// Upper bound on how long we wait for the GOAWAY. Generously above the 5 s
/// budget: the assertion that matters is that the GOAWAY arrives *at all* and
/// carries `SETTINGS_TIMEOUT`, not that it lands on a precise tick. Kept below
/// the 60 s `front_timeout` default so a frontend timeout can never be what
/// closes the connection.
const GOAWAY_DEADLINE: Duration = Duration::from_secs(25);
/// Gap between the PING pokes that give the event loop something to wake on.
const POKE_INTERVAL: Duration = Duration::from_millis(500);

/// To SEE THIS RED, two independent mutations in `lib/src/protocol/mux/h2.rs`:
///
/// * the GOAWAY half — change `SETTINGS_ACK_TIMEOUT` from `from_secs(5)` to
///   `from_secs(3600)`. The watchdog in `readable()` and
///   `flush_pending_control_frames()` never fires, no GOAWAY ever reaches the
///   client, and `goaway_error` stays `None`. Deleting both
///   `return self.goaway(H2Error::SettingsTimeout)` statements reddens it the
///   same way.
/// * the not-too-early half — flip either `>=` in those two
///   `self.now.saturating_duration_since(sent_at) >= SETTINGS_ACK_TIMEOUT`
///   guards to `<`. The GOAWAY is then emitted on the first pass after the
///   handshake and `early_goaway` is true.
///
/// What this does NOT guard: which clock the deadline is measured against.
/// Restoring `sent_at.elapsed()` in place of
/// `self.now.saturating_duration_since(sent_at)` leaves this test green — both
/// expressions yield the same elapsed time to within an event-loop pass. That
/// distinction is a unit-test concern
/// (`h2.rs::tests::settings_ack_deadline_is_evaluated_against_the_connection_snapshot`);
/// what is falsifiable from the wire is the deadline's existence, its
/// direction, and the error code it carries.
fn try_h2_settings_ack_timeout_goaways_the_frontend() -> State {
    let (mut worker, front_port, _front_address) =
        setup_h2_listener_only("H2-SETTINGS-ACK-DEADLINE");
    worker.read_to_last();

    let front_addr: SocketAddr = format!("127.0.0.1:{front_port}").parse().unwrap();
    let mut tls = raw_h2_connection(front_addr);
    tls.sock.set_read_timeout(Some(POLL_READ_TIMEOUT)).ok();

    // Half a handshake: preface + our SETTINGS, and deliberately never the
    // SETTINGS ACK. Sōzu arms the RFC 9113 §6.5 watchdog when it serialises
    // its own SETTINGS in reply and disarms it only on our ACK.
    let handshake_started = Instant::now();
    let sent = tls
        .write_all(H2_CLIENT_PREFACE)
        .and_then(|_| tls.write_all(&H2Frame::settings(&[]).encode()))
        .and_then(|_| tls.flush())
        .is_ok();
    if !sent {
        println!("H2 SETTINGS-ACK deadline - could not send the client preface");
        let _ = verify_sozu_alive(front_port);
        worker.soft_stop();
        let _ = worker.wait_for_server_stop();
        return State::Fail;
    }

    let mut carry = Vec::new();
    let frames = pump_frames(&mut tls, &mut carry, Duration::from_secs(2), |frames| {
        frames.iter().any(|(frame_type, flags, _, _)| {
            *frame_type == H2_FRAME_SETTINGS && (*flags & H2_FLAG_ACK) == 0
        })
    });
    // Without Sōzu's own SETTINGS the watchdog was never armed and the rest of
    // the test would assert nothing.
    let watchdog_armed = frames.iter().any(|(frame_type, flags, _, _)| {
        *frame_type == H2_FRAME_SETTINGS && (*flags & H2_FLAG_ACK) == 0
    });

    // Probe well inside the budget: the deadline must not have fired.
    let early_elapsed = handshake_started.elapsed();
    if early_elapsed < EARLY_PROBE_AT {
        thread::sleep(EARLY_PROBE_AT - early_elapsed);
    }
    let early_poke = tls
        .write_all(&H2Frame::ping([0xE0; 8]).encode())
        .and_then(|_| tls.flush())
        .is_ok();
    let early_frames = pump_frames(
        &mut tls,
        &mut carry,
        Duration::from_millis(500),
        contains_goaway,
    );
    let early_goaway = contains_goaway(&early_frames);
    if early_goaway {
        log_frames("H2 SETTINGS-ACK deadline - early probe", &early_frames);
    }

    // Past the budget, poll for the GOAWAY. The deadline is only evaluated
    // inside `readable()` / `flush_pending_control_frames()`, so a silent
    // connection is never re-examined: each PING is what gives the event loop
    // something to wake on. Thirty-odd PINGs over the whole window stay far
    // below the 100/window flood threshold.
    let mut goaway_error = None;
    while handshake_started.elapsed() < GOAWAY_DEADLINE {
        let frames = pump_frames(&mut tls, &mut carry, POKE_INTERVAL, contains_goaway);
        if contains_goaway(&frames) {
            log_frames("H2 SETTINGS-ACK deadline", &frames);
            goaway_error = goaway_error_code(&frames);
            break;
        }
        if tls
            .write_all(&H2Frame::ping([0xA5; 8]).encode())
            .and_then(|_| tls.flush())
            .is_err()
        {
            // Peer closed: drain whatever is still buffered — the GOAWAY may
            // already be on the wire ahead of the FIN.
            let frames = pump_frames(
                &mut tls,
                &mut carry,
                Duration::from_millis(500),
                contains_goaway,
            );
            log_frames("H2 SETTINGS-ACK deadline - after close", &frames);
            goaway_error = goaway_error_code(&frames);
            break;
        }
    }

    let elapsed = handshake_started.elapsed();
    println!(
        "H2 SETTINGS-ACK deadline - watchdog_armed={watchdog_armed} early_poke={early_poke} \
         early_goaway={early_goaway} goaway_error={goaway_error:?} elapsed={elapsed:?} \
         budget={SETTINGS_ACK_BUDGET:?}"
    );

    drop(tls);
    thread::sleep(Duration::from_millis(200));
    let still_alive = verify_sozu_alive(front_port);
    worker.soft_stop();
    let stopped = worker.wait_for_server_stop();

    if watchdog_armed
        && early_poke
        && !early_goaway
        && goaway_error == Some(H2_ERROR_SETTINGS_TIMEOUT)
        && still_alive
        && stopped
    {
        State::Success
    } else {
        println!(
            "FAIL: watchdog_armed={watchdog_armed} early_poke={early_poke} \
             early_goaway={early_goaway} goaway_error={goaway_error:?} \
             still_alive={still_alive} stopped={stopped}"
        );
        State::Fail
    }
}

#[test]
fn test_h2_settings_ack_timeout_goaways_the_frontend() {
    assert_eq!(
        repeat_until_error_or(
            2,
            "H2 SETTINGS-ACK deadline: a frontend that never ACKs is met with \
             GOAWAY(SETTINGS_TIMEOUT), and not before the budget",
            try_h2_settings_ack_timeout_goaways_the_frontend,
        ),
        State::Success,
    );
}
