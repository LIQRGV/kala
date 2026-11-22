//! Item 5 (RED until the planner exists): the planner's chosen order and index access.
//!
//! Binding rule (contract "Item 5: planner"): greedy and deterministic. Next pattern = best
//! given the vars bound so far: bound entity -> E; else bound ref value -> R; else bound value
//! of an indexed attribute -> V; `:handle` with a side bound -> Handle; else Scan of the attr
//! with the smallest `World::attr_stats(attr).pairs` (ties: query order). `(at ?t ..)` is
//! eligible only once ?t is bound. Left-deep nested Expands over Unit. A Filter sits right after
//! the step binding its last var. Project of the find vars on top. Constants and `:in` params
//! count as bound.
//!
//! Pinned readings (raise if the implementer disagrees):
//! - a pattern without `(at ..)` carries `TickSource::Default` even when the query has `:at T`
//!   (the query default lives in `Plan.default_tick`), so it prints `@latest`;
//! - the exact header/line formats are those of `Plan::explain` in the contract.
use kala_core::*;
use kala_datalog::prepare;
use kala_query::{AttrRef, Limits, Op as QOp, Params, Plan, ResultSet, TickSource, run};
use std::collections::BTreeSet;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

/// Attribute pair counts (ever committed): name 4, seen_at 2, located_in 3 (ann home, ann mill,
/// bob home), likes 2, hates 2, fears 3. ann=CHARACTER#1 bob=#2 cy=#3 di=#4.
const TINY: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name,       type: string }
    - { name: seen_at,    type: i64 }
    - { name: located_in, type: ref }
    - { name: likes,      type: ref, cardinality: many }
    - { name: hates,      type: ref, cardinality: many }
    - { name: fears,      type: ref, cardinality: many }
entities:
  CHARACTER: [ann, bob, cy, di]
  PLACE: [home, mill]
transactions:
  - ops:
      - { assert: [ann, name, "Ann"], tick: 0 }
      - { assert: [bob, name, "Bob"], tick: 0 }
      - { assert: [cy, name, "Cy"], tick: 0 }
      - { assert: [di, name, "Di"], tick: 0 }
      - { assert: [ann, seen_at, 25], tick: 0 }
      - { assert: [bob, seen_at, 5], tick: 0 }
      - { assert: [ann, located_in, home], tick: 0 }
      - { retract: [ann, located_in, home], tick: 20 }
      - { assert: [ann, located_in, mill], tick: 20 }
      - { assert: [bob, located_in, home], tick: 0 }
      - { assert: [ann, likes, bob], tick: 0 }
      - { assert: [bob, likes, cy], tick: 0 }
      - { assert: [ann, hates, di], tick: 0 }
      - { assert: [cy, hates, di], tick: 0 }
      - { assert: [ann, fears, cy], tick: 0 }
      - { assert: [cy, fears, di], tick: 0 }
      - { assert: [bob, fears, di], tick: 0 }
"#;

fn leak(yaml: &str) -> &'static World {
    Box::leak(Box::new(load_fixture(yaml).expect("fixture loads")))
}
fn varrow() -> &'static World {
    thread_local!(static W: &'static World = leak(VARROW));
    W.with(|w| *w)
}
fn tiny() -> &'static World {
    thread_local!(static W: &'static World = leak(TINY));
    W.with(|w| *w)
}
fn id(w: &World, h: &str) -> EntityId {
    w.entity_by_handle(h)
        .unwrap_or_else(|| panic!("no handle {h}"))
}
fn fid(w: &World, h: &str) -> String {
    w.format_id(id(w, h))
}

fn plan(w: &World, text: &str) -> Plan {
    match prepare(w, text) {
        Ok(p) => p.plan,
        Err(e) => panic!("prepare failed for {text}\n  {e}"),
    }
}
fn explain(w: &World, text: &str) -> String {
    plan(w, text).explain(w)
}

