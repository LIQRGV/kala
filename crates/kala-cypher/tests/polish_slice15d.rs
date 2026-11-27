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

// ---- C: DELETE / DETACH DELETE edge cases ----

fn datalog(world: &mut World, form: &str) {
    let form = kala_datalog::parse_write(form).unwrap();
    kala_datalog::apply::execute(world, &form, None).unwrap();
}

#[test]
fn a_node_with_deprecated_values_can_still_be_deleted() {
    let mut world = varrow();
    datalog(
        &mut world,
        "[:at 800 [:create :PLACE :lonely] [:assert :lonely :ruled_by :tomas]]",
    );
    datalog(&mut world, "[:deprecate :ruled_by]");
    datalog(&mut world, "[:deprecate :alias]");
    // A deprecated relationship doesn't block a plain DELETE.
    let report = ok(
        &mut world,
        "MATCH (p {kala_handle:'lonely'}) DELETE p AT TICK 900",
    );
    assert_eq!(report.changed, 0);
    // Osric holds deprecated aliases: they stay, everything else ends.
    ok(
        &mut world,
        "MATCH (o {kala_handle:'osric'}) DETACH DELETE o AT TICK 900",
    );
    let osric = world.entity_by_handle("osric").unwrap();
    assert!(world.as_of(osric, "name", Tick(900)).unwrap().is_empty());
    assert_eq!(world.as_of(osric, "alias", Tick(900)).unwrap().len(), 2);
}

#[test]
fn a_migrated_attribute_does_not_make_its_entities_undeletable() {
    let mut world = varrow();
    datalog(&mut world, "[:set :greg :hp 5 :at 700]");
    datalog(&mut world, "[:migrate :hp :f64]");
    datalog(&mut world, "[:migrate :located_in :ref :many]");
    ok(
        &mut world,
        "MATCH (g {kala_handle:'greg'}) DETACH DELETE g AT TICK 900",
    );
    let greg = world.entity_by_handle("greg").unwrap();
    assert!(world.as_of(greg, "hp", Tick(900)).unwrap().is_empty());
    assert!(
        world
            .as_of(greg, "located_in", Tick(900))
            .unwrap()
            .is_empty()
    );
    let report = ok(
        &mut world,
        "MATCH (g {kala_handle:'greg'}) DELETE g AT TICK 901",
    );
    assert_eq!(report.changed, 0);
}

#[test]
fn events_stay_and_never_block_a_plain_delete() {
    let mut world = varrow();
    let edwin = world.entity_by_handle("edwin").unwrap();
    let born = world.as_of(edwin, "born_in", Tick(900)).unwrap();
    assert!(!born.is_empty());
    ok(
        &mut world,
        "MATCH (e {kala_handle:'edwin'}) DETACH DELETE e AT TICK 900",
    );
    let report = ok(
        &mut world,
        "MATCH (e {kala_handle:'edwin'}) DELETE e AT TICK 901",
    );
    assert_eq!(report.changed, 0);
    assert_eq!(world.as_of(edwin, "born_in", Tick(901)).unwrap(), born);
    assert!(world.as_of(edwin, "bought", Tick(901)).unwrap().len() == 1);
}

