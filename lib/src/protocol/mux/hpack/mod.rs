//! HPACK, the header compression of HTTP/2 (RFC 7541), with no I/O.
//!
//! The decoder reads a complete header block from a `&[u8]` and hands each
//! field to a callback; the encoder appends to a `Vec<u8>` the caller owns.
//! Neither reads a socket, a clock or anything but its arguments and its own
//! tables, so both are driven byte-in, byte-out like the rest of the H2 core.
//!
//! # Allocations
//!
//! The hot path allocates nothing once a connection is warm:
//!
//! - Huffman decoding reads a state machine computed at compile time
//!   ([`huffman`]) and writes into the decoder's scratch buffer;
//! - a string sent without Huffman coding is borrowed from the block;
//! - a field found in a table is borrowed from the table;
//! - the dynamic table keeps all its entries in one byte buffer plus one ring
//!   of offsets, both grown to a high-water mark bounded by the table size.
//!
//! The only allocations are the growth of those three buffers — the scratch
//! buffer, the table bytes and the offset ring — up to their high-water mark.
//!
//! # Guards on untrusted input
//!
//! Every malformed block is a [`DecoderError`], never a panic:
//!
//! - an integer longer than five octets ([`DecoderError::IntegerOverflow`],
//!   §5.1), or a block ending inside an integer or a string
//!   ([`DecoderError::Truncated`]);
//! - index 0 or an index past both tables ([`DecoderError::InvalidIndex`],
//!   §2.3.3);
//! - a size update above the advertised `SETTINGS_HEADER_TABLE_SIZE`
//!   ([`DecoderError::TableSizeTooLarge`], §6.3), after a field
//!   ([`DecoderError::SizeUpdateNotAtStart`], §4.2), more than two of them
//!   ([`DecoderError::TooManySizeUpdates`]) or ending the block
//!   ([`DecoderError::SizeUpdateAtEnd`]);
//! - EOS inside a Huffman string, padding that is not all ones, or longer than
//!   seven bits ([`HuffmanError`], §5.2).
//!
//! The decoded header-list size (RFC 9113 §6.5.2) is the caller's to enforce:
//! `decode_headers_with_budget` (`lib/src/protocol/mux/pkawa.rs`) counts every
//! field the callback receives. The decoder still decodes the whole block past
//! that budget, because the dynamic table must stay in step with the peer's
//! encoder.
//!
//! # Provenance
//!
//! Written from RFC 7541. `table::STATIC_TABLE` is Appendix A and
//! `huffman::CODES` is Appendix B, both transcribed from the RFC text; the
//! code table was also checked, octet by octet, against HAProxy's `ht` table
//! (`src/hpack-huff.c`). No code comes from `loona-hpack` or `hpack-rs`, which
//! this module replaces; the encoder only reproduces the representation policy
//! `loona-hpack` applied, so that the bytes sozu sends do not change.

mod decoder;
mod encoder;
pub mod huffman;
mod table;

#[cfg(test)]
mod tests;

pub use decoder::Decoder;
pub use encoder::{Encoder, Representation};
pub use huffman::HuffmanError;

/// Why a header block could not be decoded. Every variant is a connection
/// error of type COMPRESSION_ERROR (RFC 9113 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecoderError {
    /// The block ends inside an integer or a string.
    #[error("header block truncated")]
    Truncated,
    /// An integer takes more than five octets (RFC 7541 §5.1).
    #[error("integer encoded on more than five octets")]
    IntegerOverflow,
    /// Index 0, or an index past the static and dynamic tables (§2.3.3).
    #[error("index outside the static and dynamic tables")]
    InvalidIndex,
    /// A Huffman string is invalid (§5.2).
    #[error("invalid Huffman string: {0}")]
    Huffman(HuffmanError),
    /// A size update exceeds the advertised `SETTINGS_HEADER_TABLE_SIZE`
    /// (§6.3).
    #[error("dynamic table size update above SETTINGS_HEADER_TABLE_SIZE")]
    TableSizeTooLarge,
    /// A size update follows a field in the same block (§4.2).
    #[error("dynamic table size update after a header field")]
    SizeUpdateNotAtStart,
    /// More than two size updates open the block (§4.2).
    #[error("more than two dynamic table size updates")]
    TooManySizeUpdates,
    /// The block ends with a size update.
    #[error("dynamic table size update at the end of a header block")]
    SizeUpdateAtEnd,
}

/// Octets an integer may take: its prefix octet and four continuation octets,
/// enough for 2^28 plus the prefix maximum, far above any length, index or
/// table size a block can carry.
const MAX_INTEGER_OCTETS: usize = 5;

/// Decodes the integer (§5.1) whose `prefix_bits`-bit prefix starts `input`,
/// and returns it with the number of octets it takes.
fn decode_integer(input: &[u8], prefix_bits: u8) -> Result<(usize, usize), DecoderError> {
    debug_assert!(
        (1..=8).contains(&prefix_bits),
        "an HPACK prefix is 1 to 8 bits"
    );
    let Some(&first) = input.first() else {
        return Err(DecoderError::Truncated);
    };
    let mask = ((1u16 << prefix_bits) - 1) as u8;
    let mut value = usize::from(first & mask);
    if value < usize::from(mask) {
        return Ok((value, 1));
    }
    for (position, &octet) in input.iter().enumerate().skip(1) {
        if position >= MAX_INTEGER_OCTETS {
            return Err(DecoderError::IntegerOverflow);
        }
        // At most four continuation octets of 7 bits: the sum stays below
        // 2^28 + 255 and cannot overflow a 32-bit `usize`.
        value += usize::from(octet & 0x7f) << (7 * (position - 1));
        if octet & 0x80 == 0 {
            debug_assert!(position < MAX_INTEGER_OCTETS, "the octet bound holds");
            return Ok((value, position + 1));
        }
    }
    Err(DecoderError::Truncated)
}

/// Appends `value` as an integer with a `prefix_bits`-bit prefix (§5.1), the
/// high bits of the first octet set to `flags`.
pub fn encode_integer(value: usize, prefix_bits: u8, flags: u8, out: &mut Vec<u8>) {
    debug_assert!(
        (1..=8).contains(&prefix_bits),
        "an HPACK prefix is 1 to 8 bits"
    );
    let mask = ((1u16 << prefix_bits) - 1) as u8;
    debug_assert!(flags & mask == 0, "flags do not overlap the prefix");
    if value < usize::from(mask) {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask);
    let mut rest = value - usize::from(mask);
    while rest >= 0x80 {
        out.push((rest & 0x7f) as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}
