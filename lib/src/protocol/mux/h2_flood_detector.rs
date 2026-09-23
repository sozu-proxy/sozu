//! H2 flood/abuse detection for [`super::h2::ConnectionH2`] — the CVE-2023-44487
//! (Rapid Reset), CVE-2024-27316 (CONTINUATION flood) and CVE-2025-8671
//! ("MadeYouReset") mitigations, plus the PING/SETTINGS/empty-DATA/stream-0
//! WINDOW_UPDATE rate limits and the general glitch counter.
//!
//! Groups [`H2FloodConfig`] (the CVE-tagged thresholds), [`H2FloodViolation`]
//! (what a tripped threshold reports back) and [`H2FloodDetector`] (the
//! counters themselves) behind the same closed-API shape
//! [`super::hpack_state::HpackState`], [`super::h2_flow_control::H2FlowControl`]
//! and [`super::h2_stream_table::H2StreamTable`] established in the three
//! prior extraction steps. Every `H2FloodDetector` field is private to this
//! module; `ConnectionH2` reaches them only through the accessor/mutator
//! methods declared below — `check_flood_or_return!` and the per-frame
//! `record_*` calls in `h2.rs` are the only way counters move.
//!
//! `H2FloodConfig` follows the same "closed API" shape: its fields are
//! private and [`H2FloodConfig::new`] is the only way to build one, even
//! though the type itself is `pub` because `lib/src/http.rs` and
//! `lib/src/https.rs` hand it their listener configuration.
//! `MAX_HEADER_LIST_SIZE` (the compile-time default for
//! `H2FloodConfig::max_header_list_size`) stays declared in `h2.rs` for the
//! same reason in reverse: `converter.rs` and `pkawa.rs` reference it
//! directly as a general HPACK encode/decode safety ceiling, independent of
//! any one connection's configured value, so moving it would widen this
//! extraction's blast radius into two unrelated modules for no benefit. This
//! module reaches it via `super::h2::MAX_HEADER_LIST_SIZE`, same as
//! `converter.rs`/`pkawa.rs` do.
//!
//! The fields were `pub` until sozu-proxy/sozu#1418, and that is exactly what
//! went wrong: `lib/src/http.rs`/`lib/src/https.rs` built `H2FloodConfig` with
//! a raw struct literal and never called `H2FloodConfig::new`, so the `.max(1)`
//! clamp below was bypassed on the only production path that carries
//! operator-configured thresholds — only `Default::default()`'s compile-time
//! constants, which never needed validation, went through it. An operator
//! setting a threshold to zero got it unclamped, and since `check_flood`
//! compares `count > threshold`, the first event that counter saw was already
//! a violation — for `max_header_list_size`/`max_header_fields`, which are
//! also the HPACK decode budget, that is every request on the listener; for a
//! per-window or lifetime frame counter, the first client that sends the frame
//! it counts. Private fields make that literal a compile error instead of a
//! comment, which is what the three sibling extractions
//! (`hpack_state`, `h2_flow_control`, `h2_stream_table`) already did for their
//! own invariants.
//!
//! This module owns exactly what `check_flood` polices:
//!
//! - Per-window rate counters (`rst_stream_count`, `ping_count`,
//!   `settings_count`, `empty_data_count`, `window_update_stream0_count`,
//!   `glitch_count`), half-decayed by [`H2FloodDetector::check_flood`] every
//!   [`FLOOD_WINDOW_DURATION`] rather than reset to zero, so a
//!   burst-then-wait attacker cannot escape by timing a pause exactly at the
//!   window edge.
//! - Never-decaying lifetime ceilings (`total_rst_received_lifetime`,
//!   `total_abusive_rst_received_lifetime`, `total_rst_streams_emitted_lifetime`,
//!   `total_ping_received_lifetime`, `total_settings_received_lifetime`) that
//!   put an absolute cap on a patient attacker who stays under the
//!   half-decaying per-window threshold forever.
//! - The per-header-block CONTINUATION accounting (`continuation_count`,
//!   `accumulated_header_size`), reset by [`H2FloodDetector::reset_continuation`]
//!   once a block completes.
//!
//! ## Clock: the last self-sampling exception is gone
//!
//! Before this extraction, `impl Default for H2FloodDetector` called
//! `Instant::now()` — documented as test-only (LIFECYCLE.md invariant 20) and
//! verified so here too: the sole production constructor is
//! `ConnectionH2::new`, which calls `H2FloodDetector::new(flood_config, now)`
//! with its own single accept-time sample, and `Default` was reachable from
//! nowhere else (`grep -rn 'H2FloodDetector::default\|H2FloodDetector::new'`
//! outside `h2.rs`/`h2_flood_detector.rs` returns nothing; every `default()`
//! call site was inside `h2.rs`'s `#[cfg(test)] mod tests`, `h2.rs:6946`
//! onward at the pre-extraction revision).
//!
//! A test can pass a clock as easily as inherit one, and every test that used
//! `H2FloodDetector::default()` only needed *a* instant, never a specific
//! one — so this extraction removes `impl Default` entirely rather than
//! moving it. Every former `H2FloodDetector::default()` call site below now
//! reads `H2FloodDetector::new(H2FloodConfig::default(), Instant::now())`,
//! and the old test comparing default-constructed vs. explicitly-constructed
//! detectors — whose entire premise was comparing `default()` against
//! `new()` — is deleted along with it, rather than adapted, because there is
//! no longer a second constructor for it to compare against. This closes
//! LIFECYCLE.md invariant 20's one
//! remaining exception: the only code under `ConnectionH2` that ever samples
//! the clock is now `ConnectionH2::new` itself.
//!
//! ## Determinism — there is none to fix, because there is no collection
//!
//! Enumerated by reading every field, not by grepping `.iter()/.keys()/
//! .values()/.drain()` (which is what missed the two leaks the prior two
//! extractions each found — both were a bare `for (&k, &v) in &map` loop).
//! `H2FloodDetector` holds no map, set, or `Vec` at all: every field is a
//! scalar counter (`u32`/`u64`), a `Copy` config struct, or a single
//! `Instant`. `check_flood`'s `.or_else()` chain — which counter is checked,
//! and therefore which violation is reported first when several are
//! simultaneously over threshold — is a fixed sequence written directly into
//! the source text, not an iteration over anything whose order could vary
//! between two runs of the same binary. There is nothing here for a
//! `BTreeMap` conversion to fix, unlike `h2_flow_control::H2FlowControl`'s
//! `pending_window_updates` or `h2_stream_table::H2StreamTable`'s
//! `stream_last_activity_at`/`stream_fc_stalled_since`.
//!
//! ## Security counters — every increment is an unmodified relocation
//!
//! This module implements the CVE-2023-44487 (Rapid Reset), CVE-2024-27316
//! (CONTINUATION flood) and CVE-2025-8671 (MadeYouReset) mitigations, plus
//! the PING/SETTINGS/empty-DATA/stream-0-WINDOW_UPDATE rate limits. Every
//! counter's increment operator moved verbatim — same `+=` vs
//! `.saturating_add(...)` choice per field as before this extraction, not
//! normalized to one or the other — and every threshold comparison in
//! `check_flood` is character-for-character the same `count > threshold`
//! check against the same field and the same [`H2FloodConfig`] value. The
//! `record_*` methods below (`record_glitch`, `record_continuation_frame`,
//! `record_empty_data_frame`, `record_rst_stream_window`,
//! `record_settings_frame`, `record_ping_frame`,
//! `record_window_update_stream0`) are new only in the sense that they give
//! the previously-inline `self.flood_detector.<field> += 1` (or
//! `.saturating_add`) sequences a name; the arithmetic inside each is the
//! same statement that used to sit at the `h2.rs` call site, now paired with
//! the same before/after `debug_assert!` that call site carried (or, for
//! `record_glitch`'s five call sites, a new symmetric one — those five
//! previously had no before/after assert at all, unlike every sibling
//! counter; adding it only makes `record_glitch` consistent with its
//! siblings and cannot change release behaviour, since `debug_assert!`
//! compiles to nothing outside debug/test builds).
//! `H2FloodConfig::new`'s `.max(1)` clamp (every threshold is at least 1, so
//! a zero-threshold misconfiguration cannot make the detector trip on the
//! very first frame) is unchanged in what it computes; sozu-proxy/sozu#1418
//! only changed who reaches it, by making the constructor the sole entry.

use std::time::Instant;

use super::{h2::MAX_HEADER_LIST_SIZE, parser::H2Error};

// ── Flood Detection Thresholds (CVE mitigations) ────────────────────────────

