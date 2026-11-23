//! Item 4 (RED): `kala_datalog::{compile, prepare, Prepared, ParamKind}`.
//!
//! Compiled plans are tested by EXECUTING them (`kala_query::run`) and comparing with
//! the native kala-core API; no test here pins pattern order or `Access` choice (item 5).
//!
//! Pinned readings of the contract (see the report's open decisions):
//! - `Prepared.params` / `Plan.params` names are written without `?`/`$`, in `:in` order, `$` excluded.
//! - the value position of `:kala_handle` is a string, so a param there is `Value(Str)`.
use kala_core::*;
use kala_datalog::ast;
use kala_datalog::{DatalogError, ErrorKind, ParamKind, Prepared, compile, parse, prepare};
use kala_query::{CanonSource, Limits, Params, ResultSet, TickSource, Var, run};
use std::collections::BTreeSet;

pub const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

/// ann=CHARACTER#1 bob=#2 cy=#3 di=#4; home=PLACE#1 mill=PLACE#2. Latest tick is 20.
pub const SMALL: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name,       type: string }
    - { name: age,        type: i64 }
    - { name: seen_at,    type: i64 }
    - { name: weight,     type: f64 }
    - { name: alive,      type: bool }
    - { name: located_in, type: ref }
    - { name: ally_of,    type: ref, cardinality: many, direction: bidirectional }
    - { name: parent_of,  type: ref, cardinality: many, inverse: child_of }
    - { name: likes,      type: ref, cardinality: many }
    - { name: disguised,  type: bool }
entities:
  CHARACTER: [ann, bob, cy, di]
  PLACE: [home, mill]
transactions:
  - tick: 0
    ops:
      - { assert: [ann, name, "Ann"] }
      - { assert: [bob, name, "Bob"] }
      - { assert: [cy, name, "Cy"] }
      - { assert: [di, name, "Di"] }
      - { assert: [ann, age, 30] }
      - { assert: [bob, age, 25] }
      - { assert: [cy, age, 30] }
      - { assert: [di, age, 41] }
      - { assert: [ann, weight, 60.5] }
      - { assert: [bob, weight, 80.0] }
      - { assert: [cy, weight, 60.5] }
      - { assert: [ann, alive, true] }
      - { assert: [bob, alive, false] }
      - { assert: [cy, alive, true] }
      - { assert: [ann, seen_at, 25] }
      - { assert: [bob, seen_at, 5] }
      - { assert: [ann, located_in, home] }
      - { assert: [bob, located_in, home] }
      - { assert: [cy, located_in, mill] }
      - { assert: [di, located_in, home] }
      - { assert: [ann, parent_of, cy] }
      - { assert: [ann, parent_of, di] }
      - { assert: [ann, likes, bob] }
      - { assert: [cy, likes, cy] }
  - ops:
      - { assert: [ann, ally_of, bob], tick: 5 }
      - { assert: [[ann, ally_of, bob], disguised, true], tick: 5 }
      - { assert: [bob, ally_of, cy], tick: 7 }
  - ops:
      - { retract: [ann, ally_of, bob], tick: 15 }
  - ops:
      - { retract: [ann, located_in, home], tick: 20 }
      - { assert: [ann, located_in, mill], tick: 20 }
"#;

fn leak(yaml: &str) -> &'static World {
    Box::leak(Box::new(load_fixture(yaml).expect("fixture loads")))
}
fn varrow() -> &'static World {
    thread_local!(static W: &'static World = leak(VARROW));
    W.with(|w| *w)
}
fn small() -> &'static World {
    thread_local!(static W: &'static World = leak(SMALL));
    W.with(|w| *w)
}

fn id(w: &World, h: &str) -> EntityId {
    w.entity_by_handle(h)
        .unwrap_or_else(|| panic!("no handle {h}"))
}
fn r(w: &World, h: &str) -> Value {
    Value::Ref(id(w, h))
}
fn s(x: &str) -> Value {
    Value::Str(x.to_string())
}

fn prep(w: &World, text: &str) -> Prepared {
    match prepare(w, text) {
        Ok(p) => p,
        Err(e) => panic!("prepare failed for {text}\n  {e}"),
    }
}
fn exec(w: &World, text: &str, params: &[(&str, Value)]) -> ResultSet {
    let p = prep(w, text);
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    match run(w, &p.plan, &ps, &Limits::default()) {
        Ok(rs) => rs,
        Err(e) => panic!("run failed for {text}\n  {e}"),
    }
}
fn rows(w: &World, text: &str, params: &[(&str, Value)]) -> BTreeSet<Vec<Value>> {
    exec(w, text, params)
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|d| d.as_value().unwrap().clone())
                .collect()
        })
        .collect()
}
fn set(v: impl IntoIterator<Item = Vec<Value>>) -> BTreeSet<Vec<Value>> {
    v.into_iter().collect()
}
fn col(v: impl IntoIterator<Item = Value>) -> BTreeSet<Vec<Value>> {
    v.into_iter().map(|x| vec![x]).collect()
}
fn refs(w: &World, hs: &[&str]) -> BTreeSet<Vec<Value>> {
    col(hs.iter().map(|h| r(w, h)))
}

/// `prepare` must fail with `kind`; the span (if `needle` is given) must cover exactly `needle`.
fn cerr(w: &World, text: &str, kind: ErrorKind, needle: Option<&str>) -> DatalogError {
    let e = match prepare(w, text) {
        Ok(p) => panic!(
            "expected {kind:?} for {text}, got plan:\n{}",
            p.plan.explain(w)
        ),
        Err(e) => e,
    };
    assert_eq!(e.kind, kind, "wrong kind for {text}: {e}");
    let sp = e
        .span
        .unwrap_or_else(|| panic!("no span for {text}: {e:?}"));
    assert!(sp.start <= sp.end && sp.end <= text.len());
    assert!(text.is_char_boundary(sp.start) && text.is_char_boundary(sp.end));
    if let Some(n) = needle {
        assert_eq!(&text[sp.start..sp.end], n, "span of {text}: {e}");
    }
    assert!(
        e.to_string().starts_with("line "),
        "prepare errors are located: {e}"
    );
    e
}

