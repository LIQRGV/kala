//! Format-5 schema payloads and semantic validation, with valid outer CRCs.

mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::{EntryBody, LOG_FORMAT_VERSION, read_log};
use kala_core::schema::SchemaEdit;
use kala_core::*;
use proptest::prelude::*;

fn string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn attribute(
    id: u32,
    name: &str,
    value_type: u8,
    cardinality: u8,
    inferred: u8,
    deprecated: u8,
) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&id.to_le_bytes());
    string(&mut out, name);
    out.extend_from_slice(&[value_type, 0, cardinality, 1, 0, 0, 0, inferred, deprecated]);
    out
}

fn node_type(id: u16, name: &str, inferred: u8) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&id.to_le_bytes());
    string(&mut out, name);
    out.push(inferred);
    out
}

fn setting(key: &str, value: &str) -> Vec<u8> {
    let mut out = vec![2];
    string(&mut out, key);
    string(&mut out, value);
    out
}

fn append(bytes: &mut Vec<u8>, entries: &[(u8, Vec<u8>)], next_id: u64, tx: u64) {
    let mut body = Vec::new();
    for (i, (kind, payload)) in entries.iter().enumerate() {
        body.extend_from_slice(&frame(*kind, next_id + i as u64, payload));
    }
    body.extend_from_slice(&commit_frame(&body, next_id + entries.len() as u64, tx));
    bytes.extend_from_slice(&txn(tx, &body));
}

fn baseline() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Vec<u8>,
    u64,
    u64,
    EntityId,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ));
    let mut world = World::create(&path, schema).unwrap();
    let e = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    world
        .assert(e, "age", Value::I64(42), Some(Tick(-300)))
        .unwrap();
    let tx = world.latest_tx().0 + 1;
    drop(world);
    let id = read_log(&path).unwrap().last().unwrap().id.0 + 1;
    let bytes = std::fs::read(&path).unwrap();
    (dir, path, bytes, id, tx, e)
}

fn open_appended(entries: &[(u8, Vec<u8>)]) -> Result<World, Error> {
    let (_dir, path, mut bytes, id, tx, _) = baseline();
    append(&mut bytes, entries, id, tx);
    std::fs::write(&path, bytes).unwrap();
    World::open_read_only(&path)
}

#[test]
fn version_four_is_refused_as_written_by_older_kala() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v4.kala");
    std::fs::write(&path, common::header(4)).unwrap();
    assert_eq!(LOG_FORMAT_VERSION, 5);
    let error = World::open_read_only(&path).unwrap_err();
    assert_eq!(error, Error::UnsupportedVersion(4));
    let message = error.to_string();
    assert!(
        message.contains("older Kala") && message.contains("recreat"),
        "{message}"
    );
}

#[test]
fn schema_entries_round_trip_and_replay_in_operation_order() {
    let (_dir, path, mut bytes, id, tx, _) = baseline();
    let entries = vec![
        (9, attribute(1, "height", 3, 0, 1, 0)),
        (9, node_type(1, "FACTION", 1)),
        (9, setting("schema", "strict")),
        (9, attribute(0, "years", 2, 0, 0, 0)),
    ];
    append(&mut bytes, &entries, id, tx);
    std::fs::write(&path, bytes).unwrap();
    let log = read_log(&path).unwrap();
    let changes = log
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Schema(edit) => Some(edit),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 4);
    assert!(
        matches!(changes[0], SchemaEdit::Attribute { id: AttrId(1), def } if def.name == "height" && def.inferred && def.value_type == ValueType::F64)
    );
    assert!(
        matches!(changes[1], SchemaEdit::NodeType { id: NodeTypeId(1), name, inferred: true } if name == "FACTION")
    );
    assert!(
        matches!(changes[2], SchemaEdit::Setting { key, value } if key == "schema" && value == "strict")
    );
    let world = World::open_read_only(&path).unwrap();
    assert!(world.attribute("height").unwrap().inferred);
    assert!(world.attribute("age").is_none());
    assert_eq!(
        world.attribute("years").unwrap().cardinality,
        Cardinality::One
    );
    assert!(
        doctor(&world, &DoctorOptions::default())
            .findings
            .is_empty()
    );
}

