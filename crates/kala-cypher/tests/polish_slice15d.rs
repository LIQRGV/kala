//! PR #22 review follow-ups for Cypher write statements.
use kala_core::{Tick, Value, World};
use kala_cypher::write::{WriteError, execute};
use kala_query::{Datum, Limits, Params, QueryParams, RunOptions};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn varrow() -> World {
    kala_fixture::load_fixture(VARROW).unwrap()
}

fn statement(text: &str) -> kala_cypher::ast::WriteStatement {
    match kala_cypher::parse_statement(text).unwrap_or_else(|e| panic!("{text}: {e}")) {
        kala_cypher::ast::Statement::Write(write) => write,
        _ => panic!("expected a write: {text}"),
    }
}

fn run(world: &mut World, text: &str) -> Result<kala_core::write::WriteReport, WriteError> {
    execute(
        world,
        &statement(text),
        &Params::new(),
        RunOptions::default(),
    )
}

fn ok(world: &mut World, text: &str) -> kala_core::write::WriteReport {
    run(world, text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

// ---- B: the row limit stops the reading part early ----

#[test]
fn the_row_limit_stops_the_reading_part_before_it_materialises_every_row() {
    let mut world = varrow();
    let mut params = QueryParams::new();
    params.insert(
        "xs".into(),
        Datum::List((0..60_000).map(|i| Datum::Value(Value::I64(i))).collect()),
    );
    // Enough memory to bind the list and run the pipeline, not enough to
    // also build the 60,000-row answer (measured: 14.2 MiB without the
    // answer, 20.1 MiB with it): counting after materialising the answer
    // would be a memory-budget error instead.
    let limits = Limits {
        max_bytes: 18 << 20,
        ..Limits::default()
    };
    let error = execute(
        &mut world,
        &statement("UNWIND $xs AS i CREATE (:VISITOR)"),
        &params,
        RunOptions::new(limits),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "statement exceeds the 10000-row write limit"
    );
    assert_eq!(world.latest_tx(), varrow().latest_tx());
}

#[test]
fn the_row_limit_counts_answer_rows_not_the_rows_a_match_scans() {
    let mut world = varrow();
    let mut tx = world.begin();
    tx.infer_node_type("VISITOR").unwrap();
    for i in 0..12_000 {
        let id = tx.create_entity("VISITOR", None).unwrap();
        tx.write_inferring(
            id,
            "n",
            &Value::I64(i),
            Some(Tick(1)),
            kala_core::Op::Assert,
            kala_core::Precision::Exact,
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let report = ok(
        &mut world,
        "MATCH (c:VISITOR) WHERE c.n < 5 SET c.x = 1 AT TICK 2",
    );
    assert_eq!(report.changed, 5);
    let error = run(&mut world, "MATCH (c:VISITOR) SET c.x = 1 AT TICK 2").unwrap_err();
    assert_eq!(
        error.to_string(),
        "statement exceeds the 10000-row write limit"
    );
}
