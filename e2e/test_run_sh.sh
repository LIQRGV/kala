#!/usr/bin/env bash
# Checks of run.sh itself that need no server: which Go it accepts, and that
# it stays runnable on bash older than 4.4. Run it directly: ./e2e/test_run_sh.sh
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
failed=0
fail() {
    echo "FAILED: $1" >&2
    failed=1
}

# run.sh with a `go` that reports version $1, and with stand-ins for whatever
# run.sh starts once it is past the Go check (they exit 97, so a run that got
# past the check is told apart from one the check stopped, without building
# or installing anything). Prints run.sh's stderr; returns its exit status.
run_with_go() {
    local version=$1
    shift
    mkdir -p "$tmp/bin"
    printf '#!/bin/sh\necho "go version %s linux/amd64"\n' "$version" > "$tmp/bin/go"
    for tool in python3 cargo; do
        printf '#!/bin/sh\nexit 97\n' > "$tmp/bin/$tool"
    done
    chmod +x "$tmp/bin/go" "$tmp/bin/python3" "$tmp/bin/cargo"
    env "$@" PATH="$tmp/bin:/usr/bin:/bin" bash "$repo/e2e/run.sh" 2>&1 >/dev/null
}

needed=$(awk '$1 == "go" { print $2; exit }' "$repo/e2e/go/go.mod")
older="1.$(( ${needed#1.} - 1 ))"
skipped="SKIPPED: Go driver tests (go $older.13 found, go/go.mod needs $needed or newer)"

status=0
output=$(run_with_go "go$older.13" KALA_E2E_REQUIRE_GO=1) || status=$?
if [[ $status -ne 1 || "$output" != *"$skipped"* ]]; then
    fail "a Go older than go.mod's must fail a run that requires Go (status $status): $output"
fi

status=0
output=$(run_with_go "go$older.13" KALA_E2E_REQUIRE_GO=) || status=$?
if [[ $status -ne 97 || "$output" != *"$skipped"* ]]; then
    fail "a Go older than go.mod's must be skipped visibly and the run go on (status $status): $output"
fi

for version in "go$needed" "go$needed.10" "go1.$(( ${needed#1.} + 7 )).2" go2.0; do
    status=0
    output=$(run_with_go "$version" KALA_E2E_REQUIRE_GO=1) || status=$?
    if [[ $status -ne 97 || "$output" == *SKIPPED* ]]; then
        fail "$version must be accepted (status $status): $output"
    fi
done

# bash < 4.4 with `set -u` treats "${array[@]}" of an empty array as an unset
# variable. Every array expansion must use the ${array[@]+"${array[@]}"} form
# (and "$@" likewise), which expands to nothing instead.
unguarded=$(grep -nE '"\$(@|\{[A-Za-z_]+\[@\]\})"' "$repo/e2e/run.sh" | grep -vE '\$\{(@|[A-Za-z_]+\[@\])\+' || true)
if [[ -n "$unguarded" ]]; then
    fail "array expansions that fail on an empty array under set -u before bash 4.4:
$unguarded"
fi
if grep -nE '\$\{#[A-Za-z_]+\[@\]\}' "$repo/e2e/run.sh"; then
    fail "the length of a possibly empty array is read; use a flag variable"
fi

if [[ $failed -ne 0 ]]; then
    exit 1
fi
echo "run.sh checks passed"
