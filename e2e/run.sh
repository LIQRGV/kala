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
        cat "$task_tmp/writes/server.stderr" >&2 2>/dev/null || true
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
# Keep writes and their accounts on a separate server so every existing read
# test retains its database list and fixture. The Python suite owns its lifetime.
export KALA_WRITE_DIR="$task_tmp/writes"
mkdir "$KALA_WRITE_DIR"
cp "$KALA_WORLD" "$KALA_WRITE_DIR/varrow.kala"
# A second database on the write server: drivers send every bookmark they hold
# to every database, so cross-database requests need one.
cp "$KALA_WORLD" "$KALA_WRITE_DIR/annex.kala"
for user in writer write-only reader; do
    "$KALA_BINARY" user add "$KALA_WRITE_DIR" "$user@localhost" --trust >> "$task_tmp/user.stdout"
done
"$KALA_BINARY" user grant "$KALA_WRITE_DIR" writer@localhost read varrow >> "$task_tmp/user.stdout"
"$KALA_BINARY" user grant "$KALA_WRITE_DIR" writer@localhost write varrow >> "$task_tmp/user.stdout"
"$KALA_BINARY" user grant "$KALA_WRITE_DIR" writer@localhost read annex >> "$task_tmp/user.stdout"
"$KALA_BINARY" user grant "$KALA_WRITE_DIR" writer@localhost write annex >> "$task_tmp/user.stdout"
"$KALA_BINARY" user grant "$KALA_WRITE_DIR" write-only@localhost write varrow >> "$task_tmp/user.stdout"
"$KALA_BINARY" user grant "$KALA_WRITE_DIR" reader@localhost read varrow >> "$task_tmp/user.stdout"
# The TLS tests use a self-signed server and a separate CA-signed server.
# Keep generated keys private even when invoked with a permissive umask.
generate_test_certificates() (
    umask 077
    cd "$task_tmp/tls" || return 1
    openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj /CN=localhost \
        -addext 'subjectAltName=IP:127.0.0.1,DNS:localhost' \
        -keyout self-pkcs8.key -out self.pem &&
    openssl pkey -in self-pkcs8.key -traditional -out self.key &&
    openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj '/CN=Kala test CA' \
        -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign' \
        -keyout ca.key -out ca.pem &&
    openssl ecparam -name prime256v1 -genkey -noout -out signed.key &&
    openssl req -new -key signed.key -subj /CN=localhost -out signed.csr &&
    openssl x509 -req -in signed.csr -CA ca.pem -CAkey ca.key -set_serial 1 -days 30 \
        -extfile <(printf '%s\n' 'basicConstraints=critical,CA:FALSE' \
            'extendedKeyUsage=serverAuth' 'subjectAltName=IP:127.0.0.1,DNS:localhost') \
        -out signed.pem
)
mkdir "$task_tmp/tls"
unset KALA_TLS_FIXTURES
if command -v openssl >/dev/null && generate_test_certificates >/dev/null 2>&1; then
    export KALA_TLS_FIXTURES="$task_tmp/tls"
else
    echo "Skipping TLS driver tests: OpenSSL could not generate the test certificates." >&2
fi
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
e2e/.venv/bin/python e2e/test_python_driver.py "$@"
