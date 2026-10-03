# Configuring sozu via command line

The sozu executable can be used to start the proxy, and configure it: adding new backend servers, reading metrics, etc.
It talks to the currently running proxy through a unix socket.

You can specify its path by adding to your `config.toml`:

```toml
command_socket = "path/to/your/command_folder/sock"
```

## Add a cluster with an http and https frontends

First you need to create a new cluster with an id and a load balancing policy (`round_robin`, `random`,
`power_of_two`, `least_loaded`, `hrw` or `maglev`):

```bash
sozu --config /etc/sozu/config.toml cluster add --id <my_cluster_id> --load-balancing-policy round_robin
```

`hrw` and `maglev` pin each client to one backend, keyed on its source IP (on a
UDP cluster, the source IP or source IP and port, per its `affinity_key`). On an
HTTP/HTTPS cluster, `--affinity-header <name>` or `--affinity-cookie <name>` keys
it on that request header or cookie instead, falling back to the source IP when a
request does not carry it:

```bash
sozu --config /etc/sozu/config.toml cluster add --id <my_cluster_id> --load-balancing-policy hrw --affinity-header X-Tenant
```

`--shard-percent <1..=100>` shuffle-shards the cluster: each client is served by
the top `max(2, ceil(percent × N / 100))` backends of the HRW ranking of its key,
and the load-balancing policy picks inside that shard. `--shard-min-backends`
(default 8) is the backend count from which sharding applies, and
`--shard-strict` answers 503 instead of spilling over when a client's whole
shard is down (see "Shuffle sharding" in `doc/configure.md`):

```bash
sozu --config /etc/sozu/config.toml cluster add --id <my_cluster_id> --load-balancing-policy least_loaded --shard-percent 25
```

To create a cluster with HTTP/2 backend connections enabled:

```bash
sozu --config /etc/sozu/config.toml cluster add --id <my_cluster_id> --load-balancing-policy round_robin --http2
```

It won't show anything but you can verify that the cluster has been added successfully by querying sozu:

```bash
sozu --config /etc/sozu/config.toml query clusters
```

Then you need to add a backend:

```bash
sozu --config /etc/sozu/config.toml backend add --address 127.0.0.1:3000 --backend-id <my_backend_id> --id <my_cluster_id>
```

A backend is identified by its backend id and address together. Two backends
of a cluster may share an address under distinct ids (an A/B variant, for
instance): `backend remove --id <cluster_id> --backend-id <id> --address <address>` removes only
the backend with that id at that address, from the saved state and from the
workers alike, and the other one keeps taking traffic.

### Add http frontend

And an http listener:

```bash
sozu --config /etc/sozu/config.toml listener http add --address 0.0.0.0:80 --tls-versions TLSv1.2 --tls-cipher-list ECDHE-ECDSA-AES256-GCM-SHA384 --tls-cipher-suites TLS_AES_256_GCM_SHA384 --tls-signature-algorithms ECDSA+SHA512 --tls-groups-list x25519 --expect-proxy
```

Finally you have to create a frontend to allow sozu to send traffic from the listener to your backend:

```bash
sozu --config /etc/sozu/config.toml frontend http add --address 0.0.0.0:80 --hostname <my_cluster_hostname> id <my_cluster_id>
```

To route only a sub-path of a hostname to this cluster, add `--path-prefix`
(use `--path-regex` or `--path-equals` for the other matching modes — see
"Path matching precedence within a frontend" in `doc/configure.md`):

```bash
sozu --config /etc/sozu/config.toml frontend http add --address 0.0.0.0:80 --hostname <my_cluster_hostname> --path-prefix /api id <my_cluster_id>
```

### Add https frontend

And an https listener:

```bash
sozu --config /etc/sozu/config.toml listener https add --address 0.0.0.0:443
```

Finally you have to create a frontend to allow sozu to send traffic from the listener to your backend:

```bash
sozu --config /etc/sozu/config.toml frontend https add --address 0.0.0.0:443 --hostname <my_cluster_hostname> id <my_cluster_id>
```

## Enable or disable HTTP/2 for backend connections

You can toggle HTTP/2 for backend connections on an existing cluster at runtime:

```bash
sozu --config /etc/sozu/config.toml cluster h2 enable --id <my_cluster_id>
sozu --config /etc/sozu/config.toml cluster h2 disable --id <my_cluster_id>
```

