//! Protected health-check header values and their keyed fingerprints.
//!
//! A health-check header value is a credential more often than not
//! (`Authorization`). It lives in memory only, in a [`SecretValue`](crate::secret::SecretValue) that is
//! neither `Clone`, `Serialize`, `Display` nor value-revealing `Debug`, and
//! whose buffer is zeroed when it drops. Anything that has to show or compare
//! a value shows the header name plus a
//! [`HeaderFingerprint`](crate::secret::HeaderFingerprint): a keyed HMAC-SHA256
//! under a [`FingerprintKey`](crate::secret::FingerprintKey) the process
//! generates for itself.
//!
//! Zeroing on drop covers the buffer this module owns. It does not reach what
//! the allocator, the kernel, a socket buffer or swap may already have copied.

use std::{
    collections::HashMap,
    fmt,
    io::{ErrorKind, Read},
    str::FromStr,
    sync::Arc,
};

use hmac::{Hmac, KeyInit, Mac};
use prost::{
    DecodeError, Message,
    bytes::{Buf, BufMut},
    encoding::{
        DecodeContext, WireType, bytes, check_wire_type, decode_key, decode_varint, encode_varint,
        message, skip_field, string,
    },
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use sha2::Sha256;
pub use zeroize::{Zeroize, Zeroizing};

use crate::{
    config::{
        check_health_check_header_value, validate_health_check_config,
        validate_health_check_header_values,
    },
    proto::command::{
        HealthCheckConfig, HealthCheckHeader, Request, SetProtectedHealthCheck,
        request::RequestType,
    },
};

type HmacSha256 = Hmac<Sha256>;

/// Algorithm label of a [`HeaderFingerprint`] and of its MAC input.
const FINGERPRINT_ALGORITHM: &str = "sozu-hc-hmac-sha256";
/// Format version of a [`HeaderFingerprint`] and of its MAC input.
const FINGERPRINT_VERSION: &str = "v1";
/// Bytes of the HMAC key.
const KEY_LEN: usize = 32;
/// Bytes of the public epoch that names a key without revealing it.
const EPOCH_LEN: usize = 16;
/// Bytes of an HMAC-SHA256 tag; the fingerprint carries all of them.
const TAG_LEN: usize = 32;

/// `serialize_with` of the generated ingress value: it is never serialized.
pub fn refuse_to_serialize<S: Serializer>(
    _value: &[u8],
    _serializer: S,
) -> Result<S::Ok, S::Error> {
    Err(S::Error::custom(
        "a protected health check header value is never serialized",
    ))
}

/// `deserialize_with` of the generated ingress value: it is never read back
/// from a serialized form.
pub fn refuse_to_deserialize<'de, D: Deserializer<'de>>(
    _deserializer: D,
) -> Result<Vec<u8>, D::Error> {
    Err(D::Error::custom(
        "a protected health check header value is never deserialized",
    ))
}

/// Protected ingress/IPC carrier of one health-check header value: the clear
/// bytes travel in `SetProtectedHealthCheck` from the operator to the main
/// process and are moved out into a [`SecretValue`] at once. It is the
/// `HealthCheckHeaderValue` message of `command.proto` (`name = 1`,
/// `value = 2`, both required) with a hand-written [`Message`] impl, mapped to
/// the generated path by `command/build.rs`.
///
/// prost's generated decode replaces a populated `bytes` field with `clear`,
/// `reserve`, `put`, so a reallocation frees the old buffer before any `Drop`
/// runs. This impl wipes the value before it is replaced, cleared, or kept
/// after a failed decode, and reads the new value straight from the input
/// buffer into the field, with no intermediate copy.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct HealthCheckHeaderValue {
    pub name: String,
    /// Never serialized, never read back: serde refuses it in both directions
    /// instead of rendering it, so it can never be saved or printed as JSON.
    #[serde(
        serialize_with = "refuse_to_serialize",
        deserialize_with = "refuse_to_deserialize"
    )]
    pub value: Vec<u8>,
}

impl HealthCheckHeaderValue {
    /// Zero the whole allocation behind the value, spare capacity included,
    /// and empty it.
    pub(crate) fn wipe(&mut self) {
        self.value.zeroize();
    }

    /// Replace the value with the length-delimited bytes at `buf`. The old
    /// value is wiped before the buffer can grow, so a reallocation frees only
    /// zeroes, and nothing is accepted before the wire type and the length
    /// are known to be valid.
    fn merge_value(&mut self, wire_type: WireType, buf: &mut impl Buf) -> Result<(), DecodeError> {
        check_wire_type(WireType::LengthDelimited, wire_type)?;
        let len = decode_varint(buf)?;
        if len > buf.remaining() as u64 {
            return Err(underflow(len));
        }
        let len = len as usize;
        self.wipe();
        self.value.reserve(len);
        self.value.resize(len, 0);
        buf.copy_to_slice(&mut self.value);
        Ok(())
    }

    /// The provided `Message::merge` loop: every field until `buf` is spent.
    fn merge_fields(&mut self, buf: &mut impl Buf) -> Result<(), DecodeError> {
        let ctx = DecodeContext::default();
        while buf.has_remaining() {
            let (tag, wire_type) = decode_key(buf)?;
            self.merge_field(tag, wire_type, buf, ctx.clone())?;
        }
        Ok(())
    }

    /// Wipe the value when `result` is an error: a refused fragment leaves no
    /// accepted secret behind in the carrier.
    fn wipe_on_error<T>(&mut self, result: Result<T, DecodeError>) -> Result<T, DecodeError> {
        if result.is_err() {
            self.wipe();
        }
        result
    }
}

/// The error prost raises when a length delimiter exceeds the input, built by
/// prost itself (its error kinds are private) from a value-less buffer that
/// declares `len` bytes. Nothing is copied or accepted on the way.
fn underflow(len: u64) -> DecodeError {
    let mut declared = Vec::new();
    encode_varint(len, &mut declared);
    match bytes::merge(
        WireType::LengthDelimited,
        &mut Vec::<u8>::new(),
        &mut declared.as_slice(),
        DecodeContext::default(),
    ) {
        Err(error) => error,
        Ok(()) => unreachable!("a length delimiter without its payload always underflows"),
    }
}

impl Message for HealthCheckHeaderValue {
    fn encode_raw(&self, buf: &mut impl BufMut) {
        string::encode(1, &self.name, buf);
        bytes::encode(2, &self.value, buf);
    }

    fn merge_field(
        &mut self,
        tag: u32,
        wire_type: WireType,
        buf: &mut impl Buf,
        ctx: DecodeContext,
    ) -> Result<(), DecodeError> {
        const STRUCT_NAME: &str = "HealthCheckHeaderValue";
        let result = match tag {
            1 => string::merge(wire_type, &mut self.name, buf, ctx).map_err(|mut error| {
                error.push(STRUCT_NAME, "name");
                error
            }),
            2 => self.merge_value(wire_type, buf).map_err(|mut error| {
                error.push(STRUCT_NAME, "value");
                error
            }),
            _ => skip_field(wire_type, tag, buf, ctx),
        };
        self.wipe_on_error(result)
    }

    fn encoded_len(&self) -> usize {
        string::encoded_len(1, &self.name) + bytes::encoded_len(2, &self.value)
    }

    fn clear(&mut self) {
        self.name.clear();
        self.wipe();
    }

    // The provided `merge` and `merge_length_delimited` can fail between two
    // fields (a bad key, a delimiter overrun), outside `merge_field`: these
    // overrides wipe on those paths too.
    fn merge(&mut self, mut buf: impl Buf) -> Result<(), DecodeError> {
        let result = self.merge_fields(&mut buf);
        self.wipe_on_error(result)
    }

    fn merge_length_delimited(&mut self, mut buf: impl Buf) -> Result<(), DecodeError> {
        let result = message::merge(
            WireType::LengthDelimited,
            self,
            &mut buf,
            DecodeContext::default(),
        );
        self.wipe_on_error(result)
    }
}

/// A carrier owns clear bytes: whichever way it ends, by a refused order, a
/// finished encode or a plain drop, its buffer is zeroed first. `Clone` is
/// derived, and every clone is a carrier with this same `Drop`.
impl Drop for HealthCheckHeaderValue {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// Why an ingress value was refused. Static on purpose: neither the value nor
/// anything derived from it, its length included, appears in a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SecretValueError {
    #[error("health check header value must be valid UTF-8")]
    NotUtf8,
    /// A per-value rule of `crate::config::check_health_check_header_value`.
    #[error("{0}")]
    Invalid(&'static str),
}

/// A health-check header value held in memory only.
///
/// There is no `Clone`, `Serialize`, `Deserialize` or `Display`, and `Debug`
/// prints a placeholder. The single way to read the bytes is
/// [`SecretValue::expose_secret_bytes`]; call it only for the fingerprint HMAC
/// and for the controlled in-memory IPC and wire builders.
pub struct SecretValue(Zeroizing<Vec<u8>>);

impl SecretValue {
    /// Move the value out of a generated ingress message.
    ///
    /// The allocation moves with [`std::mem::take`], so no copy is made and
    /// the generated field is left empty whether or not the value is accepted.
    /// The value must be UTF-8 and meet the per-value rules every health-check
    /// header meets (no forbidden control byte, no leading or trailing SP or
    /// HTAB); the name is the caller's to validate.
    pub fn take_from_ingress(
        message: &mut HealthCheckHeaderValue,
    ) -> Result<Self, SecretValueError> {
        // Owned by a zeroizing wrapper before any check, so a refused value is
        // wiped when the error path drops it.
        let bytes = Zeroizing::new(std::mem::take(&mut message.value));
        let text = std::str::from_utf8(&bytes).map_err(|_| SecretValueError::NotUtf8)?;
        check_health_check_header_value(text).map_err(SecretValueError::Invalid)?;
        Ok(Self(bytes))
    }

    /// The clear bytes. The name is the warning: the only legitimate callers
    /// are the fingerprint HMAC and the controlled in-memory IPC and wire
    /// builders that carry the value onward.
    pub fn expose_secret_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretValue")
            .field("value", &format_args!("<protected>"))
            .finish()
    }
}

/// Why a header value could not be read from its source. Static: neither the
/// bytes read nor any length of them appears in a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderValueReadError {
    #[error("could not read the health check header value: {0}")]
    Io(ErrorKind),
    #[error("health check header value is longer than the allowed maximum")]
    TooLong,
}

/// Read one health-check header value from `source` (a file or stdin) into a
/// protected ingress carrier.
///
/// At most `max_len + 3` bytes are read: the value, a possible terminal CRLF
/// and one byte that proves an overflow. The bytes are kept exactly, except one
/// terminal LF and the CR right before it, which a text editor or `echo` adds.
/// Nothing else is trimmed, and an empty value is a value. The value is
/// validated later, where the carrier is turned into a [`SecretValue`].
///
/// The read buffer is allocated once at its final size, so no earlier,
/// unwiped copy is left behind by a reallocation, and it is zeroed on every
/// return. Neither the bytes nor any length of them appears in an error.
pub fn read_header_value(
    name: String,
    mut source: impl Read,
    max_len: usize,
) -> Result<HealthCheckHeaderValue, HeaderValueReadError> {
    let capacity = max_len
        .checked_add(3)
        .ok_or(HeaderValueReadError::TooLong)?;
    let mut buffer = Zeroizing::new(vec![0u8; capacity]);
    let mut filled = 0;
    while filled < capacity {
        match source.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(HeaderValueReadError::Io(error.kind())),
        }
    }
    let mut end = filled;
    if end > 0 && buffer[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && buffer[end - 1] == b'\r' {
            end -= 1;
        }
    }
    if end > max_len {
        return Err(HeaderValueReadError::TooLong);
    }
    Ok(HealthCheckHeaderValue {
        name,
        value: buffer[..end].to_vec(),
    })
}

/// The HMAC-SHA256 of the concatenation of `parts`, as a still-open MAC.
fn hmac_sha256_mac(key: &[u8], parts: &[&[u8]]) -> HmacSha256 {
    // HMAC takes a key of any length: this cannot fail.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    for part in parts {
        mac.update(part);
    }
    mac
}

/// The process-local secret that keys [`HeaderFingerprint`]s, with the public
/// epoch that names it.
///
/// The key is random, lives in memory only and is zeroed on drop. There is no
/// `Clone`, no serde and `Debug` prints the epoch only. The epoch is not a
/// proof of anything: two keys may share one, and a fingerprint only verifies
/// when the key itself reproduces its tag.
pub struct FingerprintKey {
    key: Zeroizing<[u8; KEY_LEN]>,
    epoch: [u8; EPOCH_LEN],
}

