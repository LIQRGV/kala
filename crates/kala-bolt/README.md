# kala-bolt

A read-only Bolt 5.0–5.4 server over Kala's Cypher and Datalog front-ends
and shared query executor. `kala serve DIR` exposes each regular
`DIR/<name>.kala` file as a database (`[a-z0-9_-]+` names only). Symlinks and
invalid names are skipped with a startup diagnostic. Files are discovered at
startup and opened read-only on first RUN. File stamps (length, modification time, file identity and commit tail) trigger a reload of
committed transactions; each result pins one snapshot. A torn tail is never
served or repaired by this server. The served directory and its files
are trusted local input: concurrent replacement of directory entries is outside
the network security boundary.

## Using it

```sh
kala serve worlds --listen 127.0.0.1:7687 --auth accounts \
  --default-db varrow --max-connections 64 --idle-timeout 600
```

Port 0 selects a free port, reported by a stdout startup line for each listener
`kala serve: listening on <address>`. Other diagnostics use structured stderr
lines; neither queries, parameters nor credentials are logged. `localhost`
resolves to `127.0.0.1`; `127.0.0.0/8` and `::1` are accepted. Other listen
addresses require accounts and `--insecure-allow-plaintext`; `--auth none`
always requires loopback. Setting the shutdown flag closes every listener and
client socket, cancels queries and joins connection threads; `kala serve` sets
it on SIGINT or SIGTERM (and SIGHUP), so `systemctl stop` and `docker stop`
shut down cleanly and remove Unix socket files.

A single discovered user database is the implicit default (`system` is excluded). With multiple databases,
set `--default-db` or supply the driver's session database. The library exports
`Server`, `ServerConfig`, PackStream values/codecs and `auth::Authenticator`.
`Server::bind_accounts` implements the default CLI policy.
`Server::bind` retains its loopback-only `NoAuth` library behavior; it never
creates, opens or reads `system.kala` (skipping it with one stderr line) and never
serves the `system` database;
`Server::bind_with_auth` accepts a custom authenticator but still restricts
binding to loopback. Authenticators receive an IP or explicit local peer and return an identity
with a matched host and grants. `NoAuth` accepts all well-formed tokens with a `read:*` identity (a
development mode, not an administrator). Credentials format as
`<redacted>` and are dropped after the authentication callback.

Unprefixed query text is Cypher; an explicit first line `// kala:cypher` also
selects Cypher. `// kala:datalog` followed by a newline selects Datalog; trailing
ASCII spaces on that line are allowed. `// kala:gremlin` reports that Gremlin is
not supported yet. See the [Cypher subset](../kala-cypher/README.md).

## Transports and multiple listeners

A transport carries the same Bolt bytes with absolute read/write deadlines,
peer identity, a reached address, and an independent shutdown handle. TCP and
Unix streams use one session implementation, including authentication, message
size/depth bounds and failure/RESET behavior. TLS and WebSocket are planned as
wrappers of this seam. `Transport` requires no raw descriptor, and
`Server::with_transport_wrapper` can decorate accepted streams; conformance
also runs through a wrapper that splits every read and write into single bytes.
The peer used for account matching and the reached address are read from the
accepted connection before it is wrapped, so a wrapper cannot change who
connected. Per-socket setup happens after `accept`: a connection that cannot be
set up is dropped with `event=bolt_connection_dropped` and the server keeps
accepting.

`ServerConfig::listen` is a list of `ListenAddr` values; `Server::local_addrs()`
returns every actual bound endpoint in order. `--listen` accepts bare
`IP:PORT`, `[IPv6]:PORT`, `localhost:PORT`, the same forms after `bolt://`, and
(on Unix) `unix:///absolute/path`. DNS is never used. The default remains
`127.0.0.1:7687`. Up to 16 distinct listeners share one `--max-connections` cap.
All listeners bind before any startup line is printed; a failed bind closes
previous listeners and removes newly created Unix sockets. Unsupported schemes
are errors; `neo4j://` is a client routing URI, not a listen address.

```sh
kala serve worlds --listen bolt://127.0.0.1:7687 \
  --listen unix:///run/kala/bolt.sock --unix.mode 0660
```

