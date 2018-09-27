# sozu-command-lib, tools to communicate with the sozu proxy

The sozu proxy can receive dynamic configuration changes through a unix socket.
This library defines the communication protocol, the message format, the required
structures, serialization and deserialization code.

## Command socket message format

The messages are sent as JSON messages, separated by the 0 byte.

All messages contain an identifier which will be used to match the proxy's
response to the order, and a type, to decide how to parse the message.

You send a `CommandRequest`, defined as follows:

```rust
pub struct CommandRequest {
  pub id:        String,
  pub version:   u8,
  pub data:      CommandRequestData,
  pub worker_id: Option<String>,
}
```

When serialized to JSON it looks like this:

```json
{
    "id":        "ID_TEST",
    "version":   0,
    "worker_id": 0,
    "type":      "<CommandRequestData enum name>",
    "data":      { <CommandRequestData wrapped value> }
}"
```

The type indicates which kind of message it wraps. The id field will be used
for answers. Since the protocol is asynchronous, this is used to identify
answers to a specific message (you can receive multiple answers to one message).
The `worker_id` field is used to target a specific worker.

An answer has the format `CommandResponse`, defined as follows:

```rust
pub struct CommandResponse {
  pub id:      String,
  pub version: u8,
  pub status:  CommandStatus,
  pub message: String
}
```

When serialized to JSON it looks like this:

```json
{
  "id":      "ID_TEST",
  "version": 0,
  "status":  "Ok",
  "message": "done"
}
```

An answer can have 3 possible status:

- `Ok`: the task was done
- `Error`: there was an unrecoverable error
- `Processing`: the task was started but may not finish right away

As an example, in a soft shutdown, the shutdown message is sent to all
the workers, and they acknowledge the message by sending an answer
with the `Processing` status: in a soft shutdown, a worker stops accepting
new connections but keeps the active ones and exits when they are no longer
active. Once all connections are done, a worker will send an answer
with the same id and the `Ok` status.

## Message types

### Master messages

#### Save state message

This message tells the proxy to dump the current proxy state (backends,
front domains, certificates, etc) as json, to a file. This file can be used later
to bootstrap the proxy. This message is not forwarded to workers.

The save state message is defined as follows:

```json
{
  "id":   "ID_TEST",
  "version": 0,
  "type": "SAVE_STATE",
  "data": {
    "path": "./config_dump.json"
  }
}
```

If the specified path is relative, it will be calculated relative to the current
working directory of the proxy.

#### Load state message

```json
{
  "id":   "ID_TEST",
  "version": 0,
  "type": "LOAD_STATE",
  "data": {
    "path": "./config_dump.json"
  }
}
```

#### Dump state message

This message will gather the same information as the save state message, but
will write it back to the unix socket.

The dump state message is defined like this:

```json
{
  "id":   "ID_TEST",
  "version": 0,
  "type": "DUMP_STATE"
}
```

#### Soft shutdown

```json
{
  "id":      "ID_TEST",
  "version": 0,
  "data": {
    "type": "SOFT_STOP",
  }
}
```

#### Hard shutdown

```json
{
  "id":      "ID_TEST",
  "version": 0,
  "data": {
    "type": "HARD_STOP",
  }
}
```

#### Status

```json
{
  "id":      "ID_TEST",
  "version": 0,
  "data": {
    "type": "STATUS",
  }
}
```

#### Binary upgrade

```json
{
  "id":      "ID_TEST",
  "version": 0,
  "data": {
    "type": "UPGRADE_MASTER",
  }
}
```

### Worker configuration messages

The proxy's configuration can be altered at runtime, without restarts,
by communication configuration changes through the socket.

The proxy attribute indicates the proxy's name, as defined in the
configuration file, in the section name (ie, `[proxies.HELLO]` creates
the `HELLO` proxy).

The data attribute will contain one of the proxy orders.

#### Adding a new HTTP front

```json
{
  "id":       "ID_TEST",
  "version":  0,
  "data": {
    "type": "ADD_HTTP_FRONT",
    "data": {
      "app_id":     "xxx",
      "hostname":   "example.com",
      "path_begin": "/hello"
    }
  }
}
```

#### Adding a new HTTPS front

```json
{
  "id":       "ID_TEST",
  "version":  0,
  "data": {
    "type": "ADD_HTTPS_FRONT",
    "data": {
      "app_id":      "xxx",
      "hostname":    "example.com",
      "path_begin":  "/hello",
      "fingerprint": "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5"
    }
  }
}
```

The fingerprint is the hexadecimal representation of the SHA256 hash of the certificate.

#### Adding a new backend server

```json
{
  "id":       "ID_ABCD",
  "version":  0,
  "data":{
    "type": "ADD_BACKEND",
    "data": {
      "app_id": "xxx",
      "ip_address": "127.0.0.1",
      "port": 8080
    }
  }
}
```

#### Removing a HTTP frontend

```json
{
  "id":       "ID_ABCD",
  "version":  0,
  "data": {
    "type": "REMOVE_HTTP_FRONT",
    "app_id": "xxx",
    "hostname": "yyy",
    "path_begin": "xxx",
    "port": 4242
  }
}
```

#### Removing a HTTPS frontend

```json
{
  "id":      "ID_TEST",
  "version": 0,
  "data": {
    "type": "REMOVE_HTTPS_FRONT",
    "data": {
      "app_id":      "xxx",
      "hostname":    "yyy",
      "path_begin":  "xxx",
      "fingerprint": "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5"
    }
  }
}
```

#### Removing a backend server

```json
{
  "id":      "ID_789",
  "version": 0,
  "data": {
    "type": "REMOVE_BACKEND",
    "data": {
      "app_id": "xxx",
      "ip_address": "127.0.0.1",
      "port": 8080
    }
  }
}
```

#### Adding a new certificate

```json
{
  "id":      "ID_789",
  "version": 0,
  "data": {
    "type": "ADD_CERTIFICATE",
    "data": {
      "certificate": "<PEM certificate>",
      "certificate_chain": ["<PEM certificate>", "<PEM certificate>"],
      "key": "<PEM private key>"
    }
  }
}
```

#### Removing a certificate

```json
{
  "id":      "ID_789",
  "version": 0,
  "data": {
    "type": "REMOVE_CERTIFICATE",
    "data": "ab2618b674e15243fd02a5618c66509e4840ba60e7d64cebec84cdbfeceee0c5"
  }
}
```

