//! Slice 7, item 3 (confirm): edge cases of recursive rules beyond
//! `recursion.rs`: a non-linear rule on a cyclic graph under a step budget, a
//! per-row tick bound inside the recursive body, head types learned only
//! through a recursive call (definitions in reverse order), a row limit hit by
//! one round's intermediate rows while the relation itself stays tiny, a byte
//! budget swept across its boundary, and mutual recursion whose relations
//! settle in different rounds (checked against a naive oracle).

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_query::{Limits, Params, QueryError, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Value>>;
type Edge = (&'static str, &'static str, i64, Option<i64>);

struct W {
    w: World,
    ids: BTreeMap<String, EntityId>,
}

impl W {
    fn id(&self, n: &str) -> EntityId {
        self.ids[n]
    }
    fn refs(&self, names: &[&str]) -> Rows {
        names.iter().map(|n| vec![Value::Ref(self.id(n))]).collect()
    }
}

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .attribute(
            a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(a("since", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
}

fn world(
    nodes: &[String],
    parent: &[Edge],
    ally: &[Edge],
    since: &[(&str, i64)],
    names: &[(&str, &str)],
) -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    {
        let mut t = w.begin();
        for n in nodes {
            ids.insert(n.clone(), t.create_entity("CHARACTER", Some(n)).unwrap());
        }
        for (attr, edges) in [("parent_of", parent), ("ally_of", ally)] {
            for (a, b, from, until) in edges {
                let (a, b) = (ids[*a], Value::Ref(ids[*b]));
                t.assert(a, attr, b.clone(), Some(Tick(*from))).unwrap();
                if let Some(u) = until {
                    t.retract(a, attr, b, Some(Tick(*u))).unwrap();
                }
            }
        }
        for (n, s) in since {
            t.assert(ids[*n], "since", Value::I64(*s), Some(Tick(0)))
                .unwrap();
        }
        for (n, s) in names {
            t.assert(ids[*n], "name", Value::Str(s.to_string()), Some(Tick(0)))
                .unwrap();
        }
        t.commit().unwrap();
    }
    W { w, ids }
}

fn names(ns: &[&str]) -> Vec<String> {
    ns.iter().map(|s| s.to_string()).collect()
}

fn ring(n: usize) -> W {
    let ns: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
    let owned: Vec<(String, String)> = (0..n)
        .map(|i| (ns[i].clone(), ns[(i + 1) % n].clone()))
        .collect();
    // leak the names so they can sit in `Edge`s; tiny and test-only
    let edges: Vec<Edge> = owned
        .into_iter()
        .map(|(a, b)| {
            (
                &*Box::leak(a.into_boxed_str()),
                &*Box::leak(b.into_boxed_str()),
                0,
                None,
            )
        })
        .collect();
    world(&ns, &edges, &[], &[], &[])
}

fn try_go(w: &World, text: &str, lim: &Limits) -> Result<Rows, QueryError> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    run(w, &p.plan, &Params::new(), lim).map(|rs| {
        rs.rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|d| d.as_value().unwrap().clone())
                    .collect()
            })
            .collect()
    })
}

fn go(w: &World, text: &str) -> Rows {
    try_go(w, text, &Limits::default()).unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}

fn q(at: i64, rules: &str, call: &str, find: &str) -> String {
    format!("[:find {find} :at {at} :rules [{rules}] :where {call}]")
}

const ANC: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                   [(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)]";
const ANC_NONLINEAR: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                             [(ancestor ?a ?d) (ancestor ?a ?x) (ancestor ?x ?d)]";

// ------------------------------------------------- non-linear + step budget

