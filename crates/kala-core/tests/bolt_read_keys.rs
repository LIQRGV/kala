//! Coarse node-definition reads used by Bolt plans must detect phantoms
//! without turning every unrelated fact append into a conflict.
use kala_core::{
    AttrDef, Cardinality, Concurrency, ConflictKey, Error, Kind, Schema, Tick, Value, ValueType,
    World,
};

fn fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    World,
    kala_core::EntityId,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-keys.kala");
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "tag",
        ValueType::Str,
        Kind::State,
        Cardinality::Many,
    ));
    let mut world = World::create(&path, schema).unwrap();
    let entity = world.create_entity("CHARACTER", Some("greg")).unwrap();
    (dir, path, world, entity)
}

#[test]
fn entity_definition_read_conflicts_with_handle_and_new_type_phantoms() {
    for new_type in [false, true] {
        let (_dir, path, mut first, greg) = fixture();
        let mut second = World::open(&path).unwrap();
        let mut txn = first.begin();
        txn.note_entity_definitions();
        txn.assert(greg, "tag", Value::Str("pending".into()), Some(Tick(-1)))
            .unwrap();
        let mut other = second.begin();
        if new_type {
            other.declare_node_type("PLACE").unwrap();
        }
        other
            .create_entity(
                if new_type { "PLACE" } else { "CHARACTER" },
                Some("phantom"),
            )
            .unwrap();
        other.commit().unwrap();
        let before = std::fs::read(&path).unwrap();
        let outcome = txn.commit();
        assert!(
            matches!(outcome, Err(Error::Conflict(ref conflict)) if matches!(conflict.key, ConflictKey::AnyDefine)),
            "a node-definition read must reject a newer entity definition: {outcome:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn entity_definition_read_allows_unrelated_fact_commits() {
    let (_dir, path, mut first, greg) = fixture();
    let mut second = World::open(&path).unwrap();
    let mut txn = first.begin();
    txn.note_entity_definitions();
    txn.assert(greg, "tag", Value::Str("pending".into()), Some(Tick(-1)))
        .unwrap();
    second
        .assert(greg, "tag", Value::Str("other".into()), Some(Tick(0)))
        .unwrap();
    txn.commit().unwrap();
    let world = World::open_read_only(&path).unwrap();
    assert_eq!(
        world.as_of(greg, "tag", Tick(i64::MAX)).unwrap(),
        vec![Value::Str("other".into()), Value::Str("pending".into())]
    );
}

#[test]
fn weak_entity_definition_reads_do_not_conflict() {
    let (_dir, path, mut first, greg) = fixture();
    let mut second = World::open(&path).unwrap();
    let mut txn = first.begin();
    txn.set_concurrency(Concurrency::Weak);
    txn.note_entity_definitions();
    txn.assert(
        greg,
        "tag",
        Value::Str("pending".into()),
        Some(Tick(i64::MIN)),
    )
    .unwrap();
    second.create_entity("CHARACTER", Some("phantom")).unwrap();
    txn.commit().unwrap();
    let world = World::open_read_only(&path).unwrap();
    assert_eq!(
        world.as_of(greg, "tag", Tick(i64::MIN)).unwrap(),
        vec![Value::Str("pending".into())]
    );
}

#[test]
fn expired_native_deadline_refuses_commit_without_appending() {
    let (_dir, path, mut world, greg) = fixture();
    let before = std::fs::read(&path).unwrap();
    let mut tx = world.begin();
    tx.assert(
        greg,
        "tag",
        Value::Str("not committed".into()),
        Some(Tick(0)),
    )
    .unwrap();
    tx.set_deadline(std::time::Instant::now());
    let result = tx.commit();
    assert!(
        matches!(
            result,
            Err(Error::WorkInterrupted {
                reason: kala_core::WorkInterruptReason::Stopped
            })
        ),
        "expired transaction must refuse its append: {result:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn expired_native_deadline_refuses_another_stage() {
    let (_dir, path, mut world, greg) = fixture();
    let before = std::fs::read(&path).unwrap();
    let mut tx = world.begin();
    tx.set_deadline(std::time::Instant::now());
    let result = tx.assert(
        greg,
        "tag",
        Value::Str("not staged".into()),
        Some(Tick(i64::MIN)),
    );
    assert!(
        matches!(
            result,
            Err(Error::WorkInterrupted {
                reason: kala_core::WorkInterruptReason::Stopped
            })
        ),
        "expired transaction must refuse staging: {result:?}"
    );
    drop(tx);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn owned_deadline_survives_reborrowing_and_refuses_commit() {
    let (_dir, path, world, greg) = fixture();
    let before = std::fs::read(&path).unwrap();
    let mut owned = world.into_transaction();
    owned
        .with(|tx| {
            tx.assert(
                greg,
                "tag",
                Value::Str("not committed".into()),
                Some(Tick(-1)),
            )
        })
        .unwrap();
    owned.with(|tx| tx.set_deadline(std::time::Instant::now()));
    // A separate borrow must preserve the deadline set by the first message.
    owned.with(|_| ());
    let (_, result) = owned.commit();
    assert!(
        matches!(
            result,
            Err(Error::WorkInterrupted {
                reason: kala_core::WorkInterruptReason::Stopped
            })
        ),
        "owned transaction lost its deadline: {result:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn expired_deadline_survives_finalization_over_a_new_commit() {
    let (_dir, path, mut first, greg) = fixture();
    let mut other = World::open(&path).unwrap();
    let mut tx = first.begin();
    tx.assert(
        greg,
        "tag",
        Value::Str("not committed".into()),
        Some(Tick(0)),
    )
    .unwrap();
    tx.set_deadline(std::time::Instant::now());
    other
        .assert(
            greg,
            "tag",
            Value::Str("other".into()),
            Some(Tick(i64::MAX)),
        )
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let result = tx.commit();
    assert!(
        matches!(
            result,
            Err(Error::WorkInterrupted {
                reason: kala_core::WorkInterruptReason::Stopped
            })
        ),
        "finalization must not reset an expired deadline: {result:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn expired_work_meter_stops_reads_and_remains_expired_after_reset() {
    let (_dir, _path, mut world, greg) = fixture();
    world
        .assert(greg, "tag", Value::Str("visible".into()), Some(Tick(0)))
        .unwrap();
    for meter in [
        kala_core::WorkMeter::new(),
        kala_core::WorkMeter::bounded(100, None),
    ] {
        let meter = meter.with_deadline(Some(std::time::Instant::now()));
        let result = meter.check();
        assert!(
            matches!(
                result,
                Err(Error::WorkInterrupted {
                    reason: kala_core::WorkInterruptReason::Stopped
                })
            ),
            "an expired meter must interrupt before delivering rows: {result:?}"
        );
        let view = world.at_canon(world.latest_tx()).metered(&meter);
        assert!(matches!(
            view.as_of(greg, "tag", Tick(0)),
            Err(Error::WorkInterrupted {
                reason: kala_core::WorkInterruptReason::Stopped
            })
        ));
        meter.take();
        assert!(
            matches!(
                meter.check(),
                Err(Error::WorkInterrupted {
                    reason: kala_core::WorkInterruptReason::Stopped
                })
            ),
            "resetting work counters must not extend an expired deadline"
        );
    }
}
