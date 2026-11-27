//! Equivalent Cypher and Datalog statements append identical transactions.
use kala_core::World;
use kala_query::{Params, RunOptions};
use std::fs;

#[path = "common/write_cases.rs"]
mod write_cases;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn parity(cypher: &str, datalog: &str) {
    with_setup(None, cypher, datalog);
}

fn with_setup(setup: Option<&str>, cypher: &str, datalog: &str) {
    let dir = tempfile::tempdir().unwrap();
    let dl_path = dir.path().join("datalog.kala");
    let cy_path = dir.path().join("cypher.kala");
    let mut dl = kala_fixture::load_fixture_to(VARROW, &dl_path).unwrap();
    if let Some(setup) = setup {
        let form = kala_datalog::parse_write(setup).unwrap();
        kala_datalog::apply::execute(&mut dl, &form, None).unwrap();
    }
    fs::copy(&dl_path, &cy_path).unwrap();
    let form = kala_datalog::parse_write(datalog).unwrap();
    kala_datalog::apply::execute(&mut dl, &form, None).unwrap();

    let statement =
        kala_cypher::parse_statement(cypher).unwrap_or_else(|err| panic!("{cypher}: {err}"));
    let kala_cypher::ast::Statement::Write(write) = statement else {
        panic!("expected a write: {cypher}");
    };
    let mut cy = World::open(&cy_path).unwrap();
    kala_cypher::write::execute(
        &mut cy,
        &write,
        &Params::new(),
        RunOptions::new(Default::default()),
    )
    .unwrap_or_else(|err| panic!("{cypher}: {err}"));
    assert!(
        fs::read(cy_path).unwrap() == fs::read(dl_path).unwrap(),
        "Cypher and Datalog appended different log bytes: {cypher}"
    );
}

#[test]
fn create_node_matches_the_datalog_forms_byte_for_byte() {
    parity(
        "CREATE (n:CHARACTER {kala_handle: 'kael2', name: 'Kael'}) AT TICK 700",
        r#"[[:create :CHARACTER :kael2] [:assert :kael2 :name "Kael" :at 700]]"#,
    );
}

#[test]
fn writes_match_the_equivalent_facts_at_each_tick() {
    for &(cypher, datalog) in write_cases::CASES {
        parity(cypher, datalog);
    }
}

#[test]
fn inverse_name_asserts_the_stored_relationship_with_flipped_endpoints() {
    with_setup(
        Some("[:attribute :parent_of :ref :state :many :directed :inverse :child_of]"),
        "MATCH (g {kala_handle:'greg'}), (m {kala_handle:'mara'}) CREATE (g)-[:child_of]->(m) AT TICK 700",
        "[:assert :mara :parent_of :greg :at 700]",
    );
}

#[test]
fn replacing_an_inverse_relationship_retracts_the_old_incoming_value() {
    with_setup(
        Some(
            "[:at 400 [:attribute :parent_of :ref :state :many :directed :inverse :child_of] [:assert :edwin :parent_of :greg]]",
        ),
        "MATCH (g {kala_handle:'greg'}), (m {kala_handle:'mara'}) SET g.child_of = m AT TICK 700",
        "[:at 700 [:retract :edwin :parent_of :greg] [:assert :mara :parent_of :greg]]",
    );
}

#[test]
fn newly_inferred_extra_labels_have_many_cardinality() {
    let mut w = World::in_memory(kala_core::Schema::new()).unwrap();
    let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(
        "CREATE (n:CHARACTER:Scout:Guide {kala_handle:'new'}) AT TICK 0",
    )
    .unwrap() else {
        panic!("expected a write");
    };
    kala_cypher::write::execute(
        &mut w,
        &stmt,
        &Params::new(),
        RunOptions::new(Default::default()),
    )
    .unwrap();
    let label = w.attributes().find(|a| a.name == "label").unwrap();
    assert_eq!(label.cardinality, kala_core::Cardinality::Many);
    let new = w.entity_by_handle("new").unwrap();
    assert_eq!(w.as_of(new, "label", kala_core::Tick(0)).unwrap().len(), 2);
}