/// Default maximum RST_STREAM frames per window (CVE-2023-44487 Rapid Reset + CVE-2019-9514)
const DEFAULT_MAX_RST_STREAM_PER_WINDOW: u32 = 100;
/// Hard lifetime cap on total RST_STREAM frames received on a single
/// connection (CVE-2023-44487 Rapid Reset).
///
/// The per-window counter half-decays, which allows a patient attacker to
/// sustain ~50 RST/sec indefinitely — each one costs the backend a request
/// that will be cancelled before any response work is produced. A lifetime
/// counter that never decays puts an absolute ceiling on that amplification
/// per connection. 10 000 is generous for legitimate traffic (months of
/// occasional client-side cancellations) but rapidly trips on the ~30/sec
/// abusive pace reported in the CVE-2023-44487 advisory (~5 minutes).
const DEFAULT_MAX_RST_STREAM_LIFETIME: u64 = 10_000;
/// Hard lifetime cap on RST_STREAM frames received BEFORE the corresponding
/// backend response has started. These are the cheap-for-client /
/// expensive-for-us resets that characterise Rapid Reset: the client pays
/// one RST frame, we pay a round-trip to the backend plus request parsing.
/// A much lower ceiling kills the attack well before 10 000 lifetime total.
const DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME: u64 = 50;
/// Absolute lifetime cap on **server-emitted** RST_STREAM frames on a single
/// connection (CVE-2025-8671 — "MadeYouReset"). Distinct from
/// [`DEFAULT_MAX_RST_STREAM_LIFETIME`] which caps *received* RSTs
/// (CVE-2023-44487 Rapid Reset).
///
/// MadeYouReset has the server talk itself into flooding: the attacker sends
/// legitimate-looking frames that force the server to emit RST_STREAM (content
/// -length mismatch, header parse error, rejected priority, zero-increment
/// `WINDOW_UPDATE` on an open stream, …). Each forced RST costs the server a
/// header-decode, kawa buffer setup and frame serialisation; uncapped, it
/// becomes the same class of DoS as Rapid Reset but with a flipped emission
/// direction.
///
/// 500 is conservative: legitimate traffic very rarely triggers a
/// server-initiated RST (aside from graceful `NoError` cancels which are not
/// counted), so crossing 500 on a single connection is a strong abuse signal.
const DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME: u64 = 500;
/// Default maximum PING frames per window (CVE-2019-9512 Ping Flood)
const DEFAULT_MAX_PING_PER_WINDOW: u32 = 100;
/// Absolute lifetime cap on PING frames received on a single connection.
/// Mirrors DEFAULT_MAX_RST_STREAM_LIFETIME — generous for legitimate
/// keep-alives but trips on sustained low-rate abuse (CVE-2019-9512).
const DEFAULT_MAX_PING_LIFETIME: u32 = 10_000;
/// Default maximum SETTINGS frames per window (CVE-2019-9515 Settings Flood)
const DEFAULT_MAX_SETTINGS_PER_WINDOW: u32 = 50;
/// Absolute lifetime cap on SETTINGS frames received on a single connection.
/// Mirrors DEFAULT_MAX_RST_STREAM_LIFETIME — generous for legitimate
/// renegotiations but trips on sustained low-rate abuse (CVE-2019-9515).
const DEFAULT_MAX_SETTINGS_LIFETIME: u32 = 10_000;
/// Default maximum empty DATA frames per window (CVE-2019-9518 Empty Frames)
const DEFAULT_MAX_EMPTY_DATA_PER_WINDOW: u32 = 100;
/// Default maximum connection-level (stream 0) WINDOW_UPDATE frames per
/// sliding window. Non-zero stream-0 WINDOW_UPDATE frames are otherwise
/// uncounted by the generic glitch detector — a peer could burn proxy CPU by
/// sending millions of legal-looking stream-0 WINDOW_UPDATEs. Value mirrors
/// [`DEFAULT_MAX_EMPTY_DATA_PER_WINDOW`] / [`DEFAULT_MAX_PING_PER_WINDOW`] —
/// legitimate proxies only need a handful per second.
const DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW: u32 = 100;
/// Default maximum CONTINUATION frames per header block (CVE-2024-27316)
const DEFAULT_MAX_CONTINUATION_FRAMES: u32 = 20;
/// Default maximum HPACK dynamic table size (SETTINGS_HEADER_TABLE_SIZE)
/// accepted from the peer. 64 KB is well above the RFC default of 4 KB
/// while preventing a malicious peer from advertising up to 4 GB.
const DEFAULT_MAX_HEADER_TABLE_SIZE: u32 = 65536;
/// Default maximum number of materialized header fields per request/response —
/// HPACK fields plus expanded cookie crumbs (RFC 9113 §8.2.3). Bounds the HPACK
/// indexed-reference "header bomb": each 1-byte indexed reference materializes a
/// `Pair` of per-entry bookkeeping, so an attacker amplifies wire bytes into
/// allocation. RFC 9113 §6.5.2's +32-octet/field accounting alone caps this at
/// ~2048 fields for a 64 KB list; this explicit count cap is the tighter,
/// upstream-matching defense (cf. nginx `max_headers`, Apache `LimitRequestFields`).
const DEFAULT_MAX_HEADER_FIELDS: u32 = 128;
/// Duration of the sliding window for rate-based flood counters
const FLOOD_WINDOW_DURATION: std::time::Duration = std::time::Duration::from_secs(1);
/// Default maximum general anomaly count before triggering ENHANCE_YOUR_CALM
const DEFAULT_MAX_GLITCH_COUNT: u32 = 100;

/// Configurable thresholds for H2 flood detection.
///
/// All values have safe defaults matching the compile-time constants.
/// When configured via listener config, `None` values fall back to these defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H2FloodConfig {
    /// Maximum RST_STREAM frames per second window (CVE-2023-44487, CVE-2019-9514)
    max_rst_stream_per_window: u32,
    /// Maximum PING frames per second window (CVE-2019-9512)
    max_ping_per_window: u32,
    /// Maximum SETTINGS frames per second window (CVE-2019-9515)
    max_settings_per_window: u32,
    /// Maximum empty DATA frames per second window (CVE-2019-9518)
    max_empty_data_per_window: u32,
    /// Maximum connection-level (stream 0) WINDOW_UPDATE frames per sliding
    /// window. Caps the CPU cost of a peer sending a flood of non-zero
    /// stream-0 WINDOW_UPDATEs — each is individually legal so the generic
    /// glitch counter does not trip, yet millions per connection still burn
    /// server CPU parsing and updating the flow window.
    max_window_update_stream0_per_window: u32,
    /// Maximum CONTINUATION frames per header block (CVE-2024-27316)
    max_continuation_frames: u32,
    /// Maximum accumulated protocol anomalies before ENHANCE_YOUR_CALM
    max_glitch_count: u32,
    /// Absolute lifetime cap on RST_STREAM frames received on a single
    /// connection (CVE-2023-44487). Never decays — provides a ceiling the
    /// per-window counter cannot.
    max_rst_stream_lifetime: u64,
    /// Lifetime cap on "abusive" (pre-response-start) RST_STREAM frames —
    /// the Rapid Reset signature (CVE-2023-44487).
    max_rst_stream_abusive_lifetime: u64,
    /// Absolute lifetime cap on **server-emitted** RST_STREAM frames for this
    /// connection (CVE-2025-8671 "MadeYouReset"). Only non-`NoError` resets
    /// count — graceful cancels are exempt.
    max_rst_stream_emitted_lifetime: u64,
    /// Maximum accumulated HPACK-decoded header list size per request
    /// (SETTINGS_MAX_HEADER_LIST_SIZE, RFC 9113 §6.5.2).
    max_header_list_size: u32,
    /// Maximum HPACK dynamic table size (SETTINGS_HEADER_TABLE_SIZE) accepted
    /// from the peer. Caps the value the peer advertises in SETTINGS frames to
    /// prevent unbounded HPACK encoder memory growth.
    max_header_table_size: u32,
    /// Maximum number of materialized header fields, enforced per HEADERS block
    /// and (independently) per trailers block — HPACK fields plus expanded
    /// cookie crumbs (RFC 9113 §8.2.3). Bounds the HPACK indexed-reference
    /// header bomb, where many 1-byte indexed references each materialize a
    /// `Pair` of per-entry bookkeeping.
    max_header_fields: u32,
}

impl Default for H2FloodConfig {
    fn default() -> Self {
        Self {
            max_rst_stream_per_window: DEFAULT_MAX_RST_STREAM_PER_WINDOW,
            max_ping_per_window: DEFAULT_MAX_PING_PER_WINDOW,
            max_settings_per_window: DEFAULT_MAX_SETTINGS_PER_WINDOW,
            max_empty_data_per_window: DEFAULT_MAX_EMPTY_DATA_PER_WINDOW,
            max_window_update_stream0_per_window: DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW,
            max_continuation_frames: DEFAULT_MAX_CONTINUATION_FRAMES,
            max_glitch_count: DEFAULT_MAX_GLITCH_COUNT,
            max_rst_stream_lifetime: DEFAULT_MAX_RST_STREAM_LIFETIME,
            max_rst_stream_abusive_lifetime: DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME,
            max_rst_stream_emitted_lifetime: DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME,
            max_header_list_size: MAX_HEADER_LIST_SIZE as u32,
            max_header_table_size: DEFAULT_MAX_HEADER_TABLE_SIZE,
            max_header_fields: DEFAULT_MAX_HEADER_FIELDS,
        }
    }
}

