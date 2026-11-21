//! Slice 2, item 5: the R index (docs/slice-2-decisions.md, "Item 5").
//! `R | target | attr | source | ^tick | ^tx -> op` is written for every ref
//! fact in the same commit as its E entry. The only observable surface is the
//! hidden `World::index_dump() -> IndexDump` (re-exported at the root):
//! `e` rows `(entity, attr, value, tick, tx, op)` and `r` rows
//! `(target, attr, source, tick, tx, op)`, both in key order (E: entity, attr,
//! value encoding, then newest tick first, newest tx first; R: target, attr,
//! source, then the same). Expected derives on IndexDump: Debug, Clone,
//! PartialEq. AttrId is the attribute's declaration index in the schema.
//! Directed attributes only: bidirectional storage is item 7.

use kala_core::*;
use proptest::prelude::*;
use std::path::PathBuf;

type ERow = (EntityId, AttrId, Value, Tick, TxId, Op);
type RRow = (EntityId, AttrId, EntityId, Tick, TxId, Op);

const LOCATED_IN: AttrId = AttrId(0);
const BOUGHT: AttrId = AttrId(1);
const WITNESS: AttrId = AttrId(4);
const NOTE: AttrId = AttrId(5);

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(a("bought", ValueType::Ref, Kind::Event, Cardinality::Many))
        .attribute(a("age", ValueType::I64, Kind::State, Cardinality::Many))
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("witness", ValueType::Ref, Kind::State, Cardinality::Many))
        .attribute(a("note", ValueType::Str, Kind::State, Cardinality::One))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn ent_key(e: EntityId) -> [u8; 8] {
    e.to_bytes()
}

fn e_key(
    r: &ERow,
) -> (
    Vec<u8>,
    u32,
    Vec<u8>,
    std::cmp::Reverse<i64>,
    std::cmp::Reverse<u64>,
) {
    (
        ent_key(r.0).to_vec(),
        (r.1).0,
        r.2.encode().unwrap(),
        std::cmp::Reverse((r.3).0),
        std::cmp::Reverse((r.4).0),
    )
}

fn r_key(
    r: &RRow,
) -> (
    Vec<u8>,
    u32,
    Vec<u8>,
    std::cmp::Reverse<i64>,
    std::cmp::Reverse<u64>,
) {
    (
        ent_key(r.0).to_vec(),
        (r.1).0,
        ent_key(r.2).to_vec(),
        std::cmp::Reverse((r.3).0),
        std::cmp::Reverse((r.4).0),
    )
}

/// Rows are strictly in key order.
fn assert_key_order(d: &IndexDump) {
    for w in d.e.windows(2) {
        assert!(
            e_key(&w[0]) < e_key(&w[1]),
            "E rows out of order: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
    for w in d.r.windows(2) {
        assert!(
            r_key(&w[0]) < r_key(&w[1]),
            "R rows out of order: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
}

/// Every ref E row has exactly one R mirror, every R row has its E row, and
/// non-ref E rows have none.
fn assert_mirror(d: &IndexDump) {
    let mut expected: Vec<RRow> =
        d.e.iter()
            .filter_map(|row: &ERow| match &row.2 {
                Value::Ref(target) => Some((*target, row.1, row.0, row.3, row.4, row.5)),
                _ => None,
            })
            .collect();
    expected.sort_by_key(r_key);
    assert_eq!(
        d.r, expected,
        "R must be exactly the ref rows of E, re-keyed"
    );
    assert_key_order(d);
}

fn dump_sorted_ok(w: &World) -> IndexDump {
    let d = w.index_dump();
    assert_mirror(&d);
    d
}

struct Fx {
    w: World,
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    Fx {
        w,
        greg,
        edwin,
        varrow,
        ashford,
    }
}

#[test]
fn empty_world_has_empty_dump() {
    let f = fx();
    let d = f.w.index_dump();
    assert!(d.e.is_empty());
    assert!(d.r.is_empty());
}

#[test]
fn claude_md_example_gives_three_r_rows_newest_tick_first() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), t(-250))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(-200))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    let tx0 = f.w.latest_tx().0 - 2; // three single-write txs
    assert_eq!(
        d.r,
        vec![
            (
                f.varrow,
                LOCATED_IN,
                f.greg,
                Tick(-200),
                TxId(tx0 + 2),
                Op::Assert
            ),
            (
                f.varrow,
                LOCATED_IN,
                f.greg,
                Tick(-250),
                TxId(tx0 + 1),
                Op::Retract
            ),
            (
                f.varrow,
                LOCATED_IN,
                f.greg,
                Tick(-300),
                TxId(tx0),
                Op::Assert
            ),
        ]
    );
    // E mirrors them, in the same newest-first order.
    let e_ticks: Vec<i64> = d.e.iter().map(|r| (r.3).0).collect();
    assert_eq!(e_ticks, vec![-200, -250, -300]);
}

