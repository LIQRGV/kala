//! Slice 7, item 3 (RED): recursive rules, evaluated semi-naively to a
//! fixpoint. Today `prepare` refuses any cycle in the rule call graph with
//! `ErrorKind::Unsupported`; the contract is that a cycle (direct or mutual)
//! compiles and runs, results are sets equal to naive evaluation, evaluation
//! terminates on cyclic data, every pattern reads at its own tick, and the
//! row, step and byte limits stop a runaway recursion with the usual errors.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Value>>;

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
    fn pairs(&self, ps: &[(&str, &str)]) -> Rows {
        ps.iter()
            .map(|(a, b)| vec![Value::Ref(self.id(a)), Value::Ref(self.id(b))])
            .collect()
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
}

/// `parent`: (from, to, assert tick, retract tick); `ally`: the same.
fn world(
    nodes: &[&str],
    parent: &[(&str, &str, i64, Option<i64>)],
    ally: &[(&str, &str, i64, Option<i64>)],
) -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    {
        let mut t = w.begin();
        for n in nodes {
            ids.insert(
                n.to_string(),
                t.create_entity("CHARACTER", Some(n)).unwrap(),
            );
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
        t.commit().unwrap();
    }
    W { w, ids }
}

/// The family: ann -> bob, eve; bob -> cat (retracted -150), dan (from -100);
/// eve -> fay; cat -> gus.
fn family() -> W {
    world(
        &["ann", "bob", "cat", "dan", "eve", "fay", "gus"],
        &[
            ("ann", "bob", -300, None),
            ("ann", "eve", -300, None),
            ("bob", "cat", -250, Some(-150)),
            ("bob", "dan", -100, None),
            ("eve", "fay", -300, None),
            ("cat", "gus", -250, None),
        ],
        &[],
    )
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

const ANC: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                   [(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)]";
/// Left recursion: the recursive call comes first in the body (and in the text).
const ANC_LEFT: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                        [(ancestor ?a ?d) (ancestor ?a ?x) [?x :parent_of ?d]]";
/// Non-linear: two recursive calls in one body.
const ANC_NONLINEAR: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                             [(ancestor ?a ?d) (ancestor ?a ?x) (ancestor ?x ?d)]";

fn q(at: i64, rules: &str, call: &str, find: &str) -> String {
    format!("[:find {find} :at {at} :rules [{rules}] :where {call}]")
}

// -------------------------------------------------------------- ancestors

#[test]
fn ancestors_from_a_constant_at_several_ticks() {
    let f = family();
    for (t, want) in [
        (-200, vec!["bob", "eve", "cat", "fay", "gus"]),
        (-100, vec!["bob", "eve", "dan", "fay"]),
        (0, vec!["bob", "eve", "dan", "fay"]),
        (-400, vec![]),
    ] {
        let got = go(&f.w, &q(t, ANC, "(ancestor :ann ?d)", "?d"));
        assert_eq!(got, f.refs(&want), "at {t}");
    }
}

#[test]
fn ancestors_free_call_returns_every_pair() {
    let f = family();
    let got = go(&f.w, &q(-200, ANC, "(ancestor ?a ?d)", "?a ?d"));
    let want = f.pairs(&[
        ("ann", "bob"),
        ("ann", "eve"),
        ("ann", "cat"),
        ("ann", "fay"),
        ("ann", "gus"),
        ("bob", "cat"),
        ("bob", "gus"),
        ("eve", "fay"),
        ("cat", "gus"),
    ]);
    assert_eq!(got, want);
    // @0: bob -> cat is gone, so bob reaches only dan; cat -> gus still holds
    let got = go(&f.w, &q(0, ANC, "(ancestor ?a ?d)", "?a ?d"));
    let want = f.pairs(&[
        ("ann", "bob"),
        ("ann", "eve"),
        ("ann", "dan"),
        ("ann", "fay"),
        ("bob", "dan"),
        ("eve", "fay"),
        ("cat", "gus"),
    ]);
    assert_eq!(got, want);
}

#[test]
fn a_constant_at_the_second_position_asks_who_are_the_ancestors() {
    let f = family();
    let got = go(&f.w, &q(-200, ANC, "(ancestor ?a :gus)", "?a"));
    assert_eq!(got, f.refs(&["ann", "bob", "cat"]));
    // yes/no: both constants
    let yes = go(
        &f.w,
        &q(-200, ANC, "(ancestor :ann :gus) [?x :child_of :ann]", "?x"),
    );
    assert_eq!(yes, f.refs(&["bob", "eve"]));
    let no = go(
        &f.w,
        &q(0, ANC, "(ancestor :ann :gus) [?x :child_of :ann]", "?x"),
    );
    assert!(no.is_empty());
}

#[test]
fn left_recursion_gives_the_same_answer() {
    let f = family();
    for t in [-400, -200, -120, -100, 0] {
        let right = go(&f.w, &q(t, ANC, "(ancestor ?a ?d)", "?a ?d"));
        let left = go(&f.w, &q(t, ANC_LEFT, "(ancestor ?a ?d)", "?a ?d"));
        assert_eq!(left, right, "at {t}");
        let c = go(&f.w, &q(t, ANC_LEFT, "(ancestor :ann ?d)", "?d"));
        let want = go(&f.w, &q(t, ANC, "(ancestor :ann ?d)", "?d"));
        assert_eq!(c, want, "from ann at {t}");
    }
}

#[test]
fn rule_order_and_body_order_do_not_matter() {
    let f = family();
    let swapped = "[(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)] \
                   [(ancestor ?a ?d) [?a :parent_of ?d]]";
    assert_eq!(
        go(&f.w, &q(-200, swapped, "(ancestor ?a ?d)", "?a ?d")),
        go(&f.w, &q(-200, ANC, "(ancestor ?a ?d)", "?a ?d"))
    );
}

#[test]
fn a_non_linear_rule_with_two_recursive_calls() {
    let f = family();
    for t in [-200, 0] {
        assert_eq!(
            go(&f.w, &q(t, ANC_NONLINEAR, "(ancestor ?a ?d)", "?a ?d")),
            go(&f.w, &q(t, ANC, "(ancestor ?a ?d)", "?a ?d")),
            "at {t}"
        );
    }
}

#[test]
fn a_recursive_rule_called_with_a_param_and_used_twice() {
    let f = family();
    let text = format!(
        "[:find ?x ?y :in $ ?who :at -200 :rules [{ANC}] \
         :where (ancestor ?who ?x) (ancestor ?x ?y)]"
    );
    let p = prepare(&f.w, &text).expect("prepare");
    let ps: Params = [("who".to_string(), Value::Ref(f.id("ann")))].into();
    let rs = run(&f.w, &p.plan, &ps, &Limits::default()).expect("run");
    let got: Rows = rs
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|d| d.as_value().unwrap().clone())
                .collect()
        })
        .collect();
    let want = f.pairs(&[
        ("bob", "cat"),
        ("bob", "gus"),
        ("eve", "fay"),
        ("cat", "gus"),
    ]);
    assert_eq!(got, want);
}

