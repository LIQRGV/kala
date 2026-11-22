//! Slice 7, item 4 (confirm): edge cases of `not` beyond `negation.rs`.
//!
//! World: characters a..f (handles `a`..`f`). parent_of chain a->b->c->d->e
//! @0 (f has no parents or children). `flagged` (bool): d @0. `marked` (bool):
//! b @-10, c @0, d @0. `born` (i64): a 5, b -5, c -5, d 5, e -5. `name`: a "A",
//! b "B". `located_in`: c -> p (a PLACE).

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_query::{Limits, Params, QueryError, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Value>>;

struct W {
    w: World,
    ids: BTreeMap<String, EntityId>,
}

impl W {
    fn refs(&self, ns: &[&str]) -> Rows {
        ns.iter().map(|n| vec![Value::Ref(self.ids[*n])]).collect()
    }
    fn pairs(&self, ps: &[(&str, &str)]) -> Rows {
        ps.iter()
            .map(|(a, b)| vec![Value::Ref(self.ids[*a]), Value::Ref(self.ids[*b])])
            .collect()
    }
}

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("born", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(a("flagged", ValueType::Bool, Kind::State, Cardinality::One))
        .attribute(a("marked", ValueType::Bool, Kind::State, Cardinality::One))
        .attribute(a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(a("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
}

fn world(extra: usize) -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    {
        let mut t = w.begin();
        for h in ["a", "b", "c", "d", "e", "f"] {
            ids.insert(
                h.to_string(),
                t.create_entity("CHARACTER", Some(h)).unwrap(),
            );
        }
        ids.insert("p".into(), t.create_entity("PLACE", Some("p")).unwrap());
        let id = |n: &str| ids[n];
        for w in ["a", "b", "c", "d"] {
            let next =
                ["b", "c", "d", "e"][["a", "b", "c", "d"].iter().position(|x| *x == w).unwrap()];
            t.assert(id(w), "parent_of", Value::Ref(id(next)), Some(Tick(0)))
                .unwrap();
        }
        t.assert(id("d"), "flagged", Value::Bool(true), Some(Tick(0)))
            .unwrap();
        t.assert(id("b"), "marked", Value::Bool(true), Some(Tick(-10)))
            .unwrap();
        t.assert(id("c"), "marked", Value::Bool(true), Some(Tick(0)))
            .unwrap();
        t.assert(id("d"), "marked", Value::Bool(true), Some(Tick(0)))
            .unwrap();
        for (h, b) in [("a", 5), ("b", -5), ("c", -5), ("d", 5), ("e", -5)] {
            t.assert(id(h), "born", Value::I64(b), Some(Tick(0)))
                .unwrap();
        }
        t.assert(id("a"), "name", Value::Str("A".into()), Some(Tick(0)))
            .unwrap();
        t.assert(id("b"), "name", Value::Str("B".into()), Some(Tick(0)))
            .unwrap();
        t.assert(id("c"), "located_in", Value::Ref(id("p")), Some(Tick(0)))
            .unwrap();
        for i in 0..extra {
            let e = t.create_entity("CHARACTER", None).unwrap();
            t.assert(e, "born", Value::I64(i as i64), Some(Tick(0)))
                .unwrap();
        }
        t.commit().unwrap();
    }
    W { w, ids }
}

fn try_go(
    w: &World,
    text: &str,
    params: &[(&str, Value)],
    lim: &Limits,
) -> Result<Rows, QueryError> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, lim).map(|rs| {
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

fn go(w: &World, text: &str, params: &[(&str, Value)]) -> Rows {
    try_go(w, text, params, &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}

fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.kind,
    }
}

const ANC: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                   [(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)]";

// ------------------------------------------------ not inside recursive rules

#[test]
fn not_inside_a_recursive_rule_body_over_plain_patterns() {
    let f = world(0);
    // clear(a, d): a path of parent_of edges whose every node after the start is
    // unflagged. d is flagged, so no edge into d (c->d) counts. Edges kept: a->b,
    // b->c, d->e. Closure: (a,b) (b,c) (a,c) (d,e). Nothing reaches d or e via a.
    let rules = "[(clear ?a ?d) [?a :parent_of ?d] (not [?d :flagged true])] \
                 [(clear ?a ?d) [?a :parent_of ?x] (not [?x :flagged true]) (clear ?x ?d)]";
    let text = format!("[:find ?a ?d :at 0 :rules [{rules}] :where (clear ?a ?d)]");
    assert_eq!(
        go(&f.w, &text, &[]),
        f.pairs(&[("a", "b"), ("b", "c"), ("a", "c"), ("d", "e")])
    );
}

#[test]
fn not_over_a_lower_recursive_rule_inside_a_recursive_rule() {
    let f = world(0);
    // far(a,d): a proper (non-direct) descendant. ancestor is the lower stratum;
    // far is itself recursive (its second rule extends by one parent_of step).
    // ancestor on the chain a>b>c>d>e = every ordered pair (i<j); minus the 4
    // direct edges leaves 6 pairs, and the extension rule adds nothing new.
    let rules = format!(
        "{ANC} [(far ?a ?d) (ancestor ?a ?d) (not [?a :parent_of ?d])] \
         [(far ?a ?d) (far ?a ?x) [?x :parent_of ?d]]"
    );
    let text = format!("[:find ?a ?d :at 0 :rules [{rules}] :where (far ?a ?d)]");
    assert_eq!(
        go(&f.w, &text, &[]),
        f.pairs(&[
            ("a", "c"),
            ("a", "d"),
            ("a", "e"),
            ("b", "d"),
            ("b", "e"),
            ("c", "e"),
        ])
    );
}

// ------------------------------------------------------------ unbound vars

#[test]
fn a_find_variable_bound_only_inside_a_not_is_unbound() {
    let f = world(0);
    assert_eq!(
        kind(
            &f.w,
            "[:find ?x :where [?c :flagged true] (not [?c :parent_of ?x])]"
        ),
        ErrorKind::UnboundVariable
    );
}

#[test]
fn a_rule_head_variable_bound_only_inside_a_not_is_unbound() {
    let f = world(0);
    let text = "[:find ?c ?x :rules [[(r ?c ?x) [?c :flagged true] (not [?c :parent_of ?x])]] \
                :where (r ?c ?x)]";
    assert_eq!(kind(&f.w, text), ErrorKind::UnboundVariable);
}

// ------------------------------------------------- locals are local (types)

#[test]
fn two_sibling_nots_may_reuse_a_local_name_with_different_types() {
    let f = world(0);
    // ?x is a string in the first not and a ref in the second. Locals are local,
    // so this must compile. Drops a (named "A") and c (located somewhere).
    let text = "[:find ?c :at 0 :where [?c :born _] \
                (not [?c :name ?x] [(= ?x \"A\")]) (not [?c :located_in ?x])]";
    assert_eq!(go(&f.w, text, &[]), f.refs(&["b", "d", "e"]));
}

// ----------------------------------------------------- ticks, params, preds

#[test]
fn a_tick_variable_bound_outside_is_read_inside_the_not() {
    let f = world(0);
    // marked: b @-10, c @0, d @0. Each row reads `marked` at its own birth tick:
    // a (5): never marked -> kept; b (-5): marked since -10 -> out; c (-5): marked
    // only from 0 -> kept; d (5): marked -> out; e (-5): never -> kept.
    let text = "[:find ?c :where [?c :born ?t] (not (at ?t [?c :marked true]))]";
    assert_eq!(go(&f.w, text, &[]), f.refs(&["a", "c", "e"]));
}

#[test]
fn a_param_used_inside_a_not() {
    let f = world(0);
    let text = "[:find ?c :in $ ?n :at 0 :where [?c :born _] (not [?c :name ?n])]";
    assert_eq!(
        go(&f.w, text, &[("n", Value::Str("A".into()))]),
        f.refs(&["b", "c", "d", "e"])
    );
    assert_eq!(
        go(&f.w, text, &[("n", Value::Str("nobody".into()))]),
        f.refs(&["a", "b", "c", "d", "e"])
    );
}

#[test]
fn a_not_whose_body_is_only_a_predicate_on_outer_variables() {
    let f = world(0);
    // born: a 5, b -5, c -5, d 5, e -5. Keep the rows where `?b > 0` is false.
    let text = "[:find ?c :at 0 :where [?c :born ?b] (not [(> ?b 0)])]";
    assert_eq!(go(&f.w, text, &[]), f.refs(&["b", "c", "e"]));
}

// ------------------------------------------------------- cost of a global not

#[test]
fn a_global_not_over_2000_rows_succeeds_and_its_cost_stays_linear() {
    let f = world(2_000);
    // 2,005 rows have `born`. The body names no outer variable: e is never
    // flagged (true for all rows), d is flagged (false for all rows).
    let keep = "[:find ?c :at 0 :where [?c :born _] (not [:e :flagged true])]";
    assert_eq!(go(&f.w, keep, &[]).len(), 2_005);
    let drop = "[:find ?c :at 0 :where [?c :born _] (not [:d :flagged true])]";
    assert!(go(&f.w, drop, &[]).is_empty());
    // Linear work is a few steps per row (~10k); re-running the whole input per
    // row would be ~4M. 50,000 (measured: 10k-20k) leaves headroom and fails the latter.
    let lim = Limits {
        max_steps: 50_000,
        ..Limits::default()
    };
    assert_eq!(
        try_go(&f.w, keep, &[], &lim)
            .expect("within 50k steps")
            .len(),
        2_005
    );
    assert!(
        try_go(&f.w, drop, &[], &lim)
            .expect("within 50k steps")
            .is_empty()
    );
}
