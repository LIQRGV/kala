#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
# The Go tests need the Go that e2e/go/go.mod asks for (its `go` line). A
# missing or older Go skips them with a visible line, or fails the run when
# KALA_E2E_REQUIRE_GO=1 (every Bolt review sets it).
go_needed=$(awk '$1 == "go" { print $2; exit }' e2e/go/go.mod)
IFS=. read -r go_major go_minor _ <<< "$go_needed"
go_skipped=
if ! command -v go >/dev/null; then
    go_skipped='go not found'
elif [[ "$(go version 2>/dev/null)" =~ go([0-9]+)\.([0-9]+)([.][0-9]+)? ]]; then
    go_found="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}${BASH_REMATCH[3]}"
    if (( BASH_REMATCH[1] < go_major || (BASH_REMATCH[1] == go_major && BASH_REMATCH[2] < go_minor) )); then
        go_skipped="go $go_found found, go/go.mod needs $go_needed or newer"
    fi
else
    go_skipped='go version did not report a release version'
fi
run_go=1
if [[ -n "$go_skipped" ]]; then
    echo "SKIPPED: Go driver tests ($go_skipped)" >&2
    if [[ "${KALA_E2E_REQUIRE_GO:-}" == 1 ]]; then
        exit 1
    fi
    run_go=
fi
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
    # A PKCS#1 key as well as the PKCS#8 one. `pkey -traditional` is OpenSSL 3;
    # OpenSSL 1.1.1's `rsa` writes the traditional form by default.
    { openssl pkey -in self-pkcs8.key -traditional -out self.key ||
        openssl rsa -in self-pkcs8.key -out self.key; } &&
    grep -q 'BEGIN RSA PRIVATE KEY' self.key &&
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
# Set KALA_E2E_REQUIRE_TLS=1 (every Bolt review does) to make a skipped TLS
# suite a failure instead of a quiet pass.
tls_skipped=
if ! command -v openssl >/dev/null; then
    tls_skipped="openssl is not installed"
elif ! generate_test_certificates > "$task_tmp/openssl.log" 2>&1; then
    tls_skipped="openssl ($(openssl version 2>/dev/null || echo unknown version)) could not generate the test certificates"
    echo "--- openssl output ---" >&2
    cat "$task_tmp/openssl.log" >&2
    echo "----------------------" >&2
else
    export KALA_TLS_FIXTURES="$task_tmp/tls"
fi
if [[ -n "$tls_skipped" ]]; then
    if [[ "${KALA_E2E_REQUIRE_TLS:-}" == 1 ]]; then
        echo "KALA_E2E_REQUIRE_TLS=1 but the TLS driver tests cannot run: $tls_skipped." >&2
        exit 1
    fi
    echo "Skipping TLS driver tests: $tls_skipped. (KALA_E2E_REQUIRE_TLS=1 makes this an error.)" >&2
fi
export KALA_SERVER_STDOUT="$task_tmp/server.stdout"
export KALA_SERVER_STDERR="$task_tmp/server.stderr"
# bash before 4.4 treats the plain expansion of an empty array as an unset
# variable under `set -u`: expand arrays as ${array[@]+"${array[@]}"} and keep
# a flag instead of reading an array's length (e2e/test_run_sh.sh checks this).
tls_listener=()
tls_listening=
unset KALA_GO_TLS_URI
if [[ -n "$run_go" && -n "${KALA_TLS_FIXTURES:-}" ]]; then
    tls_listening=1
    tls_listener=(--listen bolt+s://127.0.0.1:0 --tls.cert "$KALA_TLS_FIXTURES/signed.pem" --tls.key "$KALA_TLS_FIXTURES/signed.key")
fi
mkfifo "$task_tmp/ready"
"$KALA_BINARY" serve "$task_tmp" --listen 127.0.0.1:0 ${tls_listener[@]+"${tls_listener[@]}"} --default-db varrow > "$task_tmp/ready" 2> "$task_tmp/server.stderr" &
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
if [[ -n "$tls_listening" ]]; then
    if ! IFS= read -r -t 20 tls_ready <&"$ready_fd" || [[ "$tls_ready" != "kala serve: listening on bolt+s://127.0.0.1:"* ]]; then
        echo "missing TLS listener announcement" >&2
        exit 1
    fi
    printf '%s\n' "$tls_ready" >> "$KALA_SERVER_STDOUT"
    export KALA_GO_TLS_URI="${tls_ready#kala serve: listening on }"
fi
cat <&"$ready_fd" >> "$KALA_SERVER_STDOUT" &
stdout_pid=$!
export KALA_BOLT_URI="bolt://${ready#kala serve: listening on }"
e2e/.venv/bin/python e2e/test_python_driver.py ${@+"$@"}
if [[ -n "$run_go" ]]; then
    (cd e2e/go && go test -mod=readonly -count=1 -v ./...)
fi