// ---------------------------------------------------------------- structure

#[test]
fn plan_carries_find_params_default_tick_and_canon() {
    let w = small();
    let p = prep(
        w,
        "[:find ?n ?e :in $ ?t ?c :at ?t :canon ?c :where [?e :name ?n]]",
    );
    assert_eq!(p.plan.find, vec![Var("n".into()), Var("e".into())]);
    assert_eq!(p.plan.params, vec!["t".to_string(), "c".to_string()]);
    assert_eq!(p.plan.default_tick, TickSource::Param("t".into()));
    assert_eq!(p.plan.canon, CanonSource::Param("c".into()));
    assert_eq!(
        p.params,
        vec![
            ("t".to_string(), ParamKind::Tick),
            ("c".to_string(), ParamKind::Canon)
        ]
    );
}

#[test]
fn constant_at_and_canon_reach_the_plan() {
    let p = prep(
        small(),
        "[:find ?n :at -210 :canon 41 :where [?e :name ?n]]",
    );
    assert_eq!(p.plan.default_tick, TickSource::Const(Tick(-210)));
    assert_eq!(p.plan.canon, CanonSource::Const(41));
    assert!(p.plan.params.is_empty() && p.params.is_empty());
}

#[test]
fn no_at_no_canon_means_default_and_latest() {
    let p = prep(small(), "[:find ?n :where [?e :name ?n]]");
    assert_eq!(p.plan.default_tick, TickSource::Default);
    assert_eq!(p.plan.canon, CanonSource::Latest);
}

#[test]
fn compile_of_parse_equals_prepare() {
    let w = varrow();
    let t = "[:find ?n :in $ ?who :at -210 :where [?e :kala_handle ?who] [?e :ally_of ?a] [?a :name ?n] [(!= ?a ?e)]]";
    let q: ast::Query = parse(t).unwrap();
    assert_eq!(compile(w, &q).unwrap(), prepare(w, t).unwrap().plan);
}

#[test]
fn prepare_is_deterministic() {
    let w = varrow();
    let t = "[:find ?a ?p :where [?e :ally_of ?a] [?a :located_in ?p] [?a :name ?n]]";
    assert_eq!(prepare(w, t).unwrap().plan, prepare(w, t).unwrap().plan);
}

#[test]
fn syntax_errors_pass_through_prepare() {
    cerr(
        small(),
        "[:find ?e :where [?e :name",
        ErrorKind::Syntax,
        None,
    );
}

// ---------------------------------------------------------------- terms

#[test]
fn variable_pattern_equals_attr_scan_at_latest_tick() {
    let w = small();
    let t = w.latest_tick();
    let expect = set(w
        .attr_scan("name", t)
        .unwrap()
        .into_iter()
        .map(|(e, v)| vec![Value::Ref(e), v]));
    assert_eq!(expect.len(), 4);
    assert_eq!(rows(w, "[:find ?e ?n :where [?e :name ?n]]", &[]), expect);
}

#[test]
fn find_order_is_the_columns_order() {
    let w = small();
    let rs = exec(w, "[:find ?n ?e :where [?e :name ?n]]", &[]);
    assert_eq!(rs.columns, vec![Var("n".into()), Var("e".into())]);
    assert_eq!(rs.rows[0].len(), 2);
    assert!(
        matches!(rs.rows[0][0].as_value(), Some(Value::Str(_)))
            && matches!(rs.rows[0][1].as_value(), Some(Value::Ref(_)))
    );
}

#[test]
fn handle_keyword_in_entity_position() {
    assert_eq!(
        rows(small(), "[:find ?n :where [:ann :name ?n]]", &[]),
        col([s("Ann")])
    );
}

#[test]
fn id_constant_in_entity_position() {
    assert_eq!(
        rows(small(), "[:find ?n :where [CHARACTER#2 :name ?n]]", &[]),
        col([s("Bob")])
    );
}

#[test]
fn edge_id_constant_in_entity_position() {
    let w = small();
    let e = w
        .edge_id(id(w, "ann"), "ally_of", id(w, "bob"))
        .expect("edge");
    let ids = w.format_id(EntityId::from(e));
    assert!(ids.starts_with("edge#"), "{ids}");
    let q = format!("[:find ?d :where [{ids} :disguised ?d]]");
    assert_eq!(rows(w, &q, &[]), col([Value::Bool(true)]));
    let q = format!("[:find ?d :where (at 4 [{ids} :disguised ?d])]");
    assert!(rows(w, &q, &[]).is_empty(), "the property starts at tick 5");
}

#[test]
fn handle_keyword_and_id_in_ref_value_position() {
    let w = small();
    let expect = refs(w, &["cy", "ann"]);
    assert_eq!(
        rows(w, "[:find ?x :where [?x :located_in :mill]]", &[]),
        expect
    );
    assert_eq!(
        rows(w, "[:find ?x :where [?x :located_in PLACE#2]]", &[]),
        expect
    );
}

#[test]
fn string_constant() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?e :where [?e :name \"Bob\"]]", &[]),
        refs(w, &["bob"])
    );
    assert!(
        rows(w, "[:find ?e :where [?e :name \"bob\"]]", &[]).is_empty(),
        "exact bytes"
    );
}

#[test]
fn i64_constant() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?e :where [?e :age 30]]", &[]),
        refs(w, &["ann", "cy"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :age +30]]", &[]),
        refs(w, &["ann", "cy"])
    );
}

#[test]
fn f64_constant_float_and_integer_literal() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?e :where [?e :weight 60.5]]", &[]),
        refs(w, &["ann", "cy"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :weight 80.0]]", &[]),
        refs(w, &["bob"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :weight 80]]", &[]),
        refs(w, &["bob"]),
        "an integer literal is fine for f64"
    );
}

#[test]
fn bool_constant() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?e :where [?e :alive true]]", &[]),
        refs(w, &["ann", "cy"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :alive false]]", &[]),
        refs(w, &["bob"])
    );
}