impl FingerprintKey {
    /// A fresh key and an independently drawn epoch from the OS generator.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(key.as_mut_slice())?;
        let mut epoch = [0u8; EPOCH_LEN];
        getrandom::fill(&mut epoch)?;
        Ok(Self { key, epoch })
    }

    /// Rebuild a key that has to survive a hot upgrade, or a known test key.
    pub fn from_parts(key: Zeroizing<[u8; KEY_LEN]>, epoch: [u8; EPOCH_LEN]) -> Self {
        Self { key, epoch }
    }

    /// The MAC of one header under this key. The input is unambiguous: every
    /// variable-length field carries its 8-byte big-endian length, and the
    /// algorithm, version and epoch are part of it.
    fn mac(&self, name: &str, value: &SecretValue) -> HmacSha256 {
        let domain = format!("{FINGERPRINT_ALGORITHM}:{FINGERPRINT_VERSION}");
        let name = name.to_ascii_lowercase();
        let value = value.expose_secret_bytes();
        let domain_len = (domain.len() as u64).to_be_bytes();
        let name_len = (name.len() as u64).to_be_bytes();
        let value_len = (value.len() as u64).to_be_bytes();
        hmac_sha256_mac(
            self.key.as_slice(),
            &[
                &domain_len,
                domain.as_bytes(),
                &self.epoch,
                &name_len,
                name.as_bytes(),
                &value_len,
                value,
            ],
        )
    }

    /// The fingerprint of `value` as the header `name` (case-insensitive).
    pub fn fingerprint(&self, name: &str, value: &SecretValue) -> HeaderFingerprint {
        HeaderFingerprint {
            epoch: self.epoch,
            tag: self.mac(name, value).finalize().into_bytes().into(),
        }
    }

    /// Whether `fingerprint` was issued by this very key for this header and
    /// value. A matching epoch is necessary, never sufficient: the full tag is
    /// checked in constant time.
    pub fn verify(&self, name: &str, value: &SecretValue, fingerprint: &HeaderFingerprint) -> bool {
        fingerprint.epoch == self.epoch
            && self.mac(name, value).verify_slice(&fingerprint.tag).is_ok()
    }
}

impl fmt::Debug for FingerprintKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FingerprintKey")
            .field("epoch", &hex::encode(self.epoch))
            .field("key", &format_args!("<protected>"))
            .finish()
    }
}

/// Why a fingerprint string was refused. Static: it never echoes the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintParseError {
    #[error("fingerprint must look like 'algorithm:version:epoch:tag'")]
    Malformed,
    #[error("unknown fingerprint algorithm")]
    Algorithm,
    #[error("unsupported fingerprint version")]
    Version,
    #[error("fingerprint epoch must be 32 lowercase hex digits")]
    Epoch,
    #[error("fingerprint tag must be 64 lowercase hex digits")]
    Tag,
}

/// A keyed fingerprint of a header value:
/// `sozu-hc-hmac-sha256:v1:<epoch-32-hex>:<full-tag-64-hex>`. The tag is the
/// whole 256-bit HMAC, never truncated. The same name and value under one
/// epoch give the same fingerprint, so equality is visible; without the key it
/// reveals no plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HeaderFingerprint {
    epoch: [u8; EPOCH_LEN],
    tag: [u8; TAG_LEN],
}

impl fmt::Display for HeaderFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{FINGERPRINT_ALGORITHM}:{FINGERPRINT_VERSION}:{}:{}",
            hex::encode(self.epoch),
            hex::encode(self.tag)
        )
    }
}

/// Exactly `N` bytes from exactly `2 * N` lowercase hex digits.
fn parse_lowercase_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(text, &mut out).ok()?;
    Some(out)
}

impl FromStr for HeaderFingerprint {
    type Err = FingerprintParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = text.split(':');
        let (Some(algorithm), Some(version), Some(epoch), Some(tag), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(FingerprintParseError::Malformed);
        };
        if algorithm != FINGERPRINT_ALGORITHM {
            return Err(FingerprintParseError::Algorithm);
        }
        if version != FINGERPRINT_VERSION {
            return Err(FingerprintParseError::Version);
        }
        Ok(Self {
            epoch: parse_lowercase_hex(epoch).ok_or(FingerprintParseError::Epoch)?,
            tag: parse_lowercase_hex(tag).ok_or(FingerprintParseError::Tag)?,
        })
    }
}

/// Longest header name [`ProtectedHeader`]'s `Debug` prints.
const DEBUG_NAME_LEN: usize = 64;

/// One protected header: its public name, its public keyed fingerprint and a
/// shared handle on the value. Cloning clones the `Arc`, never the bytes.
#[derive(Clone)]
pub struct ProtectedHeader {
    name: String,
    fingerprint: HeaderFingerprint,
    value: Arc<SecretValue>,
}

impl ProtectedHeader {
    /// Fingerprint `value` under `key` as header `name`, which the caller has
    /// already validated.
    pub fn new(key: &FingerprintKey, name: String, value: SecretValue) -> Self {
        let fingerprint = key.fingerprint(&name, &value);
        Self {
            name,
            fingerprint,
            value: Arc::new(value),
        }
    }

    /// A header whose fingerprint another process issued and this one trusts:
    /// the worker side of an order the main process already fingerprinted.
    pub fn from_trusted(name: String, fingerprint: HeaderFingerprint, value: SecretValue) -> Self {
        Self {
            name,
            fingerprint,
            value: Arc::new(value),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fingerprint(&self) -> &HeaderFingerprint {
        &self.fingerprint
    }

    pub fn value(&self) -> &Arc<SecretValue> {
        &self.value
    }
}

impl fmt::Debug for ProtectedHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name: String = self.name.chars().take(DEBUG_NAME_LEN).collect();
        f.debug_struct("ProtectedHeader")
            .field("name_truncated", &(name.len() < self.name.len()))
            .field("name", &name)
            .field("fingerprint", &self.fingerprint.to_string())
            .field("value", &format_args!("<protected>"))
            .finish()
    }
}

/// An immutable, ordered set of [`ProtectedHeader`]s behind one `Arc`. Cloning
/// clones that handle; the headers and their values are shared.
#[derive(Clone)]
pub struct ProtectedHeaderSet(Arc<[ProtectedHeader]>);

impl ProtectedHeaderSet {
    /// The headers, in the order given.
    pub fn new(headers: Vec<ProtectedHeader>) -> Self {
        Self(Arc::from(headers))
    }

    pub fn headers(&self) -> &[ProtectedHeader] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether this set holds the value behind every header of `metadata`, in
    /// order: same count, same names, same fingerprints. A configuration that
    /// names headers is only as good as the set that satisfies it.
    pub fn satisfies(&self, metadata: &[HealthCheckHeader]) -> bool {
        self.0.len() == metadata.len()
            && self.0.iter().zip(metadata).all(|(held, meta)| {
                held.name == meta.name && held.fingerprint.to_string() == meta.fingerprint
            })
    }

    /// Whether both handles share one allocation.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for ProtectedHeaderSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedHeaderSet")
            .field("headers", &self.0.len())
            .finish()
    }
}

/// Why a protected health-check order was refused. Static on purpose: neither
/// a value, nor any length of one, appears in a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtectedHealthCheckError {
    /// A rule of `validate_health_check_config` or
    /// `validate_health_check_header_values`.
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Value(#[from] SecretValueError),
    #[error("health check header fingerprint does not match its value")]
    FingerprintMismatch,
    #[error("health check header fingerprint is missing")]
    FingerprintMissing,
}

/// A validated protected health-check order: the cluster, its configuration
/// with a trusted fingerprint on every header, and the held values.
pub struct ProtectedHealthCheck {
    cluster_id: String,
    config: HealthCheckConfig,
    headers: ProtectedHeaderSet,
}

impl ProtectedHealthCheck {
    /// An operator order: validate every name and value, then fingerprint each
    /// value under `key`. A fingerprint the client supplied is only accepted
    /// when it is the one `key` computes. Nothing is built unless all of it is
    /// valid; the carriers are emptied of the values that are accepted.
    pub fn from_ingress(
        key: &FingerprintKey,
        order: &mut SetProtectedHealthCheck,
    ) -> Result<Self, ProtectedHealthCheckError> {
        let held = Self::held_values(order)?;
        let mut config = order.config.to_owned();
        let mut headers = Vec::with_capacity(held.len());
        for (meta, (name, value)) in config.headers.iter_mut().zip(held) {
            let header = ProtectedHeader::new(key, name, value);
            if !meta.fingerprint.is_empty() {
                let claimed: HeaderFingerprint = meta.fingerprint.parse().map_err(|_| {
                    ProtectedHealthCheckError::Invalid(
                        "health check header fingerprint is malformed",
                    )
                })?;
                if &claimed != header.fingerprint() {
                    return Err(ProtectedHealthCheckError::FingerprintMismatch);
                }
            }
            meta.fingerprint = header.fingerprint().to_string();
            headers.push(header);
        }
        Ok(Self {
            cluster_id: order.cluster_id.to_owned(),
            config,
            headers: ProtectedHeaderSet::new(headers),
        })
    }

    /// An order the main process built: every header already carries the
    /// fingerprint it issued, which is trusted as it is.
    pub fn from_trusted(
        order: &mut SetProtectedHealthCheck,
    ) -> Result<Self, ProtectedHealthCheckError> {
        let held = Self::held_values(order)?;
        let mut headers = Vec::with_capacity(held.len());
        for (meta, (name, value)) in order.config.headers.iter().zip(held) {
            if meta.fingerprint.is_empty() {
                return Err(ProtectedHealthCheckError::FingerprintMissing);
            }
            let fingerprint = meta.fingerprint.parse().map_err(|_| {
                ProtectedHealthCheckError::Invalid("health check header fingerprint is malformed")
            })?;
            headers.push(ProtectedHeader::from_trusted(name, fingerprint, value));
        }
        Ok(Self {
            cluster_id: order.cluster_id.to_owned(),
            config: order.config.to_owned(),
            headers: ProtectedHeaderSet::new(headers),
        })
    }

    /// Validate `order` as a whole, then move every value out of its carriers.
    fn held_values(
        order: &mut SetProtectedHealthCheck,
    ) -> Result<Vec<(String, SecretValue)>, ProtectedHealthCheckError> {
        validate_health_check_config(&order.config).map_err(ProtectedHealthCheckError::Invalid)?;
        validate_health_check_header_values(&order.config, &order.values)
            .map_err(ProtectedHealthCheckError::Invalid)?;
        order
            .values
            .iter_mut()
            .map(|carrier| {
                let value = SecretValue::take_from_ingress(carrier)?;
                Ok((carrier.name.to_owned(), value))
            })
            .collect()
    }

    /// The order of a cluster from its parts: the main process's own
    /// configuration, every header of which carries the fingerprint it issued,
    /// and the held values behind them. The set must satisfy the headers.
    pub fn from_parts(
        cluster_id: String,
        config: HealthCheckConfig,
        headers: ProtectedHeaderSet,
    ) -> Result<Self, ProtectedHealthCheckError> {
        if !headers.satisfies(&config.headers) {
            return Err(ProtectedHealthCheckError::Invalid(
                "health check header values do not match the header metadata",
            ));
        }
        Ok(Self {
            cluster_id,
            config,
            headers,
        })
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    /// The configuration, headers carrying names and fingerprints only.
    pub fn config(&self) -> &HealthCheckConfig {
        &self.config
    }

    pub fn headers(&self) -> &ProtectedHeaderSet {
        &self.headers
    }

    /// Whether the held values are the ones `metadata` names.
    pub fn satisfies(&self, metadata: &[HealthCheckHeader]) -> bool {
        self.headers.satisfies(metadata)
    }

    /// The order a worker receives: trusted fingerprints in place and one
    /// freshly copied carrier per value. Each carrier wipes itself on drop.
    pub fn to_order(&self) -> SetProtectedHealthCheck {
        SetProtectedHealthCheck {
            cluster_id: self.cluster_id.to_owned(),
            config: self.config.to_owned(),
            values: self
                .headers
                .headers()
                .iter()
                .map(|header| HealthCheckHeaderValue {
                    name: header.name().to_owned(),
                    value: header.value().expose_secret_bytes().to_vec(),
                })
                .collect(),
        }
    }

    /// The same order as a request, for the caller to encode and drop.
    pub fn to_request(&self) -> Request {
        RequestType::SetProtectedHealthCheck(self.to_order()).into()
    }
}

impl fmt::Debug for ProtectedHealthCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedHealthCheck")
            .field("cluster_id", &self.cluster_id)
            .field("headers", &self.config.headers)
            .finish()
    }
}

/// Opaque identity of one proposal. It carries no header, so a task may
/// persist it next to a cluster id; the set it names lives in memory only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProtectedHeaderVersion(u64);

impl ProtectedHeaderVersion {
    /// The version as it travels in a protected health check order.
    pub fn get(self) -> u64 {
        self.0
    }

    /// The version a peer named in an order or an answer. It is only ever
    /// compared with the versions the peer was given: it grants nothing.
    pub fn from_wire(version: u64) -> Self {
        Self(version)
    }
}

/// The confirmed clusters a fresh worker is given and the version it stages
/// them under: names and a number, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedBootstrap {
    version: ProtectedHeaderVersion,
    clusters: Vec<String>,
}

impl ConfirmedBootstrap {
    pub fn version(&self) -> ProtectedHeaderVersion {
        self.version
    }

    /// The clusters holding a confirmed value, in lexical order.
    pub fn clusters(&self) -> &[String] {
        &self.clusters
    }
}

/// A full replacement of the protected headers of one cluster.
pub enum ProtectedHeaderChange {
    /// These headers, in this order.
    Replace(ProtectedHeaderSet),
    /// The cluster stays, with an empty header set.
    Clear,
    /// The cluster is absent.
    Remove,
}

