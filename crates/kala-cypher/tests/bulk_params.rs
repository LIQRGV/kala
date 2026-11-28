//! Map-row writes keep their independently supplied story ticks.
use kala_core::{Schema, Tick, Value, World};
use kala_query::{Datum, QueryParams, RunOptions};
use std::collections::BTreeMap;

const MERGE: &str = "UNWIND $rows AS row MERGE (n:NPC {kala_handle: row.handle}) SET n.name = row.name AT TICK row.tick";

fn inputs() -> QueryParams {
    let mut params = QueryParams::new();
    let rows = [("rook", "Rook", -17), ("finch", "Finch", 9)]
        .into_iter()
        .map(|(handle, name, tick)| {
            BTreeMap::from([
                ("handle".into(), Datum::Value(Value::Str(handle.into()))),
                ("name".into(), Datum::Value(Value::Str(name.into()))),
                ("tick".into(), Datum::Value(Value::I64(tick))),
            ])
        })
        .collect();
    params.insert_rows("rows".into(), rows).unwrap();
    params
}

fn load() -> World {
    let mut world = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    let parsed = kala_cypher::parse_statement(MERGE);
    assert!(
        parsed.is_ok(),
        "per-row write tick should parse: {parsed:?}"
    );
    let kala_cypher::ast::Statement::Write(stmt) = parsed.unwrap() else {
        panic!("expected a write");
    };
    kala_cypher::write::execute(&mut world, &stmt, &inputs(), RunOptions::default()).unwrap();
    world
}

#[test]
fn map_rows_merge_supplied_handles_and_names() {
    let w = load();
    for (handle, name) in [("rook", "Rook"), ("finch", "Finch")] {
        let id = w
            .entity_by_handle(handle)
            .expect("MERGE should bind row.handle");
        assert_eq!(
            w.as_of(id, "name", Tick(i64::MAX)).unwrap(),
            [Value::Str(name.into())]
        );
    }
}

#[test]
fn map_rows_land_at_two_distinct_ticks() {
    let w = load();
    for (handle, tick) in [("rook", -17), ("finch", 9)] {
        let id = w.entity_by_handle(handle).unwrap();
        let history = w.history(id, "name").unwrap();
        assert_eq!(
            history.iter().map(|fact| fact.tick).collect::<Vec<_>>(),
            [Tick(tick)]
        );
        assert!(w.as_of(id, "name", Tick(tick - 1)).unwrap().is_empty());
    }
}

fn write(
    world: &mut World,
    text: &str,
    params: &QueryParams,
) -> Result<kala_core::write::WriteReport, kala_cypher::write::WriteError> {
    let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(text).unwrap()
    else {
        panic!("expected write");
    };
    kala_cypher::write::execute(world, &stmt, params, RunOptions::default())
}