#[test]
fn any_matches_and_binds_nothing() {
    let w = small();
    let rs = exec(w, "[:find ?e :where [?e :located_in _]]", &[]);
    assert_eq!(rs.columns.len(), 1);
    assert_eq!(rs.rows.len(), 4);
    assert_eq!(
        rows(w, "[:find ?n :where [_ :name ?n]]", &[]),
        col([s("Ann"), s("Bob"), s("Cy"), s("Di")])
    );
    // two `_` in one query are independent
    assert_eq!(
        rows(w, "[:find ?e :where [?e :name _] [?e :alive _]]", &[]),
        refs(w, &["ann", "bob", "cy"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [_ :likes _] [?e :name \"Di\"]]", &[]),
        refs(w, &["di"])
    );
}

#[test]
fn same_variable_twice_in_one_pattern_must_agree() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?x :where [?x :likes ?x]]", &[]),
        refs(w, &["cy"])
    );
    assert!(rows(w, "[:find ?x :where [?x :located_in ?x]]", &[]).is_empty());
}

// ---------------------------------------------------------------- params

#[test]
fn param_kind_entity_from_entity_position() {
    let w = small();
    let p = prep(w, "[:find ?n :in $ ?e :where [?e :name ?n]]");
    assert_eq!(p.params, vec![("e".to_string(), ParamKind::Entity)]);
    assert_eq!(
        rows(
            w,
            "[:find ?n :in $ ?e :where [?e :name ?n]]",
            &[("e", r(w, "ann"))]
        ),
        col([s("Ann")])
    );
}

#[test]
fn param_kind_entity_from_ref_value_position() {
    let w = small();
    let t = "[:find ?x :in $ ?p :where [?x :located_in ?p]]";
    assert_eq!(
        prep(w, t).params,
        vec![("p".to_string(), ParamKind::Entity)]
    );
    assert_eq!(rows(w, t, &[("p", r(w, "home"))]), refs(w, &["bob", "di"]));
}

#[test]
fn param_kind_value_of_the_attribute_type() {
    let w = small();
    let cases = [
        (
            "[:find ?e :in $ ?v :where [?e :name ?v]]",
            ValueType::Str,
            s("Cy"),
            "cy",
        ),
        (
            "[:find ?e :in $ ?v :where [?e :age ?v]]",
            ValueType::I64,
            Value::I64(25),
            "bob",
        ),
        (
            "[:find ?e :in $ ?v :where [?e :weight ?v]]",
            ValueType::F64,
            Value::F64(80.0),
            "bob",
        ),
        (
            "[:find ?e :in $ ?v :where [?e :alive ?v]]",
            ValueType::Bool,
            Value::Bool(false),
            "bob",
        ),
    ];
    for (t, ty, v, who) in cases {
        assert_eq!(
            prep(w, t).params,
            vec![("v".to_string(), ParamKind::Value(ty))],
            "{t}"
        );
        assert_eq!(rows(w, t, &[("v", v)]), refs(w, &[who]), "{t}");
    }
}

#[test]
fn handle_value_param_is_a_string() {
    let w = varrow();
    let t = "[:find ?n :in $ ?who :where [?e :kala_handle ?who] [?e :name ?n]]";
    assert_eq!(
        prep(w, t).params,
        vec![("who".to_string(), ParamKind::Value(ValueType::Str))]
    );
    let got = rows(w, t, &[("who", s("edwin"))]);
    let native = w.as_of(id(w, "edwin"), "name", w.latest_tick()).unwrap();
    assert_eq!(got, col(native));
    assert!(!got.is_empty());
}

#[test]
fn in_without_db_marker_is_fine() {
    let w = small();
    let t = "[:find ?n :in ?e :where [?e :name ?n]]";
    assert_eq!(
        prep(w, t).params,
        vec![("e".to_string(), ParamKind::Entity)]
    );
    assert_eq!(rows(w, t, &[("e", r(w, "di"))]), col([s("Di")]));
}

#[test]
fn param_order_follows_in_order() {
    let w = small();
    let p = prep(
        w,
        "[:find ?e :in $ ?v ?t ?x :at ?t :where [?e :age ?v] [?e :located_in ?x]]",
    );
    assert_eq!(
        p.params,
        vec![
            ("v".to_string(), ParamKind::Value(ValueType::I64)),
            ("t".to_string(), ParamKind::Tick),
            ("x".to_string(), ParamKind::Entity),
        ]
    );
}

#[test]
fn param_kind_tick_from_at_clause_and_at_section() {
    let w = small();
    let t = "[:find ?p :in $ ?t :where (at ?t [:ann :located_in ?p])]";
    assert_eq!(prep(w, t).params, vec![("t".to_string(), ParamKind::Tick)]);
    assert_eq!(rows(w, t, &[("t", Value::I64(19))]), refs(w, &["home"]));
    assert_eq!(rows(w, t, &[("t", Value::I64(20))]), refs(w, &["mill"]));
    assert!(
        rows(w, t, &[("t", Value::I64(-1))]).is_empty(),
        "before the first fact"
    );
    let t2 = "[:find ?p :in $ ?t :at ?t :where [:ann :located_in ?p]]";
    assert_eq!(rows(w, t2, &[("t", Value::I64(19))]), refs(w, &["home"]));
}

#[test]
fn param_kind_canon() {
    let w = small();
    let t = "[:find ?e :in $ ?c :canon ?c :where [?e :name _]]";
    assert_eq!(prep(w, t).params, vec![("c".to_string(), ParamKind::Canon)]);
}

#[test]
fn param_used_as_both_tick_and_entity_is_a_type_mismatch() {
    cerr(
        small(),
        "[:find ?n :in $ ?x :where (at ?x [?x :name ?n])]",
        ErrorKind::TypeMismatch,
        None,
    );
}

#[test]
fn param_used_as_entity_and_string_is_a_type_mismatch() {
    cerr(
        small(),
        "[:find ?n :in $ ?x :where [?x :name ?n] [?y :name ?x]]",
        ErrorKind::TypeMismatch,
        None,
    );
}

#[test]
fn at_or_canon_param_not_in_in_is_unbound() {
    cerr(
        small(),
        "[:find ?n :at ?t :where [?e :name ?n]]",
        ErrorKind::UnboundVariable,
        Some("?t"),
    );
    cerr(
        small(),
        "[:find ?n :canon ?c :where [?e :name ?n]]",
        ErrorKind::UnboundVariable,
        Some("?c"),
    );
}

