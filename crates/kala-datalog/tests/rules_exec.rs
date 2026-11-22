//! Slice 7, item 2 (RED): non-recursive Datalog rules, end to end through
//! `prepare` + `run`. Results are sets and equal the hand-expanded query.
//!
//! Family world (tick of each fact in brackets; `parent_of` has inverse
//! `child_of`):
//!   ann parent_of bob [-300]          ann parent_of eve [-300]
//!   bob parent_of cat [-250, retracted -150]
//!   bob parent_of dan [-100]          eve parent_of fay [-300]
//!   cat ally_of fay [-300] (bidirectional)   ann epoch = -200 [-400]
//! so grandchildren of ann: at -200 {cat, fay}; at -120 {fay}; at 0 {dan, fay}.

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_query::{Limits, Params, run};
use std::collections::BTreeSet;

type Rows = BTreeSet<Vec<Value>>;

struct Fam {
    w: World,
    ids: Vec<(&'static str, EntityId)>,
}

impl Fam {
    fn id(&self, n: &str) -> EntityId {
        self.ids.iter().find(|(k, _)| *k == n).unwrap().1
    }
    fn refs(&self, names: &[&str]) -> Rows {
        names.iter().map(|n| vec![Value::Ref(self.id(n))]).collect()
    }
}

fn fam() -> Fam {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(
            a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("epoch", ValueType::I64, Kind::State, Cardinality::One));
    let mut w = World::in_memory(schema).unwrap();
    let mut ids = vec![];
    for n in ["ann", "bob", "cat", "dan", "eve", "fay"] {
        ids.push((n, w.create_entity("CHARACTER", Some(n)).unwrap()));
    }
    let id = |n: &str| ids.iter().find(|(k, _)| *k == n).unwrap().1;
    let t = |x: i64| Some(Tick(x));
    let r = |n: &str| Value::Ref(id(n));
    w.assert(id("ann"), "parent_of", r("bob"), t(-300)).unwrap();
    w.assert(id("ann"), "parent_of", r("eve"), t(-300)).unwrap();
    w.assert(id("bob"), "parent_of", r("cat"), t(-250)).unwrap();
    w.retract(id("bob"), "parent_of", r("cat"), t(-150))
        .unwrap();
    w.assert(id("bob"), "parent_of", r("dan"), t(-100)).unwrap();
    w.assert(id("eve"), "parent_of", r("fay"), t(-300)).unwrap();
    w.assert(id("cat"), "ally_of", r("fay"), t(-300)).unwrap();
    w.assert(id("ann"), "epoch", Value::I64(-200), t(-400))
        .unwrap();
    w.assert(id("ann"), "name", Value::Str("Ann".into()), t(-400))
        .unwrap();
    Fam { w, ids }
}

fn go(w: &World, text: &str, params: &[(&str, Value)]) -> Rows {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n{}\n  {e}", p.plan.explain(w)))
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|d| d.as_value().unwrap().clone())
                .collect()
        })
        .collect()
}

fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Err(e) => e.kind,
        Ok(_) => panic!("expected an error for {text}"),
    }
}

const GP: &str = "[(grandparent ?g ?c) [?g :parent_of ?p] [?p :parent_of ?c]]";

fn gp_query(at: i64) -> String {
    format!("[:find ?c :at {at} :rules [{GP}] :where (grandparent :ann ?c)]")
}

// ----------------------------------------------------------- basic rules

#[test]
fn grandparent_rule_matches_hand_expansion_at_several_ticks() {
    let f = fam();
    for (t, want) in [
        (-200, vec!["cat", "fay"]),
        (-120, vec!["fay"]),
        (0, vec!["dan", "fay"]),
        (-350, vec![]),
    ] {
        let expanded = format!("[:find ?c :at {t} :where [:ann :parent_of ?p] [?p :parent_of ?c]]");
        let got = go(&f.w, &gp_query(t), &[]);
        assert_eq!(got, f.refs(&want), "tick {t}");
        assert_eq!(got, go(&f.w, &expanded, &[]), "hand-expanded, tick {t}");
    }
}

#[test]
fn rule_with_two_free_head_variables_returns_pairs() {
    let f = fam();
    let got = go(
        &f.w,
        &format!("[:find ?g ?c :at -200 :rules [{GP}] :where (grandparent ?g ?c)]"),
        &[],
    );
    let want: Rows = [("ann", "cat"), ("ann", "fay")]
        .iter()
        .map(|(g, c)| vec![Value::Ref(f.id(g)), Value::Ref(f.id(c))])
        .collect();
    assert_eq!(got, want);
}

