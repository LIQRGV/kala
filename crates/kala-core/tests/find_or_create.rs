use kala_core::*;

#[test]
fn native_find_or_create_returns_existing_identity_without_entries() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").node_type("PLACE")).unwrap();
    let mut tx = w.begin();
    let (first, created) = tx.find_or_create_entity("CHARACTER", "edwin").unwrap();
    assert!(created);
    let (same, created) = tx.find_or_create_entity("CHARACTER", "edwin").unwrap();
    assert_eq!(same, first);
    assert!(!created);
    tx.commit().unwrap();
    let tx_before = w.latest_tx();
    let entries_before = w.entries().count();
    let mut tx = w.begin();
    let (same, created) = tx.find_or_create_entity("CHARACTER", "edwin").unwrap();
    assert_eq!(same, first);
    assert!(!created);
    let result = tx.commit().unwrap();
    assert_eq!(result.tx, tx_before);
    assert!(result.entries.is_empty());
    assert_eq!(w.entries().count(), entries_before);
    let mut tx = w.begin();
    let error = tx.find_or_create_entity("PLACE", "edwin").unwrap_err();
    assert!(error.to_string().contains("CHARACTER"), "{error}");
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert_eq!(w.entries().count(), entries_before);
}

#[test]
fn native_find_or_create_dropped_handles_are_reusable_and_plain_create_still_errors() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let first;
    {
        let mut tx = w.begin();
        first = tx.find_or_create_entity("CHARACTER", "kael").unwrap().0;
    }
    assert!(w.entity_by_handle("kael").is_none());
    let mut tx = w.begin();
    assert_eq!(
        tx.find_or_create_entity("CHARACTER", "kael").unwrap(),
        (first, true)
    );
    tx.commit().unwrap();
    let mut tx = w.begin();
    assert!(matches!(
        tx.create_entity("CHARACTER", Some("kael")),
        Err(Error::DuplicateHandle(_))
    ));
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
}
