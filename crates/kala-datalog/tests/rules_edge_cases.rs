//! Slice 7, item 2 (confirm): rule edge cases found while implementing.
//! Same family world as rules_exec.rs plus an f64 `weight`.
//!
//!   ann parent_of bob [-300]          ann parent_of eve [-300]
//!   bob parent_of cat [-250, retracted -150]
//!   bob parent_of dan [-100]          eve parent_of fay [-300]
//!   ann epoch = -200 [-400]           ann weight 3.0, bob weight 4.5 [-400]

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
        .attribute(
            a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(a("epoch", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(a("weight", ValueType::F64, Kind::State, Cardinality::One));
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
    w.assert(id("ann"), "epoch", Value::I64(-200), t(-400))
        .unwrap();
    w.assert(id("ann"), "weight", Value::F64(3.0), t(-400))
        .unwrap();
    w.assert(id("bob"), "weight", Value::F64(4.5), t(-400))
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

// ------------------------------------------- head variables vs parameters

#[test]
fn a_head_variable_named_like_a_param_shadows_it_inside_the_definition() {
    let f = fam();
    // ?g is a param (= ann) AND the rule's head variable: in the definition it is
    // the head variable, so `(kids :bob ?c)` asks about bob, not ann.
    let q = "[:find ?c :in $ ?g :at -200 :rules [[(kids ?g ?c) [?g :parent_of ?c]]] \
             :where (kids :bob ?c) [?g :epoch ?e]]";
    let ann = Value::Ref(f.id("ann"));
    assert_eq!(go(&f.w, q, &[("g", ann)]), f.refs(&["cat"]));
}

#[test]
fn a_body_variable_named_like_a_param_refers_to_the_param() {
    let f = fam();
    // ?g is not in the head, so it is the param: always ann's children.
    let q = "[:find ?c :in $ ?g :at -200 :rules [[(kid ?c) [?g :parent_of ?c]]] :where (kid ?c)]";
    let ann = Value::Ref(f.id("ann"));
    assert_eq!(go(&f.w, q, &[("g", ann)]), f.refs(&["bob", "eve"]));
    let bob = Value::Ref(f.id("bob"));
    assert_eq!(go(&f.w, q, &[("g", bob)]), f.refs(&["cat"]));
}

#[test]
fn shadowing_in_one_definition_does_not_leak_to_another() {
    let f = fam();
    // def 1: ?g is the head var (shadows); def 2: ?g is the param (= bob).
    let q = "[:find ?c :in $ ?g :at -200 :rules [[(k ?g ?c) [?g :parent_of ?c]] \
             [(k2 ?x ?c) [?x :epoch ?e] [?g :parent_of ?c]]] :where (k :ann ?c)]";
    let bob = Value::Ref(f.id("bob"));
    assert_eq!(go(&f.w, q, &[("g", bob.clone())]), f.refs(&["bob", "eve"]));
    let q2 = q.replace("(k :ann ?c)", "(k2 :ann ?c)");
    assert_eq!(go(&f.w, &q2, &[("g", bob)]), f.refs(&["cat"]));
}

// --------------------------------------------------------- body shapes

#[test]
fn a_definition_whose_body_is_only_a_call() {
    let f = fam();
    let q = "[:find ?c :at -200 :rules [[(base ?a ?b) [?a :parent_of ?b]] [(wrap ?a ?b) (base ?a ?b)] \
             [(wrap2 ?a ?b) (wrap ?a ?b)]] :where (wrap2 :bob ?c)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["cat"]));
    // the head may reorder the called rule's columns
    let q = "[:find ?p :at -200 :rules [[(base ?a ?b) [?a :parent_of ?b]] [(flip ?y ?x) (base ?x ?y)]] \
             :where (flip :cat ?p)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["bob"]));
}

#[test]
fn a_call_only_where_clause() {
    let f = fam();
    let q = "[:find ?a ?b :at -200 :rules [[(base ?a ?b) [?a :parent_of ?b]]] :where (base ?a ?b)]";
    assert_eq!(go(&f.w, q, &[]).len(), 4); // ann->bob,eve; bob->cat; eve->fay (bob->dan not yet)
}

// ------------------------------------------------------------- ticks

#[test]
fn at_with_a_tick_variable_bound_only_by_a_rule_call() {
    let f = fam();
    let q = |at: i64| {
        format!(
            "[:find ?c :at {at} :rules [[(epoch-of ?a ?t) [?a :epoch ?t]] \
             [(gp-then ?g ?c) (epoch-of ?g ?t) (at ?t [?g :parent_of ?p] [?p :parent_of ?c])]] \
             :where (gp-then :ann ?c)]"
        )
    };
    for t in [-350, 0, 600] {
        assert_eq!(go(&f.w, &q(t), &[]), f.refs(&["cat", "fay"]), ":at {t}");
    }
}

#[test]
fn at_in_where_with_a_tick_variable_bound_only_by_a_rule_call() {
    let f = fam();
    let q = "[:find ?c :at 0 :rules [[(epoch-of ?a ?t) [?a :epoch ?t]]] \
             :where (epoch-of :ann ?t) (at ?t [:ann :parent_of ?c])]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["bob", "eve"]));
}

// -------------------------------------------------------------- types

#[test]
fn an_integer_constant_in_an_f64_head_position_is_a_float() {
    let f = fam();
    let q = "[:find ?e :at 0 :rules [[(heavy ?e ?w) [?e :weight ?w]]] :where (heavy ?e 3)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["ann"]));
    let q = q.replace("3)", "4)");
    assert_eq!(go(&f.w, &q, &[]), Rows::new()); // bob weighs 4.5, not 4
    let q = "[:find ?e :at 0 :rules [[(heavy ?e ?w) [?e :weight ?w]]] :where (heavy ?e 3.0)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["ann"]));
}

#[test]
fn an_integer_constant_in_an_f64_head_position_through_two_calls() {
    let f = fam();
    let q = "[:find ?e :at 0 :rules [[(heavy ?e ?w) [?e :weight ?w]] [(h2 ?e ?w) (heavy ?e ?w)]] \
             :where (h2 ?e 3)]";
    assert_eq!(go(&f.w, q, &[]), f.refs(&["ann"]));
}

#[test]
fn a_string_constant_in_an_f64_head_position_is_a_type_mismatch() {
    let f = fam();
    let q = "[:find ?e :rules [[(heavy ?e ?w) [?e :weight ?w]]] :where (heavy ?e \"3\")]";
    assert_eq!(kind(&f.w, q), ErrorKind::TypeMismatch);
}

#[test]
fn a_float_constant_in_an_i64_head_position_is_a_type_mismatch() {
    let f = fam();
    let q = "[:find ?e :rules [[(ep ?e ?t) [?e :epoch ?t]]] :where (ep ?e -200.5)]";
    assert_eq!(kind(&f.w, q), ErrorKind::TypeMismatch);
}