impl ProtectedHeaderChange {
    /// `None` is an absent cluster, an empty set a cleared one.
    fn into_state(self) -> Option<ProtectedHeaderSet> {
        match self {
            Self::Replace(set) => Some(set),
            Self::Clear => Some(ProtectedHeaderSet::new(Vec::new())),
            Self::Remove => None,
        }
    }
}

/// Every [`ProtectedHeaderVersion`] has been issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("protected health check header versions are exhausted")]
pub struct VersionExhausted;

/// What a resolution did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Applied,
    /// The version is unknown, already resolved or superseded: nothing changed.
    Ignored,
}

/// An unresolved proposal; `state` is `None` for a removal.
struct PendingVersion {
    version: ProtectedHeaderVersion,
    state: Option<ProtectedHeaderSet>,
}

#[derive(Default)]
struct ClusterJournal {
    /// What every targeted worker acknowledged; `None` is an absent cluster.
    confirmed: Option<ProtectedHeaderSet>,
    /// Unresolved proposals, oldest first. The last one is optimistically
    /// effective.
    pending: Vec<PendingVersion>,
}

/// The in-memory protected headers of every cluster, and the unresolved
/// proposals that may still replace them.
///
/// A proposal is effective at once. A rejection drops it, so the newest
/// remaining proposal, else the confirmed state, is effective: an ancestor's
/// rejection leaves a live descendant alone. An acceptance confirms the
/// proposal and supersedes every older one, which can no longer change the
/// outcome. Resolved proposals are forgotten; versions are never reused, so a
/// late or repeated resolution finds nothing and changes nothing.
///
/// There is no serde, no `Clone` and `Debug` prints counts only: the journal
/// never reaches a state file, a task snapshot or a log.
#[derive(Default)]
pub struct ProtectedHeaderJournal {
    last_issued: u64,
    clusters: HashMap<String, ClusterJournal>,
}

impl ProtectedHeaderJournal {
    /// Start from a journal that already issued `last_issued` versions.
    #[cfg(test)]
    fn with_last_issued(last_issued: u64) -> Self {
        Self {
            last_issued,
            clusters: HashMap::new(),
        }
    }

    /// Record `change` as the effective state of `cluster` until it is
    /// resolved. Nothing is touched when the versions are exhausted.
    pub fn propose(
        &mut self,
        cluster: &str,
        change: ProtectedHeaderChange,
    ) -> Result<ProtectedHeaderVersion, VersionExhausted> {
        let next = self.last_issued.checked_add(1).ok_or(VersionExhausted)?;
        let version = ProtectedHeaderVersion(next);
        self.last_issued = next;
        self.clusters
            .entry(cluster.to_owned())
            .or_default()
            .pending
            .push(PendingVersion {
                version,
                state: change.into_state(),
            });
        Ok(version)
    }

    /// Whether the journal retains a non-empty header set anywhere: confirmed,
    /// or in any unresolved proposal, for any cluster. A later `Clear` or
    /// `Remove` hides the headers beneath it without dropping them, so this
    /// looks at every retained state and not only [`Self::effective`].
    pub fn holds_values(&self) -> bool {
        self.clusters.values().any(|journal| {
            journal
                .confirmed
                .iter()
                .chain(journal.pending.iter().filter_map(|p| p.state.as_ref()))
                .any(|set| !set.is_empty())
        })
    }

    /// What a fresh worker must be given to hold the confirmed values: the
    /// clusters whose confirmed set is not empty, in lexical order, and the
    /// version they are all staged under. `None` when no value is held.
    ///
    /// Read-only: it proposes, accepts and rejects nothing, and returns no
    /// byte of a value. A fresh worker store is empty, so the last version the
    /// journal issued is acceptable for every cluster: it is at least `1` as
    /// soon as a value was ever held, and no cluster was confirmed under a
    /// newer one. Refused while any proposal is unresolved, and when a
    /// confirmed set is not the one `metadata` names.
    pub fn confirmed_bootstrap<'m>(
        &self,
        metadata: impl Fn(&str) -> Option<&'m [HealthCheckHeader]>,
    ) -> Result<Option<ConfirmedBootstrap>, TransferError> {
        if self
            .clusters
            .values()
            .any(|cluster| !cluster.pending.is_empty())
        {
            return Err(TransferError::Pending);
        }
        let mut clusters = Vec::new();
        for (id, cluster) in &self.clusters {
            let Some(set) = &cluster.confirmed else {
                continue;
            };
            verify_set(set, metadata(id))?;
            if !set.is_empty() {
                clusters.push(id.to_owned());
            }
        }
        if clusters.is_empty() {
            return Ok(None);
        }
        if self.last_issued == 0 {
            return Err(TransferError::Malformed);
        }
        clusters.sort();
        Ok(Some(ConfirmedBootstrap {
            version: ProtectedHeaderVersion(self.last_issued),
            clusters,
        }))
    }

    /// The effective protected headers of `cluster`: `None` when it is
    /// absent, an empty set when it was cleared.
    pub fn effective(&self, cluster: &str) -> Option<&ProtectedHeaderSet> {
        let journal = self.clusters.get(cluster)?;
        journal
            .pending
            .last()
            .map_or(journal.confirmed.as_ref(), |pending| pending.state.as_ref())
    }

    /// The state `version` proposed for `cluster` while it is unresolved:
    /// `None` when the version is unknown or resolved, `Some(None)` for a
    /// removal.
    pub fn proposed(
        &self,
        cluster: &str,
        version: ProtectedHeaderVersion,
    ) -> Option<Option<&ProtectedHeaderSet>> {
        self.clusters
            .get(cluster)?
            .pending
            .iter()
            .find(|pending| pending.version == version)
            .map(|pending| pending.state.as_ref())
    }

    /// Every targeted worker acknowledged `version`. It becomes the confirmed
    /// state and supersedes the proposals older than it.
    pub fn accept(&mut self, cluster: &str, version: ProtectedHeaderVersion) -> Resolution {
        let Some(journal) = self.clusters.get_mut(cluster) else {
            return Resolution::Ignored;
        };
        let Some(index) = journal.pending.iter().position(|p| p.version == version) else {
            return Resolution::Ignored;
        };
        if let Some(accepted) = journal.pending.drain(..=index).next_back() {
            journal.confirmed = accepted.state;
        }
        self.forget_if_absent(cluster);
        Resolution::Applied
    }

    /// `version` failed: drop it and fall back to what is left.
    pub fn reject(&mut self, cluster: &str, version: ProtectedHeaderVersion) -> Resolution {
        let Some(journal) = self.clusters.get_mut(cluster) else {
            return Resolution::Ignored;
        };
        let Some(index) = journal.pending.iter().position(|p| p.version == version) else {
            return Resolution::Ignored;
        };
        journal.pending.remove(index);
        self.forget_if_absent(cluster);
        Resolution::Applied
    }

    /// An absent cluster with nothing pending has no entry: a removal leaves
    /// no tombstone, since no resolved version is ever matched again.
    fn forget_if_absent(&mut self, cluster: &str) {
        if self
            .clusters
            .get(cluster)
            .is_some_and(|j| j.confirmed.is_none() && j.pending.is_empty())
        {
            self.clusters.remove(cluster);
        }
    }
}

impl fmt::Debug for ProtectedHeaderJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedHeaderJournal")
            .field("last_issued", &self.last_issued)
            .field("clusters", &self.clusters.len())
            .field(
                "pending",
                &self
                    .clusters
                    .values()
                    .map(|j| j.pending.len())
                    .sum::<usize>(),
            )
            .finish()
    }
}

/// Largest encoded [`ProtectedTransfer`] a main upgrade may carry.
pub const PROTECTED_TRANSFER_MAX_BYTES: usize = 64 << 20;
/// Layout version of an encoded [`ProtectedTransfer`].
const TRANSFER_LAYOUT_VERSION: u8 = 1;
/// Most clusters one transfer may name.
const TRANSFER_MAX_CLUSTERS: usize = 1 << 16;
/// Longest cluster id or header name one transfer may carry.
const TRANSFER_MAX_TEXT: usize = 1 << 16;

/// Why the confirmed protected state could not be handed to a new main
/// process, or an encoded hand-over was refused. Static on purpose: neither a
/// value, nor the key, nor any length of one appears in a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransferError {
    #[error("an unresolved protected header version cannot be handed over")]
    Pending,
    #[error("held protected header values have no fingerprint key")]
    KeyMissing,
    #[error("held protected header state does not match the configuration metadata")]
    Mismatch,
    #[error("protected header transfer exceeds its maximum size")]
    TooLarge,
    #[error("protected header transfer is truncated")]
    Truncated,
    #[error("protected header transfer has trailing bytes")]
    Trailing,
    #[error("protected header transfer is malformed")]
    Malformed,
    #[error("protected header transfer buffer is too small")]
    BufferTooSmall,
}

/// The fingerprint key of a [`ProtectedTransfer`], in zeroing storage.
struct TransferKey {
    key: Zeroizing<[u8; KEY_LEN]>,
    epoch: [u8; EPOCH_LEN],
}

struct TransferHeader {
    name: String,
    value: Arc<SecretValue>,
}

/// The confirmed protected state of a main process, as a new main process
/// takes it over: the optional fingerprint key, the number of versions issued,
/// and the confirmed header set of every cluster, clusters sorted by id and
/// headers in their configured order. Pending proposals never travel.
///
/// There is no serde, no `Clone`, and `Debug` prints counts only. The values
/// stay behind their `Arc` handles until [`Self::encode_into`] writes them
/// into a caller-owned zeroing buffer; the key is copied into zeroing
/// storage. The one reader is [`Self::restore`], which accepts nothing the
/// authoritative configuration metadata does not name.
pub struct ProtectedTransfer {
    key: Option<TransferKey>,
    last_issued: u64,
    clusters: Vec<(String, Vec<TransferHeader>)>,
}

/// Whether `set` is the confirmed state the configuration metadata of its
/// cluster names: an empty set needs no header named, a held one needs every
/// name and full fingerprint, in order.
fn verify_set(
    set: &ProtectedHeaderSet,
    metadata: Option<&[HealthCheckHeader]>,
) -> Result<(), TransferError> {
    let matches = if set.is_empty() {
        metadata.is_none_or(<[HealthCheckHeader]>::is_empty)
    } else {
        metadata.is_some_and(|metadata| set.satisfies(metadata))
    };
    if matches {
        Ok(())
    } else {
        Err(TransferError::Mismatch)
    }
}

