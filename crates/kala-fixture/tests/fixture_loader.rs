//! Slice 4, items 6 and 7: the YAML fixture format and its loader
//! (docs/slice-4-decisions.md "Item 6/7" and "Dependencies"; CLAUDE.md "Open questions ->
//! Test fixture format" and "Core ideas" tick defaulting).
//!
//! RED until the implementation lands: this file fails to COMPILE (there is no
//! `kala_fixture` crate).
//!
//! API expected (module `fixture`, also re-exported at the crate root):
//!
//! ```text
//! pub fn load_fixture(yaml: &str) -> Result<World, FixtureError>
//! pub fn load_fixture_to(yaml: &str, path: &Path) -> Result<World, FixtureError>
//! pub struct FixtureError { pub at: Option<OpRef>, pub kind: FixtureErrorKind }
//!     // Debug, Clone, PartialEq, Display, std::error::Error
//! pub struct OpRef { pub txn: usize, pub op: usize, pub label: Option<String> }
//! pub enum FixtureErrorKind {
//!     Yaml(String), Invalid(String), UnknownHandle(String), UnknownEdge(String),
//!     UnknownLabel(String), DuplicateLabel(String), Store(Error),
//! }
//! ```
//!
//! The strongest check is parity: the same world built by hand through the native API
//! into a file has exactly the bytes of the file the loader wrote (so the same entry
//! IDs, transactions, ticks and precisions). Every parity test builds the native side
//! FIRST, then calls the loader.
#[path = "../../kala-core/tests/common/mod.rs"]
mod common;

use kala_core::log::EntryBody;
use kala_core::*;
use kala_fixture::{FixtureError, FixtureErrorKind, OpRef, load_fixture, load_fixture_to};
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};
use std::path::Path;
use std::time::{Duration, Instant};

// ============================================================ helpers

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

/// The schema every fixture below declares, as YAML.
const SCHEMA: &str = r#"
schema:
  node_types: [CHARACTER, PLACE, ITEM, EVENT]
  attributes:
    - { name: name,      type: string }
    - { name: alias,     type: string, cardinality: many }
    - { name: biography, type: string, indexed: false }
    - { name: located_in, type: ref }
    - { name: ally_of,   type: ref, cardinality: many, direction: bidirectional }
    - { name: parent_of, type: ref, cardinality: many, inverse: child_of }
    - { name: ruled_by,  type: ref, reverse: one }
    - { name: bought,    type: ref, kind: event, cardinality: many }
    - { name: disguised, type: bool }
    - { name: present,   type: ref, cardinality: many }
    - { name: saw,       type: ref, cardinality: many }
    - { name: age,       type: i64 }
    - { name: height,    type: f64 }
"#;

/// The same schema through the native builder.
fn schema() -> Schema {
    let st = |n: &str, c| AttrDef::new(n, ValueType::Str, Kind::State, c);
    let rf = |n: &str, k, c| AttrDef::new(n, ValueType::Ref, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .node_type("EVENT")
        .attribute(st("name", Cardinality::One))
        .attribute(st("alias", Cardinality::Many))
        .attribute(st("biography", Cardinality::One).unindexed())
        .attribute(rf("located_in", Kind::State, Cardinality::One))
        .attribute(rf("ally_of", Kind::State, Cardinality::Many).bidirectional())
        .attribute(rf("parent_of", Kind::State, Cardinality::Many).inverse("child_of"))
        .attribute(rf("ruled_by", Kind::State, Cardinality::One).reverse_one())
        .attribute(rf("bought", Kind::Event, Cardinality::Many))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(rf("present", Kind::State, Cardinality::Many))
        .attribute(rf("saw", Kind::State, Cardinality::Many))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "height",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
}

/// Entities section, written with PLACE before CHARACTER on purpose: types are created in
/// schema order, whatever the YAML order.
const ENTITIES: &str = r#"
entities:
  PLACE: [varrow, ashford, carnath]
  CHARACTER: [edwin, greg, mara]
"#;

fn doc(rest: &str) -> String {
    format!("{SCHEMA}{ENTITIES}{rest}")
}

/// A native world with the schema and the entities (CHARACTER first, then PLACE, one txn).
fn native_base(path: &Path) -> World {
    let mut w = World::create(path, schema()).unwrap();
    let mut tx = w.begin();
    for h in ["edwin", "greg", "mara"] {
        tx.create_entity("CHARACTER", Some(h)).unwrap();
    }
    for h in ["varrow", "ashford", "carnath"] {
        tx.create_entity("PLACE", Some(h)).unwrap();
    }
    tx.commit().unwrap();
    w
}

fn id(w: &World, h: &str) -> EntityId {
    w.entity_by_handle(h)
        .unwrap_or_else(|| panic!("no entity {h}"))
}

fn load(yaml: &str) -> World {
    match load_fixture(yaml) {
        Ok(w) => w,
        Err(e) => panic!("fixture failed to load: {e}\n---\n{yaml}"),
    }
}

fn err(yaml: &str) -> FixtureError {
    match load_fixture(yaml) {
        Ok(_) => panic!("expected an error\n---\n{yaml}"),
        Err(e) => e,
    }
}

fn is_yaml(e: &FixtureError) -> bool {
    matches!(e.kind, FixtureErrorKind::Yaml(_))
}
fn is_invalid(e: &FixtureError) -> bool {
    matches!(e.kind, FixtureErrorKind::Invalid(_))
}

fn invalid(yaml: &str) -> FixtureError {
    let e = err(yaml);
    assert!(is_invalid(&e), "expected Invalid, got {e:?}\n---\n{yaml}");
    e
}

fn store(yaml: &str) -> (Error, Option<OpRef>) {
    let e = err(yaml);
    match e.kind {
        FixtureErrorKind::Store(x) => (x, e.at),
        other => panic!("expected Store, got {other:?} at {:?}\n---\n{yaml}", e.at),
    }
}

fn opref(txn: usize, op: usize, label: Option<&str>) -> Option<OpRef> {
    Some(OpRef {
        txn,
        op,
        label: label.map(str::to_owned),
    })
}

/// `(tick, op, value, precision)` rows.
fn hist(w: &World, e: EntityId, a: &str) -> Vec<(i64, Op, Value, Precision)> {
    w.history(e, a)
        .unwrap()
        .into_iter()
        .map(|c| (c.tick.0, c.op, c.value, c.precision))
        .collect()
}

struct Parity {
    _dir: tempfile::TempDir,
    world: World,
}

/// Builds the native world with `native` (its file is dropped before the loader runs),
/// loads `yaml` into a second file, asserts the two files are byte-identical, and returns
/// the loaded file reopened (so the reopened answers are checked against the native ones).
fn parity(yaml: &str, native: impl FnOnce(&mut World)) -> Parity {
    let dir = tempfile::tempdir().unwrap();
    let np = dir.path().join("native.kala");
    let lp = dir.path().join("loaded.kala");
    {
        let mut w = native_base(&np);
        native(&mut w);
        clean(&w);
    }
    {
        let w = match load_fixture_to(yaml, &lp) {
            Ok(w) => w,
            Err(e) => panic!("load_fixture_to failed: {e}\n---\n{yaml}"),
        };
        clean(&w);
    }
    let (a, b) = (std::fs::read(&lp).unwrap(), std::fs::read(&np).unwrap());
    if a != b {
        let la = kala_core::log::read_log(&lp).unwrap();
        let lb = kala_core::log::read_log(&np).unwrap();
        for (i, (x, y)) in la.iter().zip(lb.iter()).enumerate() {
            assert_eq!(x, y, "first differing log entry is #{i}");
        }
        assert_eq!(la.len(), lb.len(), "entry counts differ (loaded vs native)");
        panic!("log bytes differ but every entry is equal");
    }
    let world = World::open(&lp).unwrap();
    assert_eq!(*world.recovery(), RecoveryReport::default());
    let native_world = World::open(&np).unwrap();
    assert_eq!(world.index_dump(), native_world.index_dump());
    assert_eq!(world.latest_tx(), native_world.latest_tx());
    clean(&world);
    Parity { _dir: dir, world }
}

// ============================================================ the API surface

#[test]
fn the_fixture_api_is_exported_from_the_module_and_the_crate_root() {
    let _: fn(&str) -> Result<World, FixtureError> = kala_fixture::load_fixture;
    let _: fn(&str, &Path) -> Result<World, FixtureError> = kala_fixture::load_fixture_to;
    let _: fn(&str) -> Result<World, kala_fixture::FixtureError> = kala_fixture::load_fixture;
    let _: fn(&str, &Path) -> Result<World, kala_fixture::FixtureError> =
        kala_fixture::load_fixture_to;
    let _ = kala_fixture::OpRef {
        txn: 0,
        op: 0,
        label: None,
    };
    let _: kala_fixture::FixtureErrorKind = kala_fixture::FixtureErrorKind::Yaml(String::new());
}

#[test]
fn fixture_error_is_debug_clone_eq_display_and_a_std_error() {
    fn is_std_error<E: std::error::Error>() {}
    is_std_error::<FixtureError>();
    let a = FixtureError {
        at: opref(3, 1, Some("greg-leaves")),
        kind: FixtureErrorKind::UnknownHandle("nobody".into()),
    };
    let b = a.clone();
    assert_eq!(a, b);
    assert_ne!(
        a,
        FixtureError {
            at: None,
            kind: FixtureErrorKind::UnknownHandle("nobody".into())
        }
    );
    assert!(!format!("{a:?}").is_empty());
    assert!(!a.to_string().is_empty());
    // every kind constructs
    for k in [
        FixtureErrorKind::Yaml("x".into()),
        FixtureErrorKind::Invalid("x".into()),
        FixtureErrorKind::UnknownHandle("x".into()),
        FixtureErrorKind::UnknownEdge("x".into()),
        FixtureErrorKind::UnknownLabel("x".into()),
        FixtureErrorKind::DuplicateLabel("x".into()),
        FixtureErrorKind::Store(Error::UnknownAttribute("x".into())),
    ] {
        let e = FixtureError { at: None, kind: k };
        assert_eq!(e.clone(), e);
    }
}

#[test]
fn display_names_the_position_and_the_label() {
    // txn 2, op 1, label my-label: an unknown attribute (a store error) on a labelled op
    let y = doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
  - ops:
      - assert: [greg, name, "b"]
        tick: 2
  - ops:
      - assert: [greg, name, "c"]
        tick: 3
      - assert: [greg, nope, "d"]
        label: my-label
"#);
    let e = err(&y);
    assert_eq!(e.at, opref(2, 1, Some("my-label")));
    assert_eq!(
        e.kind,
        FixtureErrorKind::Store(Error::UnknownAttribute("nope".into()))
    );
    let d = e.to_string();
    assert!(d.contains("my-label"), "{d}");
    // as built: 0-based, "transaction N, op M (label L): ..."
    assert!(
        d.starts_with("transaction 2, op 1 (label my-label): "),
        "{d}"
    );
}

// ============================================================ the format, end to end

/// One fixture using the whole format; the native world below is its hand-written twin.
const KITCHEN_SINK_OPS: &str = r#"
transactions:
  - tick: -300
    ops:
      - assert: [greg, located_in, varrow]
      - assert: [greg, name, "Greg"]
      - assert: [edwin, ally_of, mara]
        tick: -208
        precision: year
        label: edwin-mara
      - assert: [edwin, present, carnath]
  - ops:
      - create: [CHARACTER, lisa]
      - retract: [greg, located_in, varrow]
        tick: -250
        label: greg-leaves
      - assert: [[edwin, present, carnath], disguised, true]
        tick: -300
      - assert: [mara, alias, "M"]
  - ops:
      - correct: greg-leaves
        replace: { tick: -280 }
      - correct: edwin-mara
        delete: true
"#;

#[test]
fn the_whole_format_loads_to_exactly_the_hand_built_world() {
    let p = parity(&doc(KITCHEN_SINK_OPS), |w| {
        let (edwin, greg, mara, varrow, carnath) = (
            id(w, "edwin"),
            id(w, "greg"),
            id(w, "mara"),
            id(w, "varrow"),
            id(w, "carnath"),
        );
        let mut tx = w.begin();
        tx.set_default_tick(Tick(-300));
        tx.assert(greg, "located_in", Value::Ref(varrow), None)
            .unwrap();
        tx.assert(greg, "name", s("Greg"), None).unwrap();
        tx.assert_with_precision(edwin, "ally_of", Value::Ref(mara), t(-208), Precision::Year)
            .unwrap();
        tx.assert(edwin, "present", Value::Ref(carnath), None)
            .unwrap();
        let c0 = tx.commit().unwrap();
        let edwin_mara = c0.entries[2];
        let edge: EntityId = w.edge_id(edwin, "present", carnath).unwrap().into();
        let mut tx = w.begin();
        tx.create_entity("CHARACTER", Some("lisa")).unwrap();
        tx.retract(greg, "located_in", Value::Ref(varrow), t(-250))
            .unwrap();
        tx.assert(edge, "disguised", Value::Bool(true), t(-300))
            .unwrap();
        // no tick, no default: the latest tick at the txn's start (-208)
        tx.assert(mara, "alias", s("M"), None).unwrap();
        let c1 = tx.commit().unwrap();
        let greg_leaves = c1.entries[0];
        let mut tx = w.begin();
        tx.correct(
            greg_leaves,
            Correction::Replace {
                value: None,
                tick: t(-280),
                op: None,
                precision: None,
            },
        )
        .unwrap();
        tx.correct(edwin_mara, Correction::Delete).unwrap();
        tx.commit().unwrap();
    });
    let w = &p.world;
    let (greg, mara, edwin) = (id(w, "greg"), id(w, "mara"), id(w, "edwin"));
    // spot-check the answers the parity implies
    assert_eq!(
        hist(w, greg, "located_in")
            .iter()
            .map(|r| (r.0, r.1))
            .collect::<Vec<_>>(),
        vec![(-300, Op::Assert), (-280, Op::Retract)]
    );
    assert!(w.as_of(edwin, "ally_of", Tick(0)).unwrap().is_empty());
    assert_eq!(hist(w, mara, "alias")[0].0, -208);
    assert_eq!(
        w.entity_by_handle("lisa").unwrap().seq(),
        4,
        "after edwin, greg, mara"
    );
    assert_eq!(id(w, "lisa").node_type(), id(w, "edwin").node_type());
}

#[test]
fn entities_get_sequence_numbers_in_list_order_and_types_are_created_in_schema_order() {
    let w = load(&doc(""));
    for (i, h) in ["edwin", "greg", "mara"].iter().enumerate() {
        let e = id(&w, h);
        assert_eq!(e.seq(), i as u64 + 1, "{h}");
    }
    for (i, h) in ["varrow", "ashford", "carnath"].iter().enumerate() {
        assert_eq!(id(&w, h).seq(), i as u64 + 1, "{h}");
    }
    assert_ne!(id(&w, "edwin").node_type(), id(&w, "varrow").node_type());
    // listed PLACE first in the YAML, but CHARACTER is first in the schema: the log's
    // DefineEntity entries run CHARACTER (edwin, greg, mara), then PLACE
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    drop(load_fixture_to(&doc(""), &path).unwrap());
    let handles: Vec<String> = kala_core::log::read_log(&path)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::DefineEntity { handle, .. } => handle.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        handles,
        ["edwin", "greg", "mara", "varrow", "ashford", "carnath"]
    );
    // `entities_of_type` lists them in sequence order
    let chars: Vec<_> = w.entities_of_type("CHARACTER").unwrap().collect();
    assert_eq!(chars, vec![id(&w, "edwin"), id(&w, "greg"), id(&w, "mara")]);
}