impl H2FloodConfig {
    /// Create a validated config, clamping all thresholds to at least 1.
    /// Zero thresholds would cause immediate flood detection on any frame.
    ///
    /// With [`Self::from_optional`], this is the **only** way to build an
    /// `H2FloodConfig`: the fields above are private, so the clamp below cannot
    /// be routed around by a raw struct literal the way `lib/src/http.rs` and
    /// `lib/src/https.rs` used to route around it (sozu-proxy/sozu#1418).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_rst_stream_per_window: u32,
        max_ping_per_window: u32,
        max_settings_per_window: u32,
        max_empty_data_per_window: u32,
        max_window_update_stream0_per_window: u32,
        max_continuation_frames: u32,
        max_glitch_count: u32,
        max_rst_stream_lifetime: u64,
        max_rst_stream_abusive_lifetime: u64,
        max_rst_stream_emitted_lifetime: u64,
        max_header_list_size: u32,
        max_header_table_size: u32,
        max_header_fields: u32,
    ) -> Self {
        let config = Self {
            max_rst_stream_per_window: max_rst_stream_per_window.max(1),
            max_ping_per_window: max_ping_per_window.max(1),
            max_settings_per_window: max_settings_per_window.max(1),
            max_empty_data_per_window: max_empty_data_per_window.max(1),
            max_window_update_stream0_per_window: max_window_update_stream0_per_window.max(1),
            max_continuation_frames: max_continuation_frames.max(1),
            max_glitch_count: max_glitch_count.max(1),
            max_rst_stream_lifetime: max_rst_stream_lifetime.max(1),
            max_rst_stream_abusive_lifetime: max_rst_stream_abusive_lifetime.max(1),
            max_rst_stream_emitted_lifetime: max_rst_stream_emitted_lifetime.max(1),
            max_header_list_size: max_header_list_size.max(1),
            max_header_table_size: max_header_table_size.max(1),
            max_header_fields: max_header_fields.max(1),
        };
        // Post-condition: every threshold is clamped to at least 1. A zero
        // threshold would make `check_flood`/`record_rst_*` trip on the very
        // first frame (count > 0 > threshold), turning a legitimate connection
        // into an immediate GOAWAY. This is the central invariant the clamps
        // above exist to enforce — assert it rather than trusting the `.max(1)`
        // chain stays correct under future edits.
        debug_assert!(
            config.max_rst_stream_per_window >= 1
                && config.max_ping_per_window >= 1
                && config.max_settings_per_window >= 1
                && config.max_empty_data_per_window >= 1
                && config.max_window_update_stream0_per_window >= 1
                && config.max_continuation_frames >= 1
                && config.max_glitch_count >= 1,
            "every u32 flood threshold must be clamped to >= 1"
        );
        debug_assert!(
            config.max_rst_stream_lifetime >= 1
                && config.max_rst_stream_abusive_lifetime >= 1
                && config.max_rst_stream_emitted_lifetime >= 1
                && config.max_header_list_size >= 1
                && config.max_header_table_size >= 1
                && config.max_header_fields >= 1,
            "every lifetime/size flood threshold must be clamped to >= 1"
        );
        config
    }

    /// Build a validated config from a listener's per-knob overrides: each
    /// `None` takes the compile-time default, then [`Self::new`] clamps.
    /// Mirrors [`super::h2::H2ConnectionConfig::from_optional`], which the
    /// adjacent `get_h2_connection_config` already uses for the connection
    /// knobs read out of the very same listener config.
    ///
    /// This is what `lib/src/http.rs` and `lib/src/https.rs` call. They used to
    /// spell the same `unwrap_or(default)` table into a raw struct literal,
    /// which skipped `new` and therefore skipped the clamp on the one
    /// production path that carries operator-set thresholds
    /// (sozu-proxy/sozu#1418). A zero that survives to here does not disable a
    /// check, it makes the first event its counter sees a violation, and how
    /// far that reaches depends on the knob: `max_header_list_size` and
    /// `max_header_fields` are also the HPACK decode budget
    /// `pkawa::decode_headers_with_budget` enforces, so `0` refuses every
    /// request on the listener with `ENHANCE_YOUR_CALM` — per stream for a
    /// header block that fits one HEADERS frame, and as a connection
    /// `GOAWAY` for one that spans CONTINUATION frames, where
    /// `H2FloodDetector::record_continuation_frame` lifts `accumulated_header_size`
    /// above the zero threshold and `h2.rs`'s `check_flood_or_return!` fires
    /// before the explicit size test. `max_rst_stream_per_window = 0` (and
    /// every other per-window or lifetime frame counter) is inert until a
    /// client actually sends the frame it counts — `check_flood` compares
    /// `count > threshold`, and `0 > 0` is false.
    ///
    /// Every door that STATES a listener configuration now rejects an
    /// out-of-range knob outright — the configuration file and
    /// `sozu ctl add listener` via `ConfigError::H2ThresholdBelowMinimum`
    /// (`command/src/config.rs`), `UpdateHttp(s)Listener` and a raw protobuf
    /// `Add{Http,Https}Listener` via `command/src/state.rs`'s four
    /// `validate_h2_flood_knobs_*` — so the operator is told which key to fix
    /// rather than having their stated value silently rewritten. The clamp
    /// reached through here is the backstop for the one path that deliberately
    /// does NOT reject: replay of a state file saved before those gates
    /// existed, where dropping the listener would be a worse outcome than
    /// clamping one threshold (`bin/src/command/requests.rs`'s
    /// `RequestOrigin::Replayed`).
    #[allow(clippy::too_many_arguments)]
    pub fn from_optional(
        max_rst_stream_per_window: Option<u32>,
        max_ping_per_window: Option<u32>,
        max_settings_per_window: Option<u32>,
        max_empty_data_per_window: Option<u32>,
        max_window_update_stream0_per_window: Option<u32>,
        max_continuation_frames: Option<u32>,
        max_glitch_count: Option<u32>,
        max_rst_stream_lifetime: Option<u64>,
        max_rst_stream_abusive_lifetime: Option<u64>,
        max_rst_stream_emitted_lifetime: Option<u64>,
        max_header_list_size: Option<u32>,
        max_header_table_size: Option<u32>,
        max_header_fields: Option<u32>,
    ) -> Self {
        let defaults = Self::default();
        Self::new(
            max_rst_stream_per_window.unwrap_or(defaults.max_rst_stream_per_window),
            max_ping_per_window.unwrap_or(defaults.max_ping_per_window),
            max_settings_per_window.unwrap_or(defaults.max_settings_per_window),
            max_empty_data_per_window.unwrap_or(defaults.max_empty_data_per_window),
            max_window_update_stream0_per_window
                .unwrap_or(defaults.max_window_update_stream0_per_window),
            max_continuation_frames.unwrap_or(defaults.max_continuation_frames),
            max_glitch_count.unwrap_or(defaults.max_glitch_count),
            max_rst_stream_lifetime.unwrap_or(defaults.max_rst_stream_lifetime),
            max_rst_stream_abusive_lifetime.unwrap_or(defaults.max_rst_stream_abusive_lifetime),
            max_rst_stream_emitted_lifetime.unwrap_or(defaults.max_rst_stream_emitted_lifetime),
            max_header_list_size.unwrap_or(defaults.max_header_list_size),
            max_header_table_size.unwrap_or(defaults.max_header_table_size),
            max_header_fields.unwrap_or(defaults.max_header_fields),
        )
    }

    /// Maximum RST_STREAM frames per rate window (CVE-2023-44487, CVE-2019-9514).
    pub fn max_rst_stream_per_window(&self) -> u32 {
        self.max_rst_stream_per_window
    }

    /// Maximum PING frames per rate window (CVE-2019-9512).
    pub fn max_ping_per_window(&self) -> u32 {
        self.max_ping_per_window
    }

    /// Maximum SETTINGS frames per rate window (CVE-2019-9515).
    pub fn max_settings_per_window(&self) -> u32 {
        self.max_settings_per_window
    }

    /// Maximum empty DATA frames per rate window (CVE-2019-9518).
    pub fn max_empty_data_per_window(&self) -> u32 {
        self.max_empty_data_per_window
    }

    /// Maximum connection-level (stream 0) WINDOW_UPDATE frames per rate window.
    pub fn max_window_update_stream0_per_window(&self) -> u32 {
        self.max_window_update_stream0_per_window
    }

    /// Maximum CONTINUATION frames per header block (CVE-2024-27316).
    pub fn max_continuation_frames(&self) -> u32 {
        self.max_continuation_frames
    }

    /// Maximum accumulated protocol anomalies before ENHANCE_YOUR_CALM.
    pub fn max_glitch_count(&self) -> u32 {
        self.max_glitch_count
    }

    /// Absolute lifetime cap on RST_STREAM frames received (CVE-2023-44487).
    pub fn max_rst_stream_lifetime(&self) -> u64 {
        self.max_rst_stream_lifetime
    }

    /// Lifetime cap on pre-response-start RST_STREAM frames (CVE-2023-44487).
    pub fn max_rst_stream_abusive_lifetime(&self) -> u64 {
        self.max_rst_stream_abusive_lifetime
    }

    /// Lifetime cap on server-emitted RST_STREAM frames (CVE-2025-8671).
    pub fn max_rst_stream_emitted_lifetime(&self) -> u64 {
        self.max_rst_stream_emitted_lifetime
    }

    /// Maximum accumulated HPACK-decoded header list size (RFC 9113 §6.5.2).
    pub fn max_header_list_size(&self) -> u32 {
        self.max_header_list_size
    }

    /// Maximum HPACK dynamic table size accepted from the peer.
    pub fn max_header_table_size(&self) -> u32 {
        self.max_header_table_size
    }

    /// Maximum number of materialized header fields per HEADERS/trailers block.
    pub fn max_header_fields(&self) -> u32 {
        self.max_header_fields
    }
}

