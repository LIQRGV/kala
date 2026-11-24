//! Slice 15a review fixes at the shared applier (no CLI in between).
use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
use kala_lang::write::{Failure, WritePolicy, execute, execute_with, parse_write, stage};

/// A world carrying the system marker, as `kala_system::ensure` creates it.
fn system_world() -> World {
    World::in_memory(
        Schema::new()
            .node_type(kala_core::SYSTEM_NODE_TYPE)
            .node_type("ACCOUNT"),
    )
    .unwrap()
}

const SYSTEM_FORMS: [&str; 3] = [
    "[:assert :root :plugin \"trust\" :at 1]",
    "[:set :root :plugin \"trust\" :at 1]",
    "[:create :ACCOUNT :mallory :if-missing]",
];

#[test]
fn the_shared_applier_refuses_a_system_world_by_default() {
    for text in SYSTEM_FORMS {
        let mut w = system_world();
        let before = w.latest_tx();
        let form = parse_write(text).unwrap();
        let error = execute(&mut w, &form, None).unwrap_err();
        assert!(
            matches!(error.cause, Failure::SystemWorld),
            "{text}: {error:?}"
        );
        assert_eq!(error.operation, None, "{text}");
        assert_eq!(
            error.to_string(),
            "the system database is written only through account management",
            "{text}"
        );
        assert_eq!(w.latest_tx(), before, "{text}");
        assert!(w.entity_by_handle("mallory").is_none(), "{text}");
    }
}

#[test]
fn staging_into_a_caller_owned_transaction_on_a_system_world_is_refused_and_poisons_it() {
    let mut w = system_world();
    let form = parse_write("[:create :ACCOUNT :mallory :if-missing]").unwrap();
    let mut tx = w.begin();
    let error = stage(&mut tx, &form, None).unwrap_err();
    assert!(matches!(error.cause, Failure::SystemWorld), "{error:?}");
    assert!(matches!(
        tx.commit(),
        Err(kala_core::Error::TransactionAborted)
    ));
    assert!(w.entity_by_handle("mallory").is_none());
}

#[test]
fn an_explicit_policy_allows_a_system_world_for_restores() {
    let mut w = system_world();
    let form = parse_write("[:create :ACCOUNT :root]").unwrap();
    let report = execute_with(&mut w, &form, None, WritePolicy { allow_system: true }).unwrap();
    assert_eq!(report.tx, Some(w.latest_tx()));
    assert!(w.entity_by_handle("root").is_some());
    // The default policy is the refusing one.
    assert!(!WritePolicy::default().allow_system);
}

#[test]
fn an_ordinary_world_is_unaffected_by_the_system_refusal() {
    let mut w = World::in_memory(Schema::new()).unwrap();
    assert!(!w.is_system());
    let form = parse_write(
        "[:at 5 [:create :CHARACTER :kael :if-missing] [:assert :kael :age 18] [:set :kael :name \"Kael\"]]",
    )
    .unwrap();
    let report = execute(&mut w, &form, None).unwrap();
    assert_eq!(report.changed, 2);
    let kael = w.entity_by_handle("kael").unwrap();
    assert_eq!(w.as_of(kael, "age", Tick(5)).unwrap(), [Value::I64(18)]);
}

fn age_world() -> (World, kala_core::EntityId) {
    let mut w = World::in_memory(
        Schema::new()
            .node_type("CHARACTER")
            .attribute(AttrDef::new(
                "age",
                ValueType::I64,
                Kind::State,
                Cardinality::One,
            ))
            .attribute(AttrDef::new(
                "v",
                ValueType::I64,
                Kind::State,
                Cardinality::Many,
            )),
    )
    .unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    (w, greg)
}

/// The lookup that finds the conflicting entry shares the form's read budget.
/// When the entry sits in a transaction too large to search within it, the
/// same-tick conflict is still the error; only the entry detail is missing.
#[test]
fn a_same_tick_conflict_survives_an_interrupted_entry_lookup() {
    let (mut w, greg) = age_world();
    let mut tx = w.begin();
    // First entry of a huge transaction: the lookup walks it from the end.
    tx.assert(greg, "age", Value::I64(18), Some(Tick(601)))
        .unwrap();
    for i in 0..70_000 {
        tx.assert(greg, "v", Value::I64(i), Some(Tick(1))).unwrap();
    }
    tx.commit_quiet().unwrap();
    let form = parse_write("[:set :greg :age 19 :at 601]").unwrap();
    let error = execute(&mut w, &form, None).unwrap_err();
    assert!(
        matches!(&error.cause, Failure::SameTick(conflict) if conflict.occupant.is_none()),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "op 1 (:set): same-tick conflict: the opposite op is already stored for greg \"age\" at tick 601"
    );
}

/// A conflict with a form staged earlier into the same caller-owned
/// transaction has no op index in this form and no entry ID.
#[test]
fn a_same_tick_conflict_with_an_earlier_form_of_the_transaction_says_so() {
    let (mut w, _) = age_world();
    let first = parse_write("[:assert :greg :age 18 :at 601]").unwrap();
    let second = parse_write("[[:assert :greg :v 1 :at 1] [:set :greg :age 19 :at 601]]").unwrap();
    let mut tx = w.begin();
    stage(&mut tx, &first, None).unwrap();
    let error = stage(&mut tx, &second, None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "op 2 (:set): same-tick conflict: the opposite op is already stored for greg \"age\" at tick 601; 18 was asserted at that tick by an earlier operation of this transaction: change that operation instead"
    );
}
