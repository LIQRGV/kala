# kala-core

Kala is a temporal graph database for fictional story and game worlds: every
fact is `(entity, attribute, value, tick, tx, op)`, so you can ask what an
entity looked like, and who it was connected to, at any moment of the story.
`kala-core` is the append-only log, the indexes and the native Rust API on
top of them (milestone 0); query languages and Bolt come later as adapters.

Two time axes: the **tick** is story time (a signed `i64`, negative ticks are
normal), the **tx** is author time (the position in the log). Truth at a tick
follows tick order, never tx order, and nothing is overwritten: a state ends
with an explicit `retract`, a retcon is a `correct`.

## Edwin's allies, by hand

```rust
use kala_core::{
    AttrDef, Cardinality, Correction, Direction, DoctorOptions, EntityId, Kind, Op, Schema,
    Tick, Value, ValueType, World, doctor,
};

let schema = Schema::new()
    .node_type("CHARACTER")
    .node_type("PLACE")
    .attribute(AttrDef::new("name", ValueType::Str, Kind::State, Cardinality::One))
    .attribute(
        AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
    )
    .attribute(AttrDef::new("located_in", ValueType::Ref, Kind::State, Cardinality::One));
let mut world = World::in_memory(schema)?;

// One transaction: the cast and their names. Kala assigns the IDs
// (CHARACTER#1, #2, #3); handles are for scripting.
let mut txn = world.begin();
let edwin = txn.create_entity("CHARACTER", Some("edwin"))?;
let mara = txn.create_entity("CHARACTER", Some("mara"))?;
let greg = txn.create_entity("CHARACTER", Some("greg"))?;
let varrow = txn.create_entity("PLACE", Some("varrow"))?;
let ashford = txn.create_entity("PLACE", Some("ashford"))?;
for (who, name) in [(edwin, "Edwin"), (mara, "Mara"), (greg, "Greg")] {
    txn.assert(who, "name", Value::Str(name.into()), Some(Tick(-300)))?;
}
txn.commit()?;

// Single writes auto-commit. Each reports what it changed and the tick it used.
let alliance = world.assert(edwin, "ally_of", Value::Ref(mara), Some(Tick(-208)))?;
assert_eq!((alliance.changed, alliance.tick), (1, Tick(-208)));
// ally_of is bidirectional: Mara's side is the same edge, so this ends it.
let split = world.retract(mara, "ally_of", Value::Ref(edwin), Some(Tick(-190)))?;
assert_eq!(split.changed, 1);
// A prequel, written later but earlier in the story.
world.assert(greg, "ally_of", Value::Ref(edwin), Some(Tick(-250)))?;

// "Edwin's allies at tick T".
let allies = |w: &World, t: i64| -> Vec<EntityId> {
    w.neighbors(edwin, "ally_of", Direction::Both, Tick(t))
        .unwrap()
        .into_iter()
        .map(|(ally, _edge)| ally)
        .collect()
};
assert_eq!(allies(&world, -260), vec![]);
assert_eq!(allies(&world, -230), vec![greg]);
assert_eq!(allies(&world, -200), vec![mara, greg]);
assert_eq!(allies(&world, -180), vec![greg]);

// Cardinality `one` is a label: an overlap is a warning, never a refusal.
world.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(-300)))?;
let moved = world.assert(greg, "located_in", Value::Ref(ashford), Some(Tick(-200)))?;
assert_eq!(moved.warnings.introduced.len(), 1); // in Varrow and Ashford from -200
let left = world.retract(greg, "located_in", Value::Ref(varrow), Some(Tick(-220)))?;
assert_eq!(left.warnings.resolved.len(), 1);
assert_eq!(world.as_of(greg, "located_in", Tick(-210))?, vec![]); // a gap is fine
assert_eq!(world.as_of(greg, "located_in", Tick(-200))?, vec![Value::Ref(ashford)]);

// Case-insensitive lookup by value.
assert_eq!(world.lookup_ci("name", "EDWIN", Tick(-200))?, vec![edwin]);

// History: change points in tick order.
let changes: Vec<(Tick, Op, Value)> = world
    .history(edwin, "ally_of")?
    .into_iter()
    .map(|c| (c.tick, c.op, c.value))
    .collect();
assert_eq!(
    changes,
    vec![
        (Tick(-250), Op::Assert, Value::Ref(greg)),
        (Tick(-208), Op::Assert, Value::Ref(mara)),
        (Tick(-190), Op::Retract, Value::Ref(mara)),
    ]
);

// A retcon: the alliance began at -210, not -208. That is a correction of the
// original entry (a DELETED marker at -208 plus the fact at -210).
let before = world.latest_tx();
let retcon = world.correct(
    alliance.entries[0],
    Correction::Replace { value: None, tick: Some(Tick(-210)), op: None, precision: None },
)?;
assert_eq!(retcon.changed, 2);
assert_eq!(allies(&world, -209), vec![mara, greg]);
// The older canon is still readable.
let old = world.at_canon(before);
let old_allies: Vec<EntityId> = old
    .neighbors(edwin, "ally_of", Direction::Both, Tick(-209))?
    .into_iter()
    .map(|(ally, _edge)| ally)
    .collect();
assert_eq!(old_allies, vec![greg]);

// Doctor: store integrity and schema rules. The overlap was resolved.
assert!(doctor(&world, &DoctorOptions::default()).is_clean());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## The same world from a YAML fixture

`kala_fixture::load_fixture` (the `kala-fixture` crate) builds a world from an operation-oriented YAML document by
calling the same native API; see the `kala-fixture` crate docs for the full
format (schema, entities, transactions, labels and corrections).

```rust
use kala_core::{Direction, Tick};
use kala_fixture::load_fixture;

