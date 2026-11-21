//! Slice 2, item 2: direction in the schema (docs/slice-2-decisions.md).
//!
//! Expected API (all re-exported at the crate root):
//!   `pub enum RefDirection { Directed, Bidirectional }`
//!       (Clone, Copy, Debug, PartialEq, Eq, Hash), module `schema`
//!   `AttrDef { .., pub direction: Option<RefDirection>, pub inverse: Option<String> }`
//!   `AttrDef::new`: Ref -> direction Some(Directed), else None; inverse None
//!   `.directed()`, `.bidirectional()`, `.inverse(&str)` builders
//!   `Error::InvalidDirection(String)` (attribute name)
//! Log v2 DefineAttr body:
//!   id u32 | name str | value_type u8 | kind u8 | cardinality u8 | indexed u8
//!   | direction u8 (0 none, 1 directed, 2 bidirectional)
//!   | has_inverse u8 | [inverse str]
//! `neighbors` / `edge_id` (items 3 and 6) are not tested here.
mod common;

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn base() -> Schema {
    Schema::new().node_type("CHARACTER").node_type("PLACE")
}

fn reff(name: &str) -> AttrDef {
    AttrDef::new(name, ValueType::Ref, Kind::State, Cardinality::Many)
}

fn world_err(schema: Schema) -> Error {
    match World::in_memory(schema) {
        Err(e) => e,
        Ok(_) => panic!("in_memory unexpectedly succeeded"),
    }
}

/// `World::create` must fail the same way and must not create the file
/// (slice 1: the schema is validated before the file system is touched).
fn create_err(schema: Schema) -> Error {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let e = match World::create(&p, schema) {
        Err(e) => e,
        Ok(_) => panic!("create unexpectedly succeeded"),
    };
    assert!(!p.exists(), "a rejected schema must not create the file");
    e
}

/// Both constructors must agree on the error.
fn both_err(schema: Schema) -> Error {
    let a = world_err(schema.clone());
    let b = create_err(schema);
    assert_eq!(a, b, "in_memory and create disagree");
    a
}

// ---------------------------------------------------------------------------
// AttrDef
// ---------------------------------------------------------------------------

#[test]
fn ref_attribute_defaults_to_directed_without_inverse() {
    for kind in [Kind::State, Kind::Event] {
        let d = AttrDef::new("owns", ValueType::Ref, kind, Cardinality::Many);
        assert_eq!(d.direction, Some(RefDirection::Directed));
        assert_eq!(d.inverse, None);
    }
}

#[test]
fn non_ref_attribute_has_no_direction_and_no_inverse() {
    for vt in [
        ValueType::Bool,
        ValueType::I64,
        ValueType::F64,
        ValueType::Str,
    ] {
        let d = AttrDef::new("x", vt, Kind::State, Cardinality::One);
        assert_eq!(d.direction, None, "{vt:?}");
        assert_eq!(d.inverse, None, "{vt:?}");
    }
}

#[test]
fn builders_set_direction_and_inverse() {
    let d = reff("ally_of").bidirectional();
    assert_eq!(d.direction, Some(RefDirection::Bidirectional));
    assert_eq!(d.inverse, None);

    let d = reff("parent_of").inverse("child_of");
    assert_eq!(d.direction, Some(RefDirection::Directed));
    assert_eq!(d.inverse.as_deref(), Some("child_of"));

    let d = reff("parent_of").directed().inverse("child_of");
    assert_eq!(d.direction, Some(RefDirection::Directed));
    assert_eq!(d.inverse.as_deref(), Some("child_of"));
}

#[test]
fn last_direction_builder_wins_and_unindexed_keeps_direction() {
    assert_eq!(
        reff("a").bidirectional().directed().direction,
        Some(RefDirection::Directed)
    );
    assert_eq!(
        reff("a").directed().bidirectional().direction,
        Some(RefDirection::Bidirectional)
    );
    let d = reff("a").bidirectional().unindexed();
    assert_eq!(d.direction, Some(RefDirection::Bidirectional));
    assert!(!d.indexed);
}