/// `count` characters, each with a name, eight earlier hp values (none held
/// from tick 8), a home and an ally (the previous character).
fn crowd(count: usize) -> World {
    use kala_core::{AttrDef, Cardinality, Kind, Schema, ValueType};
    let attr = AttrDef::new;
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(attr("hp", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional());
    let mut world = World::in_memory(schema).unwrap();
    let mut tx = world.begin();
    let home = tx.create_entity("PLACE", Some("home")).unwrap();
    let mut previous = None;
    for i in 0..count {
        let id = tx.create_entity("CHARACTER", None).unwrap();
        tx.assert(id, "name", Value::Str(format!("c{i}")), Some(Tick(0)))
            .unwrap();
        for hp in 0..8 {
            tx.assert(id, "hp", Value::I64(hp), Some(Tick(hp))).unwrap();
            tx.retract(id, "hp", Value::I64(hp), Some(Tick(hp + 1)))
                .unwrap();
        }
        tx.assert(id, "located_in", Value::Ref(home), Some(Tick(0)))
            .unwrap();
        if let Some(previous) = previous {
            tx.assert(id, "ally_of", Value::Ref(previous), Some(Tick(0)))
                .unwrap();
        }
        previous = Some(id);
    }
    tx.commit().unwrap();
    world
}

#[test]
fn detach_delete_of_every_character_stays_within_the_statement_limits() {
    let mut world = crowd(600);
    let report = ok(&mut world, "MATCH (c:CHARACTER) DETACH DELETE c AT TICK 50");
    // name and home for each, and each alliance once.
    assert_eq!(report.changed, 600 * 2 + 599);
    let report = ok(&mut world, "MATCH (c:CHARACTER) DELETE c AT TICK 51");
    assert_eq!(report.changed, 0);
}

#[test]
fn deleting_nodes_after_thousands_of_staged_entries_is_not_quadratic() {
    use kala_cypher::write::{prepare, stage};
    let mut world = crowd(20);
    let mut params = QueryParams::new();
    params.insert(
        "xs".into(),
        Datum::List((0..5_000).map(|i| Datum::Value(Value::I64(i))).collect()),
    );
    let mut tx = world.begin();
    let mut changed = 0;
    for text in [
        "UNWIND $xs AS i CREATE (:PLACE {size: i}) AT TICK 40",
        "MATCH (c:CHARACTER) DETACH DELETE c AT TICK 50",
    ] {
        let statement = statement(text);
        let input = prepare(tx.base_world(), &statement, &params, RunOptions::default())
            .unwrap_or_else(|e| panic!("{text}: {e}"));
        let staged =
            stage(&mut tx, &statement, input, &params).unwrap_or_else(|e| panic!("{text}: {e}"));
        changed = staged.staged.ops.iter().map(|op| op.changed).sum();
    }
    assert_eq!(changed, 20 * 2 + 19);
    assert_eq!(tx.staged_entry_count(), 5_000 + 20 * 2 + 19);
    tx.commit().unwrap();
}

// ---- D: the read budget of a statement scales with what it may stage ----

#[test]
fn set_over_nine_thousand_entities_fits_the_statement_limits() {
    let mut world = crowd(9_000);
    let report = ok(&mut world, "MATCH (c:CHARACTER) SET c.hp = 100 AT TICK 50");
    assert_eq!(report.changed, 9_000);
    let hp = |world: &World| {
        let id = world.entities_of_type("CHARACTER").unwrap().last().unwrap();
        world.as_of(id, "hp", Tick(60)).unwrap()
    };
    assert_eq!(hp(&world), [Value::I64(100)]);
    // Replacing a held value is two primitives a row: 18,000 in all.
    let error = run(&mut world, "MATCH (c:CHARACTER) SET c.hp = 1 AT TICK 60").unwrap_err();
    assert_eq!(
        error.to_string(),
        "op 5001 (:set): statement exceeds the 10000-primitive write limit"
    );
    assert_eq!(hp(&world), [Value::I64(100)]);
}

// ---- E: removing what was never declared, and writes on a NULL variable ----

#[test]
fn removing_an_undeclared_property_or_label_changes_nothing() {
    let mut world = crowd(2);
    let attributes = world.attributes().count();
    for text in [
        "MATCH (c:CHARACTER) REMOVE c.nope AT TICK 50",
        "MATCH (c:CHARACTER) SET c.nope = NULL AT TICK 50",
        "MATCH (c:CHARACTER) SET c += {nope: NULL, other: NULL} AT TICK 50",
        "MATCH (c:CHARACTER) REMOVE c:Guide AT TICK 50",
    ] {
        let report = ok(&mut world, text);
        assert_eq!(report.changed, 0, "{text}");
    }
    assert_eq!(world.attributes().count(), attributes);
    // The Datalog form keeps its own rule.
    let form = kala_datalog::parse_write("[:set :greg :nope []]").unwrap();
    let error = kala_datalog::apply::execute(&mut varrow(), &form, None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "op 1 (:set): cannot set unknown attribute :nope to []; declare it first"
    );
}

#[test]
fn a_null_first_value_does_not_stop_a_bulk_merge_from_declaring_the_property() {
    use kala_core::Schema;
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let list = |values: Vec<Datum>| Datum::List(values);
    let text = |s: &str| Datum::Value(Value::Str(s.into()));
    let mut params = QueryParams::new();
    params.insert(
        "indices".into(),
        list(vec![
            Datum::Value(Value::I64(0)),
            Datum::Value(Value::I64(1)),
        ]),
    );
    params.insert("handles".into(), list(vec![text("a"), text("b")]));
    params.insert("names".into(), list(vec![Datum::Null, text("Bee")]));
    execute(
        &mut world,
        &statement(
            "UNWIND $indices AS i MERGE (n:CHARACTER {kala_handle:$handles[i]}) \
             ON CREATE SET n.name = $names[i]",
        ),
        &params,
        RunOptions::default(),
    )
    .unwrap();
    let name = |handle| {
        let id = world.entity_by_handle(handle).unwrap();
        world.as_of(id, "name", world.latest_tick()).unwrap()
    };
    assert!(name("a").is_empty());
    assert_eq!(name("b"), [Value::Str("Bee".into())]);
}

#[test]
fn set_and_remove_on_a_null_variable_skip_the_row() {
    let mut world = crowd(2);
    for text in [
        "OPTIONAL MATCH (x:PLACE {name:'nowhere'}) SET x.hp = 1 AT TICK 50",
        "OPTIONAL MATCH (x:PLACE {name:'nowhere'}) SET x += {hp: 1} AT TICK 50",
        "OPTIONAL MATCH (x:PLACE {name:'nowhere'}) REMOVE x.hp AT TICK 50",
        "OPTIONAL MATCH (x:PLACE {name:'nowhere'}) SET x:Ghost AT TICK 50",
        "OPTIONAL MATCH (x:PLACE {name:'nowhere'}) REMOVE x:Ghost AT TICK 50",
    ] {
        let report = ok(&mut world, text);
        assert_eq!(report.changed, 0, "{text}");
    }
    assert!(world.attribute("label").is_none());
}

#[test]
fn removing_an_undeclared_property_reads_the_absence_of_its_name() {
    use kala_cypher::write::{prepare, stage};
    for (declared, conflicts) in [("unrelated", false), ("nope", true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.kala");
        let mut mine = kala_fixture::load_fixture_to(VARROW, &path).unwrap();
        let mut theirs = World::open(&path).unwrap();
        let statement = statement(
            "MATCH (g {kala_handle:'greg'}) REMOVE g.nope SET g.name = 'Gregor' AT TICK 900",
        );
        let params = Params::new();
        let mut tx = mine.begin();
        let input = prepare(tx.base_world(), &statement, &params, RunOptions::default()).unwrap();
        stage(&mut tx, &statement, input, &params).unwrap();
        datalog(
            &mut theirs,
            &format!("[:assert :mara :{declared} 1 :at 800]"),
        );
        let result = tx.commit();
        assert_eq!(
            matches!(result, Err(kala_core::Error::Conflict(_))),
            conflicts,
            "{declared}: {result:?}"
        );
    }
}
