# Official Neo4j driver checks

Run `./e2e/run.sh` from the repository. It creates a Python virtual environment,
installs the pinned Python driver, builds Kala, and starts a temporary Varrow
world on a loopback port. It also runs the official Neo4j Go v5 driver pinned
in `go/go.mod` when Go 1.18 or newer is installed. The first Go run downloads
the module and checks it against `go/go.sum`. The harness stops the server and
removes its worlds on exit. It requires Python 3 with venv and pip, Bash, and
Cargo. The Rust workspace tests run independently of both drivers.

For a Bolt review, require both Go and TLS:

```sh
KALA_E2E_REQUIRE_GO=1 KALA_E2E_REQUIRE_TLS=1 ./e2e/run.sh
```

Without Go the harness prints `SKIPPED: Go driver tests (go not found)`.
Setting `KALA_E2E_REQUIRE_GO=1` makes that a failure.

The TLS tests (`test_tls_driver.py`: `bolt+ssc://`, `bolt+s://` with a test CA,
`neo4j+ssc://` routing, certificate rotation with SIGUSR1) need certificates
that `run.sh` generates with `openssl` (1.1.1 or 3.x). Without `openssl`, or
when generation fails, the script prints why (with openssl's own output) and
**skips** those tests; the run still passes. Set `KALA_E2E_REQUIRE_TLS=1` to
make that skip a failure. The Go `bolt+s://` test uses a second listener on the
same Varrow server and verifies its certificate against the generated CA.

The Python tests cover connectivity, no-auth/basic tokens, native Node and Relationship
hydration, temporal properties, scalar/history/interval values, parameters and
error codes, default/unknown databases, session reuse after failure, explicit
transaction permissions, fetch-size pagination and discard, summaries, and an
external CLI commit becoming visible without a server restart.

The connectivity test uses a temporary byte-transparent proxy to print the exact
20-byte handshake sent by the pinned driver and the four-byte selected version.
It records no auth payloads. Protocol framing, malformed bytes, resource limits,
loopback rejection, output hygiene and SIGINT are covered by the Rust tests.

The Go tests run against the same server after Python finishes. They check trust
and password accounts, database grants, and Edwin's allies in Cypher and
Datalog against `kala query --json`, including a historical canon. They also
check native Node, Relationship and Path values, multi-statement commits,
rollback, managed transaction retry after a conflicting write, and discovery of
a world created while the server is running.

## Compatibility evidence (neo4j 5.28.8)

The first real-driver run rejected `server: "Kala/0.1.0"` with
`UnsupportedServerProduct`. The driver's `_sync/io/_common.py` checks for a
`Neo4j/` prefix. Kala therefore advertises `Neo4j/0.1.0`: the required prefix and
Kala's crate version.

The observed handshake was:

```text
60 60 b0 17  00 00 01 ff  00 08 08 05  00 02 04 04  00 00 00 03
```

It offers manifest negotiation, then Bolt 5.8 through 5.0, 4.4 through 4.2, and
3.0. Kala selects the highest supported plain proposal, `00 00 04 05` (5.4),
which the driver accepts without manifest negotiation. `fields`, `t_first`,
`t_last`, `type: "r"`, and `db` support successful result consumption; no bookmark
is needed for these read-only auto-commit queries.