#[test]
fn non_linear_rule_on_a_ring_stops_at_a_small_step_budget_and_succeeds_at_default() {
    let f = ring(40); // closure: 40 * 40 = 1,600 tuples
    let text = q(0, ANC_NONLINEAR, "(ancestor ?a ?d)", "?a ?d");
    let small = Limits {
        max_steps: 1_000,
        ..Limits::default()
    };
    match try_go(&f.w, &text, &small) {
        Err(QueryError::StepBudget { limit: 1_000 }) => {}
        other => panic!("expected StepBudget, got {other:?}"),
    }
    let got = try_go(&f.w, &text, &Limits::default()).expect("default budget suffices");
    assert_eq!(got.len(), 1_600);
    // linear and non-linear agree
    assert_eq!(got, go(&f.w, &q(0, ANC, "(ancestor ?a ?d)", "?a ?d")));
}

// ------------------------------------------- per-row tick bound in the body

/// a -> b -> c -> d. a->b holds [0,10); b->c holds from 0; c->d holds from 20.
/// Each node's own `since` says the tick at which its out-edges are read.
fn since_world(a: i64, b: i64, c: i64) -> W {
    world(
        &names(&["a", "b", "c", "d"]),
        &[
            ("a", "b", 0, Some(10)),
            ("b", "c", 0, None),
            ("c", "d", 20, None),
        ],
        &[],
        &[("a", a), ("b", b), ("c", c)],
        &[],
    )
}

const PER_ROW: &str = "[(r ?a ?d) [?a :since ?t] (at ?t [?a :parent_of ?d])] \
                       [(r ?a ?d) [?a :since ?t] (at ?t [?a :parent_of ?x]) (r ?x ?d)]";

#[test]
fn a_tick_bound_by_a_pattern_inside_the_recursive_body_is_read_per_row() {
    // a@5 sees a->b; b@5 sees b->c; c@15 does not see c->d (it starts at 20)
    let f = since_world(5, 5, 15);
    assert_eq!(
        go(&f.w, &q(0, PER_ROW, "(r :a ?d)", "?d")),
        f.refs(&["b", "c"])
    );
    // c@25 sees c->d
    let f = since_world(5, 5, 25);
    assert_eq!(
        go(&f.w, &q(0, PER_ROW, "(r :a ?d)", "?d")),
        f.refs(&["b", "c", "d"])
    );
    // a@15: a->b was retracted at 10, so the chain never starts
    let f = since_world(15, 5, 25);
    assert!(go(&f.w, &q(0, PER_ROW, "(r :a ?d)", "?d")).is_empty());
    // b@15 is fine (b->c has no end); a free call gives each node's own view
    let f = since_world(5, 15, 25);
    let all = go(&f.w, &q(0, PER_ROW, "(r ?x ?d)", "?x ?d"));
    let want: Rows = [
        ("a", "b"),
        ("a", "c"),
        ("a", "d"),
        ("b", "c"),
        ("b", "d"),
        ("c", "d"),
    ]
    .iter()
    .map(|(x, y)| vec![Value::Ref(f.id(x)), Value::Ref(f.id(y))])
    .collect();
    assert_eq!(all, want);
    // the default tick only reads `since` (set at 0, so any later tick will do);
    // the edges are read at each row's own tick
    assert_eq!(
        go(&f.w, &q(500, PER_ROW, "(r :a ?d)", "?d")),
        f.refs(&["b", "c", "d"])
    );
}

// ------------------------------ head types learned only through a recursion

fn typed() -> W {
    world(
        &names(&["a", "b", "c"]),
        &[("a", "b", 0, None), ("b", "c", 0, None)],
        &[],
        &[],
        &[("a", "Anna"), ("b", "Bob"), ("c", "Cy")],
    )
}

/// `?n` is typed `string` only by the base rule; the recursive rule, written
/// first, learns it from the call.
const NAMED_REVERSED: &str = "[(named-below ?a ?n) [?a :parent_of ?x] (named-below ?x ?n)] \
                              [(named-below ?a ?n) [?a :parent_of ?x] [?x :name ?n]]";
const NAMED_FORWARD: &str = "[(named-below ?a ?n) [?a :parent_of ?x] [?x :name ?n]] \
                             [(named-below ?a ?n) [?a :parent_of ?x] (named-below ?x ?n)]";