/// The plan's steps in execution order (bottom first): `attr:Access` for an Expand
/// (`handle:Handle` for the pseudo-attribute), `filter` for a Filter. Fails on anything that
/// is not a left-deep chain `Project(Filter|Expand ... Unit)`.
fn steps(p: &Plan, shown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut op = match &p.root {
        QOp::Project { input, vars } => {
            assert_eq!(vars, &p.find, "Project must carry the find vars\n{shown}");
            &**input
        }
        other => panic!("root must be Project, got {other:?}\n{shown}"),
    };
    loop {
        match op {
            QOp::Unit => break,
            QOp::Expand {
                input,
                attr,
                access,
                ..
            } => {
                let name = match attr {
                    AttrRef::Named(n) => n.clone(),
                    AttrRef::Handle => "handle".to_string(),
                };
                out.push(format!("{name}:{access:?}"));
                op = input;
            }
            QOp::Filter { input, .. } => {
                out.push("filter".into());
                op = input;
            }
            QOp::NodeScan { input, .. } | QOp::NodeSeek { input, .. } => {
                out.push("node".into());
                op = input;
            }
            other => panic!("plan is not a left-deep chain: {other:?}\n{shown}"),
        }
    }
    out.reverse();
    out
}
fn sig(w: &World, text: &str) -> Vec<String> {
    let p = plan(w, text);
    let shown = p.explain(w);
    steps(&p, &shown)
}

fn run_q(w: &World, text: &str, params: &[(&str, Value)]) -> ResultSet {
    let p = plan(w, text);
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p, &ps, &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n{}\n  {e}", p.explain(w)))
}
fn rows(w: &World, text: &str, params: &[(&str, Value)]) -> BTreeSet<Vec<Value>> {
    run_q(w, text, params).rows.into_iter().collect()
}

// ---------------------------------------------------------------- access per pattern

#[test]
fn handle_then_e_and_no_scan_anywhere() {
    let w = varrow();
    let text = r#"[:find ?a :where [?e :handle "edwin"] [?e :ally_of ?a]]"#;
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["handle:Handle", "ally_of:E"], "\n{shown}");
    assert!(!shown.contains("via scan"), "\n{shown}");
    assert_eq!(
        shown,
        "Find ?a  at latest  canon latest\n\
         \x20 Project ?a\n\
         \x20   Expand [?e :ally_of ?a] @latest via E\n\
         \x20     Expand [?e :handle \"edwin\"] via handle\n\
         \x20       Unit\n"
    );
}

#[test]
fn constant_entity_uses_e() {
    let w = varrow();
    let text = "[:find ?a :where [:edwin :ally_of ?a]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["ally_of:E"], "\n{shown}");
    assert_eq!(
        shown,
        format!(
            "Find ?a  at latest  canon latest\n  Project ?a\n    Expand [{} :ally_of ?a] @latest via E\n      Unit\n",
            fid(w, "edwin")
        )
    );
}

#[test]
fn constant_ref_value_uses_r() {
    let w = varrow();
    let text = "[:find ?p :where [?p :located_in :varrow]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["located_in:R"], "\n{shown}");
    assert_eq!(
        shown,
        format!(
            "Find ?p  at latest  canon latest\n  Project ?p\n    Expand [?p :located_in {}] @latest via R\n      Unit\n",
            fid(w, "varrow")
        )
    );
}

#[test]
fn constant_string_value_uses_v() {
    let w = varrow();
    let text = r#"[:find ?e :where [?e :name "Edwin"]]"#;
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["name:V"], "\n{shown}");
    assert_eq!(
        shown,
        "Find ?e  at latest  canon latest\n  Project ?e\n    Expand [?e :name \"Edwin\"] @latest via V\n      Unit\n"
    );
}