#[test]
fn two_bodies_are_a_union() {
    let f = fam();
    let q = "[:find ?x :at 0 :rules [[(family ?a ?b) [?a :parent_of ?b]] \
             [(family ?a ?b) [?a :child_of ?b]]] :where (family :bob ?x)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["ann", "dan"])); // cat was retracted @-150
    let q = q.replace(":at 0", ":at -200");
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["ann", "cat"]));
}

#[test]
fn union_of_bodies_has_no_duplicate_rows() {
    let f = fam();
    // both bodies give (ann, bob) at -200: still one row
    let q = "[:find ?a ?b :at -200 :rules [[(r ?a ?b) [?a :parent_of ?b]] \
             [(r ?a ?b) [?a :parent_of ?b]]] :where (r ?a ?b)]";
    let plain = go(
        &f.w,
        "[:find ?a ?b :at -200 :where [?a :parent_of ?b]]",
        &[],
    );
    assert_eq!(go(&f.w, q, &[]), plain);
}

#[test]
fn a_rule_can_call_another_rule() {
    let f = fam();
    let q = format!(
        "[:find ?a :at -200 :rules [{GP} [(gp-ally ?g ?a) (grandparent ?g ?c) [?c :ally_of ?a]]] \
         :where (gp-ally :ann ?a)]"
    );
    // grandchildren cat, fay; cat ally fay, fay ally cat
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["cat", "fay"]));
    let q0 = q.replace(":at -200", ":at 0");
    // grandchildren dan, fay; only fay has an ally (cat)
    assert_eq!(go(&f.w, &q0, &[]), f.refs(&["cat"]));
}

#[test]
fn rule_order_in_the_section_does_not_matter() {
    let f = fam();
    let q = format!(
        "[:find ?a :at -200 :rules [[(gp-ally ?g ?a) (grandparent ?g ?c) [?c :ally_of ?a]] {GP}] \
         :where (gp-ally :ann ?a)]"
    );
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["cat", "fay"]));
}

#[test]
fn the_same_rule_called_twice_in_one_query() {
    let f = fam();
    // pairs of grandchildren of ann (cat, fay at -200)
    let q = format!(
        "[:find ?x ?y :at -200 :rules [{GP}] :where (grandparent :ann ?x) (grandparent :ann ?y) [(!= ?x ?y)]]"
    );
    let want: Rows = [("cat", "fay"), ("fay", "cat")]
        .iter()
        .map(|(a, b)| vec![Value::Ref(f.id(a)), Value::Ref(f.id(b))])
        .collect();
    assert_eq!(go(&f.w, &q, &[]), want);
}

// ------------------------------------------------------- call arguments

#[test]
fn constants_in_a_call_filter_the_rule() {
    let f = fam();
    // constant on the first position, variable on the second
    let by_handle = go(&f.w, &gp_query(-200), &[]);
    assert_eq!(by_handle, f.refs(&["cat", "fay"]));
    // both positions constants: a yes/no question with a bound variable
    let both = format!(
        "[:find ?x :at -200 :rules [{GP}] :where [?x :name \"Ann\"] (grandparent :ann :cat)]"
    );
    assert_eq!(go(&f.w, &both, &[]), f.refs(&["ann"]));
    let neither = both.replace(":cat)", ":dan)");
    assert_eq!(go(&f.w, &neither, &[]), Rows::new());
}

#[test]
fn id_constants_and_params_in_a_call() {
    let f = fam();
    let ann = f.w.entity_by_handle("ann").unwrap();
    let q = format!("[:find ?c :in $ ?g :at -200 :rules [{GP}] :where (grandparent ?g ?c)]");
    assert_eq!(
        go(&f.w, &q, &[("g", Value::Ref(ann))]),
        f.refs(&["cat", "fay"])
    );
}

#[test]
fn underscore_args_match_anything_and_bind_nothing() {
    let f = fam();
    let q = format!("[:find ?c :at -200 :rules [{GP}] :where (grandparent _ ?c)]");
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["cat", "fay"]));
    // two underscores are independent: "is there any grandparent pair?"
    let q =
        format!("[:find ?x :at -200 :rules [{GP}] :where [?x :name \"Ann\"] (grandparent _ _)]");
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["ann"]));
    let q = q.replace("-200", "-350");
    assert_eq!(go(&f.w, &q, &[]), Rows::new());
}

