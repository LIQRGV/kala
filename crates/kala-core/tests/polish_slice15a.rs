//! Slice 15a review fixes in the native API.
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use std::time::{Duration, Instant};

/// A restore form creates many entities and then writes facts that name them
/// by handle. Resolving a staged handle (and its reverse) must be a map
/// lookup, not a scan of every staged definition.
///
/// Measured in a debug build with N = 40,000: the linear scan took 13.6 s
/// for the three loops below (3.4 s at N = 20,000); map lookups take 0.17 s. The
/// bound leaves a wide margin for a slow machine and is still far below what
/// the quadratic version needs.
#[test]
fn staged_handles_resolve_in_constant_time() {
    const N: usize = 40_000;
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let handles: Vec<String> = (0..N).map(|i| format!("h{i}")).collect();
    let mut tx = w.begin();
    let mut ids = Vec::with_capacity(N);
    for h in &handles {
        ids.push(tx.create_entity("CHARACTER", Some(h)).unwrap());
    }
    let started = Instant::now();
    for (h, id) in handles.iter().zip(&ids) {
        // What the applier does per fact: handle -> ID, then the write.
        let found = tx.entity_by_handle(h).unwrap();
        assert_eq!(found, *id);
        tx.assert(found, "age", Value::I64(1), Some(Tick(1)))
            .unwrap();
    }
    for (h, id) in handles.iter().zip(&ids) {
        assert_eq!(tx.handle_of(*id), Some(h.as_str()));
    }
    for (h, id) in handles.iter().zip(&ids) {
        assert_eq!(
            tx.find_or_create_entity("CHARACTER", h).unwrap(),
            (*id, false)
        );
    }
    let elapsed = started.elapsed();
    eprintln!("staged handle lookups for {N} entities: {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "resolving {N} staged handles took {elapsed:?}: staged handle lookup is not O(1)"
    );
    assert!(tx.entity_by_handle("nobody").is_none());
    let result = tx.commit().unwrap();
    assert_eq!(result.changed, N);
    assert_eq!(w.entity_by_handle("h39999"), Some(ids[N - 1]));
    assert_eq!(w.handle_of(ids[0]), Some("h0"));
}

/// A dropped transaction's handles are free again and resolve to nothing.
#[test]
fn staged_handle_lookups_follow_the_transaction_only() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let committed = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    {
        let mut tx = w.begin();
        let kael = tx.create_entity("CHARACTER", Some("kael")).unwrap();
        let anon = tx.create_entity("CHARACTER", None).unwrap();
        assert_eq!(tx.entity_by_handle("kael"), Some(kael));
        assert_eq!(tx.handle_of(kael), Some("kael"));
        assert_eq!(tx.handle_of(anon), None);
        assert_eq!(tx.entity_by_handle("edwin"), Some(committed));
        assert_eq!(tx.handle_of(committed), Some("edwin"));
    }
    assert_eq!(w.entity_by_handle("kael"), None);
    let mut tx = w.begin();
    assert_eq!(tx.entity_by_handle("kael"), None);
    assert!(tx.find_or_create_entity("CHARACTER", "kael").unwrap().1);
}

fn bio_schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "alias",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
}

/// A world where every cardinality-one side holds one value: each character
/// has a name and a large unindexed biography.
fn single_valued_world(n: usize) -> (World, Vec<kala_core::EntityId>) {
    let mut w = World::in_memory(bio_schema()).unwrap();
    let mut tx = w.begin();
    let mut ids = Vec::new();
    for i in 0..n {
        let e = tx
            .create_entity("CHARACTER", Some(&format!("c{i}")))
            .unwrap();
        tx.assert(e, "name", Value::Str(format!("Name {i}")), Some(Tick(1)))
            .unwrap();
        tx.assert(e, "bio", Value::Str("b".repeat(4096)), Some(Tick(1)))
            .unwrap();
        ids.push(e);
    }
    tx.commit().unwrap();
    (w, ids)
}

/// A rename, cardinality change or migration compares the whole world before
/// and after. A side with one value forms no overlap pair, so neither
/// snapshot may keep its decoded value (here: 50 names and 50 4 KiB
/// biographies, twice).
#[test]
fn a_world_scope_commit_retains_no_single_valued_side() {
    let (mut w, ids) = single_valued_world(50);
    assert_eq!(w.world_warning_snapshot_size(), (0, 0));
    // The commit itself is unaffected.
    let mut tx = w.begin();
    tx.rename_attribute("alias", "nickname").unwrap();
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings, kala_core::Warnings::default());
    // Only a side with two or more values is kept.
    w.assert(ids[7], "name", Value::Str("Second".into()), Some(Tick(5)))
        .unwrap();
    assert_eq!(w.world_warning_snapshot_size(), (1, 2));
}

/// Dropping single-valued sides must not change the diff: a side that goes
/// from one value to two (and back) under a world-scope commit still reports
/// the overlap it introduced (and resolved).
#[test]
fn a_world_scope_commit_still_reports_a_side_crossing_one_value() {
    let (mut w, ids) = single_valued_world(3);
    let mut tx = w.begin();
    tx.rename_attribute("alias", "nickname").unwrap();
    tx.assert(ids[1], "name", Value::Str("Second".into()), Some(Tick(5)))
        .unwrap();
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings.introduced.len(), 1, "{:?}", result.warnings);
    assert!(result.warnings.resolved.is_empty());
    let overlap = result.warnings.introduced[0].clone();
    assert_eq!(overlap.entity, ids[1]);
    assert_eq!(overlap.attr, "name");
    assert_eq!(overlap.a, Value::Str("Name 1".into()));
    assert_eq!(overlap.b, Value::Str("Second".into()));
    assert_eq!(overlap.during.from, Tick(5));

    let mut tx = w.begin();
    tx.rename_attribute("nickname", "alias").unwrap();
    tx.retract(ids[1], "name", Value::Str("Name 1".into()), Some(Tick(5)))
        .unwrap();
    let result = tx.commit().unwrap();
    assert!(
        result.warnings.introduced.is_empty(),
        "{:?}",
        result.warnings
    );
    assert_eq!(result.warnings.resolved, [overlap]);
    // Two values with touching intervals: still a side worth comparing.
    assert_eq!(w.world_warning_snapshot_size(), (1, 2));
}
