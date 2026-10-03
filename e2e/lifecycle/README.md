# Lifecycle reproducers

These scripts exercise lifecycle boundaries that need a real Sōzu process. They
write only beneath a unique directory in `${TMPDIR:-/tmp}` and stop only the
processes they started.

## H1 keep-alive sticky cookie

`keepalive_cookie.py` sends a sticky request followed by a non-sticky request on
the same H1 connection, with a fresh non-sticky connection as the control. It
expects the second response to route to the non-sticky backend without a stale
`Set-Cookie` header. The paired unit regression is
`reset_clears_the_sticky_session_answer_of_the_previous_request`.

```console
cargo build -p sozu
python3 e2e/lifecycle/keepalive_cookie.py \
  --sozu target/debug/sozu \
  --output /tmp/sozu-h1
```

Pass `--output /tmp/another-parent` to select a different parent outside the
checkout, but keep it short enough for the generated Unix command-socket path.
The script prints the exact run directory containing `result.json`, the
generated configuration, and Sōzu's log.

## TCP upgrade with a live stream

`tcp_upgrade/run.sh` starts a TCP proxy, keeps one stream open across a worker
upgrade, and checks both the open stream and a new connection. The current
soft-stop contract closes the pre-upgrade stream; this harness records that
documented limitation as an observation while requiring worker replacement,
new-session service, process retirement, and shutdown to complete.

```console
timeout 180s e2e/lifecycle/tcp_upgrade/run.sh
```

See [`tcp_upgrade/README.md`](tcp_upgrade/README.md) for prerequisites and the
expected observations.
