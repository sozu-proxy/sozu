# PROXY Protocol — Session Workflow

Reference document for maintainers of `lib/src/protocol/proxy_protocol/`.
Companion to `lib/src/protocol/mux/LIFECYCLE.md` (the downstream H1/H2
datapath) and `lib/src/protocol/kawa_h1/LIFECYCLE.md` (the H1 vocabulary it
builds on).

Every claim is anchored to code. Where the prose names an item — a function, a
method, a struct, an enum — the anchor is that item plus its file, with no line
number, because a line number does not survive an edit above it. A `file.rs:LINE`
or `file.rs:LINE-LINE` anchor is kept only where the claim is about a specific
statement or branch inside an item; those were refreshed against `main` at
`0cb1e2a7` on 2026-09-20.

---

## 1. Wire Format Reminder

The PROXY protocol prepends a fixed metadata block to a TCP connection so the
inner protocol (H1 / H2 / TLS / TCP-passthrough) sees the original client
address even after the connection traversed an upstream load balancer.

Two wire versions exist:

- **v1** is text-only. Lines have the shape
  `PROXY TCP4 src_ip dst_ip src_port dst_port\r\n` (or `TCP6` /
  `UNKNOWN\r\n`). See `HeaderV1` (`lib/src/protocol/proxy_protocol/header.rs`)
  and the comment at `header.rs:47` flagging that **v1 is never used inside
  Sōzu** — only v2 is parsed or emitted on the wire. The v1 serializer
  (`HeaderV1::into_bytes`, `header.rs`) survives as dead weight pending the
  documented removal.
- **v2** is binary. The wire frame is a 12-byte signature
  (`0x0D 0x0A 0x0D 0x0A 0x00 0x0D 0x0A 0x51 0x55 0x49 0x54 0x0A`, see
  `header.rs:186-188`) followed by version+command, family, address-block
  length (big-endian `u16`), and the address block. `HeaderV2`
  (`header.rs`) carries the parsed shape; `HeaderV2::into_bytes`
  (`header.rs`) emits it.

Address families are modelled by `ProxyAddr` (`header.rs`): `Ipv4Addr`,
`Ipv6Addr`, `UnixAddr` (108 bytes per side per the AF_UNIX socket-path
limit), and `AfUnspec` for unknown/legacy.

The v2 parser lives in `lib/src/protocol/proxy_protocol/parser.rs`; the
public entry point is `parse_v2_header` (`parser.rs`). It uses `nom` and
returns either a complete `HeaderV2`, an `Incomplete` request for more
bytes, or a parse error.

**`LOCAL` headers carry no attributable address.** The command nibble
distinguishes `PROXY` (`0x21`) from `LOCAL` (`0x20`); per the HAProxy PROXY
protocol specification §2.2 a `LOCAL` header describes a connection the
upstream proxy originated itself — typically a health check — and its address
block must be discarded in favour of the real socket endpoints. The wire format
does **not** require a `LOCAL` header to declare `AF_UNSPEC`: only HAProxy's own
emitter pairs the two, and a crafted peer may send `LOCAL` with a fully
populated `AF_INET` / `AF_INET6` block. `parse_v2_header` therefore parses that
block (so a malformed or unknown family is still rejected, and the declared
length still delimits the header) and then yields `ProxyAddr::AfUnspec` for it.
Discarding at the parse boundary is what makes the fallback in the four
TCP-side consumers — `expect.rs`, `relay.rs`, `lib/src/protocol/tcp_preread/mod.rs`, and
`TcpSession::effective_session_address` (`lib/src/tcp.rs`), which reads
`ExpectProxyProtocol::addresses` / `RelayProxyProtocol::addresses` directly
rather than through `into_pipe`, all of which attribute `ProxyAddr::source()`
to the client — hold: none of them reads `command` or `family`, and none
re-serializes the parsed header. The `family` byte is still reported as it was
read off the wire.