let world = load_fixture(
    r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name,       type: string }
    - { name: ally_of,    type: ref, cardinality: many, direction: bidirectional }
    - { name: located_in, type: ref }
entities:
  CHARACTER: [edwin, mara, greg]
  PLACE: [varrow, ashford]
transactions:
  - tick: -300
    ops:
      - assert: [edwin, name, "Edwin"]
      - assert: [mara, name, "Mara"]
      - assert: [greg, name, "Greg"]
  - ops:
      - assert: [edwin, ally_of, mara]
        tick: -208
        label: edwin-mara
  - ops:
      - retract: [mara, ally_of, edwin]
        tick: -190
  - ops:
      - assert: [greg, ally_of, edwin]   # the prequel
        tick: -250
  - ops:
      - correct: edwin-mara
        replace: { tick: -210 }
"#,
)?;

let edwin = world.entity_by_handle("edwin").unwrap();
let mara = world.entity_by_handle("mara").unwrap();
let greg = world.entity_by_handle("greg").unwrap();
let allies = |t: i64| -> Vec<_> {
    world
        .neighbors(edwin, "ally_of", Direction::Both, Tick(t))
        .unwrap()
        .into_iter()
        .map(|(ally, _edge)| ally)
        .collect()
};
assert_eq!(allies(-260), vec![]);
assert_eq!(allies(-209), vec![mara, greg]);
assert_eq!(allies(-180), vec![greg]);
assert_eq!(world.format_id(greg), "CHARACTER#3");
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Main API

- `Schema`, `AttrDef`: node types and attributes (`ValueType`, `Kind`,
  `Cardinality`; `.bidirectional()`, `.inverse(..)`, `.reverse_one()`,
  `.unindexed()`).
- `World::in_memory`, `World::create`, `World::open`: a world in memory or
  in a log file (reopening replays the log, cuts a torn tail and reports a
  damaged one: see "Recovery" below).
  `World::open_read_only`: the same replay as a snapshot, without the writer
  lock and with no change to the file (`recovery()` reports what a writer
  would cut); every write fails with `Error::ReadOnly`.
