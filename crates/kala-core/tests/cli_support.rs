//! Slice 5 CLI support: read-only accessors the `kala` CLI needs from the
//! public API (RED until implemented). Expected signatures:
//!
//! ```text
//! World::parse_id(&self, s: &str) -> Option<EntityId>
//! World::handle_of(&self, id: EntityId) -> Option<&str>
//! World::attributes(&self) -> impl Iterator<Item = &AttrDef>
//! World::attribute(&self, name: &str) -> Option<&AttrDef>
//! World::attr_name(&self, id: AttrId) -> Option<&str>
//! World::node_types(&self) -> impl Iterator<Item = &str>
//! World::entries(&self) -> impl Iterator<Item = EntryInfo> + '_
//! View::latest_tick(&self) -> Tick
//! ```
//!
//! Decisions: leading zeros in a seq are refused (`CHARACTER#01` -> None);
//! node type names are case-sensitive; `parse_id` checks syntax and the
//! declared type only, not whether the entity is defined.

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("child_of"),
        )
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "tag_like",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn sv(s: &str) -> Value {
    Value::Str(s.to_owned())
}

/// A world with several types, handles, and edges.
fn world() -> (World, Vec<EntityId>, Vec<EdgeId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", None).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let sword = w.create_entity("ITEM", None).unwrap();
    let r1 = w
        .assert(edwin, "located_in", Value::Ref(varrow), t(1))
        .unwrap();
    let r2 = w.assert(edwin, "ally_of", Value::Ref(mara), t(2)).unwrap();
    let mut edges = r1.edges.clone();
    edges.extend(r2.edges);
    (w, vec![edwin, mara, varrow, sword], edges)
}

// ---------- parse_id ----------

#[test]
fn parse_id_inverts_format_id_for_entities_and_edges() {
    let (w, ents, edges) = world();
    assert_eq!(edges.len(), 2);
    for e in ents {
        assert_eq!(w.parse_id(&w.format_id(e)), Some(e), "{}", w.format_id(e));
    }
    for e in edges {
        let id: EntityId = e.into();
        assert_eq!(w.parse_id(&w.format_id(id)), Some(id));
    }
}

#[test]
fn parse_id_known_forms() {
    let (w, ents, edges) = world();
    assert_eq!(w.parse_id("CHARACTER#1"), Some(ents[0]));
    assert_eq!(w.parse_id("CHARACTER#2"), Some(ents[1]));
    assert_eq!(w.parse_id("PLACE#1"), Some(ents[2]));
    assert_eq!(w.parse_id("ITEM#1"), Some(ents[3]));
    assert_eq!(w.parse_id("edge#1"), Some(edges[0].into()));
    assert_eq!(w.parse_id("edge#2"), Some(edges[1].into()));
}

#[test]
fn parse_id_returns_ids_that_are_not_defined_yet() {
    let (w, _, _) = world();
    let id = w.parse_id("CHARACTER#99").unwrap();
    assert_eq!(
        id.node_type(),
        w.parse_id("CHARACTER#1").unwrap().node_type()
    );
    assert_eq!(id.seq(), 99);
    assert_eq!(w.parse_id("edge#9"), Some(EdgeId::new(9).unwrap().into()));
}

#[test]
fn parse_id_edge_zero_is_none() {
    let (w, _, _) = world();
    assert_eq!(w.parse_id("edge#0"), None);
}

#[test]
fn parse_id_refuses_malformed_input() {
    let (w, _, _) = world();
    let bad = [
        "",
        "#",
        "#1",
        "CHARACTER#",
        "CHARACTER",
        "CHARACTER#0",
        "CHARACTER#+1",
        "CHARACTER#-1",
        "CHARACTER#01",
        "CHARACTER#1#1",
        "CHARACTER##1",
        "CHARACTER#1a",
        "CHARACTER#0x1",
        "CHARACTER#1.0",
        "CHARACTER#1_0",
        " CHARACTER#1",
        "CHARACTER#1 ",
        "CHARACTER #1",
        "\tCHARACTER#1\n",
        "character#1",
        "Character#1",
        "WIZARD#1",
        "edge#",
        "edge#+1",
        "edge#-1",
        "edge#01",
        "edge#1x",
        " edge#1",
        "Edge#1",
        "EDGE#1",
        // the `#<type id>#seq` form format_id uses for unknown types
        "#0#1",
        "#1#1",
        "#999#1",
        // full-width digit
        "CHARACTER#\u{FF11}",
    ];
    for s in bad {
        assert_eq!(w.parse_id(s), None, "{s:?}");
    }
}

