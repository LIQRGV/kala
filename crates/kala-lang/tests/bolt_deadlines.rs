//! A server deadline is cancellation, not the write language's resource limit.
use kala_core::{Error, Schema, Tick, Value, WorkInterruptReason, World};
use kala_lang::write::{Failure, execute, parse_write, stage};

#[test]
fn expired_set_read_preserves_typed_cancellation_instead_of_a_form_limit() {
    let mut world = World::in_memory(Schema::new()).unwrap();
    execute(
        &mut world,
        &parse_write("[[:create :CHARACTER :greg] [:assert :greg :alias \"held\" :at 0]]").unwrap(),
        None,
    )
    .unwrap();
    let before = world.latest_tx();
    let greg = world.entity_by_handle("greg").unwrap();
    let mut tx = world.begin();
    tx.set_deadline(std::time::Instant::now());
    // Clearing the set reaches its held-value read without an infer/assert
    // operation first, so this pins the read-meter error translation itself.
    let failure = stage(
        &mut tx,
        &parse_write("[:set :greg :alias [] :at 1]").unwrap(),
        None,
    )
    .unwrap_err();
    assert_eq!(failure.operation, Some((1, "set")));
    assert!(
        matches!(
            failure.cause,
            Failure::Store(Error::WorkInterrupted {
                reason: WorkInterruptReason::Stopped
            })
        ),
        "deadline cancellation must retain its typed cause, got {failure:?}"
    );
    drop(tx);
    assert_eq!(world.latest_tx(), before);
    assert_eq!(
        world.as_of(greg, "alias", Tick(1)).unwrap(),
        vec![Value::Str("held".into())]
    );
}