#[test]
fn attr_def_equality_includes_direction_and_inverse() {
    assert_ne!(reff("a"), reff("a").bidirectional());
    assert_ne!(reff("a"), reff("a").inverse("b"));
    assert_eq!(reff("a").inverse("b"), reff("a").inverse("b"));
    assert_ne!(reff("a").inverse("b"), reff("a").inverse("c"));
}

#[test]
fn ref_direction_is_copy_eq_hash_debug() {
    use std::collections::HashSet;
    let d = RefDirection::Directed;
    let e = d; // Copy
    assert_eq!(d, e);
    assert_ne!(RefDirection::Directed, RefDirection::Bidirectional);
    let mut s = HashSet::new();
    s.insert(d);
    s.insert(RefDirection::Bidirectional);
    assert_eq!(s.len(), 2);
    assert!(format!("{d:?}").contains("Directed"));
}

// ---------------------------------------------------------------------------
// Schema checks at world creation
// ---------------------------------------------------------------------------

#[test]
fn valid_direction_schemas_are_accepted() {
    let s = base()
        .attribute(reff("located_in"))
        .attribute(reff("ally_of").bidirectional())
        .attribute(reff("parent_of").inverse("child_of"))
        .attribute(reff("married_to").bidirectional().unindexed())
        .attribute(
            AttrDef::new("bought", ValueType::Ref, Kind::Event, Cardinality::Many)
                .inverse("bought_by"),
        )
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ));
    World::in_memory(s.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    World::create(&dir.path().join("w.kala"), s).unwrap();
}

#[test]
fn non_ref_attribute_with_a_direction_is_invalid_direction() {
    for vt in [
        ValueType::Bool,
        ValueType::I64,
        ValueType::F64,
        ValueType::Str,
    ] {
        for dir in [RefDirection::Directed, RefDirection::Bidirectional] {
            let mut d = AttrDef::new("x", vt, Kind::State, Cardinality::One);
            d.direction = Some(dir);
            assert_eq!(
                both_err(base().attribute(d)),
                Error::InvalidDirection("x".into()),
                "{vt:?} {dir:?}"
            );
        }
    }
}

#[test]
fn non_ref_attribute_with_an_inverse_is_invalid_direction() {
    for vt in [
        ValueType::Bool,
        ValueType::I64,
        ValueType::F64,
        ValueType::Str,
    ] {
        let d = AttrDef::new("x", vt, Kind::State, Cardinality::One).inverse("y");
        assert_eq!(
            both_err(base().attribute(d)),
            Error::InvalidDirection("x".into()),
            "{vt:?}"
        );
    }
}

#[test]
fn ref_attribute_with_no_direction_is_invalid_direction() {
    let mut d = reff("owns");
    d.direction = None;
    assert_eq!(
        both_err(base().attribute(d)),
        Error::InvalidDirection("owns".into())
    );
}

#[test]
fn invalid_direction_names_the_offending_attribute_not_an_earlier_one() {
    let mut bad = reff("second");
    bad.direction = None;
    let s = base().attribute(reff("first")).attribute(bad);
    assert_eq!(both_err(s), Error::InvalidDirection("second".into()));
}

#[test]
fn inverse_on_a_bidirectional_attribute_is_invalid_direction() {
    let d = reff("ally_of").bidirectional().inverse("ally_back");
    assert_eq!(
        both_err(base().attribute(d)),
        Error::InvalidDirection("ally_of".into())
    );
}

#[test]
fn empty_inverse_name_is_invalid_name() {
    let d = reff("parent_of").inverse("");
    assert_eq!(
        both_err(base().attribute(d)),
        Error::InvalidName(String::new())
    );
}

