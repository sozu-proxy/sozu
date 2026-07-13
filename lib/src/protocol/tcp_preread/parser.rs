//! ClientHello wire parser (RFC 8446 §5.1 record layer + §4.1.2 ClientHello).
//!
//! Pure, sans-io, nom-based. [`parse_client_hello`] is the only entry point:
//! it walks TLS records from the front of a borrowed buffer, reassembles a
//! (possibly multi-record) ClientHello handshake message, and extracts the
//! `server_name` (RFC 6066 §3) and `application_layer_protocol_negotiation`
//! (RFC 7301 §3.1) extensions plus the presence of `encrypted_client_hello`
//! (draft-ietf-tls-esni, assigned type `0xfe0d`). It never mutates `buf`,
//! and copies from it only for the transient multi-record reassembly buffer
//! ([`append_fragment`]) and the extracted SNI/ALPN values it returns; it
//! performs no semantic validation beyond framing -- cipher/version
//! negotiation is the backend's job.
//!
//! Two parsing regimes coexist deliberately:
//! - The **outer record/handshake framing** uses `nom::*::streaming`
//!   combinators, because the caller may not have received the whole
//!   ClientHello yet -- a genuine `Err::Incomplete` must surface as
//!   [`ParseOutcome::NeedMore`].
//! - The **inner ClientHello body** (session id, cipher suites, extensions)
//!   uses `nom::*::complete` combinators: by the time [`parse_client_hello_body`]
//!   runs, the handshake framing has already proven the body is fully
//!   present, so any further shortfall is a lying length field --
//!   [`RejectReason::MalformedHandshake`], never `NeedMore`.

use std::borrow::Cow;

use nom::Err as NomErr;

use super::RejectReason;

/// TLS record `ContentType::handshake` (RFC 8446 §5.1).
const TLS_CONTENT_TYPE_HANDSHAKE: u8 = 22;

/// Handshake `msg_type` for `client_hello` (RFC 8446 §4).
const TLS_HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 1;

/// Maximum plaintext TLS record payload length (RFC 8446 §5.1: "length MUST
/// NOT exceed 2^14 bytes"). Enforced on every record, not just the ones
/// that end up carrying ClientHello bytes.
const MAX_TLS_RECORD_LEN: usize = 16384;

/// `server_name` extension (RFC 6066 §3).
const EXT_SERVER_NAME: u16 = 0x0000;
/// `application_layer_protocol_negotiation` extension (RFC 7301 §3.1).
const EXT_ALPN: u16 = 0x0010;
/// `encrypted_client_hello` extension (draft-ietf-tls-esni, assigned `0xfe0d`).
const EXT_ENCRYPTED_CLIENT_HELLO: u16 = 0xfe0d;

/// Outcome of attempting to parse a ClientHello out of a borrowed buffer.
/// Mirrors [`super::Output`] minus the routing decision, which the caller
/// (`mod.rs`) makes once it also has the [`super::PrereadConfig`] in scope.
pub(super) enum ParseOutcome {
    /// Not enough bytes yet to reach a verdict; the caller re-feeds the full
    /// accumulated buffer on the next `Bytes` input.
    NeedMore,
    /// The buffer can never become a valid ClientHello (or hit a hard cap).
    Reject(RejectReason),
    /// A complete ClientHello was decoded. `sni` is the raw wire-form
    /// `host_name` (not yet normalized -- the caller lowercases + strips a
    /// trailing dot); `alpn` preserves client offer order; `ech_present`
    /// flags whether `0xfe0d` was seen (used to distinguish `NoSni` from
    /// `EchOuterAbsent`).
    ClientHello {
        sni: Option<String>,
        alpn: Vec<Vec<u8>>,
        ech_present: bool,
    },
}

/// TLS record layer header (RFC 8446 §5.1): 1-byte `ContentType`, 2-byte
/// legacy `ProtocolVersion` (tolerated, unvalidated -- RFC 8446 §5.1
/// deprecates the field and requires ignoring it; an initial ClientHello
/// record may even carry `{3, 1}` for middlebox compatibility), 2-byte
/// big-endian length.
struct RecordHeader {
    content_type: u8,
    length: u16,
}

/// Parse a single TLS record header. Streaming: short input yields
/// `Err::Incomplete`, mapped by the caller to [`ParseOutcome::NeedMore`].
fn record_header(i: &[u8]) -> nom::IResult<&[u8], RecordHeader> {
    let in_len = i.len();
    let (i, content_type) = nom::number::streaming::be_u8(i)?;
    let (i, _legacy_record_version) = nom::number::streaming::be_u16(i)?;
    let (i, length) = nom::number::streaming::be_u16(i)?;
    // Post: the fixed 5-byte record header is consumed exactly, on every
    // success path -- never more (it would desync the next record), never
    // less (a short read must have been `Incomplete`, not a short `Ok`).
    debug_assert!(i.len() <= in_len, "record_header must not grow its input");
    debug_assert_eq!(
        in_len - i.len(),
        5,
        "TLS record header is exactly 5 bytes (type + legacy_version + length)"
    );
    Ok((
        i,
        RecordHeader {
            content_type,
            length,
        },
    ))
}