Those four are not the only consumers of the parsed pair, and the other two
do not fall back. `HttpSession::upgrade_expect` (`lib/src/http.rs`) and
`HttpsSession::upgrade_expect` (`lib/src/https.rs`) each need a source
**and** a destination to build the downstream session; `ProxyAddr::AfUnspec`
returns `None` from both accessors (`ProxyAddr::source` /
`ProxyAddr::destination` in `header.rs`), so the upgrade is
refused and `HttpSession::upgrade` reports `SessionIsToBeClosed`
(`lib/src/http.rs`). An HTTP or HTTPS session that presents a `LOCAL` header is closed here, not
re-attributed to `peer_addr`. That is unchanged for the `AF_UNSPEC` block
HAProxy actually pairs with `LOCAL` — those sessions already closed — and is
the intended outcome for a forged populated block.

---

## 2. Three Roles, Three SessionStates

`lib/src/protocol/proxy_protocol/mod.rs` exposes three sibling
`SessionState`s, one per role in the data path:

| Role     | Module         | Front interest at boot | Back interest at boot | When used                                                                                |
|----------|----------------|------------------------|-----------------------|------------------------------------------------------------------------------------------|
| `expect` | `expect.rs`    | `READABLE\|HUP\|ERROR` | n/a                   | Server-side ingress: the upstream LB sent us a v2 header; consume it, capture peer pair, transition to `Pipe`. |
| `relay`  | `relay.rs`     | `READABLE\|HUP\|ERROR` | `HUP\|ERROR`          | Forward an inbound v2 header verbatim onto a freshly opened backend socket.              |
| `send`   | `send.rs`      | `HUP\|ERROR`           | `HUP\|ERROR`          | Synthesise a v2 header describing the original client and emit it on the backend socket. |

All three carry a per-connection ULID (`request_id`) used by the log
context macros (`log_context!` in each module: `expect.rs`,
`relay.rs`, `send.rs`) so a session is grep-correlatable across the
PROXY phase and the downstream protocol.

### 2.1 `ExpectProxyProtocol`

- Type: `ExpectProxyProtocol<Front: SocketHandler>` (`expect.rs`).
- Buffer: `frontend_buffer: [u8; 232]` — the maximum legal v2 header size
  (Unix-socket family carries 2 × 108 bytes of address plus header overhead).
  Hard-bounded to defend against a malicious peer that opens TCP and never
  finishes the header.
- Entry point: `ExpectProxyProtocol::readable` (`expect.rs`).
  - `header_len` (`expect.rs:119-123`) tracks the expected read window;
    starts at the v4 size (28 bytes), bumps to v6 (52) and finally Unix
    (232) if `parse_v2_header` returns `Incomplete` after the prior cap.
  - 0-byte read with `index == 0` (`expect.rs:201-214`) closes the session
    immediately; this is the standard HAProxy bare-TCP healthcheck pattern
    (SYN/ACK/FIN with no `send-proxy`). Closing fast avoids zombie sessions
    sitting on `request_timeout` (default 10 s) and consuming the
    `nb_connections` quota.
  - Index of 232 with the parser still `Incomplete` (`expect.rs:249-259`)
    is the oversized-header sentinel — increment the
    `proxy_protocol.errors` metric and close.
  - Successful parse (`expect.rs:219-236`) stores the `ProxyAddr` into
    `self.addresses` and returns `SessionResult::Upgrade`; the proxy then
    swaps the session for a `Pipe` via `ExpectProxyProtocol::into_pipe` (`expect.rs`), which
    prefers `ProxyAddr::source()` and falls back to the front socket's
    `peer_addr` when it is `AfUnspec` — including for every `LOCAL` header
    (§1).

### 2.2 `RelayProxyProtocol`

- Type: `RelayProxyProtocol<Front: SocketHandler>` (`relay.rs`).
- Used when Sōzu sits between two PROXY-aware peers: read the inbound
  header, then write those exact bytes (and only those bytes) to the
  backend before any user-payload byte.
