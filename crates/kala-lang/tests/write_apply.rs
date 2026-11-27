//! Shared writes stay usable below CLI and can stage several forms per native transaction.
use kala_core::{Error, Precision, Schema, Tick, Value, World};
use kala_lang::write::{execute, parse_write, stage};

fn empty() -> World {
    World::in_memory(Schema::new()).unwrap()
}

#[test]
fn shared_applier_creates_facts_edges_and_properties_without_cli() {
    let mut w = empty();
    let form = parse_write("[:at -300 [:create :CHARACTER :kael] [:create :PLACE :keep] [:assert :kael :name \"Kael\" :precision :year] [:assert :kael :located_in :keep] [:assert [:kael :located_in :keep] :disguised true]]").unwrap();
    let report = execute(&mut w, &form, None).unwrap();
    assert_eq!(report.tx, Some(w.latest_tx()));
    assert_eq!(report.changed, 3);
    assert_eq!(report.declared, 5);
    assert_eq!(report.entries.len(), 3);
    assert_eq!(report.ops.len(), 5);
    assert!(report.ops.iter().all(|op| op.tick == Tick(-300)));
    assert_eq!(report.ops[2].precision, Some(Precision::Year));
    assert_eq!(report.ops[2].entries, [report.entries[0]]);
    assert_eq!(report.ops[3].entries, [report.entries[1]]);
    assert_eq!(report.ops[4].entries, [report.entries[2]]);
    let kael = w.entity_by_handle("kael").unwrap();
    let keep = w.entity_by_handle("keep").unwrap();
    let edge = w.edge_id(kael, "located_in", keep).unwrap();
    assert_eq!(report.ops[0].entity, Some(kael));
    assert_eq!(report.edges, [edge]);
    assert_eq!(
        w.as_of(edge.into(), "disguised", Tick(-300)).unwrap(),
        [Value::Bool(true)]
    );
    assert!(report.warnings.introduced.is_empty());
}

#[test]
fn two_forms_stage_into_one_caller_owned_transaction() {
    let mut w = empty();
    let before = w.latest_tx();
    let first =
        parse_write("[:at -10 [:create :CHARACTER :kael] [:assert :kael :age 18]]").unwrap();
    let second = parse_write("[:assert :kael :name \"Kael\" :at -5]").unwrap();
    let mut tx = w.begin();
    let first_report = stage(&mut tx, &first, None).unwrap();
    let second_report = stage(&mut tx, &second, None).unwrap();
    let committed = tx.commit().unwrap();
    assert_eq!(committed.tx.0, before.0 + 1);
    assert_eq!(committed.changed, 2);
    let first_report = first_report.complete(&committed);
    let second_report = second_report.complete(&committed);
    assert_eq!(first_report.tx, second_report.tx);
    assert_eq!(first_report.ops[1].entries, [committed.entries[0]]);
    assert_eq!(second_report.ops[0].entries, [committed.entries[1]]);
    let kael = w.entity_by_handle("kael").unwrap();
    assert_eq!(w.as_of(kael, "age", Tick(-5)).unwrap(), [Value::I64(18)]);
    assert_eq!(
        w.as_of(kael, "name", Tick(-5)).unwrap(),
        [Value::Str("Kael".into())]
    );
}

#[test]
fn language_resolution_failure_poisoning_prevents_partial_commit() {
    let mut w = empty();
    let before = w.latest_tx();
    let good = parse_write("[:create :CHARACTER :kael]").unwrap();
    let bad = parse_write("[:assert :missing :age 1]").unwrap();
    let mut tx = w.begin();
    stage(&mut tx, &good, None).unwrap();
    let error = stage(&mut tx, &bad, None).unwrap_err();
    assert_eq!(error.operation, Some((1, "assert")));
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert!(w.entity_by_handle("kael").is_none());
    assert_eq!(w.latest_tx(), before);
}

#[test]
fn form_tick_defaults_do_not_leak_to_the_next_staged_form() {
    let mut w = empty();
    execute(
        &mut w,
        &parse_write("[[:create :CHARACTER :kael] [:assert :kael :age 18 :at 5]]").unwrap(),
        None,
    )
    .unwrap();
    let mut tx = w.begin();
    tx.set_default_tick(Tick(7));
    let first = stage(
        &mut tx,
        &parse_write("[:at 20 [:assert :kael :name \"Kael\"]]").unwrap(),
        None,
    )
    .unwrap();
    let second = stage(
        &mut tx,
        &parse_write("[:assert :kael :title \"Knight\"]").unwrap(),
        None,
    )
    .unwrap();
    let commit = tx.commit().unwrap();
    assert_eq!(first.complete(&commit).ops[0].tick, Tick(20));
    assert_eq!(second.complete(&commit).ops[0].tick, Tick(7));
}

#[test]
fn entity_only_commit_reports_the_same_tx_for_each_staged_form() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let mut tx = w.begin();
    let first = stage(
        &mut tx,
        &parse_write("[:create :CHARACTER :kael]").unwrap(),
        None,
    )
    .unwrap();
    let second = stage(
        &mut tx,
        &parse_write("[:node-type :CHARACTER]").unwrap(),
        None,
    )
    .unwrap();
    let commit = tx.commit().unwrap();
    assert_eq!(first.complete(&commit).tx, Some(commit.tx));
    assert_eq!(second.complete(&commit).tx, Some(commit.tx));
}