#[test]
fn a_predicate_inside_a_recursive_body() {
    // reach without the trivial loops, over a directed cycle a -> b -> c -> a
    let f = world(
        &["a", "b", "c"],
        &[
            ("a", "b", 0, None),
            ("b", "c", 0, None),
            ("c", "a", 0, None),
        ],
        &[],
    );
    let rules = "[(r ?x ?y) [?x :parent_of ?y] [(!= ?x ?y)]] \
                 [(r ?x ?y) [?x :parent_of ?m] (r ?m ?y) [(!= ?x ?y)]]";
    let got = go(&f.w, &q(0, rules, "(r ?x ?y)", "?x ?y"));
    let want = f.pairs(&[
        ("a", "b"),
        ("a", "c"),
        ("b", "a"),
        ("b", "c"),
        ("c", "a"),
        ("c", "b"),
    ]);
    assert_eq!(got, want);
}

// ---------------------------------------------------------- cyclic data

#[test]
fn a_directed_cycle_terminates_and_includes_self_pairs() {
    let f = world(
        &["a", "b", "c"],
        &[
            ("a", "b", 0, None),
            ("b", "c", 0, None),
            ("c", "a", 0, None),
        ],
        &[],
    );
    let got = go(&f.w, &q(0, ANC, "(ancestor ?x ?y)", "?x ?y"));
    assert_eq!(got.len(), 9); // every node reaches every node, itself included
    let got = go(&f.w, &q(0, ANC, "(ancestor :a ?y)", "?y"));
    assert_eq!(got, f.refs(&["a", "b", "c"]));
}

