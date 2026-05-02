# Sōzu unix-socket protocol

This document describes the wire format used between Sōzu's master process
and its CLI clients over the `command_socket` unix domain socket configured
in the main TOML. The reference client is the `sozu` binary itself in
client mode; third-party clients can be written against the protobuf
schema using any language with a `prost`-compatible code generator.

It addresses [#1155](https://github.com/sozu-proxy/sozu/issues/1155).

> **Status**: structural reference. The protobuf schema in
> `command/src/command.proto` is the source of truth; this document
> covers the framing, lifecycle, and authentication discipline that are
> not directly captured by the schema. Wire-level field numbers and
> message shapes are best read straight from
> [`command/src/command.proto`](../command/src/command.proto).

## At a glance

- **Transport**: unix domain stream socket (`AF_UNIX`, `SOCK_STREAM`),
  mode `0o600` by default.
- **Authentication**: `SO_PEERCRED` UID check (and the optional
  `command_allowed_uids` allowlist) at `accept(2)` time. The socket file
  permission is itself a coarse gate; UIDs that pass kernel-level access
  control still face the allowlist.
- **Framing**: 8-byte little-endian length-prefix followed by a protobuf
  message. The length value includes the prefix itself.
- **Message types**: `Request` (client → master) and `Response` (master
  → client), both `oneof`-typed in [`command/src/command.proto`](../command/src/command.proto).
- **File-descriptor passing**: a separate side-band channel uses
  `SCM_RIGHTS` (see `command/src/scm_socket.rs`) to hand off listener
  fds during hot upgrades. Regular CLI traffic does not use SCM.

## Wire frame

Each frame on the wire is:

```
+--------------------------+----------------------+
| length: u64 little-endian| protobuf payload     |
| (8 bytes)                | (length − 8 bytes)   |
+--------------------------+----------------------+
```

- `length` is `sizeof(usize)` bytes — 8 on every supported platform
  (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-freebsd`).
- `length` is the **total** frame length **including** the prefix
  itself. A 100-byte protobuf body has `length = 108`.
- The receiver bounds `length` by `max_buffer_size` (default 2 MiB,
  configured via the `max_command_buffer_size` global TOML key) to
  defeat allocation-pressure attacks. Frames declaring a length above
  the cap are rejected with `MessageTooLarge` and the channel is
  closed.

The reference Rust implementation lives at
[`command/src/channel.rs`](../command/src/channel.rs); the helpers
`Channel::write_message`, `Channel::read_message`, and the
`delimiter_size()` constant are the entry points.

## Message types

### Client → master (`Request`)

```
message Request {
  oneof request_type {
    AddCluster                add_cluster                = 1;
    RemoveCluster             remove_cluster             = 2;
    AddBackend                add_backend                = 3;
    AddCertificate            add_certificate            = 16;
    ReplaceCertificate        replace_certificate        = 17;
    RemoveCertificate         remove_certificate         = 18;
    AddHttpListener           add_http_listener          = 32;
    AddHttpsListener          add_https_listener         = 33;
    UpdateHttpListenerConfig  update_http_listener       = 34;
    AddHttpFrontend           add_http_frontend          = 36;
    SubscribeEvents           subscribe_events           = 41;
    SetMaxConnectionsPerIp    set_max_connections_per_ip = 50;
    SetHealthCheck            set_health_check           = 52;
    // … see command/src/command.proto for the full set
  }
  string id = 99;  // request correlation id (echoed in Response)
}
```

The `id` field is operator-supplied (a UUIDv4 is a sensible default).
The master echoes it on every response that correlates back to this
request.

### Master → client (`Response`)

```
message Response {
  string id            = 1;  // echoed from Request.id
  ResponseStatus status = 2; // OK, FAILURE, PROCESSING
  string message       = 3;  // free-form diagnostic
  ResponseContent content = 4;  // oneof — see command.proto
}
```

`ResponseStatus::PROCESSING` is sent when a request fans out to all
workers and the master is waiting on per-worker replies. The terminal
response is `OK` or `FAILURE`.

## Lifecycle

### Connect

```
client                                      master
  | --- connect(unix:/var/lib/sozu.sock) ----> |
  | <-- accept (kernel SO_PEERCRED captures UID) |
  | <-- master applies command_allowed_uids -- |
```

If the connecting UID is not in `command_allowed_uids` (when set), the
master closes the socket immediately.

### Request / response

```
client                                      master
  | --- Request{id="...", oneof=AddCluster}  -> |
  | <-- Response{id="...", status=PROCESSING}   |
  | <-- Response{id="...", status=OK}            |
```

Some verbs fan out to N workers. The master sends one
`status=PROCESSING` per worker reply, and one terminal `OK` / `FAILURE`
once aggregated.

### Subscribe events (server-push)

```
client                                      master
  | --- Request{id="X", oneof=SubscribeEvents} -> |
  | <-- Response{id="X", status=OK}                |
  | <-- Response{id="evt-N", content=Event{...}}   |
  | <-- Response{id="evt-N+1", content=Event{...}} |
  | … indefinite stream of events                  |
```

Event delivery uses the same frame format. The 22 `EventKind` variants
emitted by the audit-log family (`CLUSTER_ADDED`, `FRONTEND_REMOVED`,
`HEALTH_CHECK_HEALTHY`, `MAIN_UPGRADED`, etc.) are described in
[`doc/observability.md`](observability.md).

### Disconnect

Either side can close the underlying socket. Pending PROCESSING fans
abandoned by the client are still completed master-side (the worker
fan-out is not cancellable mid-flight) but the response is dropped.

## Auto-generated reference (planned)

A follow-up commit can run `protoc-gen-doc` against
`command/src/command.proto` so the auto-generated
`command/src/proto/command.rs` is paired with a fresh wire-level
reference document.

Until then, the canonical wire reference is `command/src/command.proto`
itself; this document covers the framing, lifecycle, and auth that are
not visible in the schema.

## Examples

### Pseudo-code (Python)

```python
import socket
import struct
from sozu_command_pb2 import Request, AddCluster, Cluster, Response

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("/var/lib/sozu.sock")

req = Request(
    id="my-request-1",
    add_cluster=AddCluster(cluster=Cluster(cluster_id="example", protocol="HTTP")),
)
body = req.SerializeToString()
length = 8 + len(body)                    # delimiter is 8 bytes
s.sendall(struct.pack("<Q", length) + body)  # little-endian u64 prefix

# Receive responses until terminal status
while True:
    prefix = s.recv(8)
    (length,) = struct.unpack("<Q", prefix)
    body = b""
    while len(body) < length - 8:
        body += s.recv(length - 8 - len(body))
    resp = Response()
    resp.ParseFromString(body)
    print(resp)
    if resp.status in (Response.OK, Response.FAILURE):
        break
```

The `sozu_command_pb2` module is generated from `command.proto` via
`protoc --python_out=...`; pin the schema to a specific Sōzu release
to avoid mid-upgrade mismatches.

### Rust

A typed Rust client can be built directly on top of `sozu-command-lib`
(the same crate the binary uses). For an external project the pattern
is: depend on `sozu-command-lib`, build a `Channel<Request, Response>`
over `UnixStream`, call `write_message` / `read_message`. The crate
already exposes `Channel::generate(buffer_size, max_buffer_size)` and
the prost-generated `Request` / `Response` types.

## Hardening / observability

- **Allocation pressure**: `max_command_buffer_size` (default 2 MiB)
  bounds the per-channel buffer.
- **UID allowlist**: `command_allowed_uids: Vec<u32>` rejects
  non-allowlisted UIDs at the socket boundary.
- **Audit log v2** records every privileged mutation with actor
  attribution (UID/GID/PID/comm via `SO_PEERCRED` + `/proc/<pid>/stat`)
  and a 16-hex SHA-256 fingerprint for replay correlation. See the
  control-plane audit log section of [`doc/observability.md`](observability.md).
- **Mode 0o600** on the socket file blocks cross-UID access at the
  kernel level. Combine with `command_allowed_uids` for defence in
  depth when multiple daemons run as the same UID.

## Related

- [`command/src/command.proto`](../command/src/command.proto) — schema source of truth
- [`command/src/channel.rs`](../command/src/channel.rs) — Rust framing implementation
- [`command/src/scm_socket.rs`](../command/src/scm_socket.rs) — SCM_RIGHTS fd-passing for hot upgrades
- [`bin/src/command/server.rs`](../bin/src/command/server.rs) — master-side command server
- [`bin/src/command/LIFECYCLE.md`](../bin/src/command/LIFECYCLE.md) — supervisor lifecycle, audit-log envelope
- [`doc/observability.md`](observability.md) — audit log schema + retention
- [`doc/upgrade/1.x-to-2.0.md`](upgrade/1.x-to-2.0.md) — v1.1.x → v2.0.0 migration guide
