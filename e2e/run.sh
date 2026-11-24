#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
if [[ ! -x e2e/.venv/bin/python ]]; then
    python3 -m venv e2e/.venv
fi
e2e/.venv/bin/pip install -r e2e/requirements.txt
cargo build -p kala-cli

task_tmp=$(mktemp -d)
server_pid=
stdout_pid=
cleanup() {
    status=$?
    trap - EXIT
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill -INT "$server_pid" 2>/dev/null || true
        wait "$server_pid" || true
    fi
    if [[ -n "$stdout_pid" ]]; then
        wait "$stdout_pid" || true
    fi
    if [[ $status -ne 0 ]]; then
        cat "$task_tmp/server.stderr" >&2 2>/dev/null || true
    fi
    rm -rf -- "$task_tmp"
    exit "$status"
}
trap cleanup EXIT
export KALA_BINARY="$repo/target/debug/kala"
export KALA_WORLD="$task_tmp/varrow.kala"
"$KALA_BINARY" load crates/kala-core/tests/fixtures/varrow.yaml "$KALA_WORLD" > "$task_tmp/load.stdout"
"$KALA_BINARY" load crates/kala-core/tests/fixtures/varrow.yaml "$task_tmp/hidden.kala" >> "$task_tmp/load.stdout"
"$KALA_BINARY" user add "$task_tmp" test-user@localhost --trust > "$task_tmp/user.stdout"
"$KALA_BINARY" user grant "$task_tmp" test-user@localhost read varrow >> "$task_tmp/user.stdout"
export KALA_SERVER_STDOUT="$task_tmp/server.stdout"
export KALA_SERVER_STDERR="$task_tmp/server.stderr"
mkfifo "$task_tmp/ready"
"$KALA_BINARY" serve "$task_tmp" --listen 127.0.0.1:0 --default-db varrow > "$task_tmp/ready" 2> "$task_tmp/server.stderr" &
server_pid=$!
exec {ready_fd}<"$task_tmp/ready"
if ! IFS= read -r -t 20 ready <&"$ready_fd"; then
    echo "server did not announce its listen address" >&2
    exit 1
fi
if [[ "$ready" != "kala serve: listening on 127.0.0.1:"* ]]; then
    echo "unexpected server startup line: $ready" >&2
    exit 1
fi
printf '%s\n' "$ready"
printf '%s\n' "$ready" > "$KALA_SERVER_STDOUT"
cat <&"$ready_fd" >> "$KALA_SERVER_STDOUT" &
stdout_pid=$!
export KALA_BOLT_URI="bolt://${ready#kala serve: listening on }"
e2e/.venv/bin/python e2e/test_python_driver.py