#[test]
fn parse_id_seq_range() {
    let (w, _, _) = world();
    let max = (1u64 << 48) - 1;
    let id = w.parse_id(&format!("CHARACTER#{max}")).unwrap();
    assert_eq!(id.seq(), max);
    assert_eq!(w.parse_id(&format!("CHARACTER#{}", max + 1)), None);
    assert_eq!(w.parse_id("CHARACTER#18446744073709551615"), None);
    assert_eq!(w.parse_id("CHARACTER#18446744073709551616"), None);
    assert_eq!(w.parse_id("CHARACTER#99999999999999999999999999"), None);
    // edges: 63-bit sequence
    let emax = (1u64 << 63) - 1;
    assert_eq!(
        w.parse_id(&format!("edge#{emax}")),
        Some(EdgeId::new(emax).unwrap().into())
    );
    assert_eq!(w.parse_id(&format!("edge#{}", emax + 1)), None);
    assert_eq!(w.parse_id("edge#18446744073709551616"), None);
}

#[test]
fn parse_id_in_a_world_with_no_node_types() {
    let w = World::in_memory(Schema::new()).unwrap();
    assert_eq!(w.parse_id("CHARACTER#1"), None);
    assert_eq!(w.parse_id("edge#1"), Some(EdgeId::new(1).unwrap().into()));
}

// ---------- handle_of ----------

#[test]
fn handle_of_round_trips_entity_by_handle() {
    let (w, ents, edges) = world();
    assert_eq!(w.handle_of(ents[0]), Some("edwin"));
    assert_eq!(w.handle_of(ents[2]), Some("varrow"));
    assert_eq!(
        w.entity_by_handle(w.handle_of(ents[0]).unwrap()),
        Some(ents[0])
    );
    assert_eq!(w.handle_of(ents[1]), None, "created without a handle");
    assert_eq!(w.handle_of(ents[3]), None);
    for e in edges {
        assert_eq!(w.handle_of(e.into()), None);
    }
}

#[test]
fn handle_of_undefined_ids_is_none() {
    let (w, _, _) = world();
    assert_eq!(w.handle_of(w.parse_id("CHARACTER#99").unwrap()), None);
    assert_eq!(w.handle_of(EdgeId::new(99).unwrap().into()), None);
    assert_eq!(
        w.handle_of(EntityId::new(NodeTypeId(500), 1).unwrap()),
        None
    );
}

#[test]
fn handle_of_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let (e, m) = {
        let mut w = World::create(&p, schema()).unwrap();
        let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
        let m = w.create_entity("CHARACTER", None).unwrap();
        (e, m)
    };
    let w = World::open(&p).unwrap();
    assert_eq!(w.handle_of(e), Some("edwin"));
    assert_eq!(w.handle_of(m), None);
}

// ---------- schema introspection ----------

fn check_schema(w: &World) {
    let names: Vec<&str> = w.attributes().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "located_in",
            "parent_of",
            "ally_of",
            "name",
            "bio",
            "tag_like"
        ]
    );
    let got: Vec<AttrDef> = w.attributes().cloned().collect();
    assert_eq!(got[1].inverse.as_deref(), Some("child_of"));
    assert_eq!(got[2].direction, Some(RefDirection::Bidirectional));
    assert!(!got[4].indexed);
    assert!(got[3].indexed);
    assert_eq!(got[0].cardinality, Cardinality::One);
    assert_eq!(got[0].kind, Kind::State);

    for a in &got {
        assert_eq!(w.attribute(&a.name), Some(a));
    }
    assert_eq!(
        w.attribute("child_of"),
        None,
        "inverse names aren't attributes"
    );
    assert_eq!(w.attribute("Name"), None, "exact match");
    assert_eq!(w.attribute("nope"), None);
    assert_eq!(w.attribute(""), None);

    assert_eq!(
        w.node_types().collect::<Vec<_>>(),
        ["CHARACTER", "PLACE", "ITEM"]
    );
}

#[test]
fn schema_introspection_in_memory() {
    let w = World::in_memory(schema()).unwrap();
    check_schema(&w);
}

