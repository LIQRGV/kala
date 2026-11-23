//! Polish fix H: `Plan::explain` renders `Row` and `Output` stably (no Rust Debug,
//! no control characters, generated variables as `?#N`).
use kala_core::{World, load_fixture};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
fn world() -> World {
    load_fixture(VARROW).unwrap()
}
fn explain(w: &World, text: &str) -> String {
    kala_cypher::prepare(w, text)
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .plan
        .explain(w)
}

/// A bag query using every row action: nodes, edge, bind (ORDER BY keys), select.
const BAG: &str = "MATCH (c:CHARACTER:City)-[r:present]->(p) \
    WHERE c.alias IS NULL OR p.name <> 'x' \
    RETURN c, r ORDER BY c.name DESC, p.name SKIP 1 LIMIT $l";

/// Pinned rendering. `?#1`/`?#2` are the generated order-key variables
/// (`\u{1}`/`\u{2}` today); `<>` is `CmpOp::Ne`; `out` is the edge orientation.
const BAG_EXPLAIN: &str = "\
Find ?c ?r  in $l  at latest  canon latest
  Output ?c ?r  order ?#1 desc, ?#2 asc  skip 1  limit $l
    Row bind ?#2 = ?p.name@latest
      Row bind ?#1 = ?c.name@latest
        Row select (?c.alias@latest IS NULL OR ?p.name@latest <> \"x\")
          Row nodes ?c :CHARACTER:City @latest
            Row edge [?c :present ?p] as ?r out @latest
              Unit
";

#[test]
fn explain_of_a_bag_query_is_pinned_text() {
    let w = world();
    assert_eq!(explain(&w, BAG), BAG_EXPLAIN);
}

#[test]
fn guard_explain_has_no_control_characters_other_than_newline() {
    let w = world();
    for q in [
        BAG,
        "MATCH (c:CHARACTER) RETURN DISTINCT c.name ORDER BY c.name LIMIT 3",
        "MATCH (c:CHARACTER)-[r:ally_of]-(d) AT TICK -240 RETURN c, d ORDER BY d.name DESC, c.name",
    ] {
        let text = explain(&w, q);
        assert!(
            text.chars().all(|c| c == '\n' || c >= ' '),
            "control character in explain of {q}:\n{text:?}"
        );
    }
}

#[test]
fn explain_prints_generated_variables_as_hash_numbers() {
    let w = world();
    for q in [
        BAG,
        "MATCH (c:CHARACTER) RETURN DISTINCT c.name ORDER BY c.name LIMIT 3",
    ] {
        let text = explain(&w, q);
        assert!(
            text.contains("?#1"),
            "generated vars should print as ?#N:\n{text}"
        );
    }
}

#[test]
fn explain_does_not_use_rust_debug_for_rows_or_output() {
    let w = world();
    let text = explain(&w, BAG);
    for debug in [
        "OutputSpec",
        "Var(",
        "Bind {",
        "Select(",
        "Nodes {",
        "Edge {",
        "Property {",
    ] {
        assert!(!text.contains(debug), "{debug} leaked into:\n{text}");
    }
}

/// Plain `RETURN DISTINCT` (no ORDER BY/SKIP/LIMIT) deliberately compiles to the set
/// `Project` (parity with Datalog). First-clause row actions use `TickSource::Default`,
/// which explain prints as `@latest`, like Datalog `Expand` lines under `at -240`; the
/// `-240` shows only in the header.
#[test]
fn explain_plain_distinct_is_a_project_and_default_ticks_print_latest() {
    let w = world();
    let text = explain(
        &w,
        "MATCH (c:CHARACTER)-[r:ally_of]-(d) AT TICK -240 RETURN DISTINCT c, d",
    );
    assert_eq!(
        text,
        "Find ?c ?d  at -240  canon latest\n  Project ?c ?d\n    Row nodes ?c :CHARACTER @latest\n      Row edge [?c :ally_of ?d] as ?r either @latest\n        Unit\n"
    );
}

/// With ORDER BY or LIMIT the bag `Output` is used and prints `distinct`.
#[test]
fn explain_distinct_appears_on_the_output_line_with_order_or_limit() {
    let w = world();
    let ordered = explain(
        &w,
        "MATCH (c:CHARACTER)-[r:ally_of]-(d) AT TICK -240 RETURN DISTINCT c, d ORDER BY d DESC",
    );
    assert_eq!(
        ordered,
        "Find ?c ?d  at -240  canon latest\n  Output ?c ?d  distinct  order ?d desc\n    Row nodes ?c :CHARACTER @latest\n      Row edge [?c :ally_of ?d] as ?r either @latest\n        Unit\n"
    );
    let limited = explain(
        &w,
        "MATCH (c:CHARACTER)-[r:ally_of]-(d) AT TICK -240 RETURN DISTINCT c, d LIMIT 3",
    );
    assert!(
        limited.contains("  Output ?c ?d  distinct  limit 3\n"),
        "{limited}"
    );
    assert!(!limited.contains("Project"), "{limited}");
}

/// A later MATCH clause with its own `AT TICK` names it on its row action; the plan
/// default (header) stays `latest` because the first clause names no tick.
#[test]
fn explain_later_match_clause_prints_its_own_tick() {
    let w = world();
    let either = explain(
        &w,
        "MATCH (c:CHARACTER) MATCH (c)-[r:ally_of]-(d) AT TICK -240 RETURN c, d",
    );
    assert_eq!(
        either,
        "Find ?c ?d  at latest  canon latest\n  Output ?c ?d\n    Row edge [?c :ally_of ?d] as ?r either @-240\n      Row nodes ?c :CHARACTER @latest\n        Unit\n"
    );
    let out = explain(
        &w,
        "MATCH (c:CHARACTER) MATCH (c)-[r:located_in]->(d) AT TICK -240 RETURN c, d",
    );
    assert!(out.contains("as ?r out @-240"), "{out}");
    assert!(out.contains("Row nodes ?c :CHARACTER @latest"), "{out}");
}

/// Guard: Datalog explain stays exactly as the plan documents.
#[test]
fn guard_datalog_explain_is_unchanged() {
    let w = world();
    let p = kala_datalog::prepare(
        &w,
        "[:find ?a :at -210 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]",
    )
    .unwrap();
    assert_eq!(
        p.plan.explain(&w),
        "Find ?a  at -210  canon latest\n  Project ?a\n    Expand [?e :ally_of ?a] @latest via E\n      Expand [?e :kala_handle \"edwin\"] via handle\n        Unit\n"
    );
}

/// Explain renders IR from any front-end: schema names in NodeScan and
/// Expand lines are escaped like Row/Output names (security review, low).
#[test]
fn explain_escapes_control_characters_in_scan_and_expand_names() {
    use kala_query::{Access, AttrRef, CanonSource, Op, Plan, Term, TickSource, Var};
    let world = kala_core::load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml"))
        .unwrap();
    let scan = Op::NodeScan {
        input: Box::new(Op::Unit),
        var: Var("n".into()),
        node_type: "A\u{1b}[2J".into(),
    };
    let plan = Plan {
        find: vec![Var("n".into())],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root: Op::Expand {
            input: Box::new(scan),
            entity: Term::Var(Var("n".into())),
            attr: AttrRef::Named("b\nc".into()),
            value: Term::Var(Var("v".into())),
            tick: TickSource::Default,
            access: Access::E,
        },
    };
    let text = plan.explain(&world);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains(":b\\nc"), "{text:?}");
    assert_eq!(text.lines().count(), 4, "{text:?}");
}
