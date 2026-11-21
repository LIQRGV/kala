//! Item 3: entity IDs, handles and per-type sequencing through the World API.

use kala_core::*;

fn world() -> World {
    World::in_memory(Schema::new().node_type("CHARACTER").node_type("PLACE")).expect("world")
}

#[test]
fn first_character_gets_seq_1() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    assert_eq!(e.seq(), 1);
}

#[test]
fn second_character_gets_seq_2() {
    let mut w = world();
    let a = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = w.create_entity("CHARACTER", Some("mara")).unwrap();
    assert_eq!(a.seq(), 1);
    assert_eq!(b.seq(), 2);
    assert_ne!(a, b);
    assert_eq!(a.node_type(), b.node_type());
}

#[test]
fn sequences_are_per_type() {
    let mut w = world();
    let c = w.create_entity("CHARACTER", None).unwrap();
    let p = w.create_entity("PLACE", None).unwrap();
    assert_eq!(c.seq(), 1);
    assert_eq!(p.seq(), 1);
    assert_ne!(c.node_type(), p.node_type());
    assert_ne!(c, p);
}

#[test]
fn places_sort_apart_from_characters() {
    let mut w = world();
    let c1 = w.create_entity("CHARACTER", None).unwrap();
    let c2 = w.create_entity("CHARACTER", None).unwrap();
    let p1 = w.create_entity("PLACE", None).unwrap();
    // Declared first => lower type ID => all characters sort before all places.
    assert!(c1 < c2);
    assert!(c2 < p1);
    assert_eq!(c1.to_bytes()[0..2], c2.to_bytes()[0..2]);
    assert_ne!(c1.to_bytes()[0..2], p1.to_bytes()[0..2]);
}

#[test]
fn format_id_uses_type_name_and_seq() {
    let mut w = world();
    let c = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let p = w.create_entity("PLACE", None).unwrap();
    let c2 = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(w.format_id(c), "CHARACTER#1");
    assert_eq!(w.format_id(p), "PLACE#1");
    assert_eq!(w.format_id(c2), "CHARACTER#2");
}

#[test]
fn entity_by_handle_finds_it() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let v = w.create_entity("PLACE", Some("varrow")).unwrap();
    assert_eq!(w.entity_by_handle("edwin"), Some(e));
    assert_eq!(w.entity_by_handle("varrow"), Some(v));
}

#[test]
fn unknown_handle_is_none() {
    let mut w = world();
    assert_eq!(w.entity_by_handle("edwin"), None);
    w.create_entity("CHARACTER", Some("edwin")).unwrap();
    assert_eq!(w.entity_by_handle("Edwin"), None); // handles are exact
    assert_eq!(w.entity_by_handle("mara"), None);
}

#[test]
fn entity_without_handle_works() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(e.seq(), 1);
    assert_eq!(w.format_id(e), "CHARACTER#1");
}

#[test]
fn duplicate_handle_same_type_is_error() {
    let mut w = world();
    w.create_entity("CHARACTER", Some("edwin")).unwrap();
    assert!(matches!(
        w.create_entity("CHARACTER", Some("edwin")),
        Err(Error::DuplicateHandle(_))
    ));
}

#[test]
fn duplicate_handle_across_types_is_error() {
    let mut w = world();
    w.create_entity("CHARACTER", Some("varrow")).unwrap();
    assert!(matches!(
        w.create_entity("PLACE", Some("varrow")),
        Err(Error::DuplicateHandle(_))
    ));
}

#[test]
fn duplicate_handle_does_not_consume_a_sequence_number() {
    let mut w = world();
    let a = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    assert!(w.create_entity("CHARACTER", Some("edwin")).is_err());
    assert!(w.create_entity("PLACE", Some("edwin")).is_err());
    let b = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let p = w.create_entity("PLACE", None).unwrap();
    assert_eq!(a.seq(), 1);
    assert_eq!(b.seq(), 2);
    assert_eq!(p.seq(), 1);
    // Original handle still maps to the first entity.
    assert_eq!(w.entity_by_handle("edwin"), Some(a));
}

