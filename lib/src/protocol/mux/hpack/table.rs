//! The static table (RFC 7541 Appendix A) and the dynamic table (§2.3.2, §4).
//!
//! [`DynamicTable`] stores every entry's name and value back to back in one
//! byte buffer and keeps a ring of offsets beside it, so an entry costs no
//! allocation of its own and a lookup returns slices of that buffer.

use std::collections::VecDeque;

/// RFC 7541 Appendix A, indexes 1 to 61.
pub(super) const STATIC_TABLE: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

/// First index of the dynamic table in the index address space (§2.3.3).
pub(super) const DYNAMIC_TABLE_START: usize = STATIC_TABLE.len() + 1;

/// Per-entry overhead added to the octet lengths of name and value (§4.1).
pub(super) const ENTRY_OVERHEAD: usize = 32;

/// Initial maximum size of both tables: the default `SETTINGS_HEADER_TABLE_SIZE`
/// (RFC 9113 §6.5.2).
pub(super) const DEFAULT_MAX_SIZE: usize = 4096;

/// Capacity of the entry buffer at its first insertion, capped by the table
/// size: a first request's fields fit it without regrowing.
const INITIAL_BYTES: usize = 512;

/// Capacity of the offset ring at its first insertion.
const INITIAL_ENTRIES: usize = 16;

/// One entry's place in [`DynamicTable::bytes`].
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Offset of the name; the value follows it.
    offset: usize,
    name_len: usize,
    value_len: usize,
}

/// The dynamic table of one decoder or one encoder (RFC 7541 §2.3.2).
///
/// Entries are appended to `bytes` in insertion order, so the oldest entry
/// starts at `start` and eviction (§4.4) only moves `start` forward. When an
/// insertion would grow `bytes` while the evicted prefix `bytes[..start]` is at
/// least as long as the live part, the live part is moved to the front instead
/// (`compact`): each compaction moves at most the live bytes and follows at
/// least as many appended ones, so its cost is amortized, and growth only
/// happens while the buffer is less than twice the live bytes plus the new
/// entry. The buffer therefore grows to a high-water mark bounded by a small
/// multiple of the maximum size, and a steady-state insertion allocates
/// nothing. `entries` is bounded by `max_size / 32` (§4.1).
#[derive(Debug)]
pub(super) struct DynamicTable {
    bytes: Vec<u8>,
    start: usize,
    /// Oldest entry at the front, newest at the back.
    entries: VecDeque<Entry>,
    /// Sum of the entry sizes (§4.1), never above `max_size`.
    size: usize,
    max_size: usize,
}

impl DynamicTable {
    /// An empty table. Allocates nothing until the first insertion.
    pub(super) const fn new(max_size: usize) -> Self {
        DynamicTable {
            bytes: Vec::new(),
            start: 0,
            entries: VecDeque::new(),
            size: 0,
            max_size,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn size(&self) -> usize {
        self.size
    }

    pub(super) fn max_size(&self) -> usize {
        self.max_size
    }

    /// The entry at `index` from the newest one (0), as `(name, value)`.
    pub(super) fn get(&self, index: usize) -> Option<(&[u8], &[u8])> {
        let position = self.entries.len().checked_sub(index + 1)?;
        let entry = self.entries[position];
        let value_start = entry.offset + entry.name_len;
        Some((
            &self.bytes[entry.offset..value_start],
            &self.bytes[value_start..value_start + entry.value_len],
        ))
    }

    /// Iterates from the newest entry to the oldest.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        (0..self.entries.len()).filter_map(|index| self.get(index))
    }

    /// Changes the maximum size and evicts until the table fits (§4.3).
    pub(super) fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
        while self.size > self.max_size {
            self.evict_oldest();
        }
        self.check_invariants();
    }

    /// Adds an entry (§4.4): evicts until it fits, or empties the table when
    /// the entry alone is larger than the maximum size.
    pub(super) fn insert(&mut self, name: &[u8], value: &[u8]) {
        let entry_size = name.len() + value.len() + ENTRY_OVERHEAD;
        if entry_size > self.max_size {
            self.clear();
            self.check_invariants();
            return;
        }
        while self.size + entry_size > self.max_size {
            self.evict_oldest();
        }
        let needed = name.len() + value.len();
        let live = self.bytes.len() - self.start;
        if self.bytes.len() + needed > self.bytes.capacity() && self.start > 0 && self.start >= live
        {
            self.compact();
        }
        if self.bytes.capacity() == 0 {
            // One allocation for the first entries of a connection instead of
            // a doubling series from 8 octets.
            self.bytes
                .reserve(needed.max(INITIAL_BYTES.min(self.max_size)));
            self.entries.reserve(INITIAL_ENTRIES);
        } else {
            // One growth step for both strings, not one per `extend`.
            self.bytes.reserve(needed);
        }
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(name);
        self.bytes.extend_from_slice(value);
        self.entries.push_back(Entry {
            offset,
            name_len: name.len(),
            value_len: value.len(),
        });
        self.size += entry_size;
        debug_assert!(
            self.get(0) == Some((name, value)),
            "the inserted field is the newest entry"
        );
        self.check_invariants();
    }

    fn evict_oldest(&mut self) {
        let Some(oldest) = self.entries.pop_front() else {
            debug_assert_eq!(self.size, 0, "an empty table has size 0");
            self.size = 0;
            return;
        };
        self.size -= oldest.name_len + oldest.value_len + ENTRY_OVERHEAD;
        match self.entries.front() {
            Some(next) => self.start = next.offset,
            None => {
                self.bytes.clear();
                self.start = 0;
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes.clear();
        self.start = 0;
        self.size = 0;
    }

    /// Moves the live bytes to the front of the buffer.
    fn compact(&mut self) {
        let start = self.start;
        self.bytes.copy_within(start.., 0);
        self.bytes.truncate(self.bytes.len() - start);
        for entry in &mut self.entries {
            entry.offset -= start;
        }
        self.start = 0;
    }

    /// Full sweep of the table's invariants, run after every mutation in
    /// debug builds: the entries tile `bytes[start..]` in insertion order and
    /// their sizes add up to `size`, which fits `max_size` (RFC 7541 §4.1).
    fn check_invariants(&self) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut end = self.start;
        let mut size = 0;
        for entry in &self.entries {
            debug_assert_eq!(entry.offset, end, "entries are contiguous");
            end += entry.name_len + entry.value_len;
            size += entry.name_len + entry.value_len + ENTRY_OVERHEAD;
        }
        debug_assert_eq!(end, self.bytes.len(), "the newest entry ends the buffer");
        debug_assert_eq!(size, self.size, "size is the sum of the entry sizes");
        debug_assert!(
            self.size <= self.max_size,
            "the table fits its maximum size"
        );
        debug_assert!(
            self.entries.len() <= self.max_size / ENTRY_OVERHEAD,
            "every entry costs at least {ENTRY_OVERHEAD} octets"
        );
    }
}