#[test]
fn schema_introspection_after_create_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let w = World::create(&p, schema()).unwrap();
    check_schema(&w);
    drop(w);
    let w = World::open(&p).unwrap();
    check_schema(&w);
}

#[test]
fn schema_introspection_empty_schema() {
    let w = World::in_memory(Schema::new()).unwrap();
    assert_eq!(w.attributes().count(), 0);
    assert_eq!(w.node_types().count(), 0);
}

#[test]
fn attr_name_maps_ids_from_entries() {
    let (w, ents, _) = world();
    let mut seen = 0;
    for info in w.entries() {
        if let EntryBody::Fact { attr, .. } = info.body {
            let name = w.attr_name(attr).expect("attribute of a stored fact");
            assert!(w.attribute(name).is_some());
            seen += 1;
        }
    }
    assert_eq!(seen, 2);
    let _ = ents;
    // ID order == declaration order, and out-of-range is None.
    for (i, a) in w.attributes().enumerate() {
        assert_eq!(w.attr_name(AttrId(i as u32)), Some(a.name.as_str()));
    }
    let n = w.attributes().count() as u32;
    assert_eq!(w.attr_name(AttrId(n)), None);
    assert_eq!(w.attr_name(AttrId(u32::MAX)), None);
}

#[test]
fn attr_name_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    {
        let mut w = World::create(&p, schema()).unwrap();
        let c = w.create_entity("CHARACTER", None).unwrap();
        w.assert(c, "name", sv("Edwin"), t(1)).unwrap();
    }
    let w = World::open(&p).unwrap();
    let attr = w
        .entries()
        .find_map(|e| match e.body {
            EntryBody::Fact { attr, .. } => Some(attr),
            _ => None,
        })
        .unwrap();
    assert_eq!(w.attr_name(attr), Some("name"));
}

// ---------- entries ----------

fn assert_entries_match_entry(w: &World) {
    let all: Vec<EntryInfo> = w.entries().collect();
    for (i, info) in all.iter().enumerate() {
        assert_eq!(info.id, log::EntryId(i as u64 + first_id(w)), "dense IDs");
        assert_eq!(w.entry(info.id), Some(info.clone()));
    }
    // nothing past the last
    let next = all.last().map_or(first_id(w), |l| l.id.0 + 1);
    assert_eq!(w.entry(log::EntryId(next)), None);
}

/// The ID of the first entry (the log's numbering base).
fn first_id(w: &World) -> u64 {
    w.entries().next().map_or(0, |e| e.id.0)
}

#[test]
fn entries_equal_the_entry_by_id_sequence() {
    let (mut w, ents, _) = world();
    w.assert(ents[0], "name", sv("Edwin"), t(5)).unwrap();
    let id = w.assert(ents[0], "name", sv("Grey"), t(9)).unwrap().entries[0];
    w.correct(id, Correction::Delete).unwrap();
    assert_entries_match_entry(&w);
    let all: Vec<EntryInfo> = w.entries().collect();
    assert!(
        all.iter()
            .any(|e| matches!(e.body, EntryBody::DefineEntity { .. }))
    );
    assert!(
        all.iter()
            .any(|e| matches!(e.body, EntryBody::DefineAttr { .. }))
    );
    assert!(
        all.iter()
            .any(|e| matches!(e.body, EntryBody::DefineEdge { .. }))
    );
    assert!(
        all.iter()
            .any(|e| matches!(e.body, EntryBody::Deleted { .. }))
    );
    assert!(
        all.iter()
            .any(|e| matches!(e.body, EntryBody::Commit { .. }))
    );
    // tx is non-decreasing and ends at latest_tx
    assert!(all.windows(2).all(|p| p[0].tx <= p[1].tx));
    assert_eq!(all.last().unwrap().tx, w.latest_tx());
    assert!(matches!(all.last().unwrap().body, EntryBody::Commit { .. }));
}

#[test]
fn entries_of_a_fresh_world_are_the_genesis_transaction() {
    let w = World::in_memory(schema()).unwrap();
    let all: Vec<EntryInfo> = w.entries().collect();
    // 3 node types + 6 attributes + Commit
    assert_eq!(all.len(), 10);
    assert!(matches!(all.last().unwrap().body, EntryBody::Commit { .. }));
    assert_entries_match_entry(&w);
}