#[test]
fn unknown_node_type_is_error() {
    let mut w = world();
    assert!(matches!(
        w.create_entity("DRAGON", None),
        Err(Error::UnknownNodeType(_))
    ));
    // Node type names are exact.
    assert!(matches!(
        w.create_entity("character", None),
        Err(Error::UnknownNodeType(_))
    ));
}

#[test]
fn unknown_node_type_does_not_consume_handle_or_sequence() {
    let mut w = world();
    assert!(w.create_entity("DRAGON", Some("edwin")).is_err());
    assert_eq!(w.entity_by_handle("edwin"), None);
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    assert_eq!(e.seq(), 1);
}

#[test]
fn empty_handle_is_invalid() {
    let mut w = world();
    assert!(matches!(
        w.create_entity("CHARACTER", Some("")),
        Err(Error::InvalidHandle(_))
    ));
    assert_eq!(w.entity_by_handle(""), None);
    // and it did not consume a sequence number
    let e = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(e.seq(), 1);
}

#[test]
fn duplicate_node_type_name_in_schema_is_error() {
    let r = World::in_memory(Schema::new().node_type("CHARACTER").node_type("CHARACTER"));
    assert!(matches!(r, Err(Error::DuplicateName(_))));
}

#[test]
fn empty_schema_world_rejects_every_create() {
    let mut w = World::in_memory(Schema::new()).unwrap();
    assert!(matches!(
        w.create_entity("CHARACTER", None),
        Err(Error::UnknownNodeType(_))
    ));
}

#[test]
fn many_entities_are_distinct_and_sequential() {
    let mut w = world();
    let ids: Vec<EntityId> = (0..50)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    for (i, e) in ids.iter().enumerate() {
        assert_eq!(e.seq(), i as u64 + 1);
    }
}

fn schema_with_types(n: usize) -> Schema {
    (0..n).fold(Schema::new(), |s, i| s.node_type(&format!("T{i}")))
}

#[test]
fn schema_with_32768_node_types_is_accepted() {
    let mut w = World::in_memory(schema_with_types(32768)).expect("32768 types fit in 15 bits");
    let first = w.create_entity("T0", None).unwrap();
    let last = w.create_entity("T32767", None).unwrap();
    assert_eq!(first.node_type(), NodeTypeId(0));
    assert_eq!(last.node_type(), NodeTypeId(32767));
    assert_eq!(last.seq(), 1);
    assert_eq!(w.format_id(last), "T32767#1");
}

#[test]
fn schema_with_32769_node_types_is_id_error() {
    assert!(matches!(
        World::in_memory(schema_with_types(32769)),
        Err(Error::Id(_))
    ));
}

#[test]
fn empty_node_type_name_is_invalid() {
    assert!(matches!(
        World::in_memory(Schema::new().node_type("")),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        World::in_memory(Schema::new().node_type("CHARACTER").node_type("")),
        Err(Error::InvalidName(_))
    ));
}

// ---- PM decision: create_entity is one auto-committed transaction ----

#[test]
fn fresh_world_latest_tx_is_zero() {
    assert_eq!(world().latest_tx(), TxId(0));
}

#[test]
fn create_entity_advances_latest_tx_by_exactly_one() {
    let mut w = world();
    let t0 = w.latest_tx();
    w.create_entity("CHARACTER", Some("edwin")).unwrap();
    assert_eq!(w.latest_tx(), TxId(t0.0 + 1));
    w.create_entity("PLACE", None).unwrap();
    assert_eq!(w.latest_tx(), TxId(t0.0 + 2));
    w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(w.latest_tx(), TxId(t0.0 + 3));
}

#[test]
fn failed_create_entity_does_not_advance_latest_tx() {
    let mut w = world();
    w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let tx = w.latest_tx();
    assert!(w.create_entity("CHARACTER", Some("edwin")).is_err()); // duplicate handle
    assert!(w.create_entity("NOPE", None).is_err()); // unknown node type
    assert!(w.create_entity("CHARACTER", Some("")).is_err()); // invalid handle
    assert_eq!(w.latest_tx(), tx);
}
