//! HPACK-owned state for [`super::h2::ConnectionH2`].
//!
//! Groups the connection-level HPACK codec pair (RFC 7541) and their
//! reusable scratch buffers behind a narrow, closed API. `ConnectionH2`
//! still owns a single [`HpackState`] field, so ownership does not split —
//! but every field here is private to this module, so nothing outside
//! `hpack_state.rs` can reach the raw `loona_hpack::Decoder`/`Encoder` or
//! the scratch `Vec`s directly; every access goes through an accessor
//! declared here.
//!
//! This is a pure relocation of fields that previously lived directly on
//! `ConnectionH2` (`decoder`, `encoder`, `converter_buf`, `lowercase_buf`,
//! `cookie_buf`, `priorities_buf`) — no behaviour change. See `h2.rs`'s call
//! sites (constructor, the two SETTINGS table-size-adjustment paths, the
//! `write_streams` converter borrow, and the scratch-buffer reclaim logic)
//! for how they're used.

use super::StreamId;

/// The connection-level HPACK decoder/encoder pair plus their reusable
/// scratch buffers.
pub(super) struct HpackState {
    decoder: loona_hpack::Decoder<'static>,
    encoder: loona_hpack::Encoder<'static>,
    /// Reusable buffer for HPACK-encoded headers in the H2 block converter.
    converter_buf: Vec<u8>,
    /// Reusable buffer for lowercasing header keys in the H2 block converter.
    lowercase_buf: Vec<u8>,
    /// Reusable buffer for assembling cookie values in the H2 block converter.
    cookie_buf: Vec<u8>,
    /// Reusable buffer for priority-sorted stream IDs in write_streams().
    /// Cleared and reused each call to avoid per-frame allocation.
    priorities_buf: Vec<StreamId>,
}

impl HpackState {
    /// Builds the decoder/encoder pair and empty scratch buffers for a new
    /// connection.
    ///
    /// `header_table_size` is `local_settings.settings_header_table_size` —
    /// RFC 7541 §4.2 requires the decoder enforce it as the upper bound for
    /// dynamic table size updates from the peer.
    pub(super) fn new(header_table_size: usize) -> Self {
        let mut decoder = loona_hpack::Decoder::new();
        decoder.set_max_allowed_table_size(header_table_size);
        HpackState {
            decoder,
            encoder: loona_hpack::Encoder::new(),
            converter_buf: Vec::new(),
            lowercase_buf: Vec::new(),
            cookie_buf: Vec::new(),
            priorities_buf: Vec::new(),
        }
    }

    pub(super) fn decoder_mut(&mut self) -> &mut loona_hpack::Decoder<'static> {
        &mut self.decoder
    }

    pub(super) fn encoder_mut(&mut self) -> &mut loona_hpack::Encoder<'static> {
        &mut self.encoder
    }

    /// RFC 7541 §4.2: sync the decoder's max allowed table size with what we
    /// advertised.
    pub(super) fn set_decoder_max_allowed_table_size(&mut self, size: usize) {
        self.decoder.set_max_allowed_table_size(size);
    }

    /// RFC 7541 §4.2: cap the encoder's dynamic table to the peer-advertised
    /// (and locally capped) `SETTINGS_HEADER_TABLE_SIZE`.
    pub(super) fn set_encoder_max_table_size(&mut self, size: usize) {
        self.encoder.set_max_table_size(size);
    }

    /// Takes ownership of the converter scratch buffer, leaving an empty
    /// `Vec` in its place — mirrors the pre-move `std::mem::take(&mut
    /// self.converter_buf)` call sites.
    pub(super) fn take_converter_buf(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.converter_buf)
    }

    pub(super) fn take_lowercase_buf(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.lowercase_buf)
    }

    pub(super) fn take_cookie_buf(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.cookie_buf)
    }

    /// Returns a buffer taken via [`Self::take_converter_buf`] once the
    /// borrow that needed it (the `H2BlockConverter`) is done with it.
    pub(super) fn put_converter_buf(&mut self, buf: Vec<u8>) {
        self.converter_buf = buf;
    }

    pub(super) fn put_lowercase_buf(&mut self, buf: Vec<u8>) {
        self.lowercase_buf = buf;
    }

    pub(super) fn put_cookie_buf(&mut self, buf: Vec<u8>) {
        self.cookie_buf = buf;
    }

    /// Takes ownership of the priority-sorted-stream-IDs scratch buffer,
    /// leaving an empty `Vec` in its place. `write_streams` needs this one
    /// taken out by value too (not just borrowed in place): once the
    /// converter borrows the encoder out of `self.hpack` for the pass, no
    /// other `self.hpack` accessor — including one returning `&mut
    /// priorities_buf` — can run until the converter is dropped.
    pub(super) fn take_priorities_buf(&mut self) -> Vec<StreamId> {
        std::mem::take(&mut self.priorities_buf)
    }

    pub(super) fn put_priorities_buf(&mut self, buf: Vec<StreamId>) {
        self.priorities_buf = buf;
    }

    /// Shrink reusable converter buffers when they grow beyond 16 KB to avoid
    /// holding memory after a burst of large headers.
    pub(super) fn shrink_converter_buffers(&mut self) {
        if self.converter_buf.capacity() > 16_384 {
            self.converter_buf.shrink_to(4096);
        }
        if self.lowercase_buf.capacity() > 16_384 {
            self.lowercase_buf.shrink_to(4096);
        }
        if self.cookie_buf.capacity() > 16_384 {
            self.cookie_buf.shrink_to(4096);
        }
    }

    /// Quiet-time reclaim of every scratch buffer (including
    /// `priorities_buf`, which [`Self::shrink_converter_buffers`] does not
    /// touch) once it holds 4x `retain_size`. Called from
    /// `cancel_timed_out_streams`, which only runs on a session idle long
    /// enough to risk timing out a stream — see that call site for why this
    /// is the right place to reclaim a high-water-mark buffer.
    pub(super) fn reclaim_idle_buffers(&mut self, retain_size: usize) {
        if self.converter_buf.capacity() > retain_size * 4 {
            self.converter_buf.shrink_to(retain_size);
        }
        if self.lowercase_buf.capacity() > retain_size * 4 {
            self.lowercase_buf.shrink_to(retain_size);
        }
        if self.cookie_buf.capacity() > retain_size * 4 {
            self.cookie_buf.shrink_to(retain_size);
        }
        if self.priorities_buf.capacity() > retain_size * 4 {
            self.priorities_buf.shrink_to(retain_size);
        }
    }
}