#[test]
fn detaching_a_node_emits_incoming_and_outgoing_retractions_in_stored_order() {
    with_setup(
        Some(
            r#"[:at 720 [:create :CHARACTER :noble] [:assert :greg :present :noble] [:assert :noble :present :mara] [:assert :noble :title "Steward"]]"#,
        ),
        "MATCH (n {kala_handle:'noble'}) DETACH DELETE n AT TICK 721",
        r#"[:at 721 [:retract :greg :present :noble] [:retract :noble :present :mara] [:retract :noble :title "Steward"]]"#,
    );
}

#[test]
fn exhausted_staging_budget_rolls_back_and_a_retry_can_commit() {
    use kala_cypher::write;
    use kala_query::{Datum, Limits, QueryParams};
    let mut w = kala_fixture::load_fixture(VARROW).unwrap();
    let before = w.latest_tx();
    let query = "UNWIND $rows AS i CREATE (:CHARACTER {score:i})";
    let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(query).unwrap()
    else {
        panic!("expected a write");
    };
    let mut p = QueryParams::new();
    p.insert(
        "rows".into(),
        Datum::List((0..64).map(|i| kala_core::Value::I64(i).into()).collect()),
    );
    let unbounded = RunOptions::new(Limits::default());
    let input = write::prepare(&w, &stmt, &p, unbounded).unwrap();
    let (_, usage) = kala_query::run_with(&w, &input.prepared.plan, &p, unbounded).unwrap();
    let small = RunOptions::new(Limits {
        max_steps: usage.steps + 20,
        ..Limits::default()
    });
    // Confirm that all 64 input rows fit, then fail during the writes.
    let input = write::prepare(&w, &stmt, &p, small).unwrap();
    let mut tx = w.begin();
    let err = write::stage(&mut tx, &stmt, input, &p)
        .err()
        .expect("work limit must stop the write");
    assert!(err.to_string().contains("step"), "{err}");
    assert!(
        tx.commit().is_err(),
        "a partial write must be uncommittable"
    );
    assert_eq!(w.latest_tx(), before);
    write::execute(&mut w, &stmt, &p, unbounded).unwrap();
    assert_eq!(w.latest_tx().0, before.0 + 1);
}

#[test]
fn signed_tick_extremes_survive_the_write_path() {
    for tick in [i64::MIN, 0, i64::MAX] {
        parity(
            &format!("CREATE (:CHARACTER {{kala_handle:'tick',age:1}}) AT TICK {tick}"),
            &format!("[:at {tick} [:create :CHARACTER :tick] [:assert :tick :age 1]]"),
        );
    }
}