#[test]
fn a_repeated_variable_in_a_call_means_equality() {
    let f = fam();
    // no one is their own grandparent
    let q = format!("[:find ?x :at 0 :rules [{GP}] :where (grandparent ?x ?x)]");
    assert_eq!(go(&f.w, &q, &[]), Rows::new());
}

// --------------------------------------------------------------- ticks

#[test]
fn rule_patterns_read_at_the_query_default_tick() {
    let f = fam();
    // the same rule text, only :at differs
    assert_eq!(go(&f.w, &gp_query(-200), &[]), f.refs(&["cat", "fay"]));
    assert_eq!(go(&f.w, &gp_query(0), &[]), f.refs(&["dan", "fay"]));
}

#[test]
fn at_inside_a_rule_body_overrides_the_default() {
    let f = fam();
    let q = |at: i64| {
        format!(
            "[:find ?c :at {at} :rules [[(gp-then ?g ?c) \
             (at -200 [?g :parent_of ?p] [?p :parent_of ?c])]] :where (gp-then :ann ?c)]"
        )
    };
    for t in [-350, -200, 0, 600] {
        assert_eq!(go(&f.w, &q(t), &[]), f.refs(&["cat", "fay"]), ":at {t}");
    }
}

#[test]
fn at_wraps_only_the_patterns_it_names() {
    let f = fam();
    // first hop at -200, second hop at the default (0): ann->bob(-200) , bob->{dan}(0)
    let q = "[:find ?c :at 0 :rules [[(mixed ?g ?c) (at -200 [?g :parent_of ?p]) [?p :parent_of ?c]]] \
             :where (mixed :ann ?c)]";
    // p in {bob, eve} at -200; their children at 0: bob->dan, eve->fay
    assert_eq!(go(&f.w, q, &[]), f.refs(&["dan", "fay"]));
}

#[test]
fn at_with_a_body_bound_variable_tick() {
    let f = fam();
    // ann's epoch is -200, so the hops read at -200 whatever :at says
    let q = |at: i64| {
        format!(
            "[:find ?c :at {at} :rules [[(gp-epoch ?g ?c) [?g :epoch ?t] \
             (at ?t [?g :parent_of ?p] [?p :parent_of ?c])]] :where (gp-epoch :ann ?c)]"
        )
    };
    assert_eq!(go(&f.w, &q(0), &[]), f.refs(&["cat", "fay"]));
    assert_eq!(go(&f.w, &q(-120), &[]), f.refs(&["cat", "fay"]));
}

#[test]
fn a_param_used_inside_a_rule_body() {
    let f = fam();
    let q = "[:find ?c :in $ ?t :at 0 :rules [[(gp-then ?g ?c) \
             (at ?t [?g :parent_of ?p] [?p :parent_of ?c])]] :where (gp-then :ann ?c)]";
    assert_eq!(
        go(&f.w, q, &[("t", Value::I64(-200))]),
        f.refs(&["cat", "fay"])
    );
    assert_eq!(go(&f.w, q, &[("t", Value::I64(-120))]), f.refs(&["fay"]));
    assert_eq!(
        go(&f.w, q, &[("t", Value::I64(0))]),
        f.refs(&["dan", "fay"])
    );
}

#[test]
fn a_param_as_a_filter_value_inside_a_rule_body() {
    let f = fam();
    let q = "[:find ?x :in $ ?n :at 0 :rules [[(named ?x) [?x :name ?n]]] :where (named ?x)]";
    assert_eq!(
        go(&f.w, q, &[("n", Value::Str("Ann".into()))]),
        f.refs(&["ann"])
    );
    assert_eq!(go(&f.w, q, &[("n", Value::Str("Zed".into()))]), Rows::new());
}

// ------------------------------------------------------ scoping, predicates

#[test]
fn variables_are_local_to_each_definition_and_to_where() {
    let f = fam();
    // ?p is used inside the rule and again in :where: unrelated.
    let q = format!(
        "[:find ?c ?p :at 0 :rules [{GP}] :where (grandparent :ann ?c) [:bob :parent_of ?p]]"
    );
    let want: Rows = ["dan", "fay"]
        .iter()
        .flat_map(|c| ["dan"].iter().map(move |p| (*c, *p))) // bob's child at 0: dan only
        .map(|(c, p)| vec![Value::Ref(f.id(c)), Value::Ref(f.id(p))])
        .collect();
    assert_eq!(go(&f.w, &q, &[]), want);
}