- Several handles on one file (`World::catch_up`, `World::set_busy_timeout`,
  `Concurrency`, `Error::Conflict`, `Error::LogDiverged`): see
  "Concurrent writers" below.
- Writes: `World::create_entity`, `World::assert`, `World::retract` (and the
  `_with_precision` forms), `World::correct` with `Correction::{Delete,
  Replace}`; `World::begin` returns a `Txn` grouping writes into one
  transaction (`Txn::set_default_tick`, `Txn::commit`). Results report
  `changed`, the `tick` used, the `entries` written and overlap `warnings`.
- Reads at tick `T`: `as_of`, `as_of_value`, `as_of_detail`, `neighbors`
  (`Direction::Out | In | Both`), `history`, `lookup`, `lookup_ci`,
  `entities_of_type`, `overlaps`; `edge_id` / `edge` for edges,
  `entity_by_handle` / `format_id` for IDs.
- `World::at_canon(tx)`: a `View` with the same reads at an older canon;
  `View::edge` / `View::edge_id` see only edges defined up to that canon.
- `kala_fixture::load_fixture`, `load_fixture_to` (the `kala-fixture` crate):
  build a world from YAML.
- `World::chain_tip` and `log::chain_tip_holds`: the log's transaction
  checksums are chained (format 6: each covers the one before, the first the
  file header), so the last transaction a handle applied (`ChainTip`: its
  byte range, tx and checksum) stands for its whole history. Re-reading that
  one transaction tells whether another Kala writer cut and rewrote that
  history underneath the handle (see "The checksum chain" below for what
  it does not tell). Files of an older format
  are refused (`Error::UnsupportedVersion`): dump with the older binary,
  `kala transact --create` with this one.
- `doctor`, `doctor_log`: the read-only integrity checker (`Report`,
  `Finding`, `Severity`).

## Concurrent writers

One world file may be open in several handles at once, in one process or many:
any number of readers and any number of writers. Writers take turns, one commit
at a time (the SQLite model); nothing below needs a server.

```rust
use kala_core::{
    AttrDef, Cardinality, Concurrency, ConflictKey, Error, Kind, Schema, Tick, Value,
    ValueType, World,
};

let dir = tempfile::tempdir()?;
let path = dir.path().join("world.kala");
let schema = Schema::new()
    .node_type("CHARACTER")
    .node_type("PLACE")
    .attribute(AttrDef::new("located_in", ValueType::Ref, Kind::State, Cardinality::One));
let mut a = World::create(&path, schema)?;
let mut txn = a.begin();
let greg = txn.create_entity("CHARACTER", Some("greg"))?;
let varrow = txn.create_entity("PLACE", Some("varrow"))?;
let ashford = txn.create_entity("PLACE", Some("ashford"))?;
let carnath = txn.create_entity("PLACE", Some("carnath"))?;
txn.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(600)))?;
txn.commit()?;

// A second writable handle on the same file: no lock is held between commits.
let mut b = World::open(&path)?;

// b reads where Greg is and decides to move him to Carnath...
let mut move_b = b.begin();
move_b.note_read(greg, "located_in")?;
move_b.retract(greg, "located_in", Value::Ref(varrow), Some(Tick(610)))?;
move_b.assert(greg, "located_in", Value::Ref(carnath), Some(Tick(610)))?;

// ...while a moves him to Ashford and commits first.
let mut move_a = a.begin();
move_a.retract(greg, "located_in", Value::Ref(varrow), Some(Tick(605)))?;
move_a.assert(greg, "located_in", Value::Ref(ashford), Some(Tick(605)))?;
move_a.commit()?;

// b's commit finds a newer transaction on something it read: a conflict.
// Nothing is written, and b is caught up with a's commit.
let Err(Error::Conflict(conflict)) = move_b.commit() else {
    panic!("the read was invalidated");
};
assert!(matches!(conflict.key, ConflictKey::EntityAttribute { .. }));
assert_eq!(b.as_of(greg, "located_in", Tick(605))?, [Value::Ref(ashford)]);

// Run again on the new state: he leaves Ashford, not Varrow.
let mut again = b.begin();
again.note_read(greg, "located_in")?;
again.retract(greg, "located_in", Value::Ref(ashford), Some(Tick(610)))?;
again.assert(greg, "located_in", Value::Ref(carnath), Some(Tick(610)))?;
again.commit()?;

// A blind write never conflicts, whatever was committed meanwhile.
let mut blind = a.begin();
blind.assert(carnath, "located_in", Value::Ref(varrow), Some(Tick(1)))?;
b.assert(ashford, "located_in", Value::Ref(varrow), Some(Tick(1)))?;
assert_eq!(blind.concurrency(), Concurrency::Optimistic);
blind.commit()?;
assert_eq!(a.catch_up()?, 0, "the commit caught up first");
# Ok::<(), Box<dyn std::error::Error>>(())
```