#[test]
fn inverse_equal_to_its_own_attribute_name_is_duplicate_name() {
    let d = reff("parent_of").inverse("parent_of");
    assert_eq!(
        both_err(base().attribute(d)),
        Error::DuplicateName("parent_of".into())
    );
}

#[test]
fn inverse_equal_to_another_attribute_name_is_duplicate_name_in_either_order() {
    let s = base()
        .attribute(reff("parent_of").inverse("child_of"))
        .attribute(reff("child_of"));
    assert_eq!(both_err(s), Error::DuplicateName("child_of".into()));
    let s = base()
        .attribute(reff("child_of"))
        .attribute(reff("parent_of").inverse("child_of"));
    assert_eq!(both_err(s), Error::DuplicateName("child_of".into()));
}

#[test]
fn two_attributes_with_the_same_inverse_name_is_duplicate_name() {
    let s = base()
        .attribute(reff("parent_of").inverse("child_of"))
        .attribute(reff("mother_of").inverse("child_of"));
    assert_eq!(both_err(s), Error::DuplicateName("child_of".into()));
}

#[test]
fn inverse_names_are_case_sensitive() {
    let s = base()
        .attribute(reff("parent_of").inverse("Parent_Of"))
        .attribute(reff("mother_of").inverse("CHILD_OF"))
        .attribute(reff("child_of"));
    World::in_memory(s).unwrap();
}

// ---------------------------------------------------------------------------
// Inverse names are not attributes for reads and writes
// ---------------------------------------------------------------------------

fn kin() -> Schema {
    base()
        .attribute(reff("parent_of").inverse("child_of"))
        .attribute(reff("ally_of").bidirectional())
}

#[test]
fn inverse_name_is_unknown_attribute_for_world_reads_and_writes() {
    let mut w = World::in_memory(kin()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let b = w.create_entity("CHARACTER", None).unwrap();
    let unknown = Error::UnknownAttribute("child_of".into());
    assert_eq!(
        w.assert(a, "child_of", Value::Ref(b), t(1)).unwrap_err(),
        unknown
    );
    assert_eq!(
        w.retract(a, "child_of", Value::Ref(b), t(1)).unwrap_err(),
        unknown
    );
    assert_eq!(w.as_of(a, "child_of", Tick(1)).unwrap_err(), unknown);
    assert_eq!(
        w.as_of_value(a, "child_of", &Value::Ref(b), Tick(1))
            .unwrap_err(),
        unknown
    );
    assert_eq!(w.history(a, "child_of").unwrap_err(), unknown);
    // the base name still works and nothing was stored by the failures
    assert_eq!(w.latest_tx(), TxId(2));
    w.assert(a, "parent_of", Value::Ref(b), t(1)).unwrap();
    assert_eq!(
        w.as_of(a, "parent_of", Tick(1)).unwrap(),
        vec![Value::Ref(b)]
    );
}

#[test]
fn inverse_name_is_unknown_attribute_for_txn_and_view() {
    let mut w = World::in_memory(kin()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let b = w.create_entity("CHARACTER", None).unwrap();
    let unknown = Error::UnknownAttribute("child_of".into());
    {
        let mut txn = w.begin();
        assert_eq!(
            txn.assert(a, "child_of", Value::Ref(b), t(1)).unwrap_err(),
            unknown
        );
    }
    {
        let mut txn = w.begin();
        assert_eq!(
            txn.retract(a, "child_of", Value::Ref(b), t(1)).unwrap_err(),
            unknown
        );
    }
    let v = w.at_canon(w.latest_tx());
    assert_eq!(v.as_of(a, "child_of", Tick(1)).unwrap_err(), unknown);
    assert_eq!(
        v.as_of_value(a, "child_of", &Value::Ref(b), Tick(1))
            .unwrap_err(),
        unknown
    );
    assert_eq!(v.history(a, "child_of").unwrap_err(), unknown);
}

#[test]
fn inverse_name_on_a_different_unknown_name_still_reports_that_name() {
    let mut w = World::in_memory(kin()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(
        w.history(a, "grandchild_of").unwrap_err(),
        Error::UnknownAttribute("grandchild_of".into())
    );
}

// ---------------------------------------------------------------------------
// Persistence: reopen and the log layout
// ---------------------------------------------------------------------------

fn full_schema() -> Schema {
    base()
        .attribute(reff("located_in").directed())
        .attribute(reff("ally_of").bidirectional())
        .attribute(reff("parent_of").inverse("child_of"))
        .attribute(
            AttrDef::new("bought", ValueType::Ref, Kind::Event, Cardinality::Many)
                .inverse("bought_by")
                .unindexed(),
        )
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("alive", ValueType::Bool, Kind::State, Cardinality::One).unindexed(),
        )
}

fn expected_defs() -> Vec<AttrDef> {
    vec![
        reff("located_in").directed(),
        reff("ally_of").bidirectional(),
        reff("parent_of").inverse("child_of"),
        AttrDef::new("bought", ValueType::Ref, Kind::Event, Cardinality::Many)
            .inverse("bought_by")
            .unindexed(),
        AttrDef::new("name", ValueType::Str, Kind::State, Cardinality::One),
        AttrDef::new("alive", ValueType::Bool, Kind::State, Cardinality::One).unindexed(),
    ]
}

fn logged_defs(p: &Path) -> Vec<AttrDef> {
    read_log(p)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            EntryBody::DefineAttr { def, .. } => Some(def),
            _ => None,
        })
        .collect()
}

