//! Slice 7, item 3 (RED): recursive queries equal a plain-Rust transitive
//! closure of the edges the world holds at the query tick (read through
//! kala-core's `attr_scan`), on random small directed and bidirectional
//! graphs with retractions at random ticks.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, run};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

type Pairs = BTreeSet<(EntityId, EntityId)>;

const ANC: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                   [(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)]";
const ANC_LEFT: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                        [(ancestor ?a ?d) (ancestor ?a ?x) [?x :parent_of ?d]]";
const REACH: &str = "[(reach ?a ?d) [?a :ally_of ?d]] \
                     [(reach ?a ?d) [?a :ally_of ?x] (reach ?x ?d)]";

/// One edge: endpoints (node indexes), assert tick, optional retract tick
/// (strictly later, so no same-tick conflict).
type Edge = (usize, usize, i64, Option<i64>);

fn edges(n: usize, directed: bool) -> impl Strategy<Value = Vec<Edge>> {
    prop::collection::vec((0..n, 0..n, 0i64..10, prop::option::of(1i64..8)), 0..24).prop_map(
        move |raw| {
            // One history per (unordered, if bidirectional) pair; no self-loops
            // on bidirectional edges.
            let mut seen = BTreeMap::new();
            for (a, b, from, until) in raw {
                if !directed && a == b {
                    continue;
                }
                let key = if directed {
                    (a, b)
                } else {
                    (a.min(b), a.max(b))
                };
                seen.entry(key)
                    .or_insert((key.0, key.1, from, until.map(|u| from + u)));
            }
            seen.into_values().collect()
        },
    )
}

fn build(n: usize, es: &[Edge], attr: &str) -> (World, Vec<EntityId>) {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    let ally = a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional();
    let parent = a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
        .directed()
        .inverse("child_of");
    let schema = Schema::new()
        .node_type("CHARACTER")
        .attribute(ally)
        .attribute(parent);
    let mut w = World::in_memory(schema).unwrap();
    let mut t = w.begin();
    let ids: Vec<EntityId> = (0..n)
        .map(|i| {
            t.create_entity("CHARACTER", Some(&format!("n{i}")))
                .unwrap()
        })
        .collect();
    for &(a, b, from, until) in es {
        let v = Value::Ref(ids[b]);
        t.assert(ids[a], attr, v.clone(), Some(Tick(from))).unwrap();
        if let Some(u) = until {
            t.retract(ids[a], attr, v, Some(Tick(u))).unwrap();
        }
    }
    t.commit().unwrap();
    (w, ids)
}

/// The transitive closure of the edges `attr` holds at `tick`, read through
/// kala-core. A bidirectional attribute is symmetrized.
fn closure(w: &World, attr: &str, symmetric: bool, tick: i64) -> Pairs {
    let mut adj: BTreeMap<EntityId, BTreeSet<EntityId>> = BTreeMap::new();
    for (e, v) in w.attr_scan(attr, Tick(tick)).unwrap() {
        let Value::Ref(t) = v else { unreachable!() };
        adj.entry(e).or_default().insert(t);
        if symmetric {
            adj.entry(t).or_default().insert(e);
        }
    }
    let mut out = Pairs::new();
    for &start in adj.keys() {
        // plain BFS: everything reachable by one or more edges
        let mut seen = BTreeSet::new();
        let mut todo: Vec<EntityId> = adj[&start].iter().copied().collect();
        while let Some(x) = todo.pop() {
            if seen.insert(x)
                && let Some(next) = adj.get(&x)
            {
                todo.extend(next.iter().copied());
            }
        }
        out.extend(seen.into_iter().map(|d| (start, d)));
    }
    out
}

fn pairs_of(w: &World, text: &str) -> Pairs {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let rs = run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"));
    rs.rows
        .into_iter()
        .map(|r| {
            let id = |d: &kala_query::Datum| match d.as_value() {
                Some(Value::Ref(e)) => *e,
                other => panic!("not a ref: {other:?}"),
            };
            (id(&r[0]), id(&r[1]))
        })
        .collect()
}

fn query(rules: &str, call: &str, tick: i64) -> String {
    format!("[:find ?a ?d :at {tick} :rules [{rules}] :where {call}]")
}

fn cases() -> ProptestConfig {
    ProptestConfig {
        cases: 96,
        ..ProptestConfig::default()
    }
}

/// A node count (2..=12) with edges over exactly those nodes.
fn graph(directed: bool) -> impl Strategy<Value = (usize, Vec<Edge>)> {
    (2usize..=12).prop_flat_map(move |n| (Just(n), edges(n, directed)))
}

fn ds_of(w: &World, text: &str) -> BTreeSet<EntityId> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let rs = run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"));
    rs.rows
        .into_iter()
        .map(|r| match r[0].as_value() {
            Some(Value::Ref(e)) => *e,
            other => panic!("not a ref: {other:?}"),
        })
        .collect()
}

proptest! {
    #![proptest_config(cases())]

    #[test]
    fn directed_ancestors_equal_the_transitive_closure(
        (n, es) in graph(true),
        tick in -2i64..16,
    ) {
        let (w, _) = build(n, &es, "parent_of");
        let want = closure(&w, "parent_of", false, tick);
        prop_assert_eq!(pairs_of(&w, &query(ANC, "(ancestor ?a ?d)", tick)), want.clone());
        prop_assert_eq!(pairs_of(&w, &query(ANC_LEFT, "(ancestor ?a ?d)", tick)), want);
    }

    #[test]
    fn directed_ancestors_from_a_constant(
        (n, es) in graph(true),
        tick in -2i64..16,
        start in 0usize..12,
    ) {
        let (w, ids) = build(n, &es, "parent_of");
        let k = start % n;
        let want: BTreeSet<EntityId> = closure(&w, "parent_of", false, tick)
            .into_iter()
            .filter(|(a, _)| *a == ids[k])
            .map(|(_, d)| d)
            .collect();
        let text = format!(
            "[:find ?d :at {tick} :rules [{ANC}] :where (ancestor :n{k} ?d)]"
        );
        prop_assert_eq!(ds_of(&w, &text), want);
    }

    #[test]
    fn bidirectional_reachability_equals_the_transitive_closure(
        (n, es) in graph(false),
        tick in -2i64..16,
    ) {
        let (w, _) = build(n, &es, "ally_of");
        let want = closure(&w, "ally_of", true, tick);
        prop_assert_eq!(pairs_of(&w, &query(REACH, "(reach ?a ?d)", tick)), want);
    }

    #[test]
    fn bidirectional_reachability_from_a_constant(
        (n, es) in graph(false),
        tick in -2i64..16,
        start in 0usize..12,
    ) {
        let (w, ids) = build(n, &es, "ally_of");
        let k = start % n;
        let want: BTreeSet<EntityId> = closure(&w, "ally_of", true, tick)
            .into_iter()
            .filter(|(a, _)| *a == ids[k])
            .map(|(_, d)| d)
            .collect();
        let text = format!(
            "[:find ?d :at {tick} :rules [{REACH}] :where (reach :n{k} ?d)]"
        );
        prop_assert_eq!(ds_of(&w, &text), want);
    }
}