This queries the current cluster configuration, updates the `http2` flag, and re-applies it
to all workers without affecting other cluster settings.

## Set the backend connection attempt budget

A request may try `max_connection_attempts` backend connections, the first included, before it
is answered `503` (a TCP session is closed instead); a connection that is refused, times out on `connect_timeout` or reports a
socket error is retried on another backend while attempts remain (5 by default, 1 to 255). See
"Backend connection failover" in `doc/configure.md`.

Per cluster, when the cluster is added or on an existing cluster:

```bash
sozu --config /etc/sozu/config.toml cluster add --id <my_cluster_id> --load-balancing-policy round_robin --max-connection-attempts 3
sozu --config /etc/sozu/config.toml cluster connection-attempts set --id <my_cluster_id> 3
sozu --config /etc/sozu/config.toml cluster connection-attempts unset --id <my_cluster_id>
```

`set` and `unset` query the current cluster configuration and re-apply it with only
`max_connection_attempts` changed, as `cluster h2` does for `http2`; `unset` makes the cluster use
the global budget again. The value is part of the cluster's state and is saved with it.

Globally, on every running worker:

```bash
sozu --config /etc/sozu/config.toml connection-attempts set 5
```

A cluster's own budget takes precedence. The global change is not saved: a worker started later
reads `max_connection_attempts` from the configuration file, so mirror the change there.

## Configure active health checks

```bash
sozu --config /etc/sozu/config.toml cluster health-check set --id <my_cluster_id> --uri /health --accepted-statuses 200-399,404
sozu --config /etc/sozu/config.toml cluster health-check set --id <my_cluster_id> --mode tcp
sozu --config /etc/sozu/config.toml cluster health-check list
sozu --config /etc/sozu/config.toml cluster health-check remove --id <my_cluster_id>
```

`--mode` is `http` (the default) or `tcp`, which only checks that a TCP connection is
established. The probe timeout is `--probe-timeout` (seconds); `--timeout` is the global
command timeout in milliseconds. See `doc/health_checks.md` for every flag and default.

## Remove a cluster

```bash
sozu --config /etc/sozu/config.toml cluster remove --id <my_cluster_id>
```

Removing a cluster also removes every frontend (HTTP, HTTPS, TCP and UDP) and every backend
that names it, in the main process state and in every worker. A request to one of its
hostnames then gets the listener's 404, unless another cluster's wildcard or catch-all route
also covers that hostname: that route then serves it. A new connection on one of its TCP
listeners is closed. A session established before the removal keeps its backend connection
until it closes. A frontend that names no cluster (a deny or answer route) stays, even on the
removed cluster's hostname.

There is no separate `frontend … remove` or `backend remove` to send afterwards. Sent anyway,
it answers ok and changes nothing, since its object went with the cluster; it never removes an
object another cluster owns, even one that took over the same address, hostname and path. To
serve the cluster again, add the cluster first, then its frontends and backends.

A frontend or backend removal naming a cluster that does not exist, because it was removed or
never added (a typo after `id`, or `id <cluster>` where the route was added with `deny`), gets the
same ok answer and
removes nothing. The answer says so, and names the cluster, or the route with no cluster, that
holds the same key:

```text
nothing to remove: cluster my_clstr does not exist (removed, or never added), and none of its
frontends or backends either; this key belongs to cluster my_cluster, left in place
```

To remove that route, send the removal again with `id <cluster>` for the cluster it names, or
with `deny` for a route with no cluster.

## Check the status of sozu

It shows a list of workers and show information about their statuses.

```bash
sozu --config /etc/sozu/config.toml status
```

## Get metrics and statistics

It will show global statistics about sozu, workers and clusters metrics.

```bash
sozu --config /etc/sozu/config.toml query metrics
```

## Query certificates

```bash
# every certificate the main process knows about
sozu --config /etc/sozu/config.toml certificate list

# the certificate with this fingerprint
sozu --config /etc/sozu/config.toml certificate list --fingerprint <hex>

# the certificate Sōzu would present for this host
sozu --config /etc/sozu/config.toml certificate list --domain foo.example.com

# ask the workers instead of the main process (slower)
sozu --config /etc/sozu/config.toml certificate list --domain foo.example.com --workers
```

`--domain` answers **"which certificate would Sōzu present for this host?"**,
not "which certificates carry this exact SAN?". The host is resolved through
the same SNI trie the TLS resolver uses, so a `*.example.com` certificate
answers a query for `foo.example.com`. Consequences of that reading:

