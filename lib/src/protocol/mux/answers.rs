//! Default-answer helpers shared by the H1 and H2 mux paths.
//!
//! These helpers materialise sozu's configured error templates into the
//! per-stream kawa buffers, then flip the appropriate readiness bits so the
//! response is flushed on the next writable pass.

use std::{cell::RefCell, rc::Rc, time::Duration};

use sozu_command::logging::ansi_palette;

use super::{GenericHttpStream, H2Error, Readiness, Stream, StreamState, h2::MetricEvent};
use crate::metrics::names;
use crate::protocol::http::{DefaultAnswer, answers::HttpAnswers};
use crate::{L7ListenerHandler, ListenerHandler};

/// Module-level prefix used on every log line emitted from the mux
/// answers helpers. The standalone helpers run before any per-stream
/// scope is in hand (or after it has been torn down), so a single
/// `MUX-ANSWERS` label is used, coloured bold bright-white (uniform
/// across every protocol) when the logger supports ANSI.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-ANSWERS{reset}\t >>>", open = open, reset = reset)
    }};
}

/// Terminate a default answer with optional `Connection: close` and `Cache-Control` headers.
///
/// Note: the `Connection: close` header is only valid for HTTP/1.1. For H2 streams,
/// the H2 block converter (`is_connection_specific_header`) automatically strips it
/// before serialization, so callers need not guard on the protocol version.
pub fn terminate_default_answer<T: kawa::AsBuffer>(kawa: &mut kawa::Kawa<T>, close: bool) {
    if close {
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Cache-Control"),
            val: kawa::Store::Static(b"no-cache"),
        }));
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Connection"),
            val: kawa::Store::Static(b"close"),
        }));
    }
    kawa.push_block(kawa::Block::Flags(kawa::Flags {
        end_body: false,
        end_chunk: false,
        end_header: true,
        end_stream: true,
    }));
    kawa.parsing_phase = kawa::ParsingPhase::Terminated;
}

/// Copy blocks from a rendered `DefaultAnswerStream` into `stream.back`.
///
/// The template-rendered stream uses `SharedBuffer` storage, so any `Store::Slice`
/// references must be captured (converted to owned `Store::Alloc`) before copying.
fn copy_default_answer_to_stream(
    rendered: crate::protocol::http::answers::DefaultAnswerStream,
    kawa: &mut GenericHttpStream,
) {
    let buf = rendered.storage.buffer();

    // Copy the status line, capturing any buffer-dependent Stores
    kawa.detached.status_line = match rendered.detached.status_line {
        kawa::StatusLine::Response {
            version,
            code,
            status,
            reason,
        } => kawa::StatusLine::Response {
            version,
            code,
            status: status.capture(buf),
            reason: reason.capture(buf),
        },
        other => other,
    };
    kawa.push_block(kawa::Block::StatusLine);

    // Copy all remaining blocks, capturing buffer-dependent Stores
    for block in rendered.blocks {
        let captured = match block {
            kawa::Block::StatusLine => continue, // already handled above
            kawa::Block::Header(kawa::Pair { key, val }) => kawa::Block::Header(kawa::Pair {
                key: key.capture(buf),
                val: val.capture(buf),
            }),
            kawa::Block::Chunk(kawa::Chunk { data }) => kawa::Block::Chunk(kawa::Chunk {
                data: data.capture(buf),
            }),
            kawa::Block::Flags(flags) => kawa::Block::Flags(flags),
            kawa::Block::Cookies => kawa::Block::Cookies,
            kawa::Block::ChunkHeader(kawa::ChunkHeader { length }) => {
                kawa::Block::ChunkHeader(kawa::ChunkHeader {
                    length: length.capture(buf),
                })
            }
        };
        kawa.push_block(captured);
    }

    kawa.parsing_phase = rendered.parsing_phase;
    kawa.body_size = rendered.body_size;
}

fn ensure_default_answer_end_stream(kawa: &mut GenericHttpStream) {
    let has_end_stream = kawa.blocks.iter().any(|block| {
        matches!(
            block,
            kawa::Block::Flags(kawa::Flags {
                end_stream: true,
                ..
            })
        )
    });

    if !has_end_stream {
        kawa.push_block(kawa::Block::Flags(kawa::Flags {
            end_body: false,
            end_chunk: false,
            end_header: false,
            end_stream: true,
        }));
    }
    kawa.parsing_phase = kawa::ParsingPhase::Terminated;
}

