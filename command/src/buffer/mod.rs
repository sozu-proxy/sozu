//! Channel-side ring buffers.
//!
//! Two flavours: `fixed` is a fixed-capacity ring used when the maximum
//! payload size is known up-front; `growable` doubles its backing
//! `Vec<u8>` until it hits a configured cap. Both flavours implement the
//! same shift/insert/replace operations and use overlapping-safe
//! `std::ptr::copy` (NOT `copy_nonoverlapping`) with bounds checked
//! against `self.capacity()`.

pub mod fixed;
pub mod growable;
