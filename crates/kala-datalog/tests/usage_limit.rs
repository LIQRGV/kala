//! The step budget and reported usage agree (PR #2 review round 1).

// Review round 1: the step budget and the reported usage must agree. A query
// whose reads walk many IDs (an attribute scan at a per-row tick re-walks
// every defined ID each time) must succeed under a step limit equal to the
// usage it reports; ID walks are real work and count as steps.
#[test]
fn a_query_fits_a_limit_equal_to_its_own_usage() {
    let world = kala_core::load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml"))
        .unwrap();
    let q = "[:find ?c ?d :where [?x :located_in ?q ?t] (at ?t [?c :disguised ?d])]";
    let p = kala_datalog::prepare(&world, q).unwrap();
    let (rows, usage) = kala_query::run_with(
        &world,
        &p.plan,
        &kala_query::Params::new(),
        kala_query::RunOptions::new(kala_query::Limits::default()),
    )
    .unwrap();
    let exact = kala_query::Limits {
        max_steps: usage.steps,
        ..kala_query::Limits::default()
    };
    let (again, _) = kala_query::run_with(
        &world,
        &p.plan,
        &kala_query::Params::new(),
        kala_query::RunOptions::new(exact),
    )
    .unwrap_or_else(|e| panic!("limit {} = own usage, got {e}", usage.steps));
    assert_eq!(again.rows, rows.rows);
}