#[test]
fn head_types_flow_through_a_recursive_call_whatever_the_definition_order() {
    let f = typed();
    let want: Rows = ["Bob", "Cy"]
        .iter()
        .map(|s| vec![Value::Str(s.to_string())])
        .collect();
    for rules in [NAMED_REVERSED, NAMED_FORWARD] {
        assert_eq!(go(&f.w, &q(0, rules, "(named-below :a ?n)", "?n")), want);
        // the learned type is usable by a predicate
        assert_eq!(
            go(
                &f.w,
                &q(0, rules, "(named-below :a ?n) [(= ?n \"Cy\")]", "?n")
            ),
            [vec![Value::Str("Cy".into())]]
                .into_iter()
                .collect::<Rows>()
        );
    }
}

#[test]
fn a_type_clash_with_a_type_learned_through_recursion_is_a_type_mismatch() {
    let f = typed();
    for rules in [NAMED_REVERSED, NAMED_FORWARD] {
        let text = q(0, rules, "(named-below :a ?n) [(= ?n 3)]", "?n");
        let err = prepare(&f.w, &text).expect_err("string vs integer");
        assert_eq!(err.kind, ErrorKind::TypeMismatch, "{err}");
    }
}

// ------------------------------------ row limit: one round's intermediate rows

#[test]
fn a_round_whose_intermediate_rows_exceed_max_rows_fails_while_the_relation_is_tiny() {
    // root -> h, and 300 parents x_i -> c. g(h) is the seed; the recursive rule
    // joins the one delta tuple with *every* parent_of edge (301 rows) but only
    // ever derives the single new tuple g(c).
    let mut ns = names(&["root", "h", "c"]);
    ns.extend((0..300).map(|i| format!("x{i}")));
    let leaked: Vec<&'static str> = ns[3..]
        .iter()
        .map(|s| &*Box::leak(s.clone().into_boxed_str()))
        .collect();
    let mut edges: Vec<Edge> = vec![("root", "h", 0, None)];
    edges.extend(leaked.iter().map(|x| (*x, "c", 0, None)));
    let f = world(&ns, &edges, &[], &[], &[]);
    let rules = "[(g ?b) [:root :parent_of ?b]] [(g ?b) (g ?a) [?z :parent_of ?b]]";
    let text = q(0, rules, "(g ?x)", "?x");
    // g = {h, c}: 2 tuples, nowhere near the limit
    assert_eq!(go(&f.w, &text), f.refs(&["h", "c"]));
    let lim = Limits {
        max_rows: 100,
        ..Limits::default()
    };
    match try_go(&f.w, &text, &lim) {
        Err(QueryError::RowLimit { limit: 100 }) => {}
        other => panic!("expected RowLimit, got {other:?}"),
    }
}

// ----------------------------------------------- byte budget at a boundary

#[test]
fn a_byte_budget_is_all_or_nothing_across_its_boundary() {
    let ns: Vec<String> = (0..40).map(|i| format!("n{i}")).collect();
    let leaked: Vec<&'static str> = ns
        .iter()
        .map(|s| &*Box::leak(s.clone().into_boxed_str()))
        .collect();
    let edges: Vec<Edge> = leaked.windows(2).map(|p| (p[0], p[1], 0, None)).collect();
    let f = world(&ns, &edges, &[], &[], &[]);
    let text = q(0, ANC, "(ancestor ?a ?d)", "?a ?d");
    let want = go(&f.w, &text);
    assert_eq!(want.len(), 40 * 39 / 2);
    let at = |b: usize| {
        try_go(
            &f.w,
            &text,
            &Limits {
                max_bytes: b,
                ..Limits::default()
            },
        )
    };
    // smallest budget that succeeds
    let (mut lo, mut hi) = (1usize, 64 * 1024 * 1024);
    assert!(at(hi).is_ok());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match at(mid) {
            Ok(_) => hi = mid,
            Err(QueryError::MemoryBudget { .. }) => lo = mid + 1,
            Err(e) => panic!("unexpected error at {mid}: {e}"),
        }
    }
    let min = lo;
    let from = min.saturating_sub(4096).max(1);
    for b in (from..min + 256).step_by(64) {
        match at(b) {
            Ok(got) => {
                assert!(b >= min, "succeeded at {b}, below the minimum {min}");
                assert_eq!(got, want, "wrong answer at {b}");
            }
            Err(QueryError::MemoryBudget { limit }) => {
                assert!(b < min, "failed at {b}, at or above the minimum {min}");
                assert_eq!(limit, b);
            }
            Err(e) => panic!("unexpected error at {b}: {e}"),
        }
    }
}

