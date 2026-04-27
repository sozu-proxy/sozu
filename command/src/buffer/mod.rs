//! Channel-side ring buffers.
//!
//! Two flavours: `fixed` is a fixed-capacity ring used when the maximum
//! payload size is known up-front; `growable` doubles its backing
//! `Vec<u8>` until it hits a configured cap. Both flavours implement the
//! same shift/insert/replace operations with overlapping-safe moves and
//! bounds checked against `self.capacity()`.

pub mod fixed;
pub mod growable;