/// Build a `DefaultAnswer` variant for the given status code with placeholder fields.
///
/// The mux layer does not have HTTP/1.1 parse state, so error-detail fields
/// (`message`, `phase`, `successfully_parsed`, etc.) are filled with neutral
/// placeholders. The 301 redirect and 401 unauthorized variants take the
/// caller-supplied context (the `Location` URL and the `WWW-Authenticate`
/// realm respectively) so the routing layer can drive both `https_redirect`
/// and the explicit `RedirectPolicy::PERMANENT` /
/// `RedirectPolicy::UNAUTHORIZED` policies through the same chokepoint.
fn default_answer_for_code(
    code: u16,
    redirect_location: Option<&str>,
    www_authenticate: Option<&str>,
    retry_after: Option<u32>,
) -> DefaultAnswer {
    match code {
        301 => DefaultAnswer::Answer301 {
            location: redirect_location.unwrap_or_default().to_owned(),
        },
        302 => DefaultAnswer::Answer302 {
            location: redirect_location.unwrap_or_default().to_owned(),
        },
        308 => DefaultAnswer::Answer308 {
            location: redirect_location.unwrap_or_default().to_owned(),
        },
        400 => DefaultAnswer::Answer400 {
            message: String::new(),
            phase: kawa::ParsingPhaseMarker::Error,
            successfully_parsed: "null".to_owned(),
            partially_parsed: "null".to_owned(),
            invalid: "null".to_owned(),
        },
        401 => DefaultAnswer::Answer401 {
            // The realm flows in from `cluster.www_authenticate` via
            // `HttpContext.www_authenticate`. The template engine elides
            // the `WWW-Authenticate` header when the value is empty
            // (`or_elide_header = true`) so a None / empty realm yields
            // a plain 401 without a synthetic header.
            www_authenticate: www_authenticate.map(str::to_owned),
        },
        404 => DefaultAnswer::Answer404 {},
        408 => DefaultAnswer::Answer408 {
            duration: String::new(),
        },
        421 => DefaultAnswer::Answer421 {},
        429 => DefaultAnswer::Answer429 { retry_after },
        502 => DefaultAnswer::Answer502 {
            message: String::new(),
            phase: kawa::ParsingPhaseMarker::Error,
            successfully_parsed: "null".to_owned(),
            partially_parsed: "null".to_owned(),
            invalid: "null".to_owned(),
        },
        503 => DefaultAnswer::Answer503 {
            message: String::new(),
        },
        504 => DefaultAnswer::Answer504 {
            duration: String::new(),
        },
        _ => DefaultAnswer::Answer503 {
            message: format!("Unexpected error code: {code}"),
        },
    }
}

/// Replace the content of the kawa message with a default Sozu answer for a given status code.
///
/// Uses the listener's `HttpAnswers` templates to produce responses matching the configured
/// custom answers, so an operator's `answer_NNN` overrides keep rendering exactly as they did
/// before the mux took over the H1 datapath.
pub(crate) fn set_default_answer(
    stream: &mut Stream,
    readiness: &mut Readiness,
    code: u16,
    answers: &HttpAnswers,
) {
    set_default_answer_with_retry_after(stream, readiness, code, answers, None);
}