#[test]
fn r_rows_group_by_target_then_attr_then_source() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), t(5))
        .unwrap();
    f.w.assert(f.edwin, "located_in", Value::Ref(f.varrow), t(5))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(7))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    assert_eq!(d.r.len(), 3);
    // Under varrow: sources in entity-ID order (greg < edwin), then ashford.
    let targets: Vec<EntityId> = d.r.iter().map(|r| r.0).collect();
    let mut sorted = targets.clone();
    sorted.sort_by_key(|e| e.to_bytes());
    assert_eq!(targets, sorted);
}

#[test]
fn extreme_ticks_order_correctly() {
    let mut f = fx();
    for x in [i64::MIN, -1, 0, 1, i64::MAX] {
        f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(x))
            .ok();
        f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), t(x))
            .ok();
    }
    let d = dump_sorted_ok(&f.w);
    assert!(!d.r.is_empty());
}

#[test]
fn non_ref_facts_have_no_r_rows() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(30), t(0)).unwrap();
    f.w.assert(f.greg, "name", Value::Str("Greg".into()), t(0))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    assert_eq!(d.e.len(), 2);
    assert!(d.r.is_empty());
}

#[test]
fn event_ref_attributes_mirror() {
    let mut f = fx();
    f.w.assert(f.edwin, "bought", Value::Ref(f.varrow), t(-208))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    assert_eq!(d.r.len(), 1);
    assert_eq!(d.r[0].0, f.varrow);
    assert_eq!(d.r[0].1, BOUGHT);
    assert_eq!(d.r[0].2, f.edwin);
    assert_eq!(d.r[0].3, Tick(-208));
    assert_eq!(d.r[0].5, Op::Assert);
}

#[test]
fn noop_writes_add_no_r_row() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(10))
        .unwrap();
    let before = f.w.index_dump();
    // already holds at tick 20; repeat at same tick; retract of absent value.
    assert_eq!(
        f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(20))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(10))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        f.w.retract(f.greg, "located_in", Value::Ref(f.ashford), t(10))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), t(5))
            .unwrap()
            .changed,
        0
    );
    let after = dump_sorted_ok(&f.w);
    assert_eq!(before, after);
    assert_eq!(after.r.len(), 1);
}

#[test]
fn same_tick_conflict_adds_no_row() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(10))
        .unwrap();
    let before = f.w.index_dump();
    let err =
        f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), t(10));
    assert!(
        matches!(err, Err(Error::SameTickConflict { .. })),
        "{err:?}"
    );
    assert_eq!(f.w.index_dump(), before);
}

#[test]
fn dropped_and_aborted_txns_add_no_row() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(10))
        .unwrap();
    let before = f.w.index_dump();
    {
        let mut tx = f.w.begin();
        tx.assert(f.edwin, "located_in", Value::Ref(f.ashford), t(1))
            .unwrap();
        tx.assert(f.edwin, "witness", Value::Ref(f.greg), t(1))
            .unwrap();
        // dropped without commit
    }
    assert_eq!(f.w.index_dump(), before);
    {
        let mut tx = f.w.begin();
        tx.assert(f.edwin, "located_in", Value::Ref(f.ashford), t(1))
            .unwrap();
        assert!(
            tx.assert(f.edwin, "no_such_attr", Value::Ref(f.ashford), t(1))
                .is_err()
        );
        assert!(tx.commit().is_err());
    }
    assert_eq!(f.w.index_dump(), before);
    dump_sorted_ok(&f.w);
}

#[test]
fn multi_write_txn_rows_share_one_tx() {
    let mut f = fx();
    let mut tx = f.w.begin();
    tx.assert(f.greg, "located_in", Value::Ref(f.varrow), t(1))
        .unwrap();
    tx.assert(f.edwin, "located_in", Value::Ref(f.varrow), t(2))
        .unwrap();
    tx.assert(f.edwin, "age", Value::I64(3), t(2)).unwrap();
    let cr = tx.commit().unwrap();
    let d = dump_sorted_ok(&f.w);
    assert_eq!(d.e.len(), 3);
    assert_eq!(d.r.len(), 2);
    assert!(d.e.iter().all(|r| r.4 == cr.tx));
    assert!(d.r.iter().all(|r| r.4 == cr.tx));
}