/// Take exactly `length` bytes of record content, streaming. Wrapped in its
/// own function (rather than calling `nom::bytes::streaming::take` inline)
/// so the default `nom::error::Error<&[u8]>` error type is pinned by the
/// return type instead of left ambiguous at the call site.
fn take_record_content(i: &[u8], length: u16) -> nom::IResult<&[u8], &[u8]> {
    nom::bytes::streaming::take(length)(i)
}

/// Append a newly-read record's content to the handshake reassembly
/// accumulator. Zero-copy for the single-record case (by far the common
/// one): the first fragment is returned `Cow::Borrowed` straight out of
/// `buf`. Only a ClientHello genuinely split across records pays for a
/// `Vec` -- and even then, `buf` itself is never touched, only copied from.
fn append_fragment<'a>(acc: Cow<'a, [u8]>, fragment: &'a [u8]) -> Cow<'a, [u8]> {
    match acc {
        Cow::Borrowed([]) => Cow::Borrowed(fragment),
        Cow::Borrowed(b) => {
            let mut owned = Vec::with_capacity(b.len() + fragment.len());
            owned.extend_from_slice(b);
            owned.extend_from_slice(fragment);
            Cow::Owned(owned)
        }
        Cow::Owned(mut owned) => {
            owned.extend_from_slice(fragment);
            Cow::Owned(owned)
        }
    }
}

/// Walk TLS records from the front of `buf`, reassembling the (possibly
/// multi-record) ClientHello handshake message, then decode it.
///
/// `buf` is exactly the post-PROXY-header slice (`content_offset` already
/// applied by the caller); this function never sees or reasons about the
/// PROXY-v2 prefix.
pub(super) fn parse_client_hello(buf: &[u8]) -> ParseOutcome {
    let mut pos = 0usize;
    let mut handshake: Cow<'_, [u8]> = Cow::Borrowed(&[][..]);
    let mut records_seen: u32 = 0;
    let mut msg_type_checked = false;

    loop {
        let remaining = &buf[pos..];
        let (after_header, header) = match record_header(remaining) {
            Ok(v) => v,
            Err(NomErr::Incomplete(_)) => return ParseOutcome::NeedMore,
            Err(_) => return ParseOutcome::Reject(RejectReason::MalformedRecord),
        };
        records_seen += 1;

        if header.content_type != TLS_CONTENT_TYPE_HANDSHAKE {
            // The very FIRST record deciding the wire isn't TLS at all is a
            // different (and more useful) signal than a later record
            // breaking an in-progress reassembly.
            let reason = if records_seen == 1 {
                RejectReason::NotTls
            } else {
                RejectReason::MalformedRecord
            };
            debug_assert!(
                matches!(reason, RejectReason::NotTls) == (records_seen == 1),
                "only the first non-handshake record may reject as NotTls"
            );
            return ParseOutcome::Reject(reason);
        }
        if header.length as usize > MAX_TLS_RECORD_LEN {
            return ParseOutcome::Reject(RejectReason::MalformedRecord);
        }

        let (after_content, content) = match take_record_content(after_header, header.length) {
            Ok(v) => v,
            Err(NomErr::Incomplete(_)) => return ParseOutcome::NeedMore,
            Err(_) => return ParseOutcome::Reject(RejectReason::MalformedRecord),
        };

        let consumed_so_far = buf.len() - after_content.len();
        // Post (pair): the cursor only ever advances, and never past the
        // buffer it was sliced from -- a stuck or overshooting cursor here
        // would either infinite-loop or panic on the next slice.
        debug_assert!(
            consumed_so_far <= buf.len(),
            "record walk cursor must never exceed the buffer length"
        );
        debug_assert!(
            consumed_so_far > pos,
            "each successfully parsed record must strictly advance the cursor"
        );
        pos = consumed_so_far;

        handshake = append_fragment(handshake, content);

        if !msg_type_checked && let Some(&msg_type) = handshake.first() {
            if msg_type != TLS_HANDSHAKE_TYPE_CLIENT_HELLO {
                return ParseOutcome::Reject(RejectReason::MalformedHandshake);
            }
            msg_type_checked = true;
        }

        if handshake.len() >= 4 {
            let hs_len = u32::from_be_bytes([0, handshake[1], handshake[2], handshake[3]]) as usize;
            if handshake.len() >= 4 + hs_len {
                let body = &handshake[4..4 + hs_len];
                return parse_client_hello_body(body);
            }
        }
        // Else: the declared handshake length reaches past what has been
        // reassembled so far -- read another record (a multi-record
        // ClientHello) rather than giving up.
    }
}