Unix paths must be absolute and fit the platform's `sun_path` limit (107 bytes
on Linux, excluding the trailing NUL). Socket permissions default to `0600`;
`--unix.mode` accepts octal permission bits through `0777`. Other users gain
write permission only if it is explicitly requested, for example `0666`.
Creation binds in a private staging directory (created `0700`, whatever the
umask), sets the requested mode,
then publishes with a no-clobber hard link on the same filesystem. There is no
public permission window and no process-global umask change. The filesystem
must support hard links to socket inodes.

The parent directory must belong to the server user or root and must not be
group/other writable unless sticky. Prefer a private directory restricted to
intended clients. Linux uses pinned directory descriptors through
`/proc/self/fd` (procfs must be available), including for staging, and
no-follow descriptor-relative metadata, publication and cleanup. Other Unix
platforms additionally require trusted owners/permissions throughout the
canonical directory ancestry; staging may use a shorter writable ancestor on
the same filesystem to fit `sun_path`. Processes with the same UID and root
share filesystem authority and are outside this boundary.

**Lock file.** Next to the socket the server keeps a lock file named
`.<socket name>.lock` (for `/run/kala/bolt.sock`: `/run/kala/.bolt.sock.lock`),
created with mode `0600`, empty, and `flock`-ed exclusively for as long as the
listener exists, as PostgreSQL does. It decides Kala against Kala: a second
`kala serve` on the same path waits up to about one second (so a restart that
overlaps a shutdown works) and then fails with `unix:///path is already in use
by another kala serve (lock file … is held)`; the running server's socket is
never touched. The lock file must be a regular file owned by the server's user:
a symlink, a directory or another user's file at that name is refused with an
error naming it, and is left untouched. The parent directory itself is not
locked, so a user who can merely open that directory cannot block a start or a
shutdown. Clean shutdown removes the socket first, then the lock file (while
the lock is still held; a start that was waiting notices the file is gone and
takes a fresh one), then lets go. After a crash both files stay; the empty lock
file is harmless and the next start reuses it.

**An existing socket file.** With the lock held, a socket already at the path
cannot belong to a live Kala server. It is still probed with a nonblocking
connect, to protect a listener that is not Kala:

- the connection is accepted or queued: `already in use`, nothing is changed;
- the connection is refused: the socket is a leftover and is replaced;
- the server may not connect (a leftover created with `--unix.mode 0000` or
  `0060` denies its own user): replaced if this user owns it; if another user
  owns it, the start fails with an error naming the path and the remedy
  (remove the file or choose another path).

On Linux a live listener with a full backlog answers "try again", so it counts
as in use. BSD-family kernels (macOS, FreeBSD) answer "refused" in that case as
well, so there a non-Kala listener with a full backlog on the same path can be
taken for a leftover and its socket file replaced. Kala servers are not
affected, because the lock file decides between them.

Regular files and symlinks at the socket path are refused without deletion.
Clean shutdown removes only the socket inode this listener created; if the
parent directory's permissions were loosened meanwhile it logs
`event=bolt_socket_cleanup_failed` and leaves the socket, which a later start
reclaims. `kala serve` runs this cleanup on SIGINT, SIGTERM and SIGHUP; only an
uncatchable kill (SIGKILL, power loss) leaves the two files behind.

**Who may connect.** `--unix.mode` is the access control of a Unix listener.
A new `system` database has `root@localhost` with `trust` (no password) and
`admin:*`, and a Unix peer is `localhost`, so until that account gets a
password every user who can open the socket is an administrator. With the
default `0600` that is the server's own user; `0660` adds the socket's group;
`0666` means every local user, which is what the default TCP loopback listener
already allows. For a group of users, give the socket a directory of its own,
owned by the server's user with the clients' group and mode `0750`, and use
`--unix.mode 0660`. A parent directory that is group- or other-writable
without the sticky bit is refused (a `0770` directory, for example): anyone
who can write to it could swap the socket. Clients connect by path, so no
ancestor directory of the socket should be writable by other users either;
Linux does not check the ancestors (it pins the parent directory), the other
Unix platforms refuse an untrusted ancestor.