#[test]
fn read_log_returns_define_attr_with_direction_and_inverse() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, full_schema()).unwrap());
    assert_eq!(logged_defs(&p), expected_defs());
}

#[test]
fn schema_with_direction_survives_reopen_and_keeps_its_rules() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, full_schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(a, "parent_of", Value::Ref(b), t(5)).unwrap();
    drop(w);

    assert_eq!(logged_defs(&p), expected_defs());
    let mut w = World::open(&p).unwrap();
    assert!(w.recovery() == &RecoveryReport::default());
    assert_eq!(logged_defs(&p), expected_defs());
    assert_eq!(
        w.as_of(a, "parent_of", Tick(5)).unwrap(),
        vec![Value::Ref(b)]
    );
    // inverse names are still not attributes after replay
    assert_eq!(
        w.as_of(a, "child_of", Tick(5)).unwrap_err(),
        Error::UnknownAttribute("child_of".into())
    );
    assert_eq!(
        w.assert(a, "bought_by", Value::Ref(b), t(5)).unwrap_err(),
        Error::UnknownAttribute("bought_by".into())
    );
    // and writes keep working and persisting
    w.assert(b, "located_in", Value::Ref(a), t(6)).unwrap();
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(
        w.as_of(b, "located_in", Tick(6)).unwrap(),
        vec![Value::Ref(a)]
    );
}

/// The raw DefineAttr payload body (after kind + entry id) for `name` in the
/// genesis txn of the file at `p`.
fn raw_define_attr_body(p: &Path, name: &str) -> Vec<u8> {
    let file = std::fs::read(p).unwrap();
    let s = common::spans(&file)[0];
    for (start, end, kind) in common::frame_spans(&file, s) {
        if kind != 2 {
            continue;
        }
        let body = &file[start + 4 + 9..end];
        let n = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
        if &body[8..8 + n] == name.as_bytes() {
            return body.to_vec();
        }
    }
    panic!("no DefineAttr frame for {name}");
}

/// Mirrors the documented DefineAttr body field by field, hence the arguments.
#[allow(clippy::too_many_arguments)]
fn attr_body(
    id: u32,
    name: &str,
    vt: u8,
    kind: u8,
    card: u8,
    indexed: u8,
    dir: u8,
    inverse: Option<&str>,
) -> Vec<u8> {
    let mut b = id.to_le_bytes().to_vec();
    b.extend_from_slice(&(name.len() as u32).to_le_bytes());
    b.extend_from_slice(name.as_bytes());
    b.extend_from_slice(&[vt, kind, card, indexed, dir]);
    match inverse {
        None => b.push(0),
        Some(i) => {
            b.push(1);
            b.extend_from_slice(&(i.len() as u32).to_le_bytes());
            b.extend_from_slice(i.as_bytes());
        }
    }
    b
}