- Entry points: `RelayProxyProtocol::readable` (`relay.rs`) feeds the parser; on a complete
  parse it flips `frontend_readiness.interest` to drop READABLE and arms
  the backend WRITABLE bit (`relay.rs:163-164`). It then records
  `header_size` and consumes **nothing**: those buffered bytes are the only
  copy of the header, since this state never re-serializes `addresses`.
  `RelayProxyProtocol::back_writable` (`relay.rs`) drains exactly the first `header_size`
  bytes of `frontend_buffer` onto the backend socket and returns
  `SessionResult::Upgrade` once the cursor reaches them.
- Anything the client pipelined into the same read stays in
  `frontend_buffer` for the pipe phase, and `RelayProxyProtocol::into_pipe` (`relay.rs`)
  hands that same `Checkout` to `Pipe::new`. Note where the surviving
  wake-up actually comes from: `Pipe::new`'s own
  `arm_inherited_buffer_writes` is immediately overwritten by the restored
  event words, so it is `into_pipe`'s `restore_readiness_events` — which
  restores both words and THEN re-runs that arm — that leaves the backend
  WRITABLE readiness set. Never replace it with a bare
  `pipe.backend_readiness.event = …` pair; `tcp.rs`'s
  `build_pipe_from_preread` and `upgrade_send` carry the same warning for
  the same reason. The relay must also never widen its write past the
  header tail, and never consume past what it forwarded — either would
  silently drop client payload.
- Three yield-instead-of-spin guards, all of the same class:
  - A zero-length **write** drops the backend WRITABLE event and breaks.
    Without it the loop spins: a connected non-blocking socket answers
    `write(&[])` with `Ok(0)` indefinitely, `cursor_header` never advances,
    and `MAX_LOOP_ITERATIONS` bounds only the outer `tcp.rs::ready_inner`
    dispatch loop, so the worker burns 100% CPU with its event loop
    starved.
  - A zero-length **read** in `readable` drops the frontend READABLE event,
    as `expect.rs:183` does. `tcp_socket_read` returns
    `(0, SocketResult::Continue)` for an empty slice, which is what
    `space()` yields once the buffer is full, so without the guard a client
    that declares a large `len` and stalls has `ready_inner` re-enter until
    the outer bound trips.
  - `back_writable`'s `Err` arm distinguishes the retryable kinds.
    `WouldBlock` clears only the stale WRITABLE **event** and keeps the
    interest (as `send.rs::back_writable` does); `Interrupted` touches
    neither word so `ready_inner` retries under its own bound. Only a
    genuine error resets both words. Clearing `interest` on backpressure
    would mean no later epoll edge is ever acted on, stranding a
    half-written header on the backend until the frontend timeout, and
    clearing the `event` on EINTR would strand it the same way — a signal
    is not a socket state change, so edge-triggered epoll owes no new edge
    for it. The `Interrupted` arm is defensive and unreachable in
    production: the backend socket comes from `mio::net::TcpStream::connect`
    (`lib/src/backends.rs:332`), so it is always non-blocking and its `send` answers
    EAGAIN, never EINTR. Exercising it takes a deliberately blocking
    socketpair — see
    `back_writable_keeps_its_readiness_when_a_signal_interrupts_the_write`
    (`relay.rs`).
- `relay.rs` places **no** upper bound on header size, unlike `expect.rs`'s
  fixed 232-byte staging array: a v2 header is `16 + len` with `len: u16`,
  and this state reads into a pool `Checkout`, so a declared `len` larger
  than the session buffer stalls until the frontend timeout. Bounded and
  non-spinning since the zero-read guard, but still unbounded in size.

### 2.3 `SendProxyProtocol`

- Type: `SendProxyProtocol<Front: SocketHandler>` (`send.rs`).
- Used when the front-end accepted a non-PROXY connection but the
  downstream backend expects PROXY-v2. Sōzu synthesises a header from the
  TCP peer pair captured on the frontend socket
  (`peer_addr` / `local_addr` at `send.rs:118-120`).
