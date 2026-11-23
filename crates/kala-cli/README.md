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

`--json` prints one document per successful form, with fields `form`, `dry_run`,
`tx`, `changed`, `entries`, `edges`, `ops` and `warnings`. Each op has `position`,
`kind`, `tick`, `precision`, `changed`, `entries` and `entity` (null unless created).
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
PRIV is `read`, `write` or `admin`, and DB is a database name or `*` (quote it in
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
