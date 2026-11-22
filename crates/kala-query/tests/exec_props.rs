//! Slice 6, item 2: property tests for the executor against the native
//! kala-core API on random small worlds. Needs `View::attr_scan` and
//! `Value: Ord` (kala-core item 0) and `kala_query::{execute, Params, Limits}`.

mod common;

use common::*;
use kala_core::{
    AttrDef, Cardinality, Correction, Direction, EntityId, EntryId, Kind, Schema, Tick, TxId,
    Value, ValueType, View, World,
};
use kala_query::{Access, Limits, Params, Plan, ResultSet, TickSource, execute};
use proptest::prelude::*;

const N_ENT: usize = 5;

#[derive(Clone, Debug)]
enum Step {
    Assert {
        e: usize,
        attr: usize,
        val: i64,
        tick: i64,
    },
    Retract {
        e: usize,
        attr: usize,
        val: i64,
        tick: i64,
    },
    /// Delete the k-th (mod count) entry stored so far.
    Delete { k: usize },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0..N_ENT, 0..3usize, 0..N_ENT as i64, -4..5i64)
            .prop_map(|(e, attr, val, tick)| Step::Assert { e, attr, val, tick }),
        2 => (0..N_ENT, 0..3usize, 0..N_ENT as i64, -4..5i64)
            .prop_map(|(e, attr, val, tick)| Step::Retract { e, attr, val, tick }),
        1 => (0..100usize).prop_map(|k| Step::Delete { k }),
    ]
}

const ATTRS: [&str; 3] = ["likes", "mutual", "score"];

