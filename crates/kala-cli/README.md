# Kala CLI

Kala explores temporal story worlds. Build the sample world and open a read-only
Datalog session:

```sh
kala load crates/kala-core/tests/fixtures/varrow.yaml varrow.kala
kala repl varrow.kala
```

```text
kala> .at -210
kala> [:find ?a :where [:edwin :ally_of ?a]]
query at tick -210, canon tx#12: 2 rows
  ?a
  greg
  tomas
kala> .at -195
kala> .canon 11
kala> [:find ?a :where [:edwin :ally_of ?a]]
query at tick -195, canon tx#11: 3 rows
  ?a
  greg
  mara
  tomas
kala> .quit
```

The world opens once, read-only, without a writer lock. All answers use that
snapshot until `.reload`, even if another process changes or replaces the file.
Session defaults apply only when a query has no explicit `:at` or `:canon`.
`kala repl WORLD --at -210 --canon tx#11` sets the initial defaults.

Queries, `.rule` definitions and `.explain` queries may span lines. Continuation
uses the `  ...> ` prompt. Strings, escapes and `;` comments use Datalog's normal
lexer. An unmatched closer reports an error immediately. Blank lines do not
finish incomplete input. While input is incomplete every line belongs to it,
even one starting with `.` (it may continue a string); only a line that is
exactly `.cancel` is a command, and it discards the input. Each accumulated input is capped
at 65,536 bytes. Session parameters and rule definitions are also bounded.

| Command | Effect |
| --- | --- |
| `.at [T]` | Set the default signed 64-bit tick; no argument clears it. |
| `.canon [C]` | Set the default canon (`N` or `tx#N`); no argument clears it. |
| `.param NAME VALUE` | Set a parameter, with or without `?` in its name. Values use the same type-directed parsing as `query --param`, except that surrounding whitespace is trimmed (use `kala query --param` for a value that starts or ends with spaces). |
| `.unparam NAME` / `.params` | Remove a parameter / list stored parameters. |
| `.rule [[(name ?a) ...]]` | Add definitions using the `:rules` vector syntax. A query only compiles the session rules it reaches, so a broken rule only fails queries that call it. |
| `.unrule NAME` / `.rules` | Remove all definitions of a name / list stored definitions. |
| `.explain QUERY` | Print a plan with the session defaults, rules and parameters applied. |
| `.load FILE` | Run one UTF-8 query file (regular files only, at most 65,536 bytes). |
| `.json on` / `.json off` | Select JSON / text query results, using the same format as `kala query`. |
| `.schema` | List node types and each attribute's type, kind, cardinality, direction, inverse, reverse cardinality and indexing. |
| `.info` | Print information about the opened snapshot, as `kala info` does. |
| `.reload` | Reopen the world and report its latest transaction and tick. |
| `.limits` | Show per-query limits: 100,000 rows, 10,000,000 steps, 256 MiB. |
| `.cancel` | Discard incomplete input. |
| `.help` | List commands. |
| `.quit` / `.exit` | End the session. |

A query receives only the session parameters declared in its `:in`; missing
parameters are errors and unused session parameters are ignored. Query-local
rule definitions replace session definitions of the same name for that query.
Errors in stored rules identify the rule and its original definition location.

Ctrl-C at the prompt clears the buffer. During execution it cancels the query
without returning partial results, then returns to the prompt. Ctrl-D ends the
session. Cancellation is checked at executor work charges; a storage read
already in progress completes before its charge is checked.

With redirected stdin or stdout the same command loop uses plain lines, without
prompts, ANSI or signal handlers. Results are separated by a blank line; errors
go to stderr and the loop continues:

```sh
printf '.at -210\n[:find ?a :where [:edwin :ally_of ?a]]\n' | kala repl varrow.kala
```

Plain mode exits 1 if any command failed, otherwise 0. An incomplete query at EOF
is an error. Startup world-open failures and output failures other than a broken
pipe exit 3. Interactive command errors keep the session open; a normal exit is 0.

## Optional history

No history file is read or written by default. `--history FILE` explicitly opts
in, in either input mode. History is stored as JSON strings, one submitted line
per record. The file is capped at 1 MiB and 1,000 entries, each at most 65,536
bytes; old entries are dropped to stay within the cap. Queries may contain
private story material, so select the file deliberately.

