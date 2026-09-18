# PROXY Protocol — Session Workflow

Reference document for maintainers of `lib/src/protocol/proxy_protocol/`.
Companion to `lib/src/protocol/kawa_h1/LIFECYCLE.md` (downstream H1) and
`lib/src/protocol/mux/LIFECYCLE.md` (downstream H2).

Every claim is anchored to a concrete `file.rs:LINE`; line numbers were last
refreshed against the `docs/feat-h2-mux-audit` branch tip on 2026-04-26.

---

## 1. Wire Format Reminder

The PROXY protocol prepends a fixed metadata block to a TCP connection so the
inner protocol (H1 / H2 / TLS / TCP-passthrough) sees the original client
address even after the connection traversed an upstream load balancer.

Two wire versions exist:

- **v1** is text-only. Lines have the shape
  `PROXY TCP4 src_ip dst_ip src_port dst_port\r\n` (or `TCP6` /
  `UNKNOWN\r\n`). See `HeaderV1` (`lib/src/protocol/proxy_protocol/header.rs:56-60`)
  and the comment at `header.rs:47` flagging that **v1 is never used inside
  Sōzu** — only v2 is parsed or emitted on the wire. The v1 serializer
  (`HeaderV1::into_bytes`, `header.rs:81`) survives as dead weight pending the
  documented removal.
- **v2** is binary. The wire frame is a 12-byte signature
  (`0x0D 0x0A 0x0D 0x0A 0x00 0x0D 0x0A 0x51 0x55 0x49 0x54 0x0A`, see
  `header.rs:186-188`) followed by version+command, family, address-block
  length (big-endian `u16`), and the address block. `HeaderV2`
  (`header.rs:150`) carries the parsed shape; `HeaderV2::into_bytes`
  (`header.rs:181`) emits it.

Address families are modelled by `ProxyAddr` (`header.rs:249`): `Ipv4Addr`,
`Ipv6Addr`, `UnixAddr` (108 bytes per side per the AF_UNIX socket-path
limit), and `AfUnspec` for unknown/legacy.

The v2 parser lives in `lib/src/protocol/proxy_protocol/parser.rs`; the
public entry point is `parse_v2_header` (`parser.rs:40`). It uses `nom` and
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
TCP-side consumers — `expect.rs`, `relay.rs`, `tcp_preread/mod.rs`, and
`TcpSession::effective_session_address` (`lib/src/tcp.rs:373`), which reads
`ExpectProxyProtocol::addresses` / `RelayProxyProtocol::addresses` directly
rather than through `into_pipe`, all of which attribute `ProxyAddr::source()`
to the client — hold: none of them reads `command` or `family`, and none
re-serializes the parsed header. The `family` byte is still reported as it was
read off the wire.

Those four are not the only consumers of the parsed pair, and the other two
do not fall back. `HttpSession::upgrade_expect` (`lib/src/http.rs:316`) and
`HttpsSession::upgrade_expect` (`lib/src/https.rs:334`) each need a source
**and** a destination to build the downstream session; `ProxyAddr::AfUnspec`
returns `None` from both accessors (`header.rs:303, 311`), so the upgrade is
refused and `upgrade` reports `SessionIsToBeClosed` (`lib/src/http.rs:254`). An
HTTP or HTTPS session that presents a `LOCAL` header is closed here, not
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
context macros (`log_context!` in each module: `expect.rs:54`,
`relay.rs:45`, `send.rs:47`) so a session is grep-correlatable across the
PROXY phase and the downstream protocol.

### 2.1 `ExpectProxyProtocol`

- Type: `ExpectProxyProtocol<Front: SocketHandler>` (`expect.rs:80`).
- Buffer: `frontend_buffer: [u8; 232]` — the maximum legal v2 header size
  (Unix-socket family carries 2 × 108 bytes of address plus header overhead).
  Hard-bounded to defend against a malicious peer that opens TCP and never
  finishes the header.
- Entry point: `readable` (`expect.rs:118`).
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
    swaps the session for a `Pipe` via `into_pipe` (`expect.rs:280`), which
    prefers `ProxyAddr::source()` and falls back to the front socket's
    `peer_addr` when it is `AfUnspec` — including for every `LOCAL` header
    (§1).

### 2.2 `RelayProxyProtocol`

- Type: `RelayProxyProtocol<Front: SocketHandler>` (`relay.rs:63`).
- Used when Sōzu sits between two PROXY-aware peers: read the inbound
  header, then write those exact bytes (and only those bytes) to the
  backend before any user-payload byte.
- Entry points: `readable` (`relay.rs:116`) feeds the parser; on a complete
  parse it flips `frontend_readiness.interest` to drop READABLE and arms
  the backend WRITABLE bit (`relay.rs:159-160`). `back_writable`
  (`relay.rs:199`) drains the captured prefix on the backend socket and
  returns `SessionResult::Upgrade` once the cursor reaches the recorded
  `header_size`.

### 2.3 `SendProxyProtocol`

- Type: `SendProxyProtocol<Front: SocketHandler>` (`send.rs:65`).
- Used when the front-end accepted a non-PROXY connection but the
  downstream backend expects PROXY-v2. Sōzu synthesises a header from the
  TCP peer pair captured on the frontend socket
  (`peer_addr` / `local_addr` at `send.rs:117-124`).
- Entry point: `back_writable` (`send.rs:110`). On first call it builds the
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
listeners the downstream is built from `Http<Front, L>` + the mux
`Connection` enum, depending on the negotiated protocol.

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
- `lib/src/protocol/kawa_h1/LIFECYCLE.md` — H1 frontend that follows
  PROXY-v2 ingress on HTTP listeners.
- `lib/src/protocol/mux/LIFECYCLE.md` — H2 mux that follows PROXY-v2
  ingress on HTTPS listeners.
- `bin/src/command/LIFECYCLE.md` — how listener configuration (incl.
  `expect_proxy`, `send_proxy`) is delivered from the supervisor.
- HAProxy upstream spec: <https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt>
