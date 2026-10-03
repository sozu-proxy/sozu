#!/usr/bin/env bash
set -euo pipefail

scratch=$(cd "$(dirname "$0")" && pwd)

pid_is_live() {
  local pid=$1
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && ! grep -q ') Z ' "/proc/$pid/stat" 2>/dev/null
}

assert_pid_exited() {
  local label=$1
  local pid=$2
  if pid_is_live "$pid"; then
    printf '%s pid %s is still live\n' "$label" "$pid" >&2
    return 1
  fi
}

if [[ ${1:-} == --self-test-live-pid-assertion ]]; then
  assert_pid_exited 'live witness' "$$"
  exit 99
fi

base=${BASE_BINARY:?Set BASE_BINARY to the absolute path of the starting sozu binary}
candidate=${CANDIDATE_BINARY:?Set CANDIDATE_BINARY to the absolute path of the replacement sozu binary}
runtime=$(mktemp -d "${TMPDIR:-/tmp}/sozu-tcp-upgrade.XXXXXX")
runtime_binary="$runtime/bin/sozu"
config="$runtime/config.toml"

backend_pid=
client_pid=
old_main_pid=
new_main_pid=
old_worker_pid=
new_worker_pid=
upgrade_pid=

cleanup() {
  set +e
  if [[ -S "$runtime/sozu.sock" && -x "$runtime_binary" ]]; then
    "$runtime_binary" --config "$config" --timeout 20000 shutdown --hard >"$runtime/cleanup-shutdown.log" 2>&1
  fi
  for pid in "$upgrade_pid" "$client_pid" "$new_worker_pid" "$old_worker_pid" "$new_main_pid" "$old_main_pid" "$backend_pid"; do
    if pid_is_live "$pid"; then
      kill "$pid" 2>/dev/null
      wait "$pid" 2>/dev/null
    fi
  done
  printf 'Evidence: %s\n' "$runtime"
}
trap cleanup EXIT

test -x "$base"
test -x "$candidate"
mkdir -p "$runtime/bin"
install -m 0755 "$base" "$runtime_binary"
install -m 0755 "$candidate" "$runtime/bin/sozu.candidate"

python3 "$scratch/backend.py" "$runtime" >"$runtime/backend.stdout.log" 2>"$runtime/backend.stderr.log" &
backend_pid=$!
for _ in $(seq 1 200); do
  [[ -s "$runtime/backend-ready.json" ]] && break
  pid_is_live "$backend_pid"
  sleep 0.05
done
test -s "$runtime/backend-ready.json"
backend_port=$(jq -er '.port' "$runtime/backend-ready.json")
front_port=$(python3 "$scratch/allocate_port.py")
printf 'backend_port=%s\nfront_port=%s\n' "$backend_port" "$front_port" >"$runtime/ports.txt"

sed \
  -e "s|@SOCKET@|$runtime/sozu.sock|" \
  -e "s|@PIDFILE@|$runtime/sozu.pid|" \
  -e "s|@FRONT_PORT@|$front_port|" \
  -e "s|@BACKEND_PORT@|$backend_port|" \
  "$scratch/config.toml.in" >"$config"

"$runtime_binary" --config "$config" start >"$runtime/sozu.log" 2>&1 &
old_main_pid=$!
printf 'old_main_pid=%s\n' "$old_main_pid" >"$runtime/pids.txt"

for _ in $(seq 1 200); do
  if [[ -S "$runtime/sozu.sock" ]] \
    && "$runtime_binary" --config "$config" --timeout 1000 --json status >"$runtime/status-before.json" 2>"$runtime/status-before.stderr" \
    && jq -e '.WORKERS.vec | any(.run_state == 0)' "$runtime/status-before.json" >/dev/null; then
    break
  fi
  pid_is_live "$old_main_pid"
  sleep 0.05
done
test -s "$runtime/status-before.json"
old_worker_id=$(jq -er '.WORKERS.vec[] | select(.run_state == 0) | .id' "$runtime/status-before.json")
old_worker_pid=$(jq -er '.WORKERS.vec[] | select(.run_state == 0) | .pid' "$runtime/status-before.json")
printf 'old_worker_id=%s\nold_worker_pid=%s\n' "$old_worker_id" "$old_worker_pid" >>"$runtime/pids.txt"
"$runtime_binary" --version >"$runtime/version-before.txt"
"$runtime_binary" --config "$config" --timeout 20000 state save --file "$runtime/state-before.json"
python3 "$scratch/one_shot_client.py" "$front_port" readiness >"$runtime/readiness.txt"

python3 "$scratch/client.py" "$front_port" "$runtime" >"$runtime/client.stdout.log" 2>"$runtime/client.stderr.log" &
client_pid=$!
for _ in $(seq 1 200); do
  [[ -s "$runtime/client-session-open.json" ]] && break
  pid_is_live "$client_pid"
  sleep 0.05
done
test -s "$runtime/client-session-open.json"
grep -q 'line=OPEN actual-version-upgrade' "$runtime/backend-accepted.log"

install -m 0755 "$runtime/bin/sozu.candidate" "$runtime/bin/sozu.new"
mv -f "$runtime/bin/sozu.new" "$runtime_binary"
"$runtime_binary" --config "$config" --timeout 60000 upgrade >"$runtime/upgrade.log" 2>&1 &
upgrade_pid=$!