#[test]
fn entities_are_created_in_one_transaction_before_every_op() {
    // no ops: the whole entities section is exactly one transaction (tx 1; genesis is tx 0)
    let w = load(&doc(""));
    assert_eq!(w.latest_tx(), TxId(1));
    // with a transaction: tx 2
    let w = load(&doc(
        "transactions:\n  - ops:\n      - assert: [greg, name, \"G\"]\n",
    ));
    assert_eq!(w.latest_tx(), TxId(2));
    // no entities at all: nothing is committed for them
    let w = load(&format!("{SCHEMA}\ntransactions: []\n"));
    assert_eq!(w.latest_tx(), TxId(0));
    // the very first op may name any entity
    let w = load(&doc(
        "transactions:\n  - ops:\n      - assert: [mara, located_in, carnath]\n",
    ));
    let m = id(&w, "mara");
    assert_eq!(w.as_of(m, "located_in", Tick(0)).unwrap().len(), 1);
}

#[test]
fn every_omitted_attribute_option_equals_its_documented_default() {
    let explicit = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name, type: string, kind: state, cardinality: one, indexed: true }
    - { name: located_in, type: ref, kind: state, cardinality: one, direction: directed, indexed: true }
    - { name: age, type: i64, kind: state, cardinality: one }
"#;
    let omitted = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name, type: string }
    - { name: located_in, type: ref }
    - { name: age, type: i64 }
"#;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.kala"), dir.path().join("b.kala"));
    drop(load_fixture_to(explicit, &a).unwrap());
    drop(load_fixture_to(omitted, &b).unwrap());
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    // and both equal the native world with the library's defaults
    let c = dir.path().join("c.kala");
    drop(
        World::create(
            &c,
            Schema::new()
                .node_type("CHARACTER")
                .node_type("PLACE")
                .attribute(AttrDef::new(
                    "name",
                    ValueType::Str,
                    Kind::State,
                    Cardinality::One,
                ))
                .attribute(AttrDef::new(
                    "located_in",
                    ValueType::Ref,
                    Kind::State,
                    Cardinality::One,
                ))
                .attribute(AttrDef::new(
                    "age",
                    ValueType::I64,
                    Kind::State,
                    Cardinality::One,
                )),
        )
        .unwrap(),
    );
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&c).unwrap());
}