/// Decode a ClientHello body that is already known-complete: the caller
/// only reaches here once the handshake `length` field is fully satisfied
/// by the reassembled bytes, so every parser below uses `nom::*::complete`
/// -- any further shortfall is a lying length field, i.e.
/// `RejectReason::MalformedHandshake`, never `NeedMore`.
fn parse_client_hello_body(body: &[u8]) -> ParseOutcome {
    match parse_client_hello_fields(body) {
        Ok((sni, alpn, ech_present)) => ParseOutcome::ClientHello {
            sni,
            alpn,
            ech_present,
        },
        Err(reason) => ParseOutcome::Reject(reason),
    }
}

/// Maps any `nom` failure over the (known-complete) ClientHello body to
/// `MalformedHandshake` -- there is no `NeedMore` once framing proved the
/// body is fully present, so every nom error here is a genuine violation of
/// the length fields the body itself declared.
fn malformed(_: NomErr<nom::error::Error<&[u8]>>) -> RejectReason {
    RejectReason::MalformedHandshake
}

/// Raw (not-yet-normalized) fields extracted from a ClientHello relevant to
/// routing: wire-form `server_name`, client-offer-order ALPN list, and
/// whether `encrypted_client_hello` was present. Named (rather than an
/// inline tuple type) so `parse_client_hello_fields` and `parse_extensions`
/// share one shape.
type ClientHelloFields = (Option<String>, Vec<Vec<u8>>, bool);

/// Walk the fixed + length-prefixed ClientHello body fields (RFC 8446
/// §4.1.2) up to and including the extensions block, then hand the
/// extensions off to [`parse_extensions`].
fn parse_client_hello_fields(body: &[u8]) -> Result<ClientHelloFields, RejectReason> {
    use nom::{
        bytes::complete::take,
        number::complete::{be_u8, be_u16},
    };

    let in_len = body.len();
    let (i, _legacy_version) = be_u16(body).map_err(malformed)?;
    let (i, _random) = take(32usize)(i).map_err(malformed)?;
    let (i, session_id_len) = be_u8(i).map_err(malformed)?;
    let (i, _session_id) = take(session_id_len)(i).map_err(malformed)?;
    let (i, cipher_suites_len) = be_u16(i).map_err(malformed)?;
    let (i, _cipher_suites) = take(cipher_suites_len)(i).map_err(malformed)?;
    let (i, compression_len) = be_u8(i).map_err(malformed)?;
    let (i, _compression_methods) = take(compression_len)(i).map_err(malformed)?;
    // Post (pair): the fixed-shape prefix up to the extensions block has
    // been consumed, and consuming it can only ever shrink the remainder.
    debug_assert!(
        i.len() <= in_len,
        "parse_client_hello_fields must not grow its input"
    );
    debug_assert!(
        in_len - i.len() >= 2 + 32 + 1 + 1 + 2,
        "legacy_version (2) + random (32) + the session_id (1), cipher_suites (2) and compression_methods (1) length prefixes must be consumed"
    );

    // Extensions are the last block. RFC 8446 §4.1.2 makes the block itself
    // mandatory in TLS 1.3, but tolerate a ClientHello with nothing left --
    // it simply carries no SNI/ALPN, which the caller maps to `NoSni`.
    if i.is_empty() {
        return Ok((None, Vec::new(), false));
    }
    let (i, ext_total_len) = be_u16(i).map_err(malformed)?;
    let (_, ext_block) = take(ext_total_len)(i).map_err(malformed)?;

    parse_extensions(ext_block)
}