On Unix, new and existing history files use mode `0600`; symlinks, hard links,
special files and aliases of the world are refused. Entries containing terminal
controls or invisible formatting characters are refused on load and omitted
from newly recorded history. Reads and writes use the
same verified handle, with no path-based truncation. On other platforms, only a
new history file is supported; existing paths are refused. File privacy there
follows the parent directory's access controls. The line editor retains a bounded
number of submitted entries in memory. Its current editing buffer can grow
before submission; a very large local terminal paste can allocate beyond the
query cap, though the submitted query is still rejected. Pasted control and
formatting characters are escaped for display without changing query bytes;
cursor placement may be imperfect around those raw characters. Prefer Datalog
string escapes when entering them. Interactive editing requires a terminal
supported by rustyline; redirected input always uses the bounded plain backend.

## Story-check budgets

`kala doctor WORLD --checks FILE` shares one default steps/bytes budget across
all checks. Each completed check deducts its measured steps and peak charged
memory; summing peaks is deliberately conservative. Exhaustion emits one hint,
`story checks stopped: budget exhausted at :name`, covering that check and all
later checks. Hints do not change doctor's exit code. Output renders only the
first five rows in text or 100 in JSON, while retaining the exact total row count.

Doctor story-check output is capped across hints at 10,000 rendered rows and 16 MiB of row text or JSON; exact row counts remain available with an `output_truncated` hint.

## Writing from the terminal

See the [story-author guide](../../GUIDE.md) for a tested tutorial.
`kala transact WORLD FILE` (or `-` for stdin) reads one or more EDN write forms.
A single op is one transaction; a vector of ops commits atomically:

```clojure
[:at 800
 [:create :CHARACTER :kael]
 [:assert :kael :name "Kael"]
 [:assert :kael :located_in :varrow :precision :year]]
```

The first failed form stops the command (exit 1), preserving earlier commits.
Each successful form prints its number, transaction, changed count, per-op tick,
precision and entry IDs, new edge IDs, and introduced/resolved overlap warnings.
A correction that moves a key appends two entries (so reports 2 changed); entity
creation reports its entity ID and 0 fact entries changed. Use printed entry IDs
with `[:correct N :delete]` or `[:correct N :replace :at 830]`. Replacement also
accepts `:value`, `:op` and `:precision`. A no-op prints 0 changed.

`[:create :CHARACTER :kael2 :if-missing]` creates a free handle, or reports
`exists CHARACTER#N kael2` if it already belongs to that type. Another type is
an error. It requires a handle and sees earlier creates in the transaction.
Existing entities write no entry; a form containing only that no-op reports
`nothing committed`. JSON adds `created: true` or `false` for conditional creates.
Plain creates keep rejecting duplicate handles.

`[:set :greg :located_in :ashford :at 225]` replaces what Greg holds **at tick
225** by explicitly retracting removed members and asserting new members.
A single value is a singleton; `[:set :edwin :alias ["the Grey King" "Ned"] :at 800]`
replaces the whole set, and `[]` clears it. Duplicate members count once; held
members keep their original entries and precision. The fixture defines `alias`.

The value position takes every single value `:assert` takes, including the
tagged scalars: `[:set :greg :score [:f64 :inf]]` and
`[:set :greg :friend [:id "CHARACTER#2"]]` each set ONE value. Any other vector
is the member list, and each member is written like an `:assert` value, so
`[:set :greg :scores [1.5 [:f64 :-inf]]]` and
`[:set :greg :friend [:mara [:id "CHARACTER#2"]]]` work. One consequence: an
edge triple as the single value must be wrapped,
`[:set :note :about [[:edwin :ally_of :mara]]]`, because a bare
`[:edwin :ally_of :mara]` is a list of three members (and fails with
``unknown entity `:ally_of` `` unless all three are handles).

`:at` and `:precision` work as on assertions; edge triples can be subjects.
A set with several values on a `one` attribute is accepted with overlap warnings.
Events, deprecated attributes and unknown attributes with `[]` are refused.
A set with no tick uses the transaction's stable default/latest tick.

A same-tick conflict (the set would retract a value asserted at that very tick,
or assert one retracted at it) is refused, and the message says what holds the
tick. For a committed entry it names the entry and the correction that fixes
it: `[:correct N :delete]` removes that assert or retract, and for an assert
`[:correct N :replace :value …]` changes its value instead. When the conflict
is with an earlier operation of the same transaction, the message names that
operation (`op 1 of this transaction`) and no entry ID: nothing was committed,
so there is nothing to correct; change the earlier operation. Entities are
shown by handle, else as `TYPE#n`.
Dumps contain only the stored assert/retract primitives, never `:set`.

