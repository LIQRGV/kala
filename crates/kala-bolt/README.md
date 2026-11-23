# kala-bolt

A read-only, loopback Bolt 5.0–5.4 server over Kala's existing Datalog front-end
and shared query executor. `kala serve DIR` exposes each regular
`DIR/<name>.kala` file as a database (`[a-z0-9_-]+` names only). Symlinks and
invalid names are skipped with a startup diagnostic. Files are discovered at
startup and opened read-only on first RUN. Length changes trigger a reload of
committed transactions; each result pins one snapshot. A torn tail is never
served or repaired by this server. Replacing a file without changing its length
is outside this append-only reload contract. The served directory and its files
are trusted local input: concurrent replacement of directory entries is outside
the network security boundary.

## Using it

```sh
kala serve worlds --listen 127.0.0.1:7687 --auth none \
  --default-db varrow --max-connections 64 --idle-timeout 600
```

Port 0 selects a free port, reported by the single stdout startup line
`kala serve: listening on <address>`. Other diagnostics use structured stderr
lines; neither queries, parameters nor credentials are logged. `localhost`
resolves to `127.0.0.1`; `127.0.0.0/8` and `::1` are accepted. Other listen
addresses fail before binding. SIGINT closes the listener and client sockets,
cancels queries and joins connection threads.

A single discovered database is the implicit default. With multiple databases,
set `--default-db` or supply the driver's session database. The library exports
`Server`, `ServerConfig`, PackStream values/codecs and `auth::Authenticator`.
`Server::bind_with_auth` accepts a custom authenticator but still restricts
binding to loopback. `NoAuth` accepts none/basic tokens. Credentials format as
`<redacted>` and are dropped after the authentication callback.

Query text must start with exactly `// kala:datalog` followed by a newline;
trailing ASCII spaces on that line are allowed. Unprefixed Cypher and
`// kala:gremlin` receive SyntaxError. Cypher is the next slice.

## Protocol

The plain handshake selects the highest offered version in 5.0–5.4, including
minor-version ranges. Unsupported manifest proposals are skipped in favor of
a supported plain proposal; a manifest-only or 4.x-only proposal is rejected.
HELLO authenticates in 5.0; 5.1+ requires LOGON. LOGOFF returns to authentication.
RUN, PULL, DISCARD, RESET, GOODBYE and 5.4 TELEMETRY are supported. BEGIN,
COMMIT, ROLLBACK and ROUTE return `Neo.ClientError.Request.Invalid` with
"not supported yet". Malformed PackStream closes after FAILURE when possible;
pre-authentication failures close. Other failures enter FAILED, which answers
IGNORED until RESET. RESET never grants authentication.

RUN materializes a bounded result. PULL/DISCARD accept positive `n` or `-1`;
partial consumption returns `has_more: true`. Only one result is open, `qid=0`
(`-1` means that result). RUN reports `fields`, `qid` and `t_first`. Final
SUCCESS reports `t_last`, `type: "r"` and `db`. No bookmark is issued for these
read-only queries. RESET received during a running query is handled after that
query finishes; shutdown cancellation is immediate at query work checks.

### Official driver compatibility

The pinned official Python driver `neo4j==5.28.8` proposed
`000001ff 00080805 00020404 00000003`: manifest v1, Bolt 5.8–5.0,
4.4–4.2 and 3.0. Kala selects the plain 5.4 proposal (`00000405`);
manifest support is unnecessary for this driver. The driver rejected the initial
`Kala/0.1.0` agent with `UnsupportedServerProduct`. HELLO therefore advertises
`server: "Neo4j/5.0.0"`: the prefix drivers require, with the Bolt-5
compatibility level as its version (tools read that number for feature checks,
and a `0.x` would look like an ancient server). Kala's own version is in
`kala_version`, which drivers ignore. This is a compatibility identifier, like a
browser's `Mozilla/5.0`, not a claim to be Neo4j or to implement its features.

