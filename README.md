# Kala

Kala is a temporal graph database for fictional story and game worlds. It
answers point-in-time questions: what did Edwin own in -208, who were his
allies, who was in Varrow at the same time as him, and what did the canon say
before last week's retcon?

The name means "time" in Sanskrit, Javanese and Indonesian.

> **Status: early.** The storage engine, terminal writes, the REPL and Datalog work.
> Bolt 5 supports Cypher reads, Datalog reads and writes, and explicit transactions through Neo4j drivers.
> Gremlin is not built yet. The
> on-disk format may still change without migration.

## Ideas

- **Everything is a fact** `(entity, attribute, value, tick, tx, op)`. A
  relationship is an attribute whose value is another entity. Every edge has
  its own ID, so it can carry properties.
- **Two timelines.**
  - **tick** is story time: a signed 64-bit integer whose unit your app
    chooses. Negative years are normal.
  - **tx** is author time: when a fact was written into canon.

  Authors write prequels and flashbacks, so the two orders differ. Truth at a
  tick follows tick order, never write order.
- **Append-only.** Nothing is overwritten.
  - A state ends with an explicit `retract`.
  - A retcon is a `correct`, which adds a new log entry.
  - Every older canon stays readable.
- **The store doesn't judge the story.** Cardinality is a label, not a rule.
  If Greg is in two places at once, writes succeed with a warning and
  `kala doctor` reports it. Your draft can be inconsistent while you work it
  out.
- **The log is the source of truth.** It is a single append-only file with
  chained per-transaction checksums (each covers the one before, so a
  rewritten history is detected). Indexes are rebuilt from it, a crash
  mid-write is recovered on open, and backup is copying the file.

## Build

Needs Rust 1.89 or later (edition 2024).

```sh
cargo build --release -p kala-cli     # the `kala` binary
cargo test --workspace
```

## Quick start

`crates/kala-core/tests/fixtures/varrow.yaml` is a small story world: 32
characters, 14 places, alliances, travels, a prequel written late, and a
retcon. Load it and explore:

```sh
kala load crates/kala-core/tests/fixtures/varrow.yaml w.kala

kala neighbors w.kala edwin ally_of --both @-208    # Edwin's allies at tick -208
kala neighbors w.kala varrow located_in --in @-215  # who is in Varrow
kala history w.kala greg located_in                 # every move Greg made
kala find w.kala name edwin -i                      # case-insensitive lookup
kala doctor w.kala                                  # integrity and overlap checks
kala log w.kala                                     # the author timeline, with entry IDs
```

Ticks are written `@T` (or `--at T`). Every read takes `--canon tx#N` to see an
older canon and `--json` for scripts.

### Serve through a Neo4j driver

Serve a directory of worlds; `worlds/varrow.kala` becomes database `varrow`:

```sh
mkdir -p worlds
kala load crates/kala-core/tests/fixtures/varrow.yaml worlds/varrow.kala
kala serve worlds --listen 127.0.0.1:7687
```

Install the Python driver (`python3 -m pip install neo4j==5.28.8`), then:

```python
from neo4j import GraphDatabase
uri = "bolt://127.0.0.1:7687"
query = "// kala:datalog\n[:find ?a :at -210 :where [:edwin :ally_of ?a]]"
with GraphDatabase.driver(uri, auth=None) as driver:
    driver.verify_connectivity()
    with driver.session(database="varrow", fetch_size=5) as session:
        for record in session.run(query):
            ally = record["a"]
            print(ally.element_id, ally.labels, ally["name"])
# CHARACTER#… frozenset({'CHARACTER', ...}) Greg / Tomas
```

The server defaults to account authentication. First startup creates
`worlds/system.kala` with `root@localhost`, a loopback-only `trust` account,
so the example's `auth=None` works locally. Use `kala user set worlds
root@localhost --password-stdin` to switch root to a password, and `kala user`
to manage per-database grants. Password accounts use Argon2id; non-loopback
listeners require `--insecure-allow-plaintext` until TLS is available.
`--auth none` remains a loopback-only development option. Use `--read-only` to refuse writes; `--auth none` grants read access only.
Changes committed by `kala transact` become visible on the next query.
Ctrl-C (SIGINT) or SIGTERM stops the server cleanly. Cypher reads, routing, explicit transactions and Datalog writes are supported.
Cypher write clauses and Neo4j Browser are not supported yet. See the
[Bolt support and limits](crates/kala-bolt/README.md) and the opt-in
[driver E2E tests](e2e/README.md).

### Datalog

Queries use a Datomic-style syntax with time built in. `:at` sets the query's
tick, `:canon` the canon, and `(at T …)` lets one pattern read at its own tick.

```sh
# Edwin's allies at -210
kala query w.kala '[:find ?a :at -210 :where [:edwin :ally_of ?a]]'

# allies at -210 who were no longer allies at -200
kala query w.kala '[:find ?a :at -210 :where [:edwin :ally_of ?a]
                                            (not (at -200 [:edwin :ally_of ?a]))]'