fn build(steps: &[Step]) -> (World, Vec<EntityId>) {
    let schema = Schema::new()
        .node_type("A")
        .node_type("B")
        .attribute(AttrDef::new(
            "likes",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("mutual", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(AttrDef::new(
            "score",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ));
    let mut w = World::in_memory(schema).unwrap();
    let mut ents = Vec::new();
    for i in 0..N_ENT {
        ents.push(
            w.create_entity(if i % 2 == 0 { "A" } else { "B" }, None)
                .unwrap(),
        );
    }
    let mut entries: Vec<EntryId> = Vec::new();
    let value_for = |attr: usize, val: i64| -> Value {
        if attr == 2 {
            Value::I64(val)
        } else {
            Value::Ref(ents[val as usize])
        }
    };
    for s in steps {
        // same-tick conflicts and the like are errors: ignore, the model is the store
        let res = match s {
            Step::Assert { e, attr, val, tick } => w.assert(
                ents[*e],
                ATTRS[*attr],
                value_for(*attr, *val),
                Some(Tick(*tick)),
            ),
            Step::Retract { e, attr, val, tick } => w.retract(
                ents[*e],
                ATTRS[*attr],
                value_for(*attr, *val),
                Some(Tick(*tick)),
            ),
            Step::Delete { k } => {
                if entries.is_empty() {
                    continue;
                }
                w.correct(entries[k % entries.len()], Correction::Delete)
            }
        };
        if let Ok(r) = res {
            entries.extend(r.entries);
        }
    }
    (w, ents)
}

fn run_plan(view: &View<'_>, p: &Plan) -> ResultSet {
    execute(view, p, &Params::new(), &Limits::default()).expect("plan runs")
}

/// An entity read at a canon before its DefineEntity tx is
/// `Error::UnknownEntity` (kala-core slice-6 item 00b): the oracle treats that
/// (and only that) as "no values".
fn unknown_is_empty<T>(r: Result<Vec<T>, kala_core::Error>) -> Vec<T> {
    match r {
        Ok(v) => v,
        Err(kala_core::Error::UnknownEntity(..)) => Vec::new(),
        Err(e) => panic!("native read failed: {e:?}"),
    }
}

/// Native pairs `(e, v)` of `attr` at `t`, from per-entity reads (not attr_scan).
fn native_pairs(view: &View<'_>, ents: &[EntityId], attr: &str, t: i64) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for &e in ents {
        match attr {
            "score" => {
                for x in unknown_is_empty(view.as_of(e, attr, Tick(t))) {
                    out.push(vec![Value::Ref(e), x]);
                }
            }
            _ => {
                let dir = if attr == "mutual" {
                    Direction::Both
                } else {
                    Direction::Out
                };
                for (n, _) in unknown_is_empty(view.neighbors(e, attr, dir, Tick(t))) {
                    out.push(vec![Value::Ref(e), Value::Ref(n)]);
                }
            }
        }
    }
    sorted_rows(out)
}

fn pair_plan(attr: &str, t: i64, access: Access) -> Plan {
    plan(
        &["e", "v"],
        project(
            expand(tv("e"), attr, tv("v"), TickSource::Const(Tick(t)), access),
            &["e", "v"],
        ),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// (1) per-entity E reads, (2) full Scan, against the native API, at a
    /// random canon and tick.
    #[test]
    fn expand_matches_native_reads(
        steps in proptest::collection::vec(step(), 0..40),
        canon in 0u64..60,
        t in -6i64..7,
    ) {
        let (w, ents) = build(&steps);
        let view = w.at_canon(TxId(canon));
        for attr in ATTRS {
            let expected = native_pairs(&view, &ents, attr, t);
            // E, entity by entity
            let mut via_e = Vec::new();
            for &e in &ents {
                let p = plan(&["v"], project(
                    expand(tr(e), attr, tv("v"), TickSource::Const(Tick(t)), Access::E), &["v"]));
                for r in run_plan(&view, &p).rows {
                    via_e.push(vec![Value::Ref(e), r[0].clone()]);
                }
            }
            prop_assert_eq!(&sorted_rows(via_e), &expected, "E {} canon {} t {}", attr, canon, t);
            // Scan
            let scan = run_plan(&view, &pair_plan(attr, t, Access::Scan));
            prop_assert_eq!(&scan.rows, &expected, "Scan {} canon {} t {}", attr, canon, t);
            // Scan agrees with kala-core's attr_scan
            let mut direct: Vec<Vec<Value>> = view.attr_scan(attr, Tick(t)).unwrap()
                .into_iter().map(|(e, v)| vec![Value::Ref(e), v]).collect();
            direct.sort();
            direct.dedup();
            prop_assert_eq!(&scan.rows, &direct, "attr_scan {} canon {} t {}", attr, canon, t);
        }
    }

    /// (3) With one side bound to a constant, every feasible access gives the
    /// same rows.
    #[test]
    fn feasible_accesses_agree(
        steps in proptest::collection::vec(step(), 0..40),
        canon in 0u64..60,
        t in -6i64..7,
        bound in 0..N_ENT,
        bound_val in -1i64..5,
    ) {
        let (w, ents) = build(&steps);
        let view = w.at_canon(TxId(canon));
        let c = ents[bound];
        for attr in ["likes", "mutual"] {
            let tk = TickSource::Const(Tick(t));
            // entity bound: E vs Scan
            let e_side = |access| run_plan(&view, &plan(&["v"], project(
                expand(tr(c), attr, tv("v"), tk.clone(), access), &["v"]))).rows;
            prop_assert_eq!(e_side(Access::E), e_side(Access::Scan), "{} entity bound", attr);
            // value (ref) bound: R vs Scan
            let v_side = |access| run_plan(&view, &plan(&["e"], project(
                expand(tv("e"), attr, tr(c), tk.clone(), access), &["e"]))).rows;
            prop_assert_eq!(v_side(Access::R), v_side(Access::Scan), "{} value bound", attr);
        }
        // i64 attribute: entity bound E vs Scan; value bound V vs Scan
        let tk = TickSource::Const(Tick(t));
        let e_side = |access| run_plan(&view, &plan(&["v"], project(
            expand(tr(c), "score", tv("v"), tk.clone(), access), &["v"]))).rows;
        prop_assert_eq!(e_side(Access::E), e_side(Access::Scan));
        let v_side = |access| run_plan(&view, &plan(&["e"], project(
            expand(tv("e"), "score", ti(bound_val), tk.clone(), access), &["e"]))).rows;
        prop_assert_eq!(v_side(Access::V), v_side(Access::Scan));
        // and the V answer equals the native lookup
        let expected = ref_rows(view.lookup("score", &Value::I64(bound_val), Tick(t)).unwrap());
        prop_assert_eq!(col0(&run_plan(&view, &plan(&["e"], project(
            expand(tv("e"), "score", ti(bound_val), tk.clone(), Access::V), &["e"])))), expected);
    }

    /// (4) A two-pattern Join equals a naive nested loop over native reads,
    /// with the two patterns at different ticks.
    #[test]
    fn join_matches_nested_loop(
        steps in proptest::collection::vec(step(), 0..40),
        canon in 0u64..60,
        t1 in -6i64..7,
        t2 in -6i64..7,
    ) {
        let (w, ents) = build(&steps);
        let view = w.at_canon(TxId(canon));
        let left = native_pairs(&view, &ents, "likes", t1);   // (x, y)
        let right = native_pairs(&view, &ents, "mutual", t2); // (y, z)
        let mut expected = Vec::new();
        for l in &left {
            for r in &right {
                if l[1] == r[0] {
                    expected.push(vec![l[0].clone(), l[1].clone(), r[1].clone()]);
                }
            }
        }
        let expected = sorted_rows(expected);
        let mk = |a1, a2| plan(&["x", "y", "z"], project(
            join(
                expand(tv("x"), "likes", tv("y"), TickSource::Const(Tick(t1)), a1),
                expand(tv("y"), "mutual", tv("z"), TickSource::Const(Tick(t2)), a2),
            ),
            &["x", "y", "z"],
        ));
        prop_assert_eq!(run_plan(&view, &mk(Access::Scan, Access::Scan)).rows, expected.clone());
        // nested form: the second Expand runs per row of the first, entity bound (E)
        let nested = plan(&["x", "y", "z"], project(
            expand_on(
                expand(tv("x"), "likes", tv("y"), TickSource::Const(Tick(t1)), Access::Scan),
                tv("y"), "mutual", tv("z"), TickSource::Const(Tick(t2)), Access::E),
            &["x", "y", "z"]));
        prop_assert_eq!(run_plan(&view, &nested).rows, expected);
    }

    /// Results never depend on the plan being run twice, and rows are sorted
    /// and duplicate-free.
    #[test]
    fn rows_sorted_unique_and_stable(
        steps in proptest::collection::vec(step(), 0..40),
        t in -6i64..7,
    ) {
        let (w, _ents) = build(&steps);
        let view = w.at_canon(w.latest_tx());
        let p = pair_plan("mutual", t, Access::Scan);
        let r1 = run_plan(&view, &p);
        let r2 = run_plan(&view, &p);
        prop_assert_eq!(&r1.rows, &r2.rows);
        let mut s = r1.rows.clone();
        s.sort();
        s.dedup();
        prop_assert_eq!(&r1.rows, &s);
        // bidirectional: symmetric
        for r in &r1.rows {
            prop_assert!(r1.rows.contains(&vec![r[1].clone(), r[0].clone()]));
        }
    }
}