/// Detail of a flood-threshold violation returned by
/// [`H2FloodDetector::check_flood`] and [`H2FloodDetector::record_rst_lifetime`].
///
/// Carrying `(reason, count, threshold)` lets the caller emit a session-scoped
/// log line with full context — the detector itself is connection-agnostic and
/// never logs.
#[derive(Debug, Clone, PartialEq)]
pub struct H2FloodViolation {
    /// HTTP/2 error code to emit on the GOAWAY.
    pub error: H2Error,
    /// Human-readable name of the counter that tripped (e.g. `"RST_STREAM"`).
    pub reason: &'static str,
    /// Statsd metric key emitted by [`super::h2::ConnectionH2::handle_flood_violation`].
    /// Carried alongside `reason` so a single field maps to both the log line
    /// and the dashboard counter — adding a new violation kind requires
    /// choosing both at the construction site, preventing drift.
    pub metric_key: &'static str,
    /// Observed counter value at the moment of detection.
    pub count: u64,
    /// Configured ceiling that was crossed.
    pub threshold: u64,
}

/// Tracks per-connection frame rates to detect and mitigate H2 flood attacks.
///
/// Monitors RST_STREAM (CVE-2023-44487), PING (CVE-2019-9512), SETTINGS (CVE-2019-9515),
/// empty DATA (CVE-2019-9518), and CONTINUATION (CVE-2024-27316) flood patterns.
/// When any counter exceeds its threshold, `check_flood()` returns the violation
/// detail so callers can log with connection context before sending GOAWAY.
///
/// Thresholds are configurable via [`H2FloodConfig`], with safe defaults matching
/// the original compile-time constants.
#[derive(Debug)]
pub(super) struct H2FloodDetector {
    /// RST_STREAM frames received in current window (CVE-2023-44487 + CVE-2019-9514)
    rst_stream_count: u32,
    /// Lifetime RST_STREAM frames received on this connection.
    ///
    /// Never decays — provides an absolute ceiling that the half-decaying
    /// per-window counter cannot, preventing a sustained ~50 RST/sec burst
    /// from running forever.
    total_rst_received_lifetime: u64,
    /// Lifetime RST_STREAM frames received that targeted a stream whose
    /// backend response had not yet started. These are the "Rapid Reset"
    /// signature — cheap for the attacker, expensive for the proxy — and
    /// trip on a much lower ceiling than the generic lifetime counter.
    total_abusive_rst_received_lifetime: u64,
    /// Lifetime RST_STREAM frames **emitted by the server** on this
    /// connection (CVE-2025-8671 "MadeYouReset" mitigation). Incremented
    /// inside `ConnectionH2::reset_stream` whenever a non-`NoError` reset
    /// is triggered by an attacker-crafted frame (content-length mismatch,
    /// header parse error, priority rejection, zero-increment WINDOW_UPDATE
    /// on an open stream). Never decays — provides an absolute ceiling that
    /// short-circuits patient-attacker patterns that stay under any windowed
    /// counter.
    total_rst_streams_emitted_lifetime: u64,
    /// PING frames received in current window (CVE-2019-9512)
    ping_count: u32,
    /// Lifetime PING frames received on this connection.
    ///
    /// Never decays — provides an absolute ceiling that the half-decaying
    /// per-window counter cannot, preventing sustained low-rate PING abuse.
    total_ping_received_lifetime: u32,
    /// SETTINGS frames received in current window (CVE-2019-9515)
    settings_count: u32,
    /// Lifetime SETTINGS frames received on this connection.
    ///
    /// Never decays — provides an absolute ceiling that the half-decaying
    /// per-window counter cannot, preventing sustained low-rate SETTINGS abuse.
    total_settings_received_lifetime: u32,
    /// Empty DATA frames received in current window (CVE-2019-9518)
    empty_data_count: u32,
    /// Connection-level (stream 0) WINDOW_UPDATE frames received in current
    /// sliding window. Half-decays with [`Self::maybe_reset_window`] like other
    /// rate counters. Increments on non-zero stream-0 WINDOW_UPDATEs only —
    /// zero-increment frames short-circuit into GOAWAY(PROTOCOL_ERROR) per
    /// RFC 9113 §6.9 before reaching this counter.
    window_update_stream0_count: u32,
    /// CONTINUATION frames received for current header block (CVE-2024-27316)
    continuation_count: u32,
    /// Total accumulated header block size across CONTINUATION frames
    accumulated_header_size: u32,
    /// General anomaly counter
    glitch_count: u32,
    /// Window start for rate-based counters.
    ///
    /// Private: the detector never samples the clock itself, so this field is
    /// only ever advanced from a `now` the caller supplies to
    /// [`Self::check_flood`]. Exposing it would let a caller reset the window
    /// against a clock the connection is not reading, which is exactly the
    /// dual-clock hazard the injected `now` removes.
    window_start: Instant,
    /// Configurable thresholds for flood detection
    config: H2FloodConfig,
}

impl H2FloodDetector {
    /// `now` is the caller's clock snapshot — the detector never samples the
    /// clock itself. It seeds the first rate window, which
    /// [`Self::check_flood`] then advances from the `now` it is handed.
    pub(super) fn new(config: H2FloodConfig, now: Instant) -> Self {
        // Pre-condition: thresholds are already validated (clamped to >= 1 by
        // `H2FloodConfig::new`, the only constructor — `H2FloodConfig`'s fields
        // are private, so no caller can assemble one with a struct literal).
        // A zero threshold would make the first event its counter sees a
        // violation. The assertion outlived the hole it was written for
        // (sozu-proxy/sozu#1418: `get_h2_flood_config` WAS that raw struct
        // literal) and is kept as the debug-build tripwire for any future
        // construction path inside this module.
        //
        // It covers all thirteen fields, not the five it used to: `impl
        // Default` above is still an in-module struct literal, so a future
        // `DEFAULT_MAX_HEADER_LIST_SIZE = 0` would otherwise pass unnoticed —
        // and that is the worst of the thirteen, since `max_header_list_size`
        // is also the HPACK decode budget and `0` refuses every request.
        debug_assert!(
            config.max_rst_stream_per_window >= 1
                && config.max_ping_per_window >= 1
                && config.max_settings_per_window >= 1
                && config.max_empty_data_per_window >= 1
                && config.max_window_update_stream0_per_window >= 1
                && config.max_continuation_frames >= 1
                && config.max_glitch_count >= 1,
            "flood detector must be constructed with validated (>= 1) per-window thresholds"
        );
        debug_assert!(
            config.max_rst_stream_lifetime >= 1
                && config.max_rst_stream_abusive_lifetime >= 1
                && config.max_rst_stream_emitted_lifetime >= 1
                && config.max_header_list_size >= 1
                && config.max_header_table_size >= 1
                && config.max_header_fields >= 1,
            "flood detector must be constructed with validated (>= 1) lifetime/size thresholds"
        );
        let detector = Self {
            rst_stream_count: 0,
            total_rst_received_lifetime: 0,
            total_abusive_rst_received_lifetime: 0,
            total_rst_streams_emitted_lifetime: 0,
            ping_count: 0,
            total_ping_received_lifetime: 0,
            settings_count: 0,
            total_settings_received_lifetime: 0,
            empty_data_count: 0,
            window_update_stream0_count: 0,
            continuation_count: 0,
            accumulated_header_size: 0,
            glitch_count: 0,
            window_start: now,
            config,
        };
        detector.debug_assert_invariants();
        detector
    }

    /// Current configured thresholds — a cheap `Copy` out, not a reference,
    /// so callers cannot smuggle a `&mut` in through it. Backs the SETTINGS
    /// `SETTINGS_HEADER_TABLE_SIZE` cap, the `pkawa::handle_header` budget
    /// arguments, and the CONTINUATION accumulated-size log/GOAWAY check.
    pub(super) fn config(&self) -> H2FloodConfig {
        self.config
    }

    /// Total accumulated header block size across CONTINUATION frames for the
    /// block currently in progress (CVE-2024-27316).
    pub(super) fn accumulated_header_size(&self) -> u32 {
        self.accumulated_header_size
    }

    /// Lifetime RST_STREAM frames received on this connection — read by the
    /// `MUX-H2` session log line (`log_context!`/`log_context_stream!`).
    pub(super) fn total_rst_received_lifetime(&self) -> u64 {
        self.total_rst_received_lifetime
    }

    /// Lifetime RST_STREAM frames emitted by the server on this connection
    /// (CVE-2025-8671) — read by the `MUX-H2` session log line.
    pub(super) fn total_rst_streams_emitted_lifetime(&self) -> u64 {
        self.total_rst_streams_emitted_lifetime
    }

