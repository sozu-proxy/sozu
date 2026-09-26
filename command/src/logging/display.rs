use std::fmt;

use rusty_ulid::Ulid;

use crate::{
    AsStr,
    logging::{
        EndpointRecord, FullTags, LogAddress, LogContext, LogDuration, LogLevel, LogMessage,
        LoggerBackend, Rfc3339Time,
    },
};

impl LogLevel {
    pub const fn as_str(&self, access: bool, colored: bool) -> &'static str {
        match (self, access, colored) {
            (LogLevel::Error, false, false) => "ERROR",
            (LogLevel::Warn, false, false) => "WARN ",
            (LogLevel::Info, false, false) => "INFO ",
            (LogLevel::Debug, false, false) => "DEBUG",
            (LogLevel::Trace, false, false) => "TRACE",

            (LogLevel::Error, false, true) => "\x1b[;31;1mERROR",
            (LogLevel::Warn, false, true) => "\x1b[;33;1mWARN ",
            (LogLevel::Info, false, true) => "\x1b[;32;1mINFO ",
            (LogLevel::Debug, false, true) => "\x1b[;36mDEBUG",
            (LogLevel::Trace, false, true) => "\x1b[;90mTRACE",

            (LogLevel::Error, true, false) => "ERROR-ACCESS",
            (LogLevel::Info, true, false) => "INFO-ACCESS ",
            (_, true, false) => "???",

            (LogLevel::Error, true, true) => "\x1b[;35;1mERROR-ACCESS",
            (LogLevel::Info, true, true) => "\x1b[;35;1mINFO-ACCESS ",
            (_, true, true) => "\x1b[;35;1m???",
        }
    }
}

impl AsRef<str> for LoggerBackend {
    fn as_ref(&self) -> &str {
        match self {
            LoggerBackend::Stdout(_) => "stdout",
            LoggerBackend::Unix(_) => "UNIX socket",
            LoggerBackend::Udp(_, _) => "UDP socket",
            LoggerBackend::Tcp(_) => "TCP socket",
            LoggerBackend::File(_) => "file",
        }
    }
}

impl fmt::Display for Rfc3339Time {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let t = self.inner;
        write!(
            f,
            "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.microsecond()
        )
    }
}

impl fmt::Display for LogMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(message) => write!(f, " | {message}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for LogDuration {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            None => write!(f, "-"),
            Some(duration) => {
                let secs = duration.as_secs();
                if secs >= 10 {
                    return write!(f, "{secs}s");
                }

                let ms = duration.as_millis();
                if ms < 10 {
                    let us = duration.as_micros();
                    if us >= 10 {
                        return write!(f, "{us}μs");
                    }

                    let ns = duration.as_nanos();
                    return write!(f, "{ns}ns");
                }

                write!(f, "{ms}ms")
            }
        }
    }
}

impl fmt::Display for LogAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            // `SocketAddr`'s own `Display` formats on the stack.
            Some(address) => write!(f, "{address}"),
            None => f.write_str("-"),
        }
    }
}

/// Crockford base32 alphabet of a ULID, the one `rusty_ulid` encodes with.
const ULID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Write `ulid` in its 26-character canonical form.
///
/// `rusty_ulid`'s `Display` is `f.write_str(&self.to_string())`, one `String`
/// per ULID, and every log line carrying a [`LogContext`] renders one or two
/// of them. This encodes into a stack array instead, with the same alphabet
/// and the same bit layout: the first character carries the top 3 bits of the
/// 128-bit value, each of the other 25 the next 5.
fn write_ulid(f: &mut fmt::Formatter, ulid: Ulid) -> fmt::Result {
    let value = u128::from(ulid);
    let mut encoded = [0u8; 26];
    for (index, digit) in encoded.iter_mut().enumerate() {
        let shift = 125 - 5 * index;
        *digit = ULID_ALPHABET[((value >> shift) & 0x1f) as usize];
    }
    // Every byte comes from the ASCII alphabet above, so this never fails.
    f.write_str(std::str::from_utf8(&encoded).map_err(|_| fmt::Error)?)
}

impl fmt::Display for LogContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("[")?;
        write_ulid(f, self.session_id)?;
        f.write_str(" ")?;
        match self.request_id {
            Some(id) => write_ulid(f, id)?,
            None => f.write_str("-")?,
        }
        write!(
            f,
            " {} {}]",
            self.cluster_id.unwrap_or("-"),
            self.backend_id.unwrap_or("-")
        )
    }
}

impl fmt::Display for EndpointRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Http {
                method,
                authority,
                path,
                status,
                ..
            } => {
                let pretty = f.alternate();
                write!(
                    f,
                    "{} {} {} ",
                    authority.as_str_or("-"),
                    method.as_str_or("-"),
                    path.as_str_or("-"),
                )?;
                write_status(f, *status, pretty)
            }
            Self::Tcp => {
                write!(f, "-")
            }
        }
    }
}