A Unix peer matches **only `localhost`**, so `root@localhost` trust and password
accounts work; IP patterns and `%` never match it (an account that exists only
as `alice@%` cannot log in over the socket; add `alice@localhost`). Socket and directory
permissions are the access boundary, not peer-credential authentication. Some
Unix systems do not enforce socket write permissions, so restrict the directory
as well. IP-only custom authenticators retain their existing API and refuse
local peers unless they implement `authenticate_peer`. Unix listeners count as
local for `--auth none` and need no plaintext opt-in; every non-loopback TCP
listener still requires accounts and `--insecure-allow-plaintext`.

With no `--advertised-address`, Unix `SHOW DATABASES` reports `unix:///path`,
and `ROUTE` fails with `routing over a Unix socket needs --advertised-address`.
An explicit advertised address is used for both. TCP continues to advertise the
connection's local address by default, with a warning per non-loopback listener.

Official Neo4j drivers cannot dial Unix sockets. The Unix conformance tests use
the Rust socket client; the Python driver E2E suite continues to use TCP. On
non-Unix systems, `unix://` reports `not supported on this platform`.

## Accounts and privileges

On first startup, `kala serve DIR` creates `DIR/system.kala`, a private account
store with the `_KALA_SYSTEM` schema marker and schema version 1. The initial
account is `root@localhost`, plugin `trust`, grant `admin:*`. Startup warns for
every trust account. `scheme: none` (or the official driver's empty auth map) logs in as root; `basic` uses the supplied
principal and password; other schemes fail. Authentication failures always
return `Neo.ClientError.Security.Unauthorized` / `authentication failed` and
close the connection. After setting root's password, `auth=None` stops working.

Accounts are `USER@HOST`; users are 1–64 ASCII letters, digits, `_`, `.` or `-`.
Hosts are matched without DNS, in this order:

1. Exact IP (IPv4 or IPv6).
2. `localhost` (`127.0.0.1` or `::1`).
3. CIDR, longest prefix first, e.g. `10.0.0.0/8` or `fd00::/8`.
4. IPv4 patterns with whole trailing wildcard octets, most literal octets first,
   e.g. `192.168.1.%` or `10.%`.
5. `%` (any IP peer).

IPv4-mapped IPv6 peers match IPv4 patterns. The most specific account is selected
before checking credentials; a failed password never falls back to a broader
account. Ties use the canonical host's lexical order.

`kala_password` stores a salted Argon2id PHC hash (v19, 19 MiB, two iterations,
one lane). At most four verifications run simultaneously; unknown users perform
a dummy verification too. `trust` ignores credentials and is allowed only for
`localhost`, `127.0.0.1` or `::1`, with an independent loopback check at login.
**Behind a local reverse proxy every client can appear as 127.0.0.1.** Set
passwords on local accounts before exposing such a proxy.

Grants are `read`, `write` or `admin`, on a database name or `*`. Every RUN checks
the authenticated identity's grants before loading the database. `admin` implies
read and write; `write` is stored for future Bolt writes and does not imply read.
Denied queries return `Neo.ClientError.Security.Forbidden`. `system` requires
`admin:system` or `admin:*`, even if the account has a read grant on it.

The reserved system database is queryable with Cypher or Datalog by an admin. Password
hash values and node properties are redacted in Bolt results and CLI reads,
including history. Account changes are native append-only retract/assert
transactions on increasing logical ticks, so their audit history is retained.
Only `kala user` commands may write this world through the CLI; renamed copies
are protected by the marker too. A user world cannot be named `system`.

Account snapshots reload using the same file-stamp cache as worlds. Grants and
password changes take effect at the next login; an existing session retains its
identity until LOGOFF or disconnect. `kala user` takes a writer lock only for its
command and works while the server is running. The last `admin:*` account cannot
be dropped or have that grant revoked.

TLS is not implemented. Non-loopback account listeners need the explicit
`--insecure-allow-plaintext` opt-in, which prints a warning that passwords travel
unencrypted. This opt-in never permits `--auth none` on a non-loopback address.

## Protocol

The plain handshake selects the highest offered version in 5.0–5.4, including
minor-version ranges. Unsupported manifest proposals are skipped in favor of
a supported plain proposal; a manifest-only or 4.x-only proposal is rejected.
HELLO authenticates in 5.0; 5.1+ requires LOGON. LOGOFF returns to authentication.
RUN, PULL, DISCARD, RESET, GOODBYE, ROUTE and 5.4 TELEMETRY are supported. BEGIN,
COMMIT and ROLLBACK return `Neo.ClientError.Request.Invalid` with
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
| NULL | PackStream null / Python None |
| Property/query list (collect, nodes, relationships) | Bolt List / Python list, preserving query order |
| Path | Bolt 5 Path / neo4j.graph.Path; deduplicated Nodes, UnboundRelationships, signed traversal indices |
| i64, f64, bool, string | Integer, Float, Boolean, String |
| Entity reference | Bolt 5 Node: raw integer ID, labels, properties, readable element_id |
| Edge reference | Bolt 5 Relationship: negative tagged ID, endpoints, type, properties and all three element IDs |
| Op | `"assert"` or `"retract"` |
| Precision | `"exact"`, `"day"`, `"month"`, `"year"`, `"decade"`, `"century"` |
| Interval set | List of `[start, end]` lists, null for an open end |

Nodes, relationships and paths are enriched at the result's default tick and canon.
Node labels include its immutable type and active string `label` values.
Properties include every active non-ref attribute and the world handle (if present)
under `kala_handle`. A user attribute named `handle` is an ordinary property and
can coexist with `kala_handle`. The `kala_` schema-name prefix is reserved.
Cardinality-many attributes are sorted lists. A cardinality-one overlap is also
returned as a list so neither asserted value is lost. Reference attributes are
relationships in graph structures; explicit Cypher `v.ref_attribute` returns
the referenced node(s). Graph data is returned verbatim; diagnostics are
escaped with `kala_core::must_escape` rules.

RUN parameters use the front-end's inferred kinds: integers for ticks/i64,
floats for f64, booleans, strings for strings, handles or readable IDs for
entities, and a nonnegative integer or `tx#N`/`N` string for canon. No numeric
coercion; NaN, byte arrays and other mismatched types are TypeError. Cypher
value parameters also accept NULL and lists, while ticks/canon remain integers. Undeclared params are
ignored. Fields omit the variable's `?`; aggregate names retain their text.

## Errors and limits

| Condition | Bolt code |
| --- | --- |
| Cypher/Datalog syntax or unsupported language | `Neo.ClientError.Statement.SyntaxError` (located when available) |
| Missing parameter | `Neo.ClientError.Statement.ParameterMissing` |
| Parameter type mismatch | `Neo.ClientError.Statement.TypeError` |
| Query/enrichment limits or cancellation | `Neo.ClientError.Statement.ExecutionFailed` |
| Authentication failure | `Neo.ClientError.Security.Unauthorized` (connection closes) |
| Missing database privilege | `Neo.ClientError.Security.Forbidden` |
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
Rust tests and the server need no Python or async runtime. Account password
hashing uses the pinned RustCrypto `argon2` dependency.

Implementation references: [handshake](https://neo4j.com/docs/bolt/current/bolt/handshake/),
[messages](https://neo4j.com/docs/bolt/current/bolt/message/),
[server states](https://neo4j.com/docs/bolt/current/bolt/server-state/),
[PackStream](https://neo4j.com/docs/bolt/current/packstream/).

## NULL and Cypher differences

| Behavior | Cypher | Datalog |
| --- | --- | --- |
| Missing property | NULL, preserving the row | No binding; row absent |
| Comparison with NULL | NULL (three-valued logic) | No NULL |
| Filtering | WHERE keeps only true | Boolean; negation via `not` |
| NULL in RETURN | PackStream null | Not produced |

Known differences from Neo4j: each attribute has one stored type; stored property lists are
sorted sets (no order or duplicates); labels consist of one node type plus
`label` values. Cardinality-one overlaps also return lists. Ref properties
return graph references or lists of them. Node/Relationship/Path enrichment always
uses the plan's default tick; explicit `v.key` uses its binding clause's tick.
Cypher parameters support NULL and lists (with input order preserved). Native
ref-to-edge targets remain edge references in unlabelled relationship endpoints;
standalone node scans exclude edges.
Every explicit system-world `password_hash` reference is refused before
execution, including WHERE, maps, RETURN aliases, IS NULL and ORDER BY/LIMIT.
Returning account nodes, including inside Paths, continues to redact hashes.

## Neo4j drivers and tools

The official Python driver is tested through `./e2e/run.sh` with both `bolt://`
and routing `neo4j://` URIs, `verify_connectivity()` and auto-commit
`session.run`, with explicit and default databases. Tests cover aggregates,
OPTIONAL MATCH NULLs, UNWIND list parameters, Path hydration and stored edge
directions, all four procedures below, database visibility and protected
system attributes. Other official drivers and TCP tools such as `cypher-shell`
and JDBC tools are expected to use these Bolt operations; they have not been
run and compatibility is not claimed as tested. Explicit transactions remain
unsupported: driver `execute_query`/`execute_read` still require slice 15.

| Request | Answer |
| --- | --- |
| `CALL dbms.components()` | One row: name=`Kala`, versions=[`5.0.0`], edition=`community` |
| `CALL db.labels()` | Sorted node types at the canon plus distinct active label values at the query tick |
| `CALL db.relationshipTypes()` | Sorted ref-attribute names and inverse names, including deprecated readable definitions |
| `CALL db.propertyKeys()` | Sorted non-ref attribute names plus `kala_handle`; names only, including on system |
| `SHOW DATABASES` | Only databases on which the logged-in identity has any grant; system only for admins |
| `SHOW DATABASE name`, `SHOW DEFAULT DATABASE`, `SHOW HOME DATABASE` | The corresponding filtered database rows |

Each database row has columns in this order: `name, type, aliases, access,
address, role, writer, requestedStatus, currentStatus, statusMessage, default,
home, constituents`. Type is `standard` or `system`; aliases/constituents are
empty lists, access=`read-only`, address=the advertised address, role=`primary`,
writer=true, both statuses=`online`, statusMessage="", and default/home indicate
the configured default database. Metadata listings on `system` work for a
non-admin without opening that world; all filtering uses the shared executor.
Unknown procedures return `Neo.ClientError.Procedure.ProcedureNotFound`.

ROUTE is accepted only when authenticated and Ready. It returns a routing table
with ttl=300, the resolved database name, and the single advertised address in
WRITE, READ and ROUTE roles. Writer=true and the WRITE role keep driver routing
usable; write queries still fail. Database authorization and unknown-database
errors are shared with RUN, with one exception: ROUTE for `system` answers any
authenticated identity, so a routing driver (`neo4j://`) can reach the database
listing there. That table grants nothing; RUN still refuses a non-admin's reads
of `system`. Client routing-context addresses are never echoed.

Impersonation (`imp_user`, a driver session's `impersonated_user`) is refused
on both ROUTE and RUN with `Neo.ClientError.Security.Forbidden`
("impersonation is not supported"); a request is never run with the
connection's own grants instead. A non-string `imp_user` is an invalid request.

RUN checks that the database exists and may be read before it parses the query,
so a malformed query on an unknown or forbidden database gets the database
error. A Path whose nodes would include an edge (a ref attribute may point at
one) is refused with an execution error; return its nodes and relationships
separately.

`kala serve --advertised-address HOST:PORT` sets the public endpoint; without
it, TCP tables use the connection's local socket address (including its actual
port when listening on port 0). **Set `--advertised-address` whenever the
listen address is not loopback:** with `--listen 0.0.0.0:7687` behind NAT, a
proxy or a container, the local socket address is an internal one, and ROUTE
and SHOW DATABASES would hand it to remote clients, which then cannot connect.
`kala serve` prints a startup warning in that case. The Python suite exercises
both forms. HELLO's
`Neo4j/5.0.0` and components' `5.0.0` are Bolt compatibility versions; HELLO's
`kala_version` is Kala's actual version.

**Neo4j Browser and other web-page clients are not supported yet.** They need
Bolt over WebSocket, UNION ALL, `dbms.showCurrentUser()`, `dbms.clientConfig()`,
and SHOW PROCEDURES/FUNCTIONS. Those belong to a later slice; the current
transports are TCP and Unix sockets.