#[test]
fn bound_ref_variable_value_uses_r() {
    let w = tiny();
    // likes (2 pairs) is scanned first; ?b is then a bound ref value of `fears` -> R
    let text = "[:find ?a ?c :where [?a :fears ?b] [?b :likes ?c]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["likes:Scan", "fears:R"], "\n{shown}");
}

#[test]
fn bound_entity_variable_uses_e() {
    let w = tiny();
    let text = "[:find ?a ?c :where [?a :likes ?b] [?b :fears ?c]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["likes:Scan", "fears:E"], "\n{shown}");
}

#[test]
fn bound_value_variable_of_indexed_attr_uses_v() {
    let w = varrow();
    let text = "[:find ?a ?b :where [?a :name ?n] [?b :name ?n]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["name:Scan", "name:V"], "\n{shown}");
}

#[test]
fn handle_with_bound_entity_uses_handle() {
    let w = varrow();
    let text = "[:find ?h :where [:edwin :ally_of ?a] [?a :handle ?h]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["ally_of:E", "handle:Handle"], "\n{shown}");
}

#[test]
fn param_counts_as_bound() {
    let w = varrow();
    let text = "[:find ?a :in $ ?who :where [?e :ally_of ?a] [?e :handle ?who]]";
    let shown = explain(w, text);
    assert_eq!(sig(w, text), ["handle:Handle", "ally_of:E"], "\n{shown}");
    assert!(
        shown.contains("Expand [?e :handle $who] via handle"),
        "\n{shown}"
    );
}

// ---------------------------------------------------------------- order independence

#[test]
fn reversed_text_order_gives_the_same_plan_for_selective_queries() {
    let w = varrow();
    let pairs = [
        (
            r#"[:find ?a :where [?e :handle "edwin"] [?e :ally_of ?a]]"#,
            r#"[:find ?a :where [?e :ally_of ?a] [?e :handle "edwin"]]"#,
        ),
        (
            "[:find ?n :where [?p :located_in :varrow] [?p :name ?n]]",
            "[:find ?n :where [?p :name ?n] [?p :located_in :varrow]]",
        ),
        (
            r#"[:find ?p :where [?e :name "Edwin"] [?e :located_in ?p]]"#,
            r#"[:find ?p :where [?e :located_in ?p] [?e :name "Edwin"]]"#,
        ),
    ];
    for (a, b) in pairs {
        let (pa, pb) = (plan(w, a), plan(w, b));
        assert_eq!(
            pa,
            pb,
            "\n{a}\n{}\nvs\n{b}\n{}",
            pa.explain(w),
            pb.explain(w)
        );
        assert!(!pa.explain(w).contains("via scan"), "\n{}", pa.explain(w));
    }
}

#[test]
fn selective_queries_never_scan() {
    let w = varrow();
    for text in [
        r#"[:find ?a :where [?e :handle "edwin"] [?e :ally_of ?a] [?a :name ?n]]"#,
        "[:find ?a :where [:edwin :ally_of ?a] [?a :located_in ?p] [?p :name ?n]]",
        "[:find ?e :where [?e :located_in :varrow] [?e :name ?n]]",
        r#"[:find ?e ?p :where [?e :name "Edwin"] [?e :located_in ?p] [?p :name ?pn]]"#,
        r#"[:find ?a :in $ ?who :where [?a :name ?n] [?e :ally_of ?a] [?e :handle ?who]]"#,
    ] {
        let shown = explain(w, text);
        assert!(!shown.contains("via scan"), "{text}\n{shown}");
    }
}

// ---------------------------------------------------------------- scan choice

#[test]
fn two_unbound_patterns_scan_the_smaller_attribute_first() {
    let w = tiny();
    assert_eq!(w.attr_stats("likes").unwrap().pairs, 2);
    assert_eq!(w.attr_stats("fears").unwrap().pairs, 3);
    for text in [
        "[:find ?a ?c :where [?a :fears ?b] [?c :likes ?d]]",
        "[:find ?a ?c :where [?c :likes ?d] [?a :fears ?b]]",
    ] {
        let shown = explain(w, text);
        assert_eq!(
            sig(w, text),
            ["likes:Scan", "fears:Scan"],
            "{text}\n{shown}"
        );
    }
}

