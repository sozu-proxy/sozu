# Lexicon

What terms are used in Sōzu.

## User-facing

*backend*: a socket address towards which Sōzu will redirect traffic.

*cluster*: a set of frontends, routing rules, and backends.

*frontend*: a socket address on which a cluster receives connections. Should be defined with a matching listener.

*listener*: a socket address on which Sōzu will accept incoming connections.

*worker*: an instance of Sōzu, managed by the main process.

## Under the hood

*session*: smallest unit to handle the link between a frontend and a backend. There is roughly one session for each
incoming request.

*proxy*: a unit that handles all traffic for a given protocol. In each Sōzu server there is a TCP proxy,
an HTTP proxy and an HTTPS proxy.