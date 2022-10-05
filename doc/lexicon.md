# Lexicon

What terms are used in Sōzu.

## User-facing

*backend*: a socket address towards which Sōzu will redirect traffic.

*cluster*: a set of frontends, routing rules, and backends.

*frontend*: a socket address on which a cluster receives connections. Should be defined with a matching listener.

*listener*: a socket address on which Sōzu will accept incoming connections.