#[test]
fn a_self_loop_terminates() {
    let f = world(&["a"], &[("a", "a", 0, None)], &[]);
    assert_eq!(
        go(&f.w, &q(0, ANC, "(ancestor ?x ?y)", "?x ?y")),
        f.pairs(&[("a", "a")])
    );
}

const REACH: &str = "[(reach ?a ?d) [?a :ally_of ?d]] \
                     [(reach ?a ?d) [?a :ally_of ?x] (reach ?x ?d)]";

/// Triangle a-b-c, a tail c-d, a separate pair x-y, a loner z. All @0.
fn allies() -> W {
    world(
        &["a", "b", "c", "d", "x", "y", "z"],
        &[],
        &[
            ("a", "b", 0, None),
            ("b", "c", 0, None),
            ("c", "a", 0, None),
            ("c", "d", 0, None),
            ("x", "y", 0, None),
        ],
    )
}

#[test]
fn reachability_over_bidirectional_ally_of_with_cycles() {
    let f = allies();
    let got = go(&f.w, &q(0, REACH, "(reach :a ?d)", "?d"));
    // a reaches itself through the triangle
    assert_eq!(got, f.refs(&["a", "b", "c", "d"]));
    // the tail node reaches the whole component, itself included
    let got = go(&f.w, &q(0, REACH, "(reach :d ?d)", "?d"));
    assert_eq!(got, f.refs(&["a", "b", "c", "d"]));
    // the separate pair
    let got = go(&f.w, &q(0, REACH, "(reach :x ?d)", "?d"));
    assert_eq!(got, f.refs(&["x", "y"]));
    // the loner reaches nothing, not even itself
    assert!(go(&f.w, &q(0, REACH, "(reach :z ?d)", "?d")).is_empty());
}

#[test]
fn reachability_free_call_is_the_cross_product_of_each_component() {
    let f = allies();
    let got = go(&f.w, &q(0, REACH, "(reach ?a ?d)", "?a ?d"));
    assert_eq!(got.len(), 4 * 4 + 2 * 2);
    assert!(got.contains(&f.pairs(&[("d", "d")]).into_iter().next().unwrap()));
    assert!(!got.contains(&f.pairs(&[("a", "x")]).into_iter().next().unwrap()));
}

#[test]
fn reachability_yes_no_with_both_constants() {
    let f = allies();
    let probe = |call: &str| {
        go(
            &f.w,
            &q(0, REACH, &format!("{call} [:a :ally_of ?w]"), "?w"),
        )
    };
    assert!(!probe("(reach :d :b)").is_empty());
    assert!(probe("(reach :d :x)").is_empty());
    assert!(!probe("(reach :a :a)").is_empty()); // itself, through a cycle
}

#[test]
fn a_tree_without_a_cycle_does_not_reach_itself() {
    let f = world(
        &["a", "b", "c"],
        &[],
        &[("a", "b", 0, None), ("a", "c", 0, None)],
    );
    // b - a - c: b reaches a, c and (through a) b itself; that is a cycle of length 2.
    let got = go(&f.w, &q(0, REACH, "(reach :b ?d)", "?d"));
    assert_eq!(got, f.refs(&["a", "b", "c"]));
    // a directed chain has no such cycle
    let g = world(
        &["a", "b", "c"],
        &[("a", "b", 0, None), ("b", "c", 0, None)],
        &[],
    );
    assert_eq!(
        go(&g.w, &q(0, ANC, "(ancestor :a ?d)", "?d")),
        g.refs(&["b", "c"])
    );
    assert!(go(&g.w, &q(0, ANC, "(ancestor :c ?d)", "?d")).is_empty());
}

// --------------------------------------------------- mutual recursion