impl ProtectedTransfer {
    /// The confirmed state of `journal` and `key`, or why it cannot be handed
    /// over exactly: an unresolved proposal, a held value without a key, or a
    /// set the configuration metadata of its cluster does not name.
    pub fn export<'m>(
        journal: &ProtectedHeaderJournal,
        key: Option<&FingerprintKey>,
        metadata: impl Fn(&str) -> Option<&'m [HealthCheckHeader]>,
    ) -> Result<Self, TransferError> {
        if journal
            .clusters
            .values()
            .any(|cluster| !cluster.pending.is_empty())
        {
            return Err(TransferError::Pending);
        }
        let mut clusters = Vec::with_capacity(journal.clusters.len());
        for (id, cluster) in &journal.clusters {
            let Some(set) = &cluster.confirmed else {
                continue;
            };
            verify_set(set, metadata(id))?;
            if !set.is_empty() {
                let key = key.ok_or(TransferError::KeyMissing)?;
                if !set
                    .headers()
                    .iter()
                    .all(|header| key.verify(&header.name, &header.value, &header.fingerprint))
                {
                    return Err(TransferError::Mismatch);
                }
            }
            clusters.push((
                id.to_owned(),
                set.headers()
                    .iter()
                    .map(|header| TransferHeader {
                        name: header.name.to_owned(),
                        value: Arc::clone(&header.value),
                    })
                    .collect(),
            ));
        }
        clusters.sort_by(|left, right| left.0.cmp(&right.0));
        let key = key.map(|key| {
            let mut copy = Zeroizing::new([0u8; KEY_LEN]);
            copy.copy_from_slice(key.key.as_slice());
            TransferKey {
                key: copy,
                epoch: key.epoch,
            }
        });
        Ok(Self {
            key,
            last_issued: journal.last_issued,
            clusters,
        })
    }

    /// Rebuild a fresh key and journal from the transfer, and keep a cluster's
    /// set only when `metadata`, the authoritative configuration, names it
    /// exactly: same names and full HMAC fingerprints under the transferred
    /// key, in the same order. An epoch alone is never acceptance.
    pub fn restore<'m>(
        self,
        metadata: impl Fn(&str) -> Option<&'m [HealthCheckHeader]>,
    ) -> Result<(Option<FingerprintKey>, ProtectedHeaderJournal), TransferError> {
        let Self {
            key,
            last_issued,
            clusters,
        } = self;
        let key = key.map(|transfer| FingerprintKey::from_parts(transfer.key, transfer.epoch));
        let mut restored = HashMap::with_capacity(clusters.len());
        for (id, headers) in clusters {
            let mut protected = Vec::with_capacity(headers.len());
            for header in headers {
                let key = key.as_ref().ok_or(TransferError::KeyMissing)?;
                protected.push(ProtectedHeader {
                    fingerprint: key.fingerprint(&header.name, &header.value),
                    name: header.name,
                    value: header.value,
                });
            }
            let set = ProtectedHeaderSet::new(protected);
            verify_set(&set, metadata(&id))?;
            restored.insert(
                id,
                ClusterJournal {
                    confirmed: Some(set),
                    pending: Vec::new(),
                },
            );
        }
        Ok((
            key,
            ProtectedHeaderJournal {
                last_issued,
                clusters: restored,
            },
        ))
    }

    /// The exact number of bytes [`Self::encode_into`] appends.
    pub fn encoded_len(&self) -> Result<usize, TransferError> {
        // Version, key flag, `last_issued` and the cluster count.
        let mut total = 2 + 8 + 4;
        if self.key.is_some() {
            total += KEY_LEN + EPOCH_LEN;
        }
        for (id, headers) in &self.clusters {
            total = total
                .checked_add(4 + id.len() + 4)
                .ok_or(TransferError::TooLarge)?;
            for header in headers {
                total = total
                    .checked_add(4 + header.name.len() + 4)
                    .and_then(|total| total.checked_add(header.value.expose_secret_bytes().len()))
                    .ok_or(TransferError::TooLarge)?;
            }
        }
        if total > PROTECTED_TRANSFER_MAX_BYTES {
            return Err(TransferError::TooLarge);
        }
        Ok(total)
    }

    /// Append the encoding to `out` without ever growing it: the spare
    /// capacity must hold [`Self::encoded_len`] bytes, so no unwiped block is
    /// left behind by a reallocation.
    pub fn encode_into(&self, out: &mut Zeroizing<Vec<u8>>) -> Result<(), TransferError> {
        let needed = self.encoded_len()?;
        if out.capacity() - out.len() < needed {
            return Err(TransferError::BufferTooSmall);
        }
        let start = out.len();
        let length = |len: usize| u32::try_from(len).map_err(|_| TransferError::TooLarge);
        out.push(TRANSFER_LAYOUT_VERSION);
        match &self.key {
            Some(transfer) => {
                out.push(1);
                out.extend_from_slice(transfer.key.as_slice());
                out.extend_from_slice(&transfer.epoch);
            }
            None => out.push(0),
        }
        out.extend_from_slice(&self.last_issued.to_be_bytes());
        out.extend_from_slice(&length(self.clusters.len())?.to_be_bytes());
        for (id, headers) in &self.clusters {
            out.extend_from_slice(&length(id.len())?.to_be_bytes());
            out.extend_from_slice(id.as_bytes());
            out.extend_from_slice(&length(headers.len())?.to_be_bytes());
            for header in headers {
                let value = header.value.expose_secret_bytes();
                out.extend_from_slice(&length(header.name.len())?.to_be_bytes());
                out.extend_from_slice(header.name.as_bytes());
                out.extend_from_slice(&length(value.len())?.to_be_bytes());
                out.extend_from_slice(value);
            }
        }
        debug_assert_eq!(out.len() - start, needed, "encoded_len is exact");
        Ok(())
    }

    /// Read an encoding back. Every count and length is bounded before it is
    /// used, text must be UTF-8, values must pass the per-value header rules,
    /// clusters must be strictly increasing, and nothing may follow.
    pub fn decode(bytes: &[u8]) -> Result<Self, TransferError> {
        struct Reader<'a>(&'a [u8]);
        impl<'a> Reader<'a> {
            fn take(&mut self, len: usize) -> Result<&'a [u8], TransferError> {
                if self.0.len() < len {
                    return Err(TransferError::Truncated);
                }
                let (head, tail) = self.0.split_at(len);
                self.0 = tail;
                Ok(head)
            }
            fn count(&mut self, maximum: usize) -> Result<usize, TransferError> {
                let bytes: [u8; 4] = self
                    .take(4)?
                    .try_into()
                    .map_err(|_| TransferError::Malformed)?;
                let count = usize::try_from(u32::from_be_bytes(bytes))
                    .map_err(|_| TransferError::TooLarge)?;
                if count > maximum {
                    return Err(TransferError::TooLarge);
                }
                Ok(count)
            }
            fn text(&mut self) -> Result<String, TransferError> {
                let len = self.count(TRANSFER_MAX_TEXT)?;
                String::from_utf8(self.take(len)?.to_vec()).map_err(|_| TransferError::Malformed)
            }
        }

        if bytes.len() > PROTECTED_TRANSFER_MAX_BYTES {
            return Err(TransferError::TooLarge);
        }
        let mut reader = Reader(bytes);
        if reader.take(1)? != [TRANSFER_LAYOUT_VERSION] {
            return Err(TransferError::Malformed);
        }
        let key = match reader.take(1)? {
            [0] => None,
            [1] => {
                let mut key = Zeroizing::new([0u8; KEY_LEN]);
                key.copy_from_slice(reader.take(KEY_LEN)?);
                let epoch = reader
                    .take(EPOCH_LEN)?
                    .try_into()
                    .map_err(|_| TransferError::Malformed)?;
                Some(TransferKey { key, epoch })
            }
            _ => return Err(TransferError::Malformed),
        };
        let last_issued = u64::from_be_bytes(
            reader
                .take(8)?
                .try_into()
                .map_err(|_| TransferError::Malformed)?,
        );
        let cluster_count = reader.count(TRANSFER_MAX_CLUSTERS)?;
        let mut clusters: Vec<(String, Vec<TransferHeader>)> = Vec::new();
        for _ in 0..cluster_count {
            let id = reader.text()?;
            if clusters.last().is_some_and(|(previous, _)| *previous >= id) {
                return Err(TransferError::Malformed);
            }
            let header_count = reader.count(crate::config::HEALTH_CHECK_MAX_HEADERS)?;
            let mut headers = Vec::new();
            for _ in 0..header_count {
                let name = reader.text()?;
                let len = reader.count(crate::config::HEALTH_CHECK_MAX_HEADER_BYTES)?;
                let mut carrier = HealthCheckHeaderValue {
                    name: name.to_owned(),
                    value: reader.take(len)?.to_vec(),
                };
                let value = SecretValue::take_from_ingress(&mut carrier)
                    .map_err(|_| TransferError::Malformed)?;
                headers.push(TransferHeader {
                    name,
                    value: Arc::new(value),
                });
            }
            clusters.push((id, headers));
        }
        if !reader.0.is_empty() {
            return Err(TransferError::Trailing);
        }
        Ok(Self {
            key,
            last_issued,
            clusters,
        })
    }
}