**The lock is per commit.** A writable world holds the log file's exclusive lock
only while one transaction commits, for the last step of an open, and while
the file is created. Between transactions it holds nothing, so an idle handle
never blocks another writer. Opening reads and replays the log *without* the
lock (so opening a large world doesn't keep others from committing), then takes
it for a moment to settle what it read: transactions committed during the
replay are applied, a torn tail is cut, a damaged one is reported. A commit that finds the lock
held tries again, sleeping briefly, for up to the **busy timeout**
(`World::set_busy_timeout`, `World::open_waiting`; default 5 s, zero = one
attempt), then fails with `Error::WorldLocked` having written nothing. The lock
is advisory (`flock` on Linux): it keeps out other Kala handles, not other
programs, and it dies with its process, so a crashed writer never leaves the
world locked.

**Catch-up.** `World::catch_up` applies the transactions other handles appended
since this one last looked and returns how many. It reads only those
transactions (plus one re-read of the handle's own last transaction), validates
them exactly as `World::open` validates a log and never changes the file.
`World::begin` and the auto-committed writes do it first (without ever
waiting), and every commit does it again under the writer lock. A read-only handle
(`World::open_read_only`) can catch up too.

**Readers wait out a commit in progress.** A transaction is applied by other
handles only once it is durable. A writer holds the exclusive lock across its
write, its fsync and (if the fsync fails) the truncation that takes the
transaction back; a catch-up that finds the file grown reads the new tail
under the file's *shared* lock. If a writer holds its lock at that moment, an
explicit `World::catch_up` waits for it, at most 50 ms (a commit in progress
finishes in that time, so a handle that asks keeps up even behind a writer
committing back to back, such as a dump being restored); the catch-up inside
`World::begin` doesn't wait at all. If the writer still holds its lock, the
catch-up applies nothing and returns
`Ok(0)`: "not yet", and the next one finds what was committed. So `Ok(0)`
means "nothing new that is durable yet", not "the file has nothing new". When
the file hasn't grown, a catch-up takes no lock at all, so readers never slow a
writer down when nothing changed. An incomplete last transaction is likewise
"not yet": ignored, not judged; only a commit, holding the writer lock, cuts
one. `World::open_read_only` settles its snapshot the same way, waiting up to
100 ms for a commit in progress.

**Recovery.** A *torn* last transaction (a writer died mid-append) was never
acknowledged to anyone: `World::open` and every commit cut it, under the lock,
after saving it to a `<file>.torn-N` quarantine file. A *damaged* one (all its
bytes are there, its checksum fails, nothing valid follows) may be a write that
*was* acknowledged, and cutting it frees its entry IDs for reuse, so it is
never cut behind a caller's back, by any handle in any process:

- `World::open` reports it (`recovery()`: cause and length, no quarantine file
  yet), leaves it out of the world and **in the file**, and refuses every
  write with `Error::RecoveryUnacknowledged`
  (`World::open_refusing_damaged` fails the open instead);
- a handle that was already open meets it at its next commit, under the lock:
  that commit fails with `Error::RecoveryRequired`, the file untouched;
- after `World::acknowledge_recovery()`, that handle's next commit (or
  `World::cut_acknowledged_tail()`) quarantines the bytes, cuts them and goes
  ahead. An acknowledgement covers the tail the handle reported, not a later
  one, and a handle that never acknowledged stays refused.

Because the bytes stay until then, no process can write over the lost
transaction's entry IDs without its own caller having been told. An entry ID
noted from the lost transaction must not be used in a `correct` afterwards.

**The file under an append.** Before a commit writes, under the lock, it checks
that the file ends exactly where the handle's log does and that the transaction
continues it (tx, entry IDs, chained checksum). If not (a file system whose
locks do nothing, a program that ignores the lock), nothing is written and the
commit fails with `Error::LogDiverged`.

**A diverged handle.** If the file is no longer the log a handle applied
(shorter than its last transaction, that transaction replaced, the header
changed, or the path now names another file: a recovery or fsync-failure cut
in another process, or the file was replaced), `catch_up` and every write fail
with `Error::LogDiverged`. The handle keeps answering reads from what it had
applied, which the file no longer backs, and never writes again: reopen the
world.

**Committing over newer commits.** A transaction works on the world as it was
when it began. Everything it allocates is provisional until commit: entity
numbers, edge IDs, entry IDs, its tx. If other transactions were committed
meanwhile, the commit re-applies the transaction's operations to the caught-up
world under the store's own rules (duplicate handle, same-tick conflict, an
entry already deleted, a deprecated attribute, strict schema, value types) and
assigns final IDs:

- a created entity gets the next free number of its type; an edge gets a new
  ID, or **reuses** the edge a newer commit already interned for the same
  triple (one `DEFINE` in the log, and the transaction's edge properties land
  on the shared edge);
- `CommitResult::ids` maps every provisional ID that changed to its final one
  (`IdMap::resolve`); `CommitResult::finalized` says what each operation
  stored (a write may have become "0 changed" because a newer commit wrote the
  same fact). A caller that already showed a provisional ID to someone who
  can't be given the map sets `Txn::set_ids_exposed`: a renumbering is then a
  conflict instead;
- **the latest tick stays the one the transaction began with.** A write with
  no tick keeps that tick even if a newer commit moved the world's latest tick,
  and the result carries `CommitResult::latest_tick_moved` (`latest tick moved
  601 → 610 while this transaction ran`). The ordinary rules then decide: the
  same op on the same value at that tick is "0 changed", the opposite op is the
  same-tick conflict error, a new overlap is a warning.

**What conflicts.** Under `Concurrency::Optimistic` (the default) a transaction
records what it *reads*, and its commit fails with `Error::Conflict` if a
transaction committed since it began wrote through one of those keys. The
goal is that committed transactions are serializable in commit order. The
keys, each with what invalidates it:

| The transaction read | Recorded by | Conflicts with a newer |
| --- | --- | --- |
| an entity's attribute ("where is Greg?") | `Txn::values_at`, `Txn::note_read` | entry on that entity and attribute |
| who points at an entity ("who is in Varrow?") | `Txn::note_reverse_read` | entry with that target under the attribute |
| a value lookup ("the entity named Edwin") | `Txn::note_value_read` | entry with that attribute and value |
| a whole attribute | `Txn::note_read_attribute` | entry or schema change on it |
| every entity of a node type | `Txn::note_type_scan` | entity created with that type |
| a handle found free | `Txn::find_or_create_entity`, `Txn::entity_by_handle` | entity created with that handle |
| an entry's revision chain | `Txn::correct` | correction of that entry |
| everything | `Txn::note_read_all` | commit of any kind |

Three examples, as the Datalog write forms make them:

- **`:set`** reads the attribute to compute what to retract. Two writers both
  run `[:set :greg :located_in …]` from the same state: the second to commit
  read "Greg is in Varrow", which the first changed, so it conflicts instead
  of leaving Greg in two places. Run again, it retracts the first writer's
  value.