#[test]
fn impossible_schema_redefinitions_are_corrupt_with_positive_controls() {
    // A pure rename/cardinality label edit is valid even after facts exist.
    assert!(open_appended(&[(9, attribute(0, "years", 2, 0, 0, 0))]).is_ok());
    assert!(open_appended(&[(9, attribute(1, "height", 3, 0, 0, 0))]).is_ok());
    for bad in [
        attribute(0, "age", 4, 0, 0, 0),     // type change with a stored fact
        attribute(2, "height", 3, 0, 0, 0),  // nonconsecutive new ID
        attribute(1, "age", 2, 0, 0, 0),     // duplicate latest name
        attribute(0, "age@old", 2, 0, 0, 0), // reserved migration rename
        attribute(1, "handle", 2, 0, 0, 0),  // pseudo-attribute
        node_type(2, "FACTION", 0),          // unknown node-type ID gap
        node_type(1, "CHARACTER", 0),        // duplicate node type
    ] {
        let opened = open_appended(&[(9, bad.clone())]);
        assert!(
            matches!(opened, Err(Error::Corrupt(_))),
            "{bad:?}: {opened:?}"
        );
    }
}

#[test]
fn a_fact_after_deprecation_and_undeprecation_are_corrupt() {
    assert!(open_appended(&[(9, attribute(0, "age", 2, 0, 0, 1))]).is_ok());
    let (_dir, path, mut bytes, id, tx, e) = baseline();
    let mut fact = Vec::new();
    fact.extend_from_slice(&e.raw().to_le_bytes());
    fact.extend_from_slice(&0_u32.to_le_bytes());
    fact.extend_from_slice(&0_i64.to_le_bytes());
    fact.extend_from_slice(&[0, 0]); // assert, exact
    fact.extend_from_slice(&Value::I64(50).encode().unwrap());
    append(
        &mut bytes,
        &[(9, attribute(0, "age", 2, 0, 0, 1)), (4, fact)],
        id,
        tx,
    );
    std::fs::write(&path, bytes).unwrap();
    assert!(matches!(
        World::open_read_only(&path),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(
        open_appended(&[
            (9, attribute(0, "age", 2, 0, 0, 1)),
            (9, attribute(0, "age", 2, 0, 0, 0)),
        ]),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn every_new_tag_and_boolean_flag_is_validated() {
    let (_dir, path, bytes, id, tx, _) = baseline();
    let valid = attribute(1, "height", 3, 0, 1, 0);
    let attrs_start = 1 + 4 + 4 + "height".len();
    for bad in [2, 255] {
        for flag_offset in [attrs_start + 7, attrs_start + 8] {
            let mut payload = valid.clone();
            payload[flag_offset] = bad;
            let mut file = bytes.clone();
            append(&mut file, &[(9, payload)], id, tx);
            std::fs::write(&path, file).unwrap();
            assert!(matches!(read_log(&path), Err(Error::Corrupt(_))));
        }
        let mut file = bytes.clone();
        append(&mut file, &[(9, node_type(1, "FACTION", bad))], id, tx);
        std::fs::write(&path, file).unwrap();
        assert!(matches!(read_log(&path), Err(Error::Corrupt(_))));
        let mut migration = valid.clone();
        migration[0] = 3;
        migration.push(bad);
        let mut file = bytes.clone();
        append(&mut file, &[(9, migration)], id, tx);
        std::fs::write(&path, file).unwrap();
        assert!(matches!(read_log(&path), Err(Error::Corrupt(_))));
    }
    for bad_tag in [4, 127, 255] {
        let mut file = bytes.clone();
        append(&mut file, &[(9, vec![bad_tag])], id, tx);
        std::fs::write(&path, file).unwrap();
        assert!(matches!(read_log(&path), Err(Error::Corrupt(_))));
    }
}

#[test]
fn inferred_definitions_require_the_live_inference_defaults() {
    let scalar = attribute(1, "height", 3, 0, 1, 0);
    assert!(open_appended(&[(9, scalar.clone())]).is_ok());
    let fields = 1 + 4 + 4 + "height".len();
    for (offset, value) in [(fields + 1, 1), (fields + 2, 1), (fields + 3, 0)] {
        let mut invalid = scalar.clone();
        invalid[offset] = value;
        let result = open_appended(&[(9, invalid)]);
        assert!(
            matches!(result, Err(Error::Corrupt(_))),
            "impossible inferred scalar field {offset}={value}: {result:?}"
        );
    }
    let mut reference = attribute(1, "height", 5, 1, 1, 0);
    reference[fields + 5] = 1; // ref inference is directed + many
    assert!(open_appended(&[(9, reference.clone())]).is_ok());
    for (offset, value) in [
        (fields + 1, 1),
        (fields + 2, 0),
        (fields + 3, 0),
        (fields + 4, 1),
        (fields + 5, 2),
    ] {
        let mut invalid = reference.clone();
        invalid[offset] = value;
        let result = open_appended(&[(9, invalid)]);
        assert!(
            matches!(result, Err(Error::Corrupt(_))),
            "impossible inferred ref field {offset}={value}: {result:?}"
        );
    }
    let mut inverse = reference;
    inverse.truncate(fields + 6);
    inverse.push(1);
    string(&mut inverse, "shorter");
    inverse.extend_from_slice(&[1, 0]);
    assert!(matches!(
        open_appended(&[(9, inverse)]),
        Err(Error::Corrupt(_))
    ));
    // A later label edit remains valid while preserving its inferred origin.
    assert!(open_appended(&[(9, scalar), (9, attribute(1, "height", 3, 1, 1, 0)),]).is_ok());
}

#[test]
fn inferred_redefinition_cannot_change_structure_without_becoming_declared() {
    let inferred = attribute(1, "height", 3, 0, 1, 0);
    let fields = 1 + 4 + 4 + "height".len();
    for (offset, byte) in [(fields, 2), (fields + 1, 1), (fields + 3, 0)] {
        let mut impossible = inferred.clone();
        impossible[offset] = byte;
        let opened = open_appended(&[(9, inferred.clone()), (9, impossible)]);
        assert!(
            matches!(opened, Err(Error::Corrupt(_))),
            "an inferred structural redefinition cannot be produced by a live operation: field {offset}={byte}"
        );
    }
    let mut reference = attribute(1, "height", 5, 1, 1, 0);
    reference[fields + 5] = 1;
    let mut mutual = reference.clone();
    mutual[fields + 5] = 2;
    let mut reverse_label = reference.clone();
    reverse_label[fields + 4] = 1;
    let mut inverse = reference[..fields + 6].to_vec();
    inverse.push(1);
    string(&mut inverse, "shorter");
    inverse.extend_from_slice(&[1, 0]);
    for revision in [mutual, reverse_label, inverse] {
        assert!(matches!(
            open_appended(&[(9, reference.clone()), (9, revision)]),
            Err(Error::Corrupt(_))
        ));
    }

    // These native operations preserve inferred origin and remain legal.
    for revision in [
        attribute(1, "height", 3, 1, 1, 0),
        attribute(1, "height", 3, 0, 1, 1),
        attribute(1, "stature", 3, 0, 1, 0),
    ] {
        assert!(open_appended(&[(9, inferred.clone()), (9, revision)]).is_ok());
    }
    // An explicit declaration can promote and redefine an unused attribute.
    assert!(open_appended(&[(9, inferred), (9, attribute(1, "height", 2, 1, 0, 0)),]).is_ok());
}

#[test]
fn one_schema_entry_cannot_combine_deprecation_with_other_changes() {
    let combined_cardinality = attribute(0, "age", 2, 1, 0, 1);
    assert!(
        matches!(
            open_appended(&[(9, combined_cardinality)]),
            Err(Error::Corrupt(_))
        ),
        "cardinality and deprecation require separate native operations"
    );
    let inferred = attribute(1, "height", 3, 0, 1, 0);
    let combined_promotion = attribute(1, "height", 3, 0, 0, 1);
    assert!(
        matches!(
            open_appended(&[(9, inferred.clone()), (9, combined_promotion)]),
            Err(Error::Corrupt(_))
        ),
        "promotion and deprecation cannot be emitted by one native operation"
    );

    // The same final definitions are valid when each operation is logged.
    assert!(
        open_appended(&[
            (9, attribute(0, "age", 2, 1, 0, 0)),
            (9, attribute(0, "age", 2, 1, 0, 1)),
        ])
        .is_ok()
    );
    assert!(
        open_appended(&[
            (9, inferred),
            (9, attribute(1, "height", 3, 0, 0, 0)),
            (9, attribute(1, "height", 3, 0, 0, 1)),
        ])
        .is_ok()
    );
}

#[test]
fn deprecated_attribute_cardinality_only_revision_is_a_valid_log_sequence() {
    let world = open_appended(&[
        (9, attribute(0, "age", 2, 0, 0, 1)),
        (9, attribute(0, "age", 2, 1, 0, 1)),
    ])
    .expect("cardinality remains editable after deprecation");
    assert!(world.attribute("age").unwrap().deprecated);
    assert_eq!(
        world.attribute("age").unwrap().cardinality,
        Cardinality::Many
    );
    let e = world.entity_by_handle("edwin").unwrap();
    assert_eq!(world.as_of(e, "age", Tick(0)).unwrap(), [Value::I64(42)]);
}

#[test]
fn strict_mode_replay_refuses_new_inferred_attributes_and_node_types() {
    for definition in [
        attribute(1, "height", 3, 0, 1, 0),
        node_type(1, "FACTION", 1),
    ] {
        let result = open_appended(&[(9, setting("schema", "strict")), (9, definition)]);
        assert!(
            matches!(result, Err(Error::Corrupt(_))),
            "strict mode cannot infer: {result:?}"
        );
    }
    assert!(
        open_appended(&[
            (9, setting("schema", "strict")),
            (9, attribute(1, "height", 3, 0, 0, 0)),
            (9, node_type(1, "FACTION", 0)),
        ])
        .is_ok()
    );
    assert!(
        open_appended(&[
            (9, setting("schema", "strict")),
            (9, setting("schema", "infer")),
            (9, attribute(1, "height", 3, 0, 1, 0)),
            (9, node_type(1, "FACTION", 1)),
        ])
        .is_ok()
    );
}

fn reseal_last_transaction(bytes: &mut Vec<u8>, commit_id: u64) {
    let span = *common::spans(bytes).last().unwrap();
    let frames = common::frame_spans(bytes, span);
    let commit_start = frames.last().unwrap().0;
    let mut body = bytes[span.body_start..commit_start].to_vec();
    body.extend_from_slice(&commit_frame(&body, commit_id, span.tx));
    common::replace_last_body(bytes, &body);
}

#[test]
fn altered_migration_copy_is_corrupt_even_with_valid_checksums() {
    let (_dir, path, _, _, _, e) = baseline();
    let mut world = World::open(&path).unwrap();
    let mut tx = world.begin();
    tx.migrate_attribute(
        "age",
        MigrationOptions {
            value_type: Some(ValueType::Str),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    drop(world);
    let control = World::open_read_only(&path).unwrap();
    assert_eq!(
        control.as_of(e, "age", Tick(0)).unwrap(),
        [Value::Str("42".into())]
    );
    let commit_id = read_log(&path).unwrap().last().unwrap().id.0;
    let mut bytes = std::fs::read(&path).unwrap();
    let span = *common::spans(&bytes).last().unwrap();
    let (_, end, _) = common::frame_spans(&bytes, span)
        .into_iter()
        .find(|(_, _, kind)| *kind == 4)
        .unwrap();
    let original = Value::Str("42".into()).encode().unwrap();
    let changed = Value::Str("43".into()).encode().unwrap();
    assert_eq!(&bytes[end - original.len()..end], original);
    bytes[end - original.len()..end].copy_from_slice(&changed);
    reseal_last_transaction(&mut bytes, commit_id);
    std::fs::write(&path, bytes).unwrap();
    assert!(
        read_log(&path).is_ok(),
        "framing/checksums must still be valid"
    );
    let error = World::open_read_only(&path).unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "{error}");
    assert!(error.to_string().contains("migration"), "{error}");
}

#[test]
fn migration_marker_cannot_omit_the_ref_direction_and_borrow_the_old_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
            "link",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        )),
    )
    .unwrap();
    let a = world.create_entity("CHARACTER", None).unwrap();
    let b = world.create_entity("CHARACTER", None).unwrap();
    world
        .assert(a, "link", Value::Ref(b), Some(Tick(0)))
        .unwrap();
    let mut tx = world.begin();
    tx.migrate_attribute(
        "link",
        MigrationOptions {
            cardinality: Some(Cardinality::Many),
            ..MigrationOptions::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    drop(world);
    World::open_read_only(&path).unwrap();
    let commit_id = read_log(&path).unwrap().last().unwrap().id.0;
    let mut bytes = std::fs::read(&path).unwrap();
    let span = *common::spans(&bytes).last().unwrap();
    let (start, _, _) = common::frame_spans(&bytes, span)
        .into_iter()
        .find(|(start, _, kind)| *kind == 9 && bytes[start + 13] == 3)
        .unwrap();
    let name_length =
        u32::from_le_bytes(bytes[start + 18..start + 22].try_into().unwrap()) as usize;
    let direction = start + 22 + name_length + 5;
    assert_eq!(bytes[direction], 1);
    bytes[direction] = 0;
    reseal_last_transaction(&mut bytes, commit_id);
    std::fs::write(&path, bytes).unwrap();
    assert!(read_log(&path).is_ok());
    let error = World::open_read_only(&path).unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "{error}");
    assert!(error.to_string().contains("migration"), "{error}");
}

#[test]
fn doctor_schema_metadata_matches_replay() {
    let mut world = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.commit().unwrap();
    assert!(
        doctor(&world, &DoctorOptions::default())
            .findings
            .is_empty()
    );
    world.tamper_schema();
    let report = doctor(&world, &DoctorOptions::default());
    assert!(
        report
            .findings
            .iter()
            .any(|finding| matches!(finding, Finding::SchemaMismatch)),
        "doctor must compare rebuilt schema metadata, including settings: {report:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn arbitrary_schema_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..256)) {
        let (_dir, path, mut bytes, id, tx, _) = baseline();
        append(&mut bytes, &[(9, payload)], id, tx);
        std::fs::write(&path, bytes).unwrap();
        let _ = read_log(&path);
        let _ = World::open_read_only(&path);
        let _ = doctor_log(&path, &DoctorOptions::default());
    }
}