#[test]
fn entries_exclude_failed_and_dropped_transactions() {
    let (mut w, ents, _) = world();
    let before: Vec<EntryInfo> = w.entries().collect();
    // failed write (unknown attribute)
    assert!(w.assert(ents[0], "nope", sv("x"), t(1)).is_err());
    // no-op write (0 changed)
    w.retract(ents[0], "name", sv("never"), t(1)).unwrap();
    // dropped transaction
    {
        let mut txn = w.begin();
        txn.create_entity("PLACE", Some("ghost")).unwrap();
        txn.assert(ents[0], "name", sv("Dropped"), t(3)).unwrap();
        drop(txn);
    }
    let after: Vec<EntryInfo> = w.entries().collect();
    assert_eq!(before, after);
}

#[test]
fn entries_count_matches_read_log_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    {
        let mut w = World::create(&p, schema()).unwrap();
        let a = w.create_entity("CHARACTER", Some("a")).unwrap();
        let b = w.create_entity("PLACE", None).unwrap();
        w.assert(a, "located_in", Value::Ref(b), t(-5)).unwrap();
        let r = w.assert(a, "name", sv("A"), t(3)).unwrap();
        w.correct(
            r.entries[0],
            Correction::Replace {
                value: None,
                tick: t(7),
                op: None,
                precision: None,
            },
        )
        .unwrap();
    }
    let w = World::open(&p).unwrap();
    let log = read_log(&p).unwrap();
    let ents: Vec<EntryInfo> = w.entries().collect();
    assert_eq!(ents.len(), log.len());
    for (a, b) in ents.iter().zip(&log) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.body, b.body);
    }
    assert_entries_match_entry(&w);
}

// ---------- View::latest_tick ----------

#[test]
fn view_latest_tick_at_latest_canon_equals_world_latest_tick() {
    let (w, _, _) = world();
    assert_eq!(w.at_canon(w.latest_tx()).latest_tick(), w.latest_tick());
    let empty = World::in_memory(schema()).unwrap();
    assert_eq!(empty.at_canon(empty.latest_tx()).latest_tick(), Tick(0));
    assert_eq!(empty.at_canon(TxId(0)).latest_tick(), Tick(0));
}

#[test]
fn view_latest_tick_follows_canon_through_prequels_and_corrections() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = w.create_entity("CHARACTER", None).unwrap();
    let tx_genesis_and_entity = w.latest_tx();
    let r1 = w.assert(c, "name", sv("a"), t(-300)).unwrap();
    let tx1 = w.latest_tx();
    let r2 = w.assert(c, "name", sv("b"), t(500)).unwrap();
    let tx2 = w.latest_tx();
    w.assert(c, "name", sv("c"), t(-900)).unwrap();
    let tx3 = w.latest_tx();
    w.correct(
        r2.entries[0],
        Correction::Replace {
            value: None,
            tick: t(100),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let tx4 = w.latest_tx();
    let _ = r1;
    // each write is its own tx, in order
    assert!(tx_genesis_and_entity < tx1 && tx1 < tx2 && tx2 < tx3 && tx3 < tx4);

    assert_eq!(w.at_canon(tx_genesis_and_entity).latest_tick(), Tick(0));
    assert_eq!(w.at_canon(TxId(0)).latest_tick(), Tick(0));
    assert_eq!(w.at_canon(tx1).latest_tick(), Tick(-300));
    assert_eq!(w.at_canon(tx2).latest_tick(), Tick(500));
    assert_eq!(w.at_canon(tx3).latest_tick(), Tick(500));
    assert_eq!(w.at_canon(tx4).latest_tick(), Tick(100));
    assert_eq!(w.latest_tick(), Tick(100));
    // beyond latest clamps
    assert_eq!(w.at_canon(TxId(tx4.0 + 10)).latest_tick(), Tick(100));
}

#[test]
fn view_latest_tick_ignores_a_fact_deleted_at_that_canon() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = w.create_entity("CHARACTER", None).unwrap();
    w.assert(c, "name", sv("a"), t(10)).unwrap();
    let late = w.assert(c, "name", sv("b"), t(99)).unwrap();
    let before = w.latest_tx();
    w.correct(late.entries[0], Correction::Delete).unwrap();
    assert_eq!(w.at_canon(before).latest_tick(), Tick(99));
    assert_eq!(w.at_canon(w.latest_tx()).latest_tick(), Tick(10));
}

