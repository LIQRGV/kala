//! A directed relationship with an inverse name and/or reverse cardinality can
//! be migrated to bidirectional: the new attribute drops both, and the inverse
//! name is freed.

use kala_core::*;

fn world(
    def: AttrDef,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    World,
    EntityId,
    EntityId,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world =
        World::create(&path, Schema::new().node_type("CHARACTER").attribute(def)).unwrap();
    let a = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = world.create_entity("CHARACTER", Some("mara")).unwrap();
    world
        .assert(a, "kin", Value::Ref(b), Some(Tick(-300)))
        .unwrap();
    (dir, path, world, a, b)
}

fn kin() -> AttrDef {
    AttrDef::new("kin", ValueType::Ref, Kind::State, Cardinality::Many)
}

fn to_bidirectional(world: &mut World) -> Result<MigrationReport, Error> {
    let mut tx = world.begin();
    let report = tx.migrate_attribute(
        "kin",
        MigrationOptions {
            direction: Some(RefDirection::Bidirectional),
            ..MigrationOptions::default()
        },
    )?;
    tx.commit()?;
    Ok(report)
}

#[test]
fn migrating_an_inverse_and_reverse_one_attribute_to_bidirectional_drops_both() {
    let (_d, path, mut world, a, b) = world(kin().inverse("kin_of").reverse_one());
    let migrated = to_bidirectional(&mut world);
    assert!(
        migrated.is_ok(),
        "must succeed, dropping inverse/reverse: {migrated:?}"
    );
    let def = world.attribute("kin").unwrap();
    assert_eq!(def.direction, Some(RefDirection::Bidirectional));
    assert_eq!(def.inverse, None);
    assert_eq!(def.reverse_cardinality, None);
    assert_eq!(world.as_of(a, "kin", Tick(0)).unwrap(), [Value::Ref(b)]);
    // The inverse name is freed: it no longer resolves, anywhere.
    assert!(world.attribute("kin_of").is_none());
    assert!(
        world
            .neighbors(b, "kin_of", Direction::Out, Tick(0))
            .is_err(),
        "the dropped inverse name must not resolve"
    );
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert!(
        doctor(&reopened, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

#[test]
fn migrating_a_reverse_one_only_attribute_to_bidirectional_drops_the_reverse_label() {
    let (_d, path, mut world, a, b) = world(kin().reverse_one());
    let migrated = to_bidirectional(&mut world);
    assert!(
        migrated.is_ok(),
        "must succeed, dropping reverse: {migrated:?}"
    );
    let def = world.attribute("kin").unwrap();
    assert_eq!(def.direction, Some(RefDirection::Bidirectional));
    assert_eq!(def.reverse_cardinality, None);
    assert_eq!(world.as_of(a, "kin", Tick(0)).unwrap(), [Value::Ref(b)]);
    let reopened = World::open_read_only(&path).unwrap();
    assert_eq!(world.index_dump(), reopened.index_dump());
}

#[test]
fn the_freed_inverse_name_can_be_declared_again() {
    let (_d, _p, mut world, _a, _b) = world(kin().inverse("kin_of"));
    to_bidirectional(&mut world).expect("migration succeeds");
    let mut tx = world.begin();
    let reused = tx.declare_attribute(AttrDef::new(
        "kin_of",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    assert!(reused.is_ok(), "{reused:?}");
}