/// Walk the `(u16 type, u16 len, data)` extension list, extracting
/// `server_name` / `alpn` and noting `encrypted_client_hello`. Every OTHER
/// extension -- including every RFC 8701 GREASE value (`0x?A?A`, reserved
/// precisely so clients can probe unknown-extension tolerance) -- is
/// skipped by its declared length with no special-casing.
///
/// RFC 8446 §4.2 forbids more than one extension of a given type. A
/// well-formed client never sends a second `server_name` or `alpn`
/// extension, so accepting one silently is not "tolerance" -- it is a
/// parser-differential hazard: the original bytes are
/// forwarded untouched to the backend, so a tolerant backend resolving the
/// duplicate differently than Sōzu did would see a different SNI/ALPN than
/// the one that was routed on. `seen_sni` / `seen_alpn` therefore track
/// EXTENSION PRESENCE, independent of whether a value was extracted from
/// it (an extension can legitimately decode to `None` / empty), so a
/// second occurrence of either rejects outright instead of silently
/// keeping the first (`sni`) or overwriting with the last (`alpn`, the
/// pre-fix behavior). GREASE/unknown extension types are unaffected --
/// only these two routing-relevant types get duplicate detection.
fn parse_extensions(mut ext_block: &[u8]) -> Result<ClientHelloFields, RejectReason> {
    use nom::{bytes::complete::take, number::complete::be_u16};

    let mut sni: Option<String> = None;
    let mut alpn: Vec<Vec<u8>> = Vec::new();
    let mut ech_present = false;
    let mut seen_sni = false;
    let mut seen_alpn = false;

    while !ext_block.is_empty() {
        let in_len = ext_block.len();
        let (i, ext_type) = be_u16(ext_block).map_err(malformed)?;
        let (i, ext_len) = be_u16(i).map_err(malformed)?;
        let (i, data) = take(ext_len)(i).map_err(malformed)?;
        // Post: every extension record consumes exactly its declared
        // 4-byte header plus `ext_len` payload bytes -- never more, never
        // less, regardless of which arm below reads `data`.
        debug_assert_eq!(
            in_len - i.len(),
            4 + ext_len as usize,
            "an extension record consumes exactly 4 + declared_len bytes"
        );

        match ext_type {
            EXT_SERVER_NAME => {
                if seen_sni {
                    return Err(RejectReason::MalformedHandshake);
                }
                seen_sni = true;
                sni = parse_server_name_extension(data)?;
            }
            EXT_ALPN => {
                if seen_alpn {
                    return Err(RejectReason::MalformedHandshake);
                }
                seen_alpn = true;
                alpn = parse_alpn_extension(data)?;
            }
            EXT_ENCRYPTED_CLIENT_HELLO => ech_present = true,
            _ => {}
        }

        ext_block = i;
    }

    // Post (pair): `sni`/`alpn` can only carry an extracted value when the
    // corresponding extension was actually walked -- these are the
    // function's OWN bookkeeping flags (set exactly once per branch above,
    // gated by an early return before either flag is set), not raw wire
    // fields, so asserting their relationship to the extracted output is a
    // computed post-condition, not a trust-the-attacker precondition.
    debug_assert!(
        seen_sni || sni.is_none(),
        "sni can only be extracted when a server_name extension was seen"
    );
    debug_assert!(
        seen_alpn || alpn.is_empty(),
        "alpn can only be non-empty when an alpn extension was seen"
    );

    Ok((sni, alpn, ech_present))
}

/// Decode a `server_name` extension body (RFC 6066 §3):
/// `u16 server_name_list_len` then a list of `(u8 name_type, u16 name_len,
/// name)`. Returns the `host_name` (type `0`) entry. RFC 6066 §3: "The
/// ServerNameList MUST NOT contain more than one name of the same
/// name_type" -- a SECOND `host_name` entry is therefore rejected outright
/// rather than silently kept-first; entries of any
/// OTHER type are still skipped by length with no special-casing.
fn parse_server_name_extension(data: &[u8]) -> Result<Option<String>, RejectReason> {
    use nom::{
        bytes::complete::take,
        number::complete::{be_u8, be_u16},
    };

    // An empty extension body carries no name -- treat as "no usable SNI"
    // rather than a hard parse failure; the caller folds this into `NoSni`
    // / `EchOuterAbsent`.
    if data.is_empty() {
        return Ok(None);
    }

    // `list_len` is ATTACKER-CONTROLLED wire data -- never assert on it.
    // The `take` below enforces the bound as a graceful reject
    // (`MalformedHandshake`), exactly like the ALPN sibling. A former
    // `debug_assert!(list_len <= data.len())` here panicked every
    // debug-assertions build on a lying length (fuzz_tcp_clienthello
    // finding); asserts are for computed post-conditions only.
    let (i, list_len) = be_u16(data).map_err(malformed)?;
    let (_, mut entries) = take(list_len)(i).map_err(malformed)?;

    let mut host_name: Option<&[u8]> = None;
    let mut host_name_entries: u32 = 0;
    while !entries.is_empty() {
        let (i, name_type) = be_u8(entries).map_err(malformed)?;
        let (i, name_len) = be_u16(i).map_err(malformed)?;
        let (i, name) = take(name_len)(i).map_err(malformed)?;
        if name_type == 0 {
            host_name_entries += 1;
            if host_name_entries > 1 {
                return Err(RejectReason::MalformedHandshake);
            }
            host_name = Some(name);
        }
        entries = i;
    }
    // Post (pair): the server_name_list is drained exactly -- a well-formed
    // walk never exits with bytes left unaccounted for -- and at most one
    // `host_name` entry ever survives the walk: RFC 6066 forbids more than
    // one name of the same type, and a second sighting returns early
    // above, so completing the loop implies `host_name_entries <= 1`. Both
    // are the walk's OWN bookkeeping (a local counter and a cursor),
    // checked only after `take` has already bounded every field against
    // the declared lengths -- not a trust-the-attacker precondition.
    debug_assert!(
        entries.is_empty(),
        "server_name_list walk must fully drain the declared list"
    );
    debug_assert!(
        host_name_entries <= 1,
        "at most one host_name entry may survive the walk -- a second must reject early"
    );

    match host_name {
        Some(name) => match std::str::from_utf8(name) {
            Ok(s) => Ok(Some(s.to_owned())),
            Err(_) => Err(RejectReason::MalformedHandshake),
        },
        None => Ok(None),
    }
}