    /// Increment the lifetime RST_STREAM counters and return a
    /// [`H2FloodViolation`] if either the global or the abusive
    /// (pre-response-start) lifetime cap has been exceeded.
    ///
    /// `response_started` indicates whether the backend response had already
    /// begun when the RST arrived; `false` is the cheap-for-client /
    /// expensive-for-us Rapid Reset signature (CVE-2023-44487).
    pub(super) fn record_rst_lifetime(
        &mut self,
        response_started: bool,
    ) -> Option<H2FloodViolation> {
        let total_before = self.total_rst_received_lifetime;
        let abusive_before = self.total_abusive_rst_received_lifetime;
        self.total_rst_received_lifetime = self.total_rst_received_lifetime.saturating_add(1);
        if !response_started {
            self.total_abusive_rst_received_lifetime =
                self.total_abusive_rst_received_lifetime.saturating_add(1);
        }
        // Monotonicity: the global lifetime counter advances by one per call
        // (until saturation), and the abusive sub-counter advances iff the RST
        // arrived before the backend response started. The abusive counter can
        // never exceed the global one — every abusive RST is also a received RST.
        debug_assert!(
            self.total_rst_received_lifetime >= total_before,
            "lifetime RST counter must be monotonic non-decreasing"
        );
        debug_assert_eq!(
            self.total_abusive_rst_received_lifetime > abusive_before,
            !response_started,
            "abusive RST counter advances iff the RST is pre-response-start"
        );
        debug_assert!(
            self.total_abusive_rst_received_lifetime <= self.total_rst_received_lifetime,
            "abusive RST count is a subset of total received RST count"
        );
        self.debug_assert_invariants();
        if self.total_rst_received_lifetime > self.config.max_rst_stream_lifetime {
            return Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "Rapid Reset: lifetime RST_STREAM",
                metric_key: "h2.flood.violation.rst_stream_lifetime",
                count: self.total_rst_received_lifetime,
                threshold: self.config.max_rst_stream_lifetime,
            });
        }
        if self.total_abusive_rst_received_lifetime > self.config.max_rst_stream_abusive_lifetime {
            return Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "Rapid Reset: lifetime pre-response RST_STREAM",
                metric_key: "h2.flood.violation.rst_stream_pre_response_lifetime",
                count: self.total_abusive_rst_received_lifetime,
                threshold: self.config.max_rst_stream_abusive_lifetime,
            });
        }
        None
    }

    /// Increment the lifetime **server-emitted** RST_STREAM counter and
    /// return a [`H2FloodViolation`] once the configured ceiling is exceeded.
    ///
    /// Call sites are the error paths inside `ConnectionH2::reset_stream`
    /// where an attacker-crafted frame coerces the server into emitting a
    /// RST_STREAM (CVE-2025-8671 "MadeYouReset"). Only non-`NoError` resets
    /// are reported — callers must exclude graceful cancels.
    pub(super) fn record_rst_emitted(&mut self) -> Option<H2FloodViolation> {
        let before = self.total_rst_streams_emitted_lifetime;
        self.total_rst_streams_emitted_lifetime =
            self.total_rst_streams_emitted_lifetime.saturating_add(1);
        // Monotonic: the emitted-RST counter never decays (it is the absolute
        // MadeYouReset ceiling, CVE-2025-8671), so each call strictly advances
        // it until u64 saturation.
        debug_assert!(
            self.total_rst_streams_emitted_lifetime > before || before == u64::MAX,
            "emitted-RST lifetime counter must advance (or already be saturated)"
        );
        self.debug_assert_invariants();
        if self.total_rst_streams_emitted_lifetime > self.config.max_rst_stream_emitted_lifetime {
            return Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "MadeYouReset: lifetime server-emitted RST_STREAM",
                metric_key: "h2.flood.violation.rst_stream_emitted_lifetime",
                count: self.total_rst_streams_emitted_lifetime,
                threshold: self.config.max_rst_stream_emitted_lifetime,
            });
        }
        None
    }

    /// CVE-2023-44487 Rapid Reset + CVE-2019-9514: record one inbound
    /// RST_STREAM toward the per-window rate counter. The never-decaying
    /// lifetime ceilings are tracked separately by [`Self::record_rst_lifetime`].
    pub(super) fn record_rst_stream_window(&mut self) {
        let before = self.rst_stream_count;
        self.rst_stream_count += 1;
        debug_assert_eq!(
            self.rst_stream_count,
            before + 1,
            "per-window RST_STREAM counter must advance by exactly one per inbound RST"
        );
        self.debug_assert_invariants();
    }

    /// CVE-2019-9512: record one non-ACK PING frame toward both the
    /// per-window rate counter and the never-decaying lifetime counter.
    pub(super) fn record_ping_frame(&mut self) {
        let count_before = self.ping_count;
        let lifetime_before = self.total_ping_received_lifetime;
        self.ping_count += 1;
        self.total_ping_received_lifetime = self.total_ping_received_lifetime.saturating_add(1);
        debug_assert_eq!(
            self.ping_count,
            count_before + 1,
            "per-window PING counter must advance by one per non-ACK PING"
        );
        debug_assert!(
            self.total_ping_received_lifetime > lifetime_before || lifetime_before == u32::MAX,
            "lifetime PING counter must advance (or already be saturated)"
        );
        self.debug_assert_invariants();
    }

    /// CVE-2019-9515: record one non-ACK SETTINGS frame toward both the
    /// per-window rate counter and the never-decaying lifetime counter.
    pub(super) fn record_settings_frame(&mut self) {
        let count_before = self.settings_count;
        let lifetime_before = self.total_settings_received_lifetime;
        self.settings_count += 1;
        self.total_settings_received_lifetime =
            self.total_settings_received_lifetime.saturating_add(1);
        debug_assert_eq!(
            self.settings_count,
            count_before + 1,
            "per-window SETTINGS counter must advance by one per non-ACK SETTINGS"
        );
        debug_assert!(
            self.total_settings_received_lifetime > lifetime_before || lifetime_before == u32::MAX,
            "lifetime SETTINGS counter must advance (or already be saturated)"
        );
        self.debug_assert_invariants();
    }

    /// CVE-2019-9518: record one empty (no payload, no END_STREAM) DATA frame
    /// toward the per-window rate counter.
    pub(super) fn record_empty_data_frame(&mut self) {
        let before = self.empty_data_count;
        self.empty_data_count += 1;
        debug_assert_eq!(
            self.empty_data_count,
            before + 1,
            "empty-DATA flood counter must advance by exactly one per empty frame"
        );
        self.debug_assert_invariants();
    }

    /// Record one non-zero connection-level (stream 0) WINDOW_UPDATE toward
    /// the per-window rate counter, before the arithmetic cost of applying it
    /// to the connection window is paid.
    pub(super) fn record_window_update_stream0(&mut self) {
        let before = self.window_update_stream0_count;
        self.window_update_stream0_count = self.window_update_stream0_count.saturating_add(1);
        debug_assert!(
            self.window_update_stream0_count > before || before == u32::MAX,
            "stream-0 WINDOW_UPDATE flood counter must advance before the flood check"
        );
        self.debug_assert_invariants();
    }

    /// Record one general protocol anomaly that does not fit a specific flood
    /// pattern (a frame on a closed stream, an unknown SETTINGS identifier, a
    /// zero-increment WINDOW_UPDATE on an already-closed stream, ...) toward
    /// the cumulative, half-decaying glitch counter.
    pub(super) fn record_glitch(&mut self) {
        let before = self.glitch_count;
        self.glitch_count += 1;
        debug_assert!(
            self.glitch_count > before || before == u32::MAX,
            "glitch counter must advance (or already be saturated)"
        );
        self.debug_assert_invariants();
    }

    /// CVE-2024-27316: record one CONTINUATION frame toward the per-block
    /// count and accumulated header-block size. `payload_len` is the frame's
    /// own payload length (RFC 9113 §6.10).
    pub(super) fn record_continuation_frame(&mut self, payload_len: u32) {
        let cont_count_before = self.continuation_count;
        let acc_size_before = self.accumulated_header_size;
        self.continuation_count += 1;
        self.accumulated_header_size = self.accumulated_header_size.saturating_add(payload_len);
        // Per-block CONTINUATION accounting must grow monotonically within a
        // header block: each frame bumps the count by one and the
        // accumulated size by the frame's payload (never shrinks mid-block).
        // `reset_continuation` is the only thing allowed to zero these — and
        // only once the block is complete.
        debug_assert_eq!(
            self.continuation_count,
            cont_count_before + 1,
            "CONTINUATION per-block counter must advance by one per frame"
        );
        debug_assert!(
            self.accumulated_header_size >= acc_size_before,
            "accumulated header size must not shrink within a header block"
        );
        self.debug_assert_invariants();
    }

    /// Seed [`Self::accumulated_header_size`] from the first HEADERS frame's
    /// own field-block fragment length, but ONLY the very first time a block
    /// starts (`continuation_count == 0`). A re-entry from
    /// `H2State::ContinuationFrame` (the accumulated block replayed through
    /// `handle_frame(Frame::Headers)` once assembly completes) must not stomp
    /// a running total a CONTINUATION frame may already have advanced.
    pub(super) fn begin_header_block_if_new(&mut self, initial_fragment_len: u32) {
        if self.continuation_count == 0 {
            self.accumulated_header_size = initial_fragment_len;
        }
        self.debug_assert_invariants();
    }

    /// Half-decay rate-based counters if the current window has expired.
    /// Uses half-window decay instead of full reset to catch burst-then-wait attacks.
    ///
    /// `now` is the caller's snapshot rather than a fresh `Instant::now()`, so
    /// a window cannot decay part-way through a pass: a burst that arrives in
    /// one pass is weighed in full against the window that was open when the
    /// pass started. That is the fail-closed direction.
    fn maybe_reset_window(&mut self, now: Instant) {
        if now.saturating_duration_since(self.window_start) >= FLOOD_WINDOW_DURATION {
            let (rst_before, ping_before, settings_before) =
                (self.rst_stream_count, self.ping_count, self.settings_count);
            let (empty_before, wu0_before, glitch_before) = (
                self.empty_data_count,
                self.window_update_stream0_count,
                self.glitch_count,
            );
            self.rst_stream_count /= 2;
            self.ping_count /= 2;
            self.settings_count /= 2;
            self.empty_data_count /= 2;
            self.window_update_stream0_count /= 2;
            self.glitch_count /= 2;
            self.window_start = now;
            // Half-decay invariant: each rate-based counter is exactly halved
            // (integer division), never increased. Catching burst-then-wait
            // attacks relies on the counter shrinking but not vanishing — a
            // full reset would let a patient attacker reset to zero each window.
            debug_assert_eq!(self.rst_stream_count, rst_before / 2, "RST count halves");
            debug_assert_eq!(self.ping_count, ping_before / 2, "PING count halves");
            debug_assert_eq!(
                self.settings_count,
                settings_before / 2,
                "SETTINGS count halves"
            );
            debug_assert_eq!(
                self.empty_data_count,
                empty_before / 2,
                "empty-DATA count halves"
            );
            debug_assert_eq!(
                self.window_update_stream0_count,
                wu0_before / 2,
                "stream-0 WINDOW_UPDATE count halves"
            );
            debug_assert_eq!(self.glitch_count, glitch_before / 2, "glitch count halves");
            // The lifetime counters are deliberately NOT touched here — they are
            // the never-decaying ceilings. Guard against a future edit decaying
            // them by accident.
            debug_assert!(
                now.saturating_duration_since(self.window_start) < FLOOD_WINDOW_DURATION,
                "window_start must be refreshed to the caller's now after decay"
            );
        }
        self.debug_assert_invariants();
    }

    /// Check all flood counters. Returns a [`H2FloodViolation`] when a threshold
    /// is exceeded; the caller is responsible for logging with session context
    /// and escalating to GOAWAY.
    ///
    /// `now` is the caller's clock snapshot — in production
    /// `ConnectionH2::now`, refreshed by `Mux` once per pass. The detector
    /// never reads the clock itself, so the ten `check_flood_or_return!` sites
    /// in `h2.rs` all weigh a burst against one instant.
    pub(super) fn check_flood(&mut self, now: Instant) -> Option<H2FloodViolation> {
        self.maybe_reset_window(now);

        fn flag(
            reason: &'static str,
            metric_key: &'static str,
            count: u32,
            threshold: u32,
        ) -> Option<H2FloodViolation> {
            if count > threshold {
                Some(H2FloodViolation {
                    error: H2Error::EnhanceYourCalm,
                    reason,
                    metric_key,
                    count: count as u64,
                    threshold: threshold as u64,
                })
            } else {
                None
            }
        }

        let violation = flag(
            "RST_STREAM",
            "h2.flood.violation.rst_stream_window",
            self.rst_stream_count,
            self.config.max_rst_stream_per_window,
        )
        .or_else(|| {
            flag(
                "PING",
                "h2.flood.violation.ping_window",
                self.ping_count,
                self.config.max_ping_per_window,
            )
        })
        .or_else(|| {
            flag(
                "PING lifetime",
                "h2.flood.violation.ping_lifetime",
                self.total_ping_received_lifetime,
                DEFAULT_MAX_PING_LIFETIME,
            )
        })
        .or_else(|| {
            flag(
                "SETTINGS",
                "h2.flood.violation.settings_window",
                self.settings_count,
                self.config.max_settings_per_window,
            )
        })
        .or_else(|| {
            flag(
                "SETTINGS lifetime",
                "h2.flood.violation.settings_lifetime",
                self.total_settings_received_lifetime,
                DEFAULT_MAX_SETTINGS_LIFETIME,
            )
        })
        .or_else(|| {
            flag(
                "empty DATA",
                "h2.flood.violation.empty_data_window",
                self.empty_data_count,
                self.config.max_empty_data_per_window,
            )
        })
        .or_else(|| {
            flag(
                "CONTINUATION",
                "h2.flood.violation.continuation_per_block",
                self.continuation_count,
                self.config.max_continuation_frames,
            )
        })
        .or_else(|| {
            flag(
                "WINDOW_UPDATE stream 0",
                "h2.flood.violation.window_update_stream0_window",
                self.window_update_stream0_count,
                self.config.max_window_update_stream0_per_window,
            )
        })
        .or_else(|| {
            flag(
                "accumulated header size",
                "h2.flood.violation.header_size_per_block",
                self.accumulated_header_size,
                self.config.max_header_list_size,
            )
        })
        .or_else(|| {
            flag(
                "glitch",
                "h2.flood.violation.glitch_window",
                self.glitch_count,
                self.config.max_glitch_count,
            )
        });
        // Post-condition: any reported violation is well-formed — every H2
        // flood escalation is an ENHANCE_YOUR_CALM connection error, and the
        // observed count strictly exceeds the threshold it tripped (the `flag`
        // helper and the lifetime checks all use strict `>`). A violation whose
        // count <= threshold would be a false positive terminating a healthy
        // connection.
        debug_assert!(
            violation
                .as_ref()
                .is_none_or(|v| v.error == H2Error::EnhanceYourCalm && v.count > v.threshold),
            "a flood violation must be EnhanceYourCalm with count strictly above threshold"
        );
        violation
    }

    /// Reset CONTINUATION-specific counters when a header block is complete.
    pub(super) fn reset_continuation(&mut self) {
        self.continuation_count = 0;
        self.accumulated_header_size = 0;
        // Post-condition: both CONTINUATION-block accumulators are cleared so
        // the next header block starts from zero (CVE-2024-27316 per-block
        // accounting must not leak across blocks).
        debug_assert_eq!(
            self.continuation_count, 0,
            "continuation_count must be zero after a block completes"
        );
        debug_assert_eq!(
            self.accumulated_header_size, 0,
            "accumulated_header_size must be zero after a block completes"
        );
        self.debug_assert_invariants();
    }

    // ---- invariants ----------------------------------------------------------

    /// TigerStyle invariant sweep. A single full check of every structural
    /// invariant this type must preserve, asserted at the END of every public
    /// mutating method via [`debug_assert_invariants`](Self::debug_assert_invariants)
    /// — exactly the pattern `h2_flow_control::H2FlowControl`,
    /// `h2_stream_table::H2StreamTable` and `protocol::udp::manager::UdpManager`
    /// use. Compiled out entirely in release (`#[cfg(debug_assertions)]`); on
    /// in every test/e2e/fuzz/dev build. Must never change runtime behavior —
    /// it only reads.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // The abusive (pre-response-start) lifetime RST counter is a subset
        // of the total lifetime RST counter — `record_rst_lifetime` only ever
        // advances both together or the total alone, never the abusive
        // counter alone.
        debug_assert!(
            self.total_abusive_rst_received_lifetime <= self.total_rst_received_lifetime,
            "abusive RST count is a subset of total received RST count"
        );
    }

    /// Run the full [`check_invariants`](Self::check_invariants) sweep, but
    /// only in debug builds — a thin wrapper so call sites read as one line
    /// and the whole sweep is dead-stripped in release.
    #[inline]
    fn debug_assert_invariants(&self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── H2FloodDetector ──────────────────────────────────────────────────

    #[test]
    fn test_flood_detector_no_flood_below_threshold() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        // All counters at zero -> no flood
        assert!(detector.check_flood(base).is_none());

        // Increment each counter to exactly the threshold (not exceeding)
        detector.rst_stream_count = config.max_rst_stream_per_window;
        detector.ping_count = config.max_ping_per_window;
        detector.settings_count = config.max_settings_per_window;
        detector.empty_data_count = config.max_empty_data_per_window;
        detector.continuation_count = config.max_continuation_frames;
        detector.glitch_count = config.max_glitch_count;
        // At threshold but not exceeding -> no flood
        assert!(detector.check_flood(base).is_none());
    }

    #[test]
    fn test_flood_detector_detects_rapid_reset() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.rst_stream_count = config.max_rst_stream_per_window + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_ping_flood() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.ping_count = config.max_ping_per_window + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_settings_flood() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.settings_count = config.max_settings_per_window + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_empty_data_flood() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.empty_data_count = config.max_empty_data_per_window + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_continuation_flood() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.continuation_count = config.max_continuation_frames + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_header_size_flood() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.accumulated_header_size = MAX_HEADER_LIST_SIZE as u32 + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_detects_glitch_flood() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.glitch_count = config.max_glitch_count + 1;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_custom_thresholds() {
        let base = Instant::now();
        let config = H2FloodConfig {
            max_rst_stream_per_window: 5,
            max_ping_per_window: 10,
            max_settings_per_window: 3,
            max_empty_data_per_window: 8,
            max_continuation_frames: 2,
            max_glitch_count: 15,
            ..H2FloodConfig::default()
        };
        let mut detector = H2FloodDetector::new(config, base);

        // Below custom threshold -> no flood
        detector.rst_stream_count = 5;
        assert!(detector.check_flood(base).is_none());

        // Above custom threshold -> flood
        detector.rst_stream_count = 6;
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_reset_continuation() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.continuation_count = 15;
        detector.accumulated_header_size = 30000;

        detector.reset_continuation();

        assert_eq!(detector.continuation_count, 0);
        assert_eq!(detector.accumulated_header_size, 0);
    }

    #[test]
    fn test_flood_detector_half_decay_on_window_expiry() {
        let base = Instant::now();
        let config = H2FloodConfig::default();
        let mut detector = H2FloodDetector::new(config, base);

        detector.rst_stream_count = 80;
        detector.ping_count = 60;
        detector.settings_count = 40;
        detector.empty_data_count = 20;
        detector.window_update_stream0_count = 90;
        detector.glitch_count = 50;

        // Expiry is expressed as the instant the caller hands in, not by
        // back-dating the detector's own window: the detector no longer reads
        // a clock, so `base + FLOOD_WINDOW_DURATION` IS the next window.
        // No sleep, no wall-clock dependency.
        //
        // To SEE THIS RED: in `maybe_reset_window`, change the guard to
        // `> FLOOD_WINDOW_DURATION` (strict). The boundary instant then no
        // longer expires the window, nothing decays, and all six asserts
        // below fail with the undecayed values (80/60/40/20/90/50).
        let _ = detector.check_flood(base + FLOOD_WINDOW_DURATION);

        assert_eq!(detector.rst_stream_count, 40);
        assert_eq!(detector.ping_count, 30);
        assert_eq!(detector.settings_count, 20);
        assert_eq!(detector.empty_data_count, 10);
        assert_eq!(detector.window_update_stream0_count, 45);
        assert_eq!(detector.glitch_count, 25);
    }

    #[test]
    fn test_flood_detector_window_update_stream0_trips_at_threshold() {
        let base = Instant::now();
        let config = H2FloodConfig {
            max_window_update_stream0_per_window: 5,
            ..H2FloodConfig::default()
        };
        let mut detector = H2FloodDetector::new(config, base);

        // At threshold — no flood yet (strict greater-than, matches existing counters).
        detector.window_update_stream0_count = 5;
        assert!(detector.check_flood(base).is_none());

        // Above threshold — flood with the correct violation reason + metric key.
        detector.window_update_stream0_count = 6;
        let violation = detector
            .check_flood(base)
            .expect("WINDOW_UPDATE stream-0 flood must trip above threshold");
        assert_eq!(violation.error, H2Error::EnhanceYourCalm);
        assert_eq!(violation.reason, "WINDOW_UPDATE stream 0");
        assert_eq!(
            violation.metric_key,
            "h2.flood.violation.window_update_stream0_window"
        );
        assert_eq!(violation.count, 6);
        assert_eq!(violation.threshold, 5);
    }

    #[test]
    fn test_flood_detector_window_update_stream0_honours_default() {
        // Default threshold must match the documented constant so operators
        // can reason about behaviour without reading code.
        let detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
        assert_eq!(
            detector.config.max_window_update_stream0_per_window,
            DEFAULT_MAX_WINDOW_UPDATE_STREAM0_PER_WINDOW
        );
        assert_eq!(detector.window_update_stream0_count, 0);
    }

    #[test]
    fn test_flood_detector_decay_prevents_flood() {
        let base = Instant::now();
        let config = H2FloodConfig {
            max_rst_stream_per_window: 10,
            ..H2FloodConfig::default()
        };
        let mut detector = H2FloodDetector::new(config, base);

        // Set counter just above threshold
        detector.rst_stream_count = 12;

        // Without decay -> flood
        assert!(matches!(
            detector.check_flood(base),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));

        // Reset and cross into the next window via the injected instant.
        detector.rst_stream_count = 12;

        // After decay: 12/2 = 6, which is below threshold 10 -> no flood.
        //
        // To SEE THIS RED: make `maybe_reset_window` take `Instant::now()`
        // instead of its `now` parameter. The injected future instant is then
        // ignored, the window never expires, the count stays 12 > 10 and this
        // assert trips on the flood it should have decayed away.
        assert!(detector.check_flood(base + FLOOD_WINDOW_DURATION).is_none());
    }

    #[test]
    fn test_flood_detector_lifetime_rst_cap_triggers_enhance_your_calm() {
        // CVE-2023-44487 Rapid Reset: a patient attacker that stays under
        // the half-decaying per-window threshold must still be stopped by
        // the lifetime cap. Simulate a response-started RST (no abusive
        // counter bump) so only the lifetime ceiling is tested.
        let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
        for _ in 0..DEFAULT_MAX_RST_STREAM_LIFETIME {
            assert!(detector.record_rst_lifetime(true).is_none());
        }
        assert_eq!(
            detector.total_rst_received_lifetime,
            DEFAULT_MAX_RST_STREAM_LIFETIME
        );
        assert_eq!(detector.total_abusive_rst_received_lifetime, 0);
        // Next RST crosses the ceiling.
        assert!(matches!(
            detector.record_rst_lifetime(true),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_abusive_rst_cap_triggers_first() {
        // Pre-response-start RSTs have a much lower ceiling; they trip
        // well before the generic lifetime cap.
        let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
        for _ in 0..DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME {
            assert!(detector.record_rst_lifetime(false).is_none());
        }
        assert_eq!(
            detector.total_abusive_rst_received_lifetime,
            DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME
        );
        assert!(matches!(
            detector.record_rst_lifetime(false),
            Some(H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                ..
            })
        ));
    }

    #[test]
    fn test_flood_detector_emitted_rst_below_threshold_is_clean() {
        // Server may legitimately RST some streams (protocol errors,
        // client-side abuse caught by other mitigations). Staying at the
        // threshold must not trip the ceiling.
        let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
        for _ in 0..DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME {
            assert!(detector.record_rst_emitted().is_none());
        }
        assert_eq!(
            detector.total_rst_streams_emitted_lifetime,
            DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME
        );
    }

    #[test]
    fn test_flood_detector_emitted_rst_cap_triggers_made_you_reset() {
        // CVE-2025-8671 MadeYouReset: unbounded server-emitted RST_STREAM is
        // a DoS vector equivalent to Rapid Reset with the emission direction
        // flipped. Crossing the ceiling must surface a EnhanceYourCalm
        // violation so the caller can GOAWAY.
        let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
        for _ in 0..DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME {
            assert!(detector.record_rst_emitted().is_none());
        }
        let violation = detector
            .record_rst_emitted()
            .expect("emitting past the cap should produce a violation");
        assert!(matches!(
            violation,
            H2FloodViolation {
                error: H2Error::EnhanceYourCalm,
                reason: "MadeYouReset: lifetime server-emitted RST_STREAM",
                ..
            }
        ));
        assert_eq!(violation.count, DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME + 1);
        assert_eq!(violation.threshold, DEFAULT_MAX_RST_STREAM_EMITTED_LIFETIME);
    }

    #[test]
    fn test_flood_detector_emitted_rst_counter_does_not_decay() {
        let base = Instant::now();
        // Unlike the windowed rst_stream_count, the emitted lifetime counter
        // is strictly monotonic — a patient attacker cannot reset it by
        // waiting out a window. maybe_reset_window must NOT touch it.
        let mut detector = H2FloodDetector::new(H2FloodConfig::default(), base);
        for _ in 0..10 {
            detector.record_rst_emitted();
        }
        // Force a window reset through the injected instant.
        //
        // To SEE THIS RED: add `self.total_rst_streams_emitted_lifetime /= 2;`
        // to the decay block in `maybe_reset_window`. The counter halves to 5
        // and this assert fails — which is the MadeYouReset ceiling (CVE-2025-8671)
        // becoming evadable by waiting out a window.
        let _ = detector.check_flood(base + FLOOD_WINDOW_DURATION);
        assert_eq!(detector.total_rst_streams_emitted_lifetime, 10);
    }

    /// Every violation kind must carry a metric_key under the agreed
    /// `h2.flood.violation.*` namespace, and the keys must be unique. The
    /// statsd counter at `ConnectionH2::handle_flood_violation` reads
    /// `violation.metric_key` directly — drift between the construction site
    /// and the metric name would silently lose alerting on a CVE mitigation.
    #[test]
    fn test_flood_violation_metric_keys_are_unique_and_namespaced() {
        // Helper: run `record_rst_lifetime` until it trips, returning the metric_key.
        fn key_from_rst_lifetime(response_started: bool) -> &'static str {
            let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
            loop {
                if let Some(v) = detector.record_rst_lifetime(response_started) {
                    return v.metric_key;
                }
            }
        }

        // Helper: run `record_rst_emitted` until it trips, returning the metric_key.
        fn key_from_rst_emitted() -> &'static str {
            let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
            loop {
                if let Some(v) = detector.record_rst_emitted() {
                    return v.metric_key;
                }
            }
        }

        // Helper: drive a single `check_flood` counter past its threshold.
        // A nested `fn` cannot capture, so it takes its own snapshot; seeding
        // the window and checking it at the same instant means no decay can
        // interfere with the threshold this helper is probing.
        fn key_from_check_flood(setup: impl FnOnce(&mut H2FloodDetector)) -> &'static str {
            let now = Instant::now();
            let mut detector = H2FloodDetector::new(H2FloodConfig::default(), now);
            setup(&mut detector);
            detector
                .check_flood(now)
                .expect("setup should always trip a flood")
                .metric_key
        }

        let keys: [&'static str; 12] = [
            // Lifetime methods on the detector itself.
            key_from_rst_lifetime(true),
            key_from_rst_lifetime(false),
            key_from_rst_emitted(),
            // `check_flood` arms.
            key_from_check_flood(|d| d.rst_stream_count = u32::MAX),
            key_from_check_flood(|d| d.ping_count = u32::MAX),
            key_from_check_flood(|d| d.total_ping_received_lifetime = u32::MAX),
            key_from_check_flood(|d| d.settings_count = u32::MAX),
            key_from_check_flood(|d| d.total_settings_received_lifetime = u32::MAX),
            key_from_check_flood(|d| d.empty_data_count = u32::MAX),
            key_from_check_flood(|d| d.continuation_count = u32::MAX),
            key_from_check_flood(|d| d.accumulated_header_size = u32::MAX),
            key_from_check_flood(|d| d.glitch_count = u32::MAX),
        ];

        for key in keys {
            assert!(
                key.starts_with("h2.flood.violation."),
                "metric key {key} is missing the h2.flood.violation. prefix",
            );
        }
        let mut deduped = keys.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            keys.len(),
            "metric keys must be unique across violation kinds; collisions: {keys:?}",
        );
    }

    #[test]
    fn test_flood_detector_response_started_rst_not_abusive() {
        // When the backend response has begun, the RST is cheap for us
        // too — it only bumps the generic lifetime counter.
        let mut detector = H2FloodDetector::new(H2FloodConfig::default(), Instant::now());
        for _ in 0..(DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME + 100) {
            assert!(detector.record_rst_lifetime(true).is_none());
        }
        assert_eq!(detector.total_abusive_rst_received_lifetime, 0);
        assert_eq!(
            detector.total_rst_received_lifetime,
            DEFAULT_MAX_RST_STREAM_ABUSIVE_LIFETIME + 100
        );
    }

    // ── H2FloodConfig ──────────────────────────────────────────────────

    #[test]
    fn test_flood_config_default_values() {
        let config = H2FloodConfig::default();
        assert_eq!(config.max_rst_stream_per_window, 100);
        assert_eq!(config.max_ping_per_window, 100);
        assert_eq!(config.max_settings_per_window, 50);
        assert_eq!(config.max_empty_data_per_window, 100);
        assert_eq!(config.max_continuation_frames, 20);
        assert_eq!(config.max_glitch_count, 100);
        assert_eq!(config.max_rst_stream_lifetime, 10_000);
        assert_eq!(config.max_rst_stream_abusive_lifetime, 50);
        assert_eq!(config.max_header_list_size, MAX_HEADER_LIST_SIZE as u32);
    }

    #[test]
    fn test_flood_config_default_matches_constants() {
        let config = H2FloodConfig::default();
        assert_eq!(
            config.max_rst_stream_per_window,
            DEFAULT_MAX_RST_STREAM_PER_WINDOW
        );
        assert_eq!(config.max_ping_per_window, DEFAULT_MAX_PING_PER_WINDOW);
        assert_eq!(
            config.max_settings_per_window,
            DEFAULT_MAX_SETTINGS_PER_WINDOW
        );
        assert_eq!(
            config.max_empty_data_per_window,
            DEFAULT_MAX_EMPTY_DATA_PER_WINDOW
        );
        assert_eq!(
            config.max_continuation_frames,
            DEFAULT_MAX_CONTINUATION_FRAMES
        );
        assert_eq!(config.max_glitch_count, DEFAULT_MAX_GLITCH_COUNT);
    }

    #[test]
    fn test_flood_config_equality() {
        let config_a = H2FloodConfig::default();
        let config_b = H2FloodConfig::default();
        assert_eq!(config_a, config_b);

        let config_c = H2FloodConfig {
            max_rst_stream_per_window: 1,
            ..H2FloodConfig::default()
        };
        assert_ne!(config_a, config_c);
    }

    /// An unset knob keeps the compile-time default, exactly as the
    /// `unwrap_or(defaults.<field>)` table `lib/src/http.rs` and
    /// `lib/src/https.rs` used to carry did.
    #[test]
    fn test_flood_config_from_optional_all_none_is_default() {
        let config = H2FloodConfig::from_optional(
            None, None, None, None, None, None, None, None, None, None, None, None, None,
        );
        assert_eq!(config, H2FloodConfig::default());
    }

    /// `from_optional` takes thirteen same-typed positional arguments, so a
    /// transposed pair compiles silently and would quietly apply one operator
    /// knob's value to a different CVE's threshold. Thirteen distinct values,
    /// each asserted against its own field, is what makes that a test failure
    /// instead of a production surprise.
    #[test]
    fn test_flood_config_from_optional_maps_each_knob_to_its_own_field() {
        let config = H2FloodConfig::from_optional(
            Some(11),
            Some(12),
            Some(13),
            Some(14),
            Some(15),
            Some(16),
            Some(17),
            Some(18),
            Some(19),
            Some(20),
            Some(21),
            Some(22),
            Some(23),
        );
        assert_eq!(config.max_rst_stream_per_window, 11);
        assert_eq!(config.max_ping_per_window, 12);
        assert_eq!(config.max_settings_per_window, 13);
        assert_eq!(config.max_empty_data_per_window, 14);
        assert_eq!(config.max_window_update_stream0_per_window, 15);
        assert_eq!(config.max_continuation_frames, 16);
        assert_eq!(config.max_glitch_count, 17);
        assert_eq!(config.max_rst_stream_lifetime, 18);
        assert_eq!(config.max_rst_stream_abusive_lifetime, 19);
        assert_eq!(config.max_rst_stream_emitted_lifetime, 20);
        assert_eq!(config.max_header_list_size, 21);
        assert_eq!(config.max_header_table_size, 22);
        assert_eq!(config.max_header_fields, 23);
    }

    /// Regression for sozu-proxy/sozu#1418: a zero that reached the runtime
    /// without passing config load must be clamped, not carried. `check_flood`
    /// compares `count > threshold`, so a zero per-window threshold trips on
    /// the first counted frame of every connection.
    #[test]
    fn test_flood_config_from_optional_clamps_every_zero_to_one() {
        let config = H2FloodConfig::from_optional(
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        );
        assert_eq!(config.max_rst_stream_per_window, 1);
        assert_eq!(config.max_ping_per_window, 1);
        assert_eq!(config.max_settings_per_window, 1);
        assert_eq!(config.max_empty_data_per_window, 1);
        assert_eq!(config.max_window_update_stream0_per_window, 1);
        assert_eq!(config.max_continuation_frames, 1);
        assert_eq!(config.max_glitch_count, 1);
        assert_eq!(config.max_rst_stream_lifetime, 1);
        assert_eq!(config.max_rst_stream_abusive_lifetime, 1);
        assert_eq!(config.max_rst_stream_emitted_lifetime, 1);
        assert_eq!(config.max_header_list_size, 1);
        assert_eq!(config.max_header_table_size, 1);
        assert_eq!(config.max_header_fields, 1);
    }

    /// Every accessor reads back its own field — the same transposition guard
    /// as above, on the read side that `h2.rs` uses.
    #[test]
    fn test_flood_config_accessors_read_their_own_field() {
        let config = H2FloodConfig::from_optional(
            Some(11),
            Some(12),
            Some(13),
            Some(14),
            Some(15),
            Some(16),
            Some(17),
            Some(18),
            Some(19),
            Some(20),
            Some(21),
            Some(22),
            Some(23),
        );
        assert_eq!(config.max_rst_stream_per_window(), 11);
        assert_eq!(config.max_ping_per_window(), 12);
        assert_eq!(config.max_settings_per_window(), 13);
        assert_eq!(config.max_empty_data_per_window(), 14);
        assert_eq!(config.max_window_update_stream0_per_window(), 15);
        assert_eq!(config.max_continuation_frames(), 16);
        assert_eq!(config.max_glitch_count(), 17);
        assert_eq!(config.max_rst_stream_lifetime(), 18);
        assert_eq!(config.max_rst_stream_abusive_lifetime(), 19);
        assert_eq!(config.max_rst_stream_emitted_lifetime(), 20);
        assert_eq!(config.max_header_list_size(), 21);
        assert_eq!(config.max_header_table_size(), 22);
        assert_eq!(config.max_header_fields(), 23);
    }
}