/// OPEN DECISION: the contract is silent on a declared-but-unused `:in` param.
/// Asserted only: no panic. Whatever it does, the result is reported.
#[test]
fn unused_in_param_does_not_panic() {
    let w = small();
    let out = prepare(w, "[:find ?n :in $ ?x :where [:ann :name ?n]]");
    eprintln!(
        "OPEN DECISION unused :in param -> {:?}",
        out.as_ref()
            .map(|p| p.params.clone())
            .map_err(|e| (e.kind, e.to_string()))
    );
}

// ---------------------------------------------------------------- ticks

#[test]
fn at_section_sets_the_default_tick() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?p :at 19 :where [:ann :located_in ?p]]", &[]),
        refs(w, &["home"])
    );
    assert_eq!(
        rows(w, "[:find ?p :where [:ann :located_in ?p]]", &[]),
        refs(w, &["mill"])
    );
    assert_eq!(
        exec(w, "[:find ?p :at 7 :where [:ann :located_in ?p]]", &[]).tick,
        Tick(7)
    );
}

#[test]
fn at_clause_with_constant_overrides_the_default() {
    let w = small();
    let t = "[:find ?p :at 100 :where (at 19 [:ann :located_in ?p])]";
    assert_eq!(rows(w, t, &[]), refs(w, &["home"]));
}

#[test]
fn at_clause_with_several_patterns() {
    let w = small();
    let t = "[:find ?e ?p :where (at 19 [?e :located_in ?p] [?e :alive true])]";
    let native: BTreeSet<Vec<Value>> = ["ann", "cy"]
        .iter()
        .flat_map(|h| {
            let e = id(w, h);
            w.as_of(e, "located_in", Tick(19))
                .unwrap()
                .into_iter()
                .map(move |p| vec![Value::Ref(e), p])
        })
        .collect();
    assert_eq!(rows(w, t, &[]), native);
}

#[test]
fn at_clause_with_variable_bound_by_an_i64_attribute() {
    let w = small();
    let t = "[:find ?e ?p :where [?e :seen_at ?t] (at ?t [?e :located_in ?p])]";
    let mut native = BTreeSet::new();
    for (e, v) in w.attr_scan("seen_at", w.latest_tick()).unwrap() {
        let Value::I64(t) = v else { panic!() };
        for p in w.as_of(e, "located_in", Tick(t)).unwrap() {
            native.insert(vec![Value::Ref(e), p]);
        }
    }
    assert_eq!(native.len(), 2);
    assert_eq!(rows(w, t, &[]), native);
    assert_eq!(
        native,
        set([
            vec![r(w, "ann"), r(w, "mill")],
            vec![r(w, "bob"), r(w, "home")]
        ])
    );
}

#[test]
fn comparing_two_moments_in_one_query() {
    let w = small();
    // allies of bob at both ticks (a conjunction of two reads)
    for (a, b) in [(8, 12), (6, 20), (8, 8)] {
        let t =
            format!("[:find ?x :where (at {a} [:bob :ally_of ?x]) (at {b} [:bob :ally_of ?x])]");
        let at = |t| -> BTreeSet<EntityId> {
            w.neighbors(id(w, "bob"), "ally_of", Direction::Both, Tick(t))
                .unwrap()
                .into_iter()
                .map(|(e, _)| e)
                .collect()
        };
        let expect: BTreeSet<_> = at(a).intersection(&at(b)).copied().collect();
        assert_eq!(
            rows(w, &t, &[]),
            col(expect.into_iter().map(Value::Ref)),
            "ticks {a} and {b}"
        );
    }
}

#[test]
fn at_variable_never_bound_is_unbound() {
    cerr(
        small(),
        "[:find ?p :where (at ?t [:ann :located_in ?p])]",
        ErrorKind::UnboundVariable,
        Some("?t"),
    );
    cerr(
        small(),
        "[:find ?p :in $ ?x :where (at ?t [:ann :located_in ?p])]",
        ErrorKind::UnboundVariable,
        Some("?t"),
    );
}

#[test]
fn at_variable_bound_to_a_non_i64_is_a_type_mismatch() {
    cerr(
        small(),
        "[:find ?p :where [?e :name ?t] (at ?t [?e :located_in ?p])]",
        ErrorKind::TypeMismatch,
        None,
    );
    cerr(
        small(),
        "[:find ?p :where [?e :located_in ?t] (at ?t [?e :located_in ?p])]",
        ErrorKind::TypeMismatch,
        None,
    );
    cerr(
        small(),
        "[:find ?p :where [?e :weight ?t] (at ?t [?e :located_in ?p])]",
        ErrorKind::TypeMismatch,
        None,
    );
}

/// OPEN DECISION: `(at ?t ..)` written BEFORE the pattern that binds ?t. The contract says
/// "bound earlier in the query"; the planner section says the pattern is only eligible once
/// ?t is bound, which suggests order-independence. Asserted only: no panic.
#[test]
fn at_variable_bound_later_in_text_does_not_panic() {
    let w = small();
    let out = prepare(
        w,
        "[:find ?e ?p :where (at ?t [?e :located_in ?p]) [?e :seen_at ?t]]",
    );
    eprintln!(
        "OPEN DECISION (at ?t ..) before its binder -> {:?}",
        out.as_ref()
            .map(|_| ())
            .map_err(|e| (e.kind, e.to_string()))
    );
}

// ---------------------------------------------------------------- :kala_handle, inverse, bidirectional

#[test]
fn handle_entity_to_string() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?h :where [:ann :kala_handle ?h]]", &[]),
        col([s("ann")])
    );
    assert_eq!(
        rows(w, "[:find ?h :where [PLACE#2 :kala_handle ?h]]", &[]),
        col([s("mill")])
    );
}

#[test]
fn handle_string_to_entity() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?e :where [?e :kala_handle \"bob\"]]", &[]),
        refs(w, &["bob"])
    );
    assert!(rows(w, "[:find ?e :where [?e :kala_handle \"nobody\"]]", &[]).is_empty());
    assert!(
        rows(w, "[:find ?e :where [?e :kala_handle \"BOB\"]]", &[]).is_empty(),
        "handles are exact"
    );
}