/// Decode an `application_layer_protocol_negotiation` extension body (RFC
/// 7301 §3.1): `u16 list_len` then a list of `(u8 len, name)` entries,
/// preserved in client offer order.
///
/// RFC 7301 §3.1 declares `opaque ProtocolName<1..2^8-1>` -- a protocol
/// name is NEVER zero-length -- and `ProtocolName
/// protocol_name_list<2..2^16-1>` -- the list always carries at least one
/// entry. Both are enforced here: a zero-length name
/// used to cost only 1 wire byte yet still allocate a `Vec<u8>` + `Vec`
/// slot, so a single ClientHello could inflate `protocols` into tens of
/// thousands of empty entries that `Output::Routed` then clones and the
/// routed info-log renders one by one. Rejecting `name_len == 0` restores
/// the natural bound: every surviving entry costs >= 2 wire bytes (the
/// 1-byte length prefix plus >= 1 name byte), so the entry count can no
/// longer approach the raw byte length of the list.
fn parse_alpn_extension(data: &[u8]) -> Result<Vec<Vec<u8>>, RejectReason> {
    use nom::{
        bytes::complete::take,
        number::complete::{be_u8, be_u16},
    };

    let (i, list_len) = be_u16(data).map_err(malformed)?;
    let (_, mut entries) = take(list_len)(i).map_err(malformed)?;

    let mut protocols = Vec::new();
    while !entries.is_empty() {
        let (i, name_len) = be_u8(entries).map_err(malformed)?;
        if name_len == 0 {
            return Err(RejectReason::MalformedHandshake);
        }
        let (i, name) = take(name_len)(i).map_err(malformed)?;
        protocols.push(name.to_vec());
        entries = i;
    }
    // An extension that IS present but whose list carries zero entries is
    // a malformed offer, not "no ALPN" -- the latter is only ever
    // represented by `alpn: Vec::new()` for a hello that omitted the
    // extension entirely (see `parse_extensions`'s default), so folding an
    // explicit empty list into that same shape would let a malformed
    // offer through as a legitimate "no preference" signal.
    if protocols.is_empty() {
        return Err(RejectReason::MalformedHandshake);
    }
    // Post (pair): every entry came out of the declared list bytes and now
    // costs >= 2 bytes (1-byte length prefix + >= 1 name byte, `name_len
    // == 0` having just been rejected above), so the entry count can never
    // exceed half the byte length of the list -- tighter than the pre-fix
    // bound of one entry per byte.
    debug_assert!(
        protocols.len() <= list_len as usize / 2,
        "post-fix, each ALPN entry costs >= 2 wire bytes, halving the pre-fix entry-count bound"
    );
    debug_assert!(
        entries.is_empty(),
        "ALPN protocol_name_list walk must fully drain the declared list"
    );

    Ok(protocols)
}

#[cfg(test)]
pub(super) fn encode_extension(ext_type: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&ext_type.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
pub(super) fn encode_sni_extension(host: &str) -> Vec<u8> {
    let mut name_list = vec![0u8]; // name_type = host_name
    name_list.extend_from_slice(&(host.len() as u16).to_be_bytes());
    name_list.extend_from_slice(host.as_bytes());
    let mut data = Vec::new();
    data.extend_from_slice(&(name_list.len() as u16).to_be_bytes());
    data.extend_from_slice(&name_list);
    encode_extension(EXT_SERVER_NAME, &data)
}

#[cfg(test)]
pub(super) fn encode_alpn_extension(protocols: &[&[u8]]) -> Vec<u8> {
    let mut list = Vec::new();
    for p in protocols {
        list.push(p.len() as u8);
        list.extend_from_slice(p);
    }
    let mut data = Vec::new();
    data.extend_from_slice(&(list.len() as u16).to_be_bytes());
    data.extend_from_slice(&list);
    encode_extension(EXT_ALPN, &data)
}

/// RFC 8701 GREASE extension value (`0x?A?A` pattern) with a small opaque
/// payload, used by tests to prove the extension walk skips unrecognised
/// (incl. GREASE) extensions purely by declared length.
#[cfg(test)]
pub(super) fn encode_grease_extension() -> Vec<u8> {
    encode_extension(0x0a0a, &[0x00])
}

#[cfg(test)]
pub(super) fn build_client_hello_body(extra_extensions: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]); // legacy_version: {3, 3}
    body.extend_from_slice(&[0u8; 32]); // random
    body.push(0); // session_id: empty
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites: TLS_AES_128_GCM_SHA256
    body.push(1); // compression_methods length = 1
    body.push(0); // compression_method: null

    let mut ext_block = Vec::new();
    for ext in extra_extensions {
        ext_block.extend_from_slice(ext);
    }
    body.extend_from_slice(&(ext_block.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext_block);
    body
}

#[cfg(test)]
pub(super) fn wrap_handshake(body: &[u8]) -> Vec<u8> {
    let mut hs = Vec::with_capacity(4 + body.len());
    hs.push(TLS_HANDSHAKE_TYPE_CLIENT_HELLO);
    let len = body.len() as u32;
    hs.extend_from_slice(&len.to_be_bytes()[1..4]);
    hs.extend_from_slice(body);
    hs
}

