//! Helpers shared between the H1 and H2 multiplexers.
//!
//! Both protocol state machines have converged on a handful of small
//! routines that operate on SocketHandler and StreamState in the same way.
//! Keeping them here prevents drift between the two write paths — which is
//! load-bearing for TLS close_notify ordering (see agent memory
//! `feedback_tls_write_symmetry`).

use kawa::AsBuffer;

use crate::{
    protocol::http::editor::{HeaderEditMode, HeaderEditSnapshot},
    socket::{SocketHandler, SocketResult},
};

use super::Stream;

/// Decision returned by [`end_stream_decision`] for the server-side end-of-stream path.
///
/// Both H1 and H2 must take the same action when a server-side stream ends, based
/// on whether the request was already partially consumed and whether the backend
/// has produced a forwardable response. Centralising the decision here keeps the
/// two protocols from drifting (e.g. H1 used to send 503 where H2 sent 502 for
/// "backend closed before any response").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndStreamAction {
    /// Backend already produced a fully terminated response — finish forwarding
    /// it and mark the stream `Unlinked`.
    ForwardTerminated,
    /// Backend produced a partial response that the caller cannot finish (no
    /// keep-alive AND no Content-Length); mark the response terminated so the
    /// converter emits a final DATA frame with END_STREAM.
    CloseDelimited,
    /// Backend produced a partial response but the connection is keep-alive
    /// (or otherwise expected to terminate cleanly): the backend went away
    /// mid-response — caller must forcefully terminate with an internal error.
    ForwardUnterminated,
    /// No response is available and the request was already partially consumed,
    /// so retrying is unsafe — send the given default status (502 Bad Gateway).
    SendDefault(u16),
    /// No response is available and the request is untouched, so the caller may
    /// link the stream to a fresh backend and retry.
    Reconnect,
}

/// Compute the canonical end-of-stream decision for a server-side stream.
///
/// Mirrors the H1 and H2 server end-of-stream logic so both protocols agree on
/// the outcome (in particular, "backend closed without response" is normalised
/// to **502 Bad Gateway**).
pub(super) fn end_stream_decision(stream: &Stream) -> EndStreamAction {
    if stream.back.is_main_phase() {
        if stream.back.is_terminated() {
            EndStreamAction::ForwardTerminated
        } else if !stream.context.keep_alive_backend {
            EndStreamAction::CloseDelimited
        } else {
            EndStreamAction::ForwardUnterminated
        }
    } else if stream.front.consumed {
        EndStreamAction::SendDefault(502)
    } else {
        EndStreamAction::Reconnect
    }
}

/// Drain rustls's pending TLS output before the underlying TCP socket is
/// shut down. A single write attempt is insufficient when the kernel send
/// buffer is full (common during large-response transfers): without this
/// drain loop, a partial TLS record can be left in-flight, producing
/// "TLS decode error / unexpected eof" on the client side.
///
/// Emits close_notify the first time it is called for a given session
/// (tracked by the caller's `close_notify_sent` flag) and then attempts
/// up to `MAX_DRAIN_ROUNDS` empty-vectored writes to flush rustls.
///
/// Returns `(tls_still_pending, drain_rounds)` so the caller can log or
/// react to an incomplete drain.
pub(super) fn drain_tls_close_notify<S: SocketHandler>(
    socket: &mut S,
    close_notify_sent: &mut bool,
) -> (bool, u32) {
    const MAX_DRAIN_ROUNDS: u32 = 16;
    if !*close_notify_sent {
        socket.socket_close();
        *close_notify_sent = true;
    }
    let mut drain_rounds = 0;
    while socket.socket_wants_write() && drain_rounds < MAX_DRAIN_ROUNDS {
        let (_size, status) = socket.socket_write_vectored(&[]);
        drain_rounds += 1;
        match status {
            SocketResult::WouldBlock | SocketResult::Error | SocketResult::Closed => break,
            SocketResult::Continue => {}
        }
    }
    (socket.socket_wants_write(), drain_rounds)
}