#[test]
fn define_attr_body_follows_the_documented_layout() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, full_schema()).unwrap());
    // value_type: bool 1, i64 2, f64 3, str 4, ref 5; kind: state 0, event 1;
    // cardinality: one 0, many 1; ids are declaration order from 0.
    assert_eq!(
        raw_define_attr_body(&p, "located_in"),
        attr_body(0, "located_in", 5, 0, 1, 1, 1, None)
    );
    assert_eq!(
        raw_define_attr_body(&p, "ally_of"),
        attr_body(1, "ally_of", 5, 0, 1, 1, 2, None)
    );
    assert_eq!(
        raw_define_attr_body(&p, "parent_of"),
        attr_body(2, "parent_of", 5, 0, 1, 1, 1, Some("child_of"))
    );
    assert_eq!(
        raw_define_attr_body(&p, "bought"),
        attr_body(3, "bought", 5, 1, 1, 0, 1, Some("bought_by"))
    );
    assert_eq!(
        raw_define_attr_body(&p, "name"),
        attr_body(4, "name", 4, 0, 0, 1, 0, None)
    );
    assert_eq!(
        raw_define_attr_body(&p, "alive"),
        attr_body(5, "alive", 1, 0, 0, 0, 0, None)
    );
}

#[test]
fn reopen_rs_body_define_attr_helper_matches_the_v2_layout() {
    // tests/reopen.rs writes `[2, 0, 1, 1, 0, 0]` after the name: i64, state,
    // many, indexed, direction none, no inverse. That is exactly the layout.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let s = base().attribute(AttrDef::new(
        "rank",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    ));
    drop(World::create(&p, s).unwrap());
    assert_eq!(
        raw_define_attr_body(&p, "rank"),
        attr_body(0, "rank", 2, 0, 1, 1, 0, None)
    );
}

// ---------------------------------------------------------------------------
// Replay re-runs the schema checks: hand-crafted genesis files
// ---------------------------------------------------------------------------

/// header + tx0 holding one node type and the given DefineAttr bodies.
fn genesis_file(attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut nt = 0u16.to_le_bytes().to_vec();
    nt.extend_from_slice(&9u32.to_le_bytes());
    nt.extend_from_slice(b"CHARACTER");
    body.extend(common::frame(1, 1, &nt));
    for (i, a) in attrs.iter().enumerate() {
        body.extend(common::frame(2, 2 + i as u64, a));
    }
    let commit = common::commit_frame(&body, 2 + attrs.len() as u64, 0);
    body.extend(commit);
    let mut file = common::header(2);
    file.extend(common::txn(0, &body));
    file
}

fn write_file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

fn assert_open_corrupt(attrs: &[Vec<u8>], label: &str) {
    let file = genesis_file(attrs);
    let (_d, p) = write_file(&file);
    match World::open(&p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{label}: expected Corrupt, open succeeded"),
    }
    assert_eq!(std::fs::read(&p).unwrap(), file, "{label}: file touched");
    let torn = format!("{}.torn-", p.file_name().unwrap().to_string_lossy());
    let any = std::fs::read_dir(p.parent().unwrap())
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(&torn));
    assert!(!any, "{label}: quarantine made");
}

/// Unparseable bodies: read_log must refuse too.
fn assert_unparseable(attrs: &[Vec<u8>], label: &str) {
    assert_open_corrupt(attrs, label);
    let file = genesis_file(attrs);
    let (_d, p) = write_file(&file);
    match read_log(&p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: read_log: expected Corrupt, got {e:?}"),
        Ok(v) => panic!(
            "{label}: read_log: expected Corrupt, got {} entries",
            v.len()
        ),
    }
    assert_eq!(
        std::fs::read(&p).unwrap(),
        file,
        "{label}: read_log touched"
    );
}