#[test]
fn set_and_if_missing_see_prior_forms_in_the_same_transaction() {
    let mut w = empty();
    let mut tx = w.begin();
    let first = stage(
        &mut tx,
        &parse_write(
            "[:at -10 [:create :CHARACTER :kael :if-missing] [:assert :kael :name \"old\"]]",
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let second = stage(
        &mut tx,
        &parse_write("[:at -5 [:create :CHARACTER :kael :if-missing] [:set :kael :name \"new\"]]")
            .unwrap(),
        None,
    )
    .unwrap();
    let commit = tx.commit().unwrap();
    assert_eq!(commit.changed, 3);
    let first = first.complete(&commit);
    let second = second.complete(&commit);
    assert_eq!(first.tx, second.tx);
    assert_eq!(second.ops[1].entries, commit.entries[1..]);
    let kael = w.entity_by_handle("kael").unwrap();
    assert_eq!(
        w.as_of(kael, "name", Tick(-10)).unwrap(),
        [Value::Str("old".into())]
    );
    assert_eq!(
        w.as_of(kael, "name", Tick(-5)).unwrap(),
        [Value::Str("new".into())]
    );
}

#[test]
fn failed_set_forms_poison_a_caller_owned_transaction_and_rollback_schema_and_handles() {
    for bad in [
        "[:set :missing :age 1]",
        "[:set :kael :age \"not a number\"]",
        "[:set :kael :bought :kael]",
        "[:set :kael :missing []]",
    ] {
        let mut w = empty();
        execute(
            &mut w,
            &parse_write("[[:attribute :age :i64] [:attribute :bought :ref :event]]").unwrap(),
            None,
        )
        .unwrap();
        let before = w.latest_tx();
        let mut tx = w.begin();
        stage(
            &mut tx,
            &parse_write("[:create :CHARACTER :kael]").unwrap(),
            None,
        )
        .unwrap();
        assert!(
            stage(&mut tx, &parse_write(bad).unwrap(), None).is_err(),
            "{bad}"
        );
        assert!(
            matches!(tx.commit(), Err(Error::TransactionAborted)),
            "{bad}"
        );
        assert!(w.entity_by_handle("kael").is_none());
        assert_eq!(w.latest_tx(), before);
        assert!(w.attribute("missing").is_none());
    }
}

#[test]
fn set_reads_the_fact_revived_by_an_earlier_staged_correction() {
    let mut w = empty();
    let initial = execute(&mut w, &parse_write("[[:create :CHARACTER :kael] [:assert :kael :name \"old\" :at 0] [:retract :kael :name \"old\" :at 10] [:assert :kael :name \"middle\" :at 10]]").unwrap(), None).unwrap();
    let retraction = initial.ops[2].entries[0];
    let mut tx = w.begin();
    stage(
        &mut tx,
        &parse_write(&format!("[:correct {} :delete]", retraction.0)).unwrap(),
        None,
    )
    .unwrap();
    let set = stage(
        &mut tx,
        &parse_write("[:set :kael :name \"new\" :at 20]").unwrap(),
        None,
    )
    .unwrap();
    let commit = tx.commit().unwrap();
    let report = set.complete(&commit);
    assert_eq!(
        report.ops[0].changed, 3,
        "old and middle must both be retracted"
    );
    let kael = w.entity_by_handle("kael").unwrap();
    assert_eq!(
        w.as_of(kael, "name", Tick(20)).unwrap(),
        [Value::Str("new".into())]
    );
    assert_eq!(
        w.as_of(kael, "name", Tick(19)).unwrap(),
        [Value::Str("middle".into()), Value::Str("old".into())]
    );
    assert!(
        kala_core::doctor(&w, &kala_core::DoctorOptions::default())
            .errors()
            .is_empty()
    );
}

#[test]
fn set_read_limit_poisoning_rolls_back_earlier_staged_forms() {
    let mut w = empty();
    execute(
        &mut w,
        &parse_write("[[:create :CHARACTER :kael] [:attribute :alias :string :many]]").unwrap(),
        None,
    )
    .unwrap();
    let kael = w.entity_by_handle("kael").unwrap();
    let mut tx = w.begin();
    for i in 0..257 {
        tx.assert(kael, "alias", Value::Str(format!("a{i}")), Some(Tick(0)))
            .unwrap();
    }
    tx.commit().unwrap();
    let before = w.latest_tx();
    let mut tx = w.begin();
    stage(
        &mut tx,
        &parse_write("[:create :CHARACTER :uncommitted]").unwrap(),
        None,
    )
    .unwrap();
    let failure = stage(
        &mut tx,
        &parse_write("[:set :kael :alias [] :at 10]").unwrap(),
        None,
    )
    .unwrap_err();
    assert!(failure.to_string().contains("limit"), "{failure}");
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert!(w.entity_by_handle("uncommitted").is_none());
    assert_eq!(w.latest_tx(), before);
    assert_eq!(w.as_of(kael, "alias", Tick(10)).unwrap().len(), 257);
}

#[test]
fn descending_sets_share_a_value_byte_budget_and_poison_the_transaction() {
    use kala_core::{AttrDef, Cardinality, Kind, ValueType};
    let schema = Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::Many).unindexed());
    let mut w = World::in_memory(schema).unwrap();
    let e = w.create_entity("CHARACTER", Some("e")).unwrap();
    w.assert(
        e,
        "bio",
        Value::Str("x".repeat(9 * 1024 * 1024)),
        Some(Tick(0)),
    )
    .unwrap();
    let before = w.latest_tx();
    let mut tx = w.begin();
    let form = parse_write("[[:set :e :bio [] :at 2] [:set :e :bio [] :at 1]]").unwrap();
    let failure = stage(&mut tx, &form, None)
        .expect_err("re-reading a long held value must charge the cumulative form byte budget");
    assert!(failure.to_string().contains("budget"), "{failure}");
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert_eq!(w.latest_tx(), before);
    assert_eq!(w.history(e, "bio").unwrap().len(), 1);
}