#[test]
fn head_names_can_differ_between_definitions_of_one_rule() {
    let f = fam();
    let q = "[:find ?x :at 0 :rules [[(kin ?a ?b) [?a :parent_of ?b]] [(kin ?x ?y) [?y :parent_of ?x]]] \
             :where (kin :bob ?x)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["ann", "dan"]));
}

#[test]
fn predicates_inside_a_rule_body() {
    let f = fam();
    let q = "[:find ?a ?b :at 0 :rules [[(sib ?a ?b) [?p :parent_of ?a] [?p :parent_of ?b] [(!= ?a ?b)]]] \
             :where (sib ?a ?b)]";
    // at 0: ann->{bob, eve}; bob->{dan}; eve->{fay}
    let want: Rows = [("bob", "eve"), ("eve", "bob")]
        .iter()
        .map(|(a, b)| vec![Value::Ref(f.id(a)), Value::Ref(f.id(b))])
        .collect();
    assert_eq!(go(&f.w, q, &[]), want);
}

#[test]
fn a_rule_that_is_defined_but_never_called_is_harmless() {
    let f = fam();
    let q = format!("[:find ?x :at 0 :rules [{GP}] :where [?x :name \"Ann\"]]");
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["ann"]));
}

// ---------------------------------------------------------------- explain

#[test]
fn explain_shows_fixpoint_define_and_rel() {
    let f = fam();
    let p = prepare(&f.w, &gp_query(-200)).unwrap();
    let shown = p.plan.explain(&f.w);
    for needle in ["Fixpoint", "Define", "Rel"] {
        assert!(shown.contains(needle), "no `{needle}` in:\n{shown}");
    }
}

// ----------------------------------------------------------------- errors

#[test]
fn unknown_rule_is_unknown_rule() {
    let f = fam();
    assert_eq!(
        kind(&f.w, "[:find ?c :where (nope :ann ?c)]"),
        ErrorKind::UnknownRule
    );
    assert_eq!(
        kind(&f.w, "[:find ?c :rules [[(r ?a) (nope ?a)]] :where (r ?c)]"),
        ErrorKind::UnknownRule
    );
}

#[test]
fn unknown_rule_error_is_located() {
    let f = fam();
    let t = "[:find ?c :where (nope :ann ?c)]";
    let e = prepare(&f.w, t).expect_err("unknown rule");
    assert_eq!(e.kind, ErrorKind::UnknownRule);
    let sp = e.span.expect("span");
    assert!(t[sp.start..sp.end].contains("nope"), "{sp:?}");
}

#[test]
fn call_arity_mismatch_is_arity() {
    let f = fam();
    for args in [":ann", ":ann ?c ?d", ""] {
        let q = format!("[:find ?c :rules [{GP}] :where [?c :name ?n] (grandparent {args})]");
        assert_eq!(kind(&f.w, &q), ErrorKind::Arity, "args `{args}`");
    }
}

#[test]
fn definitions_of_one_name_must_share_an_arity() {
    let f = fam();
    let q =
        "[:find ?c :rules [[(r ?a) [?a :name ?n]] [(r ?a ?b) [?a :parent_of ?b]]] :where (r ?c)]";
    assert_eq!(kind(&f.w, q), ErrorKind::Arity);
}

#[test]
fn unbound_head_variable_is_unbound_variable() {
    let f = fam();
    let q = "[:find ?a :rules [[(bad ?a ?z) [?a :parent_of ?b]]] :where (bad ?a ?z)]";
    assert_eq!(kind(&f.w, q), ErrorKind::UnboundVariable);
    // in one of several bodies is enough
    let q = "[:find ?a :rules [[(bad ?a ?b) [?a :parent_of ?b]] [(bad ?a ?b) [?a :name ?n]]] :where (bad ?a ?b)]";
    assert_eq!(kind(&f.w, q), ErrorKind::UnboundVariable);
}

#[test]
fn a_variable_bound_only_in_where_does_not_bind_the_head() {
    let f = fam();
    // ?z is bound in :where but variables are local to each definition
    let q = "[:find ?z :rules [[(bad ?a ?z) [?a :parent_of ?b]]] :where [?z :name ?n] (bad ?z ?z)]";
    assert_eq!(kind(&f.w, q), ErrorKind::UnboundVariable);
}