#[test]
fn handle_joins_with_other_patterns_and_ignores_the_tick() {
    let w = small();
    let t = "[:find ?h :where [?e :age 30] [?e :kala_handle ?h]]";
    assert_eq!(rows(w, t, &[]), col([s("ann"), s("cy")]));
    let t = "[:find ?h :where [?e :kala_handle ?h] [?e :age 30]]";
    assert_eq!(rows(w, t, &[]), col([s("ann"), s("cy")]));
    let t = "[:find ?n :at -1000 :where [?e :kala_handle \"ann\"] (at 0 [?e :name ?n])]";
    assert_eq!(rows(w, t, &[]), col([s("Ann")]));
}

#[test]
fn handle_with_both_sides_unbound_does_not_panic() {
    // OPEN DECISION: all (entity, handle) pairs, or a compile/run error? Contract: "Handle: either side bound".
    let w = small();
    let out = prepare(w, "[:find ?e ?h :where [?e :kala_handle ?h]]")
        .map(|p| run(w, &p.plan, &Params::new(), &Limits::default()));
    eprintln!(
        "OPEN DECISION [?e :kala_handle ?h] unbound -> {:?}",
        out.map(|r| r.map(|rs| rs.rows.len()).map_err(|e| e.to_string()))
    );
}

#[test]
fn inverse_name_behaves_like_neighbors() {
    let w = small();
    let t = Tick(0);
    let kids = |p: &str| -> BTreeSet<Vec<Value>> {
        col(w
            .neighbors(id(w, p), "parent_of", Direction::Out, t)
            .unwrap()
            .into_iter()
            .map(|(e, _)| Value::Ref(e)))
    };
    assert_eq!(
        rows(w, "[:find ?c :at 0 :where [:ann :parent_of ?c]]", &[]),
        kids("ann")
    );
    assert_eq!(
        rows(w, "[:find ?c :at 0 :where [?c :child_of :ann]]", &[]),
        kids("ann")
    );
    assert_eq!(
        rows(w, "[:find ?c :at 0 :where [?c :child_of :ann]]", &[]),
        refs(w, &["cy", "di"])
    );
    assert_eq!(
        rows(w, "[:find ?p :at 0 :where [:cy :child_of ?p]]", &[]),
        refs(w, &["ann"])
    );
    assert_eq!(
        rows(w, "[:find ?p :at 0 :where [?p :parent_of :cy]]", &[]),
        refs(w, &["ann"])
    );
    assert!(rows(w, "[:find ?p :at 0 :where [:ann :child_of ?p]]", &[]).is_empty());
    // both variables: pairs are (child, parent)
    assert_eq!(
        rows(w, "[:find ?c ?p :at 0 :where [?c :child_of ?p]]", &[]),
        set([vec![r(w, "cy"), r(w, "ann")], vec![r(w, "di"), r(w, "ann")]])
    );
}

#[test]
fn bidirectional_reads_from_either_side() {
    let w = small();
    for t in [4, 5, 6, 7, 14, 15, 20] {
        let native = |h: &str| -> BTreeSet<Vec<Value>> {
            col(w
                .neighbors(id(w, h), "ally_of", Direction::Both, Tick(t))
                .unwrap()
                .into_iter()
                .map(|(e, _)| Value::Ref(e)))
        };
        for h in ["ann", "bob", "cy", "di"] {
            let a = format!("[:find ?x :at {t} :where [:{h} :ally_of ?x]]");
            let b = format!("[:find ?x :at {t} :where [?x :ally_of :{h}]]");
            assert_eq!(rows(w, &a, &[]), native(h), "{a}");
            assert_eq!(rows(w, &b, &[]), native(h), "{b}");
        }
    }
    assert_eq!(
        rows(w, "[:find ?x :at 8 :where [:bob :ally_of ?x]]", &[]),
        refs(w, &["ann", "cy"])
    );
}

#[test]
fn bidirectional_unbound_both_gives_both_orientations() {
    let w = small();
    let got = rows(w, "[:find ?a ?b :at 8 :where [?a :ally_of ?b]]", &[]);
    let native: BTreeSet<Vec<Value>> = w
        .attr_scan("ally_of", Tick(8))
        .unwrap()
        .into_iter()
        .map(|(e, v)| vec![Value::Ref(e), v])
        .collect();
    assert_eq!(got, native);
    assert_eq!(got.len(), 4);
}

// ---------------------------------------------------------------- predicates

#[test]
fn predicate_lt_on_i64_variables() {
    let w = small();
    let ages: Vec<(EntityId, i64)> = w
        .attr_scan("age", w.latest_tick())
        .unwrap()
        .into_iter()
        .map(|(e, v)| (e, if let Value::I64(n) = v { n } else { panic!() }))
        .collect();
    let mut native = BTreeSet::new();
    for (a, x) in &ages {
        for (b, y) in &ages {
            if x < y {
                native.insert(vec![Value::Ref(*a), Value::Ref(*b)]);
            }
        }
    }
    assert!(!native.is_empty());
    let t = "[:find ?a ?b :where [?a :age ?x] [?b :age ?y] [(< ?x ?y)]]";
    assert_eq!(rows(w, t, &[]), native);
    // predicate written first: order of clauses must not matter
    let t = "[:find ?a ?b :where [(< ?x ?y)] [?a :age ?x] [?b :age ?y]]";
    assert_eq!(rows(w, t, &[]), native);
}

#[test]
fn every_operator_on_i64_with_constants_on_either_side() {
    let w = small();
    let cases = [
        ("(= ?x 30)", vec!["ann", "cy"]),
        ("(!= ?x 30)", vec!["bob", "di"]),
        ("(< ?x 30)", vec!["bob"]),
        ("(<= ?x 30)", vec!["ann", "bob", "cy"]),
        ("(> ?x 30)", vec!["di"]),
        ("(>= ?x 30)", vec!["ann", "cy", "di"]),
        ("(> 30 ?x)", vec!["bob"]),
        ("(<= 30 ?x)", vec!["ann", "cy", "di"]),
    ];
    for (p, who) in cases {
        let t = format!("[:find ?e :where [?e :age ?x] [{p}]]");
        assert_eq!(rows(w, &t, &[]), refs(w, &who), "{p}");
    }
}

