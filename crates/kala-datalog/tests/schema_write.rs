//! Schema write forms share the ordinary transaction grammar and its limits.

use kala_core::{
    AttrDef, Cardinality, Kind, MigrationOptions, Schema, Tick, Value, ValueType, World,
};
use kala_datalog::{ErrorKind, FormKind, form_kind, parse, parse_write, parse_writes};

const FORMS: &[&str] = &[
    "[:attribute :title :string]",
    "[:attribute :score :i64 :many :event]",
    "[:attribute :height :f64 :one :state]",
    "[:attribute :alive :bool]",
    "[:attribute :biography :string :unindexed]",
    "[:attribute :parent_of :ref :many :directed :inverse :child_of :reverse :one]",
    "[:attribute :ally_of :ref :bidirectional :many]",
    "[:node-type :FACTION]",
    "[:rename :titel :title]",
    "[:rename :age@old :age_v1]",
    "[:deprecate :name]",
    "[:cardinality :located_in :many]",
    "[:cardinality :ally_of :one]",
    "[:migrate :age :string]",
    "[:migrate :age :i64 :skip-unconvertible]",
    "[:migrate :present :bidirectional]",
    "[:migrate :born_in :state]",
    "[:migrate :description :unindexed]",
    "[:migrate :parent_of :reverse :many :inverse :child_of]",
    "[:world-setting :schema :strict]",
    "[:world-setting :schema :infer]",
];

#[test]
fn every_schema_form_dispatches_and_parses_standalone() {
    for text in FORMS {
        let kind = form_kind(text);
        assert_eq!(
            kind.unwrap_or_else(|e| panic!("{text}: {e}")),
            FormKind::Write
        );
        let txn = parse_write(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(txn.ops.len(), 1, "{text}");
        assert_eq!(txn.span.start, 0, "{text}");
        assert_eq!(txn.span.end, text.len(), "{text}");
    }
}

#[test]
fn schema_forms_mix_with_facts_in_one_ordered_transaction() {
    let text = "[:at -300 [:world-setting :schema :strict] [:attribute :age :i64] [:assert :edwin :age 42] [:rename :age :years] [:deprecate :years] [:node-type :FACTION] [:create :FACTION :red_hand]]";
    let txn = parse_write(text).unwrap();
    assert_eq!(txn.ops.len(), 7);
    assert_eq!(txn.at, Some(kala_core::Tick(-300)));
    let text = format!("[{}]", FORMS.join(" "));
    assert_eq!(parse_write(&text).unwrap().ops.len(), FORMS.len());
    assert_eq!(parse_writes(&FORMS.join("\n")).unwrap().len(), FORMS.len());
}

#[test]
fn migration_names_lex_in_queries_and_writes() {
    for name in ["age@old", "age@old2", "age@old10"] {
        parse(&format!("[:find ?v :where [:edwin :{name} ?v]]")).unwrap();
        parse_write(&format!("[:assert :edwin :{name} 42]")).unwrap();
        parse_write(&format!("[:deprecate :{name}]")).unwrap();
    }
    assert!(parse("[:find ?v :where [:edwin :@old ?v]]").is_err());
}

#[test]
fn malformed_schema_forms_have_located_errors() {
    for text in [
        "[:attribute :age]",
        "[:attribute :age :integer]",
        "[:attribute :age :i64 :bogus]",
        "[:attribute :parent :ref :inverse]",
        "[:attribute :parent :ref :reverse :sometimes]",
        "[:node-type]",
        "[:node-type :FACTION :extra]",
        "[:rename :old]",
        "[:rename :old :new :extra]",
        "[:deprecate]",
        "[:deprecate :age :extra]",
        "[:cardinality :age :sometimes]",
        "[:migrate]",
        "[:migrate :age :integer]",
        "[:world-setting :schema]",
        "[:world-setting :schema :strict :extra]",
    ] {
        let error = parse_write(text).expect_err(text);
        assert!(error.span.is_some(), "{text}: {error}");
        assert!(error.to_string().contains("line 1"), "{text}: {error}");
    }
}

#[test]
fn schema_forms_count_toward_operation_and_form_limits() {
    let op = "[:node-type :FACTION]";
    assert_eq!(
        parse_write(&format!("[{}]", op.repeat(256)))
            .unwrap()
            .ops
            .len(),
        256
    );
    assert_eq!(
        parse_write(&format!("[{}]", op.repeat(257)))
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
    assert_eq!(
        parse_writes(&op.repeat(257)).unwrap_err().kind,
        ErrorKind::Limit
    );
}

fn parent_world() -> World {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "parent_of",
        ValueType::Ref,
        Kind::State,
        Cardinality::One,
    )))
    .unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    world
        .assert(a, "parent_of", Value::Ref(b), Some(Tick(0)))
        .unwrap();
    world
}

#[test]
fn inverse_added_after_query_canon_is_not_resolved() {
    let mut world = parent_world();
    let before = world.latest_tx();
    let mut tx = world.begin();
    tx.declare_attribute(
        AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::One)
            .inverse("child_of"),
    )
    .unwrap();
    tx.commit().unwrap();
    let query = format!(
        "[:find ?p :at 0 :canon {} :where [:mara :child_of ?p]]",
        before.0
    );
    let result = kala_datalog::prepare(&world, &query);
    assert!(
        result.is_err(),
        "the inverse does not exist at the query canon: {result:?}"
    );
    kala_datalog::prepare(&world, "[:find ?p :at 0 :where [:mara :child_of ?p]]").unwrap();
}

#[test]
fn migrated_inverse_resolves_latest_attribute_id_at_every_query_canon() {
    let mut world = parent_world();
    let mut tx = world.begin();
    tx.declare_attribute(
        AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::One)
            .inverse("child_of"),
    )
    .unwrap();
    let before = tx.commit().unwrap().tx;
    let mut tx = world.begin();
    tx.migrate_attribute(
        "parent_of",
        MigrationOptions {
            cardinality: Some(Cardinality::Many),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let query = format!(
        "[:find ?p :at 0 :canon {} :where [:mara :child_of ?p]]",
        before.0
    );
    let result = kala_datalog::prepare(&world, &query);
    assert!(
        result.is_err(),
        "the inverse now belongs to a replacement attribute undefined at this canon: {result:?}"
    );
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("not defined") && error.contains(&format!("tx#{}", before.0)),
        "{error}"
    );
    kala_datalog::prepare(&world, "[:find ?p :at 0 :where [:mara :child_of ?p]]").unwrap();
    kala_datalog::prepare(
        &world,
        &format!(
            "[:find ?p :at 0 :canon {} :where [:edwin :parent_of@old ?p]]",
            before.0
        ),
    )
    .unwrap();
}