/// Write the status code, colored by class when `pretty`, straight into the
/// formatter rather than through an intermediate `String`.
fn write_status(f: &mut fmt::Formatter, status: Option<u16>, pretty: bool) -> fmt::Result {
    match (status, pretty) {
        (Some(s @ 200..=299), true) => write!(f, "\x1b[32m{s}"),
        (Some(s @ 300..=399), true) => write!(f, "\x1b[34m{s}"),
        (Some(s @ 400..=499), true) => write!(f, "\x1b[33m{s}"),
        (Some(s @ 500..=599), true) => write!(f, "\x1b[31m{s}"),
        (Some(s), _) => write!(f, "{s}"),
        (None, _) => f.write_str("-"),
    }
}

impl fmt::Display for FullTags<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.concatenated, self.user_agent) {
            (None, None) => Ok(()),
            (Some(tags), None) => write!(f, "{tags}"),
            (Some(tags), Some(ua)) if !tags.is_empty() => {
                write!(f, "{tags}, user-agent={}", EscapedUserAgent(ua))
            }
            (_, Some(ua)) => write!(f, "user-agent={}", EscapedUserAgent(ua)),
        }
    }
}

/// A user agent as the access log renders it: every space becomes `_`, `[`
/// becomes `{` and `]` becomes `}`, so the value can neither split the line
/// into more columns nor close the bracketed tag block early. Every other
/// byte is copied verbatim.
///
/// The spans between two replaced bytes are written straight into the
/// formatter. The three replaced bytes are ASCII and never occur inside a
/// multi-byte UTF-8 sequence, so splitting on them keeps every span valid.
struct EscapedUserAgent<'a>(&'a str);

impl fmt::Display for EscapedUserAgent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rest = self.0;
        while let Some(index) = rest.find([' ', '[', ']']) {
            f.write_str(&rest[..index])?;
            f.write_str(match rest.as_bytes()[index] {
                b' ' => "_",
                b'[' => "{",
                _ => "}",
            })?;
            rest = &rest[index + 1..];
        }
        f.write_str(rest)
    }
}

#[cfg(test)]
mod tests {
    use rusty_ulid::Ulid;

    use super::EscapedUserAgent;
    use crate::logging::{EndpointRecord, FullTags, LogAddress, LogContext};

    /// The allocating renderer this module used before it wrote into the
    /// formatter, kept as the byte-for-byte oracle of the escaping.
    fn replace_chain(user_agent: &str) -> String {
        user_agent
            .replace(' ', "_")
            .replace('[', "{")
            .replace(']', "}")
    }

    /// Edge cases of the user-agent escaping, each with its expected
    /// rendering. Only space, `[` and `]` are rewritten: control bytes,
    /// quotes, tabs, other whitespace and non-ASCII pass through unchanged.
    const USER_AGENTS: &[(&str, &str)] = &[
        ("", ""),
        ("curl/8.18.0", "curl/8.18.0"),
        (" ", "_"),
        ("[", "{"),
        ("]", "}"),
        ("  [[]]  ", "__{{}}__"),
        (
            "Mozilla/5.0 (X11; Linux x86_64) [en]",
            "Mozilla/5.0_(X11;_Linux_x86_64)_{en}",
        ),
        ("leading and trailing ", "leading_and_trailing_"),
        ("tab\there", "tab\there"),
        ("nul\0byte esc\x1b[31m", "nul\0byte_esc\x1b{31m"),
        ("cr\r\nlf del\x7f", "cr\r\nlf_del\x7f"),
        ("\"double\" 'single'", "\"double\"_'single'"),
        ("back\\slash", "back\\slash"),
        ("nbsp\u{a0}kept", "nbsp\u{a0}kept"),
        ("ideographic\u{3000}space", "ideographic\u{3000}space"),
        (
            "caf\u{e9} [\u{65e5}\u{672c}] \u{1f980}",
            "caf\u{e9}_{\u{65e5}\u{672c}}_\u{1f980}",
        ),
        ("{already} braces_", "{already}_braces_"),
    ];

    #[test]
    fn user_agent_escaping_matches_the_expected_rendering() {
        for (input, expected) in USER_AGENTS {
            assert_eq!(
                EscapedUserAgent(input).to_string(),
                *expected,
                "user agent {input:?}"
            );
        }
    }

    #[test]
    fn user_agent_escaping_is_byte_identical_to_the_replace_chain() {
        for (input, _) in USER_AGENTS {
            assert_eq!(
                EscapedUserAgent(input).to_string().as_bytes(),
                replace_chain(input).as_bytes(),
                "user agent {input:?}"
            );
        }
    }