#[test]
fn predicate_with_param_operand() {
    let w = small();
    let t = "[:find ?e :in $ ?m :where [?e :age ?x] [(> ?x ?m)]]";
    assert_eq!(
        rows(w, t, &[("m", Value::I64(29))]),
        refs(w, &["ann", "cy", "di"])
    );
    assert_eq!(rows(w, t, &[("m", Value::I64(41))]), set([]));
    assert_eq!(
        prep(w, t).params,
        vec![("m".to_string(), ParamKind::Value(ValueType::I64))],
        "a predicate param takes the type of the variable it is compared with"
    );
}

#[test]
fn predicate_on_strings() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?n :where [?e :name ?n] [(!= ?n \"Ann\")]]", &[]),
        col([s("Bob"), s("Cy"), s("Di")])
    );
    assert_eq!(
        rows(w, "[:find ?n :where [?e :name ?n] [(< ?n \"Cy\")]]", &[]),
        col([s("Ann"), s("Bob")])
    );
    assert_eq!(
        rows(
            w,
            "[:find ?e :in $ ?n :where [?e :name ?x] [(= ?x ?n)]]",
            &[("n", s("Di"))]
        ),
        refs(w, &["di"])
    );
}

#[test]
fn predicate_on_refs() {
    let w = small();
    assert_eq!(
        rows(
            w,
            "[:find ?e :where [?e :located_in ?p] [(= ?p :mill)]]",
            &[]
        ),
        refs(w, &["ann", "cy"])
    );
    assert_eq!(
        rows(
            w,
            "[:find ?e :where [?e :located_in ?p] [(!= ?p PLACE#2)]]",
            &[]
        ),
        refs(w, &["bob", "di"])
    );
    // ann likes bob; cy likes cy
    assert_eq!(
        rows(w, "[:find ?a ?b :where [?a :likes ?b] [(!= ?a ?b)]]", &[]),
        set([vec![r(w, "ann"), r(w, "bob")]])
    );
    assert_eq!(
        rows(w, "[:find ?a :where [?a :likes ?b] [(= ?a ?b)]]", &[]),
        refs(w, &["cy"])
    );
    // Ord on refs is the entity-id order (ann < bob)
    assert_eq!(
        rows(w, "[:find ?a ?b :where [?a :likes ?b] [(< ?a ?b)]]", &[]),
        set([vec![r(w, "ann"), r(w, "bob")]])
    );
    let t = "[:find ?e :in $ ?p :where [?e :located_in ?q] [(= ?q ?p)]]";
    assert_eq!(rows(w, t, &[("p", r(w, "home"))]), refs(w, &["bob", "di"]));
}

#[test]
fn predicate_on_f64_and_bool() {
    let w = small();
    assert_eq!(
        rows(w, "[:find ?e :where [?e :weight ?w] [(> ?w 60.5)]]", &[]),
        refs(w, &["bob"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :weight ?w] [(>= ?w 60.5)]]", &[]),
        refs(w, &["ann", "bob", "cy"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :alive ?v] [(= ?v false)]]", &[]),
        refs(w, &["bob"])
    );
    assert_eq!(
        rows(w, "[:find ?e :where [?e :alive ?v] [(!= ?v false)]]", &[]),
        refs(w, &["ann", "cy"])
    );
}

#[test]
fn predicate_between_variables_of_different_patterns() {
    let w = small();
    let t = "[:find ?a ?b :where [?a :age ?x] [?b :age ?y] [(= ?x ?y)] [(!= ?a ?b)]]";
    assert_eq!(
        rows(w, t, &[]),
        set([vec![r(w, "ann"), r(w, "cy")], vec![r(w, "cy"), r(w, "ann")]])
    );
}

#[test]
fn several_predicates_all_apply() {
    let w = small();
    let t = "[:find ?e :where [?e :age ?x] [(> ?x 25)] [(< ?x 41)] [(!= ?e :cy)]]";
    assert_eq!(rows(w, t, &[]), refs(w, &["ann"]));
}

// ---------------------------------------------------------------- joins and sets

#[test]
fn join_on_shared_variable_equals_native_neighbors() {
    let w = small();
    let t = "[:find ?n :at 8 :where [:bob :ally_of ?a] [?a :name ?n]]";
    let native: Vec<Value> = w
        .neighbors(id(w, "bob"), "ally_of", Direction::Both, Tick(8))
        .unwrap()
        .into_iter()
        .flat_map(|(e, _)| w.as_of(e, "name", Tick(8)).unwrap())
        .collect();
    assert_eq!(native.len(), 2);
    assert_eq!(rows(w, t, &[]), col(native));
}

#[test]
fn cartesian_product_of_unrelated_patterns() {
    let w = small();
    let t = "[:find ?a ?b :where [?a :alive true] [?b :located_in :home]]";
    // alive: ann, cy; at home at tick 20: bob, di
    let mut native = BTreeSet::new();
    for a in ["ann", "cy"] {
        for b in ["bob", "di"] {
            native.insert(vec![r(w, a), r(w, b)]);
        }
    }
    assert_eq!(rows(w, t, &[]), native);
    assert_eq!(exec(w, t, &[]).rows.len(), 4);
}

#[test]
fn results_are_sets() {
    let w = small();
    let rs = exec(w, "[:find ?p :where [?e :located_in ?p]]", &[]);
    assert_eq!(
        rs.rows.len(),
        2,
        "four people, two places, each place once: {:?}",
        rs.rows
    );
    let w = varrow();
    let rs = exec(w, "[:find ?l :where [?e :label ?l]]", &[]);
    let distinct: BTreeSet<_> = rs.rows.iter().cloned().collect();
    assert_eq!(distinct.len(), rs.rows.len(), "no duplicate rows");
    assert!(rs.rows.len() < w.attr_scan("label", w.latest_tick()).unwrap().len());
}

