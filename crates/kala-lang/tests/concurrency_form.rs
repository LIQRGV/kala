//! The write entry point carries a form's concurrency mode, so a server
//! adapter (slice 15c) needs no language crate to honor
//! `[:concurrency :weak]`. What the modes do at commit is tested where two
//! handles can write one file (`kala-datalog/tests/concurrency_form.rs`).
use kala_core::{Schema, Tick, Value, World};
use kala_lang::write::{Concurrency, WritePolicy, commit, execute_in, parse_write, stage};

fn empty() -> World {
    World::in_memory(Schema::new()).unwrap()
}

#[test]
fn a_staged_form_sets_its_own_mode_on_the_callers_transaction() {
    for (text, start, want) in [
        (
            "[[:concurrency :weak] [:create :CHARACTER :kael]]",
            Concurrency::Optimistic,
            Concurrency::Weak,
        ),
        (
            "[[:concurrency :optimistic] [:create :CHARACTER :kael]]",
            Concurrency::Weak,
            Concurrency::Optimistic,
        ),
        // A form that doesn't say keeps what the caller chose.
        (
            "[:create :CHARACTER :kael]",
            Concurrency::Weak,
            Concurrency::Weak,
        ),
        (
            "[:create :CHARACTER :kael]",
            Concurrency::Optimistic,
            Concurrency::Optimistic,
        ),
    ] {
        let mut w = empty();
        let form = parse_write(text).unwrap();
        let mut tx = w.begin();
        tx.set_concurrency(start);
        let staged = stage(&mut tx, &form, None).unwrap();
        assert_eq!(tx.concurrency(), want, "{text}");
        let report = commit(tx, staged).unwrap();
        assert_eq!(report.ops.len(), 1, "{text}: the mode is not an op");
        assert!(w.entity_by_handle("kael").is_some(), "{text}");
    }
}

#[test]
fn execute_in_runs_a_form_in_the_callers_mode() {
    let mut w = empty();
    let form = parse_write(
        "[:at 5 [:concurrency :weak] [:create :CHARACTER :kael] [:assert :kael :age 30]]",
    )
    .unwrap();
    assert_eq!(form.concurrency, Some(Concurrency::Weak));
    let report = execute_in(
        &mut w,
        &form,
        None,
        WritePolicy::default(),
        Concurrency::Optimistic,
    )
    .unwrap();
    assert_eq!((report.changed, report.ops.len()), (1, 2));
    let kael = w.entity_by_handle("kael").unwrap();
    assert_eq!(w.as_of(kael, "age", Tick(5)).unwrap(), [Value::I64(30)]);
}