#[test]
fn varrow_two_unbound_patterns_follow_the_stats() {
    let w = varrow();
    let (ally, ruled) = (
        w.attr_stats("ally_of").unwrap().pairs,
        w.attr_stats("ruled_by").unwrap().pairs,
    );
    assert_ne!(ally, ruled, "test needs differing stats");
    let (small, big) = if ruled < ally {
        ("ruled_by", "ally_of")
    } else {
        ("ally_of", "ruled_by")
    };
    for text in [
        "[:find ?x ?p :where [?x :ally_of ?y] [?p :ruled_by ?r]]",
        "[:find ?x ?p :where [?p :ruled_by ?r] [?x :ally_of ?y]]",
    ] {
        let shown = explain(w, text);
        assert_eq!(
            sig(w, text),
            [format!("{small}:Scan"), format!("{big}:Scan")],
            "{text}\n{shown}"
        );
    }
}

#[test]
fn equal_stats_tie_breaks_by_query_order() {
    let w = tiny();
    assert_eq!(
        w.attr_stats("likes").unwrap().pairs,
        w.attr_stats("hates").unwrap().pairs
    );
    let a = "[:find ?a ?c :where [?a :hates ?b] [?c :likes ?d]]";
    let b = "[:find ?a ?c :where [?c :likes ?d] [?a :hates ?b]]";
    assert_eq!(
        sig(w, a),
        ["hates:Scan", "likes:Scan"],
        "\n{}",
        explain(w, a)
    );
    assert_eq!(
        sig(w, b),
        ["likes:Scan", "hates:Scan"],
        "\n{}",
        explain(w, b)
    );
}

#[test]
fn planning_is_deterministic() {
    let w = varrow();
    let text = "[:find ?x ?p :where [?x :ally_of ?y] [?p :ruled_by ?r] [?x :name ?n]]";
    assert_eq!(plan(w, text), plan(w, text));
}

// ---------------------------------------------------------------- filters

#[test]
fn filter_sits_right_after_the_step_binding_its_last_var() {
    let w = varrow();
    let text =
        r#"[:find ?n :where [?e :handle "edwin"] [?e :ally_of ?a] [?a :name ?n] [(!= ?a ?e)]]"#;
    let shown = explain(w, text);
    assert_eq!(
        sig(w, text),
        ["handle:Handle", "ally_of:E", "filter", "name:E"],
        "\n{shown}"
    );
}

#[test]
fn filter_written_first_still_waits_for_its_vars() {
    let w = varrow();
    let text =
        r#"[:find ?n :where [(!= ?a ?e)] [?e :handle "edwin"] [?e :ally_of ?a] [?a :name ?n]]"#;
    let shown = explain(w, text);
    assert_eq!(
        sig(w, text),
        ["handle:Handle", "ally_of:E", "filter", "name:E"],
        "\n{shown}"
    );
}

#[test]
fn filter_on_param_and_var_goes_right_after_the_var_is_bound() {
    let w = varrow();
    let text =
        "[:find ?a :in $ ?who ?other :where [?e :handle ?who] [(!= ?e ?other)] [?e :ally_of ?a]]";
    let shown = explain(w, text);
    assert_eq!(
        sig(w, text),
        ["handle:Handle", "filter", "ally_of:E"],
        "\n{shown}"
    );
}

// ---------------------------------------------------------------- (at ?t ..)

