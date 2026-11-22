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

`load_fixture` builds a world from an operation-oriented YAML document by
calling the same native API; see the `fixture` module docs for the full
format (schema, entities, transactions, labels and corrections).

```rust
use kala_core::{Direction, Tick, load_fixture};

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
  in a log file (reopening replays the log and recovers from a torn tail).
- Writes: `World::create_entity`, `World::assert`, `World::retract` (and the
  `_with_precision` forms), `World::correct` with `Correction::{Delete,
  Replace}`; `World::begin` returns a `Txn` grouping writes into one
  transaction (`Txn::set_default_tick`, `Txn::commit`). Results report
  `changed`, the `tick` used, the `entries` written and overlap `warnings`.
- Reads at tick `T`: `as_of`, `as_of_value`, `as_of_detail`, `neighbors`
  (`Direction::Out | In | Both`), `history`, `lookup`, `lookup_ci`,
  `entities_of_type`, `overlaps`; `edge_id` / `edge` for edges,
  `entity_by_handle` / `format_id` for IDs.
- `World::at_canon(tx)`: a `View` with the same reads at an older canon.
- `load_fixture`, `load_fixture_to`: build a world from YAML.
- `doctor`, `doctor_log`: the read-only integrity checker (`Report`,
  `Finding`, `Severity`).