/// Variant of `set_default_answer` that also threads a `Retry-After` value
/// (in seconds) into the rendered answer for 429 responses. `None` (or
/// `Some(0)`) instructs the template engine to omit the `Retry-After`
/// header — `Retry-After: 0` invites an immediate retry that defeats the
/// per-(cluster, source-IP) limit. Other status codes ignore the value.
pub(crate) fn set_default_answer_with_retry_after(
    stream: &mut Stream,
    readiness: &mut Readiness,
    code: u16,
    answers: &HttpAnswers,
    retry_after: Option<u32>,
) {
    let metric_incarnation = stream.context.cluster_metrics_incarnation();
    let context = &mut stream.context;
    let kawa = &mut stream.back;
    kawa.clear();
    kawa.storage.clear();
    let key = match code {
        301 => "http.301.redirection",
        302 => "http.302.redirection",
        308 => "http.308.redirection",
        400 => names::http::ERR_400,
        401 => "http.401.errors",
        404 => names::http::ERR_404,
        408 => "http.408.errors",
        413 => "http.413.errors",
        421 => "http.421.errors",
        429 => "connections.rejected_per_cluster_ip",
        502 => "http.502.errors",
        503 => "http.503.errors",
        504 => "http.504.errors",
        507 => "http.507.errors",
        _ => "http.other.errors",
    };
    incr!(
        key,
        context.cluster_id.as_deref(),
        context.backend_id.as_deref(),
        metric_incarnation
    );

    // Routing layer stashes both the resolved `Location` URL (for 301) and
    // the `WWW-Authenticate` realm (for 401). Fall back to the legacy
    // `https://<authority><path>` shape when no explicit redirect target was
    // computed — this preserves behaviour for the historical
    // `cluster.https_redirect = true` path that did not carry per-frontend
    // policy.
    let is_redirect = matches!(code, 301 | 302 | 308);
    let fallback_location;
    let redirect_location = match context.redirect_location.as_deref() {
        Some(l) => Some(l),
        None if is_redirect => {
            fallback_location = format!(
                "https://{}{}",
                context.authority.as_deref().unwrap_or_default(),
                context.path.as_deref().unwrap_or_default()
            );
            Some(fallback_location.as_str())
        }
        None => None,
    };
    let answer = default_answer_for_code(
        code,
        redirect_location,
        context.www_authenticate.as_deref(),
        retry_after,
    );

    let request_id = context.id.to_string();
    let route = context.get_route();
    let cluster_id = context.cluster_id.as_deref();
    let backend_id = context.backend_id.as_deref();

    // ── Frontend-scoped redirect template override (#1009) ──
    //
    // A `Frontend` with `redirect = permanent | found | permanent_redirect`
    // may carry its own `redirect_template` body that overrides the
    // persistent listener / cluster default for this one request.
    // Compile on demand via `HttpAnswers::render_inline_redirect`; on
    // any compile failure (operator config bug surfacing late) fall
    // through to the regular `answers.get` path so the request still
    // completes with the listener default — a rendered fallback is
    // always preferable to a hung response or 503.
    let inline_override = if is_redirect {
        context
            .frontend_redirect_template
            .as_deref()
            .and_then(|template_str| {
                let result = HttpAnswers::render_inline_redirect(
                    code,
                    template_str,
                    context.redirect_location.clone(),
                    context.id.to_string(),
                    context.get_route(),
                );
                if result.is_none() {
                    error!(
                        "{} frontend redirect_template failed to compile, falling back to default {} template",
                        log_module_context!(),
                        code
                    );
                    incr!(names::http::REDIRECT_TEMPLATE_COMPILE_ERROR);
                }
                result
            })
    } else {
        None
    };

    let (resolved_status, keep_alive, rendered) = inline_override
        .unwrap_or_else(|| answers.get(answer, request_id, cluster_id, backend_id, route));
    // Honour the rendered template's `Connection` header. Most error
    // templates carry `Connection: close`, which flips the frontend
    // keep-alive bit to false; cluster operators can opt back into
    // keep-alive by shipping a custom template without that header.
    // Only an H1 frontend reads the bit: on H2 the header is stripped
    // (RFC 9113 §8.2.2) and the answer ends its own stream only.
    if !keep_alive {
        context.keep_alive_frontend = false;
    }
    copy_default_answer_to_stream(rendered, kawa);
    ensure_default_answer_end_stream(kawa);

    // The template's resolved status may differ from the requested code
    // when a fallback template is selected (e.g. unknown status).
    context.status = Some(resolved_status);
    stream.state = StreamState::Unlinked;
    readiness.arm_writable();
    incr!(names::h2::SIGNAL_WRITABLE_REARMED_DEFAULT_ANSWER);
}

/// Forcefully terminates a kawa message by setting the "end_stream" flag and setting the parsing_phase to Error.
/// An H2 converter will produce an RstStream frame.
pub(crate) fn forcefully_terminate_answer(
    stream: &mut Stream,
    readiness: &mut Readiness,
    error: H2Error,
) {
    let kawa = &mut stream.back;
    kawa.out.clear();
    kawa.blocks.clear();
    kawa.parsing_phase.error(error.as_str().into());
    stream.state = StreamState::Unlinked;
    readiness.arm_writable();
    incr!(names::h2::SIGNAL_WRITABLE_REARMED_FORCEFULLY_TERMINATE_ANSWER);
}

