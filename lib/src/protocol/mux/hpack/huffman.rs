//! Huffman code of RFC 7541 §5.2 and Appendix B, with no allocation.
//!
//! `CODES` is Appendix B transcribed: the code of each of the 256 octets
//! plus EOS (256), right-aligned, and its length in bits. Nothing else here is
//! data: the decoding state machine is computed from `CODES` at compile time
//! by `build_machine`, so the table the decoder reads is `static` read-only
//! memory and a decode never builds or allocates anything.
//!
//! # The decoding state machine
//!
//! The code is a complete prefix code of 257 leaves, hence a full binary tree
//! with exactly 256 internal nodes. A decoder state is an internal node: the
//! bits read since the last emitted symbol. The input is consumed four bits at
//! a time, so `Machine::transitions` holds 256 × 16 entries, each giving the
//! next state, whether a symbol completed inside those four bits, and whether
//! EOS was reached. The shortest code is five bits long, so a nibble completes
//! at most one symbol — `build_machine` asserts it. This is the layout
//! nghttp2 uses (`lib/nghttp2_hd_huffman.c` and the `mkhufftbl.py` generator
//! that writes its `huff_decode_table`), except that the table is produced by
//! `const` evaluation instead of a script whose output is committed.
//!
//! HAProxy (`src/hpack-huff.c`, `huff_dec`) takes the other classic route: it
//! looks up whole bytes of an MSB-aligned 32-bit window in several reversed
//! tables split by code length. It reads fewer entries per symbol but needs
//! hand-simplified tables; the nibble machine is derived mechanically from
//! Appendix B, which is what lets it be a `const fn`.
//!
//! # Padding and EOS (RFC 7541 §5.2)
//!
//! When the input ends, the state holds the bits read after the last symbol.
//! They are valid padding only when they are the most significant bits of EOS
//! (all ones) and at most seven of them. `Machine::padding` records, for the
//! states on the all-ones path from the root, how many ones lead to them; every
//! other state is marked `NOT_PADDING`. EOS itself is thirty ones: reaching
//! its leaf inside a string is an error.