#[cfg(test)]
pub(super) fn wrap_record(content_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut rec = Vec::with_capacity(5 + payload.len());
    rec.push(content_type);
    rec.extend_from_slice(&[0x03, 0x03]); // legacy record version
    rec.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    rec.extend_from_slice(payload);
    rec
}

/// Build a single-record wire ClientHello: `wrap_record(22, wrap_handshake(build_client_hello_body(...)))`.
#[cfg(test)]
pub(super) fn build_client_hello_wire(extra_extensions: &[Vec<u8>]) -> Vec<u8> {
    wrap_record(
        TLS_CONTENT_TYPE_HANDSHAKE,
        &wrap_handshake(&build_client_hello_body(extra_extensions)),
    )
}

/// Split a wire ClientHello's SINGLE record back into N records that
/// together reassemble to the same handshake bytes -- exercising the
/// multi-record reassembly path. `wire` must be exactly one TLS record
/// (as produced by [`build_client_hello_wire`]); `chunk_count` must be
/// `>= 1` and `<=` the handshake payload length.
#[cfg(test)]
pub(super) fn split_into_records(wire: &[u8], chunk_count: usize) -> Vec<u8> {
    assert!(chunk_count >= 1, "test helper requires at least one chunk");
    let payload = &wire[5..];
    let chunk_len = payload.len().div_ceil(chunk_count);
    let mut out = Vec::new();
    for chunk in payload.chunks(chunk_len.max(1)) {
        out.extend_from_slice(&wrap_record(TLS_CONTENT_TYPE_HANDSHAKE, chunk));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- record-layer framing --------------------------------------------

    #[test]
    fn empty_input_needs_more() {
        assert!(matches!(parse_client_hello(&[]), ParseOutcome::NeedMore));
    }

    #[test]
    fn non_handshake_first_record_is_not_tls() {
        // ContentType 23 = application_data.
        let record = wrap_record(23, &[0u8; 4]);
        assert!(matches!(
            parse_client_hello(&record),
            ParseOutcome::Reject(RejectReason::NotTls)
        ));
    }

    #[test]
    fn oversized_record_length_is_malformed() {
        // Declare a record length larger than MAX_TLS_RECORD_LEN, with no
        // body -- the length check must fire before any `take`.
        let mut record = vec![TLS_CONTENT_TYPE_HANDSHAKE, 0x03, 0x03];
        record.extend_from_slice(&(MAX_TLS_RECORD_LEN as u16 + 1).to_be_bytes());
        assert!(matches!(
            parse_client_hello(&record),
            ParseOutcome::Reject(RejectReason::MalformedRecord)
        ));
    }

    #[test]
    fn wrong_handshake_msg_type_is_malformed_handshake() {
        // A well-formed handshake header (msg_type=2 == ServerHello) inside
        // an otherwise valid record.
        let mut hs = vec![2u8]; // ServerHello
        hs.extend_from_slice(&[0, 0, 1]); // length = 1
        hs.push(0);
        let record = wrap_record(TLS_CONTENT_TYPE_HANDSHAKE, &hs);
        assert!(matches!(
            parse_client_hello(&record),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    #[test]
    fn one_byte_drip_needs_more_until_complete() {
        let wire = build_client_hello_wire(&[encode_sni_extension("example.com")]);
        for i in 0..wire.len() {
            assert!(
                matches!(parse_client_hello(&wire[..i]), ParseOutcome::NeedMore),
                "prefix of length {i} (of {}) must NeedMore",
                wire.len()
            );
        }
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello { sni, .. } => {
                assert_eq!(sni.as_deref(), Some("example.com"));
            }
            _ => panic!("expected a complete ClientHello, got a different outcome"),
        }
    }

    #[test]
    fn byte_replay_buffer_is_untouched_by_parsing() {
        let wire = build_client_hello_wire(&[
            encode_sni_extension("example.com"),
            encode_alpn_extension(&[b"h2", b"http/1.1"]),
        ]);
        let before = wire.clone();
        let _ = parse_client_hello(&wire);
        assert_eq!(wire, before, "parsing must never mutate the input buffer");
    }

    // ---- multi-record reassembly ------------------------------------------

    #[test]
    fn multi_record_client_hello_reassembles() {
        let wire = build_client_hello_wire(&[
            encode_sni_extension("split.example.com"),
            encode_alpn_extension(&[b"h2"]),
        ]);
        for chunks in 1..=6usize {
            let split = split_into_records(&wire, chunks);
            match parse_client_hello(&split) {
                ParseOutcome::ClientHello { sni, alpn, .. } => {
                    assert_eq!(sni.as_deref(), Some("split.example.com"));
                    assert_eq!(alpn, vec![b"h2".to_vec()]);
                }
                _ => panic!("chunk count {chunks} must reassemble to a complete ClientHello"),
            }
        }
    }

    #[test]
    fn non_handshake_record_mid_reassembly_is_malformed_record() {
        let wire = build_client_hello_wire(&[encode_sni_extension("example.com")]);
        let split = split_into_records(&wire, 3);
        // Corrupt the SECOND record's content_type (offset found by
        // re-deriving the first record's length from the split bytes).
        let first_len = u16::from_be_bytes([split[3], split[4]]) as usize;
        let second_record_offset = 5 + first_len;
        let mut corrupted = split.clone();
        corrupted[second_record_offset] = 23; // application_data
        assert!(matches!(
            parse_client_hello(&corrupted),
            ParseOutcome::Reject(RejectReason::MalformedRecord)
        ));
    }

    // ---- extension walk / SNI + ALPN extraction ----------------------------

    #[test]
    fn grease_extensions_are_skipped_by_length() {
        // RFC 8701: GREASE values are reserved so clients/servers tolerate
        // unknown extensions; the walk must skip them purely by their
        // declared length, with zero special-casing.
        let wire = build_client_hello_wire(&[
            encode_grease_extension(),
            encode_sni_extension("grease.example.com"),
            encode_alpn_extension(&[b"h2"]),
            encode_grease_extension(),
        ]);
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello {
                sni,
                alpn,
                ech_present,
            } => {
                assert_eq!(sni.as_deref(), Some("grease.example.com"));
                assert_eq!(alpn, vec![b"h2".to_vec()]);
                assert!(!ech_present);
            }
            _ => panic!("GREASE-laden ClientHello must still parse"),
        }
    }

    #[test]
    fn alpn_preserves_client_offer_order() {
        let wire = build_client_hello_wire(&[encode_alpn_extension(&[b"http/1.1", b"h2", b"foo"])]);
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello { alpn, .. } => {
                assert_eq!(
                    alpn,
                    vec![b"http/1.1".to_vec(), b"h2".to_vec(), b"foo".to_vec()]
                );
            }
            _ => panic!("expected a complete ClientHello"),
        }
    }

    #[test]
    fn no_sni_extension_yields_none() {
        let wire = build_client_hello_wire(&[encode_alpn_extension(&[b"h2"])]);
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello { sni, .. } => assert_eq!(sni, None),
            _ => panic!("expected a complete ClientHello"),
        }
    }

    #[test]
    fn ech_extension_presence_is_flagged() {
        let wire = build_client_hello_wire(&[encode_extension(0xfe0d, &[0x00, 0x01, 0x02])]);
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello {
                sni, ech_present, ..
            } => {
                assert_eq!(sni, None);
                assert!(ech_present);
            }
            _ => panic!("expected a complete ClientHello"),
        }
    }

    #[test]
    fn no_extensions_block_yields_no_sni_no_alpn() {
        let wire = build_client_hello_wire(&[]);
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello {
                sni,
                alpn,
                ech_present,
            } => {
                assert_eq!(sni, None);
                assert!(alpn.is_empty());
                assert!(!ech_present);
            }
            _ => panic!("expected a complete ClientHello"),
        }
    }

    // ---- lying wire-declared lengths (fuzz_tcp_clienthello regression) -----

    /// A `server_name` extension whose inner `server_name_list` length
    /// declares more bytes than the extension body carries. That length is
    /// ATTACKER-CONTROLLED: it must map to `Reject(MalformedHandshake)`,
    /// never a panic. Regression for a `fuzz_tcp_clienthello` crash finding:
    /// a `debug_assert!` on this exact field, placed BEFORE the enforcing
    /// `take`, panicked every debug-assertions build (dev/test/e2e/sim/fuzz).
    #[test]
    fn sni_list_len_overflowing_extension_body_is_malformed_not_panic() {
        let mut data = Vec::new();
        data.extend_from_slice(&512u16.to_be_bytes()); // lying list_len
        data.push(0); // name_type = host_name, then nothing else
        let wire = build_client_hello_wire(&[encode_extension(EXT_SERVER_NAME, &data)]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    /// Same class, sibling field: an honest `server_name_list` length whose
    /// inner entry `name_len` overruns the list it sits in -- must also be
    /// the graceful reject, never a panic.
    #[test]
    fn sni_name_len_overflowing_list_is_malformed_not_panic() {
        let mut name_list = vec![0u8]; // name_type = host_name
        name_list.extend_from_slice(&512u16.to_be_bytes()); // lying name_len
        name_list.push(b'a'); // 1 byte of "name", far short of 512
        let mut data = Vec::new();
        data.extend_from_slice(&(name_list.len() as u16).to_be_bytes()); // honest list_len
        data.extend_from_slice(&name_list);
        let wire = build_client_hello_wire(&[encode_extension(EXT_SERVER_NAME, &data)]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    /// Same class, sibling field: an ALPN extension whose inner
    /// `protocol_name_list` length overflows the extension body.
    #[test]
    fn alpn_list_len_overflowing_extension_body_is_malformed_not_panic() {
        let mut data = Vec::new();
        data.extend_from_slice(&512u16.to_be_bytes()); // lying list_len
        data.push(2); // 1 byte of "list", far short of 512
        let wire = build_client_hello_wire(&[encode_extension(EXT_ALPN, &data)]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    /// Same class, sibling field: an extension record whose `ext_len`
    /// overflows the extensions block it sits in.
    #[test]
    fn extension_len_overflowing_block_is_malformed_not_panic() {
        // Hand-craft the extension record so its declared length lies
        // (the honest `encode_extension` helper cannot produce this):
        // type = server_name, ext_len = 512, but only 2 payload bytes.
        let mut lying_ext = Vec::new();
        lying_ext.extend_from_slice(&EXT_SERVER_NAME.to_be_bytes());
        lying_ext.extend_from_slice(&512u16.to_be_bytes()); // lying ext_len
        lying_ext.extend_from_slice(&[0x00, 0x00]);
        let wire = build_client_hello_wire(&[lying_ext]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    // ---- duplicate routing extensions ---------------------------------------
    //
    // A well-formed ClientHello carries at most one extension of a given
    // type (RFC 8446 §4.2). The extension walk used to keep the FIRST
    // usable `server_name` but OVERWRITE `alpn` with the LAST occurrence --
    // and since the original bytes are forwarded untouched to the backend,
    // a tolerant backend with a different duplicate-resolution policy could
    // see a different SNI/ALPN than the one Sōzu routed on. Both routing-
    // relevant extension types must now reject outright on a second sighting.

    #[test]
    fn duplicate_server_name_extension_is_rejected() {
        let wire = build_client_hello_wire(&[
            encode_sni_extension("first.example.com"),
            encode_sni_extension("second.example.com"),
        ]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    #[test]
    fn duplicate_alpn_extension_is_rejected() {
        let wire = build_client_hello_wire(&[
            encode_alpn_extension(&[b"h2"]),
            encode_alpn_extension(&[b"http/1.1"]),
        ]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    /// RFC 6066 §3: "the ServerNameList MUST NOT contain more than one name
    /// of the same name_type." A single `server_name` extension whose list
    /// carries two `host_name` (type 0) entries must reject, not silently
    /// keep the first as before.
    #[test]
    fn server_name_list_with_two_host_name_entries_is_rejected() {
        let mut name_list = Vec::new();
        name_list.push(0u8); // name_type = host_name
        name_list.extend_from_slice(&(b"first.example.com".len() as u16).to_be_bytes());
        name_list.extend_from_slice(b"first.example.com");
        name_list.push(0u8); // name_type = host_name, again
        name_list.extend_from_slice(&(b"second.example.com".len() as u16).to_be_bytes());
        name_list.extend_from_slice(b"second.example.com");

        let mut data = Vec::new();
        data.extend_from_slice(&(name_list.len() as u16).to_be_bytes());
        data.extend_from_slice(&name_list);

        let wire = build_client_hello_wire(&[encode_extension(EXT_SERVER_NAME, &data)]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    // ---- zero-length ALPN protocol names ------------------------------------
    //
    // RFC 7301 §3.1: `opaque ProtocolName<1..2^8-1>` -- a protocol name is
    // never zero-length, and `ProtocolName protocol_name_list<2..2^16-1>`
    // -- the list itself always carries at least one entry. Before this
    // fix, a zero-length name cost only 1 wire byte yet still allocated a
    // `Vec<u8>` + `Vec` slot, letting a single ClientHello inflate into an
    // outsized `Vec<Vec<u8>>` that `Output::Routed` then clones and the
    // routed info-log renders entry by entry.

    #[test]
    fn alpn_extension_with_zero_length_name_is_rejected() {
        let wire = build_client_hello_wire(&[encode_alpn_extension(&[b"h2", b"", b"http/1.1"])]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    #[test]
    fn alpn_extension_with_empty_protocol_list_is_rejected() {
        let wire = build_client_hello_wire(&[encode_alpn_extension(&[])]);
        assert!(matches!(
            parse_client_hello(&wire),
            ParseOutcome::Reject(RejectReason::MalformedHandshake)
        ));
    }

    // ---- positive control ---------------------------------------------------

    /// Same shape as `byte_replay_buffer_is_untouched_by_parsing` -- proves
    /// the duplicate-detection and zero-length-name rejections added above
    /// don't disturb a well-formed hello carrying exactly one SNI and one
    /// ALPN extension: same extracted values as before this change.
    #[test]
    fn single_sni_and_alpn_extension_still_parses_unchanged() {
        let wire = build_client_hello_wire(&[
            encode_sni_extension("example.com"),
            encode_alpn_extension(&[b"h2", b"http/1.1"]),
        ]);
        match parse_client_hello(&wire) {
            ParseOutcome::ClientHello {
                sni,
                alpn,
                ech_present,
            } => {
                assert_eq!(sni.as_deref(), Some("example.com"));
                assert_eq!(alpn, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
                assert!(!ech_present);
            }
            _ => panic!("expected a complete ClientHello"),
        }
    }
}