HELLO `hints` carry `connection.recv_timeout_seconds` (the server's idle
timeout), so pooled drivers retire connections before the server closes them.
The real-driver tests accepted the metadata and graph layouts above, with no
bookmark required.

## Value mapping

| Kala value | Bolt / Python driver |
| --- | --- |
| i64, f64, bool, string | Integer, Float, Boolean, String |
| Entity reference | Bolt 5 Node: raw integer ID, labels, properties, readable element_id |
| Edge reference | Bolt 5 Relationship: negative tagged ID, endpoints, type, properties and all three element IDs |
| Op | `"assert"` or `"retract"` |
| Precision | `"exact"`, `"day"`, `"month"`, `"year"`, `"decade"`, `"century"` |
| Interval set | List of `[start, end]` lists, null for an open end |

Nodes and relationships are enriched at the result's default tick and canon.
Node labels include its immutable type and active string `label` values.
Properties include every active non-ref attribute and the handle (if present) under `handle`, unless the schema has its own `handle` attribute, whose value then wins.
Cardinality-many attributes are sorted lists. A cardinality-one overlap is also
returned as a list so neither asserted value is lost. Reference attributes are
relationships, not properties. Graph data is returned verbatim; diagnostics are
escaped with `kala_core::must_escape` rules.

RUN parameters use their declared Datalog kinds: integers for ticks/i64,
floats for f64, booleans, strings for strings, handles or readable IDs for
entities, and a nonnegative integer or `tx#N`/`N` string for canon. No numeric
coercion; null, NaN, byte arrays and other mismatched types are TypeError. Undeclared params are
ignored. Fields omit the variable's `?`; aggregate names retain their text.

## Errors and limits

| Condition | Bolt code |
| --- | --- |
| Datalog parse/compile or unsupported language | `Neo.ClientError.Statement.SyntaxError` (located when available) |
| Missing parameter | `Neo.ClientError.Statement.ParameterMissing` |
| Parameter type mismatch | `Neo.ClientError.Statement.TypeError` |
| Query/enrichment limits or cancellation | `Neo.ClientError.Statement.ExecutionFailed` |
| Unknown or ambiguous database | `Neo.ClientError.Database.DatabaseNotFound` |
| Invalid message/state or unsupported operation | `Neo.ClientError.Request.Invalid` |
| Store error | `Neo.DatabaseError.General.UnknownError`: "the world could not be read" |

Defaults: 64 connection threads, 10-second total handshake deadline,
10-minute complete-message/idle deadline, 10-second response-batch write
deadline, 65,535-byte chunks, 1 MiB assembled messages, 32 PackStream nesting
levels. Limits live in `ServerConfig`; depth cannot exceed 32. Counts are checked
against actual remaining bytes before allocation. NOOPs and trickled partial
chunks cannot reset deadlines. Excess connections are accepted and closed.

Every query uses `Limits::default()` (100,000 rows, 10 million steps, 256 MiB).
Enrichment has a separate cumulative 10-million-step / 256-MiB budget, charging
attribute scans, value sorting, property allocations and encoded records. A
visitor reads scalar properties one at a time rather than materializing an
unbounded property list; one temporary stored value is additionally bounded by
the core's 16-MiB log-entry limit. Oversized records fail RUN before streaming.

## Validation and specification

Pure Rust tests run with `cargo test -p kala-bolt`; the real official Python
driver is exercised separately with `./e2e/run.sh` (see `e2e/README.md`).
No Python, async runtime or new third-party runtime dependency is needed by
Rust tests or the server.

Implementation references: [handshake](https://neo4j.com/docs/bolt/current/bolt/handshake/),
[messages](https://neo4j.com/docs/bolt/current/bolt/message/),
[server states](https://neo4j.com/docs/bolt/current/bolt/server-state/),
[PackStream](https://neo4j.com/docs/bolt/current/packstream/).