#[test]
fn control_hand_built_v2_genesis_with_directions_opens() {
    let file = genesis_file(&[
        attr_body(0, "parent_of", 5, 0, 1, 1, 1, Some("child_of")),
        attr_body(1, "ally_of", 5, 0, 1, 1, 2, None),
        attr_body(2, "name", 4, 0, 0, 1, 0, None),
    ]);
    let (_d, p) = write_file(&file);
    let w = World::open(&p).unwrap();
    drop(w);
    assert_eq!(std::fs::read(&p).unwrap(), file);
    let defs = logged_defs(&p);
    assert_eq!(defs[0], reff("parent_of").inverse("child_of"));
    assert_eq!(defs[1], reff("ally_of").bidirectional());
    assert_eq!(defs[2].direction, None);
}

#[test]
fn replay_direction_on_a_non_ref_is_corrupt() {
    for vt in [1u8, 2, 3, 4] {
        for dir in [1u8, 2] {
            assert_open_corrupt(
                &[attr_body(0, "x", vt, 0, 0, 1, dir, None)],
                &format!("vt {vt} dir {dir}"),
            );
        }
    }
}

#[test]
fn replay_inverse_on_a_non_ref_is_corrupt() {
    for vt in [1u8, 2, 3, 4] {
        assert_open_corrupt(
            &[attr_body(0, "x", vt, 0, 0, 1, 0, Some("y"))],
            &format!("vt {vt}"),
        );
    }
}

#[test]
fn replay_ref_with_direction_none_is_corrupt() {
    assert_open_corrupt(&[attr_body(0, "owns", 5, 0, 1, 1, 0, None)], "ref, dir 0");
}

#[test]
fn replay_unknown_direction_byte_is_corrupt_for_open_and_read_log() {
    for dir in [3u8, 4, 255] {
        assert_unparseable(
            &[attr_body(0, "owns", 5, 0, 1, 1, dir, None)],
            &format!("direction byte {dir}"),
        );
    }
}

#[test]
fn replay_bad_has_inverse_byte_is_corrupt_for_open_and_read_log() {
    let mut b = attr_body(0, "owns", 5, 0, 1, 1, 1, None);
    *b.last_mut().unwrap() = 2;
    assert_unparseable(&[b], "has_inverse = 2");
}

#[test]
fn replay_v1_shaped_define_attr_without_the_new_bytes_is_corrupt() {
    // v1 body: ends right after `indexed`
    let mut b = 0u32.to_le_bytes().to_vec();
    b.extend_from_slice(&1u32.to_le_bytes());
    b.push(b'x');
    b.extend_from_slice(&[2, 0, 1, 1]);
    assert_unparseable(&[b], "no direction byte");
    // ends right after direction
    let mut b = 0u32.to_le_bytes().to_vec();
    b.extend_from_slice(&1u32.to_le_bytes());
    b.push(b'x');
    b.extend_from_slice(&[2, 0, 1, 1, 0]);
    assert_unparseable(&[b], "no has_inverse byte");
}

#[test]
fn replay_has_inverse_without_the_inverse_string_or_with_trailing_bytes_is_corrupt() {
    let mut b = attr_body(0, "owns", 5, 0, 1, 1, 1, None);
    *b.last_mut().unwrap() = 1; // claims an inverse, none follows
    assert_unparseable(&[b], "missing inverse str");
    let mut b = attr_body(0, "owns", 5, 0, 1, 1, 1, None);
    b.push(0xAA); // trailing byte
    assert_unparseable(&[b], "trailing byte");
    let mut b = attr_body(0, "owns", 5, 0, 1, 1, 1, Some("child_of"));
    b.push(0xAA);
    assert_unparseable(&[b], "trailing byte after inverse");
}

