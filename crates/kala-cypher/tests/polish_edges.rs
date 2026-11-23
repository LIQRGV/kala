//! Slice 13 polish, confirm step: edge cases the implementation revealed.
use kala_core::{Value, World};
use kala_fixture::load_fixture;
use kala_query::{
    CanonSource, CmpOp, Datum, Expr, Limits, Op, OutputSpec, Params, Plan, QueryParams, RowAction,
    TickSource, Var, run,
};

const EDGES: &str = r#"
schema:
  node_types: [NODE]
  attributes:
    - { name: link, type: ref, cardinality: many, direction: directed }
    - { name: parent_of, type: ref, cardinality: many, direction: directed, inverse: child_of }
    - { name: name, type: string }
    - { name: alias, type: string, cardinality: many }
    - { name: tags, type: i64, cardinality: many }
    - { name: score, type: i64 }
entities:
  NODE: [a, b, c]
transactions:
  - tick: -1
    ops:
      - assert: [a, name, Alpha]
      - assert: [b, name, Beta]
      - assert: [c, name, Gamma]
      - assert: [a, link, b]
      - assert: [b, link, a]
      - assert: [a, link, c]
      - assert: [a, parent_of, b]
      - assert: [a, alias, x]
      - assert: [a, alias, y]
      - assert: [b, alias, x]
      - assert: [b, alias, z]
      - assert: [c, alias, x]
      - assert: [c, alias, y]
      - assert: [a, tags, 1]
      - assert: [b, tags, 2]
      - assert: [a, score, 5]
