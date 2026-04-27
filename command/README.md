# sozu-command-lib, tools to communicate with the Sōzu proxy

The sozu proxy can receive dynamic configuration changes through a unix socket.
This library defines the communication protocol, the message format, the required
structures, serialization and deserialization code.

## Command messages are defined in protobuf

Protobuf is a language-agnostic, binary serialization format
used to efficiently transmit structured data between different systems and languages.

The idea is to define the data _once_ in this format, so that various libraries
of various languages can translate it to their own.

All types are defined in the `command.proto` file.
There are two main types received by, and sent from, Sōzu:

- `Request`
- `Response`

They look like this, in protobuf:

```protobuf
// A message received by Sōzu to change its state or query information
message Request {
  oneof request_type {
    // save Sōzu's parseable state as a file, with a path
    string save_state = 1;
    // load a state file, given its path
    string load_state = 2;
    /*
    many more — see command/src/command.proto
    */
  }
}
```

```protobuf
// Response to a request
message Response {
    // wether the request was a success, a failure, or is processing
    required ResponseStatus status = 1 [default = FAILURE];
    // a success or error message
    required string message = 2;
    // response data, if any
    optional ResponseContent content = 3;
}
```

These are serialized in binary, NOT in plain text formats like JSON.

A response can have 3 possible status:

- `Ok`: the task was done
- `Failure`: there was an unrecoverable error
- `Processing`: the task was started but may not finish right away

As an example, in a soft shutdown, the shutdown message is sent to all
the workers, and they acknowledge the message by sending an answer
with the `Processing` status: in a soft shutdown, a worker stops accepting
new connections but keeps the active ones and exits when they are no longer
active. Once all connections are done, a worker will send an answer
with the same id and the `Ok` status.

## Control-plane audit log

Every privileged mutation accepted by the unix command socket is mirrored to a
dedicated audit envelope rendered in the canonical `[session req cluster
backend]\tAUDIT\tCommand(...)` layout. Two optional sinks ship today:
`audit_logs_target` (text, ANSI-stripped, mirrors the `info!` line) and
`audit_logs_json_target` (one stable-schema JSON object per line, ready for SIEM
ingest). Both sinks open `O_APPEND | O_CREAT | chmod 0640` and create new parent
directories with mode `0o750` so PCI-DSS 10.5 remains enforceable across
restarts. Free-form fields (`target`, `actor.user`, `actor.comm`, `socket`,
`extras.reason`) are sanitised before render so attacker-influenced input cannot
forge additional audit lines via embedded `\t` / `\n` / ANSI escapes.

The optional `command_allowed_uids: Vec<u32>` config field rejects any
`SO_PEERCRED` UID outside the list at the top of `Server::handle_client_request`
(defence in depth on top of the `0o600` socket mode). Actor identity is
captured via `SO_PEERCRED` and enriched on accept with `/proc/<pid>/comm` and
NSS `getpwuid_r(uid)`; `peer_comm` opens `/proc/<pid>/stat` first so a recycled
PID returns `None` rather than the new owner's `comm`, and `peer_user` caches
results in a 16-entry process-local LRU so an SSSD/LDAP wedge cannot stall the
main event loop.

The full schema, retention policy, and supervisor-side lifecycle (accept loop,
fan-out across workers, FD-passing for hot upgrades) live in
`doc/observability.md` and the module-level comments in `bin/src/command/`.

