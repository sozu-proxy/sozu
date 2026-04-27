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
  `header.rs:159-161`) followed by version+command, family, address-block
  length (big-endian `u16`), and the address block. `HeaderV2`
  (`header.rs:139`) carries the parsed shape; `HeaderV2::into_bytes`
  (`header.rs:156`) emits it.

Address families are modelled by `ProxyAddr` (`header.rs:187`): `Ipv4Addr`,
`Ipv6Addr`, `UnixAddr` (108 bytes per side per the AF_UNIX socket-path
limit), and `AfUnspec` for unknown/legacy.

The v2 parser lives in `lib/src/protocol/proxy_protocol/parser.rs`; the
public entry point is `parse_v2_header` (`parser.rs:33`). It uses `nom` and
returns either a complete `HeaderV2`, an `Incomplete` request for more
bytes, or a parse error.

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
context macros (`log_context!` in each module: `expect.rs:53`,
`relay.rs:44`, `send.rs:46`) so a session is grep-correlatable across the
PROXY phase and the downstream protocol.

### 2.1 `ExpectProxyProtocol`

- Type: `ExpectProxyProtocol<Front: SocketHandler>` (`expect.rs:79`).
- Buffer: `frontend_buffer: [u8; 232]` — the maximum legal v2 header size
  (Unix-socket family carries 2 × 108 bytes of address plus header overhead).
  Hard-bounded to defend against a malicious peer that opens TCP and never
  finishes the header.
- Entry point: `readable` (`expect.rs:117`).
  - `header_len` (`expect.rs:118-122`) tracks the expected read window;
    starts at the v4 size (28 bytes), bumps to v6 (52) and finally Unix
    (232) if `parse_v2_header` returns `Incomplete` after the prior cap.
  - 0-byte read with `index == 0` (`expect.rs:163-175`) closes the session
    immediately; this is the standard HAProxy bare-TCP healthcheck pattern
    (SYN/ACK/FIN with no `send-proxy`). Closing fast avoids zombie sessions
    sitting on `request_timeout` (default 10 s) and consuming the
    `nb_connections` quota.
  - Index of 232 with the parser still `Incomplete` (`expect.rs:203-212`)
    is the oversized-header sentinel — increment the
    `proxy_protocol.errors` metric and close.
  - Successful parse (`expect.rs:181-190`) stores the `ProxyAddr` into
    `self.addresses` and returns `SessionResult::Upgrade`; the proxy then
    swaps the session for a `Pipe` via `into_pipe` (`expect.rs:234`).

### 2.2 `RelayProxyProtocol`

- Type: `RelayProxyProtocol<Front: SocketHandler>` (`relay.rs:62`).
- Used when Sōzu sits between two PROXY-aware peers: read the inbound
  header, then write those exact bytes (and only those bytes) to the
  backend before any user-payload byte.
- Entry points: `readable` (`relay.rs:106`) feeds the parser; on a complete
  parse it flips `frontend_readiness.interest` to drop READABLE and arms
  the backend WRITABLE bit (`relay.rs:131-135`). `back_writable`
  (`relay.rs:158`) drains the captured prefix on the backend socket and
  returns `SessionResult::Upgrade` once the cursor reaches the recorded
  `header_size`.

### 2.3 `SendProxyProtocol`

- Type: `SendProxyProtocol<Front: SocketHandler>` (`send.rs:64`).
- Used when the front-end accepted a non-PROXY connection but the
  downstream backend expects PROXY-v2. Sōzu synthesises a header from the
  TCP peer pair captured on the frontend socket
  (`peer_addr` / `local_addr` at `send.rs:117-124`).
- Entry point: `back_writable` (`send.rs:109`). On first call it builds the
  header lazily (`send.rs:116-131`). The drain loop (`send.rs:135-159`)
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
   is a stack-sized `[u8; 232]` (`expect.rs:82`) — the maximum legal v2
   header size. There is no growable backing — a peer that floods bytes
   without a valid header trips the oversized-header branch
   (`expect.rs:203-212`) and is closed.
2. **TCP healthchecks bypass the protocol.** Upstream LBs probe backends
   with bare TCP (SYN/ACK/FIN) and never send `send-proxy`. The fast-close
   branch in `expect.rs:163-175` handles that gracefully — without it,
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
   error paths follow the existing pattern (see `expect.rs:217-225`).
6. **`HeaderV1` is dead weight.** Per the `header.rs:47` comment the v1
   variant is never produced or consumed; tests are commented out
   (`header.rs:99` onwards). Removing it is a documented follow-up — until
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