#[test]
fn projection_drops_unfound_variables_and_dedups() {
    let w = small();
    let rs = exec(w, "[:find ?a :where [?a :age ?x] [?b :age ?y]]", &[]);
    assert_eq!(rs.rows.len(), 4);
}

// ---------------------------------------------------------------- varrow headline

const HEADLINE: &str = "[:find ?name :in $ ?who :at -210 :where [?e :kala_handle ?who] [?e :ally_of ?a] \
                        (at -200 [?a :located_in ?p]) [?a :name ?name] [(!= ?a ?e)]]";

fn native_headline(w: &World, who: &str, tick: i64, at: i64) -> BTreeSet<Vec<Value>> {
    let e = id(w, who);
    let mut out = BTreeSet::new();
    for (a, _) in w
        .neighbors(e, "ally_of", Direction::Both, Tick(tick))
        .unwrap()
    {
        if a == e || w.as_of(a, "located_in", Tick(at)).unwrap().is_empty() {
            continue;
        }
        for n in w.as_of(a, "name", Tick(tick)).unwrap() {
            out.insert(vec![n]);
        }
    }
    out
}

#[test]
fn headline_query_on_varrow() {
    let w = varrow();
    let got = rows(w, HEADLINE, &[("who", s("edwin"))]);
    assert_eq!(got, native_headline(w, "edwin", -210, -200));
    assert!(!got.is_empty(), "the headline answer must not be vacuous");
    assert_eq!(got, col([s("Tomas")]));
    let p = prep(w, HEADLINE);
    assert_eq!(p.plan.default_tick, TickSource::Const(Tick(-210)));
}

#[test]
fn headline_with_tick_and_who_as_params_sweeps_ticks() {
    let w = varrow();
    let t = "[:find ?name :in $ ?who ?t :at ?t :where [?e :kala_handle ?who] [?e :ally_of ?a] \
             (at -200 [?a :located_in ?p]) [?a :name ?name] [(!= ?a ?e)]]";
    let mut nonempty = 0;
    for tick in (-450..=700).step_by(10) {
        for who in ["edwin", "tomas", "mara", "greg"] {
            let got = rows(w, t, &[("who", s(who)), ("t", Value::I64(tick))]);
            let native = native_headline(w, who, tick, -200);
            assert_eq!(got, native, "{who} at {tick}");
            nonempty += !got.is_empty() as usize;
        }
    }
    assert!(nonempty > 5, "sweep must hit real answers");
}

#[test]
fn varrow_allies_by_entity_param_match_neighbors_at_every_tick() {
    let w = varrow();
    let t = "[:find ?a :in $ ?e ?t :at ?t :where [?e :ally_of ?a]]";
    for tick in (-450..=700).step_by(5) {
        for who in ["edwin", "mara", "greg", "tomas", "hild", "wren", "lisa"] {
            let native = col(w
                .neighbors(id(w, who), "ally_of", Direction::Both, Tick(tick))
                .unwrap()
                .into_iter()
                .map(|(e, _)| Value::Ref(e)));
            assert_eq!(
                rows(w, t, &[("e", r(w, who)), ("t", Value::I64(tick))]),
                native,
                "{who} at {tick}"
            );
        }
    }
}

#[test]
fn older_canon_sees_the_old_allies() {
    let w = varrow();
    let latest = w.latest_tx();
    assert!(latest.0 > 1);
    let older = TxId(latest.0 - 1); // before the retcon (last transaction)
    let edwin = id(w, "edwin");
    let mut differs = false;
    for tick in (-300..=-100).step_by(5) {
        let t = format!("[:find ?a :in $ ?c :at {tick} :canon ?c :where [:edwin :ally_of ?a]]");
        for c in [older, latest] {
            let native = col(w
                .at_canon(c)
                .neighbors(edwin, "ally_of", Direction::Both, Tick(tick))
                .unwrap()
                .into_iter()
                .map(|(e, _)| Value::Ref(e)));
            assert_eq!(
                rows(w, &t, &[("c", Value::I64(c.0 as i64))]),
                native,
                "canon {c:?} tick {tick}"
            );
        }
        let tc = format!(
            "[:find ?a :at {tick} :canon {} :where [:edwin :ally_of ?a]]",
            older.0
        );
        let tl = format!("[:find ?a :at {tick} :where [:edwin :ally_of ?a]]");
        differs |= rows(w, &tc, &[]) != rows(w, &tl, &[]);
    }
    assert!(
        differs,
        "the retcon must be visible somewhere between -300 and -100"
    );
}

#[test]
fn canon_sweep_on_small_world_resolves_the_default_tick_at_that_canon() {
    let w = small();
    for c in 1..=w.latest_tx().0 {
        let view = w.at_canon(TxId(c));
        let native: BTreeSet<Vec<Value>> = view
            .attr_scan("located_in", view.latest_tick())
            .unwrap()
            .into_iter()
            .map(|(e, v)| vec![Value::Ref(e), v])
            .collect();
        let t = format!("[:find ?e ?p :canon {c} :where [?e :located_in ?p]]");
        assert_eq!(rows(w, &t, &[]), native, "canon {c}");
    }
    // a canon past the latest means the latest
    assert_eq!(
        rows(
            w,
            "[:find ?e ?p :canon 99999 :where [?e :located_in ?p]]",
            &[]
        ),
        rows(w, "[:find ?e ?p :where [?e :located_in ?p]]", &[])
    );
}

// ---------------------------------------------------------------- compile errors

#[test]
fn unknown_attribute() {
    cerr(
        small(),
        "[:find ?e :where [?e :nope ?v]]",
        ErrorKind::UnknownAttribute,
        Some(":nope"),
    );
    cerr(
        small(),
        "[:find ?e :where [?e :name ?n] (at 3 [?e :nope ?v])]",
        ErrorKind::UnknownAttribute,
        Some(":nope"),
    );
}