#[test]
fn an_isolated_node_delete_keeps_its_handle_and_past_states() {
    with_setup(
        Some(r#"[:at -7 [:create :CHARACTER :isolated] [:assert :isolated :name "Wanderer"]]"#),
        "MATCH (n {kala_handle:'isolated'}) DELETE n AT TICK 5",
        r#"[:retract :isolated :name "Wanderer" :at 5]"#,
    );
}

#[test]
fn an_expired_deadline_between_read_and_write_prevents_commit() {
    let mut w = kala_fixture::load_fixture(VARROW).unwrap();
    let before = w.latest_tx();
    let params = Params::new();
    let kala_cypher::ast::Statement::Write(stmt) =
        kala_cypher::parse_statement("MATCH (n:CHARACTER) SET n.score = 1 AT TICK 910").unwrap()
    else {
        panic!("expected a write");
    };
    let input = kala_cypher::write::prepare(&w, &stmt, &params, RunOptions::default()).unwrap();
    let mut tx = w.begin();
    tx.set_deadline(std::time::Instant::now());
    assert!(kala_cypher::write::stage(&mut tx, &stmt, input, &params).is_err());
    assert!(tx.commit().is_err());
    assert_eq!(w.latest_tx(), before);
}

#[test]
fn staging_prepared_rows_checks_the_snapshot_that_produced_them() {
    let params = Params::new();
    let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(
        "MATCH (n {kala_handle:'greg', name:'Greg'}) SET n.disguised = true AT TICK 913",
    )
    .unwrap() else {
        panic!("expected a write");
    };
    for (intervening, conflicts) in [
        (r#"[:set :greg :name "Captain" :at 911]"#, true),
        (r#"[:assert :mara :alias "Wayfinder" :at 911]"#, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prepared.kala");
        let mut w = kala_fixture::load_fixture_to(VARROW, &path).unwrap();
        let input = kala_cypher::write::prepare(&w, &stmt, &params, RunOptions::default()).unwrap();
        let mut other = World::open(&path).unwrap();
        kala_datalog::apply::execute(
            &mut other,
            &kala_datalog::parse_write(intervening).unwrap(),
            None,
        )
        .unwrap();
        let mut tx = w.begin();
        kala_cypher::write::stage(&mut tx, &stmt, input, &params).unwrap();
        let commit = tx.commit();
        if conflicts {
            assert!(
                matches!(commit, Err(kala_core::Error::Conflict(_))),
                "stale MATCH must conflict"
            );
        } else {
            commit.unwrap();
        }
    }
}

#[test]
fn returning_null_columns_charges_their_allocated_row_cells() {
    let mut w = World::in_memory(kala_core::Schema::new().node_type("T")).unwrap();
    let before = w.latest_tx();
    let params = Params::new();
    let projection = (0..12)
        .map(|col| format!("NULL AS c{col}"))
        .collect::<Vec<_>>()
        .join(", ");
    let kala_cypher::ast::Statement::Write(stmt) =
        kala_cypher::parse_statement(&format!("CREATE (:T) RETURN {projection}")).unwrap()
    else {
        panic!("expected a write");
    };
    let input = kala_cypher::write::prepare(&w, &stmt, &params, RunOptions::default()).unwrap();
    let (_, usage) =
        kala_query::run_with(&w, &input.prepared.plan, &params, RunOptions::default()).unwrap();
    let limit = RunOptions::new(kala_query::Limits {
        max_bytes: usage.peak_bytes + 4 * std::mem::size_of::<kala_query::Datum>(),
        ..Default::default()
    });
    let input = kala_cypher::write::prepare(&w, &stmt, &params, limit).unwrap();
    let mut tx = w.begin();
    let result = kala_cypher::write::stage(&mut tx, &stmt, input, &params);
    assert!(
        result.is_err(),
        "12 output cells cannot fit in four cells of memory"
    );
    assert!(tx.commit().is_err());
    assert_eq!(w.latest_tx(), before);
}

#[test]
fn committed_return_rows_follow_the_id_remap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remap.kala");
    let mut w = kala_fixture::load_fixture_to(VARROW, &path).unwrap();
    let mut other = World::open(&path).unwrap();
    let params = Params::new();
    let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(
        "CREATE (n:CHARACTER {kala_handle:'ours',name:'Aster'}) AT TICK 801 RETURN n, n.kala_handle, n.name"
    ).unwrap() else {panic!("expected a write");};
    let mut tx = w.begin();
    let input = kala_cypher::write::prepare(tx.base_world(), &stmt, &params, RunOptions::default())
        .unwrap();
    let mut staged = kala_cypher::write::stage(&mut tx, &stmt, input, &params).unwrap();
    let provisional = staged.rows[0][0].clone();
    other.create_entity("CHARACTER", Some("theirs")).unwrap();
    let result = tx.commit().unwrap();
    assert!(!result.ids.entities.is_empty());
    let committed = staged.committed_rows(&w, &result.ids).unwrap();
    let ours = w.entity_by_handle("ours").unwrap();
    assert_ne!(committed[0][0], provisional);
    assert_eq!(
        committed[0],
        vec![
            kala_core::Value::Ref(ours),
            kala_core::Value::Str("ours".into()),
            kala_core::Value::Str("Aster".into()),
        ]
    );
}
