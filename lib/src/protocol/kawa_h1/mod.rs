//! HTTP/1.1 building blocks shared with the `mux` datapath.
//!
//! Since the `mux` migration this module no longer owns a session state
//! machine. `HttpStateMachine` (`lib/src/http.rs`) and `HttpsStateMachine`
//! (`lib/src/https.rs`) drive HTTP/1.1 through `protocol::mux` in H1 mode,
//! and the `Http<Front, L>` session that used to live here was removed on
//! 2026-09-20 (sozu#1346) once a `panic!` probe proved no binary
//! constructed it.
//!
//! What remains is the H1 vocabulary `mux` builds on:
//!
//! - [`DefaultAnswer`] — the catalogue of synthesised replies, rendered by
//!   `answers::HttpAnswers` and selected by `mux::answers`;
//! - `editor::HttpContext` — per-request state plus the Kawa parser
//!   callbacks that rewrite `Forwarded` / `X-Forwarded-*` / `Sozu-Id`;
//! - `parser::Method` and `parser::hostname_and_port` — the owned-string-free
//!   method enum and the authority splitter used by the mux router.
//!
//! Long-form map: `lib/src/protocol/kawa_h1/LIFECYCLE.md`.

pub mod answers;
pub mod editor;
pub mod parser;

use crate::pool::Checkout;

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;

impl kawa::AsBuffer for Checkout {
    fn as_buffer(&self) -> &[u8] {
        self.inner.extra()
    }
    fn as_mut_buffer(&mut self) -> &mut [u8] {
        self.inner.extra_mut()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultAnswer {
    Answer301 {
        location: String,
    },
    /// RFC 9110 §15.4.3 — temporary redirect; user agents may rewrite POST
    /// to GET on follow. Surfaced by `RedirectPolicy::Found` (#1009).
    Answer302 {
        location: String,
    },
    /// RFC 9110 §15.4.9 — permanent redirect that PRESERVES the request
    /// method on follow (no GET-rewrite on POST). Surfaced by
    /// `RedirectPolicy::PermanentRedirect` (#1009).
    Answer308 {
        location: String,
    },
    Answer400 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        successfully_parsed: String,
        partially_parsed: String,
        invalid: String,
    },
    Answer401 {
        /// Optional `WWW-Authenticate` value (e.g. `Basic realm="…"`).
        /// When `None` or empty the realm header line is elided by the
        /// template engine so the response stays a bare 401.
        www_authenticate: Option<String>,
    },
    Answer404 {},
    Answer408 {
        duration: String,
    },
    Answer413 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        capacity: usize,
    },
    /// RFC 9110 §15.5.20 — returned when the request's `:authority` / `Host`
    /// host does not match the TLS SNI negotiated for this connection.
    /// The peer may retry on a fresh TLS connection that negotiates an SNI
    /// matching the authority.
    Answer421 {},
    /// RFC 6585 §4 — emitted when the per-(cluster, source-IP) connection
    /// limit is reached. `retry_after` is the suggested wait, in seconds;
    /// `None` (or `Some(0)`) tells the template engine to omit the header
    /// — `Retry-After: 0` invites an immediate retry that defeats the
    /// limit.
    Answer429 {
        retry_after: Option<u32>,
    },
    Answer502 {
        message: String,
        phase: kawa::ParsingPhaseMarker,
        successfully_parsed: String,
        partially_parsed: String,
        invalid: String,
    },
    Answer503 {
        message: String,
    },
    Answer504 {
        duration: String,
    },
    Answer507 {
        phase: kawa::ParsingPhaseMarker,
        message: String,
        capacity: usize,
    },
}

impl From<&DefaultAnswer> for u16 {
    fn from(answer: &DefaultAnswer) -> u16 {
        match answer {
            DefaultAnswer::Answer301 { .. } => 301,
            DefaultAnswer::Answer302 { .. } => 302,
            DefaultAnswer::Answer308 { .. } => 308,
            DefaultAnswer::Answer400 { .. } => 400,
            DefaultAnswer::Answer401 { .. } => 401,
            DefaultAnswer::Answer404 { .. } => 404,
            DefaultAnswer::Answer408 { .. } => 408,
            DefaultAnswer::Answer413 { .. } => 413,
            DefaultAnswer::Answer421 { .. } => 421,
            DefaultAnswer::Answer429 { .. } => 429,
            DefaultAnswer::Answer502 { .. } => 502,
            DefaultAnswer::Answer503 { .. } => 503,
            DefaultAnswer::Answer504 { .. } => 504,
            DefaultAnswer::Answer507 { .. } => 507,
        }
    }
}
