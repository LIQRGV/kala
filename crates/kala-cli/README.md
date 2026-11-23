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