A form containing `:set` has a budget of 256 expanded primitive writes/operations
(no-op operations count once). Requested and held sets each have at most 256
members; all sets in one form share a 16 MiB budget for their combined encoded
requested/held values and a 65,536-key read budget (conflict diagnostics included). A limit error aborts the entire transaction.
When the attribute itself holds more than 256 values at the tick, the error says
so (`cannot set :alias: it holds more than 256 values at tick 10 (the :set
limit); use :retract/:assert`): change such a set member by member.

Each commit retains the first 100 introduced and 100 resolved overlap warnings,
in their existing deterministic order, with exact omitted counts. A longer list
ends with `… and N more overlap warnings (run kala doctor to list them)`, or
for resolved ones `… and N more resolved overlap warnings` (no doctor hint:
doctor inspects the full world as it is now, and resolved overlaps no longer
exist).
JSON includes `introduced_omitted` and `resolved_omitted` only when nonzero.

`--json` prints one document per successful form, with fields `form`, `dry_run`,
`tx`, `changed`, `declared`, `entries`, `edges`, `ops`, `schema` and `warnings`. Each op has `position`,
`kind`, `tick`, `precision`, `changed`, `declared`, `entries` and `entity` (null unless created).
Entry IDs are integers; new edges are `edge#N` strings. Warnings have `introduced`
and `resolved` arrays of the same messages as `kala load`. Errors stay on stderr.

`--dry-run` previews the forms in order on one discarded in-memory copy,
including native overlap warnings, so later forms see what earlier ones would
create, exactly as a real run. IDs and tx numbers are provisional. It opens a
read-only snapshot, takes no writer lock, and never changes the log, even if it
has an incomplete recovery tail. A form that changes nothing reports `nothing
committed` (JSON `"tx": null`).

A damaged last transaction (all its bytes present, checksum failing; `kala info`
reports it) may be a write that was acknowledged, so `kala transact` and
`kala repl --write` refuse to open the world, without changing the file, until
rerun with `--acknowledge-recovery`. That cuts the damaged bytes to a
`<world>.torn-N` quarantine file and continues. A torn tail (a write cut short,
never acknowledged) is cut without asking.

`kala repl WORLD --write` holds the exclusive writer lock until exit and queries
see each successful commit immediately. Another writer gets exit 3 with a locked
world message. Without `--write`, write forms are refused. `.at` supplies the
write default: an op's `:at` wins over the transaction's `:at`, then session `.at`,
then the latest tick captured when the transaction began. `.canon` and `.reload`
are refused in write mode; an explicit query `:canon` still reads an older draft.
`.json` applies to writes too, and `.load FILE` accepts one write transaction.

Write input uses the query lexer (strings, escapes, comments, commas). Input is
capped at 65,536 bytes, nesting at 32 levels, ops per transaction at 256 and forms
per file at 256. File input must be regular UTF-8 text. `or` branches count against
the ordinary query limits, with an additional 4,096-node expansion budget.

`(or clause (and clause ...))` requires the same variable set in each branch and
returns a set union. `or-join` and `not-join` remain unsupported. A sixth history
position, `[?e :attribute ?v ?tick ?op ?precision]`, binds precision keywords such
as `:year` and `:exact` (JSON cells have type `precision`). Precision annotates the
stored tick; it does not turn it into a range.


## Accounts (`kala user`)

Every account command takes the server directory, bootstrapping `system.kala`
with `root@localhost` (`trust`, `admin:*`) when absent. The directory itself must
exist: neither `kala user` nor `kala serve` creates it ("no such directory").
`kala serve DIR` defaults to `--auth accounts`; local `auth=None` uses that root
account. Startup announces creation on stderr and warns for every trust account.
`--auth none` never creates, opens or reads `system.kala` (a file of that name is
skipped with one stderr line), writes nothing to the directory, and serves every
other database read-only; the `system` database is never served without accounts.

```sh
kala user add worlds alice@127.0.0.1 --password-stdin < private-password-file
kala user grant worlds alice@127.0.0.1 read varrow
kala user list worlds
kala user set worlds root@localhost --password-stdin < private-root-password-file
kala user revoke worlds alice@127.0.0.1 read varrow
kala user drop worlds alice@127.0.0.1
kala user add worlds local_tool@localhost --trust
```

`add` and `set` read one line with `--password-stdin` (LF or CRLF removed), or
prompt without echo if stdin is a TTY and neither authentication flag is given.
Empty passwords and passwords longer than 4096 bytes are refused. There is no
password-on-argv option. `--trust` is restricted to `localhost`, `127.0.0.1` and
`::1`. Do not use local trust behind a proxy that makes remote clients appear
local. Lists show accounts, plugins and grants, never hashes.