# when did Greg leave Varrow?  (history patterns bind change points)
kala query w.kala '[:find ?t :where [:greg :located_in :varrow ?t :retract]]'

# who was in Varrow at the same time as Edwin, and when  (interval sets)
kala query w.kala '[:find ?c ?both
                    :where (during [:edwin :located_in :varrow] ?ie)
                           (during [?c :located_in :varrow] ?ic)
                           [(!= ?c :edwin)]
                           [(intersect ?ie ?ic) ?both] [(not-empty ?both)]]'

# everyone reachable through alliances (recursive rules)
kala query w.kala '[:find ?b :at 600
                    :rules [[(allied ?a ?b) [?a :ally_of ?b]]
                            [(allied ?a ?b) [?a :ally_of ?x] (allied ?x ?b)]]
                    :where (allied :edwin ?b)]'

# how many characters were in Varrow at -215 (aggregates)
kala query w.kala '[:find (count ?c) :at -215 :where [?c :located_in :varrow]]'
```

`kala explain` prints a query's plan. `kala doctor --checks checks.edn` runs
your own Datalog story checks (for example "dead characters with active
states") as hints. Kala ships no story rules of its own.

### Writing a world

Follow the [story-author guide](GUIDE.md) to write scenes, ask questions and
correct earlier canon. Open `kala repl w.kala --write` for interactive writes,
or run `kala transact w.kala scene.edn` for scripts. New attributes and node types
are inferred on write by default. Declare, rename, deprecate or migrate them with
Datalog schema forms; inspect definitions with `kala schema w.kala`. Write reports
count facts as `changed` and schema actions as `declared`. Deprecated attributes
allow corrections while refusing new assertions and retractions. Query permanent
handles with `:kala_handle` (the Bolt Node property is also `kala_handle`); a user
attribute named `handle` is ordinary data. YAML fixtures
loaded with `kala load` provide the initial schema:

```yaml
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }                          # defaults: state, one, directed
    - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
entities:
  CHARACTER: [greg, edwin]
  PLACE: [varrow]
transactions:
  - tick: -300
    ops:
      - assert: [greg, located_in, varrow]
      - retract: [greg, located_in, varrow]
        tick: -250
        precision: year
        label: greg-leaves
  - ops:                                  # a retcon: he left at -280, not -250
      - correct: greg-leaves
        replace: { tick: -280 }
```

The native Rust API (`kala-core`) can also write directly. See
[`crates/kala-core/README.md`](crates/kala-core/README.md).

## Layout

| Crate | What it is |
|---|---|
| `kala-core` | The log, indexes (`E`, `R`, `V`), `assert`/`retract`/`correct`, `AsOf`/`History`/`Neighbors`, overlap warnings, doctor |
| `kala-fixture` | The YAML fixture loader |
| `kala-query` | A language-neutral query IR and executor, with row, step and memory budgets |
| `kala-datalog` | The Datalog parser, compiler and planner |
| `kala-cypher` | Cypher read parser and compiler to the same IR |
| `kala-lang` | One `FrontEnd` interface over the query languages |
| `kala-system` | The `system` database: accounts, privileges, password hashing, database names |
| `kala-bolt` | Bolt 5 server, transactions, authentication boundary and driver value mapping |
| `kala-cli` | The `kala` command-line tool |

## Roadmap

- [x] Storage engine and native API
- [x] Read-only CLI
- [x] Datalog: joins, rules, recursion, negation, disjunction, history with precision, interval sets, aggregates
- [x] REPL (`kala repl`)
- [x] Writes from the CLI and REPL
- [x] Schema-on-write, schema revisions by canon, copy-forward migrations and `kala schema`
- [x] `kala create`, replayable full-history dumps, private server backups and JSON snapshots
- [x] `kala serve`: read-only Bolt 5 Datalog queries through Neo4j drivers
- [x] Cypher reads: MATCH/WHERE/RETURN, bags/NULL, ORDER BY/SKIP/LIMIT and time travel
- [x] Cypher breadth: OPTIONAL MATCH, WITH, aggregates, UNWIND, bounded paths and Bolt Path values
- [x] Neo4j driver metadata and routing (CALL, SHOW DATABASES, ROUTE; TCP only)
- [x] Shared write applier, Datalog `:set` / `:if-missing`, bounded commit warnings and write-privilege groundwork (slice 15a)
- [x] Shared Bolt transport seam, multiple listeners and Unix sockets (slice 16)
- [x] Several writers on one world: writer lock per commit with a busy timeout, log catch-up, chained checksums (log format 6), optimistic conflict detection with an explicit weak mode, `--concurrency` / `--busy-timeout` and a conflict exit code (slice 15b)
- [x] Datalog writes over Bolt and explicit transactions (slice 15c)
- [ ] Cypher write clauses (slice 15d)
- [ ] Web clients (WebSocket and remaining Browser stubs), then Gremlin when requested
- [x] Accounts, host matching, Argon2id passwords and database privileges
- [ ] TLS for encrypted Bolt connections
- [ ] On-disk indexes with checkpoints
