//! HPACK encoder (RFC 7541 §6) writing into a buffer the caller owns.
//!
//! [`Encoder::encode_header_into`] applies the representation policy sozu has
//! always sent — the one `loona-hpack` 0.4.3 applied, reproduced here as
//! behavior so that the bytes on the wire do not change:
//!
//! - name and value found in a table: Indexed Header Field (§6.1);
//! - only the name found: Literal without Indexing, indexed name (§6.2.2),
//!   naming the *last* entry holding that name when the static table is
//!   scanned, then the dynamic table from its newest entry — `:method: PUT`
//!   names index 3 (`:method: POST`), `:status: 201` names index 14;
//! - neither found: Literal with Incremental Indexing, new name (§6.2.1), and
//!   the field enters the dynamic table.
//!
//! Strings are sent without Huffman coding. The dynamic table only ever holds
//! fields whose name is in no table, so the peer's decoder sees at most one
//! entry per name.

use super::{
    encode_integer, huffman,
    table::{DEFAULT_MAX_SIZE, DYNAMIC_TABLE_START, DynamicTable, STATIC_TABLE},
};

/// The representation [`Encoder::encode_field`] emits. Sozu itself only sends
/// [`Representation::Proxy`]; the others reproduce RFC 7541 Appendix C and let
/// a test peer or a fuzz target produce every representation a decoder meets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Representation {
    /// The policy of [`Encoder::encode_header_into`].
    Proxy,
    /// §6.2.1, or §6.1 when name and value are found.
    IncrementalIndexing,
    /// §6.2.2, never §6.1.
    WithoutIndexing,
    /// §6.2.3, never §6.1.
    NeverIndexed,
}

/// What a table search found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    Field(usize),
    /// The first and the last index holding the name. [`Representation::Proxy`]
    /// names the last one, as `loona-hpack` did; the others name the first,
    /// as RFC 7541 Appendix C does.
    Name {
        first: usize,
        last: usize,
    },
    Nothing,
}

/// The table-size changes the peer's decoder has not been told about yet.
///
/// RFC 7541 §4.2: when the maximum size changes more than once between two
/// blocks, the next block opens with the smallest size reached, then the final
/// size. The smallest one is what decides which entries this encoder evicted;
/// the final one is what both tables hold from then on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingSizeUpdate {
    /// The smallest maximum size set since the last size update was encoded.
    smallest: usize,
    /// The maximum size set last.
    last: usize,
}

/// The connection-level HPACK encoder of one direction.
#[derive(Debug)]
pub struct Encoder {
    table: DynamicTable,
    /// Set by [`Encoder::change_max_table_size`], emptied by
    /// [`Encoder::encode_size_updates_into`].
    pending_size_update: Option<PendingSizeUpdate>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// An encoder with the default 4096-octet table. Allocates nothing.
    pub const fn new() -> Self {
        Encoder {
            table: DynamicTable::new(DEFAULT_MAX_SIZE),
            pending_size_update: None,
        }
    }

    /// Sets the maximum size of the table, at most the peer's
    /// `SETTINGS_HEADER_TABLE_SIZE`, and evicts until it fits (§4.3). The
    /// caller signals the change to the peer with a size update (§6.3) at the
    /// start of its next block; [`Self::change_max_table_size`] records it for
    /// [`Self::encode_size_updates_into`] instead.
    pub fn set_max_table_size(&mut self, max_size: usize) {
        self.table.set_max_size(max_size);
    }

    /// Sets the maximum size like [`Self::set_max_table_size`] and records the
    /// change for the size updates that open the next block (§4.2): the
    /// smallest size set since the last block, and the last one.
    pub fn change_max_table_size(&mut self, max_size: usize) {
        self.table.set_max_size(max_size);
        let smallest = self
            .pending_size_update
            .map_or(max_size, |pending| pending.smallest.min(max_size));
        self.pending_size_update = Some(PendingSizeUpdate {
            smallest,
            last: max_size,
        });
        debug_assert!(
            smallest <= max_size,
            "the smallest size reached never exceeds the last one"
        );
        debug_assert!(
            self.pending_size_update
                .is_some_and(|pending| pending.last == max_size),
            "the change is recorded as the last one"
        );
    }

