# kala-bolt

A Bolt 5.0–5.4 server with Cypher and Datalog writes and explicit transactions over Kala's Cypher and Datalog front-ends
and shared query executor. `kala serve DIR` exposes each regular
`DIR/<name>.kala` file as a database (`[a-z0-9_-]+` names only). Symlinks and
invalid names are skipped with one diagnostic per name. After startup, an
authorized lookup of an unknown database or a `SHOW DATABASES` request rescans
the directory. Sessions share one rescan per second. A new world is available
by name on the next eligible scan. Readers open immutable snapshots on first
RUN and retry a failed open on the next request. Removing a file leaves its
name registered until restart. File stamps (length, modification time, file identity and commit tail) trigger a reload of
committed transactions; each result pins one snapshot. A damaged tail is never acknowledged or cut by this server unless it was started with `--acknowledge-recovery` (see "Damaged tails"); otherwise an operator must acknowledge recovery through the CLI. The served directory and its files
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
resolves to `127.0.0.1`; `127.0.0.0/8` and `::1` are accepted. Remote TCP listeners require accounts and either TLS or an explicit
`--insecure-allow-plaintext`. Local TCP and Unix listeners also support `--auth none`. Setting the shutdown flag closes every listener and
client socket, cancels queries and joins connection threads; `kala serve` sets
it on SIGINT or SIGTERM, so `systemctl stop` and `docker stop`
shut down cleanly and remove Unix socket files.

A single user database at startup is the implicit default (`system` is excluded).
The default stays fixed until restart. With several user databases, set
`--default-db` or supply the driver's session database. With zero user
databases, start the server without a default and name each new world in the
driver session. The library exports
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
peer identity, a reached address, and an independent shutdown handle. TCP, TLS and
Unix streams use one session implementation, including authentication, message
size/depth bounds and failure/RESET behavior. A blocking rustls wrapper encrypts the stream before
the first Bolt handshake byte. `Transport` requires no raw descriptor, and
`Server::with_transport_wrapper` can decorate accepted streams; conformance
also runs through a wrapper that splits every read and write into single bytes.
The peer used for account matching and the reached address are read from the
accepted connection before it is wrapped, so a wrapper cannot change who
connected. Per-socket setup happens after `accept`: a connection that cannot be
set up is dropped with `event=bolt_connection_dropped` and the server keeps
accepting.

`ServerConfig::listen` is a list of `ListenAddr` values; `Server::local_addrs()`
returns every actual bound endpoint in order. `--listen` accepts bare
`IP:PORT`, `[IPv6]:PORT`, `localhost:PORT`, the same forms after `bolt://` or `bolt+s://`, and
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
reclaims. `kala serve` runs this cleanup on SIGINT and SIGTERM; only an
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
local for `--auth none` and need no plaintext opt-in; every non-loopback plaintext TCP
listener requires accounts and `--insecure-allow-plaintext`.

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
read and write; `write` and `read` do not imply each other.
Denied queries return `Neo.ClientError.Security.Forbidden`. `system` requires
`admin:system` or `admin:*`, even if the account has a read grant on it.

The reserved system database is queryable with Cypher or Datalog by an admin. Password
hash values and node properties are redacted in Bolt results and CLI reads,
including history. Account changes are native append-only retract/assert
transactions on increasing logical ticks, so their audit history is retained.
Only `kala user` commands may write this world through the CLI; renamed copies
are protected by the marker too. A user world cannot be named `system`.

