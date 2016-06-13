# Message format

The proxy receives order through a unix socket. The path to this unix socket can
be defined by the `command_socket` option in the TOML configuration file.

The messages are sent as JSON messages, separated by the 0 byte.

All messages contain an identifier which will be used to match the proxy's
response to the order, and a type, to decide how to parse the message.

## Save state message

This message tells the proxy to dump the current proxy state (instances,
front domains, certificates, etc) as json, to a file. This file can be used later
to bootstrap the proxy.

The save state message is defined as follows:

```json
{
  "id":   "ID_TEST",
  "type": "SAVE_STATE",
  "data": {
    "path": "./config_dump.json"
  }
}
```

If the specified path is relative, it will be calculated relative to the current
working directory of the proxy.

## Dump state message

This message will gather the same information as the save state message, but
will write it back to the unix socket.

The dump state message is defined like this:

```json
{ 
  "id":   "ID_TEST",
  "type": "DUMP_STATE"
}
```

## Proxy configuration messages

The proxy's configuration can be altered at runtime, without restarts,
by communication configuration changes through the socket.

Here is an example of one of those messages:

```json
{
  "id":       "ID_TEST",
  "type":     "PROXY",
  "listener": "HTTP",
  "data": {
    "type": "ADD_HTTP_FRONT",
    "data": {
      "app_id":     "xxx",
      "hostname":   "yyy",
      "path_begin": "xxx",
      "port":       4242
    }
  }
}
```

The listener attribute indicates the listener's name, as defined in the
configuration file, in the section name (ie, `[listeners.HELLO]` creates
the `HELLO` listener).

The data attribute will contain one of the proxy orders.

TODO: specify the proxy orders, defined in ../src/messages.rs