Grant/revoke syntax is `kala user grant|revoke DIR USER@HOST PRIV DB`, where
PRIV is `read`, `write`, `backup` or `admin`, and DB is a database name or `*` (quote it in
a shell). The final `admin:*` account is protected against removal/demotion.
Changes take effect at the next Bolt login without restarting the server;
existing sessions keep their granted identity until LOGOFF or disconnect.

Write commands print the native write result, e.g. `1 changed, written at tick 7`;
granting a held privilege or revoking an absent one prints `0 changed, …`. If the
last transaction of `system.kala` is damaged, writes are refused until rerun with
`--acknowledge-recovery`, which cuts the damaged tail to a private quarantine file
(`system.kala.torn-<offset>`) exactly as `kala transact --acknowledge-recovery`.
Usage errors keep clap's diagnosis but never echo argument values, so a mistyped
`--password=…` is not printed back.

`system` is reserved; ordinary `load`, `transact` and `repl --write` cannot
create or overwrite its content, including renamed system files. Read commands
remain available, with password hashes redacted in text and JSON audit history.
Account operations append native transactions; protect `system.kala` and its
backups because they retain old password hashes too.

See [Bolt accounts and listen rules](../kala-bolt/README.md) for host precedence,
database authorization and the explicit plaintext opt-in needed for a
non-loopback listener. `--auth none` remains loopback-only.

`--listen` is repeatable (at most 16 distinct addresses), with no DNS lookup:

- `IP:PORT`, `[IPv6]:PORT`, or `localhost:PORT` (`localhost` means `127.0.0.1`).
- `bolt://HOST:PORT`, using those same host forms.
- `unix:///absolute/path` on Unix platforms; the path must fit `sun_path`.

No `--listen` still binds `127.0.0.1:7687`. Unsupported URI schemes fail rather
than selecting another transport. `neo4j://` is a client routing URI and cannot
be a listener. All listeners share `--max-connections` and stop on Ctrl-C
(SIGINT) or SIGTERM; SIGHUP does the same. Each of them runs the same clean
shutdown: connections are closed, queries cancelled, Unix socket files removed
and the exit status is 0, so `systemctl stop` and `docker stop` need no special
handling.

```sh
kala serve worlds --listen bolt://127.0.0.1:7687 \
  --listen unix:///run/kala/bolt.sock --unix.mode 0660
```

`--unix.mode` sets octal socket permissions (default `0600`, permitted values
`0000` through `0777`; special mode bits are refused). Granting other-write
requires an explicit mode such as `0666`.

The mode decides who can log in. A new server directory has `root@localhost`
with `trust` (no password) and admin rights, and a Unix peer counts as
`localhost`, so every user who can open the socket is an administrator until
you set a password (`kala user set DIR root@localhost --password-stdin`). With
`0600` that is only the user running the server. `0666` means every local
user, which the default TCP listener on `127.0.0.1` already allows.

- Unix peers match only `localhost` accounts. A `%` host pattern never matches
  one: `alice@%` cannot log in over the socket, `alice@localhost` can.
- For a group of users: put the socket in a directory of its own, owned by the
  server's user with that group and mode `0750`, and use `--unix.mode 0660`.
- The parent directory must belong to the server's user or root. If it is
  group- or other-writable it must be sticky (like `/tmp`); a `0770` directory
  is refused, because anyone who can write to it could replace the socket.
- Clients connect by path, so no directory above the socket should be writable
  by other users.