impl fmt::Debug for ProtectedTransfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProtectedTransfer")
            .field("key", &self.key.is_some())
            .field("clusters", &self.clusters.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use zeroize::Zeroizing;

    use super::*;
    use crate::proto::command::HealthCheckHeaderValue;

    /// Test-only marker: it must never appear in a production rendering.
    const SENTINEL: &str = "SYNTHETIC-SLICE1-SECRET-4f1c";

    /// The raw HMAC-SHA256 primitive, finalized.
    fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; TAG_LEN] {
        hmac_sha256_mac(key, parts).finalize().into_bytes().into()
    }

    fn ingress(name: &str, value: &[u8]) -> HealthCheckHeaderValue {
        HealthCheckHeaderValue {
            name: name.to_owned(),
            value: value.to_vec(),
        }
    }

    fn secret(value: &[u8]) -> SecretValue {
        SecretValue::take_from_ingress(&mut ingress("authorization", value)).unwrap()
    }

    fn key(byte: u8, epoch_byte: u8) -> FingerprintKey {
        FingerprintKey::from_parts(Zeroizing::new([byte; 32]), [epoch_byte; 16])
    }

    #[test]
    fn ingress_serialization_fails_and_never_reaches_the_writer() {
        let message = ingress("authorization", SENTINEL.as_bytes());
        assert!(serde_json::to_string(&message).is_err());
        let mut written = Vec::new();
        assert!(serde_json::to_writer(&mut written, &message).is_err());
        let written = String::from_utf8_lossy(&written);
        assert!(!written.contains(SENTINEL), "{written}");
        let bytes = serde_json::to_vec(&message);
        assert!(bytes.is_err());
    }

    #[test]
    fn ingress_deserialization_fails_for_every_json_shape() {
        let as_array = format!(
            r#"{{"name":"authorization","value":{:?}}}"#,
            SENTINEL.as_bytes()
        );
        let as_string = format!(r#"{{"name":"authorization","value":"{SENTINEL}"}}"#);
        for json in [as_array, as_string] {
            let error = serde_json::from_str::<HealthCheckHeaderValue>(&json)
                .expect_err("the ingress value must never be read from JSON");
            assert!(!error.to_string().contains(SENTINEL), "{error}");
        }
    }

    #[test]
    fn ingress_debug_prints_the_name_and_a_placeholder_only() {
        let message = ingress("authorization", SENTINEL.as_bytes());
        let rendered = format!("{message:?}");
        assert!(rendered.contains("authorization"), "{rendered}");
        assert!(rendered.contains("<protected>"), "{rendered}");
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        assert!(
            !rendered.contains(&SENTINEL.len().to_string()),
            "{rendered}"
        );
        let pretty = format!("{message:#?}");
        assert!(!pretty.contains(SENTINEL), "{pretty}");
        assert!(!pretty.contains(&SENTINEL.len().to_string()), "{pretty}");

        let long = ingress(&format!("x-{}", "n".repeat(200)), b"v");
        let rendered = format!("{long:?}");
        assert!(rendered.contains("name_truncated: true"), "{rendered}");
        assert!(!rendered.contains(&"n".repeat(100)), "{rendered}");
    }

    #[test]
    fn secret_debug_never_reveals_the_value() {
        let rendered = format!("{:?}", secret(SENTINEL.as_bytes()));
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        assert!(rendered.contains("<protected>"), "{rendered}");
        assert!(
            !rendered.contains(&SENTINEL.len().to_string()),
            "{rendered}"
        );
    }

    #[test]
    fn extraction_moves_the_allocation_and_keeps_the_exact_bytes() {
        // Interior HTAB, obs-text and multi-byte UTF-8 are accepted today.
        let accepted = "Bearer a\tb \u{e9}\u{4e16}".as_bytes();
        let mut message = ingress("authorization", accepted);
        let pointer = message.value.as_ptr();
        let value = SecretValue::take_from_ingress(&mut message).unwrap();
        assert!(message.value.is_empty(), "the generated field is emptied");
        assert_eq!(message.name, "authorization", "the name is left in place");
        assert_eq!(value.expose_secret_bytes(), accepted);
        assert_eq!(value.expose_secret_bytes().as_ptr(), pointer, "moved");

        let mut empty = ingress("x-empty", b"");
        let value = SecretValue::take_from_ingress(&mut empty).unwrap();
        assert_eq!(value.expose_secret_bytes(), b"");
    }

    #[test]
    fn extraction_refuses_with_static_errors_and_still_empties_the_field() {
        let sentinel = SENTINEL.as_bytes();
        let with = |prefix: &[u8], suffix: &[u8]| [prefix, sentinel, suffix].concat();
        let cases: [(Vec<u8>, SecretValueError); 8] = [
            (with(b"\xff\xfe", b""), SecretValueError::NotUtf8),
            (with(b"", b"\xc3"), SecretValueError::NotUtf8),
            (with(b"a\0", b""), SecretValueError::Invalid(CONTROL)),
            (with(b"", b"\r\nX: y"), SecretValueError::Invalid(CONTROL)),
            (with(b"\x7f", b""), SecretValueError::Invalid(CONTROL)),
            (with(b" ", b""), SecretValueError::Invalid(EDGE)),
            (with(b"", b" "), SecretValueError::Invalid(EDGE)),
            (with(b"\t", b""), SecretValueError::Invalid(EDGE)),
        ];
        for (bytes, expected) in cases {
            let mut message = ingress("authorization", &bytes);
            let error = SecretValue::take_from_ingress(&mut message)
                .expect_err("the value must be refused");
            assert_eq!(error, expected);
            assert!(message.value.is_empty(), "refusal still empties the field");
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(!rendered.contains(SENTINEL), "{rendered}");
                assert!(!rendered.contains(&bytes.len().to_string()), "{rendered}");
            }
        }
        let trailing_tab = with(b"", b"\t");
        let error = SecretValue::take_from_ingress(&mut ingress("a", &trailing_tab));
        assert_eq!(error.unwrap_err(), SecretValueError::Invalid(EDGE));
    }

    const CONTROL: &str = "health check header value must not contain control bytes";
    const EDGE: &str = "health check header value must not begin or end with a space or a tab";

    #[test]
    fn raw_hmac_sha256_matches_rfc_4231_test_case_1() {
        let tag = hmac_sha256(&[0x0b; 20], &[b"Hi There"]);
        assert_eq!(
            hex::encode(tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Streaming the message in pieces is the same MAC.
        assert_eq!(tag, hmac_sha256(&[0x0b; 20], &[b"Hi ", b"There"]));
    }

    #[test]
    fn fingerprint_is_the_full_tag_and_is_stable() {
        let key = key(7, 9);
        let value = secret(SENTINEL.as_bytes());
        let first = key.fingerprint("Authorization", &value);
        let second = key.fingerprint("authorization", &value);
        assert_eq!(first, second, "the name is lowercased");

        let text = first.to_string();
        let parts: Vec<&str> = text.split(':').collect();
        assert_eq!(parts.len(), 4, "{text}");
        assert_eq!(parts[0], "sozu-hc-hmac-sha256");
        assert_eq!(parts[1], "v1");
        assert_eq!(parts[2], "09".repeat(16));
        assert_eq!(parts[3].len(), 64, "full 256-bit tag: {text}");
        assert!(
            parts[3]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert!(!text.contains(SENTINEL));

        assert_eq!(HeaderFingerprint::from_str(&text).unwrap(), first);
        assert!(key.verify("authorization", &value, &first));
    }

    #[test]
    fn fingerprint_depends_on_name_value_key_and_epoch() {
        let value = secret(b"one");
        let base = key(1, 1).fingerprint("authorization", &value);
        assert_ne!(base, key(1, 1).fingerprint("x-other", &value));
        assert_ne!(
            base,
            key(1, 1).fingerprint("authorization", &secret(b"two"))
        );
        assert_ne!(base, key(2, 1).fingerprint("authorization", &value));
        assert_ne!(base, key(1, 2).fingerprint("authorization", &value));
        // The epoch is part of the MAC input, not only a label on the output.
        let tag = |fingerprint: HeaderFingerprint| {
            fingerprint
                .to_string()
                .rsplit(':')
                .next()
                .unwrap()
                .to_owned()
        };
        assert_ne!(
            tag(base),
            tag(key(1, 2).fingerprint("authorization", &value))
        );
        // Length prefixes keep the name/value boundary unambiguous.
        assert_ne!(
            key(1, 1).fingerprint("ab", &secret(b"c")),
            key(1, 1).fingerprint("a", &secret(b"bc")),
        );
    }

    #[test]
    fn verification_needs_the_same_epoch_and_the_same_key() {
        let value = secret(SENTINEL.as_bytes());
        let issued = key(1, 5).fingerprint("authorization", &value);
        assert!(key(1, 5).verify("authorization", &value, &issued));
        assert!(!key(1, 5).verify("x-other", &value, &issued));
        assert!(!key(1, 5).verify("authorization", &secret(b"other"), &issued));
        // Same public epoch, another key: an epoch collision is not acceptance.
        assert!(!key(2, 5).verify("authorization", &value, &issued));
        // Another epoch, same key.
        assert!(!key(1, 6).verify("authorization", &value, &issued));
    }

    #[test]
    fn generated_keys_are_independent() {
        let (a, b) = (
            FingerprintKey::generate().unwrap(),
            FingerprintKey::generate().unwrap(),
        );
        let value = secret(b"v");
        let fingerprint = a.fingerprint("authorization", &value);
        assert!(a.verify("authorization", &value, &fingerprint));
        assert!(!b.verify("authorization", &value, &fingerprint));
        let rendered = format!("{a:?}");
        assert!(rendered.contains("<protected>"), "{rendered}");
    }

    #[test]
    fn parsing_rejects_every_malformed_fingerprint() {
        let epoch = "ab".repeat(16);
        let tag = "cd".repeat(32);
        let ok = format!("sozu-hc-hmac-sha256:v1:{epoch}:{tag}");
        assert!(HeaderFingerprint::from_str(&ok).is_ok());

        let cases = [
            (String::new(), FingerprintParseError::Malformed),
            (
                "sozu-hc-hmac-sha256:v1".to_owned(),
                FingerprintParseError::Malformed,
            ),
            (format!("{ok}:extra"), FingerprintParseError::Malformed),
            (
                format!("sozu-hc-hmac-sha512:v1:{epoch}:{tag}"),
                FingerprintParseError::Algorithm,
            ),
            (
                format!("SOZU-HC-HMAC-SHA256:v1:{epoch}:{tag}"),
                FingerprintParseError::Algorithm,
            ),
            (
                format!("sozu-hc-hmac-sha256:v2:{epoch}:{tag}"),
                FingerprintParseError::Version,
            ),
            (
                format!("sozu-hc-hmac-sha256:V1:{epoch}:{tag}"),
                FingerprintParseError::Version,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{}:{tag}", &epoch[2..]),
                FingerprintParseError::Epoch,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{}:{tag}", "AB".repeat(16)),
                FingerprintParseError::Epoch,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{}:{tag}", "zz".repeat(16)),
                FingerprintParseError::Epoch,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{epoch}:{}", &tag[2..]),
                FingerprintParseError::Tag,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{epoch}:{}", "CD".repeat(32)),
                FingerprintParseError::Tag,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{epoch}:{}0", tag),
                FingerprintParseError::Tag,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{epoch}:{}", "\u{e9}".repeat(32)),
                FingerprintParseError::Tag,
            ),
            (
                format!("sozu-hc-hmac-sha256:v1:{epoch}:{}", "+".repeat(64)),
                FingerprintParseError::Tag,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                HeaderFingerprint::from_str(&input),
                Err(expected),
                "{input}"
            );
        }
    }

    /// A header whose value is `value`, fingerprinted under the fixed test key.
    fn header(name: &str, value: &[u8]) -> ProtectedHeader {
        ProtectedHeader::new(&key(3, 4), name.to_owned(), secret(value))
    }

    fn set(entries: &[(&str, &str)]) -> ProtectedHeaderSet {
        ProtectedHeaderSet::new(
            entries
                .iter()
                .map(|(name, value)| header(name, value.as_bytes()))
                .collect(),
        )
    }

    fn replace(entries: &[(&str, &str)]) -> ProtectedHeaderChange {
        ProtectedHeaderChange::Replace(set(entries))
    }

    /// `expected` is `None` for an absent cluster, `Some(set)` for the very
    /// same `Arc`-backed set (an empty one for a cleared cluster).
    fn assert_effective(
        journal: &ProtectedHeaderJournal,
        expected: Option<&ProtectedHeaderSet>,
        context: &str,
    ) {
        match (journal.effective("c"), expected) {
            (None, None) => {}
            (Some(actual), Some(expected)) => {
                assert!(
                    actual.ptr_eq(expected),
                    "{context}: another set is effective"
                )
            }
            (actual, expected) => panic!(
                "{context}: effective present={} expected present={}",
                actual.is_some(),
                expected.is_some()
            ),
        }
    }

    fn pending_total(journal: &ProtectedHeaderJournal) -> usize {
        journal.clusters.values().map(|c| c.pending.len()).sum()
    }

    /// `old` accepted, then `A` and `B` proposed in that order.
    struct Chain {
        journal: ProtectedHeaderJournal,
        old: ProtectedHeaderSet,
        a: ProtectedHeaderSet,
        b: ProtectedHeaderSet,
        va: ProtectedHeaderVersion,
        vb: ProtectedHeaderVersion,
    }

    fn chain() -> Chain {
        let mut journal = ProtectedHeaderJournal::default();
        let old = set(&[("authorization", "old")]);
        let v_old = journal
            .propose("c", ProtectedHeaderChange::Replace(old.clone()))
            .unwrap();
        assert_eq!(journal.accept("c", v_old), Resolution::Applied);
        let a = set(&[("authorization", "a")]);
        let b = set(&[("authorization", "b")]);
        let va = journal
            .propose("c", ProtectedHeaderChange::Replace(a.clone()))
            .unwrap();
        assert_effective(&journal, Some(&a), "A is optimistically effective");
        let vb = journal
            .propose("c", ProtectedHeaderChange::Replace(b.clone()))
            .unwrap();
        assert_effective(&journal, Some(&b), "B is optimistically effective");
        Chain {
            journal,
            old,
            a,
            b,
            va,
            vb,
        }
    }

    #[test]
    fn arc_handles_clone_without_copying_secret_bytes() {
        // `SecretValue: Clone` does not exist; cloning must still compile.
        let original = header("authorization", SENTINEL.as_bytes());
        let cloned = original.clone();
        assert!(Arc::ptr_eq(original.value(), cloned.value()));
        assert_eq!(
            original.value().expose_secret_bytes().as_ptr(),
            cloned.value().expose_secret_bytes().as_ptr(),
            "the bytes are shared, not copied"
        );
        assert_eq!(Arc::strong_count(original.value()), 2);

        let handle = ProtectedHeaderSet::new(vec![original]);
        let handle_clone = handle.clone();
        assert!(handle.ptr_eq(&handle_clone));
        assert!(Arc::ptr_eq(
            handle.headers()[0].value(),
            handle_clone.headers()[0].value()
        ));
    }

    #[test]
    fn set_keeps_header_order_and_fingerprints() {
        let key = key(3, 4);
        let headers = set(&[
            ("x-zulu", "one"),
            ("authorization", SENTINEL),
            ("x-alpha", "one"),
        ]);
        let names: Vec<&str> = headers.headers().iter().map(|h| h.name()).collect();
        assert_eq!(names, ["x-zulu", "authorization", "x-alpha"]);
        for header in headers.headers() {
            assert!(key.verify(header.name(), header.value(), header.fingerprint()));
        }
        // Equality inside one epoch is visible through equal fingerprints.
        let fingerprints: Vec<&HeaderFingerprint> =
            headers.headers().iter().map(|h| h.fingerprint()).collect();
        assert_ne!(fingerprints[0], fingerprints[2], "the name is MACed too");
        assert_eq!(
            header("x-zulu", b"one").fingerprint(),
            fingerprints[0],
            "same name and value, same epoch, same fingerprint"
        );
        assert_eq!(headers.len(), 3);
        assert!(!headers.is_empty());
    }

    #[test]
    fn protected_debug_prints_no_value_and_no_length() {
        let value = format!("{SENTINEL}-{}", "z".repeat(51));
        let headers = set(&[("authorization", &value), ("x-other", &value)]);
        let mut journal = ProtectedHeaderJournal::default();
        journal
            .propose("c", ProtectedHeaderChange::Replace(headers.clone()))
            .unwrap();
        let renderings = [
            format!("{:?}", headers.headers()[0]),
            format!("{:#?}", headers.headers()[0]),
            format!("{headers:?}"),
            format!("{headers:#?}"),
            format!("{journal:?}"),
            format!("{journal:#?}"),
        ];
        for rendered in renderings {
            assert!(!rendered.contains(SENTINEL), "{rendered}");
            assert!(!rendered.contains("zzzz"), "{rendered}");
            // The fingerprints are hex: take them out before looking for the
            // decimal length, which they could contain by chance.
            let mut without_hex = rendered.clone();
            for header in headers.headers() {
                without_hex = without_hex.replace(&header.fingerprint().to_string(), "");
            }
            assert!(
                !without_hex.contains(&value.len().to_string()),
                "{rendered}"
            );
            assert!(!rendered.contains("len"), "{rendered}");
        }
        let rendered = format!("{:?}", header(&format!("x-{}", "n".repeat(200)), b"v"));
        assert!(!rendered.contains(&"n".repeat(100)), "{rendered}");
    }

    #[test]
    fn rejecting_a_then_b_restores_the_base() {
        let mut c = chain();
        assert_eq!(c.journal.reject("c", c.va), Resolution::Applied);
        assert_effective(&c.journal, Some(&c.b), "A rejected, B is still live");
        assert_eq!(c.journal.reject("c", c.vb), Resolution::Applied);
        assert_effective(&c.journal, Some(&c.old), "both rejected");
    }

    #[test]
    fn rejecting_a_then_accepting_b_keeps_b() {
        let mut c = chain();
        c.journal.reject("c", c.va);
        assert_effective(&c.journal, Some(&c.b), "A rejected, B is still live");
        assert_eq!(c.journal.accept("c", c.vb), Resolution::Applied);
        assert_effective(&c.journal, Some(&c.b), "B accepted");
    }

    #[test]
    fn rejecting_b_then_accepting_a_keeps_a() {
        let mut c = chain();
        c.journal.reject("c", c.vb);
        assert_effective(&c.journal, Some(&c.a), "B rejected, A is live");
        assert_eq!(c.journal.accept("c", c.va), Resolution::Applied);
        assert_effective(&c.journal, Some(&c.a), "A accepted");
    }

    #[test]
    fn rejecting_b_then_a_restores_the_base() {
        let mut c = chain();
        c.journal.reject("c", c.vb);
        assert_effective(&c.journal, Some(&c.a), "B rejected, A is live");
        c.journal.reject("c", c.va);
        assert_effective(&c.journal, Some(&c.old), "both rejected");
    }

    #[test]
    fn accepting_a_then_rejecting_b_keeps_a() {
        let mut c = chain();
        c.journal.accept("c", c.va);
        assert_effective(&c.journal, Some(&c.b), "A accepted, B is still live");
        c.journal.reject("c", c.vb);
        assert_effective(&c.journal, Some(&c.a), "B rejected over an accepted A");
    }

    #[test]
    fn accepting_b_then_rejecting_a_keeps_b() {
        let mut c = chain();
        c.journal.accept("c", c.vb);
        assert_effective(&c.journal, Some(&c.b), "B accepted");
        assert_eq!(c.journal.reject("c", c.va), Resolution::Ignored);
        assert_effective(&c.journal, Some(&c.b), "a stale ancestor cannot displace B");
    }

    #[test]
    fn clear_leaves_a_present_empty_set_and_failed_clear_restores_the_predecessor() {
        let mut c = chain();
        let cleared = c
            .journal
            .propose("c", ProtectedHeaderChange::Clear)
            .unwrap();
        let effective = c.journal.effective("c").expect("a cleared cluster remains");
        assert!(effective.is_empty());
        c.journal.accept("c", cleared);
        let effective = c.journal.effective("c").expect("a cleared cluster remains");
        assert!(effective.is_empty());
        assert!(c.journal.clusters.contains_key("c"), "not a tombstone");
        // The older proposals were superseded by the accepted clear.
        assert_eq!(pending_total(&c.journal), 0);

        let mut c = chain();
        let cleared = c
            .journal
            .propose("c", ProtectedHeaderChange::Clear)
            .unwrap();
        assert!(c.journal.effective("c").unwrap().is_empty());
        c.journal.reject("c", cleared);
        assert_effective(&c.journal, Some(&c.b), "failed clear restores B");
        c.journal.reject("c", c.vb);
        c.journal.reject("c", c.va);
        assert_effective(&c.journal, Some(&c.old), "and then the base");
    }

    #[test]
    fn remove_leaves_the_cluster_absent_and_failed_remove_restores_the_predecessor() {
        let mut c = chain();
        let removed = c
            .journal
            .propose("c", ProtectedHeaderChange::Remove)
            .unwrap();
        assert_effective(&c.journal, None, "removal is optimistically effective");
        c.journal.accept("c", removed);
        assert_effective(&c.journal, None, "removed");
        assert!(c.journal.clusters.is_empty(), "no tombstone is kept");

        let mut c = chain();
        let removed = c
            .journal
            .propose("c", ProtectedHeaderChange::Remove)
            .unwrap();
        c.journal.reject("c", removed);
        assert_effective(&c.journal, Some(&c.b), "failed removal restores B");
    }

    #[test]
    fn late_completion_after_a_successful_removal_cannot_resurrect() {
        for late_accept in [true, false] {
            let mut c = chain();
            let removed = c
                .journal
                .propose("c", ProtectedHeaderChange::Remove)
                .unwrap();
            c.journal.accept("c", removed);
            for version in [c.va, c.vb] {
                let resolution = if late_accept {
                    c.journal.accept("c", version)
                } else {
                    c.journal.reject("c", version)
                };
                assert_eq!(resolution, Resolution::Ignored);
                assert_effective(&c.journal, None, "no resurrection");
            }
            assert!(c.journal.clusters.is_empty());
        }
        // An older proposal accepted before the removal completes does not
        // outlive it either.
        let mut c = chain();
        let removed = c
            .journal
            .propose("c", ProtectedHeaderChange::Remove)
            .unwrap();
        c.journal.accept("c", c.va);
        assert_effective(&c.journal, None, "the removal is still the head");
        c.journal.accept("c", removed);
        assert_effective(&c.journal, None, "removed");
    }

    #[test]
    fn duplicate_and_unknown_resolutions_change_nothing() {
        let mut c = chain();
        c.journal.accept("c", c.vb);
        let (last, pending, clusters) = (
            c.journal.last_issued,
            pending_total(&c.journal),
            c.journal.clusters.len(),
        );
        for (cluster, version) in [("c", c.vb), ("c", c.va), ("other", c.vb)] {
            assert_eq!(c.journal.accept(cluster, version), Resolution::Ignored);
            assert_eq!(c.journal.reject(cluster, version), Resolution::Ignored);
            assert_effective(&c.journal, Some(&c.b), "unchanged");
        }
        let unknown = ProtectedHeaderVersion(last + 1000);
        assert_eq!(c.journal.accept("c", unknown), Resolution::Ignored);
        assert_eq!(c.journal.reject("c", unknown), Resolution::Ignored);
        assert_eq!(
            (
                c.journal.last_issued,
                pending_total(&c.journal),
                c.journal.clusters.len()
            ),
            (last, pending, clusters)
        );
        assert_effective(&c.journal, Some(&c.b), "unchanged");

        // A rejected id cannot be accepted afterwards.
        let mut c = chain();
        c.journal.reject("c", c.vb);
        assert_eq!(c.journal.accept("c", c.vb), Resolution::Ignored);
        assert_eq!(c.journal.reject("c", c.vb), Resolution::Ignored);
        assert_effective(&c.journal, Some(&c.a), "B stays rejected");
        // An id of another cluster resolves nothing here.
        let other = c
            .journal
            .propose("other", ProtectedHeaderChange::Clear)
            .unwrap();
        assert_eq!(c.journal.reject("c", other), Resolution::Ignored);
        assert!(c.journal.effective("other").unwrap().is_empty());
    }

    #[test]
    fn versions_are_unique_and_increasing_across_clusters_and_resolutions() {
        let mut journal = ProtectedHeaderJournal::default();
        let mut seen = Vec::new();
        for round in 0..4 {
            for cluster in ["c", "d"] {
                let version = journal.propose(cluster, replace(&[("a", "1")])).unwrap();
                seen.push(version);
                if round % 2 == 0 {
                    journal.reject(cluster, version);
                } else {
                    journal.accept(cluster, version);
                }
            }
        }
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");
    }

    #[test]
    fn version_exhaustion_refuses_before_any_mutation() {
        let mut journal = ProtectedHeaderJournal::with_last_issued(u64::MAX - 1);
        let kept = set(&[("authorization", "kept")]);
        let last = journal
            .propose("c", ProtectedHeaderChange::Replace(kept.clone()))
            .unwrap();
        assert_eq!(last, ProtectedHeaderVersion(u64::MAX));
        let (issued, pending, clusters) = (
            journal.last_issued,
            pending_total(&journal),
            journal.clusters.len(),
        );
        for (cluster, change) in [
            ("c", ProtectedHeaderChange::Remove),
            ("fresh", ProtectedHeaderChange::Clear),
            ("c", replace(&[("x", "y")])),
        ] {
            assert_eq!(
                journal.propose(cluster, change).unwrap_err(),
                VersionExhausted
            );
            assert_eq!(
                (
                    journal.last_issued,
                    pending_total(&journal),
                    journal.clusters.len()
                ),
                (issued, pending, clusters)
            );
            assert_effective(&journal, Some(&kept), "head untouched");
        }
        assert_eq!(journal.accept("c", last), Resolution::Applied);
        assert_effective(&journal, Some(&kept), "the last id still resolves");
    }

    #[test]
    fn compaction_drops_every_resolved_chain_and_keeps_the_effective_handle() {
        let mut c = chain();
        c.journal.accept("c", c.vb);
        assert_eq!(pending_total(&c.journal), 0, "A is superseded by B");
        assert_effective(&c.journal, Some(&c.b), "exact Arc preserved");
        // Only the effective set (confirmed) and its two handles remain: the
        // fixture `b`, the journal; `a` and `old` are no longer held by it.
        assert_eq!(Arc::strong_count(&c.b.0), 2);
        assert_eq!(Arc::strong_count(&c.a.0), 1);
        assert_eq!(Arc::strong_count(&c.old.0), 1);

        let mut c = chain();
        c.journal.reject("c", c.va);
        c.journal.reject("c", c.vb);
        assert_eq!(pending_total(&c.journal), 0);
        assert_effective(&c.journal, Some(&c.old), "exact Arc preserved");
        assert_eq!(Arc::strong_count(&c.a.0), 1, "payload of A released");
        assert_eq!(Arc::strong_count(&c.b.0), 1, "payload of B released");

        // Churn does not grow the store.
        let mut journal = ProtectedHeaderJournal::default();
        for round in 0..100 {
            let version = journal.propose("c", replace(&[("a", "1")])).unwrap();
            if round % 3 == 0 {
                journal.reject("c", version);
            } else {
                journal.accept("c", version);
            }
            assert_eq!(pending_total(&journal), 0);
            assert!(journal.clusters.len() <= 1);
        }
        let removal = journal.propose("c", ProtectedHeaderChange::Remove).unwrap();
        journal.accept("c", removal);
        assert!(journal.clusters.is_empty());
    }

    #[test]
    fn parse_errors_are_static() {
        let error = HeaderFingerprint::from_str(&format!("x:{SENTINEL}")).unwrap_err();
        assert!(!error.to_string().contains(SENTINEL));
        assert!(!format!("{error:?}").contains(SENTINEL));
    }

    // ---- protected health-check orders -------------------------------------

    use crate::proto::command::{
        HealthCheckConfig, HealthCheckHeader, HealthCheckMode, SetProtectedHealthCheck,
    };

    fn order(headers: &[(&str, &str)]) -> SetProtectedHealthCheck {
        SetProtectedHealthCheck {
            cluster_id: "app".to_owned(),
            config: HealthCheckConfig {
                uri: "/health".to_owned(),
                interval: 10,
                timeout: 5,
                healthy_threshold: 3,
                unhealthy_threshold: 3,
                mode: HealthCheckMode::Http as i32,
                headers: headers
                    .iter()
                    .map(|(name, _)| HealthCheckHeader {
                        name: (*name).to_owned(),
                        fingerprint: String::new(),
                    })
                    .collect(),
                ..Default::default()
            },
            values: headers
                .iter()
                .map(|(name, value)| ingress(name, value.as_bytes()))
                .collect(),
        }
    }

    #[test]
    fn ingress_order_gets_server_fingerprints_and_moves_its_values_out() {
        let key = key(7, 8);
        let mut incoming = order(&[("Authorization", SENTINEL), ("X-Second", "two")]);
        let protected = ProtectedHealthCheck::from_ingress(&key, &mut incoming).unwrap();

        assert_eq!(protected.cluster_id(), "app");
        let set = protected.headers();
        assert_eq!(
            set.headers().iter().map(|h| h.name()).collect::<Vec<_>>(),
            ["Authorization", "X-Second"],
            "order is kept"
        );
        for (header, meta) in set.headers().iter().zip(&protected.config().headers) {
            assert_eq!(meta.name, header.name());
            assert_eq!(meta.fingerprint, header.fingerprint().to_string());
            assert!(key.verify(header.name(), header.value(), header.fingerprint()));
        }
        assert_eq!(
            set.headers()[0].value().expose_secret_bytes(),
            SENTINEL.as_bytes()
        );
        assert!(
            incoming
                .values
                .iter()
                .all(|carrier| carrier.value.is_empty()),
            "the clear bytes moved out of the ingress carriers"
        );
        assert!(protected.satisfies(&protected.config().headers));
    }

    #[test]
    fn ingress_order_refusals_name_no_value_and_a_spoofed_fingerprint_is_one() {
        let key = key(7, 8);
        let genuine = {
            let mut incoming = order(&[("Authorization", SENTINEL)]);
            ProtectedHealthCheck::from_ingress(&key, &mut incoming)
                .unwrap()
                .config()
                .headers[0]
                .fingerprint
                .to_owned()
        };

        // A client fingerprint that is the genuine one is accepted; any other is refused.
        let mut same = order(&[("Authorization", SENTINEL)]);
        same.config.headers[0].fingerprint = genuine.to_owned();
        assert!(ProtectedHealthCheck::from_ingress(&key, &mut same).is_ok());
        let mut spoofed = order(&[("Authorization", "another")]);
        spoofed.config.headers[0].fingerprint = genuine;
        assert_eq!(
            ProtectedHealthCheck::from_ingress(&key, &mut spoofed).err(),
            Some(ProtectedHealthCheckError::FingerprintMismatch)
        );

        let mut malformed = order(&[("Authorization", SENTINEL)]);
        malformed.config.headers[0].fingerprint = "junk".to_owned();
        assert!(ProtectedHealthCheck::from_ingress(&key, &mut malformed).is_err());

        let mut short = order(&[("a", SENTINEL), ("b", "2")]);
        short.values.pop();
        let mut swapped = order(&[("a", SENTINEL), ("b", "2")]);
        swapped.values.swap(0, 1);
        let mut renamed = order(&[("a", SENTINEL)]);
        renamed.values[0].name = "A-Different".to_owned();
        let mut bad_value = order(&[("a", SENTINEL)]);
        bad_value.values[0].value = format!(" {SENTINEL}").into_bytes();
        let mut bad_utf8 = order(&[("a", SENTINEL)]);
        bad_utf8.values[0].value = vec![0xff];
        let mut bad_name = order(&[("Host", SENTINEL)]);
        let mut tcp = order(&[("a", SENTINEL)]);
        tcp.config.mode = HealthCheckMode::Tcp as i32;
        bad_name.values[0].name = "Host".to_owned();
        for (what, mut incoming) in [
            ("short", short),
            ("swapped", swapped),
            ("renamed", renamed),
            ("bad value", bad_value),
            ("bad utf8", bad_utf8),
            ("reserved name", bad_name),
            ("tcp mode", tcp),
        ] {
            let error = ProtectedHealthCheck::from_ingress(&key, &mut incoming)
                .err()
                .unwrap_or_else(|| panic!("{what} must be refused"));
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains(SENTINEL), "{what}: {rendered}");
        }
    }

    #[test]
    fn trusted_order_round_trips_and_demands_a_fingerprint_per_header() {
        let key = key(7, 8);
        let mut incoming = order(&[("Authorization", SENTINEL), ("X-Second", "two")]);
        let main = ProtectedHealthCheck::from_ingress(&key, &mut incoming).unwrap();

        let mut forwarded = main.to_order();
        let worker = ProtectedHealthCheck::from_trusted(&mut forwarded).unwrap();
        assert_eq!(worker.config(), main.config());
        for (a, b) in worker
            .headers()
            .headers()
            .iter()
            .zip(main.headers().headers())
        {
            assert_eq!(a.name(), b.name());
            assert_eq!(a.fingerprint(), b.fingerprint());
            assert_eq!(
                a.value().expose_secret_bytes(),
                b.value().expose_secret_bytes()
            );
        }
        assert!(
            forwarded
                .values
                .iter()
                .all(|carrier| carrier.value.is_empty())
        );

        // A worker never takes an unfingerprinted order as trusted.
        let mut bare = order(&[("Authorization", SENTINEL)]);
        assert_eq!(
            ProtectedHealthCheck::from_trusted(&mut bare).err(),
            Some(ProtectedHealthCheckError::FingerprintMissing)
        );
    }

    #[test]
    fn a_header_set_satisfies_only_the_metadata_it_was_built_from() {
        let key = key(7, 8);
        let mut incoming = order(&[("Authorization", SENTINEL), ("X-Second", "two")]);
        let protected = ProtectedHealthCheck::from_ingress(&key, &mut incoming).unwrap();
        let metadata = protected.config().headers.to_owned();
        assert!(protected.headers().satisfies(&metadata));

        let mut reordered = metadata.to_owned();
        reordered.reverse();
        let mut other_value = metadata.to_owned();
        other_value[0].fingerprint = {
            let mut other = order(&[("Authorization", "another")]);
            ProtectedHealthCheck::from_ingress(&key, &mut other)
                .unwrap()
                .config()
                .headers[0]
                .fingerprint
                .to_owned()
        };
        let mut missing = metadata.to_owned();
        missing.pop();
        for (what, headers) in [
            ("reordered", reordered),
            ("other value", other_value),
            ("fewer", missing),
        ] {
            assert!(!protected.headers().satisfies(&headers), "{what}");
        }
        // No metadata is satisfied by no set, and by an empty one.
        assert!(ProtectedHeaderSet::new(Vec::new()).satisfies(&[]));
        assert!(!ProtectedHeaderSet::new(Vec::new()).satisfies(&metadata));
    }

    #[test]
    fn a_dropped_ingress_carrier_wipes_its_whole_buffer() {
        let mut carrier = ingress("authorization", SENTINEL.as_bytes());
        carrier.value.reserve(256);
        let (pointer, capacity) = (carrier.value.as_ptr(), carrier.value.capacity());
        carrier.wipe();
        assert!(carrier.value.is_empty());
        // SAFETY: `wipe` zeroes the owned allocation in place and the carrier
        // still owns it, so all `capacity` bytes are initialised and live.
        let bytes = unsafe { std::slice::from_raw_parts(pointer, capacity) };
        assert!(bytes.iter().all(|&b| b == 0), "the buffer was not wiped");
    }

    /// A reader that hands out at most `step` bytes per call.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// Some bytes, then a failure carrying the sentinel in its message.
    struct Failing(bool);

    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other(SENTINEL));
            }
            self.0 = true;
            buf[..4].copy_from_slice(b"abcd");
            Ok(4)
        }
    }

    fn read_value(bytes: &[u8], max: usize) -> Result<Vec<u8>, HeaderValueReadError> {
        read_header_value("authorization".to_owned(), bytes, max).map(|mut carrier| {
            let value = carrier.value.to_vec();
            carrier.wipe();
            value
        })
    }

    #[test]
    fn the_reader_removes_exactly_one_terminal_line_ending() {
        let cases: [(&[u8], &[u8]); 14] = [
            (b"", b""),
            (b"abc", b"abc"),
            (b"abc\n", b"abc"),
            (b"abc\r\n", b"abc"),
            (b"\n", b""),
            (b"\r\n", b""),
            (b"abc\r", b"abc\r"),
            (b"a\nb\n", b"a\nb"),
            (b"a\rb\r\n", b"a\rb"),
            (b"abc\n\n", b"abc\n"),
            (b"abc\r\n\r\n", b"abc\r\n"),
            (b"abc\n\r\n", b"abc\n"),
            (b"abc \n", b"abc "),
            (b"\tabc\t\r\n", b"\tabc\t"),
        ];
        for (input, expected) in cases {
            assert_eq!(read_value(input, 64).as_deref(), Ok(expected), "{input:?}");
        }
    }

    #[test]
    fn the_reader_applies_the_maximum_after_the_line_ending_is_removed() {
        let max = 16;
        let value = vec![b'v'; max];
        for terminator in [&b""[..], b"\n", b"\r\n"] {
            let input = [value.as_slice(), terminator].concat();
            assert_eq!(read_value(&input, max).as_deref(), Ok(value.as_slice()));
        }
        let over = vec![b'v'; max + 1];
        for terminator in [&b""[..], b"\n", b"\r\n"] {
            let input = [over.as_slice(), terminator].concat();
            assert_eq!(
                read_value(&input, max),
                Err(HeaderValueReadError::TooLong),
                "terminator {terminator:?}"
            );
        }
        // Content that fills the whole allowance and then more.
        let flood = vec![b'v'; max + 64];
        assert_eq!(read_value(&flood, max), Err(HeaderValueReadError::TooLong));
        let doubled = [value.as_slice(), b"\r\n\r\n"].concat();
        assert_eq!(
            read_value(&doubled, max),
            Err(HeaderValueReadError::TooLong)
        );
    }

    #[test]
    fn the_reader_reads_at_most_the_maximum_plus_three_bytes() {
        let max = 16;
        let mut source = std::io::Cursor::new(vec![b'v'; 4096]);
        let result = read_header_value("authorization".to_owned(), &mut source, max);
        assert_eq!(result.unwrap_err(), HeaderValueReadError::TooLong);
        assert_eq!(
            source.position(),
            (max + 3) as u64,
            "the source is not drained"
        );
    }

    #[test]
    fn the_reader_assembles_short_reads_and_keeps_the_name() {
        let input = b"Bearer a\tb\r\n";
        for step in [1, 2, 3, 5] {
            let source = Trickle { data: input, step };
            let carrier = read_header_value("X-Token".to_owned(), source, 32).unwrap();
            assert_eq!(carrier.name, "X-Token");
            assert_eq!(carrier.value, b"Bearer a\tb");
        }
    }

    #[test]
    fn the_reader_reports_an_io_error_without_the_bytes_or_their_length() {
        let error = read_header_value("authorization".to_owned(), Failing(false), 32)
            .expect_err("the source fails");
        assert_eq!(error, HeaderValueReadError::Io(ErrorKind::Other));
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains(SENTINEL), "{rendered}");
            assert!(!rendered.contains("abcd"), "{rendered}");
            assert!(!rendered.contains('4'), "{rendered}");
        }
        let too_long = read_value(&[b'v'; 40], 8).unwrap_err();
        for rendered in [too_long.to_string(), format!("{too_long:?}")] {
            assert!(
                !rendered.contains("40") && !rendered.contains('8'),
                "{rendered}"
            );
        }
    }

    #[test]
    fn the_reader_leaves_validation_to_the_value_validators() {
        // UTF-8, control bytes and edge blanks are not the reader's to refuse.
        let mut carrier = read_header_value("a".to_owned(), &b"\xff\0 \r\n"[..], 16).unwrap();
        assert_eq!(carrier.value, b"\xff\0 ");
        assert!(SecretValue::take_from_ingress(&mut carrier).is_err());
        assert!(carrier.value.is_empty(), "the refused bytes are gone");
    }

    #[test]
    fn the_journal_reports_whether_it_retains_any_value() {
        let mut journal = ProtectedHeaderJournal::default();
        assert!(!journal.holds_values(), "a new journal holds nothing");

        // A pending non-empty set is retained, then dropped by its rejection.
        let pending = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        assert!(journal.holds_values());
        journal.reject("c", pending);
        assert!(!journal.holds_values());

        // A confirmed non-empty set is retained, in any cluster.
        let confirmed = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        journal.accept("c", confirmed);
        assert!(journal.holds_values());
        let other = journal.propose("d", ProtectedHeaderChange::Clear).unwrap();
        journal.accept("d", other);
        assert!(
            journal.holds_values(),
            "another cluster's empty set hides nothing"
        );
    }

    #[test]
    fn a_pending_clear_or_remove_still_retains_the_confirmed_values() {
        for hide in [ProtectedHeaderChange::Clear, ProtectedHeaderChange::Remove] {
            let mut journal = ProtectedHeaderJournal::default();
            let confirmed = journal
                .propose("c", replace(&[("authorization", "a")]))
                .unwrap();
            journal.accept("c", confirmed);
            let hiding = journal.propose("c", hide).unwrap();
            assert!(
                journal.holds_values(),
                "the confirmed bytes survive until the pending version resolves"
            );
            // Accepting it supersedes the confirmed state: nothing is left.
            let mut accepted = ProtectedHeaderJournal::default();
            let base = accepted
                .propose("c", replace(&[("authorization", "a")]))
                .unwrap();
            accepted.accept("c", base);
            let version = accepted.propose("c", ProtectedHeaderChange::Clear).unwrap();
            accepted.accept("c", version);
            assert!(
                !accepted.holds_values(),
                "an accepted clear drops the values"
            );
            // Rejecting it uncovers the confirmed state, which is still held.
            journal.reject("c", hiding);
            assert!(journal.holds_values());
        }
    }

    #[test]
    fn an_accepted_remove_leaves_nothing_retained() {
        let mut journal = ProtectedHeaderJournal::default();
        let confirmed = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        journal.accept("c", confirmed);
        let removed = journal.propose("c", ProtectedHeaderChange::Remove).unwrap();
        assert!(
            journal.holds_values(),
            "pending remove above confirmed values"
        );
        journal.accept("c", removed);
        assert!(!journal.holds_values());
    }

    #[test]
    fn a_pending_stack_retains_a_non_empty_ancestor_hidden_by_a_later_clear() {
        let mut journal = ProtectedHeaderJournal::default();
        let a = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        let b = journal.propose("c", ProtectedHeaderChange::Clear).unwrap();
        let c = journal.propose("c", ProtectedHeaderChange::Remove).unwrap();
        assert!(
            journal.effective("c").is_none(),
            "the newest pending version is effective"
        );
        assert!(journal.holds_values(), "ancestor A is hidden but retained");
        journal.reject("c", c);
        assert!(journal.holds_values());
        journal.reject("c", b);
        assert!(journal.holds_values(), "A is effective again");
        journal.reject("c", a);
        assert!(!journal.holds_values());
    }

    #[test]
    fn two_pending_non_empty_versions_are_released_one_by_one() {
        let mut journal = ProtectedHeaderJournal::default();
        let a = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        let b = journal
            .propose("c", replace(&[("authorization", "b")]))
            .unwrap();
        journal.reject("c", b);
        assert!(journal.holds_values(), "A remains pending");
        journal.reject("c", a);
        assert!(!journal.holds_values());

        // Accepting the newer supersedes the older, which then retains nothing.
        let a = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        let b = journal
            .propose("c", replace(&[("authorization", "b")]))
            .unwrap();
        journal.accept("c", b);
        assert!(journal.holds_values(), "B is confirmed");
        assert_eq!(journal.accept("c", a), Resolution::Ignored);
        assert!(journal.holds_values());

        // Accepting the older keeps the newer pending.
        let mut journal = ProtectedHeaderJournal::default();
        let a = journal
            .propose("c", replace(&[("authorization", "a")]))
            .unwrap();
        let _b = journal
            .propose("c", replace(&[("authorization", "b")]))
            .unwrap();
        journal.accept("c", a);
        assert!(journal.holds_values());
    }

    #[test]
    fn a_secret_held_by_any_cluster_is_found_among_empty_ones() {
        let mut journal = ProtectedHeaderJournal::default();
        for name in ["a", "b", "c"] {
            let version = journal.propose(name, ProtectedHeaderChange::Clear).unwrap();
            journal.accept(name, version);
        }
        assert!(!journal.holds_values(), "only empty sets");
        let pending = journal.propose("b", ProtectedHeaderChange::Clear).unwrap();
        assert!(!journal.holds_values(), "empty confirmed, empty pending");
        journal
            .propose("z", replace(&[("authorization", "x")]))
            .unwrap();
        assert!(journal.holds_values());
        journal.reject("b", pending);
    }

    // ---- ProtectedTransfer ------------------------------------------------

    const TRANSFER_SENTINEL: &str = "Bearer SYNTHETIC-TRANSFER-SECRET-91ab";

    type Metadata = HashMap<String, Vec<HealthCheckHeader>>;

    fn named(name: &str, value: &str, key: &FingerprintKey) -> ProtectedHeader {
        ProtectedHeader::new(key, name.to_owned(), secret(value.as_bytes()))
    }

    /// Confirm `clusters` (a `None` header list is a cleared cluster) and
    /// return the journal with the metadata a configuration would name.
    fn confirmed(
        key: &FingerprintKey,
        clusters: &[(&str, &[(&str, &str)])],
    ) -> (ProtectedHeaderJournal, Metadata) {
        let mut journal = ProtectedHeaderJournal::default();
        let mut metadata = Metadata::new();
        for (cluster, headers) in clusters {
            let set = ProtectedHeaderSet::new(
                headers
                    .iter()
                    .map(|(name, value)| named(name, value, key))
                    .collect(),
            );
            metadata.insert(
                (*cluster).to_owned(),
                set.headers()
                    .iter()
                    .map(|header| HealthCheckHeader {
                        name: header.name().to_owned(),
                        fingerprint: header.fingerprint().to_string(),
                    })
                    .collect(),
            );
            let version = journal
                .propose(cluster, ProtectedHeaderChange::Replace(set))
                .unwrap();
            journal.accept(cluster, version);
        }
        (journal, metadata)
    }

    fn lookup<'m>(metadata: &'m Metadata) -> impl Fn(&str) -> Option<&'m [HealthCheckHeader]> {
        |cluster| metadata.get(cluster).map(Vec::as_slice)
    }

    fn encode(transfer: &ProtectedTransfer) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(transfer.encoded_len().unwrap()));
        transfer.encode_into(&mut out).unwrap();
        out
    }

    /// The confirmed state `sample` exports: two secret-bearing clusters and
    /// an explicitly empty one, built again for each caller.
    fn sample_state() -> (ProtectedHeaderJournal, Metadata, FingerprintKey) {
        let key = key(7, 9);
        let (journal, metadata) = confirmed(
            &key,
            &[
                (
                    "b",
                    &[("X-One", "one"), ("Authorization", TRANSFER_SENTINEL)],
                ),
                ("a", &[("Authorization", "alpha")]),
                ("empty", &[]),
            ],
        );
        (journal, metadata, key)
    }

    fn sample() -> (ProtectedTransfer, Metadata, FingerprintKey) {
        let (journal, metadata, key) = sample_state();
        let transfer = ProtectedTransfer::export(&journal, Some(&key), lookup(&metadata)).unwrap();
        (transfer, metadata, key)
    }

    #[test]
    fn a_confirmed_transfer_round_trips_key_counter_order_and_empty_sets() {
        let (transfer, metadata, key) = sample();
        let encoded = encode(&transfer);
        assert_eq!(encoded.len(), transfer.encoded_len().unwrap());

        let decoded = ProtectedTransfer::decode(&encoded).unwrap();
        let (restored_key, journal) = decoded.restore(lookup(&metadata)).unwrap();
        let restored_key = restored_key.expect("the key travels");
        assert_eq!(restored_key.epoch, key.epoch);
        assert_eq!(restored_key.key.as_slice(), key.key.as_slice());
        assert_eq!(journal.last_issued, 3, "versions issued are carried");

        let held = journal.effective("b").unwrap();
        let names: Vec<_> = held.headers().iter().map(ProtectedHeader::name).collect();
        assert_eq!(names, ["X-One", "Authorization"], "header order is kept");
        assert_eq!(
            held.headers()[1].value().expose_secret_bytes(),
            TRANSFER_SENTINEL.as_bytes()
        );
        assert!(held.satisfies(&metadata["b"]));
        assert!(journal.effective("empty").unwrap().is_empty());
        assert!(journal.effective("absent").is_none());
        let mut journal = journal;
        assert_eq!(
            journal
                .propose("a", ProtectedHeaderChange::Clear)
                .unwrap()
                .get(),
            4,
            "the next version continues the counter"
        );
    }

    #[test]
    fn a_runtime_without_a_key_transfers_only_its_counter() {
        let journal = ProtectedHeaderJournal::with_last_issued(5);
        let metadata = Metadata::new();
        let transfer = ProtectedTransfer::export(&journal, None, lookup(&metadata)).unwrap();
        let decoded = ProtectedTransfer::decode(&encode(&transfer)).unwrap();
        let (key, journal) = decoded.restore(lookup(&metadata)).unwrap();
        assert!(key.is_none());
        assert_eq!(journal.last_issued, 5);
    }

    #[test]
    fn an_unresolved_version_is_never_exported() {
        let key = key(1, 2);
        let (mut journal, metadata) = confirmed(&key, &[("a", &[("Authorization", "alpha")])]);
        journal
            .propose("a", ProtectedHeaderChange::Clear)
            .expect("a pending proposal");
        assert_eq!(
            ProtectedTransfer::export(&journal, Some(&key), lookup(&metadata)).err(),
            Some(TransferError::Pending)
        );
    }

    #[test]
    fn metadata_that_does_not_name_the_held_set_exactly_is_refused_both_ways() {
        let (transfer, metadata, _) = sample();
        let encoded = encode(&transfer);
        let wrong = |edit: &dyn Fn(&mut Metadata)| {
            let mut changed = metadata.clone();
            edit(&mut changed);
            changed
        };
        let cases = [
            wrong(&|m| {
                m.remove("a");
            }),
            wrong(&|m| m.get_mut("a").unwrap()[0].name = "Other".to_owned()),
            wrong(&|m| m.get_mut("a").unwrap()[0].fingerprint.push('0')),
            wrong(&|m| m.get_mut("b").unwrap().reverse()),
            wrong(&|m| m.get_mut("b").unwrap().truncate(1)),
            wrong(&|m| {
                m.insert(
                    "empty".to_owned(),
                    vec![HealthCheckHeader {
                        name: "X".to_owned(),
                        fingerprint: String::new(),
                    }],
                );
            }),
        ];
        for changed in &cases {
            let decoded = ProtectedTransfer::decode(&encoded).unwrap();
            assert_eq!(
                decoded.restore(lookup(changed)).err(),
                Some(TransferError::Mismatch)
            );
            // Every case edits metadata the exported journal names, so the
            // journal is the whole sample state, not a subset of it.
            let (journal, _, key) = sample_state();
            assert_eq!(
                ProtectedTransfer::export(&journal, Some(&key), lookup(changed)).err(),
                Some(TransferError::Mismatch)
            );
        }
    }

    #[test]
    fn a_tampered_value_or_a_foreign_key_with_the_same_epoch_fails_the_hmac() {
        let (transfer, metadata, _) = sample();
        let mut encoded = encode(&transfer).to_vec();
        // The last byte belongs to the sorted trailing empty cluster, so
        // flipping it corrupts the structure instead of a value. `alpha` is
        // the one value of cluster `a` and occurs exactly once in the
        // encoding; flipping its first byte (`a` -> `` ` ``) keeps a valid
        // value and a decodable layout, and only the HMAC can refuse it.
        let value = b"alpha";
        let at: Vec<usize> = encoded
            .windows(value.len())
            .enumerate()
            .filter(|(_, window)| *window == value)
            .map(|(offset, _)| offset)
            .collect();
        assert_eq!(at.len(), 1, "the value is a unique preimage in the bytes");
        encoded[at[0]] ^= 1;
        let decoded = ProtectedTransfer::decode(&encoded).unwrap();
        assert_eq!(
            decoded.restore(lookup(&metadata)).err(),
            Some(TransferError::Mismatch)
        );

        // Same epoch, different key bytes: the epoch alone is not acceptance.
        // Layout: version at 0, key flag at 1, then the key from offset 2, so
        // offset 2 is the first key byte and the epoch starts at 2 + KEY_LEN.
        let (transfer, metadata, _) = sample();
        let mut encoded = encode(&transfer).to_vec();
        encoded[2] ^= 1;
        let decoded = ProtectedTransfer::decode(&encoded).unwrap();
        assert_eq!(
            decoded.restore(lookup(&metadata)).err(),
            Some(TransferError::Mismatch)
        );
    }

    #[test]
    fn every_truncation_and_any_trailing_byte_is_refused() {
        let (transfer, _, _) = sample();
        let encoded = encode(&transfer);
        for cut in 0..encoded.len() {
            assert!(
                ProtectedTransfer::decode(&encoded[..cut]).is_err(),
                "a {cut}-byte prefix was accepted"
            );
        }
        assert_eq!(
            ProtectedTransfer::decode(&encoded[..encoded.len() - 1]).err(),
            Some(TransferError::Truncated)
        );
        let mut longer = encoded.to_vec();
        longer.push(0);
        assert_eq!(
            ProtectedTransfer::decode(&longer).err(),
            Some(TransferError::Trailing)
        );
    }

    #[test]
    fn oversized_counts_lengths_and_blocks_are_refused_before_allocating() {
        let prefix = |clusters: u32| {
            let mut bytes = vec![TRANSFER_LAYOUT_VERSION, 0];
            bytes.extend_from_slice(&0u64.to_be_bytes());
            bytes.extend_from_slice(&clusters.to_be_bytes());
            bytes
        };
        let too_many_clusters = prefix(TRANSFER_MAX_CLUSTERS as u32 + 1);
        assert_eq!(
            ProtectedTransfer::decode(&too_many_clusters).err(),
            Some(TransferError::TooLarge)
        );
        let mut long_id = prefix(1);
        long_id.extend_from_slice(&(TRANSFER_MAX_TEXT as u32 + 1).to_be_bytes());
        assert_eq!(
            ProtectedTransfer::decode(&long_id).err(),
            Some(TransferError::TooLarge)
        );
        let mut many_headers = prefix(1);
        many_headers.extend_from_slice(&1u32.to_be_bytes());
        many_headers.push(b'a');
        many_headers
            .extend_from_slice(&(crate::config::HEALTH_CHECK_MAX_HEADERS as u32 + 1).to_be_bytes());
        assert_eq!(
            ProtectedTransfer::decode(&many_headers).err(),
            Some(TransferError::TooLarge)
        );
        let mut long_value = prefix(1);
        long_value.extend_from_slice(&1u32.to_be_bytes());
        long_value.push(b'a');
        long_value.extend_from_slice(&1u32.to_be_bytes());
        long_value.extend_from_slice(&1u32.to_be_bytes());
        long_value.push(b'n');
        long_value.extend_from_slice(
            &(crate::config::HEALTH_CHECK_MAX_HEADER_BYTES as u32 + 1).to_be_bytes(),
        );
        assert_eq!(
            ProtectedTransfer::decode(&long_value).err(),
            Some(TransferError::TooLarge)
        );
        let block = vec![0u8; PROTECTED_TRANSFER_MAX_BYTES + 1];
        assert_eq!(
            ProtectedTransfer::decode(&block).err(),
            Some(TransferError::TooLarge)
        );
    }

    #[test]
    fn malformed_text_order_flags_and_values_are_refused() {
        let (transfer, _, _) = sample();
        let encoded = encode(&transfer).to_vec();
        // Flag byte other than 0 or 1.
        let mut flag = encoded.clone();
        flag[1] = 2;
        assert_eq!(
            ProtectedTransfer::decode(&flag).err(),
            Some(TransferError::Malformed)
        );
        // Layout version.
        let mut version = encoded.clone();
        version[0] = 9;
        assert_eq!(
            ProtectedTransfer::decode(&version).err(),
            Some(TransferError::Malformed)
        );
        // A cluster id that is not UTF-8: first id byte sits after
        // version, flag, key, epoch, counter and count and its own length.
        let mut utf8 = encoded.clone();
        utf8[2 + KEY_LEN + EPOCH_LEN + 8 + 4 + 4] = 0xff;
        assert_eq!(
            ProtectedTransfer::decode(&utf8).err(),
            Some(TransferError::Malformed)
        );
        // Clusters out of order (or duplicated): "a" then "a".
        let mut twice = vec![TRANSFER_LAYOUT_VERSION, 0];
        twice.extend_from_slice(&0u64.to_be_bytes());
        twice.extend_from_slice(&2u32.to_be_bytes());
        for _ in 0..2 {
            twice.extend_from_slice(&1u32.to_be_bytes());
            twice.push(b'a');
            twice.extend_from_slice(&0u32.to_be_bytes());
        }
        assert_eq!(
            ProtectedTransfer::decode(&twice).err(),
            Some(TransferError::Malformed)
        );
        // A value with a forbidden control byte.
        let mut control = vec![TRANSFER_LAYOUT_VERSION, 0];
        control.extend_from_slice(&0u64.to_be_bytes());
        control.extend_from_slice(&1u32.to_be_bytes());
        control.extend_from_slice(&1u32.to_be_bytes());
        control.push(b'a');
        control.extend_from_slice(&1u32.to_be_bytes());
        control.extend_from_slice(&1u32.to_be_bytes());
        control.push(b'n');
        control.extend_from_slice(&1u32.to_be_bytes());
        control.push(0);
        assert_eq!(
            ProtectedTransfer::decode(&control).err(),
            Some(TransferError::Malformed)
        );
    }

    #[test]
    fn encoding_never_grows_its_buffer_and_nothing_prints_a_value_or_the_key() {
        let (transfer, _, _) = sample();
        let mut small = Zeroizing::new(Vec::with_capacity(transfer.encoded_len().unwrap() - 1));
        let capacity = small.capacity();
        assert_eq!(
            transfer.encode_into(&mut small).err(),
            Some(TransferError::BufferTooSmall)
        );
        assert_eq!((small.len(), small.capacity()), (0, capacity));

        let rendered = format!("{transfer:?}");
        assert!(!rendered.contains(TRANSFER_SENTINEL), "{rendered}");
        assert!(!rendered.contains(&hex::encode([7u8; 32])), "{rendered}");
        for error in [
            TransferError::Pending,
            TransferError::KeyMissing,
            TransferError::Mismatch,
            TransferError::TooLarge,
            TransferError::Truncated,
            TransferError::Trailing,
            TransferError::Malformed,
            TransferError::BufferTooSmall,
        ] {
            assert!(!error.to_string().contains(TRANSFER_SENTINEL));
        }
    }
}