const EVEN_ODD: &str = "[(odd ?a ?d) [?a :parent_of ?d]] \
                        [(odd ?a ?d) [?a :parent_of ?x] (even ?x ?d)] \
                        [(even ?a ?d) [?a :parent_of ?x] (odd ?x ?d)]";

fn chain7() -> W {
    let names: Vec<String> = (0..7).map(|i| format!("n{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let edges: Vec<(&str, &str, i64, Option<i64>)> =
        refs.windows(2).map(|p| (p[0], p[1], 0, None)).collect();
    world(&refs, &edges, &[])
}

#[test]
fn mutual_recursion_even_and_odd_distance_over_a_chain() {
    let f = chain7();
    assert_eq!(
        go(&f.w, &q(0, EVEN_ODD, "(odd :n0 ?d)", "?d")),
        f.refs(&["n1", "n3", "n5"])
    );
    assert_eq!(
        go(&f.w, &q(0, EVEN_ODD, "(even :n0 ?d)", "?d")),
        f.refs(&["n2", "n4", "n6"])
    );
    // from n1: odd = n2 n4 n6, even = n3 n5
    assert_eq!(
        go(&f.w, &q(0, EVEN_ODD, "(odd :n1 ?d)", "?d")),
        f.refs(&["n2", "n4", "n6"])
    );
    assert_eq!(
        go(&f.w, &q(0, EVEN_ODD, "(even :n1 ?d)", "?d")),
        f.refs(&["n3", "n5"])
    );
}

#[test]
fn mutual_recursion_free_calls_partition_the_closure() {
    let f = chain7();
    let odd = go(&f.w, &q(0, EVEN_ODD, "(odd ?a ?d)", "?a ?d"));
    let even = go(&f.w, &q(0, EVEN_ODD, "(even ?a ?d)", "?a ?d"));
    assert!(odd.is_disjoint(&even));
    // 7 nodes: 21 ordered pairs a < d; odd distance: 6+4+2 = 12 ... counted
    // directly: distances 1..6 occur 6,5,4,3,2,1 times.
    assert_eq!(odd.len(), 6 + 4 + 2);
    assert_eq!(even.len(), 5 + 3 + 1);
    let all = go(&f.w, &q(0, ANC, "(ancestor ?a ?d)", "?a ?d"));
    assert_eq!(odd.union(&even).cloned().collect::<Rows>(), all);
}

#[test]
fn mutual_recursion_through_a_cycle_terminates() {
    // a 3-cycle: odd/even distances wrap around, so every node is both
    let f = world(
        &["a", "b", "c"],
        &[
            ("a", "b", 0, None),
            ("b", "c", 0, None),
            ("c", "a", 0, None),
        ],
        &[],
    );
    let odd = go(&f.w, &q(0, EVEN_ODD, "(odd :a ?d)", "?d"));
    let even = go(&f.w, &q(0, EVEN_ODD, "(even :a ?d)", "?d"));
    // odd walks from a end at b (1), a (3), c (5); even walks at c (2), b (4), a (6)
    assert_eq!(odd, f.refs(&["a", "b", "c"]));
    assert_eq!(even, f.refs(&["a", "b", "c"]));
}

#[test]
fn a_rule_calling_a_recursive_rule_is_stratified_above_it() {
    let f = family();
    let rules = format!("{ANC} [(grand-anc ?a ?d) (ancestor ?a ?m) (ancestor ?m ?d)]");
    let got = go(&f.w, &q(-200, &rules, "(grand-anc :ann ?d)", "?d"));
    assert_eq!(got, f.refs(&["cat", "fay", "gus"]));
}

#[test]
fn a_recursive_rule_that_is_never_called_is_harmless() {
    let f = family();
    let t = format!("[:find ?x :at 0 :rules [{ANC}] :where [?x :child_of :ann]]");
    assert_eq!(go(&f.w, &t), f.refs(&["bob", "eve"]));
    let t = "[:find ?x :at 0 :rules [[(loop ?a) (loop ?a)]] :where [?x :child_of :ann]]";
    assert_eq!(go(&f.w, t), f.refs(&["bob", "eve"]));
}

#[test]
fn a_recursive_rule_with_no_base_case_is_empty() {
    let f = family();
    let t = "[:find ?a :at 0 :rules [[(loop ?a) (loop ?a)]] :where (loop ?a)]";
    assert!(go(&f.w, t).is_empty());
    let t = "[:find ?a :at 0 :rules [[(p ?a) (q ?a)] [(q ?a) (p ?a)]] :where (p ?a)]";
    assert!(go(&f.w, t).is_empty());
}

// ----------------------------------------------------- per-operator tick

#[test]
fn the_default_tick_reaches_into_the_recursion() {
    let f = family();
    // bob -> cat holds at -200, not at -100
    assert!(
        go(&f.w, &q(-200, ANC, "(ancestor :ann ?d)", "?d"))
            .contains(&f.refs(&["cat"]).into_iter().next().unwrap())
    );
    assert!(
        !go(&f.w, &q(-100, ANC, "(ancestor :ann ?d)", "?d"))
            .contains(&f.refs(&["cat"]).into_iter().next().unwrap())
    );
}

#[test]
fn an_at_clause_inside_the_recursive_body_ignores_the_default() {
    let f = family();
    // base reads at :at; the recursive hop reads at -200 whatever :at says.
    let rules = "[(anc-then ?a ?d) [?a :parent_of ?d]] \
                 [(anc-then ?a ?d) (at -200 [?a :parent_of ?x]) (anc-then ?x ?d)]";
    // :at 0: base {ann->bob,eve; bob->dan; eve->fay; cat->gus}; hops @-200 reach
    // cat through bob, whose base child is gus (cat itself is never a base child).
    let want = f.refs(&["bob", "eve", "dan", "fay", "gus"]);
    for t in [0, -100] {
        assert_eq!(
            go(&f.w, &q(t, rules, "(anc-then :ann ?d)", "?d")),
            want,
            "at {t}"
        );
    }
    // nothing holds @-400, and the hop alone can't start a derivation
    assert!(go(&f.w, &q(-400, rules, "(anc-then :ann ?d)", "?d")).is_empty());
    // the plain ancestor query at 0 differs: no gus
    assert_eq!(
        go(&f.w, &q(0, ANC, "(ancestor :ann ?d)", "?d")),
        f.refs(&["bob", "eve", "dan", "fay"])
    );
}

#[test]
fn an_at_clause_in_the_base_case_of_a_recursive_rule() {
    let f = family();
    // base fixed at -200: everything derived is the -200 closure, whatever :at is
    let rules = "[(anc-then ?a ?d) (at -200 [?a :parent_of ?d])] \
                 [(anc-then ?a ?d) (at -200 [?a :parent_of ?x]) (anc-then ?x ?d)]";
    for t in [-400, -200, 0, 500] {
        assert_eq!(
            go(&f.w, &q(t, rules, "(anc-then :ann ?d)", "?d")),
            f.refs(&["bob", "eve", "cat", "fay", "gus"]),
            "at {t}"
        );
    }
}

#[test]
fn a_query_can_compare_two_moments_through_two_recursive_rules() {
    let f = family();
    // descendants of ann at -200 AND at 0: a join of two closures at fixed ticks
    let rules = "[(anc-a ?a ?d) (at -200 [?a :parent_of ?d])] \
                 [(anc-a ?a ?d) (at -200 [?a :parent_of ?x]) (anc-a ?x ?d)] \
                 [(anc-b ?a ?d) (at 0 [?a :parent_of ?d])] \
                 [(anc-b ?a ?d) (at 0 [?a :parent_of ?x]) (anc-b ?x ?d)]";
    let text =
        format!("[:find ?d :at 600 :rules [{rules}] :where (anc-a :ann ?d) (anc-b :ann ?d)]");
    assert_eq!(go(&f.w, &text), f.refs(&["bob", "eve", "fay"]));
}

// -------------------------------------------------- semi-naive efficiency

fn chain(n: usize) -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    {
        let mut t = w.begin();
        let es: Vec<EntityId> = (0..n)
            .map(|i| {
                let e = t
                    .create_entity("CHARACTER", Some(&format!("n{i}")))
                    .unwrap();
                ids.insert(format!("n{i}"), e);
                e
            })
            .collect();
        for p in es.windows(2) {
            t.assert(p[0], "parent_of", Value::Ref(p[1]), Some(Tick(0)))
                .unwrap();
        }
        t.commit().unwrap();
    }
    W { w, ids }
}

/// Chain length for the efficiency tests: the closure has N(N-1)/2 = 79,800
/// tuples, and naive re-derivation (every round re-derives the whole closure
/// so far, about N^3/3 = 21 million steps) would blow the 10,000,000-step
/// default, while semi-naive needs about N^2.
const N: usize = 400;

#[test]
fn semi_naive_closure_of_a_long_chain_fits_the_default_step_budget() {
    let f = chain(N);
    let lim = Limits {
        max_rows: 200_000,   // the closure is 79,800 tuples
        ..Limits::default()  // 10,000,000 steps
    };
    let text = q(0, ANC, "(ancestor ?a ?d)", "?a ?d");
    let got = try_go(&f.w, &text, &lim).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got.len(), N * (N - 1) / 2);
    assert!(got.contains(&f.pairs(&[("n0", "n399")]).into_iter().next().unwrap()));
    assert!(!got.contains(&f.pairs(&[("n399", "n0")]).into_iter().next().unwrap()));
    // left recursion is no worse
    let left = try_go(&f.w, &q(0, ANC_LEFT, "(ancestor ?a ?d)", "?a ?d"), &lim)
        .unwrap_or_else(|e| panic!("left: {e}"));
    assert_eq!(left, got);
}

