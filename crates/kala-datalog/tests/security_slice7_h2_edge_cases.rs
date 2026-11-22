//! Slice 7 security finding H2 (CONFIRM): edge cases of the per-query read memo
//! (eviction under a tight byte budget, per-row ticks, inverse names, bound
//! string lookups, and memoized-vs-naive equivalence). The 30 s guard is a
//! hang guard, not a timing assertion; a worker that dies is reported as such.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, ResultSet, run};
use proptest::prelude::*;
use std::sync::mpsc;
use std::time::Duration;

const GUARD: Duration = Duration::from_secs(30);

fn many(n: &str, t: ValueType) -> AttrDef {
    AttrDef::new(n, t, Kind::State, Cardinality::Many)
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(many("bio", ValueType::Str).unindexed())
        .attribute(many("name", ValueType::Str))
        .attribute(many("q", ValueType::I64))
        .attribute(many("p", ValueType::Ref))
        .attribute(many("parent_of", ValueType::Ref).inverse("child_of"))
}

fn exec(w: &World, q: &str, lim: &Limits) -> Result<ResultSet, QueryError> {
    let p = prepare(w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
    run(w, &p.plan, &Params::new(), lim)
}

/// Runs `build` + query in a worker; distinguishes hang from a dead worker.
fn guarded<F>(build: F, q: &'static str, lim: Limits) -> Result<ResultSet, QueryError>
where
    F: FnOnce() -> World + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let w = build();
        let _ = tx.send(exec(&w, q, &lim));
    });
    match rx.recv_timeout(GUARD) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("HANG GUARD: still running after {GUARD:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("worker died (build/prepare failed)"),
    }
}

fn count(rs: &ResultSet) -> String {
    assert_eq!(rs.rows.len(), 1);
    format!("{:?}", rs.rows[0][0])
}

const BIO: usize = 200 * 1024;
const ROWS: usize = 2_000;