#[test]
fn ref_to_an_edge_target_mirrors() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(1))
        .unwrap();
    let edge: EntityId = f.w.edge_id(f.greg, "located_in", f.varrow).unwrap().into();
    // edwin witnessed the edge: value is an edge ID.
    f.w.assert(f.edwin, "witness", Value::Ref(edge), t(1))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    let row =
        d.r.iter()
            .find(|r| r.0 == edge)
            .expect("R row under the edge as target");
    assert_eq!(
        (row.1, row.2, row.3, row.5),
        (WITNESS, f.edwin, Tick(1), Op::Assert)
    );
}

#[test]
fn ref_with_an_edge_source_mirrors() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(1))
        .unwrap();
    let edge: EntityId = f.w.edge_id(f.greg, "located_in", f.varrow).unwrap().into();
    // edge property with a ref value: edge#1 witness edwin
    f.w.assert(edge, "witness", Value::Ref(f.edwin), t(1))
        .unwrap();
    f.w.retract(edge, "witness", Value::Ref(f.edwin), t(5))
        .unwrap();
    // and a non-ref edge property
    f.w.assert(edge, "note", Value::Str("secret".into()), t(1))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    let rows: Vec<&RRow> = d.r.iter().filter(|r| r.0 == f.edwin).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].1, rows[0].2, rows[0].3, rows[0].5),
        (WITNESS, edge, Tick(5), Op::Retract)
    );
    assert_eq!((rows[1].3, rows[1].5), (Tick(1), Op::Assert));
    assert!(d.e.iter().any(|r| r.1 == NOTE));
    assert!(d.r.iter().all(|r| r.1 != NOTE));
}

#[test]
fn out_of_order_prequel_write_sorts_into_place() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(200))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), t(-400))
        .ok(); // no-op? holds? no: later; stored
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), t(-100))
        .unwrap();
    let d = dump_sorted_ok(&f.w);
    let ticks: Vec<i64> = d.r.iter().map(|r| (r.3).0).collect();
    assert_eq!(ticks, vec![200, -100, -400]);
    // tx order is author order, not tick order
    assert_eq!(d.r[0].4, TxId(1 + 4));
}

// ---- reopen ----

fn path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("w.kala")
}

#[test]
fn dump_is_identical_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = path(&dir);
    let before;
    {
        let mut w = World::create(&p, schema()).unwrap();
        let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
        let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
        w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
            .unwrap();
        w.retract(greg, "located_in", Value::Ref(varrow), t(-250))
            .unwrap();
        w.assert(greg, "located_in", Value::Ref(ashford), t(-250))
            .unwrap();
        w.assert(greg, "age", Value::I64(9), t(0)).unwrap();
        let edge: EntityId = w.edge_id(greg, "located_in", ashford).unwrap().into();
        w.assert(edge, "witness", Value::Ref(greg), t(-250))
            .unwrap();
        w.assert(greg, "bought", Value::Ref(ashford), t(3)).unwrap();
        before = dump_sorted_ok(&w);
        assert!(!before.r.is_empty());
    }
    let w = World::open(&p).unwrap();
    let after = w.index_dump();
    assert_eq!(after.e, before.e);
    assert_eq!(after.r, before.r);
    assert_mirror(&after);
}

// ---- property test ----

#[derive(Clone, Debug)]
struct W {
    ent: usize,  // 0..3 characters
    attr: usize, // 0 located_in, 1 bought, 2 age, 3 name, 4 witness
    val: usize,  // 0..5 selects a value of the right type
    tick: i64,
    retract: bool,
}

fn write_strategy() -> impl Strategy<Value = W> {
    (
        0usize..3,
        0usize..5,
        0usize..5,
        prop_oneof![
            Just(i64::MIN),
            Just(-1i64),
            Just(0),
            Just(i64::MAX),
            -6i64..6
        ],
        any::<bool>(),
    )
        .prop_map(|(ent, attr, val, tick, retract)| W {
            ent,
            attr,
            val,
            tick,
            retract,
        })
}

fn txns() -> impl Strategy<Value = Vec<(Vec<W>, bool)>> {
    prop::collection::vec(
        (prop::collection::vec(write_strategy(), 1..4), 0u8..4),
        1..14,
    )
    .prop_map(|v| v.into_iter().map(|(ws, k)| (ws, k == 0)).collect())
}

struct Ids {
    chars: Vec<EntityId>,
    places: Vec<EntityId>,
}

fn value_for(ids: &Ids, w: &W) -> Value {
    match w.attr {
        0 | 1 | 4 => {
            let all: Vec<EntityId> = ids.places.iter().chain(ids.chars.iter()).copied().collect();
            Value::Ref(all[w.val % all.len()])
        }
        2 => Value::I64(w.val as i64),
        _ => Value::Str(format!("n{}", w.val)),
    }
}

