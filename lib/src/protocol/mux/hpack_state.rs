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
//! `cookie_buf`) — no behaviour change. See `h2.rs`'s call sites
//! (constructor, the two SETTINGS table-size-adjustment paths, the
//! `write_streams` converter borrow, and the scratch-buffer reclaim logic)
//! for how they're used.
//!
//! The three scratch buffers and the codec pair are acquired from the
//! connection's [`super::buffer_source::BufferSource`] rather than allocated
//! here, which is what puts this module's state under the same ownership
//! contract as the wire buffers instead of leaving it to the global
//! allocator. [`HpackState::new`] is fallible for that reason: a source that
//! refuses refuses the whole connection, on the same path as a refused
//! stream-0 buffer. What the source does *not* reach is documented on the
//! trait — `loona_hpack`'s own tables, and growth of a buffer already handed
//! over.
//!
//! The pass-ordering buffer `priorities_buf` arrived here with that
//! relocation and left again with the scheduler extraction: it holds
//! priority-sorted stream ids, never HPACK bytes, and now lives on
//! [`super::h2_scheduler::H2Scheduler`] with the decision that fills it.

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
}

impl HpackState {
    /// Builds the decoder/encoder pair and the scratch buffers for a new
    /// connection, drawing all three buffers from `buffers`.
    ///
    /// `header_table_size` is `local_settings.settings_header_table_size` —
    /// RFC 7541 §4.2 requires the decoder enforce it as the upper bound for
    /// dynamic table size updates from the peer.
    ///
    /// Returns `None` when the source refuses a scratch buffer. The codec
    /// pair is built only once all three have been granted, so a refusal
    /// costs no `loona_hpack` allocation either — which is the sense in which
    /// the codecs are under the contract, given the crate offers no
    /// capacity-supplied constructor to route their tables through.
    pub(super) fn new(
        buffers: &mut dyn super::buffer_source::BufferSource,
        header_table_size: usize,
    ) -> Option<Self> {
        let converter_buf = buffers.scratch()?;
        let lowercase_buf = buffers.scratch()?;
        let cookie_buf = buffers.scratch()?;
        let mut decoder = loona_hpack::Decoder::new();
        decoder.set_max_allowed_table_size(header_table_size);
        Some(HpackState {
            decoder,
            encoder: loona_hpack::Encoder::new(),
            converter_buf,
            lowercase_buf,
            cookie_buf,
        })
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

    /// Quiet-time reclaim of every scratch buffer once it holds 4x
    /// `retain_size`. Called from `cancel_timed_out_streams`, which only runs
    /// on a session idle long enough to risk timing out a stream — see that
    /// call site for why this is the right place to reclaim a high-water-mark
    /// buffer. The scheduler's own order buffer is reclaimed beside this call
    /// by [`super::h2_scheduler::H2Scheduler::reclaim_idle_buffer`].
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
    }
}