    #[test]
    fn full_tags_render_the_escaped_user_agent() {
        let ua = Some("a b [c]");
        let render = |concatenated| {
            FullTags {
                concatenated,
                user_agent: ua,
            }
            .to_string()
        };
        assert_eq!(render(Some("k=v")), "k=v, user-agent=a_b_{c}");
        assert_eq!(render(Some("")), "user-agent=a_b_{c}");
        assert_eq!(render(None), "user-agent=a_b_{c}");
    }

    #[test]
    fn ulid_rendering_is_byte_identical_to_rusty_ulid() {
        let values = [
            0u128,
            1,
            0x1f,
            0x20,
            u128::MAX,
            u128::MAX >> 3,
            1 << 125,
            0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210,
            0x0192_3A4B_5C6D_7E8F_9012_3456_7890_ABCD,
        ];
        for value in values {
            let ulid = Ulid::from(value);
            let ctx = LogContext {
                session_id: ulid,
                request_id: Some(ulid),
                cluster_id: None,
                backend_id: None,
            };
            let expected = ulid.to_string();
            assert_eq!(
                ctx.to_string(),
                format!("[{expected} {expected} - -]"),
                "ULID {value:#x}"
            );
        }
        // A generated ULID carries a real timestamp and random bits.
        for _ in 0..64 {
            let ulid = Ulid::generate();
            let ctx = LogContext {
                session_id: ulid,
                request_id: None,
                cluster_id: None,
                backend_id: None,
            };
            assert_eq!(ctx.to_string(), format!("[{ulid} - - -]"));
        }
    }

    #[test]
    fn socket_addresses_render_like_their_display_or_a_dash() {
        let v4: std::net::SocketAddr = "127.0.0.1:49312".parse().unwrap();
        let v6: std::net::SocketAddr = "[2001:db8::1]:8443".parse().unwrap();
        assert_eq!(LogAddress(Some(v4)).to_string(), "127.0.0.1:49312");
        assert_eq!(LogAddress(Some(v6)).to_string(), "[2001:db8::1]:8443");
        assert_eq!(LogAddress(None).to_string(), "-");
    }

    #[test]
    fn endpoint_status_renders_plain_and_colored() {
        let endpoint = |status| EndpointRecord::Http {
            method: Some("GET"),
            authority: Some("example.com"),
            path: Some("/"),
            status,
            reason: None,
        };
        assert_eq!(format!("{}", endpoint(Some(200))), "example.com GET / 200");
        assert_eq!(format!("{}", endpoint(Some(101))), "example.com GET / 101");
        assert_eq!(format!("{}", endpoint(None)), "example.com GET / -");
        assert_eq!(
            format!("{:#}", endpoint(Some(204))),
            "example.com GET / \x1b[32m204"
        );
        assert_eq!(
            format!("{:#}", endpoint(Some(302))),
            "example.com GET / \x1b[34m302"
        );
        assert_eq!(
            format!("{:#}", endpoint(Some(404))),
            "example.com GET / \x1b[33m404"
        );
        assert_eq!(
            format!("{:#}", endpoint(Some(503))),
            "example.com GET / \x1b[31m503"
        );
        assert_eq!(
            format!("{:#}", endpoint(Some(101))),
            "example.com GET / 101"
        );
        assert_eq!(format!("{:#}", endpoint(None)), "example.com GET / -");
        assert_eq!(format!("{}", EndpointRecord::Tcp), "-");
    }

    #[test]
    fn log_context_display_all_fields_present() {
        let session = Ulid::from(0x01_23_45_67_89_AB_CD_EF_FE_DC_BA_98_76_54_32_10_u128);
        let request = Ulid::from(0x01_23_45_67_89_AB_CD_EF_FE_DC_BA_98_76_54_32_11_u128);
        let ctx = LogContext {
            session_id: session,
            request_id: Some(request),
            cluster_id: Some("cluster-abc"),
            backend_id: Some("backend-1"),
        };
        let rendered = format!("{ctx}");
        assert_eq!(
            rendered,
            format!("[{session} {request} cluster-abc backend-1]")
        );
    }

    #[test]
    fn log_context_display_dashes_when_missing() {
        let session = Ulid::from(0xABCDu128);
        let ctx = LogContext {
            session_id: session,
            request_id: None,
            cluster_id: None,
            backend_id: None,
        };
        assert_eq!(format!("{ctx}"), format!("[{session} - - -]"));
    }

    #[test]
    fn log_context_display_partial() {
        let session = Ulid::from(0x42u128);
        let request = Ulid::from(0x43u128);
        let ctx = LogContext {
            session_id: session,
            request_id: Some(request),
            cluster_id: None,
            backend_id: Some("backend-2"),
        };
        assert_eq!(
            format!("{ctx}"),
            format!("[{session} {request} - backend-2]")
        );
    }
}