#[test]
fn unknown_handle_or_id_constant() {
    let w = small();
    cerr(
        w,
        "[:find ?n :where [:zed :name ?n]]",
        ErrorKind::UnknownEntity,
        Some(":zed"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :located_in :nowhere]]",
        ErrorKind::UnknownEntity,
        Some(":nowhere"),
    );
    cerr(
        w,
        "[:find ?n :where [CHARACTER#99 :name ?n]]",
        ErrorKind::UnknownEntity,
        Some("CHARACTER#99"),
    );
    cerr(
        w,
        "[:find ?n :where [edge#99 :disguised ?n]]",
        ErrorKind::UnknownEntity,
        Some("edge#99"),
    );
    cerr(
        w,
        "[:find ?n :where [?e :name ?n] [(= ?e :zed)]]",
        ErrorKind::UnknownEntity,
        Some(":zed"),
    );
}

#[test]
fn find_variable_not_bound() {
    cerr(
        small(),
        "[:find ?zz :where [?e :name ?n]]",
        ErrorKind::UnboundVariable,
        Some("?zz"),
    );
    cerr(
        small(),
        "[:find ?n ?zz :where [?e :name ?n]]",
        ErrorKind::UnboundVariable,
        Some("?zz"),
    );
}

#[test]
fn predicate_variable_not_bound() {
    cerr(
        small(),
        "[:find ?n :where [?e :name ?n] [(!= ?n ?q)]]",
        ErrorKind::UnboundVariable,
        Some("?q"),
    );
    cerr(
        small(),
        "[:find ?n :where [?e :name ?n] [(< ?q 3)]]",
        ErrorKind::UnboundVariable,
        Some("?q"),
    );
}

#[test]
fn wrong_typed_constants() {
    let w = small();
    cerr(
        w,
        "[:find ?e :where [?e :located_in \"Varrow\"]]",
        ErrorKind::TypeMismatch,
        Some("\"Varrow\""),
    );
    cerr(
        w,
        "[:find ?e :where [?e :age 1.5]]",
        ErrorKind::TypeMismatch,
        Some("1.5"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :age \"x\"]]",
        ErrorKind::TypeMismatch,
        Some("\"x\""),
    );
    cerr(
        w,
        "[:find ?e :where [?e :name 5]]",
        ErrorKind::TypeMismatch,
        Some("5"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :alive 1]]",
        ErrorKind::TypeMismatch,
        Some("1"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :weight true]]",
        ErrorKind::TypeMismatch,
        Some("true"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :located_in 7]]",
        ErrorKind::TypeMismatch,
        Some("7"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :kala_handle 5]]",
        ErrorKind::TypeMismatch,
        Some("5"),
    );
    cerr(
        w,
        "[:find ?e :where [?e :kala_handle :ann]]",
        ErrorKind::TypeMismatch,
        Some(":ann"),
    );
}

#[test]
fn constants_that_are_not_entities_in_entity_position() {
    let w = small();
    cerr(
        w,
        "[:find ?n :where [5 :name ?n]]",
        ErrorKind::TypeMismatch,
        Some("5"),
    );
    cerr(
        w,
        "[:find ?n :where [\"ann\" :name ?n]]",
        ErrorKind::TypeMismatch,
        Some("\"ann\""),
    );
    cerr(
        w,
        "[:find ?n :where [true :name ?n]]",
        ErrorKind::TypeMismatch,
        Some("true"),
    );
}

#[test]
fn variable_used_with_two_types() {
    let w = small();
    cerr(
        w,
        "[:find ?y :where [?x :name ?n] [?n :ally_of ?y]]",
        ErrorKind::TypeMismatch,
        Some("?n"),
    );
    cerr(
        w,
        "[:find ?x :where [?x :name ?v] [?x :age ?v]]",
        ErrorKind::TypeMismatch,
        Some("?v"),
    );
    cerr(
        w,
        "[:find ?x :where [?x :age ?v] [?x :weight ?v]]",
        ErrorKind::TypeMismatch,
        Some("?v"),
    );
}

#[test]
fn predicate_on_two_different_types() {
    let w = small();
    cerr(
        w,
        "[:find ?e :where [?e :age ?a] [?e :name ?n] [(< ?a ?n)]]",
        ErrorKind::TypeMismatch,
        None,
    );
    cerr(
        w,
        "[:find ?e :where [?e :age ?a] [(< ?a \"x\")]]",
        ErrorKind::TypeMismatch,
        None,
    );
    cerr(
        w,
        "[:find ?e :where [?e :age ?a] [(< ?a 1.5)]]",
        ErrorKind::TypeMismatch,
        None,
    );
    cerr(
        w,
        "[:find ?e :where [?e :name ?a] [(= ?a :ann)]]",
        ErrorKind::TypeMismatch,
        None,
    );
    cerr(
        w,
        "[:find ?e :where [?e :alive ?a] [(= ?a 1)]]",
        ErrorKind::TypeMismatch,
        None,
    );
}

#[test]
fn compile_errors_display_line_and_column() {
    let t = "[:find ?e\n :where\n   [?e :nope ?v]]";
    let e = cerr(small(), t, ErrorKind::UnknownAttribute, Some(":nope"));
    assert_eq!(e.to_string(), format!("line 3, column 8: {}", e.message));
}

#[test]
fn compile_errors_escape_what_they_echo() {
    let t = "[:find ?e :where [?e :no\u{202E}pe\u{7} ?v]]";
    if let Err(e) = prepare(small(), t) {
        for c in e.to_string().chars() {
            assert!(!c.is_control() && c != '\u{202E}', "raw {c:?} in {e}");
        }
    }
}

#[test]
fn compile_with_edge_case_but_valid_queries_does_not_panic() {
    let w = small();
    for t in [
        "[:find ?e :where [?e :name _]]",
        "[:find ?e ?e :where [?e :name _]]",
        "[:find ?e :where [?e :name ?n] [?e :name ?n]]",
        "[:find ?e :where [_ :name _] [?e :age _]]",
        "[:find ?e :where (at 1 [?e :name _]) (at 2 [?e :name _])]",
        "[:find ?e :at 9223372036854775807 :where [?e :name _]]",
        "[:find ?e :at -9223372036854775808 :where [?e :name _]]",
    ] {
        let p = prep(w, t);
        run(w, &p.plan, &Params::new(), &Limits::default()).unwrap_or_else(|e| panic!("{t}: {e}"));
    }
}