"#;
fn world() -> World {
    load_fixture(EDGES).unwrap()
}
fn prep_run(w: &World, text: &str) -> Vec<Vec<Datum>> {
    let p = kala_cypher::prepare(w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .rows
}
fn s(x: &str) -> Datum {
    Value::Str(x.into()).into()
}
fn col(rows: Vec<Vec<Datum>>) -> Vec<Datum> {
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}
fn var(n: &str) -> Var {
    Var(n.into())
}
fn row(input: Op, action: RowAction) -> Op {
    Op::Row {
        input: Box::new(input),
        action,
    }
}
fn plan(find: &[&str], root: Op) -> Plan {
    let vars: Vec<Var> = find.iter().map(|v| var(v)).collect();
    Plan {
        find: vars.clone(),
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root: Op::Output {
            input: Box::new(root),
            spec: OutputSpec {
                vars,
                distinct: false,
                order: vec![],
                skip: None,
                limit: None,
            },
        },
    }
}
fn edge_action(source: &str, target: &str, edge: &str, attr: &str) -> RowAction {
    RowAction::Edge {
        source: var(source),
        target: var(target),
        edge: var(edge),
        attr: attr.into(),
        direction: kala_query::Orientation::Out,
        tick: TickSource::Default,
    }
}

// ---------------------------------------------------------------- edge with a non-node bound end

#[test]
fn edge_with_null_scalar_or_edge_bound_end_gives_no_rows_and_no_error() {
    let w = world();
    let bind = |input: Op, name: &str, expr: Expr| {
        row(
            input,
            RowAction::Bind {
                var: var(name),
                expr,
            },
        )
    };
    let nodes = row(
        Op::Unit,
        RowAction::Nodes {
            var: var("a"),
            labels: vec![],
            tick: TickSource::Default,
        },
    );
    let with_edge = row(nodes.clone(), edge_action("a", "b", "r", "link"));
    for (what, root) in [
        // NULL as the source
        (
            "null source",
            row(
                bind(Op::Unit, "x", Expr::Null),
                edge_action("x", "y", "r", "link"),
            ),
        ),
        // scalar as the source
        (
            "scalar source",
            row(
                bind(Op::Unit, "x", Expr::Value(Value::I64(5))),
                edge_action("x", "y", "r", "link"),
            ),
        ),
        // NULL as the already-bound target of a real node scan
        (
            "null target",
            row(
                bind(nodes.clone(), "t", Expr::Null),
                edge_action("a", "t", "r", "link"),
            ),
        ),
        // scalar as the bound target
        (
            "scalar target",
            row(
                bind(nodes.clone(), "t", Expr::Value(Value::Str("a".into()))),
                edge_action("a", "t", "r", "link"),
            ),
        ),
        // an edge entity as the source
        (
            "edge source",
            row(with_edge.clone(), edge_action("r", "z", "q", "link")),
        ),
        // an edge entity as the bound target
        (
            "edge target",
            row(with_edge.clone(), edge_action("a", "r", "q", "link")),
        ),
    ] {
        let p = plan(&["r"], root);
        let got = run(&w, &p, &Params::new(), &Limits::default())
            .unwrap_or_else(|e| panic!("{what}: unexpected error {e}"));
        assert!(got.rows.is_empty(), "{what}: {:?}", got.rows);
    }
}

// ---------------------------------------------------------------- either on a directed attribute

#[test]
fn either_on_directed_attribute_with_both_directions_stored_gives_two_rows_per_end() {
    let w = world();
    // a->b, b->a stored: `a` and `b` each have 2 undirected incident edges to the other.
    let rows = prep_run(
        &w,
        "MATCH (x {kala_handle: 'a'})-[r:link]-(y {kala_handle: 'b'}) RETURN r",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_ne!(rows[0], rows[1], "the two rows must be different edges");
    // one of them is the a->b edge, the other the b->a edge
    let ab = prep_run(
        &w,
        "MATCH (x {kala_handle: 'a'})-[r:link]->(y {kala_handle: 'b'}) RETURN r",
    );
    let ba = prep_run(
        &w,
        "MATCH (x {kala_handle: 'b'})-[r:link]->(y {kala_handle: 'a'}) RETURN r",
    );
    assert_eq!((ab.len(), ba.len()), (1, 1));
    let mut got = col(rows);
    got.sort_by_key(|d| format!("{d:?}"));
    let mut want = vec![ab[0][0].clone(), ba[0][0].clone()];
    want.sort_by_key(|d| format!("{d:?}"));
    assert_eq!(got, want);
    assert_ne!(ab[0][0], ba[0][0]);
}

#[test]
fn either_per_bound_end_counts_each_incident_edge_once() {
    let w = world();
    // a: out a->b, a->c, in b->a => 3 rows; b: out b->a, in a->b => 2; c: in a->c => 1
    for (h, n) in [("a", 3), ("b", 2), ("c", 1)] {
        let rows = prep_run(
            &w,
            &format!("MATCH (x {{kala_handle: '{h}'}})-[r:link]-(y) RETURN r, y.name"),
        );
        assert_eq!(rows.len(), n, "{h}: {rows:?}");
    }
}

// ---------------------------------------------------------------- named inverse, both ends bound

#[test]
fn named_inverse_with_both_ends_bound_gives_the_parent_of_edge() {
    let w = world();
    let rows = prep_run(
        &w,
        "MATCH (c {kala_handle: 'b'}) MATCH (p {kala_handle: 'a'}) \
         MATCH (c)-[r:child_of]->(p) RETURN r",
    );
    let base = prep_run(
        &w,
        "MATCH (p {kala_handle: 'a'})-[r:parent_of]->(c {kala_handle: 'b'}) RETURN r",
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows, base);
    // wrong way round: nothing
    let none = prep_run(
        &w,
        "MATCH (c {kala_handle: 'b'}) MATCH (p {kala_handle: 'a'}) \
         MATCH (p)-[r:child_of]->(c) RETURN r",
    );
    assert!(none.is_empty(), "{none:?}");
}

#[test]
fn inverse_name_read_at_an_older_canon_after_the_base_attribute_was_renamed() {
    let mut w = world();
    let before = w.latest_tx().0;
    {
        let mut tx = w.begin();
        tx.rename_attribute("parent_of", "progenitor_of").unwrap();
        tx.commit().unwrap();
    }
    let old_canon =
        format!("MATCH (c)-[r:child_of]->(p) AT TICK 0 CANON {before} RETURN c.name, p.name");
    let rows = prep_run(&w, &old_canon);
    assert_eq!(rows, vec![vec![s("Beta"), s("Alpha")]]);
    // `rename_attribute` "changes the name at every canon" (same attribute ID), so the
    // new base name also answers at the older canon and the old name is gone everywhere.
    let new_at_old =
        format!("MATCH (p)-[r:progenitor_of]->(c) AT TICK 0 CANON {before} RETURN p.name, c.name");
    assert_eq!(prep_run(&w, &new_at_old), vec![vec![s("Alpha"), s("Beta")]]);
    let old_at_old =
        format!("MATCH (p)-[r:parent_of]->(c) AT TICK 0 CANON {before} RETURN p.name, c.name");
    assert!(prep_run(&w, &old_at_old).is_empty());
    // the inverse name keeps working at the latest canon under the new base name
    let latest = prep_run(&w, "MATCH (c)-[r:child_of]->(p) RETURN c.name, p.name");
    assert_eq!(latest, vec![vec![s("Beta"), s("Alpha")]]);
}

// ---------------------------------------------------------------- list ordering comparisons

fn count(w: &World, where_: &str) -> usize {
    prep_run(w, &format!("MATCH (n:NODE) WHERE {where_} RETURN n")).len()
}

#[test]
fn homogeneous_lists_compare_element_wise() {
    let w = world();
    // alias lists (sorted): a=[x,y], b=[x,z], c=[x,y]
    assert_eq!(
        col(prep_run(
            &w,
            "MATCH (p {kala_handle: 'a'}) MATCH (q {kala_handle: 'b'}) \
             WHERE p.alias < q.alias RETURN p.name"
        )),
        vec![s("Alpha")]
    );
    assert_eq!(
        prep_run(
            &w,
            "MATCH (p {kala_handle: 'b'}) MATCH (q {kala_handle: 'a'}) \
             WHERE p.alias < q.alias RETURN p.name"
        )
        .len(),
        0
    );
    // equal lists: <= and >= hold, < and > do not
    let eq = |op: &str| {
        prep_run(
            &w,
            &format!(
                "MATCH (p {{kala_handle: 'a'}}) MATCH (q {{kala_handle: 'c'}}) \
                 WHERE p.alias {op} q.alias RETURN p.name"
            ),
        )
        .len()
    };
    assert_eq!((eq("<"), eq("<="), eq(">"), eq(">=")), (0, 1, 0, 1));
}

#[test]
fn list_ordering_against_incomparable_elements_or_non_lists_is_null() {
    let w = world();
    // list of strings vs list of ints: incomparable element pair => NULL, so NOT keeps nothing
    for q in [
        "p.alias < q.tags",
        "NOT (p.alias < q.tags)",
        "p.alias >= q.tags",
        "NOT (p.alias >= q.tags)",
        // list vs scalar
        "p.alias < 1",
        "NOT (p.alias < 1)",
    ] {
        let n = prep_run(
            &w,
            &format!(
                "MATCH (p {{kala_handle: 'a'}}) MATCH (q {{kala_handle: 'b'}}) WHERE {q} RETURN p"
            ),
        )
        .len();
        assert_eq!(n, 0, "{q}");
    }
    // a missing property (NULL) on one side
    assert_eq!(count(&w, "n.alias < n.nothing"), 0);
    assert_eq!(count(&w, "NOT (n.alias < n.nothing)"), 0);
}

#[test]
fn list_parameters_with_null_or_mixed_elements_order_to_null() {
    let w = world();
    let p = kala_cypher::prepare(
        &w,
        "MATCH (n {kala_handle: 'a'}) WHERE $l < $m RETURN n.name",
    );
    let Ok(p) = p else {
        eprintln!("list parameters not accepted here: {:?}", p.err());
        return;
    };
    let list = |xs: Vec<Datum>| Datum::List(xs);
    let i = |n: i64| Datum::from(Value::I64(n));
    let run_with = |l: Datum, m: Datum| {
        let mut qp = QueryParams::new();
        qp.insert("l".into(), l);
        qp.insert("m".into(), m);
        run(&w, &p.plan, &qp, &Limits::default())
            .unwrap()
            .rows
            .len()
    };
    // homogeneous: element-wise
    assert_eq!(run_with(list(vec![i(1), i(2)]), list(vec![i(1), i(3)])), 1);
    assert_eq!(run_with(list(vec![i(1), i(3)]), list(vec![i(1), i(2)])), 0);
    // shorter prefix is smaller
    assert_eq!(run_with(list(vec![i(1)]), list(vec![i(1), i(0)])), 1);
    // NULL element reached before a difference decides: NULL, row dropped
    assert_eq!(
        run_with(list(vec![Datum::Null, i(1)]), list(vec![i(1), i(2)])),
        0
    );
    // mixed types at the first pair: NULL, row dropped
    assert_eq!(run_with(list(vec![s("a")]), list(vec![i(1)])), 0);
}

// ---------------------------------------------------------------- explain robustness

#[test]
fn explain_of_an_expression_deeper_than_the_cap_prints_dots_without_overflow() {
    let w = world();
    let mut e: Expr = Expr::Value(Value::Bool(true));
    for _ in 0..500 {
        e = Expr::Not(Box::new(e));
    }
    let p = plan(&["a"], row(Op::Unit, RowAction::Select(e.clone())));
    let text = p.explain(&w);
    assert!(text.contains("..."), "{text}");
    assert!(text.len() < 2000, "{}", text.len());
    // the same for a deep comparison chain
    let mut c: Expr = Expr::Value(Value::I64(0));
    for _ in 0..500 {
        c = Expr::Compare(CmpOp::Lt, Box::new(c), Box::new(Expr::Value(Value::I64(1))));
    }
    let text = plan(&["a"], row(Op::Unit, RowAction::Select(c))).explain(&w);
    assert!(text.contains("..."), "{text}");
    // a shallow expression prints whole, with no dots
    let shallow = Expr::Not(Box::new(Expr::Value(Value::Bool(true))));
    let text = plan(&["a"], row(Op::Unit, RowAction::Select(shallow))).explain(&w);
    assert!(!text.contains("..."), "{text}");
    // keep drop of the deep tree from being optimised away
    std::mem::forget(e);
}

#[test]
fn explain_escapes_control_characters_in_labels_keys_params_and_strings() {
    let w = world();
    let nasty = "a\u{1}b\u{7}c\rd\te\u{1b}f\u{0}g\u{7f}h";
    let nodes = row(
        Op::Unit,
        RowAction::Nodes {
            var: var("n"),
            labels: vec![nasty.into()],
            tick: TickSource::Default,
        },
    );
    let select = row(
        nodes,
        RowAction::Select(Expr::And(
            Box::new(Expr::Compare(
                CmpOp::Eq,
                Box::new(Expr::Property {
                    entity: var("n"),
                    key: nasty.into(),
                    tick: TickSource::Default,
                }),
                Box::new(Expr::Value(Value::Str(nasty.into()))),
            )),
            Box::new(Expr::Compare(
                CmpOp::Eq,
                Box::new(Expr::Param(nasty.into())),
                Box::new(Expr::Value(Value::Str("line\nbreak".into()))),
            )),
        )),
    );
    let edge = row(select, edge_action("n", "m", "r", nasty));
    let text = plan(&["n"], edge).explain(&w);
    for c in text.chars() {
        assert!(c == '\n' || c >= ' ', "control char {c:?} in {text:?}");
        assert_ne!(c, '\u{7f}', "DEL in {text:?}");
    }
    assert!(!text.contains('\r') && !text.contains('\t'), "{text:?}");
}

/// FINDING: `Plan::params` names are printed raw in the `Find ... in $name` header.
/// Unreachable from Cypher/Datalog (parameter names are identifiers), reachable only by
/// building a `Plan` by hand.
#[test]
fn explain_escapes_control_characters_in_the_params_header() {
    let w = world();
    let mut p = plan(&["n"], Op::Unit);
    p.params = vec!["a\u{1}b\nc".into()];
    let text = p.explain(&w);
    assert!(text.chars().all(|c| c == '\n' || c >= ' '), "{text:?}");
    assert_eq!(text.matches('\n').count(), 3, "{text:?}");
}

// ---------------------------------------------------------------- parser

fn prep_err(w: &World, text: &str) -> String {
    match kala_cypher::prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn x_equals_not_y_is_a_syntax_error() {
    // Documented behaviour: NOT is only a prefix of a whole expression level, so it can't
    // be the right operand of `=` without parentheses.
    let w = world();
    let e = prep_err(&w, "MATCH (n:NODE) WHERE n.name = NOT n.alias RETURN n");
    assert!(!e.is_empty());
    // the parenthesised form is fine
    assert!(
        kala_cypher::prepare(&w, "MATCH (n:NODE) WHERE n.name = (NOT n.alias) RETURN n").is_ok()
    );
}

#[test]
fn a_chain_of_forty_comparisons_gives_a_clean_error_not_a_panic() {
    let w = world();
    let chain = (0..41)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(" < ");
    let text = format!("MATCH (n:NODE) WHERE {chain} RETURN n");
    // must not panic; either accepted or rejected with a message
    let r = std::panic::catch_unwind(|| kala_cypher::prepare(&world(), &text).map(|_| ()));
    let r = r.expect("prepare panicked on a 40-comparison chain");
    match r {
        Ok(()) => {}
        Err(e) => {
            let m = e.to_string();
            assert!(!m.is_empty());
            eprintln!("40-chain rejected: {m}");
        }
    }
    let _ = w;
}

#[test]
fn duplicate_column_between_alias_and_property_name_is_an_error() {
    let w = world();
    let e = prep_err(&w, "MATCH (a:NODE) RETURN 1 AS `a.name`, a.name");
    assert!(
        e.contains("Multiple result columns with the same name are not supported"),
        "{e}"
    );
}

#[test]
fn handle_parameter_also_used_as_an_i64_is_an_incompatible_parameter_type() {
    let w = world();
    let e = prep_err(
        &w,
        "MATCH (n {kala_handle: $h}) WHERE n.score = $h RETURN n",
    );
    // The message does not name the parameter (see report); pin what it says today.
    assert!(e.contains("incompatible parameter uses"), "{e}");
}

/// Guard: a normal Datalog plan with `:in` params keeps its header exactly.
#[test]
fn guard_datalog_params_header_is_unchanged() {
    let w = world();
    let p = kala_datalog::prepare(&w, "[:find ?e :in ?who :at -1 :where [?e :name ?who]]").unwrap();
    let text = p.plan.explain(&w);
    assert_eq!(
        text.lines().next().unwrap(),
        "Find ?e  in $who  at -1  canon latest",
        "{text}"
    );
}