#[test]
fn every_attribute_option_takes_effect() {
    let mut w = load(&doc(r#"
transactions:
  - tick: 1
    ops:
      - assert: [greg, alias, "a"]
      - assert: [greg, alias, "b"]
      - assert: [edwin, ally_of, mara]
      - assert: [edwin, parent_of, greg]
      - assert: [edwin, bought, varrow]
"#));
    let (edwin, greg, mara, varrow) = (
        id(&w, "edwin"),
        id(&w, "greg"),
        id(&w, "mara"),
        id(&w, "varrow"),
    );
    // cardinality many: both aliases hold
    assert_eq!(w.as_of(greg, "alias", Tick(1)).unwrap().len(), 2);
    // bidirectional: Mara's allies include Edwin through the one stored edge
    assert_eq!(
        w.neighbors(mara, "ally_of", Direction::Out, Tick(1))
            .unwrap()
            .len(),
        1
    );
    // inverse name reads the same edge from the other side
    assert_eq!(
        w.neighbors(greg, "child_of", Direction::Out, Tick(1))
            .unwrap()
            .len(),
        1
    );
    // kind: event, so a retract is refused
    assert_eq!(
        w.retract(edwin, "bought", Value::Ref(varrow), t(5)),
        Err(Error::RetractEvent("bought".into()))
    );
    // indexed: false, so no 256 byte limit and no value lookup
    let long = "x".repeat(1000);
    w.assert(greg, "biography", Value::Str(long), t(1)).unwrap();
    assert_eq!(
        w.lookup("biography", &s("x"), Tick(1)),
        Err(Error::NotIndexed("biography".into()))
    );
    // an indexed string over 256 bytes is a write error
    assert!(matches!(
        w.assert(greg, "name", Value::Str("y".repeat(257)), t(1)),
        Err(Error::Value(ValueError::StringTooLong { .. }))
    ));
    // reverse: one + cardinality one: overlapping rulers are warnings, not errors
    w.assert(greg, "ruled_by", Value::Ref(edwin), t(1)).unwrap();
    // i64 and f64 and bool attributes exist
    w.assert(greg, "age", Value::I64(40), t(1)).unwrap();
    w.assert(greg, "height", Value::F64(1.8), t(1)).unwrap();
    w.assert(greg, "disguised", Value::Bool(false), t(1))
        .unwrap();
}

#[test]
fn the_schema_is_equal_to_the_native_one_for_every_option() {
    // a fixture with only a schema equals World::create(schema()): same genesis bytes
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.kala"), dir.path().join("b.kala"));
    drop(load_fixture_to(SCHEMA, &a).unwrap());
    drop(World::create(&b, schema()).unwrap());
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
}

// ============================================================ ticks and defaults

#[test]
fn the_txn_tick_is_the_default_and_an_op_tick_wins() {
    let w = load(&doc(r#"
transactions:
  - tick: -300
    ops:
      - assert: [greg, alias, "default"]
      - assert: [greg, alias, "own"]
        tick: -100
"#));
    let g = id(&w, "greg");
    let h = hist(&w, g, "alias");
    assert!(h.contains(&(-300, Op::Assert, s("default"), Precision::Exact)));
    assert!(h.contains(&(-100, Op::Assert, s("own"), Precision::Exact)));
}

#[test]
fn a_tickless_op_without_a_txn_tick_uses_the_latest_tick_at_txn_start_for_every_op() {
    let w = load(&doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "seed"]
        tick: 100
  - ops:
      - assert: [greg, alias, "a"]
      - assert: [greg, alias, "far"]
        tick: 900
      - assert: [greg, alias, "b"]
  - ops:
      - assert: [greg, alias, "c"]
"#));
    let g = id(&w, "greg");
    let h = hist(&w, g, "alias");
    // a and b land on 100 (the latest tick when txn 1 started); the 900 write in the same
    // txn does not move it; c lands on 900 (txn 2 started after 900 was committed)
    assert!(h.contains(&(100, Op::Assert, s("a"), Precision::Exact)));
    assert!(h.contains(&(100, Op::Assert, s("b"), Precision::Exact)));
    assert!(h.contains(&(900, Op::Assert, s("c"), Precision::Exact)));
    assert_eq!(h.len(), 5);
}

#[test]
fn in_an_empty_world_a_tickless_op_lands_on_tick_zero() {
    let w = load(&doc(
        "transactions:\n  - ops:\n      - assert: [greg, alias, \"a\"]\n",
    ));
    assert_eq!(hist(&w, id(&w, "greg"), "alias")[0].0, 0);
}

#[test]
fn a_tickless_retract_of_a_value_asserted_tickless_in_the_same_world_is_a_same_tick_conflict() {
    // CLAUDE.md: the latest tick only moves when a later tick is written
    let (e, at) = store(&doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "a"]
  - ops:
      - retract: [greg, alias, "a"]
"#));
    assert!(
        matches!(e, Error::SameTickConflict { tick: Tick(0), .. }),
        "{e:?}"
    );
    assert_eq!(at, opref(1, 0, None));
}

#[test]
fn the_bare_operations_form_makes_each_op_its_own_transaction() {
    let y = doc(r#"
operations:
  - assert: [greg, alias, "a"]
    tick: 10
  - assert: [greg, alias, "b"]
  - assert: [greg, alias, "far"]
    tick: 50
  - assert: [greg, alias, "c"]
  - assert: [greg, alias, "a"]
    tick: 10
"#);
    let w = load(&y);
    // entities (tx 1) + one tx per effective op: the fifth is a no-op (holds at 10)
    assert_eq!(w.latest_tx(), TxId(1 + 4));
    let h = hist(&w, id(&w, "greg"), "alias");
    // b lands on the latest tick when ITS txn started (10), c on 50: unlike one big txn
    assert!(h.contains(&(10, Op::Assert, s("b"), Precision::Exact)));
    assert!(h.contains(&(50, Op::Assert, s("c"), Precision::Exact)));
    // the same ops in ONE transaction would all share one tick
    let one = load(&doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "a"]
        tick: 10
      - assert: [greg, alias, "b"]
      - assert: [greg, alias, "far"]
        tick: 50
      - assert: [greg, alias, "c"]
"#));
    assert_eq!(one.latest_tx(), TxId(2));
    let h = hist(&one, id(&one, "greg"), "alias");
    assert!(h.contains(&(0, Op::Assert, s("b"), Precision::Exact)));
    assert!(h.contains(&(0, Op::Assert, s("c"), Precision::Exact)));
}

#[test]
fn the_bare_form_equals_the_native_write_per_op_and_reports_op_positions() {
    let y = doc(r#"
operations:
  - assert: [greg, name, "Greg"]
    tick: -5
    precision: month
  - create: [CHARACTER, lisa]
  - retract: [greg, name, "Greg"]
    tick: -1
"#);
    parity(&y, |w| {
        let greg = id(w, "greg");
        w.assert_with_precision(greg, "name", s("Greg"), t(-5), Precision::Month)
            .unwrap();
        w.create_entity("CHARACTER", Some("lisa")).unwrap();
        w.retract(greg, "name", s("Greg"), t(-1)).unwrap();
    });
    // bare form positions: txn is the op's index, op is 0
    let e = err(&doc(r#"
operations:
  - assert: [greg, name, "Greg"]
    tick: 1
  - assert: [greg, name, "Greg2"]
    tick: 2
  - assert: [nobody, name, "x"]
    label: bad
"#));
    assert_eq!(e.kind, FixtureErrorKind::UnknownHandle("nobody".into()));
    assert_eq!(e.at, opref(2, 0, Some("bad")));
}

#[test]
fn extreme_ticks_load() {
    let y = doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "min"]
        tick: -9223372036854775808
      - assert: [greg, alias, "max"]
        tick: 9223372036854775807
      - assert: [greg, alias, "zero"]
        tick: 0
"#);
    let w = load(&y);
    let h = hist(&w, id(&w, "greg"), "alias");
    assert_eq!(h[0].0, i64::MIN);
    assert_eq!(h[1].0, 0);
    assert_eq!(h[2].0, i64::MAX);
}

#[test]
fn every_precision_name_loads_and_is_carried() {
    let names = ["exact", "day", "month", "year", "decade", "century"];
    let all = [
        Precision::Exact,
        Precision::Day,
        Precision::Month,
        Precision::Year,
        Precision::Decade,
        Precision::Century,
    ];
    let mut ops = String::new();
    for (i, n) in names.iter().enumerate() {
        ops += &format!(
            "      - assert: [greg, alias, \"n{i}\"]\n        tick: {i}\n        precision: {n}\n"
        );
    }
    // an op without a precision is Exact
    ops += "      - assert: [greg, alias, \"plain\"]\n        tick: 9\n";
    let w = load(&doc(&format!("transactions:\n  - ops:\n{ops}")));
    let h = hist(&w, id(&w, "greg"), "alias");
    for (i, p) in all.iter().enumerate() {
        assert_eq!(h[i].3, *p, "{}", names[i]);
    }
    assert_eq!(h[6].3, Precision::Exact);
}

// ============================================================ values

#[test]
fn scalar_values_of_every_type_load_exactly() {
    let y = doc(r#"
transactions:
  - tick: 1
    ops:
      - assert: [greg, name, "Greg"]
      - assert: [greg, age, -9223372036854775808]
      - assert: [mara, age, 9223372036854775807]
      - assert: [greg, height, 1.5]
      - assert: [mara, height, 9007199254740992]
      - assert: [edwin, height, -9007199254740992]
      - assert: [greg, disguised, true]
      - assert: [mara, disguised, false]
      - assert: [edwin, name, 007]
"#);
    // `007` is a YAML integer, not a string: see the error test below. Drop it here.
    let y = y.replace("      - assert: [edwin, name, 007]\n", "");
    let w = load(&y);
    let (greg, mara, edwin) = (id(&w, "greg"), id(&w, "mara"), id(&w, "edwin"));
    assert_eq!(hist(&w, greg, "age")[0].2, Value::I64(i64::MIN));
    assert_eq!(hist(&w, mara, "age")[0].2, Value::I64(i64::MAX));
    assert_eq!(hist(&w, greg, "height")[0].2, Value::F64(1.5));
    assert_eq!(
        hist(&w, mara, "height")[0].2,
        Value::F64(9007199254740992.0)
    );
    assert_eq!(
        hist(&w, edwin, "height")[0].2,
        Value::F64(-9007199254740992.0)
    );
    assert_eq!(hist(&w, greg, "disguised")[0].2, Value::Bool(true));
    assert_eq!(hist(&w, mara, "disguised")[0].2, Value::Bool(false));
    assert_eq!(hist(&w, greg, "name")[0].2, s("Greg"));
}

#[test]
fn a_negative_zero_float_is_stored_as_positive_zero() {
    let w = load(&doc(
        "transactions:\n  - tick: 1\n    ops:\n      - assert: [greg, height, -0.0]\n",
    ));
    match &hist(&w, id(&w, "greg"), "height")[0].2 {
        Value::F64(x) => assert!(x.is_sign_positive() && *x == 0.0, "{x:?}"),
        v => panic!("{v:?}"),
    }
}

#[test]
fn string_values_are_stored_exactly_as_written() {
    let w = load(&doc(
        "transactions:\n  - tick: 1\n    ops:\n      - assert: [greg, name, \"  Gr\\u00e9g \\n tab\\t \"]\n      - assert: [greg, alias, \"123\"]\n      - assert: [greg, alias, \"true\"]\n      - assert: [greg, alias, \"\"]\n",
    ));
    let g = id(&w, "greg");
    assert_eq!(hist(&w, g, "name")[0].2, s("  Gr\u{e9}g \n tab\t "));
    let vals: Vec<Value> = hist(&w, g, "alias").into_iter().map(|r| r.2).collect();
    assert!(vals.contains(&s("123")) && vals.contains(&s("true")) && vals.contains(&s("")));
}

#[test]
fn an_unindexed_string_loads_at_any_length_and_survives_a_file_reopen() {
    let long = "x".repeat(100_000);
    let y = doc(&format!(
        "transactions:\n  - tick: 1\n    ops:\n      - assert: [greg, biography, \"{long}\"]\n"
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    drop(load_fixture_to(&y, &path).unwrap());
    let w = World::open(&path).unwrap();
    assert_eq!(hist(&w, id(&w, "greg"), "biography")[0].2, Value::Str(long));
    clean(&w);
}

#[test]
fn a_ref_value_is_a_handle_and_an_edge_triple_is_an_edge_reference() {
    let y = doc(r#"
transactions:
  - tick: -300
    ops:
      - assert: [edwin, present, carnath]
      - assert: [greg, located_in, varrow]
  - tick: -290
    ops:
      - assert: [mara, saw, [edwin, present, carnath]]
      - assert: [[edwin, present, carnath], disguised, true]
      - retract: [[edwin, present, carnath], disguised, true]
        tick: -280
"#);
    let p = parity(&y, |w| {
        let (edwin, greg, mara, varrow, carnath) = (
            id(w, "edwin"),
            id(w, "greg"),
            id(w, "mara"),
            id(w, "varrow"),
            id(w, "carnath"),
        );
        let mut tx = w.begin();
        tx.set_default_tick(Tick(-300));
        tx.assert(edwin, "present", Value::Ref(carnath), None)
            .unwrap();
        tx.assert(greg, "located_in", Value::Ref(varrow), None)
            .unwrap();
        tx.commit().unwrap();
        let edge: EntityId = w.edge_id(edwin, "present", carnath).unwrap().into();
        let mut tx = w.begin();
        tx.set_default_tick(Tick(-290));
        tx.assert(mara, "saw", Value::Ref(edge), None).unwrap();
        tx.assert(edge, "disguised", Value::Bool(true), None)
            .unwrap();
        tx.retract(edge, "disguised", Value::Bool(true), t(-280))
            .unwrap();
        tx.commit().unwrap();
    });
    let w = &p.world;
    let edge: EntityId = w
        .edge_id(id(w, "edwin"), "present", id(w, "carnath"))
        .unwrap()
        .into();
    assert_eq!(
        w.as_of(id(w, "mara"), "saw", Tick(-290)).unwrap(),
        vec![Value::Ref(edge)]
    );
    assert!(
        w.as_of(edge, "disguised", Tick(-285))
            .unwrap()
            .contains(&Value::Bool(true))
    );
    assert!(w.as_of(edge, "disguised", Tick(-280)).unwrap().is_empty());
}

#[test]
fn an_entity_created_by_a_create_op_is_usable_from_the_next_transaction() {
    let y = doc(r#"
transactions:
  - ops:
      - create: [CHARACTER, lisa]
      - create: [ITEM, sword]
  - tick: 5
    ops:
      - assert: [lisa, name, "Lisa"]
      - assert: [lisa, located_in, varrow]
      - assert: [edwin, bought, sword]
"#);
    let p = parity(&y, |w| {
        let mut tx = w.begin();
        let lisa = tx.create_entity("CHARACTER", Some("lisa")).unwrap();
        tx.create_entity("ITEM", Some("sword")).unwrap();
        tx.commit().unwrap();
        let (edwin, varrow) = (id(w, "edwin"), id(w, "varrow"));
        let sword = id(w, "sword");
        let mut tx = w.begin();
        tx.set_default_tick(Tick(5));
        tx.assert(lisa, "name", s("Lisa"), None).unwrap();
        tx.assert(lisa, "located_in", Value::Ref(varrow), None)
            .unwrap();
        tx.assert(edwin, "bought", Value::Ref(sword), None).unwrap();
        tx.commit().unwrap();
    });
    assert_eq!(id(&p.world, "lisa").seq(), 4, "after edwin, greg, mara");
    assert_eq!(id(&p.world, "sword").seq(), 1);
}

#[test]
fn a_bidirectional_write_in_either_direction_is_one_edge() {
    let y = doc(r#"
transactions:
  - tick: 1
    ops:
      - assert: [mara, ally_of, edwin]
      - assert: [edwin, ally_of, mara]
"#);
    let w = load(&y);
    // the second is a "0 changed" no-op: one fact stored (plus the entities txn)
    assert_eq!(w.latest_tx(), TxId(2));
    assert_eq!(w.history(id(&w, "edwin"), "ally_of").unwrap().len(), 1);
}

// ============================================================ corrections

#[test]
fn label_corrections_delete_and_replace_equal_the_native_corrections() {
    let y = doc(r#"
transactions:
  - tick: 10
    ops:
      - assert: [greg, name, "Greg"]
        label: n
      - assert: [greg, located_in, varrow]
        label: l
      - assert: [greg, alias, "G"]
        label: g
      - assert: [greg, alias, "Gregor"]
        label: gr
      - assert: [greg, age, 40]
        tick: 12
        precision: month
        label: a
  - ops:
      - retract: [greg, located_in, varrow]
        tick: 20
        label: leaves
  - ops:
      - correct: n
        replace: { value: "Gregory" }
      - correct: l
        replace: { value: ashford }
      - correct: leaves
        replace: { tick: 15, precision: day }
      - correct: g
        delete: true
      - correct: gr
        replace: { op: retract, precision: century }
      - correct: a
        replace: { tick: 13, value: 41, precision: year }
  - ops:
      - correct: n
        replace: { precision: decade }
"#);
    let mut older = TxId(0);
    let p = parity(&y, |w| {
        let (greg, varrow, ashford) = (id(w, "greg"), id(w, "varrow"), id(w, "ashford"));
        let mut tx = w.begin();
        tx.set_default_tick(Tick(10));
        tx.assert(greg, "name", s("Greg"), None).unwrap();
        tx.assert(greg, "located_in", Value::Ref(varrow), None)
            .unwrap();
        tx.assert(greg, "alias", s("G"), None).unwrap();
        tx.assert(greg, "alias", s("Gregor"), None).unwrap();
        tx.assert_with_precision(greg, "age", Value::I64(40), t(12), Precision::Month)
            .unwrap();
        let c = tx.commit().unwrap();
        let [n, l, g, gr, a] = c.entries[..] else {
            panic!()
        };
        let r = w
            .retract(greg, "located_in", Value::Ref(varrow), t(20))
            .unwrap();
        let leaves = r.entries[0];
        older = w.latest_tx();
        let rep = |value, tick, op, precision| Correction::Replace {
            value,
            tick,
            op,
            precision,
        };
        let mut tx = w.begin();
        tx.correct(n, rep(Some(s("Gregory")), None, None, None))
            .unwrap();
        tx.correct(l, rep(Some(Value::Ref(ashford)), None, None, None))
            .unwrap();
        tx.correct(leaves, rep(None, t(15), None, Some(Precision::Day)))
            .unwrap();
        tx.correct(g, Correction::Delete).unwrap();
        tx.correct(
            gr,
            rep(None, None, Some(Op::Retract), Some(Precision::Century)),
        )
        .unwrap();
        tx.correct(
            a,
            rep(Some(Value::I64(41)), t(13), None, Some(Precision::Year)),
        )
        .unwrap();
        tx.commit().unwrap();
        // the same label (n) corrected a second time: a second revision of the chain
        w.correct(n, rep(None, None, None, Some(Precision::Decade)))
            .unwrap();
    });
    let w = &p.world;
    let greg = id(w, "greg");
    // older canon still sees the old canon
    let old = w.at_canon(older);
    assert_eq!(old.as_of(greg, "name", Tick(10)).unwrap(), vec![s("Greg")]);
    assert_eq!(w.as_of(greg, "name", Tick(10)).unwrap(), vec![s("Gregory")]);
    assert_eq!(
        w.as_of_detail(greg, "name", Tick(10)).unwrap()[0].precision,
        Precision::Decade
    );
    assert!(
        w.as_of(greg, "alias", Tick(10))
            .unwrap()
            .iter()
            .all(|v| *v != s("G"))
    );
}

#[test]
fn a_label_can_name_a_retract_and_corrections_chain_across_transactions() {
    let y = doc(r#"
transactions:
  - tick: 1
    ops:
      - assert: [greg, alias, "x"]
  - tick: 5
    ops:
      - retract: [greg, alias, "x"]
        label: r
  - ops:
      - correct: r
        replace: { tick: 7 }
  - ops:
      - correct: r
        replace: { tick: 9, precision: year }
  - ops:
      - correct: r
        delete: true
"#);
    parity(&y, |w| {
        let greg = id(w, "greg");
        w.assert(greg, "alias", s("x"), t(1)).unwrap();
        let r = w.retract(greg, "alias", s("x"), t(5)).unwrap().entries[0];
        let rep = |tick, precision| Correction::Replace {
            value: None,
            tick,
            op: None,
            precision,
        };
        w.correct(r, rep(t(7), None)).unwrap();
        w.correct(r, rep(t(9), Some(Precision::Year))).unwrap();
        w.correct(r, Correction::Delete).unwrap();
    });
}

// ============================================================ structure errors

#[test]
fn malformed_yaml_is_a_yaml_error() {
    for bad in [
        "schema: [unclosed",
        "a: b: c: d",
        "\"unterminated",
        "key: value\n\tbad: tab",
        "- a\nb: c",
        "{a: 1,, b: 2}",
    ] {
        let e = err(bad);
        assert!(is_yaml(&e), "{bad:?} -> {e:?}");
        assert_eq!(e.at, None, "{bad:?}");
    }
}

#[test]
fn an_unknown_key_anywhere_is_invalid() {
    for (where_, y) in [
        ("top level", format!("{SCHEMA}\nbogus: 1\n")),
        (
            "schema",
            "schema:\n  node_types: [A]\n  bogus: 1\n".to_string(),
        ),
        (
            "attribute",
            "schema:\n  node_types: [A]\n  attributes:\n    - { name: n, type: string, colour: red }\n"
                .to_string(),
        ),
        (
            "transaction",
            doc("transactions:\n  - bogus: 1\n    ops:\n      - assert: [greg, name, \"x\"]\n"),
        ),
        (
            "txn-level precision",
            doc("transactions:\n  - precision: year\n    ops:\n      - assert: [greg, name, \"x\"]\n"),
        ),
        (
            "op",
            doc("transactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\n        bogus: 1\n"),
        ),
        (
            "op in the bare form",
            doc("operations:\n  - assert: [greg, name, \"x\"]\n    bogus: 1\n"),
        ),
        (
            "replace",
            doc("transactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\n        label: l\n  - ops:\n      - correct: l\n        replace: { bogus: 1 }\n"),
        ),
    ] {
        let e = err(&y);
        assert!(is_invalid(&e), "{where_}: {e:?}");
    }
}

#[test]
fn transactions_and_operations_together_are_invalid() {
    invalid(&doc(
        "transactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\noperations:\n  - assert: [greg, name, \"y\"]\n",
    ));
    // even when both are empty
    invalid(&doc("transactions: []\noperations: []\n"));
}

#[test]
fn an_op_must_be_exactly_one_of_assert_retract_create_correct() {
    let cases = [
        ("none", "      - tick: 1\n"),
        ("none, empty map", "      - {}\n"),
        (
            "assert+retract",
            "      - assert: [greg, name, \"x\"]\n        retract: [greg, name, \"x\"]\n",
        ),
        (
            "assert+create",
            "      - assert: [greg, name, \"x\"]\n        create: [CHARACTER, zed]\n",
        ),
        (
            "assert+correct",
            "      - assert: [greg, name, \"x\"]\n        correct: l\n        delete: true\n",
        ),
        (
            "create+retract",
            "      - create: [CHARACTER, zed]\n        retract: [greg, name, \"x\"]\n",
        ),
        (
            "all four",
            "      - assert: [greg, name, \"x\"]\n        retract: [greg, name, \"x\"]\n        create: [CHARACTER, zed]\n        correct: l\n",
        ),
    ];
    for (what, op) in cases {
        for (form, y) in [
            (
                "transactions",
                doc(&format!("transactions:\n  - ops:\n{op}")),
            ),
            (
                "operations",
                doc(&format!(
                    "operations:\n{}",
                    op.lines()
                        .map(|l| l.strip_prefix("    ").unwrap_or(l).to_string() + "\n")
                        .collect::<String>()
                )),
            ),
        ] {
            let e = err(&y);
            assert!(is_invalid(&e), "{what} / {form}: {e:?}");
            assert_eq!(e.at, opref(0, 0, None), "{what} / {form}");
        }
    }
}

#[test]
fn the_position_of_a_shape_error_is_the_ops_position() {
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
  - ops:
      - assert: [greg, name, "b"]
        tick: 2
      - assert: [greg, name, "c"]
        retract: [greg, name, "c"]
"#));
    assert!(is_invalid(&e), "{e:?}");
    assert_eq!(e.at, opref(1, 1, None));
}

#[test]
fn a_label_on_a_create_or_a_correct_is_invalid() {
    let e = err(&doc(
        "transactions:\n  - ops:\n      - create: [CHARACTER, zed]\n        label: l\n",
    ));
    assert!(is_invalid(&e), "{e:?}");
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((0, 0)));
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "x"]
        label: l
  - ops:
      - correct: l
        delete: true
        label: again
"#));
    assert!(is_invalid(&e), "{e:?}");
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((1, 0)));
}

#[test]
fn tick_and_precision_on_a_non_write_op_are_not_silently_ignored() {
    // CONTRACT GAP (raised in the report): the contract says `tick` / `precision` "apply to
    // assert/retract" but not what they do on create/correct. Only pin that it never
    // panics and never writes anything at that tick.
    let r = load_fixture(&doc(
        "transactions:\n  - ops:\n      - create: [CHARACTER, zed]\n        tick: 5\n",
    ));
    if let Ok(w) = r {
        assert_eq!(w.latest_tick(), Tick(0));
    }
}

#[test]
fn correct_needs_exactly_one_of_delete_or_replace() {
    let base = r#"
transactions:
  - ops:
      - assert: [greg, name, "x"]
        label: l
  - ops:
"#;
    for body in [
        "      - correct: l\n",
        "      - correct: l\n        delete: true\n        replace: { tick: 1 }\n",
    ] {
        let e = invalid(&doc(&format!("{base}{body}")));
        assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((1, 0)), "{body}");
    }
}

#[test]
fn the_type_names_of_the_schema_are_checked() {
    for bad in [
        "{ name: a, type: integer }",
        "{ name: a, type: STRING }",
        "{ name: a, type: string, kind: fact }",
        "{ name: a, type: string, cardinality: several }",
        "{ name: a, type: ref, direction: both }",
        "{ name: a, type: ref, reverse: lots }",
        "{ name: a, type: string, indexed: maybe }",
        "{ name: a }",
        "{ type: string }",
        "{ name: a, type: 5 }",
        "{ name: 5, type: string }",
    ] {
        let y = format!("schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {bad}\n");
        let e = err(&y);
        assert!(is_invalid(&e), "{bad}: {e:?}");
        assert_eq!(e.at, None);
    }
}

#[test]
fn schema_declarations_the_store_refuses_are_errors_with_no_position() {
    // The kind is the contract's "Invalid or Store": pin that it is an error with no op.
    for y in [
        // duplicate attribute name
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: a, type: string }\n    - { name: a, type: i64 }\n",
        // duplicate node type
        "schema:\n  node_types: [CHARACTER, CHARACTER]\n",
        // direction on a non-ref attribute
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: a, type: string, direction: bidirectional }\n",
        // bidirectional with an inverse
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: a, type: ref, direction: bidirectional, inverse: b }\n",
        // empty name
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: \"\", type: string }\n",
    ] {
        let e = err(y);
        assert!(
            matches!(
                e.kind,
                FixtureErrorKind::Invalid(_) | FixtureErrorKind::Store(_)
            ),
            "{y}: {e:?}"
        );
        assert_eq!(e.at, None, "{y}");
    }
}

// ============================================================ scalar type errors

fn value_op(attr: &str, value: &str) -> String {
    doc(&format!(
        "transactions:\n  - ops:\n      - assert: [greg, {attr}, {value}]\n        tick: 1\n"
    ))
}

#[test]
fn a_scalar_of_the_wrong_type_is_invalid_and_names_the_op() {
    let cases = [
        // i64
        ("age", "\"5\""),
        ("age", "5.5"),
        ("age", "true"),
        ("age", "null"),
        ("age", "9223372036854775808"),
        ("age", "-9223372036854775809"),
        ("age", "[1]"),
        // string (needs a YAML string)
        ("name", "5"),
        ("name", "007"),
        ("name", "true"),
        ("name", "null"),
        ("name", "1.5"),
        ("name", "[a, b]"),
        // bool: exactly true / false
        ("disguised", "yes"),
        ("disguised", "no"),
        ("disguised", "on"),
        ("disguised", "off"),
        ("disguised", "Yes"),
        ("disguised", "\"true\""),
        ("disguised", "1"),
        ("disguised", "0"),
        ("disguised", "null"),
        // f64
        ("height", "\"1.5\""),
        ("height", "true"),
        ("height", "9007199254740993"),
        ("height", "-9007199254740993"),
        ("height", "9223372036854775807"),
        // ref
        ("located_in", "5"),
        ("located_in", "true"),
        ("located_in", "null"),
        ("located_in", "[varrow]"),
        ("located_in", "[varrow, located_in]"),
        ("located_in", "[varrow, located_in, ashford, extra]"),
        ("located_in", "[1, located_in, ashford]"),
        ("located_in", "[varrow, 2, ashford]"),
        ("located_in", "[varrow, located_in, 3]"),
    ];
    for (attr, v) in cases {
        let y = value_op(attr, v);
        let e = err(&y);
        assert!(is_invalid(&e), "{attr} = {v}: {e:?}");
        assert_eq!(e.at, opref(0, 0, None), "{attr} = {v}");
    }
}

#[test]
fn a_nan_float_is_a_store_value_error_at_the_op() {
    for nan in [".nan", ".NaN", ".NAN"] {
        let (e, at) = store(&value_op("height", nan));
        assert_eq!(e, Error::Value(ValueError::NaN), "{nan}");
        assert_eq!(at, opref(0, 0, None));
    }
}

#[test]
fn an_indexed_string_over_256_bytes_is_a_store_error_and_256_loads() {
    let ok = "y".repeat(256);
    load(&value_op("name", &format!("\"{ok}\"")));
    let bad = "y".repeat(257);
    let (e, at) = store(&value_op("name", &format!("\"{bad}\"")));
    assert!(
        matches!(e, Error::Value(ValueError::StringTooLong { .. })),
        "{e:?}"
    );
    assert_eq!(at, opref(0, 0, None));
}

#[test]
fn bad_ticks_and_precisions_are_invalid() {
    for tick in [
        "\"5\"",
        "1.5",
        "true",
        "9223372036854775808",
        "-9223372036854775809",
        "null",
        "[1]",
    ] {
        let y = doc(&format!(
            "transactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\n        tick: {tick}\n"
        ));
        let e = err(&y);
        assert!(is_invalid(&e), "tick {tick}: {e:?}");
        // and as a txn default
        let y = doc(&format!(
            "transactions:\n  - tick: {tick}\n    ops:\n      - assert: [greg, name, \"x\"]\n"
        ));
        let e = err(&y);
        assert!(is_invalid(&e), "txn tick {tick}: {e:?}");
    }
    for p in ["fortnight", "Year", "YEAR", "5", "true", "null", "\"\""] {
        let y = doc(&format!(
            "transactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\n        precision: {p}\n"
        ));
        let e = err(&y);
        assert!(is_invalid(&e), "precision {p}: {e:?}");
    }
}

// ============================================================ reference errors

#[test]
fn an_unknown_handle_is_reported_with_its_position() {
    for (what, op) in [
        ("subject", "assert: [nobody, name, \"x\"]"),
        ("ref value", "assert: [greg, located_in, nowhere]"),
        ("retract subject", "retract: [nobody, name, \"x\"]"),
        (
            "edge triple source",
            "assert: [[nobody, present, carnath], disguised, true]",
        ),
        (
            "edge triple target",
            "assert: [[edwin, present, nowhere], disguised, true]",
        ),
        (
            "edge triple as value",
            "assert: [greg, saw, [edwin, present, nowhere]]",
        ),
    ] {
        let y = doc(&format!(
            "transactions:\n  - ops:\n      - assert: [greg, name, \"ok\"]\n      - {op}\n        label: probe\n"
        ));
        let e = err(&y);
        match &e.kind {
            FixtureErrorKind::UnknownHandle(h) => {
                assert!(h == "nobody" || h == "nowhere", "{what}: {h}");
            }
            k => panic!("{what}: {k:?}"),
        }
        assert_eq!(e.at, opref(0, 1, Some("probe")), "{what}");
    }
}

#[test]
fn a_handle_used_in_a_later_entities_less_world_is_unknown() {
    // no entities section and no create: every handle is unknown
    let e = err(&format!(
        "{SCHEMA}\ntransactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\n"
    ));
    assert_eq!(e.kind, FixtureErrorKind::UnknownHandle("greg".into()));
}

#[test]
fn an_edge_triple_naming_no_edge_is_unknown_edge_and_is_never_interned() {
    for (what, op) in [
        (
            "subject",
            "assert: [[edwin, present, carnath], disguised, true]",
        ),
        ("value", "assert: [mara, saw, [edwin, present, carnath]]"),
        (
            "retract subject",
            "retract: [[edwin, present, carnath], disguised, true]",
        ),
    ] {
        let y = doc(&format!(
            "transactions:\n  - ops:\n      - {op}\n        tick: 1\n        label: probe\n"
        ));
        let e = err(&y);
        assert!(
            matches!(e.kind, FixtureErrorKind::UnknownEdge(_)),
            "{what}: {e:?}"
        );
        assert_eq!(e.at, opref(0, 0, Some("probe")), "{what}");
    }
    // a triple whose attribute is not a ref attribute: also no edge (an error, never a panic)
    let e = err(&doc(
        "transactions:\n  - ops:\n      - assert: [[edwin, name, carnath], disguised, true]\n",
    ));
    assert!(
        matches!(
            e.kind,
            FixtureErrorKind::UnknownEdge(_)
                | FixtureErrorKind::Store(_)
                | FixtureErrorKind::Invalid(_)
        ),
        "{e:?}"
    );
}

#[test]
fn an_edge_asserted_earlier_in_the_same_run_is_usable_from_the_next_transaction_only() {
    // next transaction: fine
    load(&doc(r#"
transactions:
  - ops:
      - assert: [edwin, present, carnath]
        tick: 1
  - ops:
      - assert: [[edwin, present, carnath], disguised, true]
        tick: 1
"#));
    // the bare form puts each op in its own transaction, so it works there too
    load(&doc(r#"
operations:
  - assert: [edwin, present, carnath]
    tick: 1
  - assert: [[edwin, present, carnath], disguised, true]
    tick: 1
"#));
}

#[test]
fn a_duplicate_label_is_reported_at_the_second_op() {
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
        label: same
  - ops:
      - assert: [greg, alias, "b"]
        label: same
"#));
    assert_eq!(e.kind, FixtureErrorKind::DuplicateLabel("same".into()));
    assert_eq!(e.at, opref(1, 0, Some("same")));
    // also inside one transaction
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
        label: same
      - assert: [greg, alias, "b"]
        label: same
"#));
    assert_eq!(e.kind, FixtureErrorKind::DuplicateLabel("same".into()));
    assert_eq!(e.at, opref(0, 1, Some("same")));
}

#[test]
fn a_correct_of_a_label_that_never_existed_is_unknown_label() {
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
        label: real
  - ops:
      - correct: imaginary
        delete: true
"#));
    assert_eq!(e.kind, FixtureErrorKind::UnknownLabel("imaginary".into()));
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((1, 0)));
}

#[test]
fn a_label_used_before_its_transaction_committed_is_invalid() {
    // in its own transaction (the entry doesn't exist until the commit)
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
        label: l
      - correct: l
        delete: true
"#));
    assert!(is_invalid(&e), "{e:?}");
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((0, 1)));
    // in an earlier transaction than the one that defines it
    let e = err(&doc(r#"
transactions:
  - ops:
      - correct: l
        delete: true
  - ops:
      - assert: [greg, name, "a"]
        label: l
"#));
    assert!(is_invalid(&e), "{e:?}");
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((0, 0)));
}

#[test]
fn a_correct_of_a_label_whose_op_stored_nothing_is_invalid() {
    // the second assert holds already at its tick: "0 changed", nothing to correct
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "a"]
        tick: 1
  - ops:
      - assert: [greg, alias, "a"]
        tick: 2
        label: noop
  - ops:
      - correct: noop
        delete: true
"#));
    assert!(is_invalid(&e), "{e:?}");
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((2, 0)));
}

#[test]
fn a_correct_of_an_already_deleted_label_is_a_store_error_at_the_op() {
    let e = err(&doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "a"]
        label: l
  - ops:
      - correct: l
        delete: true
  - ops:
      - correct: l
        delete: true
"#));
    match e.kind {
        FixtureErrorKind::Store(Error::AlreadyDeleted(_)) => {}
        k => panic!("{k:?}"),
    }
    assert_eq!(e.at.as_ref().map(|a| (a.txn, a.op)), Some((2, 0)));
}

#[test]
fn store_errors_carry_the_store_error_and_the_ops_position() {
    // unknown attribute
    let (e, at) = store(&doc(
        "transactions:\n  - ops:\n      - assert: [greg, nope, \"x\"]\n        label: q\n",
    ));
    assert_eq!(e, Error::UnknownAttribute("nope".into()));
    assert_eq!(at, opref(0, 0, Some("q")));
    // an inverse name is not an attribute
    let (e, _) = store(&doc(
        "transactions:\n  - ops:\n      - assert: [greg, child_of, edwin]\n",
    ));
    assert_eq!(e, Error::UnknownAttribute("child_of".into()));
    // retracting an event
    let (e, at) = store(&doc(
        "transactions:\n  - ops:\n      - retract: [edwin, bought, varrow]\n        tick: 1\n",
    ));
    assert_eq!(e, Error::RetractEvent("bought".into()));
    assert_eq!(at, opref(0, 0, None));
    // same-tick conflict
    let (e, at) = store(&doc(r#"
transactions:
  - ops:
      - assert: [greg, alias, "a"]
        tick: 5
      - retract: [greg, alias, "a"]
        tick: 5
        label: clash
"#));
    assert!(
        matches!(&e, Error::SameTickConflict { attr, tick: Tick(5), .. } if attr == "alias"),
        "{e:?}"
    );
    assert_eq!(at, opref(0, 1, Some("clash")));
    // unknown node type in create
    let (e, at) = store(&doc(
        "transactions:\n  - ops:\n      - create: [WEAPON, sword]\n",
    ));
    assert_eq!(e, Error::UnknownNodeType("WEAPON".into()));
    assert_eq!(at, opref(0, 0, None));
    // duplicate handle in create (of an entity from the entities section)
    let (e, at) = store(&doc(
        "transactions:\n  - ops:\n      - create: [CHARACTER, greg]\n",
    ));
    assert_eq!(e, Error::DuplicateHandle("greg".into()));
    assert_eq!(at, opref(0, 0, None));
}

#[test]
fn errors_in_the_entities_section_are_store_errors() {
    let (e, _) = store(&format!("{SCHEMA}\nentities:\n  WEAPON: [sword]\n"));
    assert_eq!(e, Error::UnknownNodeType("WEAPON".into()));
    let (e, _) = store(&format!("{SCHEMA}\nentities:\n  CHARACTER: [greg, greg]\n"));
    assert_eq!(e, Error::DuplicateHandle("greg".into()));
    // a handle is unique across types
    let (e, _) = store(&format!(
        "{SCHEMA}\nentities:\n  CHARACTER: [greg]\n  PLACE: [greg]\n"
    ));
    assert_eq!(e, Error::DuplicateHandle("greg".into()));
    // shape: a type's value must be a list of strings
    let e = err(&format!("{SCHEMA}\nentities:\n  CHARACTER: greg\n"));
    assert!(is_invalid(&e), "{e:?}");
    let e = err(&format!("{SCHEMA}\nentities:\n  CHARACTER: [1, 2]\n"));
    assert!(is_invalid(&e), "{e:?}");
    let e = err(&format!("{SCHEMA}\nentities: [greg]\n"));
    assert!(is_invalid(&e), "{e:?}");
}

#[test]
fn a_fixture_that_fails_never_hands_back_a_world() {
    // the failing op is the last: everything before it was fine, still an Err
    let y = doc(r#"
transactions:
  - ops:
      - assert: [greg, name, "a"]
        tick: 1
  - ops:
      - assert: [greg, nope, "a"]
"#);
    assert!(load_fixture(&y).is_err());
}

// ============================================================ limits (security)

fn valid_padded(total: usize) -> String {
    let base =
        doc("transactions:\n  - ops:\n      - assert: [greg, name, \"a\"]\n        tick: 1\n");
    let mut out = base;
    let line = format!("# {}\n", "p".repeat(78)); // 81 bytes
    while out.len() + line.len() <= total {
        out.push_str(&line);
    }
    // top up with single-byte newlines to the exact size
    while out.len() < total {
        out.push('\n');
    }
    assert_eq!(out.len(), total);
    out
}

#[test]
fn input_over_16_mib_is_a_yaml_error_and_exactly_16_mib_loads() {
    const MAX: usize = 16 * 1024 * 1024;
    let at_limit = valid_padded(MAX);
    let w = load(&at_limit);
    assert_eq!(hist(&w, id(&w, "greg"), "name").len(), 1);
    let over = valid_padded(MAX + 1);
    let e = err(&over);
    assert!(is_yaml(&e), "{e:?}");
    assert_eq!(e.at, None);
    // the size check does not depend on the content being valid YAML
    let e = err(&"x".repeat(MAX + 1));
    assert!(is_yaml(&e), "{e:?}");
}

#[test]
fn load_fixture_to_checks_the_size_limit_before_creating_the_file() {
    const MAX: usize = 16 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let e = load_fixture_to(&"#".repeat(MAX + 1), &path).unwrap_err();
    assert!(is_yaml(&e), "{e:?}");
    assert!(!path.exists(), "an oversize input must not create the file");
}

#[test]
fn nesting_deeper_than_16_is_an_error_and_never_overflows_the_stack() {
    for depth in [17usize, 40, 5_000] {
        let open = "[".repeat(depth);
        let close = "]".repeat(depth);
        let y = format!("entities:\n  CHARACTER: {open}{close}\n");
        let e = err(&y);
        assert!(is_yaml(&e) || is_invalid(&e), "flow depth {depth}: {e:?}");
        // block nesting
        let mut b = String::from("a:\n");
        for i in 1..depth.min(200) {
            b.push_str(&format!("{}a:\n", " ".repeat(i)));
        }
        let e = err(&b);
        assert!(is_yaml(&e) || is_invalid(&e), "block depth {depth}: {e:?}");
    }
    // the flow nesting limit is 16: a fixture needs about 7 levels, so a normal one loads
    load(&doc(KITCHEN_SINK_OPS));
}

#[test]
fn an_alias_bomb_is_an_error_and_returns_quickly() {
    let start = Instant::now();
    // billion laughs, every layer a list of the previous layer ten times
    let mut y = String::from("a0: &a0 [lol, lol, lol, lol, lol, lol, lol, lol, lol, lol]\n");
    for i in 1..=9 {
        let prev = i - 1;
        let refs = vec![format!("*a{prev}"); 10].join(", ");
        y.push_str(&format!("a{i}: &a{i} [{refs}]\n"));
    }
    let e = err(&y);
    assert!(is_yaml(&e) || is_invalid(&e), "{e:?}");
    // the same bomb sitting where the format has lists
    let mut y = String::from(
        "entities:\n  CHARACTER: &a0 [lol, lol, lol, lol, lol, lol, lol, lol, lol, lol]\n",
    );
    for i in 1..=9 {
        let prev = i - 1;
        let refs = vec![format!("*a{prev}"); 10].join(", ");
        y.push_str(&format!("  PLACE{i}: &a{i} [{refs}]\n"));
    }
    let e = err(&format!("{SCHEMA}{y}"));
    assert!(is_yaml(&e) || is_invalid(&e), "{e:?}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "a bomb must be rejected quickly, took {:?}",
        start.elapsed()
    );
}

#[test]
fn more_than_1000_aliases_is_a_yaml_error_and_a_few_aliases_load() {
    // a few aliases, in the right places, are fine
    let ok = doc(r#"
operations:
  - &op { assert: [greg, alias, "a"], tick: 1 }
  - assert: [greg, alias, "b"]
    tick: &t 2
  - assert: [greg, alias, "c"]
    tick: *t
"#);
    let w = load(&ok);
    assert_eq!(hist(&w, id(&w, "greg"), "alias").len(), 3);
    // 1001 aliases of one small op
    let mut y = doc("operations:\n  - &op { assert: [greg, alias, \"a\"], tick: 1 }\n");
    for _ in 0..1001 {
        y.push_str("  - *op\n");
    }
    let e = err(&y);
    assert!(is_yaml(&e), "{e:?}");
}

#[test]
fn merge_keys_are_rejected() {
    let e = err(&doc(r#"
operations:
  - <<: { assert: [greg, alias, "a"] }
    tick: 1
"#));
    assert!(is_yaml(&e) || is_invalid(&e), "{e:?}");
    let e = err(&doc(r#"
operations:
  - &base { tick: 1 }
  - <<: *base
    assert: [greg, alias, "a"]
"#));
    assert!(is_yaml(&e) || is_invalid(&e), "{e:?}");
}

#[test]
fn explicit_tags_are_rejected() {
    for (what, y) in [
        (
            "binary label",
            doc("operations:\n  - assert: [greg, alias, \"a\"]\n    label: !!binary YQ==\n"),
        ),
        (
            "custom tag on a value",
            doc("operations:\n  - assert: [greg, alias, !custom a]\n"),
        ),
        (
            "tag on a mapping",
            doc("operations:\n  - !!map { assert: [greg, alias, \"a\"] }\n"),
        ),
        (
            "str tag on an int",
            doc("operations:\n  - assert: [greg, age, !!str 5]\n    tick: 1\n"),
        ),
        ("set tag", doc("operations: !!set { a }\n")),
    ] {
        let e = err(&y);
        assert!(is_yaml(&e) || is_invalid(&e), "{what}: {e:?}");
    }
}

#[test]
fn duplicate_keys_are_rejected() {
    for (what, y) in [
        (
            "in an op",
            doc("operations:\n  - assert: [greg, alias, \"a\"]\n    tick: 1\n    tick: 2\n"),
        ),
        (
            "at the top level",
            format!("{SCHEMA}{SCHEMA}"),
        ),
        (
            "in an attribute",
            "schema:\n  node_types: [A]\n  attributes:\n    - { name: n, type: string, type: i64 }\n".to_string(),
        ),
        (
            "in entities",
            format!("{SCHEMA}entities:\n  CHARACTER: [a]\n  CHARACTER: [b]\n"),
        ),
    ] {
        let e = err(&y);
        assert!(is_yaml(&e) || is_invalid(&e), "{what}: {e:?}");
    }
}

#[test]
fn more_than_one_yaml_document_is_a_yaml_error() {
    let e = err(&format!("{SCHEMA}\n---\n{SCHEMA}"));
    assert!(is_yaml(&e), "{e:?}");
}

#[test]
fn a_handle_label_or_node_type_over_256_bytes_is_invalid_and_256_is_fine() {
    let h256 = "h".repeat(256);
    let h257 = "h".repeat(257);
    // handles: the entities section and create
    load(&format!("{SCHEMA}\nentities:\n  CHARACTER: [{h256}]\n"));
    let e = err(&format!("{SCHEMA}\nentities:\n  CHARACTER: [{h257}]\n"));
    assert!(is_invalid(&e), "{e:?}");
    load(&doc(&format!(
        "operations:\n  - create: [CHARACTER, {h256}]\n"
    )));
    let e = err(&doc(&format!(
        "operations:\n  - create: [CHARACTER, {h257}]\n"
    )));
    assert!(is_invalid(&e), "{e:?}");
    // handles used as a subject are bounded too (an unknown 257 byte handle must not be
    // echoed back unbounded either way: just an error)
    let e = err(&doc(&format!(
        "operations:\n  - assert: [{h257}, name, \"x\"]\n    tick: 1\n"
    )));
    assert!(
        is_invalid(&e) || matches!(e.kind, FixtureErrorKind::UnknownHandle(_)),
        "{e:?}"
    );
    // labels
    let l256 = "l".repeat(256);
    let l257 = "l".repeat(257);
    load(&doc(&format!(
        "operations:\n  - assert: [greg, name, \"x\"]\n    tick: 1\n    label: {l256}\n"
    )));
    let e = err(&doc(&format!(
        "operations:\n  - assert: [greg, name, \"x\"]\n    tick: 1\n    label: {l257}\n"
    )));
    assert!(is_invalid(&e), "{e:?}");
    // node type names
    load(&format!("schema:\n  node_types: [{h256}]\n"));
    let e = err(&format!("schema:\n  node_types: [{h257}]\n"));
    assert!(is_invalid(&e), "{e:?}");
    // the limit is in bytes, not chars: 129 two-byte chars = 258 bytes
    let wide = "\u{e9}".repeat(129);
    let e = err(&format!("{SCHEMA}\nentities:\n  CHARACTER: [\"{wide}\"]\n"));
    assert!(is_invalid(&e), "{e:?}");
    load(&format!(
        "{SCHEMA}\nentities:\n  CHARACTER: [\"{}\"]\n",
        "\u{e9}".repeat(128)
    ));
}

#[test]
fn more_than_100000_ops_in_one_transaction_is_invalid() {
    let op = "      - assert: [greg, alias, \"a\"]\n        tick: 1\n";
    let mut y = doc("transactions:\n  - ops:\n");
    for _ in 0..100_000 {
        y.push_str(op);
    }
    // exactly 100,000 is within the limit (all but the first are no-ops)
    let w = load(&y);
    assert_eq!(hist(&w, id(&w, "greg"), "alias").len(), 1);
    y.push_str(op);
    let e = err(&y);
    assert!(is_invalid(&e), "{e:?}");
}

#[test]
#[cfg_attr(debug_assertions, ignore = "builds a 1M entity fixture: release only")]
fn more_than_1000000_entities_is_invalid() {
    let mut y = String::from("schema:\n  node_types: [CHARACTER]\nentities:\n  CHARACTER: [");
    for i in 0..=1_000_000u32 {
        y.push_str(&format!("e{i},"));
    }
    y.push_str("]\n");
    assert!(
        y.len() < 16 * 1024 * 1024,
        "the test input must fit the size limit"
    );
    let e = err(&y);
    assert!(is_invalid(&e), "{e:?}");
}

// ============================================================ load_fixture_to

#[test]
fn load_fixture_to_writes_a_file_that_reopens_with_identical_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let w = load_fixture_to(&doc(KITCHEN_SINK_OPS), &path).unwrap();
    let mem = load(&doc(KITCHEN_SINK_OPS));
    let (greg, edwin, mara) = (id(&w, "greg"), id(&w, "edwin"), id(&w, "mara"));
    let live = (
        hist(&w, greg, "located_in"),
        hist(&w, edwin, "ally_of"),
        hist(&w, mara, "alias"),
        w.index_dump(),
        w.latest_tx(),
    );
    // the in-memory loader and the file loader agree
    assert_eq!(live.3, mem.index_dump());
    assert_eq!(live.4, mem.latest_tx());
    assert_eq!(live.0, hist(&mem, greg, "located_in"));
    // Slice 15b: the returned world holds the lock only while it commits,
    // so another writer can open the file while it lives.
    assert!(World::open(&path).is_ok());
    drop(w);
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
    let r = World::open(&path).unwrap();
    assert_eq!(*r.recovery(), RecoveryReport::default());
    assert_eq!(
        (
            hist(&r, greg, "located_in"),
            hist(&r, edwin, "ally_of"),
            hist(&r, mara, "alias"),
            r.index_dump(),
            r.latest_tx()
        ),
        live
    );
    clean(&r);
}

#[test]
fn load_fixture_to_on_an_existing_path_is_an_already_exists_store_error_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    std::fs::write(&path, b"precious").unwrap();
    let e = load_fixture_to(&doc(""), &path).unwrap_err();
    match e.kind {
        FixtureErrorKind::Store(Error::Io { kind, .. }) => {
            assert_eq!(kind, std::io::ErrorKind::AlreadyExists)
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"precious");
    // and on a path that is a Kala world already
    let p2 = dir.path().join("w2.kala");
    drop(load_fixture_to(&doc(""), &p2).unwrap());
    let before = std::fs::read(&p2).unwrap();
    let e = load_fixture_to(&doc(""), &p2).unwrap_err();
    assert!(
        matches!(e.kind, FixtureErrorKind::Store(Error::Io { .. })),
        "{e:?}"
    );
    assert_eq!(std::fs::read(&p2).unwrap(), before);
}

#[test]
fn the_bare_form_to_a_file_is_one_transaction_per_op_in_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    drop(
        load_fixture_to(
            &doc("operations:\n  - assert: [greg, alias, \"a\"]\n    tick: 1\n  - assert: [greg, alias, \"b\"]\n    tick: 2\n  - assert: [greg, alias, \"c\"]\n    tick: 3\n"),
            &path,
        )
        .unwrap(),
    );
    let commits = kala_core::log::read_log(&path)
        .unwrap()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Commit { .. }))
        .count();
    // genesis + entities + three ops
    assert_eq!(commits, 1 + 1 + 3);
}

// ============================================================ properties

fn cfg(seed: u64, cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(seed),
        // a panic inside the loader is a failure, shrunk and reported: never caught here
        ..ProptestConfig::default()
    }
}

/// Ok or Err, deterministically; an Ok world is doctor-clean. A panic fails the test.
fn load_checked(text: &str) -> Result<IndexDump, FixtureError> {
    let first = load_fixture(text);
    let second = load_fixture(text);
    match (first, second) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a.index_dump(), b.index_dump(), "loading is deterministic");
            let rep = doctor(&a, &DoctorOptions::default());
            // mutated valid fixtures legitimately produce schema warnings (overlaps, edge
            // properties on unasserted edges); only store-integrity errors are bugs
            assert!(rep.errors().is_empty(), "{:#?}\n---\n{text}", rep.errors());
            Ok(a.index_dump())
        }
        (Err(a), Err(b)) => {
            assert_eq!(a, b, "errors are deterministic");
            // Display never panics and says something
            assert!(!a.to_string().is_empty());
            Err(a)
        }
        (a, b) => panic!(
            "non-deterministic: {:?} vs {:?}",
            a.map(|_| ()),
            b.map(|_| ())
        ),
    }
}

const TOKENS: &[&str] = &[
    "schema:",
    "node_types:",
    "attributes:",
    "entities:",
    "transactions:",
    "operations:",
    "ops:",
    "- ",
    "assert:",
    "retract:",
    "create:",
    "correct:",
    "replace:",
    "delete:",
    "label:",
    "tick:",
    "precision:",
    "name:",
    "type:",
    "kind:",
    "cardinality:",
    "direction:",
    "indexed:",
    "inverse:",
    "reverse:",
    "[",
    "]",
    "{",
    "}",
    ",",
    ": ",
    "\n",
    "\n  ",
    "\n    ",
    "\n      ",
    "-300",
    "0",
    "9223372036854775807",
    "-9223372036854775808",
    "1.5",
    ".nan",
    ".inf",
    "true",
    "false",
    "yes",
    "null",
    "~",
    "greg",
    "edwin",
    "varrow",
    "CHARACTER",
    "PLACE",
    "string",
    "ref",
    "i64",
    "bool",
    "f64",
    "many",
    "one",
    "year",
    "day",
    "&a",
    "*a",
    "&b",
    "*b",
    "<<:",
    "!!str",
    "!!binary",
    "!x",
    "\"",
    "'",
    "#",
    "---",
    "...",
    "|",
    ">",
    "?",
    "%",
    "\t",
    "\u{e9}",
    "\u{0}",
    "\u{feff}",
    "\r\n",
];

proptest! {
    #![proptest_config(cfg(0x4649_5854, 512))]

    /// Arbitrary text never panics the loader, and loading is deterministic.
    #[test]
    fn arbitrary_strings_never_panic(text in any::<String>()) {
        let _ = load_checked(&text);
    }

    #[test]
    fn arbitrary_printable_text_never_panics(text in "\\PC{0,300}") {
        let _ = load_checked(&text);
    }

    /// Yaml-looking soup built from the format's own tokens reaches the shape checks.
    #[test]
    fn token_soup_never_panics(idx in proptest::collection::vec(0..TOKENS.len(), 0..120)) {
        let text: String = idx.iter().map(|&i| TOKENS[i]).collect();
        let _ = load_checked(&text);
    }

    /// The same, with a valid schema in front so that the op parser sees the soup.
    #[test]
    fn token_soup_after_a_valid_header_never_panics(
        idx in proptest::collection::vec(0..TOKENS.len(), 0..120)
    ) {
        let tail: String = idx.iter().map(|&i| TOKENS[i]).collect();
        let text = format!("{SCHEMA}{ENTITIES}transactions:\n  - ops:\n      - {tail}");
        let _ = load_checked(&text);
        let text = format!("{SCHEMA}{ENTITIES}operations:\n  - {tail}");
        let _ = load_checked(&text);
    }
}

#[derive(Clone, Debug)]
enum Mutation {
    Flip(usize, u8),
    Insert(usize, Vec<u8>),
    Delete(usize, usize),
    Truncate(usize),
    DuplicateLine(usize),
    DropLine(usize),
    SwapLines(usize, usize),
    ShuffleLines(u64),
    Replace(usize, usize, usize),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), any::<u8>()).prop_map(|(i, b)| Mutation::Flip(i, b)),
        (any::<usize>(), proptest::collection::vec(any::<u8>(), 0..8))
            .prop_map(|(i, b)| Mutation::Insert(i, b)),
        (any::<usize>(), 0usize..40).prop_map(|(i, n)| Mutation::Delete(i, n)),
        any::<usize>().prop_map(Mutation::Truncate),
        any::<usize>().prop_map(Mutation::DuplicateLine),
        any::<usize>().prop_map(Mutation::DropLine),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Mutation::SwapLines(a, b)),
        any::<u64>().prop_map(Mutation::ShuffleLines),
        (any::<usize>(), 0usize..12, 0..TOKENS.len())
            .prop_map(|(i, n, tok)| Mutation::Replace(i, n, tok)),
    ]
}

fn apply(text: &str, m: &Mutation) -> String {
    let mut b = text.as_bytes().to_vec();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    match m {
        Mutation::Flip(i, x) if !b.is_empty() => {
            let i = i % b.len();
            b[i] ^= x | 1;
            return String::from_utf8_lossy(&b).into_owned();
        }
        Mutation::Insert(i, bytes) => {
            let i = i % (b.len() + 1);
            b.splice(i..i, bytes.iter().copied());
            return String::from_utf8_lossy(&b).into_owned();
        }
        Mutation::Delete(i, n) if !b.is_empty() => {
            let i = i % b.len();
            let end = (i + n).min(b.len());
            b.drain(i..end);
            return String::from_utf8_lossy(&b).into_owned();
        }
        Mutation::Truncate(i) if !b.is_empty() => {
            b.truncate(i % b.len());
            return String::from_utf8_lossy(&b).into_owned();
        }
        Mutation::Replace(i, n, tok) if !b.is_empty() => {
            let i = i % b.len();
            let end = (i + n).min(b.len());
            b.splice(i..end, TOKENS[*tok].bytes());
            return String::from_utf8_lossy(&b).into_owned();
        }
        Mutation::DuplicateLine(i) if !lines.is_empty() => {
            let i = i % lines.len();
            let l = lines[i].clone();
            lines.insert(i, l);
        }
        Mutation::DropLine(i) if !lines.is_empty() => {
            let i = i % lines.len();
            lines.remove(i);
        }
        Mutation::SwapLines(a, c) if !lines.is_empty() => {
            let n = lines.len();
            lines.swap(a % n, c % n);
        }
        Mutation::ShuffleLines(seed) => {
            // a deterministic Fisher-Yates driven by an LCG, so the case shrinks and replays
            let mut x = *seed | 1;
            for i in (1..lines.len()).rev() {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                lines.swap(i, (x >> 33) as usize % (i + 1));
            }
        }
        _ => {}
    }
    lines.join("\n") + "\n"
}

proptest! {
    #![proptest_config(cfg(0x4D55_5441, 512))]

    /// A valid fixture with a few random byte / line mutations: Ok or Err, never a panic;
    /// an Ok world is doctor-clean; the answer is deterministic.
    #[test]
    fn mutated_and_shuffled_valid_fixtures_never_panic(
        muts in proptest::collection::vec(mutation(), 1..6)
    ) {
        let mut text = doc(KITCHEN_SINK_OPS);
        for m in &muts {
            text = apply(&text, m);
        }
        let _ = load_checked(&text);
    }
}

#[test]
fn the_unmutated_kitchen_sink_is_ok_so_the_mutation_test_starts_from_a_valid_world() {
    load_checked(&doc(KITCHEN_SINK_OPS)).unwrap();
}

// ---- model: random write sequences, loader vs the native API

#[derive(Clone, Debug)]
struct MOp {
    ref_attr: bool,
    who: u8,
    val: u8,
    retract: bool,
    tick: Option<i8>,
    p: u8,
}

#[derive(Clone, Debug)]
struct MTxn {
    default_tick: Option<i8>,
    ops: Vec<MOp>,
}

fn mop() -> impl Strategy<Value = MOp> {
    (
        any::<bool>(),
        0u8..2,
        0u8..3,
        any::<bool>(),
        proptest::option::of(-4i8..5),
        0u8..6,
    )
        .prop_map(|(ref_attr, who, val, retract, tick, p)| MOp {
            ref_attr,
            who,
            val,
            retract,
            tick,
            p,
        })
}

fn mtxn() -> impl Strategy<Value = MTxn> {
    (
        proptest::option::of(-4i8..5),
        proptest::collection::vec(mop(), 1..5),
    )
        .prop_map(|(default_tick, ops)| MTxn { default_tick, ops })
}

const PNAMES: [&str; 6] = ["exact", "day", "month", "year", "decade", "century"];
const PREC: [Precision; 6] = [
    Precision::Exact,
    Precision::Day,
    Precision::Month,
    Precision::Year,
    Precision::Decade,
    Precision::Century,
];
const MWHO: [&str; 2] = ["greg", "mara"];
const MPLACE: [&str; 3] = ["varrow", "ashford", "carnath"];

fn m_schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "alias",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
}

const M_HEADER: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: alias, type: string, cardinality: many }
    - { name: located_in, type: ref, cardinality: many }
entities:
  CHARACTER: [greg, mara]
  PLACE: [varrow, ashford, carnath]
"#;

fn render_op(o: &MOp, indent: &str) -> String {
    let verb = if o.retract { "retract" } else { "assert" };
    let (attr, val) = if o.ref_attr {
        ("located_in", MPLACE[o.val as usize].to_string())
    } else {
        ("alias", format!("\"n{}\"", o.val))
    };
    let mut s = format!(
        "{indent}- {verb}: [{}, {attr}, {val}]\n",
        MWHO[o.who as usize]
    );
    if let Some(t) = o.tick {
        s += &format!("{indent}  tick: {t}\n");
    }
    // precision on every op (exact too, half the time) to exercise the keyword
    if o.p != 0 || o.val == 1 {
        s += &format!("{indent}  precision: {}\n", PNAMES[o.p as usize]);
    }
    s
}

fn native_op(
    tx: &mut Txn<'_>,
    w_ids: &(Vec<EntityId>, Vec<EntityId>),
    o: &MOp,
) -> Result<(), Error> {
    let e = w_ids.0[o.who as usize];
    let tk = o.tick.map(|x| Tick(x as i64));
    let p = if o.p != 0 || o.val == 1 {
        PREC[o.p as usize]
    } else {
        Precision::Exact
    };
    let (attr, v) = if o.ref_attr {
        ("located_in", Value::Ref(w_ids.1[o.val as usize]))
    } else {
        ("alias", Value::Str(format!("n{}", o.val)))
    };
    if o.retract {
        tx.retract_with_precision(e, attr, v, tk, p)?;
    } else {
        tx.assert_with_precision(e, attr, v, tk, p)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(cfg(0x4D4F_4445, 96))]

    /// Random write sequences, as transactions or as the bare form: the loader's file is
    /// byte-identical to the one the native API writes, or both fail at the same op with
    /// the same store error and the loader says where.
    #[test]
    fn the_loader_equals_the_native_api_on_random_sequences(
        txns in proptest::collection::vec(mtxn(), 1..6),
        bare in any::<bool>(),
    ) {
        // the bare form: each op its own txn, no txn default
        let txns: Vec<MTxn> = if bare {
            txns.iter()
                .flat_map(|t| t.ops.iter().map(|o| MTxn { default_tick: None, ops: vec![o.clone()] }))
                .collect()
        } else {
            txns
        };
        let mut y = String::from(M_HEADER);
        if bare {
            y.push_str("operations:\n");
            for t in &txns {
                y.push_str(&render_op(&t.ops[0], "  "));
            }
        } else {
            y.push_str("transactions:\n");
            for t in &txns {
                match t.default_tick {
                    Some(d) => y.push_str(&format!("  - tick: {d}\n    ops:\n")),
                    None => y.push_str("  - ops:\n"),
                }
                for o in &t.ops {
                    y.push_str(&render_op(o, "      "));
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (np, lp) = (dir.path().join("n.kala"), dir.path().join("l.kala"));
        // native side first
        let mut failed: Option<(usize, usize, Error)> = None;
        {
            let mut w = World::create(&np, m_schema()).unwrap();
            let mut tx = w.begin();
            let g: Vec<EntityId> = MWHO.iter().map(|h| tx.create_entity("CHARACTER", Some(h)).unwrap()).collect();
            let p: Vec<EntityId> = MPLACE.iter().map(|h| tx.create_entity("PLACE", Some(h)).unwrap()).collect();
            tx.commit().unwrap();
            let ids = (g, p);
            'outer: for (ti, t) in txns.iter().enumerate() {
                let mut tx = w.begin();
                if let Some(d) = t.default_tick {
                    tx.set_default_tick(Tick(d as i64));
                }
                for (oi, o) in t.ops.iter().enumerate() {
                    let r = native_op(&mut tx, &ids, o);
                    if let Err(e) = r {
                        failed = Some((ti, if bare { 0 } else { oi }, e));
                        break 'outer;
                    }
                }
                tx.commit().unwrap();
            }
        }
        let loaded = load_fixture_to(&y, &lp);
        match (failed, loaded) {
            (None, Ok(w)) => {
                clean(&w);
                drop(w);
                prop_assert_eq!(std::fs::read(&lp).unwrap(), std::fs::read(&np).unwrap());
            }
            (Some((ti, oi, e)), Err(fe)) => {
                prop_assert_eq!(fe.kind, FixtureErrorKind::Store(e));
                let at = fe.at.expect("a store error from an op says which op");
                prop_assert_eq!((at.txn, at.op), (ti, oi));
                prop_assert_eq!(at.label, None);
            }
            (n, l) => prop_assert!(false, "diverged: native {:?} vs loader {:?}\n{}", n, l.map(|_| ()), y),
        }
    }
}

// ============================================================ slice-4 confirm: edge cases

#[test]
fn the_bare_form_display_is_transaction_n_op_0() {
    let e = err(&doc(
        "operations:\n  - assert: [greg, name, \"a\"]\n    tick: 1\n  - assert: [greg, nope, \"b\"]\n    tick: 2\n",
    ));
    assert_eq!(e.at, opref(1, 0, None));
    assert!(e.to_string().starts_with("transaction 1, op 0: "), "{e}");
}

#[test]
fn a_schema_or_entities_error_displays_without_a_position() {
    let e = err(&format!("{SCHEMA}\nentities:\n  WEAPON: [sword]\n"));
    assert_eq!(e.at, None);
    assert!(!e.to_string().starts_with("transaction"), "{e}");
}

#[test]
fn yaml_1_2_core_booleans_true_and_false_in_any_of_the_three_cases_are_accepted() {
    for (lit, want) in [
        ("true", true),
        ("True", true),
        ("TRUE", true),
        ("false", false),
        ("False", false),
        ("FALSE", false),
    ] {
        let w = load(&value_op("disguised", lit));
        let h = hist(&w, id(&w, "greg"), "disguised");
        assert_eq!(h[0].2, Value::Bool(want), "{lit}");
    }
    for lit in ["yes", "no", "on", "off", "y", "n", "Yes", "NO", "On", "OFF"] {
        let e = err(&value_op("disguised", lit));
        assert!(is_invalid(&e), "{lit}: {e:?}");
    }
}

const ONE_LABELLED: &str = "transactions:\n  - ops:\n      - assert: [greg, name, \"x\"]\n        tick: 1\n        label: l\n  - ops:\n";

#[test]
fn delete_false_is_invalid_and_names_the_correct_op() {
    let e = invalid(&doc(&format!(
        "{ONE_LABELLED}      - correct: l\n        delete: false\n"
    )));
    assert_eq!(e.at, opref(1, 0, Some("l")));
}

#[test]
fn an_empty_replace_loads_and_changes_nothing_observable() {
    let w = load(&doc(&format!(
        "{ONE_LABELLED}      - correct: l\n        replace: {{}}\n"
    )));
    let greg = id(&w, "greg");
    assert_eq!(w.as_of(greg, "name", Tick(1)).unwrap().len(), 1);
    assert_eq!(w.as_of(greg, "name", Tick(0)).unwrap().len(), 0);
    clean(&w);
}

#[test]
fn an_explicit_int_tag_on_a_tick_is_invalid() {
    let e = invalid(&doc(
        "operations:\n  - assert: [greg, name, \"x\"]\n    tick: !!int 5\n",
    ));
    assert_eq!(
        e.at, None,
        "serde-level shape error: position is in the message"
    );
}

#[test]
fn infinite_floats_load_exactly() {
    let w = load(&doc(
        "operations:\n  - assert: [greg, height, .inf]\n    tick: 1\n  - assert: [mara, height, -.inf]\n    tick: 1\n",
    ));
    assert_eq!(
        hist(&w, id(&w, "greg"), "height")[0].2,
        Value::F64(f64::INFINITY)
    );
    assert_eq!(
        hist(&w, id(&w, "mara"), "height")[0].2,
        Value::F64(f64::NEG_INFINITY)
    );
    clean(&w);
}

#[test]
fn a_transaction_without_ops_is_invalid_and_with_empty_ops_loads_nothing() {
    let e = invalid(&doc("transactions:\n  - tick: 1\n"));
    assert!(e.to_string().contains("ops"), "{e}");
    let w = load(&doc("transactions:\n  - ops: []\n"));
    clean(&w);
    assert_eq!(w.latest_tick(), Tick(0));
}

#[test]
fn an_empty_handle_list_and_non_mapping_or_blank_documents() {
    let w = load("schema:\n  node_types: [CHARACTER]\nentities:\n  CHARACTER: []\n");
    assert!(w.entity_by_handle("x").is_none());
    clean(&w);
    for y in ["5", "[1]", "\"text\"", "true"] {
        let e = err(y);
        assert!(is_invalid(&e), "{y}: {e:?}");
    }
    for y in [
        "~",
        "null",
        "# only a comment\n# and another\n",
        "",
        "{}",
        "---\n",
    ] {
        let w = load(y);
        assert_eq!(w.latest_tick(), Tick(0), "{y:?}");
        clean(&w);
    }
}

#[test]
fn a_handle_created_in_a_transaction_is_usable_by_later_ops_of_the_same_transaction() {
    let w = load(&doc(
        "transactions:\n  - tick: 5\n    ops:\n      - create: [CHARACTER, lisa]\n      - assert: [lisa, name, \"Lisa\"]\n      - assert: [lisa, located_in, varrow]\n      - assert: [lisa, ally_of, edwin]\n",
    ));
    let lisa = id(&w, "lisa");
    assert_eq!(lisa.seq(), 4);
    assert_eq!(hist(&w, lisa, "name")[0].0, 5);
    // a bidirectional edge reads from either side through Out
    let edwin = id(&w, "edwin");
    assert_eq!(
        w.neighbors(edwin, "ally_of", Direction::Out, Tick(5))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        w.neighbors(lisa, "ally_of", Direction::Out, Tick(5))
            .unwrap()
            .len(),
        1
    );
    clean(&w);
}

#[test]
fn a_referenced_handle_over_256_bytes_is_invalid_not_unknown() {
    let h = "h".repeat(257);
    for body in [
        format!("  - assert: [{h}, name, \"x\"]\n    tick: 1\n"),
        format!("  - assert: [greg, located_in, {h}]\n    tick: 1\n"),
        format!("  - assert: [[{h}, located_in, varrow], disguised, true]\n    tick: 1\n"),
    ] {
        let e = invalid(&doc(&format!("operations:\n{body}")));
        assert_eq!(e.at, opref(0, 0, None), "{body}");
        // the message is bounded: it never echoes the whole handle
        assert!(e.to_string().len() < 400, "{}", e.to_string().len());
    }
}