#[test]
fn a_head_variable_bound_by_a_rule_call_is_safe() {
    let f = fam();
    let q = format!(
        "[:find ?c :at -200 :rules [{GP} [(wrap ?g ?c) (grandparent ?g ?c)]] :where (wrap :ann ?c)]"
    );
    assert_eq!(go(&f.w, &q, &[]), f.refs(&["cat", "fay"]));
}

#[test]
fn constant_of_the_wrong_type_at_a_call_is_type_mismatch() {
    let f = fam();
    // 2nd position of grandparent is a ref
    let q = format!("[:find ?g :rules [{GP}] :where (grandparent ?g \"x\")]");
    assert_eq!(kind(&f.w, &q), ErrorKind::TypeMismatch);
    let q = format!("[:find ?g :rules [{GP}] :where (grandparent ?g 5)]");
    assert_eq!(kind(&f.w, &q), ErrorKind::TypeMismatch);
}

#[test]
fn head_type_is_unified_across_definitions() {
    let f = fam();
    // ?y is a ref in one body and a string in the other
    let q = "[:find ?x :rules [[(r ?x ?y) [?x :parent_of ?y]] [(r ?x ?y) [?x :name ?y]]] :where (r ?x ?y)]";
    assert_eq!(kind(&f.w, q), ErrorKind::TypeMismatch);
}

#[test]
fn head_type_is_unified_across_call_sites() {
    let f = fam();
    // the where clause uses ?v as a ref (via the call) and as a string
    let q = format!("[:find ?v :rules [{GP}] :where (grandparent :ann ?v) [?v :name ?v]]");
    assert_eq!(kind(&f.w, &q), ErrorKind::TypeMismatch);
}

#[test]
fn a_param_of_two_types_across_a_rule_call_is_type_mismatch() {
    let f = fam();
    let q = format!("[:find ?c :in $ ?x :rules [{GP}] :where (grandparent ?x ?c) [?c :name ?x]]");
    assert_eq!(kind(&f.w, &q), ErrorKind::TypeMismatch);
}

// -------------------------------------------------------------- recursion

// The three item-2 tests that pinned recursion as `Unsupported`
// (recursive_rule_is_unsupported_until_item_3, mutual_recursion_is_unsupported_until_item_3,
// a_recursive_rule_is_unsupported_even_when_never_called) were removed with item 3:
// recursion.rs now covers the same queries as result tests (ancestors, even/odd
// mutual recursion, `a_recursive_rule_that_is_never_called_is_harmless`,
// `a_recursive_rule_with_no_base_case_is_empty`).

#[test]
fn diamond_calls_are_not_recursion() {
    let f = fam();
    let q = "[:find ?x :at 0 :rules [[(base ?a) [?a :parent_of ?b]] [(l ?a) (base ?a)] [(r ?a) (base ?a)] \
             [(top ?a) (l ?a) (r ?a)]] :where (top ?x)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["ann", "bob", "eve"]));
}

// ----------------------------------------------------------- the fixture

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

#[test]
fn varrow_co_located_at_a_tick() {
    // Who shares a place with Osric at tick -200 (`located_in`)?
    // Osric: in varrow from -400, never moves.
    // In varrow at -200 (assert <= -200 and no later retract <= -200):
    //   osric (-400), tomas (-350), mara (-220, leaves -190: still there),
    //   v01 (-395..-95), v15 (-325..-25).
    // Not there: hild (left varrow at -230), edwin (left at -250), v10 (arrives -50),
    //   v24 (arrives 20), greg (arrives 200).
    // The rule includes Osric himself (a == b is allowed).
    let w = load_fixture(VARROW).unwrap();
    let q = "[:find ?b :at -200 \
             :rules [[(co-located ?a ?b) [?a :located_in ?p] [?b :located_in ?p]]] \
             :where (co-located :osric ?b)]";
    let want: Rows = ["osric", "tomas", "mara", "v01", "v15"]
        .iter()
        .map(|h| vec![Value::Ref(w.entity_by_handle(h).unwrap())])
        .collect();
    assert_eq!(go(&w, q, &[]), want);
    // and it agrees with the hand-expanded query
    let hand = "[:find ?b :at -200 :where [:osric :located_in ?p] [?b :located_in ?p]]";
    assert_eq!(go(&w, hand, &[]), want);
}