/// RFC 7541 Appendix B: `(code, length in bits)` for octets 0 to 255 and EOS.
#[rustfmt::skip]
pub(super) const CODES: [(u32, u8); 257] = [
    (0x00001ff8, 13),
    (0x007fffd8, 23),
    (0x0fffffe2, 28),
    (0x0fffffe3, 28),
    (0x0fffffe4, 28),
    (0x0fffffe5, 28),
    (0x0fffffe6, 28),
    (0x0fffffe7, 28),
    (0x0fffffe8, 28),
    (0x00ffffea, 24),
    (0x3ffffffc, 30),
    (0x0fffffe9, 28),
    (0x0fffffea, 28),
    (0x3ffffffd, 30),
    (0x0fffffeb, 28),
    (0x0fffffec, 28),
    (0x0fffffed, 28),
    (0x0fffffee, 28),
    (0x0fffffef, 28),
    (0x0ffffff0, 28),
    (0x0ffffff1, 28),
    (0x0ffffff2, 28),
    (0x3ffffffe, 30),
    (0x0ffffff3, 28),
    (0x0ffffff4, 28),
    (0x0ffffff5, 28),
    (0x0ffffff6, 28),
    (0x0ffffff7, 28),
    (0x0ffffff8, 28),
    (0x0ffffff9, 28),
    (0x0ffffffa, 28),
    (0x0ffffffb, 28),
    (0x00000014,  6),
    (0x000003f8, 10),
    (0x000003f9, 10),
    (0x00000ffa, 12),
    (0x00001ff9, 13),
    (0x00000015,  6),
    (0x000000f8,  8),
    (0x000007fa, 11),
    (0x000003fa, 10),
    (0x000003fb, 10),
    (0x000000f9,  8),
    (0x000007fb, 11),
    (0x000000fa,  8),
    (0x00000016,  6),
    (0x00000017,  6),
    (0x00000018,  6),
    (0x00000000,  5),
    (0x00000001,  5),
    (0x00000002,  5),
    (0x00000019,  6),
    (0x0000001a,  6),
    (0x0000001b,  6),
    (0x0000001c,  6),
    (0x0000001d,  6),
    (0x0000001e,  6),
    (0x0000001f,  6),
    (0x0000005c,  7),
    (0x000000fb,  8),
    (0x00007ffc, 15),
    (0x00000020,  6),
    (0x00000ffb, 12),
    (0x000003fc, 10),
    (0x00001ffa, 13),
    (0x00000021,  6),
    (0x0000005d,  7),
    (0x0000005e,  7),
    (0x0000005f,  7),
    (0x00000060,  7),
    (0x00000061,  7),
    (0x00000062,  7),
    (0x00000063,  7),
    (0x00000064,  7),
    (0x00000065,  7),
    (0x00000066,  7),
    (0x00000067,  7),
    (0x00000068,  7),
    (0x00000069,  7),
    (0x0000006a,  7),
    (0x0000006b,  7),
    (0x0000006c,  7),
    (0x0000006d,  7),
    (0x0000006e,  7),
    (0x0000006f,  7),
    (0x00000070,  7),
    (0x00000071,  7),
    (0x00000072,  7),
    (0x000000fc,  8),
    (0x00000073,  7),
    (0x000000fd,  8),
    (0x00001ffb, 13),
    (0x0007fff0, 19),
    (0x00001ffc, 13),
    (0x00003ffc, 14),
    (0x00000022,  6),
    (0x00007ffd, 15),
    (0x00000003,  5),
    (0x00000023,  6),
    (0x00000004,  5),
    (0x00000024,  6),
    (0x00000005,  5),
    (0x00000025,  6),
    (0x00000026,  6),
    (0x00000027,  6),
    (0x00000006,  5),
    (0x00000074,  7),
    (0x00000075,  7),
    (0x00000028,  6),
    (0x00000029,  6),
    (0x0000002a,  6),
    (0x00000007,  5),
    (0x0000002b,  6),
    (0x00000076,  7),
    (0x0000002c,  6),
    (0x00000008,  5),
    (0x00000009,  5),
    (0x0000002d,  6),
    (0x00000077,  7),
    (0x00000078,  7),
    (0x00000079,  7),
    (0x0000007a,  7),
    (0x0000007b,  7),
    (0x00007ffe, 15),
    (0x000007fc, 11),
    (0x00003ffd, 14),
    (0x00001ffd, 13),
    (0x0ffffffc, 28),
    (0x000fffe6, 20),
    (0x003fffd2, 22),
    (0x000fffe7, 20),
    (0x000fffe8, 20),
    (0x003fffd3, 22),
    (0x003fffd4, 22),
    (0x003fffd5, 22),
    (0x007fffd9, 23),
    (0x003fffd6, 22),
    (0x007fffda, 23),
    (0x007fffdb, 23),
    (0x007fffdc, 23),
    (0x007fffdd, 23),
    (0x007fffde, 23),
    (0x00ffffeb, 24),
    (0x007fffdf, 23),
    (0x00ffffec, 24),
    (0x00ffffed, 24),
    (0x003fffd7, 22),
    (0x007fffe0, 23),
    (0x00ffffee, 24),
    (0x007fffe1, 23),
    (0x007fffe2, 23),
    (0x007fffe3, 23),
    (0x007fffe4, 23),
    (0x001fffdc, 21),
    (0x003fffd8, 22),
    (0x007fffe5, 23),
    (0x003fffd9, 22),
    (0x007fffe6, 23),
    (0x007fffe7, 23),
    (0x00ffffef, 24),
    (0x003fffda, 22),
    (0x001fffdd, 21),
    (0x000fffe9, 20),
    (0x003fffdb, 22),
    (0x003fffdc, 22),
    (0x007fffe8, 23),
    (0x007fffe9, 23),
    (0x001fffde, 21),
    (0x007fffea, 23),
    (0x003fffdd, 22),
    (0x003fffde, 22),
    (0x00fffff0, 24),
    (0x001fffdf, 21),
    (0x003fffdf, 22),
    (0x007fffeb, 23),
    (0x007fffec, 23),
    (0x001fffe0, 21),
    (0x001fffe1, 21),
    (0x003fffe0, 22),
    (0x001fffe2, 21),
    (0x007fffed, 23),
    (0x003fffe1, 22),
    (0x007fffee, 23),
    (0x007fffef, 23),
    (0x000fffea, 20),
    (0x003fffe2, 22),
    (0x003fffe3, 22),
    (0x003fffe4, 22),
    (0x007ffff0, 23),
    (0x003fffe5, 22),
    (0x003fffe6, 22),
    (0x007ffff1, 23),
    (0x03ffffe0, 26),
    (0x03ffffe1, 26),
    (0x000fffeb, 20),
    (0x0007fff1, 19),
    (0x003fffe7, 22),
    (0x007ffff2, 23),
    (0x003fffe8, 22),
    (0x01ffffec, 25),
    (0x03ffffe2, 26),
    (0x03ffffe3, 26),
    (0x03ffffe4, 26),
    (0x07ffffde, 27),
    (0x07ffffdf, 27),
    (0x03ffffe5, 26),
    (0x00fffff1, 24),
    (0x01ffffed, 25),
    (0x0007fff2, 19),
    (0x001fffe3, 21),
    (0x03ffffe6, 26),
    (0x07ffffe0, 27),
    (0x07ffffe1, 27),
    (0x03ffffe7, 26),
    (0x07ffffe2, 27),
    (0x00fffff2, 24),
    (0x001fffe4, 21),
    (0x001fffe5, 21),
    (0x03ffffe8, 26),
    (0x03ffffe9, 26),
    (0x0ffffffd, 28),
    (0x07ffffe3, 27),
    (0x07ffffe4, 27),
    (0x07ffffe5, 27),
    (0x000fffec, 20),
    (0x00fffff3, 24),
    (0x000fffed, 20),
    (0x001fffe6, 21),
    (0x003fffe9, 22),
    (0x001fffe7, 21),
    (0x001fffe8, 21),
    (0x007ffff3, 23),
    (0x003fffea, 22),
    (0x003fffeb, 22),
    (0x01ffffee, 25),
    (0x01ffffef, 25),
    (0x00fffff4, 24),
    (0x00fffff5, 24),
    (0x03ffffea, 26),
    (0x007ffff4, 23),
    (0x03ffffeb, 26),
    (0x07ffffe6, 27),
    (0x03ffffec, 26),
    (0x03ffffed, 26),
    (0x07ffffe7, 27),
    (0x07ffffe8, 27),
    (0x07ffffe9, 27),
    (0x07ffffea, 27),
    (0x07ffffeb, 27),
    (0x0ffffffe, 28),
    (0x07ffffec, 27),
    (0x07ffffed, 27),
    (0x07ffffee, 27),
    (0x07ffffef, 27),
    (0x07fffff0, 27),
    (0x03ffffee, 26),
    (0x3fffffff, 30),
];