#[test]
fn replay_inverse_on_a_bidirectional_attribute_is_corrupt() {
    assert_open_corrupt(
        &[attr_body(0, "ally_of", 5, 0, 1, 1, 2, Some("ally_back"))],
        "bidirectional with inverse",
    );
}

#[test]
fn replay_empty_inverse_name_is_corrupt() {
    assert_open_corrupt(
        &[attr_body(0, "parent_of", 5, 0, 1, 1, 1, Some(""))],
        "empty inverse",
    );
}

#[test]
fn replay_duplicate_inverse_names_are_corrupt() {
    assert_open_corrupt(
        &[
            attr_body(0, "parent_of", 5, 0, 1, 1, 1, Some("child_of")),
            attr_body(1, "mother_of", 5, 0, 1, 1, 1, Some("child_of")),
        ],
        "same inverse twice",
    );
}

#[test]
fn replay_inverse_equal_to_an_attribute_name_is_corrupt() {
    assert_open_corrupt(
        &[attr_body(0, "parent_of", 5, 0, 1, 1, 1, Some("parent_of"))],
        "inverse = own name",
    );
    assert_open_corrupt(
        &[
            attr_body(0, "parent_of", 5, 0, 1, 1, 1, Some("child_of")),
            attr_body(1, "child_of", 5, 0, 1, 1, 1, None),
        ],
        "inverse = later attribute",
    );
    assert_open_corrupt(
        &[
            attr_body(0, "child_of", 5, 0, 1, 1, 1, None),
            attr_body(1, "parent_of", 5, 0, 1, 1, 1, Some("child_of")),
        ],
        "inverse = earlier attribute",
    );
}

// ---------------------------------------------------------------------------
// Extra edge cases (confirm pass)
// ---------------------------------------------------------------------------

#[test]
fn inverse_may_equal_a_node_type_name_separate_namespaces() {
    let s = base().attribute(reff("lives_in").inverse("PLACE"));
    World::in_memory(s.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, s).unwrap());
    let w = World::open(&p).unwrap();
    drop(w);
    assert_eq!(logged_defs(&p), vec![reff("lives_in").inverse("PLACE")]);
}

#[test]
fn unindexed_ref_attribute_keeps_direction_and_inverse_through_the_log() {
    let s = base()
        .attribute(reff("a").bidirectional().unindexed())
        .attribute(reff("b").inverse("b_back").unindexed());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, s).unwrap());
    let defs = logged_defs(&p);
    assert_eq!(defs[0].direction, Some(RefDirection::Bidirectional));
    assert!(!defs[0].indexed);
    assert_eq!(defs[1].direction, Some(RefDirection::Directed));
    assert_eq!(defs[1].inverse.as_deref(), Some("b_back"));
    assert!(!defs[1].indexed);
}

#[test]
fn definitions_survive_two_reopens_and_the_file_is_unchanged_by_them() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, full_schema()).unwrap());
    let bytes = std::fs::read(&p).unwrap();
    for _ in 0..2 {
        let mut w = World::open(&p).unwrap();
        assert!(w.recovery() == &RecoveryReport::default());
        assert_eq!(
            w.history(
                w.entity_by_handle("nobody").unwrap_or_else(|| {
                    // no entities yet: any read of an inverse name must still fail
                    EntityId::new(NodeTypeId(0), 1).unwrap()
                }),
                "child_of"
            )
            .unwrap_err(),
            Error::UnknownAttribute("child_of".into())
        );
        let a = w.create_entity("CHARACTER", None).unwrap();
        let b = w.create_entity("CHARACTER", None).unwrap();
        w.assert(a, "ally_of", Value::Ref(b), t(1)).unwrap();
        drop(w);
        assert_eq!(logged_defs(&p), expected_defs());
    }
    assert!(std::fs::read(&p).unwrap().starts_with(&bytes));
}