const ATTR_NAMES: [&str; 5] = ["located_in", "bought", "age", "name", "witness"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn mirror_and_e_rows_match_model(seq in txns()) {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let mut w = World::create(&p, schema()).unwrap();
        let chars: Vec<EntityId> = (0..3)
            .map(|i| w.create_entity("CHARACTER", Some(&format!("c{i}"))).unwrap())
            .collect();
        let places: Vec<EntityId> = (0..2)
            .map(|i| w.create_entity("PLACE", Some(&format!("p{i}"))).unwrap())
            .collect();
        let ids = Ids { chars, places };
        let mut model: Vec<ERow> = Vec::new();

        for (writes, abort) in &seq {
            let mut txn = w.begin();
            let mut staged: Vec<ERow> = Vec::new();
            let mut failed = false;
            for x in writes {
                let v = value_for(&ids, x);
                let retract = x.retract && x.attr != 1; // events can't be retracted
                let ent = ids.chars[x.ent];
                let res = if retract {
                    txn.retract(ent, ATTR_NAMES[x.attr], v.clone(), Some(Tick(x.tick)))
                } else {
                    txn.assert(ent, ATTR_NAMES[x.attr], v.clone(), Some(Tick(x.tick)))
                };
                match res {
                    Ok(r) if r.changed == 1 => staged.push((
                        ent,
                        AttrId(x.attr as u32),
                        v,
                        Tick(x.tick),
                        TxId(0), // patched at commit
                        if retract { Op::Retract } else { Op::Assert },
                    )),
                    Ok(_) => {}
                    Err(_) => { failed = true; break; }
                }
            }
            if failed || *abort {
                drop(txn);
            } else {
                let cr = txn.commit().unwrap();
                if cr.changed > 0 {
                    for mut r in staged { r.4 = cr.tx; model.push(r); }
                }
            }
            let d = w.index_dump();
            assert_mirror(&d);
            let mut exp = model.clone();
            exp.sort_by_key(e_key);
            prop_assert_eq!(&d.e, &exp);
        }

        let before = w.index_dump();
        drop(w);
        let w2 = World::open(&p).unwrap();
        let after = w2.index_dump();
        assert_mirror(&after);
        prop_assert_eq!(after, before);
    }
}

// ---- property test: entities created in the same txn as the ref writes ----

#[derive(Clone, Debug)]
enum Step {
    NewChar,
    NewPlace,
    /// (source pick, attr 0 located_in / 1 bought / 2 witness, target pick, tick, retract)
    Ref(usize, usize, usize, i64, bool),
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        Just(Step::NewChar),
        Just(Step::NewPlace),
        (
            0usize..8,
            0usize..3,
            0usize..8,
            prop_oneof![Just(i64::MIN), Just(0), Just(i64::MAX), -4i64..4],
            any::<bool>()
        )
            .prop_map(|(s, a, t, k, r)| Step::Ref(s, a, t, k, r)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn mirror_holds_when_targets_are_created_in_the_same_txn(
        txns in prop::collection::vec(
            (prop::collection::vec(step_strategy(), 1..8), 0u8..4), 1..8)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let mut w = World::create(&p, schema()).unwrap();
        let seed = w.create_entity("CHARACTER", Some("seed")).unwrap();
        let mut known: Vec<EntityId> = vec![seed];
        let names = ["located_in", "bought", "witness"];
        for (steps, k) in &txns {
            let mut txn = w.begin();
            let mut staged_known = known.clone();
            let mut failed = false;
            for s in steps {
                let res = match s {
                    Step::NewChar => txn.create_entity("CHARACTER", None).map(|e| staged_known.push(e)),
                    Step::NewPlace => txn.create_entity("PLACE", None).map(|e| staged_known.push(e)),
                    Step::Ref(si, a, ti, tick, retract) => {
                        let src = staged_known[si % staged_known.len()];
                        let tgt = staged_known[ti % staged_known.len()];
                        let v = Value::Ref(tgt);
                        let tk = Some(Tick(*tick));
                        // events can't be retracted
                        if *retract && *a != 1 {
                            txn.retract(src, names[*a], v, tk).map(|_| ())
                        } else {
                            txn.assert(src, names[*a], v, tk).map(|_| ())
                        }
                    }
                };
                if res.is_err() { failed = true; break; }
            }
            if failed || *k == 0 {
                drop(txn);
            } else {
                txn.commit().unwrap();
                known = staged_known;
            }
            let d = w.index_dump();
            assert_mirror(&d);
        }
        let before = w.index_dump();
        drop(w);
        let w2 = World::open(&p).unwrap();
        let after = w2.index_dump();
        assert_mirror(&after);
        prop_assert_eq!(after, before);
    }
}