#[test]
fn at_var_pattern_written_first_runs_after_its_binder() {
    let w = tiny();
    let text = "[:find ?a ?p :where (at ?t [?a :located_in ?p]) [?a :seen_at ?t]]";
    let p = plan(w, text);
    let shown = p.explain(w);
    assert_eq!(
        steps(&p, &shown),
        ["seen_at:Scan", "located_in:E"],
        "\n{shown}"
    );
    // the located_in Expand reads at ?t
    match &p.root {
        QOp::Project { input, .. } => match &**input {
            QOp::Expand { tick, .. } => {
                assert!(
                    matches!(tick, TickSource::Var(v) if v.0 == "t"),
                    "{tick:?}\n{shown}"
                )
            }
            other => panic!("expected Expand on top, got {other:?}\n{shown}"),
        },
        _ => unreachable!(),
    }
    assert_eq!(
        shown,
        "Find ?a ?p  at latest  canon latest\n\
         \x20 Project ?a ?p\n\
         \x20   Expand [?a :located_in ?p] @?t via E\n\
         \x20     Expand [?a :seen_at ?t] @latest via scan\n\
         \x20       Unit\n"
    );
}

#[test]
fn at_var_results_do_not_depend_on_written_order() {
    let w = tiny();
    let want: BTreeSet<Vec<Value>> = [
        vec![Value::Ref(id(w, "ann")), Value::Ref(id(w, "mill"))],
        vec![Value::Ref(id(w, "bob")), Value::Ref(id(w, "home"))],
    ]
    .into_iter()
    .collect();
    let a = rows(
        w,
        "[:find ?a ?p :where (at ?t [?a :located_in ?p]) [?a :seen_at ?t]]",
        &[],
    );
    let b = rows(
        w,
        "[:find ?a ?p :where [?a :seen_at ?t] (at ?t [?a :located_in ?p])]",
        &[],
    );
    assert_eq!(a, want);
    assert_eq!(b, want);
}

// ---------------------------------------------------------------- the headline query

const HEADLINE: &str = r#"[:find ?name
 :in $ ?who
 :at -210
 :canon 41
 :where
 [?e :handle ?who]
 [?e :ally_of ?a]
 (at -200 [?a :located_in ?p])
 [?a :name ?name]
 [(!= ?a ?e)]]"#;

#[test]
fn headline_query_explain_is_pinned() {
    let w = varrow();
    let shown = explain(w, HEADLINE);
    assert_eq!(
        shown,
        "Find ?name  in $who  at -210  canon 41\n\
         \x20 Project ?name\n\
         \x20   Expand [?a :name ?name] @latest via E\n\
         \x20     Expand [?a :located_in ?p] @-200 via E\n\
         \x20       Filter ?a != ?e\n\
         \x20         Expand [?e :ally_of ?a] @latest via E\n\
         \x20           Expand [?e :handle $who] via handle\n\
         \x20             Unit\n"
    );
}

#[test]
fn headline_query_never_scans_and_matches_a_hand_reordered_equivalent() {
    let w = varrow();
    assert!(!explain(w, HEADLINE).contains("via scan"));
    let reordered = r#"[:find ?name
 :in $ ?who
 :at -210
 :canon 41
 :where
 [(!= ?a ?e)]
 [?a :name ?name]
 (at -200 [?a :located_in ?p])
 [?e :ally_of ?a]
 [?e :handle ?who]]"#;
    let params = [("who", Value::Str("edwin".into()))];
    let a = rows(w, HEADLINE, &params);
    let b = rows(w, reordered, &params);
    assert_eq!(a, b);
    assert!(!a.is_empty(), "headline query should find someone");
}

#[test]
fn results_are_unchanged_by_pattern_order() {
    let w = varrow();
    let a = rows(
        w,
        r#"[:find ?a ?n :at -210 :where [?e :handle "edwin"] [?e :ally_of ?a] [?a :name ?n]]"#,
        &[],
    );
    let b = rows(
        w,
        r#"[:find ?a ?n :at -210 :where [?a :name ?n] [?e :ally_of ?a] [?e :handle "edwin"]]"#,
        &[],
    );
    assert_eq!(a, b);
    assert!(!a.is_empty());
}