#[test]
fn semi_naive_from_a_constant_on_a_long_chain() {
    let f = chain(N);
    let got = go(&f.w, &q(0, ANC, "(ancestor :n0 ?d)", "?d"));
    assert_eq!(got.len(), N - 1);
}

// ---------------------------------------------------------------- limits

#[test]
fn a_closure_larger_than_max_rows_is_a_row_limit_error() {
    let f = chain(60); // 1,770 tuples
    let lim = Limits {
        max_rows: 500,
        ..Limits::default()
    };
    // the final result is one row; the relation is what exceeds the limit
    let text = q(0, ANC, "(ancestor :n0 :n59) [:n0 :parent_of ?x]", "?x");
    match try_go(&f.w, &text, &lim) {
        Err(QueryError::RowLimit { limit: 500 }) => {}
        other => panic!("expected RowLimit, got {other:?}"),
    }
    // the same query is fine under the default limits
    assert_eq!(go(&f.w, &text).len(), 1);
}

#[test]
fn a_small_step_budget_stops_the_recursion() {
    let f = chain(N);
    let lim = Limits {
        max_rows: 200_000,
        max_steps: 50_000,
        ..Limits::default()
    };
    match try_go(&f.w, &q(0, ANC, "(ancestor ?a ?d)", "?a ?d"), &lim) {
        Err(QueryError::StepBudget { limit: 50_000 }) => {}
        other => panic!("expected StepBudget, got {other:?}"),
    }
}