Next to the socket the server keeps an empty lock file, `.<socket name>.lock`
(mode `0600`), locked while it runs. A second `kala serve` on the same path
fails with `already in use by another kala serve`. A socket left by a crashed
server is replaced on the next start; the lock file is removed on clean
shutdown and may remain after a crash, which is harmless.
See [transport lifecycle and platform rules](../kala-bolt/README.md#transports-and-multiple-listeners).

Unix listeners count as local for auth and plaintext rules; adding a
non-loopback TCP listener still requires the usual opt-in. Official Neo4j
drivers cannot connect to the Unix socket.

Startup prints one line per bound listener in the requested order, only after
all binds succeed (port zero is replaced by its actual port):

```text
kala serve: listening on 127.0.0.1:7687
kala serve: listening on unix:///run/kala/bolt.sock
```

A failed start serves nothing, prints no startup lines, and cleans up sockets
created by earlier binds. Unix sockets are also removed on clean shutdown
(SIGINT or SIGTERM), provided the path still names this server's inode.

`--advertised-address HOST:PORT` is the address routing drivers (`neo4j://`)
and `SHOW DATABASES` are given. Without it they get the local address of each
connection, which is fine on loopback. Set it for every non-loopback listen
(`--listen 0.0.0.0:7687` behind NAT or in a container would otherwise hand out
an internal address); `kala serve` warns on stderr for each such listener when
it is missing. Over a Unix socket, `SHOW DATABASES` instead reports `unix:///path`
and `ROUTE` requires `--advertised-address`; an explicit address is used by both.


## Changing and inspecting the schema

`kala schema WORLD [--canon tx#N] [--json]` reads without a writer lock. It lists
node types, attributes and world settings at the requested canon (latest by
default). Names always use the latest spelling, including at older canons.
JSON fields are `command`, `canon`, `node_types`, `attributes`, and `settings`.
Each node type has `name`, `inferred`, `first_tx`; each attribute has `name`,
`type` (`string|i64|f64|bool|ref`), `kind`, `cardinality`, `direction` (or null),
`inverse` (or null), `reverse_cardinality` (or null), `indexed`, `deprecated`,
`inferred`, and `first_tx`. `settings` is a key/value object; `schema` defaults
to `infer`. Definitions introduced after the requested canon are absent.

Unknown attributes in assertions and unknown node types in creates declare
themselves. Scalars infer their literal type, `state`, `one`, and indexed;
entity values infer `ref`, `state`, `many`, directed and indexed. A report says
`declared :age as i64 (inferred)` or `declared node type FACTION (inferred)`.
Inference happens in operation order. A later wrong-type value aborts the whole
transaction. An unknown retract in infer mode is `0 changed` and declares
nothing; in strict mode it requires a declaration. Indexed strings are limited
to 256 UTF-8 bytes; declare an unindexed string before writing a longer value.

These forms work alone or inside a transaction vector, through `transact` and
`repl --write`:

```clojure
[:attribute :title :string]
[:attribute :biography :string :unindexed]
[:attribute :parent_of :ref :many :directed :inverse :child_of :reverse :one]
[:node-type :FACTION]
[:world-setting :schema :strict]
[:world-setting :schema :infer]
[:rename :titel :title]
[:deprecate :unused]
[:cardinality :located_in :many]
[:migrate :age :string]
[:migrate :age :i64 :skip-unconvertible]
```

Attribute options are `:one|:many`, `:state|:event`, `:directed|:bidirectional`,
`:unindexed`, `:inverse :NAME` and `:reverse :one|:many`. For a new attribute the
defaults are state, one, indexed, and directed for refs; inverse/reverse options
require a directed ref. For an existing attribute of the same type, options left
out keep the current labels: `[:attribute :owns :ref]` on an inferred `owns`
(ref, many) only marks it declared. A named label change is reported, e.g.
`cardinality of :owns: many -> one` for `[:attribute :owns :ref :one]`. Naming a
different type starts from the new-attribute defaults. Repeating an identical
explicit definition or deprecation is a no-op. Cardinality labels can change
with overlap warnings; doctor uses the label at its requested canon. Structural
changes (type, kind, direction, indexing) in place require no fact entry ever
having been logged for that attribute; otherwise use migration. Deprecated
attributes reject new assertions and retractions, but accept corrections and remain
readable. Corrections can delete an entry or replace its value, tick, op or precision,
including entries on a migration's `x@old` attribute.

Renaming keeps the attribute ID: only the new name works at every canon, and the
old name is immediately free. Names containing `@` are reserved for migration;
`kala_` is reserved for Kala at genesis and in later schema edits, and `_KALA_`
is reserved for names introduced after genesis. Both prefixes are case-sensitive.
`edge` (ID prefix) is reserved only for node types. Attributes and inverse names
may use `handle` or `edge`; the built-in query pseudo-attribute is `:kala_handle`.
Upgrading queries: the built-in used to be `:handle`. In a world without a
`handle` attribute an old `[?e :handle …]` query fails with a hint; in a world
that declares its own `handle` attribute it reads that attribute's values
instead, so rewrite old queries to `:kala_handle`.
Attribute and inverse names share a namespace. Node type renaming and inverse renaming
are not supported.

A migration accepts an optional type and the same options; omitted fields retain
the old definition. It converts every change point of the latest canon, keeping
its entity, tick, assert/retract operation and precision, including redundant
assertions. It renames the source to `NAME@old` (then `@old2`, `@old3`, … if needed),
deprecates it and gives the original name to a newly declared attribute. Old
canons read the old data through `NAME@old`; the replacement `NAME` is not defined
before the migration. The original attribute retains its correction trail.
An inverse name transfers to the replacement attribute.

| Conversion | Rule |
| --- | --- |
| i64 → f64 | Exact only for absolute values ≤ 2^53 |
| f64 → i64 | Finite, integral and within the i64 range |
| bool → i64 | false = 0; true = 1 |
| i64 → bool | Only 0 and 1 |
| Non-ref → string | Unquoted text as printed by `kala asof` |
| string → i64/f64/bool | Exact text, no whitespace trimming; bool is `true` or `false`; NaN refused |
| Unindexed → indexed string | Values over 256 UTF-8 bytes are unconvertible |
| Same type | Values preserved; kind/direction/indexing may change |
| ref ↔ non-ref | Refused |

Distinct old values of one entity converting to one new value are collisions;
all their change points are unconvertible. Without `:skip-unconvertible`, the
whole migration fails with a count and examples. With it, those change points
are skipped and counted. A state with any logged retract cannot become an event.
Directed → bidirectional refuses pairs with histories in both directions.
Bidirectional → directed copies both orientations (self-pairs once). Ref copies
create new edge IDs and copy edge-property histories to the corresponding edges,
including the histories of deprecated edge-property attributes (such as
`disguised@old` from an earlier migration); new assertions and retractions on
those attributes stay refused, while corrections are accepted.
Directed → bidirectional drops the source's inverse name (freeing it) and its
reverse cardinality, since a bidirectional attribute has neither; the report
line ends with e.g. `; dropped inverse :child_of and reverse cardinality one`.

Use `kala transact WORLD FILE --dry-run` first: it reports the same schema actions
and counts on a discarded copy. Migration reports include change points copied,
edges created, edge-property facts copied and skipped change points. Each form's
JSON `schema` array is ordered, with `kind` equal to `attribute`, `node_type`,
`rename`, `deprecate`, `cardinality`, `world_setting` or `migrate`. Records include
names/types and `inferred`; migration records add `from`, `to`, `change_points`,
`edges`, `edge_properties`, `skipped`, `dropped_inverse` (the inverse name or
`null`) and `dropped_reverse_cardinality` (`"one"`, `"many"` or `null`). `changed` counts only fact/correction entries, including migration copies.
`declared` counts reported schema actions: declarations (inferred or explicit),
renames, deprecations, cardinality changes, world settings and the migration action.
Both fields are always present at the report and per-op levels. Schema no-ops
count zero; entity creation and edge interning count in neither. Text adds
`, M declared` only when M is nonzero: an inferred assertion prints
`1 changed, 1 declared`, a pure declaration `0 changed, 1 declared`, and a
fact-only write still prints `1 changed`. The write REPL uses the same reports.

This release writes **log format 5**. Format-4 and older worlds are refused with
an older-Kala message and must be recreated from their source fixtures/scripts.
There is no old-file migration before 1.0. `kala info` reports the format version;
`kala log` includes schema revisions and world settings in operation order.
In `kala log --json` a schema entry is `{"entry": N, "kind": "schema", "change":
{...}}`, where `change` uses the write report's field names: `kind` is
`attribute`, `node_type`, `world_setting` or `migrate`, with `name`, `type` and
`inferred`. Attribute and migrate records carry the full definition the entry
logs (`id` for attributes, `attribute_kind` = `state`/`event`, `cardinality`,
`direction`, `inverse`, `reverse_cardinality`, `indexed`, `deprecated`); a
rename or deprecation is logged as an `attribute` record with the new name or
`deprecated: true`. Migrate records add `source` (the source attribute's latest
name, e.g. `age@old`), `from`, `to` and `skip_unconvertible`; world settings
have `key` and `value`. Text `kala log` prints the same with names, e.g.
`schema attribute :age i64 state one indexed (inferred)`.

Preparation of a query with `:canon ?parameter` happens before parameter values
are bound, so static schema-dependent typing and access planning use the latest
schema. Execution rechecks the definition at the bound canon. For an attribute
whose type or indexing changed while it was still empty, use a literal `:canon N`
to prepare against that historical definition. Attributes with stored facts
cannot change these fields in place, so their historical data is unaffected.
Declarations and migrations take `:indexed` or `:unindexed`; converting an
unindexed attribute back to indexed is `[:migrate :bio :string :indexed]`.

## Creating, backing up, and exporting worlds

`kala create WORLD [--schema FILE]` creates an empty world without a YAML fixture.
Without a schema file it has no node types or attributes and infers them on write.
A schema file contains `[:node-type :CHARACTER]`,
`[:attribute :name :string :state :one :indexed]`, and world-setting forms such as
`[:world-setting :schema :strict]`. An optional `[:genesis ...]` wraps them.
These declarations become tx#0; node types precede attributes, then settings.
Existing destinations, including dangling symlinks, are refused (exit 2).
`[:create :CHARACTER]` allocates an entity without a handle.

```sh
kala create new.kala
kala create strict.kala --schema schema.edn
kala dump world.kala -o world.dump.edn
kala transact --create restored.kala world.dump.edn
kala doctor restored.kala
```

`kala dump WORLD [-o FILE] [--force]` writes a deterministic, replayable script.
Without `-o` it writes to stdout; an existing output requires `--force`.
The source is opened read-only without a writer lock. A torn or damaged tail is
omitted with a stderr warning; only committed transactions are dumped.
The header records Kala's version, source log format, latest tx, entry count and
restore command. The first form is `[:genesis ...]`; each subsequent vector is
one original transaction, preceded by `;; tx#N`. Entry IDs, transaction numbers,
corrections, inferred declarations and migrations survive replay. Within the
same log format, the restored committed log is byte-identical to its source.
Dump before upgrading when moving data across a pre-1.0 format change.

`kala transact --create NEW.kala DUMP` requires a leading genesis and refuses an
existing target (exit 2). It validates every operation through the native store,
stages the file privately on the destination filesystem, then publishes it
atomically without overwriting anything. Failure leaves no destination. The
summary is one line: `replayed N transactions, M entries into NEW.kala`; N excludes
genesis and M includes all committed entries, including commits. `--json` returns
one object with `command`, `create`, `transactions`, `entries`, and `world`.
A failed restore of an ordinary world names the failing transaction form and its
reason. An accounts database dump (and any input that fails before its genesis is
known) gets diagnostics without content, because it may contain account hashes.
A restore refuses `_KALA_` names unless the dump is Kala's accounts database.
Restored worlds get the default file mode, like `kala load`; a restored accounts
database is always private (0600).
`--create` conflicts with `--dry-run` and `--acknowledge-recovery`.
Normal `transact` and the REPL reject genesis with a `--create` hint.

Create-mode input has no byte, form or operation cap; memory use scales with the
input and reconstructed world. The 32-level nesting limit and native 16 MiB
per-entry limit remain. Normal `transact` retains its 64 KiB input, 256 operations
per transaction and 256 forms per file limits. Regular input files or explicit
`-` for stdin are supported; device files and FIFOs are refused.

Dumps spell all declaration options explicitly. `:reverse :none` on an attribute
declaration removes its reverse cardinality label. `:inferred` (an option of
a node-type or attribute declaration) preserves inference origin, and is checked
against native inference rules (including strict mode and default definitions).
Entities are named by handle where they have one, else by ID; edges by `edge#N`.
`[:genesis ...]` holds operation vectors only. Names outside the keyword alphabet
are quoted in name positions;
`[:id "A type#1"]` references an ID with such a type name. String escapes include
`\"`, `\\`, `\n`, `\r`, `\t`, `\uXXXX`, and `\UXXXXXXXX` for non-BMP characters.
`[:f64 :inf]` and `[:f64 :-inf]` preserve native infinities; finite floats use
round-trip decimal notation, and integers retain their full signed 64-bit range.

### A whole server directory, including accounts

```sh
kala dump --all worlds -o private-backup
mkdir restored-worlds
kala transact --create restored-worlds/system.kala private-backup/system.dump.edn
kala transact --create restored-worlds/varrow.kala private-backup/varrow.dump.edn
kala user list restored-worlds
kala serve restored-worlds
```

`--all` requires `-o OUTDIR`, which must be absent or empty; `--force` is refused.
It uses the server's discovery rules: regular `<name>.kala` files, lowercase
ASCII letters, digits, `_` or `-`, with a nonempty name. Symlinks are skipped.
Each becomes `<name>.dump.edn`, including `system.dump.edn`. OUTDIR is private
(mode 0700 on Unix), and files are created mode 0600 before writing. Output
symlinks and nonempty directories are refused. Each completed world gets one
summary line; a failure can leave earlier completed dump files in OUTDIR.
On Linux, writes are anchored to the opened OUTDIR descriptor. On other platforms,
keep its parent directory trusted: a concurrent pathname replacement is not
prevented there. On Unix new files use private permissions; elsewhere files
inherit OUTDIR's access control, so make OUTDIR private first.
Publication never clobbers: a new file is hard-linked into place, or on file
systems without hard links (FAT, exFAT, some network mounts) created exclusively
and copied (except an accounts database restore, which needs hard links so it
is never visible half-written). Each world is a committed snapshot at its own open time, not a cross-world
transactional snapshot.

The accounts dump starts with a password-hash warning. Protect it like the
server directory: it includes current **and historical** password hashes.
Single-world `dump` and JSON `export` refuse any world with the system marker,
even under another filename. Only create-mode restore may restore an accounts
database, always to a new private file (0600), with a stderr warning. It validates
accounts before publication. Normal `transact`, `repl --write` and `load --force`
continue to refuse the system world.

`kala user grant DIR USER@HOST backup DB` (or `'*'`) stores a backup grant, and
`kala user list` shows it. `admin` implies backup. Like `read`, `backup` never
covers the accounts database (`system`): neither `backup:*` nor `backup:system`
reaches its password hashes; only `admin` does. Backup alone grants no Bolt
read access. There is no server-side dump yet: local backups are authorized by
filesystem access; the privilege is stored for the future server operation.

### JSON snapshots

```sh
kala export world.kala --at -208 --canon tx#10 -o snapshot.json
```

`kala export WORLD [--at T] [--canon tx#N] [-o FILE]` writes one JSON document to
stdout or a new file. It is read-only and takes no writer lock. Defaults are the
latest canon and its latest tick. The document has:

- `kala_version`, `format_version`, `tick`, `canon`.
- `schema`: `node_types`, `attributes`, `settings`, with the same definitions and
  fields as `kala schema --json`. Definitions are pinned to the requested canon;
  names follow the latest name table, as other Kala reads do.
- `entities`: all entities defined by that canon, ordered by ID, with `id`,
  `node_type`, `handle` (or null), and an `attributes` object containing values
  holding at T. Cardinality-many attributes use arrays; cardinality-one uses a
  single typed value unless several values overlap, in which case it uses an
  array. Attributes with no held values are omitted.
- `edges`: only edges asserted at T, ordered by ID, with `id`, `source`,
  `attribute`, `target`, and `properties` holding at T. Source and target are
  entity objects (`id`, `handle`, `name`, `label`). Edge properties follow the
  same scalar/array rule as entity attributes.

Values use `kala query --json`'s `{type,value}` representation: bool, i64, f64,
string or ref. A ref's value is an entity object. Infinite floats are the strings
`"inf"` / `"-inf"`. Ordering is deterministic. JSON snapshots are for external
tools, not for restoring history; this slice provides no JSON import.

## Cypher reads

`kala query WORLD --lang cypher QUERY` and `kala explain WORLD --lang cypher QUERY`
use the [Cypher read subset](../kala-cypher/README.md). `--lang datalog` is the
default, preserving scripts and the Datalog-only REPL. `-f`, `--param NAME=VALUE`
and `query --json` work with either language. Parameters are typed by usage;
unconstrained scalar parameters infer bool/integer/float, otherwise string.
Cypher also accepts JSON lists and `null` in `--param`; quote a JSON string to
distinguish the string `"null"` from NULL. List input order is preserved.
JSON keeps the existing document/column/row shape; NULL cells are
`{"type":"null","value":null}` and lists are `{"type":"list","value":[...]}`
containing typed cells. Output preserves ORDER BY and duplicate Cypher rows.

```sh
kala query varrow.kala --lang cypher 'MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name LIMIT 3'
kala explain varrow.kala --lang cypher "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -210 RETURN DISTINCT a"
```

Cypher list parameters also feed UNWIND, preserving order and duplicate values:

```sh
kala query varrow.kala --lang cypher \
  'UNWIND $names AS n MATCH (c:CHARACTER {name: n}) RETURN c.kala_handle ORDER BY c.kala_handle' \
  --param 'names=["Greg","Hild","Nobody"]'
```

The result contains greg and hild. List elements are validated against inferred
attribute types before execution; nested lists have a depth limit of 32. The
query/explain commands accept OPTIONAL MATCH, WITH, aggregates and bounded paths.
`db.labels()`, `db.relationshipTypes()` and `db.propertyKeys()` work locally;
server components and database listings report "only over Bolt".