    /// Appends the size updates (§6.3) that open a block after the maximum
    /// size changed to `last_size`, and forgets the changes recorded by
    /// [`Self::change_max_table_size`].
    ///
    /// When the size went below `last_size` since the last block, the
    /// smallest size goes first (§4.2): the peer's decoder must evict what
    /// this encoder evicted, or the two tables no longer hold the same
    /// entries. `last_size` follows, alone when nothing smaller was set.
    /// Returns the number of updates appended, 1 or 2 — the most a block may
    /// open with.
    pub fn encode_size_updates_into(&mut self, last_size: usize, out: &mut Vec<u8>) -> usize {
        let pending = self.pending_size_update.take();
        debug_assert!(
            pending.is_none_or(|pending| pending.last == last_size),
            "the size the caller signals is the last one recorded"
        );
        let start = out.len();
        let mut updates = 0;
        if let Some(pending) = pending
            && pending.smallest < last_size
        {
            encode_integer(pending.smallest, 5, 0x20, out);
            updates += 1;
        }
        encode_integer(last_size, 5, 0x20, out);
        updates += 1;
        debug_assert!(
            (1..=2).contains(&updates),
            "a block opens with at most two size updates (§4.2)"
        );
        debug_assert!(
            out[start] & 0xe0 == 0x20 && self.pending_size_update.is_none(),
            "the appended bytes open with a size update, and nothing stays recorded"
        );
        updates
    }

    #[cfg(test)]
    pub(super) fn table(&self) -> &DynamicTable {
        &self.table
    }

    /// Appends the representation of one field to `out`.
    pub fn encode_header_into(&mut self, (name, value): (&[u8], &[u8]), out: &mut Vec<u8>) {
        self.encode_field(name, value, Representation::Proxy, false, out);
    }

    /// Appends the representation of every field to `out`, in order.
    pub fn encode_into<'a, I>(&mut self, headers: I, out: &mut Vec<u8>)
    where
        I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
    {
        for header in headers {
            self.encode_header_into(header, out);
        }
    }

    /// Encodes `headers` into a new block.
    #[cfg(test)]
    pub fn encode<'a, I>(&mut self, headers: I) -> Vec<u8>
    where
        I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
    {
        let mut out = Vec::new();
        self.encode_into(headers, &mut out);
        out
    }

    /// Appends one field in the requested representation, the strings Huffman
    /// coded when `huffman`.
    pub fn encode_field(
        &mut self,
        name: &[u8],
        value: &[u8],
        representation: Representation,
        huffman: bool,
        out: &mut Vec<u8>,
    ) {
        let found = self.find(name, value);
        match (representation, found) {
            (Representation::Proxy | Representation::IncrementalIndexing, Found::Field(index)) => {
                encode_integer(index, 7, 0x80, out);
            }
            (Representation::Proxy, Found::Name { last, .. }) => {
                encode_integer(last, 4, 0x00, out);
                encode_string(value, huffman, out);
            }
            (Representation::Proxy | Representation::IncrementalIndexing, Found::Nothing) => {
                out.push(0x40);
                encode_string(name, huffman, out);
                encode_string(value, huffman, out);
                self.table.insert(name, value);
            }
            (Representation::IncrementalIndexing, Found::Name { first, .. }) => {
                encode_integer(first, 6, 0x40, out);
                encode_string(value, huffman, out);
                self.table.insert(name, value);
            }
            (Representation::WithoutIndexing | Representation::NeverIndexed, found) => {
                let flags = if representation == Representation::NeverIndexed {
                    0x10
                } else {
                    0x00
                };
                match found {
                    Found::Field(index) | Found::Name { first: index, .. } => {
                        encode_integer(index, 4, flags, out);
                    }
                    Found::Nothing => {
                        out.push(flags);
                        encode_string(name, huffman, out);
                    }
                }
                encode_string(value, huffman, out);
            }
        }
    }

    /// Searches the static table, then the dynamic table from its newest
    /// entry: the first index holding both name and value, else the first and
    /// the last index holding the name.
    fn find(&self, name: &[u8], value: &[u8]) -> Found {
        let entries = DYNAMIC_TABLE_START + self.table.len();
        let mut found = Found::Nothing;
        let tables = STATIC_TABLE
            .iter()
            .copied()
            .chain(self.table.iter())
            .enumerate();
        for (position, (entry_name, entry_value)) in tables {
            if entry_name != name {
                continue;
            }
            let index = position + 1;
            if entry_value == value {
                return Found::Field(index);
            }
            found = match found {
                Found::Name { first, .. } => Found::Name { first, last: index },
                _ => Found::Name {
                    first: index,
                    last: index,
                },
            };
        }
        debug_assert!(
            match found {
                Found::Field(index) => (1..entries).contains(&index),
                Found::Name { first, last } => 0 < first && first <= last && last < entries,
                Found::Nothing => true,
            },
            "a found index addresses an entry of one of the two tables"
        );
        found
    }
}

/// Appends a string literal (§5.2).
fn encode_string(string: &[u8], huffman: bool, out: &mut Vec<u8>) {
    if huffman {
        encode_integer(huffman::encoded_len(string), 7, 0x80, out);
        huffman::encode(string, out);
    } else {
        encode_integer(string.len(), 7, 0x00, out);
        out.extend_from_slice(string);
    }
}