/// Index of EOS in [`CODES`].
const EOS: usize = 256;

/// A Huffman string that RFC 7541 §5.2 requires the decoder to reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HuffmanError {
    /// The string contains the EOS symbol.
    #[error("EOS symbol inside a Huffman string")]
    Eos,
    /// The bits after the last symbol are not a prefix of EOS (all ones).
    #[error("Huffman padding is not a prefix of EOS")]
    InvalidPadding,
    /// The bits after the last symbol are all ones but longer than 7 bits.
    #[error("Huffman padding longer than 7 bits")]
    PaddingTooLong,
}

/// [`Transition::flags`] bit: a symbol completed inside this nibble.
const EMIT: u8 = 1;
/// [`Transition::flags`] bit: EOS was reached inside this nibble.
const FAIL: u8 = 2;
/// [`Machine::padding`] value of a state that is not on the all-ones path.
const NOT_PADDING: u8 = u8::MAX;

#[derive(Clone, Copy)]
struct Transition {
    next: u8,
    flags: u8,
    symbol: u8,
}

struct Machine {
    /// `transitions[state][nibble]`.
    transitions: [[Transition; 16]; 256],
    /// Number of one bits leading from the root to each state, when the path
    /// is only ones; [`NOT_PADDING`] otherwise.
    padding: [u8; 256],
}

/// The decoding machine, computed at compile time from [`CODES`].
static MACHINE: Machine = build_machine();