// ------------------------------- mutual recursion settling in different rounds

#[test]
fn mutual_recursion_where_one_relation_settles_early_equals_the_naive_closure() {
    // chain n0..n7 (parent_of); ally n5 ~ m (bidirectional); all at 0.
    let ns: Vec<String> = (0..8)
        .map(|i| format!("n{i}"))
        .chain(["m".to_string()])
        .collect();
    let leaked: Vec<&'static str> = ns
        .iter()
        .map(|s| &*Box::leak(s.clone().into_boxed_str()))
        .collect();
    let chain: Vec<Edge> = leaked[..8]
        .windows(2)
        .map(|p| (p[0], p[1], 0, None))
        .collect();
    let f = world(&ns, &chain, &[("n5", "m", 0, None)], &[], &[]);
    // p grows along the whole chain (several rounds); q is fed by the ally edge
    // and by p, and is settled after a few.
    let rules = "[(p ?a ?d) [?a :parent_of ?d]] \
                 [(p ?a ?d) [?a :parent_of ?x] (p ?x ?d)] \
                 [(p ?a ?d) (q ?a ?d)] \
                 [(q ?a ?d) [?a :ally_of ?d]] \
                 [(q ?a ?d) (p ?a ?x) [?x :ally_of ?d]]";
    // naive oracle over explicit edge sets
    let idx = |n: &str| ns.iter().position(|s| s == n).unwrap();
    let par: BTreeSet<(usize, usize)> = (0..7).map(|i| (i, i + 1)).collect();
    let ally: BTreeSet<(usize, usize)> = [(idx("n5"), idx("m")), (idx("m"), idx("n5"))]
        .into_iter()
        .collect();
    type PairSet = BTreeSet<(usize, usize)>;
    let (mut p, mut qq): (PairSet, PairSet) = (BTreeSet::new(), BTreeSet::new());
    loop {
        let (op, oq) = (p.len(), qq.len());
        p.extend(par.iter().copied());
        for &(a, x) in &par {
            let more: Vec<_> = p
                .iter()
                .filter(|(y, _)| *y == x)
                .map(|&(_, d)| (a, d))
                .collect();
            p.extend(more);
        }
        p.extend(qq.iter().copied());
        qq.extend(ally.iter().copied());
        for &(a, x) in p.clone().iter() {
            for &(y, d) in &ally {
                if x == y {
                    qq.insert((a, d));
                }
            }
        }
        if p.len() == op && qq.len() == oq {
            break;
        }
    }
    let pairs = |s: &BTreeSet<(usize, usize)>| -> Rows {
        s.iter()
            .map(|&(a, b)| vec![Value::Ref(f.id(&ns[a])), Value::Ref(f.id(&ns[b]))])
            .collect()
    };
    assert_eq!(go(&f.w, &q(0, rules, "(p ?a ?d)", "?a ?d")), pairs(&p));
    assert_eq!(go(&f.w, &q(0, rules, "(q ?a ?d)", "?a ?d")), pairs(&qq));
    // sanity on the oracle itself: p reaches m only through the ally edge
    assert!(p.contains(&(idx("n0"), idx("m"))));
    assert!(!p.contains(&(idx("m"), idx("n0"))));
}
