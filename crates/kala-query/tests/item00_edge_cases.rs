//! Slice 7, item 00 confirm: edge cases for the step budget at an older
//! canon, the byte budget on grouped `(at ?t ..)` scans and on `Union`.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, CanonSource, Limits, Op, Params, QueryError, run};

const KIB: usize = 1024;

fn limits(max_steps: u64, max_bytes: usize) -> Limits {
    Limits {
        max_rows: 1_000_000,
        max_steps,
        max_bytes,
    }
}

fn wide_schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "rare",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "year",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
}

/// 5,000 entities in one transaction, then a later write so that canon `N`
/// (the first tx) is older than the latest. Returns (world, older canon).
fn world_with_older_canon() -> (World, u64) {
    let mut w = World::in_memory(wide_schema()).unwrap();
    let mut t = w.begin();
    let mut last = None;
    for _ in 0..5_000 {
        last = Some(t.create_entity("CHARACTER", None).unwrap());
    }
    t.assert(last.unwrap(), "rare", Value::I64(7), Some(Tick(0)))
        .unwrap();
    t.commit().unwrap();
    let older = w.latest_tx().0;
    let e = w.create_entity("CHARACTER", None).unwrap();
    w.assert(e, "rare", Value::I64(8), Some(Tick(5))).unwrap();
    assert!(w.latest_tx().0 > older);
    (w, older)
}

#[test]
fn attr_scan_at_an_older_canon_exhausts_a_small_step_budget() {
    let (w, older) = world_with_older_canon();
    let mut p = plan(
        &["e", "v"],
        expand(tv("e"), "rare", tv("v"), at(0), Access::Scan),
    );
    p.canon = CanonSource::Const(older);
    match run(&w, &p, &Params::new(), &limits(1_000, 1 << 30)) {
        Err(QueryError::StepBudget { limit: 1_000 }) => {}
        other => panic!("expected StepBudget, got {:?}", other.map(|r| r.rows)),
    }
    let rs = run(&w, &p, &Params::new(), &limits(10_000_000, 1 << 30)).expect("fits");
    assert_eq!(rs.rows.len(), 1);
}

#[test]
fn default_tick_walk_at_an_older_canon_is_metered() {
    // A one-entity read at the default tick at an older canon needs only the
    // canon's latest tick (a walk of the E index over 3,000 facts); that
    // walk alone must exhaust a small step budget.
    let mut w = World::in_memory(wide_schema()).unwrap();
    let mut t = w.begin();
    let mut first = None;
    for i in 0..3_000 {
        let e = t.create_entity("CHARACTER", None).unwrap();
        t.assert(e, "year", Value::I64(i), Some(Tick(i))).unwrap();
        first.get_or_insert(e);
    }
    t.commit().unwrap();
    let older = w.latest_tx().0;
    w.assert(first.unwrap(), "year", Value::I64(-1), Some(Tick(10_000)))
        .unwrap();
    let mut p = plan(
        &["v"],
        expand(
            tr(first.unwrap()),
            "year",
            tv("v"),
            kala_query::TickSource::Default,
            Access::Scan,
        ),
    );
    p.canon = CanonSource::Const(older);
    match run(&w, &p, &Params::new(), &limits(500, 1 << 30)) {
        Err(QueryError::StepBudget { .. }) => {}
        other => panic!("expected StepBudget, got {:?}", other.map(|r| r.rows)),
    }
    run(&w, &p, &Params::new(), &limits(10_000_000, 1 << 30)).expect("fits");
}

/// `n` characters, character `i` with year `i` and a `size`-byte bio.
fn h1_world(n: i64, size: usize) -> World {
    let mut w = World::in_memory(wide_schema()).unwrap();
    let mut t = w.begin();
    for i in 0..n {
        let e = t.create_entity("CHARACTER", None).unwrap();
        t.assert(e, "year", Value::I64(i), Some(Tick(0))).unwrap();
        t.assert(e, "bio", Value::Str("x".repeat(size)), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

/// `(at ?t [?x :bio ?y])` over 60 distinct ticks, each scan returning 60
/// 8 KB strings (~500 KB per scan; ~30 MB if every scan stayed charged).
/// Measured need is ~510 KB, so 2.5 MiB is >= 4x the need and < 1/4 of keeping all.
#[test]
fn h1_per_tick_scans_are_not_all_kept_charged() {
    let w = h1_world(60, 8_000);
    let q = |_: ()| {
        let scan = Op::Expand {
            input: b(expand(tv("a"), "year", tv("t"), at(0), Access::Scan)),
            entity: tv("x"),
            attr: kala_query::AttrRef::Named("bio".into()),
            value: tv("y"),
            tick: kala_query::TickSource::Var(v("t")),
            access: Access::Scan,
        };
        let f = filter(scan, kala_query::CmpOp::Eq, tv("x"), tv("a"));
        plan(&["a"], project(f, &["a"]))
    };
    let p = q(());
    // Control: succeeds with the default budget.
    let rs = run(&w, &p, &Params::new(), &Limits::default()).expect("control");
    assert_eq!(rs.rows.len(), 60);
    let rs = run(&w, &p, &Params::new(), &limits(100_000_000, 2560 * KIB))
        .unwrap_or_else(|e| panic!("one scan at a time must fit 2.5 MiB: {e}"));
    assert_eq!(rs.rows.len(), 60);
}

#[test]
fn union_inputs_together_exceed_the_byte_budget() {
    // Eight distinct unindexed attributes, 100 entities x 2 KB each: every
    // branch is ~220 KB of distinct rows, eight together ~1.8 MB.
    let mut schema = Schema::new().node_type("CHARACTER");
    for i in 0..8 {
        schema = schema.attribute(
            AttrDef::new(
                &format!("bio{i}"),
                ValueType::Str,
                Kind::State,
                Cardinality::One,
            )
            .unindexed(),
        );
    }
    let mut w = World::in_memory(schema).unwrap();
    let mut t = w.begin();
    for _ in 0..100 {
        let e = t.create_entity("CHARACTER", None).unwrap();
        for i in 0..8 {
            t.assert(
                e,
                &format!("bio{i}"),
                Value::Str(format!("{i}{}", "x".repeat(2_000))),
                Some(Tick(0)),
            )
            .unwrap();
        }
    }
    t.commit().unwrap();
    let branch = |i: usize| expand(tv("e"), &format!("bio{i}"), tv("s"), at(0), Access::Scan);
    let one = plan(&["e", "s"], branch(0));
    let p = plan(
        &["e", "s"],
        Op::Union {
            inputs: (0..8).map(branch).collect(),
        },
    );
    // Controls: one branch fits 512 KiB, and the union fits the default.
    run(&w, &one, &Params::new(), &limits(100_000_000, 512 * KIB)).expect("one branch fits");
    let rs = run(&w, &p, &Params::new(), &Limits::default()).expect("union fits default");
    assert_eq!(rs.rows.len(), 800);
    match run(&w, &p, &Params::new(), &limits(100_000_000, 512 * KIB)) {
        Err(QueryError::MemoryBudget { limit }) => assert_eq!(limit, 512 * KIB),
        other => panic!(
            "expected MemoryBudget, got {:?}",
            other.map(|r| r.rows.len())
        ),
    }
}
