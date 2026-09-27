//! HPACK decoder (RFC 7541 §3, §6) over an untrusted header block.
//!
//! [`Decoder::decode_with_cb`] walks one complete header block and hands every
//! field to a callback as two slices that borrow the input, the static table,
//! the dynamic table or the decoder's scratch buffer. Only three things are
//! ever copied: a Huffman string, decoded into the scratch buffer; the name of
//! a dynamic entry re-indexed by a literal, copied into it too because the
//! insertion may evict that entry; and every field added to the dynamic table.

use std::borrow::Cow;

use super::{
    DecoderError, decode_integer, huffman,
    table::{DEFAULT_MAX_SIZE, DYNAMIC_TABLE_START, DynamicTable, STATIC_TABLE},
};

/// RFC 7541 §4.2: at most two size updates open a block — the smallest size
/// set since the last block, then the final one.
const MAX_LEADING_SIZE_UPDATES: usize = 2;

/// Where a decoded name or value lives until the callback reads it.
#[derive(Clone, Copy)]
enum Source {
    /// `block[start..end]`: a string sent without Huffman coding.
    Input(usize, usize),
    /// `scratch[start..end]`: a decoded Huffman string or a copied name.
    Scratch(usize, usize),
    /// A static table name.
    Static(&'static [u8]),
}

/// Where a literal's name lives.
#[derive(Clone, Copy)]
enum Name {
    Source(Source),
    /// The name of dynamic entry `index` (0 is the newest), for a literal that
    /// does not insert, so the entry cannot move before the callback.
    Dynamic(usize),
}

/// The connection-level HPACK decoder of one direction.
#[derive(Debug)]
pub struct Decoder {
    table: DynamicTable,
    /// The `SETTINGS_HEADER_TABLE_SIZE` this endpoint advertised: the upper
    /// bound of a size update (§4.2, §6.3).
    max_allowed_size: usize,
    /// Decoded Huffman strings of the current field. Cleared per field, never
    /// shrunk by a decode, so it stops growing at the longest decoded string.
    scratch: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder with the default 4096-octet table and no size-update bound
    /// until [`Self::set_max_allowed_table_size`] sets one. Allocates nothing.
    pub const fn new() -> Self {
        Decoder {
            table: DynamicTable::new(DEFAULT_MAX_SIZE),
            max_allowed_size: usize::MAX,
            scratch: Vec::new(),
        }
    }

    /// Sets the largest size a peer's size update may announce: the
    /// `SETTINGS_HEADER_TABLE_SIZE` this endpoint advertised (RFC 7541 §4.2).
    pub fn set_max_allowed_table_size(&mut self, max_allowed_size: usize) {
        self.max_allowed_size = max_allowed_size;
    }

    /// Sets the table's maximum size directly, as a size update would, and
    /// evicts until the table fits.
    pub fn set_max_table_size(&mut self, max_size: usize) {
        self.table.set_max_size(max_size);
    }

    #[cfg(test)]
    pub(super) fn table(&self) -> &DynamicTable {
        &self.table
    }

    /// Releases the scratch buffer when its capacity exceeds `retain` octets.
    pub fn shrink_scratch(&mut self, retain: usize) {
        if self.scratch.capacity() > retain {
            self.scratch = Vec::new();
        }
    }

    /// Decodes one complete header block and calls `callback(name, value)` for
    /// every field, in order. Both arguments are always `Cow::Borrowed`; the
    /// type only saves the callers a conversion.
    ///
    /// On error the block must be treated as a connection error of type
    /// COMPRESSION_ERROR (RFC 9113 §4.3): fields before the error have been
    /// delivered and inserted, and the table no longer matches the peer's.
    pub fn decode_with_cb<F>(&mut self, block: &[u8], mut callback: F) -> Result<(), DecoderError>
    where
        F: FnMut(Cow<'_, [u8]>, Cow<'_, [u8]>),
    {
        let mut position = 0;
        let mut fields = 0usize;
        let mut leading_size_updates = 0;
        let mut last_was_size_update = false;
        while position < block.len() {
            let first = block[position];
            last_was_size_update = false;
            if first & 0x80 != 0 {
                // §6.1 Indexed Header Field.
                let (index, consumed) = decode_integer(&block[position..], 7)?;
                let (name, value) = self.lookup(index)?;
                callback(Cow::Borrowed(name), Cow::Borrowed(value));
                position += consumed;
                fields += 1;
            } else if first & 0x40 != 0 {
                // §6.2.1 Literal Header Field with Incremental Indexing.
                position = self.literal(block, position, 6, true, &mut callback)?;
                fields += 1;
            } else if first & 0x20 != 0 {
                // §6.3 Dynamic Table Size Update.
                if fields > 0 {
                    return Err(DecoderError::SizeUpdateNotAtStart);
                }
                leading_size_updates += 1;
                if leading_size_updates > MAX_LEADING_SIZE_UPDATES {
                    return Err(DecoderError::TooManySizeUpdates);
                }
                let (size, consumed) = decode_integer(&block[position..], 5)?;
                if size > self.max_allowed_size {
                    return Err(DecoderError::TableSizeTooLarge);
                }
                self.table.set_max_size(size);
                position += consumed;
                last_was_size_update = true;
            } else {
                // §6.2.2 without indexing (0000) and §6.2.3 never indexed
                // (0001) decode alike; the flag only binds a re-encoder.
                position = self.literal(block, position, 4, false, &mut callback)?;
                fields += 1;
            }
        }
        if last_was_size_update {
            return Err(DecoderError::SizeUpdateAtEnd);
        }
        Ok(())
    }