/// Builds the decoding tree of [`CODES`], then the nibble transitions.
///
/// Every `assert!` here runs during `const` evaluation: a table that is not a
/// complete prefix code, or a nibble completing two symbols, fails the build
/// instead of producing a wrong decoder.
const fn build_machine() -> Machine {
    // Children of each internal node: an internal node id below 256, or
    // `LEAF + symbol` for a leaf, or `NONE` while the tree is being built.
    const NONE: u16 = u16::MAX;
    const LEAF: u16 = 256;
    let mut children = [[NONE; 2]; 256];
    let mut internal = 1usize;
    let mut symbol = 0usize;
    while symbol < CODES.len() {
        let (code, length) = CODES[symbol];
        let mut node = 0usize;
        let mut bit = length;
        while bit > 1 {
            bit -= 1;
            let branch = ((code >> bit) & 1) as usize;
            let child = children[node][branch];
            if child == NONE {
                assert!(internal < 256, "more than 256 internal nodes");
                children[node][branch] = internal as u16;
                node = internal;
                internal += 1;
            } else {
                assert!(child < LEAF, "a code is a prefix of another");
                node = child as usize;
            }
        }
        let branch = (code & 1) as usize;
        assert!(children[node][branch] == NONE, "two codes are equal");
        children[node][branch] = LEAF + symbol as u16;
        symbol += 1;
    }
    assert!(internal == 256, "the code is not complete");

    let mut padding = [NOT_PADDING; 256];
    padding[0] = 0;
    let mut node = 0usize;
    let mut depth = 0u8;
    while children[node][1] < LEAF {
        node = children[node][1] as usize;
        depth += 1;
        padding[node] = depth;
    }
    assert!(
        children[node][1] == LEAF + EOS as u16,
        "EOS is not all ones"
    );

    let empty = Transition {
        next: 0,
        flags: 0,
        symbol: 0,
    };
    let mut transitions = [[empty; 16]; 256];
    let mut state = 0usize;
    while state < 256 {
        let mut nibble = 0usize;
        while nibble < 16 {
            let mut node = state;
            let mut flags = 0u8;
            let mut emitted = 0u8;
            let mut bit = 4;
            while bit > 0 {
                bit -= 1;
                let child = children[node][(nibble >> bit) & 1];
                if child >= LEAF {
                    let symbol = (child - LEAF) as usize;
                    if symbol == EOS {
                        flags = FAIL;
                        node = 0;
                        break;
                    }
                    assert!(flags & EMIT == 0, "a nibble completes two symbols");
                    flags |= EMIT;
                    emitted = symbol as u8;
                    node = 0;
                } else {
                    node = child as usize;
                }
            }
            transitions[state][nibble] = Transition {
                next: node as u8,
                flags,
                symbol: emitted,
            };
            nibble += 1;
        }
        state += 1;
    }
    Machine {
        transitions,
        padding,
    }
}

/// Upper bound of the decoded length of `encoded_len` Huffman octets: every
/// code is at least five bits long.
pub(super) const fn max_decoded_len(encoded_len: usize) -> usize {
    encoded_len / 5 * 8 + 8
}

/// Decodes `src` and appends the octets to `out`.
///
/// Reserves `max_decoded_len` once, so `out` is never reallocated inside
/// the loop, and not at all when its capacity already suffices. On error,
/// `out` may hold a partial output past its original length; the caller
/// truncates it.
pub fn decode(src: &[u8], out: &mut Vec<u8>) -> Result<(), HuffmanError> {
    out.reserve(max_decoded_len(src.len()));
    let mut state = 0usize;
    for &byte in src {
        for nibble in [byte >> 4, byte & 0x0f] {
            let transition = MACHINE.transitions[state][nibble as usize];
            if transition.flags & FAIL != 0 {
                return Err(HuffmanError::Eos);
            }
            if transition.flags & EMIT != 0 {
                out.push(transition.symbol);
            }
            state = transition.next as usize;
        }
    }
    match MACHINE.padding[state] {
        NOT_PADDING => Err(HuffmanError::InvalidPadding),
        ones if ones > 7 => Err(HuffmanError::PaddingTooLong),
        _ => Ok(()),
    }
}

/// Length in octets of the Huffman encoding of `src`, padding included.
pub fn encoded_len(src: &[u8]) -> usize {
    let bits: usize = src
        .iter()
        .map(|&byte| usize::from(CODES[byte as usize].1))
        .sum();
    bits.div_ceil(8)
}

/// Appends the Huffman encoding of `src` to `out`, padded with the most
/// significant bits of EOS (RFC 7541 §5.2).
pub fn encode(src: &[u8], out: &mut Vec<u8>) {
    out.reserve(encoded_len(src));
    // At most 7 pending bits plus one 30-bit code: 37 bits fit in a u64.
    let mut pending: u64 = 0;
    let mut bits: u32 = 0;
    for &byte in src {
        let (code, length) = CODES[byte as usize];
        pending = (pending << length) | u64::from(code);
        bits += u32::from(length);
        while bits >= 8 {
            bits -= 8;
            out.push((pending >> bits) as u8);
        }
        pending &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(((pending << (8 - bits)) as u8) | (0xff >> bits));
    }
}