#[test]
fn view_latest_tick_counts_retracts_and_extreme_ticks() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = w.create_entity("CHARACTER", None).unwrap();
    w.assert(c, "name", sv("a"), t(i64::MIN)).unwrap();
    assert_eq!(w.at_canon(w.latest_tx()).latest_tick(), Tick(i64::MIN));
    w.retract(c, "name", sv("a"), t(-5)).unwrap();
    assert_eq!(w.at_canon(w.latest_tx()).latest_tick(), Tick(-5));
    w.assert(c, "name", sv("z"), t(i64::MAX)).unwrap();
    assert_eq!(w.at_canon(w.latest_tx()).latest_tick(), Tick(i64::MAX));
}

// ---------- property: View::latest_tick vs. a naive replay of entries() ----------

#[derive(Clone, Debug)]
enum Step {
    Assert(u8, u8, i64),
    Retract(u8, u8, i64),
    Delete(u8),
    Move(u8, i64),
}

fn tick_strategy() -> impl Strategy<Value = i64> {
    prop_oneof![-5i64..5, Just(i64::MIN), Just(i64::MAX), -1000i64..1000]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u8..2, 0u8..3, tick_strategy()).prop_map(|(e, v, k)| Step::Assert(e, v, k)),
        (0u8..2, 0u8..3, tick_strategy()).prop_map(|(e, v, k)| Step::Retract(e, v, k)),
        any::<u8>().prop_map(Step::Delete),
        (any::<u8>(), tick_strategy()).prop_map(|(i, k)| Step::Move(i, k)),
    ]
}

/// Naive: the max tick over the chains whose current revision, among
/// entries with tx <= n, is a fact (not deleted).
fn naive_latest_tick(w: &World, n: TxId) -> Tick {
    let mut chains: BTreeMap<u64, Option<Tick>> = BTreeMap::new();
    for e in w.entries().filter(|e| e.tx <= n) {
        match e.body {
            EntryBody::Fact { tick, .. } => {
                chains.insert(e.id.0, Some(tick));
            }
            EntryBody::Deleted { of, .. } => {
                chains.insert(of.0, None);
            }
            EntryBody::Revised { of, tick, .. } => {
                chains.insert(of.0, Some(tick));
            }
            _ => {}
        }
    }
    chains.values().flatten().copied().max().unwrap_or(Tick(0))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn view_latest_tick_matches_naive_replay_at_every_canon(
        steps in proptest::collection::vec(step(), 0..25)
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let ents = [
            w.create_entity("CHARACTER", None).unwrap(),
            w.create_entity("CHARACTER", None).unwrap(),
        ];
        let vals = ["a", "b", "c"];
        let mut written: Vec<log::EntryId> = Vec::new();
        for s in steps {
            // Errors (same-tick conflicts, double deletes) are fine: they
            // change nothing.
            match s {
                Step::Assert(e, v, k) => {
                    if let Ok(r) = w.assert(ents[e as usize], "tag_like", sv(vals[v as usize]), t(k)) {
                        written.extend(r.entries);
                    }
                }
                Step::Retract(e, v, k) => {
                    if let Ok(r) = w.retract(ents[e as usize], "tag_like", sv(vals[v as usize]), t(k)) {
                        written.extend(r.entries);
                    }
                }
                Step::Delete(i) => {
                    if !written.is_empty() {
                        let id = written[i as usize % written.len()];
                        let _ = w.correct(id, Correction::Delete);
                    }
                }
                Step::Move(i, k) => {
                    if !written.is_empty() {
                        let id = written[i as usize % written.len()];
                        let _ = w.correct(id, Correction::Replace {
                            value: None, tick: t(k), op: None, precision: None,
                        });
                    }
                }
            }
        }
        for n in 0..=w.latest_tx().0 + 1 {
            let n = TxId(n);
            let canon = n.min(w.latest_tx());
            prop_assert_eq!(
                w.at_canon(n).latest_tick(),
                naive_latest_tick(&w, canon),
                "canon {:?}", n
            );
        }
        prop_assert_eq!(w.at_canon(w.latest_tx()).latest_tick(), w.latest_tick());
    }
}