- Entry point: `SendProxyProtocol::back_writable` (`send.rs`). On first call it builds the
  header lazily (`send.rs:117-140`). The drain loop (`send.rs:145-192`)
  writes until the cursor reaches `header.len()` and returns
  `SessionResult::Upgrade`; partial writes set `WouldBlock` and yield to
  the event loop.

---

## 3. State Machine

```
     pre-protocol idle
            │
            │  (event-loop notices READABLE on frontend)
            ▼
       ─────────────────
       header parse loop
       ─────────────────
            │
   ┌────────┴────────┐
   │                 │
parse error      Incomplete
or oversize          │
   │            (await more bytes)
   ▼                 │
SessionResult::      │   parse OK
Close                │       │
                     ▼       ▼
              header captured: peer pair
              stored, role-specific handoff
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
     expect        relay          send
   (into_pipe)   (back_writable, (back_writable,
   transitions  drains prefix to drains synth
   to Pipe)     backend)         header to backend)
        │              │              │
        ▼              ▼              ▼
            SessionResult::Upgrade
            (proxy swaps in the
             downstream protocol's
             SessionState)
```

The `Pipe` (`lib/src/protocol/pipe.rs`) is the typical downstream — it owns
the bidirectional byte-stream forwarding for TCP listeners. For HTTP(S)
listeners the downstream is the mux `Connection` enum (`ConnectionH1` or
`ConnectionH2`, depending on the negotiated protocol).

---

## 4. Hardening Notes

The PROXY-protocol surface is the very first byte path on a new connection,
which makes it an attractive target. These rules are load-bearing.

1. **Bounded buffers, no growth.** `ExpectProxyProtocol::frontend_buffer`
   is a stack-sized `[u8; 232]` (`expect.rs:83`) — the maximum legal v2
   header size. There is no growable backing — a peer that floods bytes
   without a valid header trips the oversized-header branch
   (`expect.rs:249-259`) and is closed.
2. **TCP healthchecks bypass the protocol.** Upstream LBs probe backends
   with bare TCP (SYN/ACK/FIN) and never send `send-proxy`. The fast-close
   branch in `expect.rs:201-214` handles that gracefully — without it,
   every healthcheck would idle for the full `request_timeout`. Do not
   "fix" that branch without measuring against an HAProxy mesh.
3. **`MAX_LOOP_ITERATIONS` ceiling.** All three modules import
   `MAX_LOOP_ITERATIONS` from `sozu_command::config`
   (e.g. `expect.rs:15`); any drain or read loop must respect it so a
   misbehaving peer cannot starve the single-threaded event loop.
4. **Error counters on every reject.** Every `Close` path bumps the
   `proxy_protocol.errors` counter via `incr!` so operators can alert on a
   sudden spike (e.g. a backend that started rejecting the protocol).
5. **No panic on adversarial input.** Per the repo `CLAUDE.md`
   security-sensitive areas list, the proxy-protocol path must convert
   parse errors / partial reads / oversized headers into
   `SessionResult::Close` plus a metric and a contextual log line. New
   error paths follow the existing pattern (see `expect.rs:263-272`).
6. **`HeaderV1` is dead weight.** Per the `header.rs:47` comment the v1
   variant is never produced or consumed; tests are commented out
   (`header.rs:110-141`). Removing it is a documented follow-up — until
   then, do not extend it.

---

## 5. Cross-References

- `lib/src/protocol/pipe.rs` — the typical downstream after `expect`.
- `lib/src/protocol/mux/LIFECYCLE.md` — the H1/H2 mux that follows PROXY-v2
  ingress on HTTP and HTTPS listeners.
- `lib/src/protocol/kawa_h1/LIFECYCLE.md` — the H1 vocabulary that mux
  builds on (default answers, `HttpContext`, `Method`).
- `bin/src/command/LIFECYCLE.md` — how listener configuration (incl.
  `expect_proxy`, `send_proxy`) is delivered from the supervisor.
- HAProxy upstream spec: <https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt>