fn world_bio() -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(a, "bio", Value::Str("x".repeat(BIO)), Some(Tick(0)))
        .unwrap();
    let mut t = w.begin();
    for i in 0..ROWS {
        t.assert(b, "q", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

#[test]
fn eviction_under_tight_budget_keeps_query_ok_and_same_answer() {
    // Budget choice: bisect for the smallest max_bytes at which the query is Ok.
    // That budget fits the data (one copy of the bio, interned, plus rows) but
    // by construction leaves no room to keep a memo entry alongside it, so any
    // memo charge must evict instead of erroring. It must be far below the
    // default and the answer must equal the default-limits answer.
    let w = world_bio();
    let q = "[:find (count ?v) :where [:b :q ?v] [:a :bio ?s]]";
    let want = count(&exec(&w, q, &Limits::default()).unwrap());
    assert!(want.contains("2000"), "{want}");
    let at = |m: usize| {
        exec(
            &w,
            q,
            &Limits {
                max_bytes: m,
                ..Limits::default()
            },
        )
    };
    let (mut lo, mut hi) = (0usize, 256 * 1024 * 1024);
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match at(mid) {
            Ok(_) => hi = mid,
            Err(QueryError::MemoryBudget { .. }) => lo = mid,
            Err(e) => panic!("unexpected error at {mid}: {e}"),
        }
    }
    let min_ok = hi;
    assert!(
        min_ok < 4 * BIO,
        "min budget {min_ok} should be near one bio copy"
    );
    assert_eq!(count(&at(min_ok).unwrap()), want);
    // every budget between min and 4 copies stays Ok (eviction, never an error by itself)
    for m in [
        min_ok,
        min_ok + BIO / 2,
        min_ok + BIO,
        min_ok + 2 * BIO,
        4 * BIO,
    ] {
        assert_eq!(
            count(&at(m).unwrap_or_else(|e| panic!("{m}: {e}"))),
            want,
            "{m}"
        );
    }
}

#[test]
fn per_row_distinct_ticks_stay_bounded() {
    // Each row reads :a's bio at a different tick -> memo never hits.
    let q = "[:find (count ?t) :where [:b :q ?t] (at ?t [:a :bio ?s])]";
    for lim in [
        Limits::default(),
        Limits {
            max_bytes: 4 * 1024 * 1024,
            ..Limits::default()
        },
    ] {
        match guarded(world_bio, q, lim) {
            Ok(rs) => assert!(count(&rs).contains("2000"), "{:?}", rs.rows),
            Err(QueryError::MemoryBudget { .. } | QueryError::StepBudget { .. }) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

#[test]
fn holds_during_via_inverse_name_repeated_across_rows() {
    fn build() -> World {
        let mut w = World::in_memory(schema()).unwrap();
        let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
        let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
        w.assert(edwin, "parent_of", Value::Ref(mara), Some(Tick(5)))
            .unwrap();
        let b = w.create_entity("CHARACTER", Some("b")).unwrap();
        let mut t = w.begin();
        for i in 0..ROWS {
            t.assert(b, "q", Value::I64(i as i64), Some(Tick(0)))
                .unwrap();
        }
        t.commit().unwrap();
        w
    }
    let rs = guarded(
        build,
        "[:find (count ?v) :where [:b :q ?v] (during [:mara :child_of :edwin] ?s) [(contains ?s 7)]]",
        Limits::default(),
    )
    .unwrap();
    assert!(count(&rs).contains("2000"), "{:?}", rs.rows);
    let rs = guarded(
        build,
        "[:find (count ?v) :where [:b :q ?v] (during [:mara :child_of :edwin] ?s) [(contains ?s 3)]]",
        Limits::default(),
    )
    .unwrap();
    // no row survives the filter (the pair holds only from tick 5)
    assert!(rs.rows.is_empty(), "{:?}", rs.rows);
}

fn named_world(name: &str) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let e = w.create_entity("CHARACTER", Some("e")).unwrap();
    w.assert(e, "name", Value::Str(name.to_string()), Some(Tick(0)))
        .unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let mut t = w.begin();
    for i in 0..ROWS {
        t.assert(b, "q", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

#[test]
fn lookup_of_bound_long_indexed_name_repeated_across_rows() {
    let name = "n".repeat(256);
    let (n2, n3) = (name.clone(), name.clone());
    let build = move || named_world(&n2);
    let q: &'static str = Box::leak(
        format!("[:find (count ?v) :where [:b :q ?v] [?x :name \"{name}\"]]").into_boxed_str(),
    );
    let rs = guarded(build, q, Limits::default()).unwrap();
    assert!(count(&rs).contains("2000"), "{:?}", rs.rows);
    // also under a tiny budget: Ok with the same answer or a clean budget error
    match guarded(
        move || named_world(&n3),
        q,
        Limits {
            max_bytes: 64 * 1024,
            ..Limits::default()
        },
    ) {
        Ok(rs) => assert!(count(&rs).contains("2000")),
        Err(QueryError::MemoryBudget { .. }) => {}
        Err(e) => panic!("{e}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    /// A repeated per-row read (memo hit) gives the same rows as a query whose
    /// per-row reads can't repeat, and as a naive evaluation of the world.
    #[test]
    fn memoized_reads_match_naive_evaluation(
        bios in proptest::collection::vec((0usize..3, 0i64..4, "[a-c]{0,5}"), 0..8),
        qs in proptest::collection::vec((0usize..3, 0i64..6), 0..10),
        tick in 0i64..4,
        tight in any::<bool>(),
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let ids: Vec<_> = (0..3)
            .map(|i| w.create_entity("CHARACTER", Some(&format!("e{i}"))).unwrap())
            .collect();
        let mut model_bio: Vec<(usize, i64, String)> = vec![];
        for (e, t, s) in &bios {
            if w.assert(ids[*e], "bio", Value::Str(s.clone()), Some(Tick(*t))).is_ok() {
                model_bio.push((*e, *t, s.clone()));
            }
        }
        let mut model_q = vec![];
        for (e, v) in &qs {
            w.assert(ids[*e], "q", Value::I64(*v), Some(Tick(0))).unwrap();
            model_q.push((*e, *v));
        }
        // naive: bios of e0 at `tick` (assert-only world: tick <= t'), per q row of e1
        let bios_at: usize = {
            let mut vals: Vec<&String> = model_bio.iter()
                .filter(|(e, t, _)| *e == 0 && *t <= tick).map(|x| &x.2).collect();
            vals.sort(); vals.dedup(); vals.len()
        };
        let qvals: std::collections::BTreeSet<i64> =
            model_q.iter().filter(|(e, _)| *e == 1).map(|x| x.1).collect();
        let want_rows = qvals.len() * bios_at;
        let lim = if tight {
            Limits { max_bytes: 64 * 1024, ..Limits::default() }
        } else { Limits::default() };
        // same read repeated per row (memoizable)
        let q = format!("[:find ?v ?s :where [:e1 :q ?v] (at {tick} [:e0 :bio ?s])]");
        match exec(&w, &q, &lim) {
            Ok(rs) => prop_assert_eq!(rs.rows.len(), want_rows),
            Err(QueryError::MemoryBudget { .. }) if tight => {}
            Err(e) => prop_assert!(false, "{e}"),
        }
        // reversed order: the bio read comes first, per-row read is the q lookup
        let q2 = format!("[:find ?v ?s :where (at {tick} [:e0 :bio ?s]) [:e1 :q ?v]]");
        match exec(&w, &q2, &lim) {
            Ok(rs) => prop_assert_eq!(rs.rows.len(), want_rows),
            Err(QueryError::MemoryBudget { .. }) if tight => {}
            Err(e) => prop_assert!(false, "{e}"),
        }
        // bound-value read per row: [:e0 :bio ?s] then re-read the same triple
        let q3 = format!("[:find ?v ?s :where (at {tick} [:e0 :bio ?s]) [:e1 :q ?v] (at {tick} [:e0 :bio ?s])]");
        match exec(&w, &q3, &lim) {
            Ok(rs) => prop_assert_eq!(rs.rows.len(), want_rows),
            Err(QueryError::MemoryBudget { .. }) if tight => {}
            Err(e) => prop_assert!(false, "{e}"),
        }
    }
}