- **Find-or-create** (`[:create :CHARACTER :lisa :if-missing]`) reads "the
  handle `lisa` is free". If another writer creates `lisa` first, the second
  conflicts; run again, it reports that she exists.
- **A type scan**: a transaction that lists every `CHARACTER` to decide
  something (say, "nobody is left in Varrow, so close the gates") calls
  `note_type_scan("CHARACTER")`; a newer commit that creates a character is a
  phantom for that decision and conflicts. A new `PLACE` doesn't.

A conflict names the key as the transaction gave it and the operation that
read (`Conflict::operation`), never what the other transaction wrote. Nothing
is written and the handle is caught up: begin a new transaction and do the
same reads and writes again.

**Blind writes never conflict.** A transaction that only writes (an assert
with a tick, a create with a new handle) has read nothing, so no newer commit
can invalidate it; it commits over anything, subject only to the store's
rules. Past 1,024 keys (`MAX_READ_KEYS`) a read set is kept per attribute
(and type scans as "any new entity"), so a very large one conflicts more
often, never less.

**Weak mode.** `Txn::set_concurrency(Concurrency::Weak)` records no reads and
checks none: the commit still catches up, re-applies, renumbers and warns
about the latest tick, and the store's rules still refuse a duplicate handle,
a same-tick opposite op or a correction of an entry that a newer transaction
corrected meanwhile (`Error::AlreadyCorrected`, whether that one replaced or
deleted: a correction is never stacked on another writer's); but a write
decided from a stale read goes through (a lost update: both `:set`s commit
and Greg is in two places, with the usual overlap warning). It is an explicit
choice per transaction; the world's `:concurrency` setting accepts only
`optimistic`.

**The reader window.** It is closed for catch-ups and for writable opens: they
never apply a transaction whose writer is still between its write and its
fsync (above). What remains: a `World::open_read_only` made while a writer
holds its lock for longer than 100 ms returns the file as read, and if that
writer's fsync then fails, the snapshot has shown a transaction that was taken
back; its next `catch_up` reports `Error::LogDiverged`. And a transaction whose
writer *died* after its write is in the file for everyone, durable or not, as
after any crash.

**Replacing a world file.** `World::with_writer_lock(f)` runs `f` under the
writer lock for work on the file as a whole, and only if the path still names
the file the handle has open (otherwise `Error::LogDiverged`: another process
replaced it first, and the lock held would be the wrong file's).
`log::with_file_lock(path, timeout, f)` does the same for a file that doesn't
open as a world. `kala load --force` replaces a world through these, so it
never replaces a file in the middle of a commit to it, and two forced loads
can't make a writer commit to an unlinked file.

**The checksum chain.** Each transaction's commit checksum also covers the
previous transaction's (the first chains from the file header), so the last
transaction a handle applied stands for its whole history. Re-reading that
one transaction (`World::chain_tip`, `log::chain_tip_holds`; every catch-up
and commit does it) proves that no Kala writer cut and rewrote the history the
handle applied (a recovery cut or an fsync-failure truncation followed by new
commits, a replaced file), at a cost independent of the log's length, up to
an accidental CRC32 collision of about 2^-32. (The body checksum and the
chained checksum are both CRC32 over the same bytes: one check, not two.) It
does **not** detect a bit that flipped later in an *earlier* transaction: only
the tip is re-read. `World::open` and `kala doctor` recompute every link and
do. Nor is it protection against forgery: someone who can write the file can
recompute the chain; file permissions are the boundary against that.

Transaction commit results count fact and correction entries in `changed` and
reported schema actions in `declared` (no-ops count zero). Deprecated attributes
still accept corrections; new assertions and retractions are refused.

Schema names beginning with `kala_` are reserved, including at genesis. `_KALA_`
names can only originate at genesis (for the system schema). Both prefixes are
case-sensitive. `edge` is reserved only for node types; attributes and inverse
names may use `handle` and `edge`. Datalog reads world handles with `:kala_handle`.