/// Apply per-frontend response-side header edits to a response kawa
/// just before [`kawa::Kawa::prepare`] runs. Mirrors the request-side
/// pass that `Router::route_from_request` runs against the front kawa
/// at routing time. Each edit's [`HeaderEditMode`] selects the action:
///
/// - [`HeaderEditMode::Append`] with a non-empty `val` → append the
///   header before the `end_header` flag block.
/// - [`HeaderEditMode::Append`] with an empty `val` → fall back to the
///   legacy delete encoding (HAProxy `del-header` parity). Pre-existing
///   call sites that have not migrated to the explicit
///   [`HeaderEditMode::Delete`] continue to work unchanged.
/// - [`HeaderEditMode::Delete`] → remove every existing header with the
///   matching name from `kawa.blocks`. Equivalent to the legacy empty-
///   `val` Append shape but explicit at the call site.
/// - [`HeaderEditMode::SetIfAbsent`] → append the header only when no
///   non-elided header with the same name already exists on the
///   response (RFC 6797 §6.1 single-`Strict-Transport-Security`
///   requirement, generalised to any operator-supplied
///   set-only-when-missing edit). The presence check runs against the
///   pre-edit response state so a `SetIfAbsent` does NOT race against a
///   `Delete` issued in the same batch.
///
/// Both H1 and H2 emission paths funnel through here so the on-wire
/// header set stays consistent. Per-edit set/replace semantics that
/// need an unconditional overwrite are expressed as two entries: one
/// `Delete` (or empty-`val` Append) followed by an `Append` with the
/// new value.
pub(super) fn apply_response_header_edits<T: AsBuffer>(
    kawa: &mut kawa::Kawa<T>,
    edits: &[HeaderEditSnapshot],
) {
    use kawa::{Block, Pair, Store};

    if edits.is_empty() {
        return;
    }

    // Snapshot the lowercased header names already present on the
    // response BEFORE any edit runs. Used by `SetIfAbsent` to honour
    // upstream-supplied headers (e.g. backend already sent its own
    // `Strict-Transport-Security`) without racing against a sibling
    // `Delete` edit in the same batch.
    let existing_keys: Vec<Vec<u8>> = {
        let buf = kawa.storage.buffer();
        kawa.blocks
            .iter()
            .filter_map(|block| {
                if let Block::Header(Pair { key, val: _ }) = block {
                    // Skip elided headers — see `Store::Empty` note below
                    // in the retain pass for the panic rationale.
                    if matches!(key, Store::Empty) {
                        None
                    } else {
                        Some(key.data(buf).iter().map(u8::to_ascii_lowercase).collect())
                    }
                } else {
                    None
                }
            })
            .collect()
    };

    let keys_to_drop: Vec<Vec<u8>> = edits
        .iter()
        .filter(|e| {
            // Explicit Delete OR legacy empty-`val` Append (preserved
            // for callers still using the implicit encoding).
            matches!(e.mode, HeaderEditMode::Delete)
                || (matches!(e.mode, HeaderEditMode::Append) && e.val.is_empty())
        })
        .map(|e| e.key.iter().map(u8::to_ascii_lowercase).collect())
        .collect();
    if !keys_to_drop.is_empty() {
        let buf = kawa.storage.buffer();
        kawa.blocks.retain(|block| {
            if let Block::Header(Pair { key, val: _ }) = block {
                // Skip elided headers — kawa's earlier passes (HPACK
                // decoder, H1 header parser) rewrite headers they want to
                // suppress to `Pair { key: Store::Empty, val: Store::Empty }`
                // rather than removing them in-place. Calling `.data()` on
                // `Store::Empty` panics in `kawa-0.6.8/src/storage/repr.rs`
                // (the impl unwraps a `None` slice). Pinning this guard
                // explicitly until kawa changes its policy.
                if matches!(key, Store::Empty) {
                    return true;
                }
                // Both `keys_to_drop` and `key_lower` are already
                // ASCII-lowercased, so a byte-equality compare is enough;
                // a second `compare_no_case` pass would just refold
                // bytes that are already canonical.
                let key_lower: Vec<u8> = key.data(buf).iter().map(u8::to_ascii_lowercase).collect();
                !keys_to_drop
                    .iter()
                    .any(|k| k.as_slice() == key_lower.as_slice())
            } else {
                true
            }
        });
    }

    let end_header_idx = end_of_headers_index(kawa);

    let mut to_insert: Vec<Block> = Vec::new();
    for edit in edits {
        match edit.mode {
            // Explicit delete is materialised by the retain pass above;
            // nothing to insert. Same for the legacy empty-`val` Append
            // shape, which we route through the delete branch for
            // backwards compatibility.
            HeaderEditMode::Delete => continue,
            HeaderEditMode::Append if edit.val.is_empty() => continue,
            HeaderEditMode::Append => {}
            HeaderEditMode::SetIfAbsent => {
                let key_lower: Vec<u8> = edit.key.iter().map(u8::to_ascii_lowercase).collect();
                if existing_keys
                    .iter()
                    .any(|k| k.as_slice() == key_lower.as_slice())
                {
                    // Upstream already supplied this header — honour
                    // the operator's "set only when missing" intent and
                    // skip the insert (RFC 6797 §6.1).
                    continue;
                }
            }
        }
        to_insert.push(Block::Header(Pair {
            key: Store::from_slice(&edit.key),
            val: Store::from_slice(&edit.val),
        }));
    }
    if !to_insert.is_empty() {
        // Insert each edit one position deeper than the previous so we
        // preserve operator order at the wire. `insert_at` points at the
        // existing `Block::Flags { end_header: true }` (or
        // `kawa.blocks.len()` when absent); each successive insert pushes
        // that anchor right by one — the `+ offset` keeps the flag block
        // (and any post-headers blocks) trailing the new entries even
        // when elided headers sit between them.
        let insert_at = end_header_idx.unwrap_or(kawa.blocks.len());
        for (offset, block) in to_insert.into_iter().enumerate() {
            kawa.blocks.insert(insert_at + offset, block);
        }
    }
}

