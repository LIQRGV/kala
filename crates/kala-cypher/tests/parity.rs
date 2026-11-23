//! Milestone 1: independently parsed languages share the exact IR and rows.
use kala_core::{Tick, Value, load_fixture};
use kala_query::{Limits, Params, run};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn parity(datalog: &str, cypher: &str, params: Params) {
    let world = load_fixture(VARROW).unwrap();
    let dl = kala_datalog::prepare(&world, datalog).unwrap();
    let cy = kala_cypher::prepare(&world, cypher);
    assert!(cy.is_ok(), "Cypher must compile Edwin's allies: {cy:?}");
    let cy = cy.unwrap();
    assert_eq!(
        cy.plan, dl.plan,
        "front-ends must produce equal Plan values"
    );
    assert_eq!(cy.plan.explain(&world), dl.plan.explain(&world));
    let rows = run(&world, &cy.plan, &params, &Limits::default()).unwrap();
    assert_eq!(
        rows,
        run(&world, &dl.plan, &params, &Limits::default()).unwrap()
    );
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.tick, Tick(-210));
    let expected: [Vec<kala_query::Datum>; 2] =
        ["greg", "tomas"].map(|h| vec![Value::Ref(world.entity_by_handle(h).unwrap()).into()]);
    assert_eq!(rows.rows, expected);

    let bag = kala_cypher::prepare(&world, &cypher.replace("RETURN DISTINCT", "RETURN")).unwrap();
    assert_ne!(bag.plan, cy.plan);
    assert_eq!(bag.plan.find, cy.plan.find);
    assert_eq!(bag.plan.params, cy.plan.params);
    assert_eq!(bag.plan.default_tick, cy.plan.default_tick);
    assert_eq!(bag.plan.canon, cy.plan.canon);
    assert_eq!(
        run(&world, &bag.plan, &params, &Limits::default())
            .unwrap()
            .rows,
        rows.rows
    );
}

#[test]
fn edwins_allies_same_plan_and_rows() {
    parity(
        "[:find ?a :at -210 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]",
        "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -210 RETURN DISTINCT a",
        Params::new(),
    );
}

#[test]
fn edwins_allies_same_parameter_tick_plan_and_rows() {
    parity(
        "[:find ?a :in ?t :at ?t :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]",
        "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK $t RETURN DISTINCT a",
        [("t".into(), Value::I64(-210))].into(),
    );
}

#[test]
fn edwins_allies_same_historical_canon_plan_and_rows() {
    parity(
        "[:find ?a :at -210 :canon 10 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]",
        "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -210 CANON 10 RETURN DISTINCT a",
        Params::new(),
    );
}