for _ in $(seq 1 200); do
  new_main_pid=$(tr -d '[:space:]' <"$runtime/sozu.pid" 2>/dev/null || true)
  if [[ -n "$new_main_pid" ]] && [[ "$new_main_pid" != "$old_main_pid" ]] && pid_is_live "$new_main_pid" && ! pid_is_live "$old_main_pid"; then
    break
  fi
  sleep 0.05
done
test -n "$new_main_pid"
test "$new_main_pid" != "$old_main_pid"
pid_is_live "$new_main_pid"
assert_pid_exited 'old main' "$old_main_pid"
printf 'new_main_pid=%s\n' "$new_main_pid" >>"$runtime/pids.txt"
"$runtime_binary" --version >"$runtime/version-after-main.txt"
"$runtime_binary" --config "$config" --timeout 20000 --json status >"$runtime/status-after-main.json"
"$runtime_binary" --config "$config" --timeout 20000 state save --file "$runtime/state-after-main.json"
observed_old_worker_stopping=false
for _ in $(seq 1 400); do
  if "$runtime_binary" --config "$config" --timeout 1000 --json status >"$runtime/status-during-worker-drain.json" 2>"$runtime/status-during-worker-drain.stderr" \
    && jq -e --argjson old_pid "$old_worker_pid" '
      (.WORKERS.vec | any(.pid == $old_pid and .run_state == 1))
      and (.WORKERS.vec | any(.pid != $old_pid and .run_state == 0))
    ' "$runtime/status-during-worker-drain.json" >/dev/null; then
    observed_old_worker_stopping=true
    break
  fi
  if ! pid_is_live "$upgrade_pid"; then
    break
  fi
  sleep 0.05
done
"$runtime_binary" --config "$config" --timeout 20000 --json status >"$runtime/status-before-client-release.json"
new_worker_pid=$(jq -er --argjson old_pid "$old_worker_pid" '.WORKERS.vec[] | select(.pid != $old_pid and .run_state == 0) | .pid' "$runtime/status-before-client-release.json")
printf 'new_worker_pid=%s\n' "$new_worker_pid" >>"$runtime/pids.txt"
python3 "$scratch/one_shot_client.py" "$front_port" candidate-worker >"$runtime/new-session-after-worker-activation.txt"

touch "$runtime/release-client"
set +e
wait "$client_pid"
client_exit=$?
set -e
client_pid=
test -s "$runtime/client-result.json"

set +e
wait "$upgrade_pid"
upgrade_exit=$?
set -e
upgrade_pid=
for _ in $(seq 1 200); do
  if ! pid_is_live "$old_worker_pid"; then
    break
  fi
  sleep 0.05
done
assert_pid_exited 'old worker' "$old_worker_pid"
"$runtime_binary" --config "$config" --timeout 20000 --json status >"$runtime/status-after-worker-drain.json"
"$runtime_binary" --config "$config" --timeout 20000 state save --file "$runtime/state-after-worker-drain.json"
jq -e --argjson new_pid "$new_worker_pid" '
  (.WORKERS.vec | map(select(.run_state == 0)) | length) == 1
  and (.WORKERS.vec | any(.pid == $new_pid and .run_state == 0))
' "$runtime/status-after-worker-drain.json" >/dev/null

"$runtime_binary" --config "$config" --timeout 20000 shutdown >"$runtime/shutdown.log" 2>&1
for _ in $(seq 1 200); do
  if ! pid_is_live "$new_main_pid" && ! pid_is_live "$new_worker_pid"; then
    break
  fi
  sleep 0.05
done
assert_pid_exited 'new main after graceful shutdown' "$new_main_pid"
assert_pid_exited 'new worker after graceful shutdown' "$new_worker_pid"
new_main_pid=
new_worker_pid=

kill "$backend_pid"
wait "$backend_pid" || true
backend_pid=

tcp_preserved=false
if [[ "$client_exit" -eq 0 ]]; then
  tcp_preserved=true
  tcp_outcome=preserved
  jq -e '
    .exit_code == 0
    and .second_ack == "ACK AFTER main-and-worker-upgrade"
    and .close_ack == "ACK CLOSE"
  ' "$runtime/client-result.json" >/dev/null
else
  tcp_outcome=closed_during_upgrade
  jq -e '
    .exit_code == 1
    and (.error | type == "string" and length > 0)
  ' "$runtime/client-result.json" >/dev/null
fi

{
  base_hash=$(sha256sum "$base")
  printf 'BASE_BINARY_SHA256=%s\n' "${base_hash%% *}"
  candidate_hash=$(sha256sum "$candidate")
  printf 'CANDIDATE_BINARY_SHA256=%s\n' "${candidate_hash%% *}"
  printf 'RUNTIME=%s\n' "$runtime"
  printf 'TCP_LONG_LIVED_OUTCOME=%s\n' "$tcp_outcome"
  printf 'TCP_LONG_LIVED_PRESERVED=%s\n' "$tcp_preserved"
  printf 'OLD_WORKER_STOPPING_OBSERVED=%s\n' "$observed_old_worker_stopping"
  printf 'UPGRADE_EXIT=%s\n' "$upgrade_exit"
  printf 'CLIENT_EXIT=%s\n' "$client_exit"
  printf 'OLD_MAIN_EXITED=true\n'
  printf 'OLD_WORKER_DRAINED_AND_EXITED=true\n'
  printf 'NEW_WORKER_ACCEPTED_NEW_SESSION=true\n'
  printf 'GRACEFUL_SHUTDOWN_EXITED=true\n'
} >"$runtime/validation-summary.txt"
cat "$runtime/validation-summary.txt"

test "$upgrade_exit" -eq 0