/// Locate the `Block::Flags { end_header: true }` block in `kawa.blocks`.
///
/// Both the request-side rewrite helper (`mux::router`) and the
/// response-side edit helper above use this anchor as the insertion
/// point for new header blocks: anything after the end-of-headers flag
/// is body / chunks / trailers and must NOT receive a Header block. A
/// shared helper keeps the predicate canonical so a future change to
/// `kawa::Flags` (e.g. adding fields) only needs touching once.
pub(super) fn end_of_headers_index<T: AsBuffer>(kawa: &kawa::Kawa<T>) -> Option<usize> {
    kawa.blocks.iter().position(|b| {
        matches!(
            b,
            kawa::Block::Flags(kawa::Flags {
                end_header: true,
                ..
            })
        )
    })
}

#[cfg(test)]
mod tests {
    use super::apply_response_header_edits;
    use crate::protocol::http::editor::{HeaderEditMode, HeaderEditSnapshot};
    use kawa::{
        AsBuffer, Block, Buffer, Flags, Kawa, Kind, Pair, SliceBuffer, StatusLine, Store, Version,
    };

    fn make_kawa<'a>(buf: &'a mut [u8]) -> Kawa<SliceBuffer<'a>> {
        Kawa::new(Kind::Response, Buffer::new(SliceBuffer(buf)))
    }

    fn pretty_blocks<T: AsBuffer>(kawa: &Kawa<T>) -> Vec<(Vec<u8>, Vec<u8>)> {
        let buf = kawa.storage.buffer();
        kawa.blocks
            .iter()
            .filter_map(|b| {
                if let Block::Header(Pair { key, val }) = b {
                    if matches!(key, Store::Empty) {
                        Some((b"<elided>".to_vec(), b"".to_vec()))
                    } else {
                        Some((key.data(buf).to_vec(), val.data(buf).to_vec()))
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Insertion offset arithmetic: when the response already carries
    /// elided headers (kawa marks suppressed headers as `Store::Empty`
    /// instead of removing them), inserting two new headers must keep
    /// them contiguous AND keep the `end_header` flag block trailing.
    #[test]
    fn test_response_edit_offsets_with_elided_headers() {
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf);
        kawa.detached.status_line = StatusLine::Response {
            version: Version::V11,
            code: 200,
            status: Store::Static(b"200"),
            reason: Store::Static(b"OK"),
        };
        // Pre-populate with a real header, an elided header (suppressed
        // by an earlier kawa pass), another real header, and finally the
        // end-of-headers flag block.
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"server"),
            val: Store::Static(b"sozu"),
        }));
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Empty,
            val: Store::Empty,
        }));
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"content-length"),
            val: Store::Static(b"3"),
        }));
        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: false,
        }));

        apply_response_header_edits(
            &mut kawa,
            &[
                HeaderEditSnapshot {
                    key: b"x-frame-options".to_vec(),
                    val: b"DENY".to_vec(),
                    mode: HeaderEditMode::Append,
                },
                HeaderEditSnapshot {
                    key: b"x-content-type-options".to_vec(),
                    val: b"nosniff".to_vec(),
                    mode: HeaderEditMode::Append,
                },
            ],
        );

        let names: Vec<Vec<u8>> = pretty_blocks(&kawa).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            names,
            vec![
                b"server".to_vec(),
                b"<elided>".to_vec(),
                b"content-length".to_vec(),
                b"x-frame-options".to_vec(),
                b"x-content-type-options".to_vec(),
            ],
            "new headers must land in order, between the last real header and the end-of-headers flag"
        );

        // The end-of-headers flag must still be the final block.
        assert!(
            matches!(
                kawa.blocks.back(),
                Some(Block::Flags(Flags {
                    end_header: true,
                    ..
                }))
            ),
            "end_header flag must remain the last block"
        );
    }

    /// Delete-by-name with elided headers around the target: the elided
    /// blocks must survive the retain pass (their `key.data()` would
    /// panic) and the targeted header must be dropped.
    #[test]
    fn test_response_edit_delete_skips_elided_blocks() {
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf);
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Empty,
            val: Store::Empty,
        }));
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"x-internal"),
            val: Store::Static(b"secret"),
        }));
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Empty,
            val: Store::Empty,
        }));
        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: false,
        }));

        apply_response_header_edits(
            &mut kawa,
            &[HeaderEditSnapshot {
                key: b"X-Internal".to_vec(),
                val: Vec::new(),
                mode: HeaderEditMode::Append,
            }],
        );

        let names: Vec<Vec<u8>> = pretty_blocks(&kawa).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            names,
            vec![b"<elided>".to_vec(), b"<elided>".to_vec()],
            "x-internal must be dropped; elided blocks must survive the retain pass"
        );
    }

    /// Delete-then-set: the helper applies all deletes first, then
    /// inserts the non-empty edits. Two edits with the same key
    /// (one delete, one set) effectively replace the value.
    #[test]
    fn test_response_edit_delete_then_set_replaces() {
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf);
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"cache-control"),
            val: Store::Static(b"public"),
        }));
        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: false,
        }));

        apply_response_header_edits(
            &mut kawa,
            &[
                HeaderEditSnapshot {
                    key: b"Cache-Control".to_vec(),
                    val: Vec::new(),
                    mode: HeaderEditMode::Append,
                },
                HeaderEditSnapshot {
                    key: b"Cache-Control".to_vec(),
                    val: b"no-store".to_vec(),
                    mode: HeaderEditMode::Append,
                },
            ],
        );

        let pairs = pretty_blocks(&kawa);
        assert_eq!(pairs.len(), 1, "only the replacement header must remain");
        assert_eq!(pairs[0].0, b"Cache-Control");
        assert_eq!(pairs[0].1, b"no-store");
    }

    /// `SetIfAbsent` MUST NOT add a second header when the upstream
    /// response already carries one with the same name. This is the
    /// HSTS-on-listener-default contract: if the backend already
    /// emitted its own `Strict-Transport-Security`, the operator's
    /// listener-default policy steps aside (RFC 6797 §6.1).
    #[test]
    fn test_response_edit_set_if_absent_skips_when_present() {
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf);
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"strict-transport-security"),
            val: Store::Static(b"max-age=12345"),
        }));
        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: false,
        }));

        apply_response_header_edits(
            &mut kawa,
            &[HeaderEditSnapshot {
                key: b"strict-transport-security".to_vec(),
                val: b"max-age=31536000".to_vec(),
                mode: HeaderEditMode::SetIfAbsent,
            }],
        );

        let pairs = pretty_blocks(&kawa);
        assert_eq!(
            pairs.len(),
            1,
            "SetIfAbsent must not duplicate an existing header"
        );
        assert_eq!(pairs[0].0, b"strict-transport-security");
        assert_eq!(
            pairs[0].1, b"max-age=12345",
            "the upstream-supplied STS value must be preserved unchanged"
        );
    }

    /// `SetIfAbsent` MUST insert the new header when no upstream
    /// header with the same name is present. The new entry lands
    /// before the end-of-headers flag (same insertion point as a
    /// regular `Append`).
    #[test]
    fn test_response_edit_set_if_absent_inserts_when_absent() {
        let mut buf = vec![0u8; 4096];
        let mut kawa = make_kawa(&mut buf);
        kawa.blocks.push_back(Block::Header(Pair {
            key: Store::Static(b"server"),
            val: Store::Static(b"sozu"),
        }));
        kawa.blocks.push_back(Block::Flags(Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: false,
        }));

        apply_response_header_edits(
            &mut kawa,
            &[HeaderEditSnapshot {
                key: b"strict-transport-security".to_vec(),
                val: b"max-age=31536000".to_vec(),
                mode: HeaderEditMode::SetIfAbsent,
            }],
        );

        let pairs = pretty_blocks(&kawa);
        assert_eq!(
            pairs.len(),
            2,
            "SetIfAbsent must insert exactly one new header when absent"
        );
        assert_eq!(pairs[0].0, b"server");
        assert_eq!(pairs[1].0, b"strict-transport-security");
        assert_eq!(pairs[1].1, b"max-age=31536000");

        // The end-of-headers flag must remain the final block.
        assert!(
            matches!(
                kawa.blocks.back(),
                Some(Block::Flags(Flags {
                    end_header: true,
                    ..
                }))
            ),
            "end_header flag must remain the last block"
        );
    }
}