    /// Resolves an index of the combined address space (§2.3.3).
    fn lookup(&self, index: usize) -> Result<(&[u8], &[u8]), DecoderError> {
        match index {
            0 => Err(DecoderError::InvalidIndex),
            1..DYNAMIC_TABLE_START => Ok(STATIC_TABLE[index - 1]),
            _ => self
                .table
                .get(index - DYNAMIC_TABLE_START)
                .ok_or(DecoderError::InvalidIndex),
        }
    }

    /// Decodes the literal field starting at `block[position]` (§6.2), calls
    /// `callback`, inserts it when `indexing`, and returns the next position.
    fn literal<F>(
        &mut self,
        block: &[u8],
        position: usize,
        prefix: u8,
        indexing: bool,
        callback: &mut F,
    ) -> Result<usize, DecoderError>
    where
        F: FnMut(Cow<'_, [u8]>, Cow<'_, [u8]>),
    {
        self.scratch.clear();
        let (index, consumed) = decode_integer(&block[position..], prefix)?;
        let mut position = position + consumed;
        let name = match index {
            0 => {
                let (name, next) = decode_string(block, position, &mut self.scratch)?;
                position = next;
                Name::Source(name)
            }
            1..DYNAMIC_TABLE_START => Name::Source(Source::Static(STATIC_TABLE[index - 1].0)),
            _ => {
                let dynamic = index - DYNAMIC_TABLE_START;
                let (name, _) = self.table.get(dynamic).ok_or(DecoderError::InvalidIndex)?;
                if indexing {
                    // The insertion below may evict this very entry.
                    self.scratch.extend_from_slice(name);
                    Name::Source(Source::Scratch(0, self.scratch.len()))
                } else {
                    Name::Dynamic(dynamic)
                }
            }
        };
        let (value, position) = decode_string(block, position, &mut self.scratch)?;
        let value = resolve(value, block, &self.scratch);
        match name {
            Name::Source(name) => {
                let name = resolve(name, block, &self.scratch);
                callback(Cow::Borrowed(name), Cow::Borrowed(value));
                if indexing {
                    self.table.insert(name, value);
                }
            }
            Name::Dynamic(index) => {
                let name = self
                    .table
                    .get(index)
                    .map(|(name, _)| name)
                    .ok_or(DecoderError::InvalidIndex)?;
                callback(Cow::Borrowed(name), Cow::Borrowed(value));
            }
        }
        Ok(position)
    }
}

/// Decodes the string literal (§5.2) at `block[position]` and returns where
/// it lives and the position after it.
fn decode_string(
    block: &[u8],
    position: usize,
    scratch: &mut Vec<u8>,
) -> Result<(Source, usize), DecoderError> {
    let rest = &block[position..];
    let Some(&first) = rest.first() else {
        return Err(DecoderError::Truncated);
    };
    let (length, consumed) = decode_integer(rest, 7)?;
    if length > rest.len() - consumed {
        return Err(DecoderError::Truncated);
    }
    let start = position + consumed;
    let end = start + length;
    if first & 0x80 == 0 {
        return Ok((Source::Input(start, end), end));
    }
    let scratch_start = scratch.len();
    huffman::decode(&block[start..end], scratch).map_err(DecoderError::Huffman)?;
    Ok((Source::Scratch(scratch_start, scratch.len()), end))
}

fn resolve<'a>(source: Source, block: &'a [u8], scratch: &'a [u8]) -> &'a [u8] {
    match source {
        Source::Input(start, end) => &block[start..end],
        Source::Scratch(start, end) => &scratch[start..end],
        Source::Static(name) => name,
    }
}