- The lookup is **wildcard-aware but not a prefix or substring search.**
  `*.example.com` answers for `foo.example.com`, and not for the apex
  `example.com`, nor for `deep.foo.example.com` — `*` covers exactly one
  label. Regex hostname labels resolve the same way they do at handshake.
- The host and the stored certificate names are compared **ASCII
  case-insensitively** (RFC 4343), so `--domain FOO.Example.COM` and
  `--domain foo.example.com` answer identically.
- **At most one certificate per HTTPS listener** comes back, because a trie
  lookup resolves to a single entry — the answer is the union over listeners,
  one entry each. A host served by two certificates on the *same* listener
  therefore reports one of them. The main process does not retain certificate
  expiry, so it cannot reproduce the worker resolver's longest-lived tie-break
  and reports a reproducible choice instead; add `--workers` to see the
  certificate each worker's resolver actually selected.
- Passing `--fingerprint` and `--domain` together is not a conjunction:
  `--domain` wins and `--fingerprint` is ignored. The one exception is
  `--fingerprint <hex> --domain <host> --workers`, where the worker answers
  the fingerprint query by **exact SAN equality** on the domain instead
  (`lib/src/server.rs` intercepts any filter carrying a fingerprint). Query
  one filter at a time.

To ask the other question — "which certificates carry this exact SAN?" —
there is no flag today; list everything and filter on `names` with
`--json`.

## Dump and restore state

If sozu configurations (clusters, frontends & backends) are not written in the config file, you can save sozu state to restore it later.

```bash
sozu --config /etc/sozu/config.toml state save --file state.json
```

Then shutdown gracefully sozu:

```bash
sozu --config /etc/sozu/config.toml shutdown
```

A soft shutdown returns once every worker has finished its in-flight requests and prints the
final result, however long the drain lasts: it does not apply `--timeout` or
`ctl_command_timeout`. `shutdown --hard` does not wait for the requests and keeps the timeout.
The commands that apply the timeout wait `--timeout` milliseconds, or `ctl_command_timeout` when
the flag is absent, and `0` disables that timeout.

Restart sozu and restore its state:

```bash
sozu --config /etc/sozu/config.toml state load --file state.json
```

You should be able to request your cluster like before the shutdown.

### Monitor status of backends with events

This CLI command:

```bash
sozu --config /path/to/config.toml events
```

listens to events sent by Sōzu workers whenever a backend is down, up again,
or when no backend is available.

## Live operator TUI (`sozu top`)

The `top` subcommand is a btop/htop-style live dashboard. Build with the
optional `tui` Cargo feature (`cargo build -p sozu --features tui --release`);
`sozu --version` reports `+tui` when the subcommand is linked in. See
[`doc/sozu-top.md`](sozu-top.md) for the full operator guide (panes, key
bindings, skin format, threshold tuning).

```bash
sozu --config /path/to/config.toml top
```

Common flags:

| Flag | Effect |
|------|--------|
| `--refresh-ms <N>` | Data poll cadence in milliseconds (default `1000`). |
| `--detail <DETAIL>` | Cardinality lease level (`process|frontend|cluster|backend`, default `backend`). |
| `--lease-ttl-seconds <N>` | Lease TTL; auto-renewed at half-TTL (default `60`, server clamps at `300`). |
| `--skin <NAME>` | Resolve `$XDG_CONFIG_HOME/sozu/skins/<NAME>.toml` (`SOZU_TOP_SKIN` env wins). |
| `--glyphs <MODE>` | Force a glyph mode (`braille|block|tty`); auto-detect by default. |
| `--no-mouse` | Disable SGR mouse capture (helps with multiplexers that mis-route mouse events). |
| `--snapshot <N>`, `--tick-once` | Render N frames / one tick and exit (test affordances). `--snapshot` takes no terminal control and renders fixed 80x24 frames to stdout. |

Key bindings (operator quick reference; see `doc/sozu-top.md` for the
full list):

- `1`-`7` jumps to OVERVIEW · CLUSTERS · BACKENDS · LISTENERS · CERTS · H2 · EVENTS.
- `Tab` / `Shift-Tab` cycles tabs forward / backward.
- `s` / `S` cycles / reverses the sort column on CLUSTERS and BACKENDS.
- `q` / `Q` / `Ctrl-C` / `F10` quits, `?` / `F1` toggles help.