#[test]
fn map_rows_and_primitive_limits_accept_the_last_row_and_refuse_the_next() {
    use kala_cypher::write::{MAX_WRITE_PRIMITIVES, MAX_WRITE_ROWS};
    // One bare CREATE costs one primitive. A CREATE carrying a value costs two.
    for (statement, accepted, reason) in [
        (
            "UNWIND $rows AS row CREATE (:NPC) AT TICK row.tick",
            MAX_WRITE_ROWS,
            "row",
        ),
        (
            "UNWIND $rows AS row CREATE (:NPC {age:17}) AT TICK row.tick",
            MAX_WRITE_PRIMITIVES / 2,
            "primitive",
        ),
    ] {
        for count in [accepted, accepted + 1] {
            let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
            let original = w.latest_tx();
            let mut p = QueryParams::new();
            let row = BTreeMap::from([("tick".to_owned(), Value::I64(0).into())]);
            let outcome = p
                .insert_rows("rows".into(), vec![row; count])
                .map_err(|e| e.to_string())
                .and_then(|()| write(&mut w, statement, &p).map_err(|e| e.to_string()));
            if count == accepted {
                outcome.unwrap();
                assert!(w.latest_tx() > original);
            } else {
                let message = outcome.unwrap_err();
                assert!(message.contains(reason), "{message}");
                assert_eq!(w.latest_tx(), original);
            }
        }
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
    #[test]
    fn map_rows_match_a_tick_model_and_leave_doctor_clean(ticks in proptest::collection::vec(-12i64..=12, 1..20)) {
        let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
        let rows = ticks.iter().enumerate().map(|(i,t)| BTreeMap::from([
            ("handle".to_owned(), Value::Str(format!("bird_{i}")).into()),
            ("name".to_owned(), Value::Str(format!("Bird {i}")).into()),
            ("tick".to_owned(), Value::I64(*t).into()),
        ])).collect();
        let mut p = QueryParams::new();
        p.insert_rows("rows".into(), rows).unwrap();
        write(&mut w, MERGE, &p).unwrap();
        for (i,t) in ticks.iter().enumerate() {
            let e = w.entity_by_handle(&format!("bird_{i}")).unwrap();
            proptest::prop_assert!(w.as_of(e, "name", Tick(*t - 1)).unwrap().is_empty());
            proptest::prop_assert_eq!(w.as_of(e, "name", Tick(*t)).unwrap(), [Value::Str(format!("Bird {i}"))]);
        }
        let report = kala_core::doctor(&w, &Default::default());
        proptest::prop_assert!(report.errors().is_empty() && report.warnings().is_empty(), "doctor: {:?}", report);
    }
}

#[test]
fn same_tick_replacement_fails_on_its_row_and_appends_nothing() {
    let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    let before = w.latest_tx();
    let mut p = QueryParams::new();
    let rows = ["first", "second"]
        .map(|value| {
            BTreeMap::from([
                ("handle".into(), Value::Str("rook".into()).into()),
                ("name".into(), Value::Str(value.into()).into()),
                ("tick".into(), Value::I64(17).into()),
            ])
        })
        .into_iter()
        .collect();
    p.insert_rows("rows".into(), rows).unwrap();
    let error = write(&mut w, MERGE, &p).unwrap_err().to_string();
    assert!(error.contains("row 1") && error.contains("tick"), "{error}");
    assert_eq!(w.latest_tx(), before);
}

#[test]
fn property_names_follow_variable_scope_through_with() {
    let pass = kala_cypher::parse("UNWIND $rows AS row WITH * RETURN row.title").unwrap();
    assert!(!pass.attributes().contains(&"title"));
    let shadowed =
        kala_cypher::parse("UNWIND $rows AS row MATCH (n) WITH n AS row RETURN row.title").unwrap();
    assert!(shadowed.attributes().contains(&"title"));
}

#[test]
fn scalar_trailing_tick_controls_match_and_null_uses_transaction_default() {
    let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    for (tick, score) in [(0, 1), (50, 2)] {
        let form = if tick == 0 {
            "[[:create :NPC :rook] [:set :rook :score 1 :at 0]]".into()
        } else {
            format!("[:set :rook :score {score} :at {tick}]")
        };
        kala_datalog::apply::execute(&mut w, &kala_datalog::parse_write(&form).unwrap(), None)
            .unwrap();
    }
    let q = "MATCH (n:NPC {score:1}) SET n.mark=true AT TICK $t";
    let mut p = QueryParams::new();
    p.insert("t".into(), Value::I64(0).into());
    assert_eq!(write(&mut w, q, &p).unwrap().changed, 1);

    let kala_cypher::ast::Statement::Write(stmt) = kala_cypher::parse_statement(
        "UNWIND $rows AS row CREATE (:NPC {kala_handle:row.handle,mark:true}) AT TICK row.tick",
    )
    .unwrap() else {
        panic!("expected write");
    };
    let mut rows = QueryParams::new();
    rows.insert_rows(
        "rows".into(),
        vec![BTreeMap::from([
            ("handle".into(), Value::Str("wren".into()).into()),
            ("tick".into(), Datum::Null),
        ])],
    )
    .unwrap();
    let input = kala_cypher::write::prepare(&w, &stmt, &rows, RunOptions::default()).unwrap();
    let mut tx = w.begin();
    tx.set_default_tick(Tick(-33));
    kala_cypher::write::stage(&mut tx, &stmt, input, &rows).unwrap();
    tx.commit().unwrap();
    let wren = w.entity_by_handle("wren").unwrap();
    assert_eq!(w.history(wren, "mark").unwrap()[0].tick, Tick(-33));
}
