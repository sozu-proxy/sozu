# T5 — Audit verification for deferred items from Pass 1/4

Scope: confirm that commit `5261dbe5` ("fix(h2-parser): harden GOAWAY length,
strip_padding bounds, unknown frame, and PUSH_PROMISE") fixed the three
deferred parser audit items, and harden the fourth item (stream-ID exhaustion)
in `h2.rs`.

Worktree: `/home/florentin/Sources/github.com/sozu-proxy/sozu.fix-h2-audit-verify`
(branch `fix/h2-audit-verify`, base `feat/h2-mux`).

## Verdict matrix

| # | Item | Verdict | Evidence | Action taken |
|---|------|---------|----------|--------------|
| 1 | GOAWAY `payload_len < 8` guard | already-fixed-by-5261dbe5 | `lib/src/protocol/mux/parser.rs:380-389` emits `H2Error::FrameSizeError` when `header.payload_len < GOAWAY_PAYLOAD_SIZE` (const `= 8` at `parser.rs:52`). Unit tests `test_goaway_short_payload_rejected` and `test_goaway_minimum_payload_accepted` cover both sides of the boundary. | None — verified and covered by existing tests. |
| 2 | `strip_padding` off-by-one | already-fixed-by-5261dbe5 | `lib/src/protocol/mux/parser.rs:427-447`: check is `(pad_length as usize) > i.len()` (strict-greater, not `>=`). RFC 9113 §6.1 allows `pad_length == payload_len - 1` (all-padding, zero content). Unit tests `test_strip_padding_all_padding_accepted` and `test_strip_padding_pad_length_equals_payload_len_rejected` cover both edges. | None — verified and covered by existing tests. |
| 3 | Unknown frame types silently ignored (RFC 9113 §5.5) | already-fixed-by-5261dbe5 | `parser.rs:294-308` maps unknown type bytes to `FrameType::Unknown(u8)`; `parser.rs:401` dispatches to `unknown_frame` (`parser.rs:794-804`) which consumes the payload and returns `Frame::Unknown(t)` without error. `parser.rs:263-272` skips stream-id parity validation for `FrameType::Unknown(_)`. Unit tests `test_unknown_frame_type_is_ignored` and `test_priority_update_is_ignored` (RFC 9218 PRIORITY_UPDATE, type 0x10) cover the happy path. | None — verified and covered by existing tests. |
| 4 | Stream-ID exhaustion emits GOAWAY (not silent overflow/reuse) | patched-here (testing + off-by-one) | Client-side drain is already wired: `lib/src/protocol/mux/h2.rs:4778-4804` calls `self.new_stream_id()` and, on `None`, transitions `BackendStatus::Disconnecting` and invokes `graceful_goaway()` (FIX-22 / Pass 4 Medium #5). However the allocator had (a) no unit test and (b) an off-by-one: the guard `next > STREAM_ID_MAX` wrongly rejected the valid final-slot allocation (`issued = STREAM_ID_MAX`), costing two IDs out of 2³¹. Extracted the allocator into a pure free function `next_stream_id(last_stream_id, is_client)` at `h2.rs:153-181` and tightened the guard to check `issued > STREAM_ID_MAX`. | Refactored `new_stream_id` to delegate to the new helper. Added 9 unit tests in `h2.rs:6126-…` covering: fresh client alloc, odd-parity monotonic sequence, server even-parity symmetry, final-slot allocation (client issues `STREAM_ID_MAX`), exhausted-returns-`None` (client + server), `checked_add` saturation near `u32::MAX`, boundary sweep (`STREAM_ID_MAX ± 4`), client/server parity disjointness. |

## Validation results

| Check | Result |
|---|---|
| `cargo build --all-features --locked` | green (58.65 s) |
| `cargo clippy --all-targets --locked` | clean — only the pre-existing `clippy::if_same_then_else` in `e2e/src/tests/h2_correctness_tests.rs:504` (documented in project `CLAUDE.md` "Build" section) |
| `cargo +nightly fmt --all -- --check` | green |
| `cargo test -p sozu-lib --locked --lib tests::test_next_stream_id` | 9/9 pass |
| `cargo test --workspace --locked --lib -- parser` | 64/64 pass |
| `cargo test -p sozu-e2e --locked -- h2_security_parser h2_security_session` | 13 passed, 1 ignored (FIX-22 smoke — white-box coverage now provided by the new unit tests) |
| `cargo +nightly fuzz run fuzz_frame_parser -max_total_time=60` | no crashes, 1 536 093 executions, avg 25 181 exec/s, coverage 327, rss 342 MB |

## Notes for the next audit pass

- `parser.rs:244` carries a pre-existing nightly-only warning
  (`mismatched_lifetime_syntaxes`) that appears under `cargo +nightly`.
  Unrelated to this change; cosmetic only.
- Sōzu never advertises `SETTINGS_ENABLE_PUSH=1`, so the server path of
  `next_stream_id(last, is_client=false)` is unreachable in production. It is
  kept symmetrical and unit-tested so enabling push in the future does not
  silently regress.
- The off-by-one fix changes the exhaustion threshold by exactly one allocation
  (the final `STREAM_ID_MAX` slot is now reachable). No downstream caller
  depends on the old off-by-one because the drain path simply observes
  `None` — later or earlier by one slot is indistinguishable.