#[test]
fn a_small_step_budget_stops_a_cyclic_recursion() {
    // a 200-node ring: the closure is 40,000 tuples
    let names: Vec<String> = (0..200).map(|i| format!("n{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut edges: Vec<(&str, &str, i64, Option<i64>)> =
        refs.windows(2).map(|p| (p[0], p[1], 0, None)).collect();
    edges.push((refs[199], refs[0], 0, None));
    let f = world(&refs, &edges, &[]);
    let lim = Limits {
        max_steps: 20_000,
        ..Limits::default()
    };
    match try_go(&f.w, &q(0, ANC, "(ancestor ?a ?d)", "?a ?d"), &lim) {
        Err(QueryError::StepBudget { limit: 20_000 }) => {}
        other => panic!("expected StepBudget, got {other:?}"),
    }
}

#[test]
fn a_small_byte_budget_stops_the_recursion() {
    let f = chain(200); // 19,900 tuples
    let lim = Limits {
        max_bytes: 100 * 1024,
        ..Limits::default()
    };
    match try_go(&f.w, &q(0, ANC, "(ancestor ?a ?d)", "?a ?d"), &lim) {
        Err(QueryError::MemoryBudget { limit }) => assert_eq!(limit, 100 * 1024),
        other => panic!("expected MemoryBudget, got {other:?}"),
    }
}
