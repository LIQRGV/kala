# Official Python-driver checks

Run `./e2e/run.sh` from the repository. It creates `e2e/.venv`, installs the exact
Neo4j 5.x driver pinned in `requirements.txt`, builds Kala, loads a temporary
Varrow world, starts `kala serve` on port zero, runs stdlib `unittest`, and stops
the server and removes the world even when a test fails. Requires Python 3 with
venv, pip, Bash, and Cargo. `cargo test` never runs Python or accesses pip.

The tests cover connectivity, no-auth/basic tokens, native Node and Relationship
hydration, temporal properties, scalar/history/interval values, parameters and
error codes, default/unknown databases, session reuse after failure, explicit
transaction refusal, fetch-size pagination and discard, summaries, and an
external CLI commit becoming visible without a server restart.

The connectivity test uses a temporary byte-transparent proxy to print the exact
20-byte handshake sent by the pinned driver and the four-byte selected version.
It records no auth payloads. Protocol framing, malformed bytes, resource limits,
loopback rejection, output hygiene and SIGINT are covered by the Rust tests.

Cypher, routing, system procedures, transactions, and writes over Bolt are not
supported in this slice. Use `bolt://` and prepend `// kala:datalog` followed by a
newline to every Datalog query.

## Compatibility evidence (neo4j 5.28.8)

The first real-driver run rejected `server: "Kala/0.1.0"` with
`UnsupportedServerProduct`. The driver's `_sync/io/_common.py` checks for a
`Neo4j/` prefix. Kala therefore advertises `Neo4j/0.1.0`: the required prefix and
Kala's crate version. No server-specific procedures are implemented.

The observed handshake was:

```text
60 60 b0 17  00 00 01 ff  00 08 08 05  00 02 04 04  00 00 00 03
```

It offers manifest negotiation, then Bolt 5.8 through 5.0, 4.4 through 4.2, and
3.0. Kala selects the highest supported plain proposal, `00 00 04 05` (5.4),
which the driver accepts without manifest negotiation. `fields`, `t_first`,
`t_last`, `type: "r"`, and `db` support successful result consumption; no bookmark
is needed for these read-only auto-commit queries. All 14 driver tests pass.