/// Access-log status of a request refused by `RedirectPolicy::SilentClose`.
/// nginx logs 444 for the same close-without-response; it never reaches the
/// wire.
pub(super) const SILENT_CLOSE_STATUS: u16 = 444;

/// Access-log message of a request refused by `RedirectPolicy::SilentClose`.
pub(super) const SILENT_CLOSE_MESSAGE: &str = "silent_close";

/// Refuse a request without writing a single response byte
/// (`RedirectPolicy::SilentClose`, sozu-proxy/sozu#767), and emit its access
/// log. Returns the metric events the caller must record.
///
/// The response ends in error with `H2Error::Cancel` and nothing queued, via
/// [`forcefully_terminate_answer`]: an H2 frontend turns that into a
/// RST_STREAM(CANCEL) on this stream only, and an H1 frontend closes the
/// connection without writing (the incomplete-response branch of
/// `ConnectionH1::writable`, `lib/src/protocol/mux/h1.rs`).
///
/// The access log is emitted here, not left to the path that later retires
/// the stream, because each of those paths (`Mux::close` in
/// `lib/src/protocol/mux/mod.rs`, `ConnectionH2::try_recycle_server_stream`
/// in `lib/src/protocol/mux/h2.rs`) passes its own message, which
/// `Stream::generate_access_log` prefers over `access_log_message`. Resetting
/// the metrics afterwards is what both of them read as "already logged". The
/// line is not an error: the refusal is the frontend's configured policy, like
/// a 401 or a redirect.
#[must_use]
pub(super) fn silent_close_answer<L>(
    stream: &mut Stream,
    readiness: &mut Readiness,
    listener: Rc<RefCell<L>>,
    client_rtt: Option<Duration>,
) -> [Option<MetricEvent>; 2]
where
    L: ListenerHandler + L7ListenerHandler,
{
    stream.context.status = Some(SILENT_CLOSE_STATUS);
    forcefully_terminate_answer(stream, readiness, H2Error::Cancel);
    debug_assert!(
        stream.back.is_error() && stream.back.is_completed(),
        "a silent close must leave an errored response with nothing to write"
    );
    let events = stream.generate_access_log(
        false,
        Some(SILENT_CLOSE_MESSAGE),
        listener,
        client_rtt,
        None,
    );
    stream.metrics.reset();
    debug_assert!(
        stream.metrics.start.is_none(),
        "a logged silent close must read as already logged to every later path"
    );
    events
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use rusty_ulid::Ulid;
    use sozu_command::proto::command::SocketAddress;

    use super::*;
    use crate::{
        Protocol,
        pool::Pool,
        protocol::{kawa_h1::editor::HttpContext, mux::buffer_source::PoolBufferSource},
    };

    fn make_stream() -> (Rc<RefCell<Pool>>, Stream) {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 20, 16_384)));
        let session_id = Ulid::generate();
        let http_ctx = HttpContext {
            keep_alive_backend: true,
            keep_alive_frontend: true,
            sticky_session_found: None,
            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
            user_agent: None,
            x_request_id: None,
            xff_chain: None,
            #[cfg(feature = "opentelemetry")]
            otel: None,
            closing: false,
            session_id,
            id: Ulid::generate(),
            backend_id: None,
            cluster_id: None,
            cluster_metrics_incarnation: Default::default(),
            affinity_key: None,
            protocol: Protocol::HTTPS,
            public_address: SocketAddress::new_v4(127, 0, 0, 1, 0).into(),
            session_address: None,
            sticky_name: String::new(),
            sticky_session: None,
            backend_address: None,
            tls_server_name: None,
            tls_cert_names: None,
            strict_sni_binding: false,
            elide_x_real_ip: false,
            send_x_real_ip: false,
            forwarded_headers: sozu_command_lib::proto::command::ForwardedHeaders::Both,
            max_trailer_fields: 128,
            trailer_fields: 0,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
            sozu_id_header: String::from("Sozu-Id"),
            redirect_location: None,
            www_authenticate: None,
            original_authority: None,
            headers_response: Vec::new(),
            retry_after_seconds: None,
            frontend_redirect_template: None,
            redirect_status: None,
            tags: None,
            forwarding_hop: None,
            access_log_message: None,
            backends_unavailable: false,
        };
        let stream = Stream::new(
            &mut PoolBufferSource::new(Rc::downgrade(&pool)),
            http_ctx,
            crate::protocol::mux::test_support::test_answers(),
            65_535,
        )
        .expect("pool checkout failed");
        (pool, stream)
    }

    /// `set_default_answer` must leave the frontend readiness with both
    /// `interest` AND `event` carrying `Ready::WRITABLE`. The `event` bit
    /// is the `signal_pending_write` side of the invariant-15 pair — under
    /// edge-triggered epoll, this is what causes the scheduler's next
    /// tick to re-enter `writable()` and flush the default answer without
    /// waiting for an external epoll event.
    #[test]
    fn set_default_answer_arms_writable_and_signals() {
        let (_pool, mut stream) = make_stream();
        let mut readiness = Readiness::new();
        let answers = HttpAnswers::new(&std::collections::BTreeMap::new())
            .expect("HttpAnswers::new with empty map must succeed");

        set_default_answer(&mut stream, &mut readiness, 504, &answers);

        assert!(
            readiness.interest.is_writable(),
            "set_default_answer must leave Ready::WRITABLE in interest: {readiness:?}"
        );
        assert!(
            readiness.event.is_writable(),
            "set_default_answer must pair the insert with signal_pending_write so \
             ET epoll sees WRITABLE on the event side: {readiness:?}"
        );
    }

    /// A silent close (`RedirectPolicy::SilentClose`, sozu-proxy/sozu#767)
    /// leaves nothing to write, ends the response in error with the code an
    /// H2 frontend sends as RST_STREAM, and logs the request once, as 444
    /// `silent_close`, marking it logged for every later path.
    ///
    /// To SEE THIS RED: in `silent_close_answer`
    /// (`lib/src/protocol/mux/answers.rs`), delete the
    /// `generate_access_log` call and the `metrics.reset()` that follows it,
    /// leaving the log to the path that retires the stream, as the stream
    /// is otherwise handled. No line is emitted here and `metrics.start`
    /// stays set, so `Mux::close` later logs it as `session close`.
    #[test]
    fn a_silent_close_writes_nothing_and_logs_444_silent_close() {
        let output = crate::capture_test_logs(|| {
            let (_pool, mut stream) = make_stream();
            stream.context.method = Some(crate::protocol::http::parser::Method::Get);
            stream.context.authority = Some("blocked.example.com".to_owned());
            stream.context.path = Some("/silent-767".to_owned());
            stream.metrics.service_start();
            let mut readiness = Readiness::new();

            let _ = silent_close_answer(
                &mut stream,
                &mut readiness,
                Rc::new(RefCell::new(
                    crate::protocol::mux::test_support::TestListener::new(),
                )),
                None,
            );

            assert!(
                stream.back.out.is_empty() && stream.back.blocks.is_empty(),
                "a silent close must queue no response byte"
            );
            let code = match stream.back.parsing_phase {
                kawa::ParsingPhase::Error {
                    kind: kawa::ParsingErrorKind::Processing { message },
                    ..
                } => message.parse::<H2Error>().ok(),
                _ => None,
            };
            assert_eq!(
                code,
                Some(H2Error::Cancel),
                "an H2 frontend must reset the stream with CANCEL"
            );
            assert_eq!(stream.state, StreamState::Unlinked);
            assert!(
                readiness.interest.is_writable(),
                "the writable pass must run to reset or close: {readiness:?}"
            );
            assert_eq!(stream.context.status, Some(SILENT_CLOSE_STATUS));
            assert!(
                stream.metrics.start.is_none(),
                "the stream must read as already logged"
            );
        });

        let lines: Vec<&str> = output
            .lines()
            .filter(|line| line.contains("/silent-767"))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly one access-log line for the refused request, got: {output}"
        );
        assert!(
            lines[0].contains(" 444 ") && lines[0].contains(SILENT_CLOSE_MESSAGE),
            "the line must carry status 444 and `silent_close`, got: {}",
            lines[0]
        );
    }
}
