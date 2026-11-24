//! Slice 15a review fixes at the shared applier (no CLI in between).
use kala_core::{Schema, Tick, Value, World};
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