Account snapshots reload using the same file-stamp cache as worlds. Password
changes take effect at the next login: a connection that is already
authenticated stays so (as in MySQL), and to cut one off the account is
dropped. Grants are captured at login and
re-read, if any account changed since, whenever the connection sends BEGIN or
an auto-commit write: a revoked `write` (or a dropped account, which is then
refused everything) stops a pooled connection's next transaction or write
instead of lasting until its next login, and a new grant is picked up the same
way. The re-read is of **the account the connection logged in as**, not of
whatever account has that name now: an account that was dropped and created
again as the same `user@host` (say, after a compromise, with a new password)
is another account, and connections authenticated as the old one get none of
its grants: they are refused everything, like those of any dropped account,
until they log in again. (An identity carries the account's *generation*,
the tick in `system` at which it was created; a password or grant change
keeps it, a drop and re-create doesn't.) Plain auto-commit reads don't check: for them a session keeps the grants
of its login (or of its last BEGIN or write) until LOGOFF or disconnect. `kala user` takes a writer lock only for its
command and works while the server is running. The last `admin:*` account cannot
be dropped or have that grant revoked.

### TLS for remote accounts

Use `bolt+s://` on the server. The wrapper negotiates TLS 1.3 or 1.2 with the
ring provider and uses the server certificate for authentication. It requests
no client certificate, negotiates no ALPN protocol, and disables early data
and session resumption. A TLS connection takes a slot from the shared cap
before the handshake. The TLS and Bolt handshakes share the handshake
deadline. Partial records and blocked writes use the caller’s absolute deadline.
Outgoing buffering is capped at 64 KiB per connection. An orderly Bolt close
sends a best-effort TLS close notification within the write timeout.

On startup Kala checks the PEM chain, leaf validity, matching private key and
Unix key permissions before binding. SIGUSR1 repeats those checks and installs
the new pair for future connections. Failed reloads log one error and preserve
the active identity. Existing connections keep their original session. The
reload runs off the signal thread, and signals that arrive while one is in
flight coalesce into one more run. On a server without TLS listeners SIGUSR1
logs `event=bolt_tls_reload_ignored reason=no_tls_listener` and does nothing.
SIGHUP, like SIGTERM and SIGINT, is the clean shutdown.

The key file must give group and others no access (`mode & 077 == 0`, as ssh
requires); a chain file that group or others can write is a warning. Validity
dates are read by a small strict DER walk (`transport/tls/der.rs`), and only
when the files are read, at startup and at reload: a leaf that expires during
uptime is served until it is replaced. The accept loop re-checks the date once
an hour and logs a warning while the leaf is within 14 days of its end or
expired.

A client whose handshake rustls rejects is sent the fatal alert (bounded by the
handshake deadline and one second) before the close. Bytes that are not TLS at
all get a bare close instead: a plaintext Bolt driver would read the alert
record as the Bolt version reply. Handshake failures are
logged through one rate limiter (`suppressed_since_last=N`). The auth log line
says `peer=tls` for these connections, `peer=tcp` for plaintext TCP and
`peer=unix` for a Unix socket. A failed record write ends the stream: later
writes and the close notification fail at once.

The `ring` provider offers X25519 and the NIST curves only: there is no
post-quantum hybrid key exchange (X25519MLKEM768 comes with `aws-lc-rs`, which
needs cmake). `ring` compiles C and assembly, so building Kala needs a C
compiler.

Each non-loopback plaintext listener requires `--insecure-allow-plaintext`,
including on a server that also serves TLS. The accepted TCP peer address
determines account host matching. A remote TLS peer still needs a password
account, and only local listeners support `--auth none`.

ROUTE and SHOW DATABASES return a bare host and port for TLS connections.
The default is the endpoint reached by this connection. Routing drivers reuse
the encryption scheme of their original URI, so any configured
`--advertised-address` must also be reachable over TLS and covered by the
server certificate when clients verify its identity. A server has one
advertised address: with a plaintext and a TLS listener together it is given to
clients of both, so routing over the listener it does not belong to fails
closed (a `neo4j://` client is sent to a TLS-only port, or a `neo4j+s://`
client to a plaintext one; neither side falls back).

## Protocol

The plain handshake selects the highest offered version in 5.0–5.4, including
minor-version ranges. Unsupported manifest proposals are skipped in favor of
a supported plain proposal; a manifest-only or 4.x-only proposal is rejected.
HELLO authenticates in 5.0; 5.1+ requires LOGON. LOGOFF returns to authentication.
RUN, PULL, DISCARD, RESET, GOODBYE, ROUTE, BEGIN, COMMIT, ROLLBACK and 5.4 TELEMETRY are supported. Malformed PackStream closes after FAILURE when possible;
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
run and compatibility is not claimed as tested. Explicit transactions, `execute_query`, `execute_read` and `execute_write` are covered by the driver suite.

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
usable; Datalog writes require write privilege. Database authorization and unknown-database
errors preserve the existing read behavior; write-only identities may also ROUTE. One exception: ROUTE for `system` answers any
authenticated identity, so a routing driver (`neo4j://`) can reach the database
listing there. That table grants nothing; RUN still refuses a non-admin's reads
of `system`. Client routing-context addresses are never echoed.

Impersonation (`imp_user`, a driver session's `impersonated_user`) is refused
on ROUTE, BEGIN and RUN with `Neo.ClientError.Security.Forbidden`
("impersonation is not supported"); a request is never run with the
connection's own grants instead. A non-string `imp_user` is an invalid request.

RUN checks that the database exists and the caller has read or write access before it parses the query,
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
transports are TCP, TLS over TCP and Unix sockets.

## Writing and transactions

Cypher and Datalog both stage native writes. A blind Cypher CREATE needs
`write`. Reading clauses, SET, REMOVE, DELETE, MERGE, property reads and
RETURN also need `read`. Admission checks the entire statement before opening
a writer. Both languages use the same database, mode and privilege checks,
with reduced diagnostics for a write-only identity. Pass an empty parameter
map for Datalog writes. Cypher accepts parameters in both pipelines and updates,
converted with the kinds the statement gives them as for a read query (a handle
string for a ref position names the entity; a wrong type is a TypeError before
anything runs). `UNWIND … CREATE` has a reading part, so it needs `read` as well
as `write`.

**Type constraints.** `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:Label)
REQUIRE n.prop IS :: TYPE` declares attribute `prop` (`write`, summary type
`s`), exactly as `[:attribute :prop :type]` does. A constraint only ever *adds*
a declaration:

- A label that is not a node type yet is declared as one, by the
  schema-on-write rule (an inferred node type). **That is
  permanent**: node types cannot be removed or renamed away, so check the
  label's spelling. A strict world refuses it instead.
- With `IF NOT EXISTS`, if `prop` is already an attribute — whatever its type
  or other labels — the statement does nothing: no schema entry, no node type
  for a new label, `declared: 0`.
- Without it, the same type is a no-op (or confirms an inferred attribute as
  declared). A different type is refused for every identity, `admin` included:
  `attribute :prop already exists with type T; change it with // kala:datalog
  [:attribute …] or [:migrate …]`. Changing a definition is done with those
  Datalog forms (which need `admin`).

**Where an auto-commit Cypher write reads.** A write never replays the log to
get a snapshot; which handle its reading part runs on depends on the statement:

| Statement | Reading part runs |
| --- | --- |
| Blind: only `CREATE` or a constraint, no `RETURN` | Nowhere to speak of (one empty row), compiled on the writer under the database's writer mutex |
| Reads (`MATCH`, `UNWIND`, `SET`, `MERGE`, `RETURN`, …), default (optimistic) mode, and the cached reader snapshot is the current committed state | **Off the mutex**, on that snapshot, before the writer is taken: a heavy MATCH never blocks other writers |
| Reads, but there is no current reader snapshot to be had without a replay; or weak mode | **Under the mutex**, on the writer |

- The reader snapshot is "current" when a read query loaded it and nothing was
  committed since, or when it is behind and unused: the write path then applies
  just the log's tail to it (never a reload). It is not to be had when no read
  query ever loaded one, while a query is still reading the stale one, or while
  a reader is reloading; a bookmark the snapshot hasn't reached is waited for on
  the writer.
- Off the mutex, a commit that lands between the read and the stage is a
  transient conflict only if it touched what the statement read, and a
  tick-less write lands on the latest tick the reading part saw (the summary
  reports `latest_tick_moved` if another commit moved it).
- Under the mutex other writers wait, so the reading part has a tenth of a
  query's step budget (1,000,000 steps), besides the 10,000 rows a statement
  may write from and its deadline; over it the statement fails with `query
  step limit exceeded` and nothing written. Staging has the full statement
  budget on both paths.
- Weak mode checks no reads, so it always reads the writer's own state: a
  snapshot a commit may have overtaken would widen its lost-update window.

A `RETURN` that cannot be sent (message or result limits, deadline) is refused
before the commit, with nothing written. That check renders the answer from the
staged transaction without recording reads: only what the statement itself read
(its MATCH, the values a SET replaces) can conflict, not every property of a
returned node. A refused Cypher write logs `event=bolt_write_refused` with
`reason=privilege|conflict|statement`, as a Datalog one does.

```python
session.execute_write(lambda tx: tx.run(
    "MATCH (c {kala_handle:$h}) SET c.hp = $hp AT TICK $t",
    h="greg", hp=30, t=700,
).consume())
```

A Cypher write returns rows only for RETURN. Auto-commit renders the committed
properties and final IDs. Inside BEGIN, write RETURN sees the transaction's
staged changes. A subsequent query reads the snapshot. The committed
`kala.ops` list gives operation positions, kinds, ticks, changed counts and
entry IDs. `ops_omitted` counts operations left out to fit the response.
`relationships-created` counts newly interned edges in both languages,
`relationships-deleted` counts retracted reference facts, and `labels-added`
and `labels-removed` count changes to the label attribute. Nodes keep their
permanent identity after DELETE. A Cypher summary has type `rw` for a
reading pipeline or RETURN, otherwise `w`.

Use `SET CONCURRENCY_MODE = 'weak'` or `'optimistic'` as the first statement
of an explicit transaction, or set the mode in BEGIN metadata.
For example, with a `read` + `write` account on the varrow fixture:

```python
from neo4j import GraphDatabase

with GraphDatabase.driver("bolt://localhost:7687", auth=("writer", "")) as driver:
    with driver.session(database="varrow") as session:
        result = session.run('// kala:datalog\n[:assert :greg :alias "Ned" :at 300]')
        print(result.data())
        summary = result.consume()
        print(summary.counters.properties_set, summary.metadata["kala"])
        print(session.last_bookmarks())
        session.execute_write(lambda tx: tx.run(
            '// kala:datalog\n[:assert :greg :alias "The Grey" :at 301]'
        ).consume())
```

The official driver suite in [`e2e/`](../../e2e/) exercises this flow with both
`bolt://` and `neo4j://`, managed retries, explicit commit/rollback and bookmarks.
Python 5.28.8 retains custom auto-commit metadata in `summary.metadata`, but
its transaction API discards custom COMMIT metadata; use socket COMMIT metadata
for the full explicit result, and `session.last_bookmarks()` for its bookmark.

| State | Accepted transaction transitions |
| --- | --- |
| Ready | BEGIN → TxReady; auto-commit RUN → Streaming |
| TxReady | RUN → TxStreaming; COMMIT or ROLLBACK → Ready |
| TxStreaming | PULL/DISCARD remains here until fully consumed, then TxReady |
| Failed | Requests are IGNORED until RESET → Ready |

COMMIT/ROLLBACK outside a transaction, nested BEGIN and COMMIT while streaming
are invalid requests. RESET, GOODBYE, disconnect, timeout and server shutdown
discard staged writes and release the write slot. After a transaction timed
out, RESET still answers SUCCESS and GOODBYE still closes; any other request
gets the `TransactionTimedOut` failure. There is one open result:
`qid` must be `0` or `-1`. RUN commits an auto-commit write before sending its
SUCCESS, even if the client never pulls. The whole text of a write RUN is
parsed, and every form's privileges are checked, **before anything is staged
or committed**: a syntax error or a privilege refusal anywhere in the text
fails the RUN with nothing written (no "prior forms committed"), in
auto-commit and inside BEGIN alike. Then several top-level forms commit
individually, in order, and a form that fails for another reason (a store
rule, a conflict, a timeout) stops them: it reports `form N failed; K prior forms
committed`. Once K > 0 the message also names the last commit (`the last as
tx T (bookmark kala:<db>:T)`, so the client can read its own writes), says
only that *this form* wrote nothing, and is never a transient error that
invites the driver to replay the whole RUN. With K = 0 the original failure
and its code stand. Inside BEGIN, all forms across RUNs commit
as one transaction; any failed form discards everything and enters Failed.

BEGIN and auto-commit RUN accept `db`, `mode` (`r`/`w`, default `w`),
`tx_timeout` (nonnegative milliseconds, capped by the server; zero uses the cap),
`tx_metadata` (a map, at most 16 KiB), and `bookmarks`.
`tx_metadata.concurrency_mode` is `weak` or `optimistic`; other metadata is ignored.
A leading `[:concurrency :weak]` or `:optimistic` overrides this default, but only
the first write form may set it inside an explicit transaction.
Notification preferences are accepted and ignored. Impersonation is refused.

**Snapshots.** Every BEGIN, in any mode, pins the shared read snapshot (after any
bookmark wait) and costs nothing else: drivers begin in write mode by default,
also for reads. The private writable handle is opened, and a write slot taken, at
the transaction's **first write form**, on the world as it is then:

- an initial Datalog write can adopt that world if it ran before any query.
  A Cypher update pins its input to BEGIN, including the single empty row
  of a blind CREATE. Its later MATCH clauses keep reading BEGIN's snapshot.
- a transaction that queried first has its queries' read keys recorded on the
  handle. If **other transactions committed in between**, those keys are
  checked at COMMIT against everything committed since BEGIN's snapshot, not
  only since the handle opened: a commit that wrote something the queries
  read (an attribute they read, a new entity of a node type they scanned) is
  a conflict, with the transient code that drivers' `execute_write` retries;
  a commit that touched nothing they read is not, so a steady background
  writer doesn't starve read-then-write transactions;
- such a transaction **keeps reading the BEGIN snapshot** for its whole life,
  also after its first write: one transaction never shows two worlds. (Its
  `:set` and `:if-missing` forms read through the writable handle, which is
  newer; what they read is checked from BEGIN's snapshot as well, so they can
  conflict but never slip through.)

An accumulated read set exceeding 256 attributes, node types and rendered
identities falls back to checking every commit since BEGIN. Resolving the
same names across a schema rewrite can also require this conservative check.
Appending an unrelated attribute preserves the earlier name bindings.

**The latest tick is BEGIN's.** A tick-less write in an explicit transaction
lands on the latest tick of the snapshot BEGIN pinned, whether another
transaction committed before the transaction's first write or after it, and
if the world's latest tick is another one at COMMIT the summary carries the
`latest_tick_moved` notification (`latest tick moved 601 → 610 while this
transaction ran`). An auto-commit write begins and commits in one step, on
the latest tick it finds.

Queries read the transaction's committed snapshot. They do **not** see the transaction's
own writes. Later write forms can resolve handles created by earlier forms and
`:set`/`:if-missing` see staged writes through the native transaction. Optimistic
queries record coarse attribute, node-type and entity-definition dependencies;
unknown labels and undeclared properties record their absent names. Returning
a whole node or relationship records its identity, including graph values
nested in lists and paths. A query that runs after
its transaction staged a schema change (a rename, a declaration, also an inferred
one) records a whole-world read: its names might mean other attributes in the
transaction's provisional schema. Weak mode skips conflict checks. Sending a provisional entity ID pins it: a commit that would renumber it
fails transiently. Discarding the record does not expose that ID.

### Write privileges

| Operation | Privilege on the selected user database |
| --- | --- |
| Query | read |
| `[:assert …]`, `[:retract …]`, plain `[:create …]`, declarations (`[:attribute …]`, `[:node-type …]`, names a write declares by inference) | write |
| `[:set …]`, `[:create … :if-missing]`, `[:correct …]` | read and write |
| Cypher: `CREATE` with no reading part and no `RETURN`; `CREATE CONSTRAINT … IS :: TYPE` (it only adds: see "Type constraints") | write |
| Cypher: anything with a reading part (`MATCH`, `UNWIND`, …), `SET`, `REMOVE`, `DELETE`, `MERGE`, `RETURN` | read and write |
| `[:world-setting …]`, `[:rename …]`, `[:deprecate …]`, `[:cardinality …]`, `[:migrate …]` | admin (`admin:<db>` or `admin:*`) |
| `[:attribute …]` or `[:node-type …]` that **changes** an attribute or node type the committed schema already has | admin (`admin:<db>` or `admin:*`) |
| ROUTE | read or write |
| Any write to system, including as admin | Always forbidden |

Admin implies read and write on user databases. The admin rows are policy for
remote writers, not a store rule: those forms change what existing data or the
world's rules mean for everyone else. The CLI is not affected (`kala transact`
is bounded by file permissions).

Declaring is adding; **re-declaring is changing**. `[:attribute :located_in
:ref :many]` on an existing `located_in` relabels its cardinality exactly as
`[:cardinality …]` does (and a re-declaration can also set a reverse
cardinality or an inverse name, or, while the attribute has no facts, its
type, kind, direction or index), so without `admin` it is refused with
`Forbidden`, naming the attribute and `admin`. So is confirming a name that
was declared by inference (`[:attribute :age :i64]` after a write inferred
`:age`, or `[:node-type …]` for an inferred type): that edits an existing
definition too. Declaring again exactly what exists changes nothing and is
allowed; so is everything about names the same transaction declared itself.
"Existing" means committed: if another transaction commits a different
declaration of the same new name first, the COMMIT is refused the same way.
`[:world-setting …]` accepts only the keys Kala knows (`:schema`,
`:concurrency`, `:tick_unit`), in the CLI too; another key is an error that
lists them. `--auth none` grants `read:*`
only. `--read-only` refuses every write form before world I/O. BEGIN itself is
never refused for being in write mode: on a read-only server, on `system` (for
an admin) and for an identity without `write`, a default-mode transaction reads
normally and the refusal comes when a write form arrives.
A read-mode transaction cannot write. Every form of a RUN is parsed and its
permissions are checked before a writable handle is opened, a write slot
taken or anything staged or committed: a refused RUN wrote nothing, also when
only its last form is refused, and (under `--acknowledge-recovery`) cut
nothing. The one exception is the re-declaration row above: whether an
`[:attribute …]` or `[:node-type …]` changes an existing definition depends on
the schema, so it is refused when the form is staged. Staging is private:
inside BEGIN the transaction fails and nothing of it commits; in a multi-form
auto-commit RUN the forms before it were already committed and the failure
says so (`form N failed; K prior forms committed`).

Write-only reports retain form/op/kind/tick/changed/entries and newly created
entity identities. Existing entity details and non-true `created` fields are null;
write-result nodes contain identity/type but no stored properties or handle.
Every store-rule failure (an existing or unknown handle, a held value, a wrong
type, a same-tick conflict …) is the same code, `Neo.ClientError.Statement.ExecutionFailed`,
with the same message, `write failed; details require read privilege`, so the
failures can't be told apart; syntax errors and privilege refusals, which
describe the request and not the data, keep their own codes.
Overlap descriptions contain only `overlap details require read privilege`.
No held values appear in these records, errors or notifications. Changed counts
remain visible, as do commit IDs and counts. Inferred declaration names and
latest-tick movement remain visible. **Write-only access limits what is
returned; it is not a confidentiality boundary**: a changed count of 0 or 1 and
the latest tick still tell a determined writer something about what is held.
Queries retain their ordinary property-rich
Node/Relationship mapping for identities with read access.

### Write records and summaries

Each Datalog operation produces one record with fields:

```
["form", "op", "kind", "tick", "changed", "entries", "entity", "created"]
```

`form` is one-based within the RUN, `op` is one-based within that form, `kind`
is the verb without a colon, and `tick` is the tick actually used. `entries`
is a list of integer entry IDs; inside BEGIN it is empty until COMMIT.
`entity` identifies a created/found node (otherwise null); `created` is true/false
for create operations, null for other operations. Plain create reports true.

The final auto-commit PULL or explicit COMMIT summary contains `db`,
`bookmark: "kala:<db>:<tx>"`, `type` (`w`, `rw` when queries also ran, or `s`
for schema-only forms; the PULL summary of a write RUN inside a transaction
carries the same `type` for that RUN), and:

- `stats`: `nodes-created`, `relationships-created`, `properties-set` (non-ref
  asserted fact/revision entries), and `contains-updates`. Counters describe
  finalized committed operations, including conditional creates that become no-ops.
- `kala`: `tx`, `changed`, `declared`, `entries`; `ids` when IDs moved (pairs of
  provisional/final raw IDs); `latest_tick_moved: {from, to}` when applicable.
- `notifications`: maps with `code`, `title`, `description`, `severity`.
  Codes are `Kala.Warning.Overlap`, `Kala.Information.OverlapResolved`,
  `Kala.Information.OverlapsOmitted`, `Kala.Warning.LatestTickMoved`, and
  `Kala.Information.InferredDeclaration`. Severities are WARNING/INFORMATION.
  The native commit caps introduced and resolved overlaps at 100 each and
  reports omitted counts in the final overlap notification.

Optional summary details share a bounded budget while each form is committed;
excess details are counted rather than retained. If the final encoded summary
still exceeds the wire-message limit, detail lists are shortened in this order: notifications, operations, ID mappings, entry IDs. `kala.notifications_omitted`,
`kala.ids_omitted` and `kala.entries_omitted` report exactly how many elements were
omitted. The bookmark, transaction number, totals and counters are retained.
This ensures a durable commit still returns SUCCESS. Fetch complete history with
`kala log` when detail is omitted. Write-capable protocol settings require at
least a 1024-byte message limit and nesting depth 8.

Bookmarks on BEGIN and auto-commit reads/writes must be lists of at most 64
strings, each at most 256 bytes and of the form `kala:<db>:<tx>`. A bookmark
that names **another database is ignored** (drivers' bookmark managers send
every bookmark they hold with every request); only a malformed one is refused.
Readers refresh before pinning their snapshot and wait at most the busy timeout
for the highest requested transaction of the selected database; an auto-commit
write checks its bookmark against the writable handle instead (no reader
refresh). Future bookmarks fail transiently after that bound.

### Errors and limits

| Failure | Bolt code |
| --- | --- |
| Invalid state, options, parameters or transaction cap on forms/text | Neo.ClientError.Request.Invalid |
| Write syntax | Neo.ClientError.Statement.SyntaxError |
| Wrong value type | Neo.ClientError.Statement.TypeError |
| Same-tick conflict, duplicate handle/type mismatch | Neo.ClientError.Schema.ConstraintValidationFailed |
| Unknown handle/attribute, deprecated attribute, strict schema, event retract, unknown world setting, other store-rule errors | Neo.ClientError.Statement.SemanticError |
| Any of the three rows above, for an identity without `read` | Neo.ClientError.Statement.ExecutionFailed |
| A form after committed forms of the same auto-commit RUN that would otherwise be transient | Neo.ClientError.Statement.ExecutionFailed |
| Missing privilege (`write`, `read` or `admin`), revoked grant or dropped account, system write, read-only server | Neo.ClientError.Security.Forbidden |
| Write in read transaction | Neo.ClientError.Transaction.ForbiddenDueToTransactionType |
| Optimistic conflict (also: queried before another commit, then wrote), diverged handle, future bookmark, write-slot cap (total or per account) | Neo.TransientError.Transaction.DeadlockDetected |
| Writer lock busy timeout | Neo.TransientError.Transaction.LockAcquisitionTimeout |
| Transaction expiry | Neo.ClientError.Transaction.TransactionTimedOut |
| Malformed bookmark | Neo.ClientError.Transaction.InvalidBookmark |
| Recovery required or store I/O failure | Neo.DatabaseError.General.UnknownError |

The pinned Python driver marks generic transient errors retryable; neither
Outdated nor DeadlockDetected has a specialized entry, so Kala uses the familiar
DeadlockDetected fallback. The read-mode code is explicitly listed by that driver.
ConstraintValidationFailed uses Neo4j's Schema namespace. Recovery failures direct
an operator to a CLI write with `--acknowledge-recovery`; the server does it
itself only when started with that flag (see "Damaged tails").
The cached writer is dropped, so that the retry opens a fresh handle, only after
an error that leaves it unusable (a diverged or corrupt log, an I/O error, a
recovery error); an ordinary failed write keeps it, so a client repeating a bad
request can't make every writer replay the log.

| Serve flag | Default | Meaning |
| --- | --- | --- |
| `--read-only` | false | Refuse write forms (transactions may still read) |
| `--max-write-transactions` | 8 | Explicit transactions that have written and not ended; one account may hold at most half of them (at least 1) |
| `--tx-timeout` | 60s | Maximum transaction lifetime; bounds all server-side work in it (queries, staging, COMMIT, sending responses) |
| `--tx-idle-timeout` | 30s | How long the client may stay silent between a response and its next request |
| `--busy-timeout` | 5s | Writer mutex/file lock or bookmark wait |
| `--acknowledge-recovery` | false | Cut a world's damaged last transaction without an operator (see "Damaged tails") |

Durations accept `ms`, `s`, `m` or bare seconds. Native defaults live in
`ServerConfig`. Each transaction accepts at most 256 forms. The 16 MiB transaction budget
includes write text and, for Cypher, parameters and the estimated memory for
expanded primitive operations and their reports. Datalog also observes its
per-form limits.
Materialized write records are capped at 16 MiB per RUN. Socket messages remain
bounded by `max_message_bytes` and `max_depth`. The idle clock starts when a
response has been sent and stops when the next request has arrived: a statement
or COMMIT that takes longer than `--tx-idle-timeout` is not idle and is bounded
by `--tx-timeout` (or the client's shorter `tx_timeout`) only. A request's
deadline is checked inside the executor, where the cancel flag is; no thread is
started per query. Transaction expiry also releases
an idle or trickling client's slot; partial outbound responses close the connection.
The durable append/fsync cannot be interrupted: once appended, the server reports
success, even if completion crosses the deadline.

Each explicit transaction that writes owns one replayed world handle and holds
a slot from its first write form until it ends; a transaction that only reads,
in any mode, pins the cached read snapshot and holds no slot. The cap is
`--max-write-transactions` in total and half of it (at least 1) per account
(`user@host`), so one account cannot keep every other from writing; over either
bound the write form fails with the transient code. Auto-commit uses one
mutex-protected writable handle per database, caught up on each use (it takes no
slot and never reloads the reader snapshot). The mutex is never held across
client I/O, and a panic while it was held costs only that handle, which is
rebuilt from the log. Reader file stamps make subsequent connections see
committed writes.

### Damaged tails

A *damaged* last transaction (all its bytes are in the file but its checksum
fails, and nothing valid follows) may have been acknowledged to a client before
a crash, so no Kala process cuts one unasked. By default the server serves reads
of such a world and refuses its writes with the database-class error until an
operator runs a write command with `--acknowledge-recovery`
(`kala transact WORLD --acknowledge-recovery …`); the file stays byte-identical.

`kala serve DIR --acknowledge-recovery` is the opt-in for deployments whose
worlds can be rebuilt: when a writable handle meets a damaged tail (when it is
opened, or later under the cached handle), the server acknowledges it, saves
the bytes to a quarantine file (`<world>.kala.torn-<offset>`), cuts them under
the writer lock and carries on with the write. Each cut prints one line on
stderr, `event=recovery_acknowledged db=<name> bytes=<n> quarantine=<file name>`,
and startup prints a WARNING that the flag is on.

**What the flag accepts:** the cut transaction's entry IDs and tx number are
issued again to the next commit. A client that still holds an entry ID from the
cut transaction and sends `[:correct <id> …]` would correct a *different* fact.
Without the flag an operator decides what the lost transaction means first.

The flag never applies to `system` (a damaged `system.kala` is always manual:
`kala user … --acknowledge-recovery`), is ignored by a `--read-only` server
(which says so at startup), and is not needed for a *torn* tail (a transaction
cut short, never acknowledged), which is cut automatically as before.

Writes have **at-least-once** delivery under a lost connection: the log may commit
before SUCCESS reaches the client. Do not blindly retry non-idempotent creates
after an unknown network outcome. Use stable handles and `:if-missing` when that
matches the application's intent. No transient failure is deliberately returned
after the same RUN has committed an earlier form.

Structured write logs contain connection ID, database, authenticated user,
transaction number, form count and changed count, or a refusal category. They
never contain Datalog text, values, handles or credentials.
