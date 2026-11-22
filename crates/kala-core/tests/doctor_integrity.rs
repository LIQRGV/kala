//! Slice 3, item 8: doctor, the store-integrity layer (docs/slice-3-decisions.md,
//! "Item 8"; CLAUDE.md "Doctor"). RED until the implementation lands: this file
//! fails to COMPILE because the `doctor` module and its items don't exist.
//!
//! API expected (everything re-exported at the crate root; module `doctor`):
//!
//! ```text
//! pub fn doctor(world: &World, opts: &DoctorOptions) -> Report;
//! pub fn doctor_log(path: &Path, opts: &DoctorOptions) -> Result<Report, Error>;
//! #[derive(Clone, Debug, Default)]
//! pub struct DoctorOptions { pub canon: Option<TxId>, pub since: Option<TxId> }
//! #[derive(Clone, Debug, PartialEq)]
//! pub struct Report { pub findings: Vec<Finding> }
//! impl Report {
//!     pub fn is_clean(&self) -> bool;            // no findings at all
//!     pub fn errors(&self) -> impl IntoIterator<Item = &Finding>;   // Severity::Error ones
//!     pub fn warnings(&self) -> impl IntoIterator<Item = &Finding>; // Severity::Warning ones
//! }
//! #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Severity { Error, Warning }
//! #[derive(Clone, Debug, PartialEq)]
//! pub enum Finding {
//!     IncompleteTail { offset: u64 },
//!     ChecksumMismatch { tx: TxId, offset: u64 },
//!     MissingCommit { tx: TxId, offset: u64 },
//!     TxGap { expected: TxId, found: TxId },
//!     EntryIdGap { expected: EntryId, found: EntryId },
//!     DanglingId { entry: EntryId, id: EntityId },
//!     DanglingAttr { entry: EntryId, attr: AttrId },
//!     BadCorrection { entry: EntryId, reason: String },
//!     RevisionSkip { entry: EntryId, of: EntryId, expected: u32, found: u32 },
//!     InvalidEntry { entry: EntryId, reason: String },
//!     IndexMismatch { index: IndexName, detail: String },
//!     IndexDecode { index: IndexName, key: Vec<u8> },
//!     /* item 9 variants (warnings) arrive later */
//! }
//! impl Finding { pub fn severity(&self) -> Severity }   // all of the above: Error
//! #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum IndexName { E, R, ELatest, RLatest }
//!
//! #[doc(hidden)] pub enum Tamper {
//!     InsertRawKey { index: IndexName, key: Vec<u8> },
//!     RemoveRow { index: IndexName, nth: usize },   // nth row of that index in key order
//!     FlipOp { index: IndexName, nth: usize },      // Assert <-> Retract
//! }
//! #[doc(hidden)] impl World { pub fn tamper_index(&mut self, t: Tamper) }
//! ```
//!
//! Semantics the tests rely on (flagged in the report where the contract is
//! silent): the `offset` of `ChecksumMismatch` / `MissingCommit` is NOT
//! asserted; `IncompleteTail.offset` is the offset where the valid prefix ends
//! (the start of the torn/damaged transaction). Assertions are "contains":
//! one root cause may legitimately also surface as follow-on findings
//! (e.g. a skipped txn makes the stored indexes differ from the rebuilt ones).
//! Each stored index is compared on its own, so tampering one index yields
//! `IndexMismatch` for that `IndexName` and for no other.
//!
//! Crafted logs have valid CRCs and framing (tests/common), the kind
//! `World::open` rejects as `Corrupt`; `doctor_log` must report, not fail.
mod common;

use common::{BEGIN_LEN, commit_frame, frame, frame_spans, reseal, spans, txn};
use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

const K_DEFINE_ENTITY: u8 = 3;
const K_FACT: u8 = 4;
const K_DELETED: u8 = 7;
const K_REVISED: u8 = 8;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

// ---------------------------------------------------------------- report helpers

fn n_errors(rep: &Report) -> usize {
    rep.errors().len()
}

fn n_warnings(rep: &Report) -> usize {
    rep.warnings().len()
}

fn assert_clean(rep: &Report, label: &str) {
    assert!(
        rep.is_clean(),
        "{label}: expected clean, got {:#?}",
        rep.findings
    );
    assert_eq!(n_errors(rep), 0, "{label}");
    assert_eq!(n_warnings(rep), 0, "{label}");
}

fn any(rep: &Report, f: impl Fn(&Finding) -> bool) -> bool {
    rep.findings.iter().any(f)
}

fn assert_has(rep: &Report, f: impl Fn(&Finding) -> bool, what: &str) {
    assert!(any(rep, f), "expected {what}, got {:#?}", rep.findings);
    assert!(!rep.is_clean(), "{what}: report must not be clean");
    for x in &rep.findings {
        if f_is_integrity(x) {
            assert_eq!(x.severity(), Severity::Error, "{x:?}");
        }
    }
}

/// every item-8 variant is an Error
fn f_is_integrity(f: &Finding) -> bool {
    matches!(
        f,
        Finding::IncompleteTail { .. }
            | Finding::ChecksumMismatch { .. }
            | Finding::MissingCommit { .. }
            | Finding::TxGap { .. }
            | Finding::EntryIdGap { .. }
            | Finding::DanglingId { .. }
            | Finding::DanglingAttr { .. }
            | Finding::BadCorrection { .. }
            | Finding::RevisionSkip { .. }
            | Finding::InvalidEntry { .. }
            | Finding::IndexMismatch { .. }
            | Finding::IndexDecode { .. }
    )
}

fn opts() -> DoctorOptions {
    DoctorOptions::default()
}

fn since(n: u64) -> DoctorOptions {
    DoctorOptions {
        since: Some(TxId(n)),
        ..DoctorOptions::default()
    }
}

// ---------------------------------------------------------------- a realistic script

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("child_of"),
        )
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "level",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "weight",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
}

fn one(w: WriteResult) -> EntryId {
    assert_eq!(w.entries.len(), 1, "{w:?}");
    w.entries[0]
}

/// Entities, refs, bidirectional pairs, edge properties, deletes, moved
/// ticks, moved values, a multi-write txn. No cardinality overlaps and no
/// edge property outside its edge's life (those are item 9 warnings).
/// Every write must really change something.
fn script(w: &mut World) {
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();

    let ok = |res: Result<WriteResult, Error>, changed: usize, what: &str| -> WriteResult {
        let res = res.unwrap_or_else(|e| panic!("{what}: {e:?}"));
        assert_eq!(res.changed, changed, "{what}");
        res
    };

    // names with a rename
    ok(
        w.assert(edwin, "name", s("Edwin"), t(-400)),
        1,
        "edwin name",
    );
    ok(
        w.retract(edwin, "name", s("Edwin"), t(-180)),
        1,
        "edwin name end",
    );
    ok(
        w.assert(edwin, "name", s("the Grey King"), t(-180)),
        1,
        "edwin new name",
    );
    ok(w.assert(mara, "name", s("Mara"), t(-400)), 1, "mara name");
    // scalars
    ok(w.assert(edwin, "level", Value::I64(3), t(-400)), 1, "level");
    ok(
        w.retract(edwin, "level", Value::I64(3), t(-300)),
        1,
        "level end",
    );
    ok(
        w.assert(edwin, "level", Value::I64(4), t(-300)),
        1,
        "level 4",
    );
    ok(
        w.assert(edwin, "weight", Value::f64(72.5).unwrap(), t(-400)),
        1,
        "weight",
    );
    // Greg's moves, with a prequel written last
    ok(
        w.assert(greg, "located_in", r(varrow), t(-300)),
        1,
        "greg varrow",
    );
    ok(
        w.retract(greg, "located_in", r(varrow), t(-250)),
        1,
        "greg leaves",
    );
    ok(
        w.assert(greg, "located_in", r(ashford), t(-250)),
        1,
        "greg ashford",
    );
    ok(
        w.retract(greg, "located_in", r(ashford), t(-200)),
        1,
        "greg leaves 2",
    );
    ok(
        w.assert(greg, "located_in", r(varrow), t(-200)),
        1,
        "greg back",
    );
    ok(
        w.assert(greg, "located_in", r(carnath), t(-400)),
        1,
        "prequel",
    );
    ok(
        w.retract(greg, "located_in", r(carnath), t(-350)),
        1,
        "prequel end",
    );
    // bidirectional
    ok(w.assert(edwin, "ally_of", r(mara), t(-208)), 1, "allies");
    ok(w.assert(mara, "ally_of", r(lisa), t(-100)), 1, "allies 2");
    ok(
        w.retract(mara, "ally_of", r(edwin), t(-50)),
        1,
        "alliance ends",
    );
    // inverse-named attribute
    ok(w.assert(edwin, "parent_of", r(lisa), t(-60)), 1, "parent");
    // an edge with properties
    ok(
        w.assert(edwin, "present", r(carnath), t(-300)),
        1,
        "present",
    );
    let edge: EntityId = w.edge_id(edwin, "present", carnath).unwrap().into();
    ok(
        w.assert(edge, "disguised", Value::Bool(true), t(-300)),
        1,
        "disguised",
    );
    ok(
        w.retract(edge, "disguised", Value::Bool(true), t(-280)),
        1,
        "undisguised",
    );
    ok(
        w.assert(edge, "disguised", Value::Bool(false), t(-280)),
        1,
        "disguised f",
    );
    ok(w.retract(edwin, "present", r(carnath), t(-250)), 1, "left");
    // an event
    ok(w.assert(edwin, "bought", r(sword), t(-208)), 1, "bought");
    // a multi-write txn
    let mut tx = w.begin();
    tx.assert(lisa, "name", s("Lisa"), t(-400)).unwrap();
    tx.assert(lisa, "located_in", r(ashford), t(-150)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 2);
    // corrections: delete, moved tick, moved value, bidirectional value move
    let x = one(ok(w.assert(mara, "located_in", r(carnath), t(-90)), 1, "x"));
    ok(w.correct(x, Correction::Delete), 1, "delete");
    let y = one(ok(
        w.retract(lisa, "located_in", r(ashford), t(-100)),
        1,
        "y",
    ));
    ok(
        w.correct(
            y,
            Correction::Replace {
                value: None,
                tick: t(-120),
                op: None,
                precision: None,
            },
        ),
        2,
        "move tick",
    );
    ok(
        w.retract(mara, "name", s("Mara"), t(-300)),
        1,
        "mara name end",
    );
    let z = one(ok(w.assert(mara, "name", s("Marra"), t(-300)), 1, "z"));
    ok(
        w.correct(
            z,
            Correction::Replace {
                value: Some(s("Mara Voss")),
                tick: None,
                op: None,
                precision: None,
            },
        ),
        2,
        "move value",
    );
    let q = one(ok(w.assert(edwin, "ally_of", r(greg), t(-30)), 1, "q"));
    ok(
        w.correct(
            q,
            Correction::Replace {
                value: Some(r(lisa)),
                tick: None,
                op: None,
                precision: None,
            },
        ),
        2,
        "bidirectional move",
    );
}

// ---------------------------------------------------------------- a tiny file world

struct Tiny {
    _dir: tempfile::TempDir,
    path: PathBuf,
    w: World,
}

/// tx1 greg, tx2 varrow, tx3 `Greg name "Greg" @1`, tx4 `Greg located_in
/// Varrow @1`, tx5 `retract Greg located_in Varrow @9`. tx3 is depended on
/// by nothing later; tx4 by tx5.
fn tiny() -> Tiny {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(greg, "name", s("Greg"), t(1)).unwrap();
    w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    w.retract(greg, "located_in", r(varrow), t(9)).unwrap();
    assert_eq!(w.latest_tx(), TxId(5));
    Tiny { _dir: dir, path, w }
}

fn tiny_mem() -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(greg, "name", s("Greg"), t(1)).unwrap();
    w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    w.retract(greg, "located_in", r(varrow), t(9)).unwrap();
    w
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

// ================================================================ clean controls

#[test]
fn an_empty_in_memory_world_is_clean() {
    let w = World::in_memory(schema()).unwrap();
    assert_clean(&doctor(&w, &opts()), "empty in-memory");
}

#[test]
fn an_empty_file_world_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let w = World::create(&path, schema()).unwrap();
    assert_clean(&doctor(&w, &opts()), "empty file world");
    assert_clean(&doctor_log(&path, &opts()).unwrap(), "empty file log");
}

#[test]
fn a_realistic_in_memory_world_is_clean() {
    let mut w = World::in_memory(schema()).unwrap();
    script(&mut w);
    assert_clean(&doctor(&w, &opts()), "in-memory script");
}

#[test]
fn a_realistic_file_world_is_clean_live_and_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    script(&mut w);
    assert_clean(&doctor(&w, &opts()), "file script, live");
    assert_clean(&doctor_log(&path, &opts()).unwrap(), "file script, log");
    drop(w);
    let w = World::open(&path).unwrap();
    assert_clean(&doctor(&w, &opts()), "file script, reopened");
    assert_clean(
        &doctor_log(&path, &opts()).unwrap(),
        "file script, reopened log",
    );
}

#[test]
fn doctor_options_canon_and_since_on_a_clean_world_stay_clean() {
    let mut w = World::in_memory(schema()).unwrap();
    script(&mut w);
    let latest = w.latest_tx();
    for n in [0, 1, latest.0 / 2, latest.0, latest.0 + 5] {
        let o = DoctorOptions {
            canon: Some(TxId(n)),
            since: Some(TxId(n)),
        };
        assert_clean(&doctor(&w, &o), &format!("canon/since {n}"));
    }
}

#[test]
fn doctor_is_read_only_and_works_while_the_world_holds_its_lock() {
    let mut tiny = tiny();
    script_on_file_extra(&mut tiny.w);
    let before_bytes = std::fs::read(&tiny.path).unwrap();
    let before_dir = all_files(tiny._dir.path());
    let before_dump = tiny.w.index_dump();
    let greg = tiny.w.entity_by_handle("greg").unwrap();
    let before_hist = tiny.w.history(greg, "located_in").unwrap();
    let before_tx = tiny.w.latest_tx();

    let r1 = doctor(&tiny.w, &opts());
    let r2 = doctor(&tiny.w, &opts());
    let r3 = doctor_log(&tiny.path, &opts()).unwrap();
    assert_clean(&r1, "first");
    assert_eq!(r1, r2, "doctor is deterministic");
    assert_eq!(r3, r1, "doctor_log agrees with doctor on a healthy file");

    assert_eq!(
        std::fs::read(&tiny.path).unwrap(),
        before_bytes,
        "log bytes"
    );
    assert_eq!(all_files(tiny._dir.path()), before_dir, "no files appear");
    assert_eq!(tiny.w.index_dump(), before_dump);
    assert_eq!(tiny.w.history(greg, "located_in").unwrap(), before_hist);
    assert_eq!(tiny.w.latest_tx(), before_tx);
    // the world is still writable (its lock was never taken from it)
    tiny.w.assert(greg, "level", Value::I64(1), t(2)).unwrap();
}

fn script_on_file_extra(w: &mut World) {
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "level", Value::I64(7), t(3)).unwrap();
}

#[test]
fn doctor_is_read_only_on_a_damaged_file_too() {
    // damage must be reported, never repaired or quarantined
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let sp = spans(&b);
    let last = *sp.last().unwrap();
    b.truncate(last.body_start + 3);
    std::fs::write(&tiny.path, &b).unwrap();
    let dir_before = all_files(tiny._dir.path());
    let rep = doctor(&tiny.w, &opts());
    assert!(!rep.is_clean());
    let rep2 = doctor_log(&tiny.path, &opts()).unwrap();
    assert!(!rep2.is_clean());
    assert_eq!(std::fs::read(&tiny.path).unwrap(), b, "file untouched");
    assert_eq!(
        all_files(tiny._dir.path()),
        dir_before,
        "no quarantine file"
    );
}

// ================================================================ damage after open

#[test]
fn a_flipped_body_byte_of_a_middle_txn_is_a_checksum_mismatch_with_that_tx() {
    let tiny = tiny();
    assert_clean(&doctor(&tiny.w, &opts()), "control");
    let mut b = std::fs::read(&tiny.path).unwrap();
    let sp = spans(&b);
    let victim = sp.iter().find(|s| s.tx == 3).copied().unwrap();
    b[victim.body_start + 6] ^= 0x01;
    std::fs::write(&tiny.path, &b).unwrap();
    let rep = doctor(&tiny.w, &opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(3)),
        "ChecksumMismatch tx 3",
    );
    // no other tx is blamed for a checksum
    assert!(!any(&rep, |f| matches!(f,
        Finding::ChecksumMismatch { tx, .. } if *tx != TxId(3))));
    // the same through doctor_log
    let rep2 = doctor_log(&tiny.path, &opts()).unwrap();
    assert_has(
        &rep2,
        |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(3)),
        "ChecksumMismatch tx 3 (doctor_log)",
    );
}

#[test]
fn a_wrong_commit_checksum_in_a_middle_txn_is_a_checksum_mismatch() {
    // body_crc and framing are valid (resealed): only the Commit checksum is wrong
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let victim = spans(&b).into_iter().find(|s| s.tx == 3).unwrap();
    let last_frame = *frame_spans(&b, victim).last().unwrap();
    assert_eq!(last_frame.2, 5, "the last frame is the Commit");
    b[last_frame.1 - 1] ^= 0x80;
    reseal(&mut b, victim);
    std::fs::write(&tiny.path, &b).unwrap();
    let rep = doctor(&tiny.w, &opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(3)),
        "ChecksumMismatch tx 3",
    );
}

#[test]
fn a_flipped_byte_in_the_last_txn_is_an_incomplete_tail_at_its_start() {
    // open() would cut a damaged LAST txn like a torn tail
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let last = *spans(&b).last().unwrap();
    b[last.body_start + 6] ^= 0x01;
    std::fs::write(&tiny.path, &b).unwrap();
    let rep = doctor(&tiny.w, &opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::IncompleteTail { offset } if *offset == last.start as u64),
        "IncompleteTail at the last txn's start",
    );
}

#[test]
fn a_file_truncated_mid_txn_is_an_incomplete_tail_at_that_txns_start() {
    for cut_into in [3usize, 10, BEGIN_LEN + 3] {
        let tiny = tiny();
        let mut b = std::fs::read(&tiny.path).unwrap();
        let last = *spans(&b).last().unwrap();
        b.truncate(last.start + cut_into);
        std::fs::write(&tiny.path, &b).unwrap();
        let rep = doctor(&tiny.w, &opts());
        assert_has(
            &rep,
            |f| matches!(f, Finding::IncompleteTail { offset } if *offset == last.start as u64),
            &format!("IncompleteTail (cut {cut_into} bytes into the txn)"),
        );
        let rep2 = doctor_log(&tiny.path, &opts()).unwrap();
        assert_has(
            &rep2,
            |f| matches!(f, Finding::IncompleteTail { offset } if *offset == last.start as u64),
            &format!("IncompleteTail via doctor_log (cut {cut_into})"),
        );
    }
}

#[test]
fn a_zero_filled_tail_is_an_incomplete_tail() {
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let end = b.len();
    b.extend_from_slice(&[0u8; 40]);
    std::fs::write(&tiny.path, &b).unwrap();
    let rep = doctor_log(&tiny.path, &opts()).unwrap();
    assert_has(
        &rep,
        |f| matches!(f, Finding::IncompleteTail { offset } if *offset == end as u64),
        "IncompleteTail at the end of the valid prefix",
    );
}

// ================================================================ crafted logs

struct Crafted {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    greg: u64,
    attr_name: u32,
    next_entry: u64,
    next_tx: u64,
    /// entry id of `Greg name "Greg" @1`
    name_fact: u64,
}

enum Commit {
    Good,
    Missing,
    BadChecksum,
}

/// genesis + greg + varrow + `Greg name "Greg" @1` (tx 3).
fn crafted() -> Crafted {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let greg;
    {
        let mut w = World::create(&path, schema()).unwrap();
        let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        w.create_entity("PLACE", Some("varrow")).unwrap();
        w.assert(g, "name", s("Greg"), t(1)).unwrap();
        assert_eq!(w.latest_tx(), TxId(3));
        greg = u64::from_be_bytes(g.to_bytes());
    }
    let log = read_log(&path).unwrap();
    let attr = |n: &str| {
        log.iter()
            .find_map(|e| match &e.body {
                EntryBody::DefineAttr { id, def } if def.name == n => Some(id.0),
                _ => None,
            })
            .unwrap()
    };
    let name_fact = log
        .iter()
        .rev()
        .find(|e| matches!(e.body, EntryBody::Fact { .. }))
        .unwrap()
        .id
        .0;
    Crafted {
        bytes: std::fs::read(&path).unwrap(),
        attr_name: attr("name"),
        next_entry: log.last().unwrap().id.0 + 1,
        next_tx: 4,
        name_fact,
        greg,
        path,
        _dir: dir,
    }
}

fn fact(entity: u64, attr: u32, tick: i64, retract: bool, v: &Value) -> Vec<u8> {
    let mut b = entity.to_le_bytes().to_vec();
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(u8::from(retract));
    b.push(0); // precision byte (v4): Exact
    b.extend_from_slice(&v.encode().unwrap());
    b
}

fn deleted(of: u64, rev: u32, entity: u64, attr: u32, tick: i64, v: &Value) -> Vec<u8> {
    let mut b = of.to_le_bytes().to_vec();
    b.extend_from_slice(&rev.to_le_bytes());
    b.extend_from_slice(&entity.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.extend_from_slice(&v.encode().unwrap());
    b
}

#[allow(clippy::too_many_arguments)]
fn revised(
    of: u64,
    rev: u32,
    entity: u64,
    attr: u32,
    tick: i64,
    retract: bool,
    v: &Value,
) -> Vec<u8> {
    let mut b = of.to_le_bytes().to_vec();
    b.extend_from_slice(&rev.to_le_bytes());
    b.extend_from_slice(&entity.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(u8::from(retract));
    b.push(0); // precision byte (v4): Exact
    b.extend_from_slice(&v.encode().unwrap());
    b
}

impl Crafted {
    /// Appends one txn (tx `tx`, first entry id `first`) and returns the
    /// entry ids it used (frames + a Commit unless missing).
    fn push_raw(&mut self, tx: u64, first: u64, entries: &[(u8, Vec<u8>)], commit: Commit) {
        let mut body = Vec::new();
        let mut id = first;
        for (k, p) in entries {
            body.extend(frame(*k, id, p));
            id += 1;
        }
        match commit {
            Commit::Missing => {}
            Commit::Good => {
                let c = commit_frame(&body, id, tx);
                body.extend(c);
                id += 1;
            }
            Commit::BadChecksum => {
                let mut c = commit_frame(&body, id, tx);
                let n = c.len();
                c[n - 1] ^= 0x55;
                body.extend(c);
                id += 1;
            }
        }
        self.bytes.extend(txn(tx, &body));
        self.next_entry = id;
        self.next_tx = tx + 1;
    }

    fn push(&mut self, entries: &[(u8, Vec<u8>)], commit: Commit) {
        let (tx, first) = (self.next_tx, self.next_entry);
        self.push_raw(tx, first, entries, commit);
    }

    fn write(&self) -> &Path {
        std::fs::write(&self.path, &self.bytes).unwrap();
        &self.path
    }

    fn report(&self, o: &DoctorOptions) -> Report {
        doctor_log(self.write(), o).unwrap()
    }

    /// a valid new fact: `retract Greg name "Greg" @5` (held since @1, so not a no-op;
    /// ends the value instead of adding a second `name`, which would be a real
    /// cardinality overlap warning)
    fn good_fact(&self) -> (u8, Vec<u8>) {
        (K_FACT, fact(self.greg, self.attr_name, 5, true, &s("Greg")))
    }

    fn name_key_deleted(&self, rev: u32, tick: i64) -> (u8, Vec<u8>) {
        (
            K_DELETED,
            deleted(
                self.name_fact,
                rev,
                self.greg,
                self.attr_name,
                tick,
                &s("Greg"),
            ),
        )
    }
}

fn entity_from_raw(raw: u64) -> EntityId {
    EntityId::from_bytes(raw.to_be_bytes())
}

#[test]
fn a_crafted_valid_extension_is_clean() {
    // control for every crafted log below: the same shape with nothing wrong
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::Good);
    assert_clean(&c.report(&opts()), "valid extra txn");
}

#[test]
fn a_txn_without_a_commit_is_a_missing_commit() {
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::Missing);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::MissingCommit { tx, .. } if *tx == TxId(4)),
        "MissingCommit tx 4",
    );
    // World::open refuses this very file: doctor is for exactly such files
    assert!(matches!(World::open(&c.path), Err(Error::Corrupt(_))));
}

#[test]
fn a_wrong_commit_checksum_in_a_crafted_log_is_a_checksum_mismatch() {
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::BadChecksum);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(4)),
        "ChecksumMismatch tx 4",
    );
}

#[test]
fn a_tx_that_skips_a_number_is_a_tx_gap() {
    let mut c = crafted();
    let f = c.good_fact();
    let first = c.next_entry;
    c.push_raw(5, first, &[f], Commit::Good); // expected 4
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::TxGap { expected, found } if *expected == TxId(4) && *found == TxId(5)),
        "TxGap{4, 5}",
    );
}

#[test]
fn an_entry_id_that_skips_is_an_entry_id_gap() {
    let mut c = crafted();
    let f = c.good_fact();
    let (tx, first) = (c.next_tx, c.next_entry);
    c.push_raw(tx, first + 3, &[f], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| {
            matches!(x, Finding::EntryIdGap { expected, found }
            if *expected == EntryId(first) && *found == EntryId(first + 3))
        },
        "EntryIdGap",
    );
}

#[test]
fn a_fact_on_an_undefined_entity_is_a_dangling_id() {
    let mut c = crafted();
    let ghost = c.greg + 50; // same node type, sequence never defined
    let first = c.next_entry;
    let f = (K_FACT, fact(ghost, c.attr_name, 5, false, &s("Ghost")));
    c.push(&[f], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| {
            matches!(x, Finding::DanglingId { entry, id }
            if *entry == EntryId(first) && *id == entity_from_raw(ghost))
        },
        "DanglingId",
    );
    assert!(World::open(&c.path).is_err());
}

#[test]
fn a_fact_on_an_entity_defined_in_the_same_txn_is_not_dangling() {
    // control: DefineEntity first, then the fact, in one txn
    let mut c = crafted();
    let newbie = c.greg + 1; // greg's type, next sequence
    let mut def = newbie.to_le_bytes().to_vec();
    def.push(0); // no handle
    let f = (K_FACT, fact(newbie, c.attr_name, 5, false, &s("Newbie")));
    c.push(&[(K_DEFINE_ENTITY, def), f], Commit::Good);
    assert_clean(&c.report(&opts()), "define then use");
}

#[test]
fn a_fact_on_an_undefined_attribute_is_a_dangling_attr() {
    let mut c = crafted();
    let first = c.next_entry;
    let f = (K_FACT, fact(c.greg, 9999, 5, false, &s("x")));
    c.push(&[f], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| {
            matches!(x, Finding::DanglingAttr { entry, attr }
            if *entry == EntryId(first) && *attr == AttrId(9999))
        },
        "DanglingAttr",
    );
}

#[test]
fn a_valid_delete_of_the_name_fact_is_clean() {
    // control for the BadCorrection shapes
    let mut c = crafted();
    let d = c.name_key_deleted(1, 1);
    c.push(&[d], Commit::Good);
    assert_clean(&c.report(&opts()), "valid delete");
}

#[test]
fn a_deleted_whose_of_is_missing_is_a_bad_correction() {
    let mut c = crafted();
    let first = c.next_entry;
    let d = (
        K_DELETED,
        deleted(99_999, 1, c.greg, c.attr_name, 1, &s("Greg")),
    );
    c.push(&[d], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::BadCorrection { entry, .. } if *entry == EntryId(first)),
        "BadCorrection (missing `of`)",
    );
}

#[test]
fn a_deleted_that_points_at_a_non_fact_is_a_bad_correction() {
    let mut c = crafted();
    let first = c.next_entry;
    // entry#1 is a schema/genesis DEFINE
    let d = (K_DELETED, deleted(1, 1, c.greg, c.attr_name, 1, &s("Greg")));
    c.push(&[d], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::BadCorrection { entry, .. } if *entry == EntryId(first)),
        "BadCorrection (non-fact `of`)",
    );
}

#[test]
fn a_deleted_at_the_wrong_key_is_a_bad_correction() {
    let mut c = crafted();
    let first = c.next_entry;
    let d = c.name_key_deleted(1, 2); // the fact is at tick 1
    c.push(&[d], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::BadCorrection { entry, .. } if *entry == EntryId(first)),
        "BadCorrection (wrong tick)",
    );
    // and a wrong value
    let mut c = crafted();
    let first = c.next_entry;
    let d = (
        K_DELETED,
        deleted(c.name_fact, 1, c.greg, c.attr_name, 1, &s("Not Greg")),
    );
    c.push(&[d], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::BadCorrection { entry, .. } if *entry == EntryId(first)),
        "BadCorrection (wrong value)",
    );
}

#[test]
fn a_second_delete_of_an_already_deleted_chain_is_a_bad_correction() {
    let mut c = crafted();
    let d1 = c.name_key_deleted(1, 1);
    c.push(&[d1], Commit::Good);
    let first = c.next_entry;
    let d2 = c.name_key_deleted(2, 1);
    c.push(&[d2], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::BadCorrection { entry, .. } if *entry == EntryId(first)),
        "BadCorrection (chain already deleted)",
    );
}

#[test]
fn a_revision_that_skips_a_number_is_a_revision_skip() {
    let mut c = crafted();
    let first = c.next_entry;
    // Revised at the same key, op changed, but rev 2 first
    let rv = (
        K_REVISED,
        revised(c.name_fact, 2, c.greg, c.attr_name, 1, true, &s("Greg")),
    );
    c.push(&[rv], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| {
            matches!(x, Finding::RevisionSkip { entry, of, expected, found }
            if *entry == EntryId(first) && *of == EntryId(c.name_fact)
                && *expected == 1 && *found == 2)
        },
        "RevisionSkip{expected 1, found 2}",
    );
}

#[test]
fn a_revision_with_the_right_number_is_clean() {
    // control: rev 1 at the same key with the op changed
    let mut c = crafted();
    let rv = (
        K_REVISED,
        revised(c.name_fact, 1, c.greg, c.attr_name, 1, true, &s("Greg")),
    );
    c.push(&[rv], Commit::Good);
    assert_clean(&c.report(&opts()), "valid revision");
}

#[test]
fn a_replay_rule_violation_that_is_none_of_the_named_ones_is_an_invalid_entry() {
    // retract of a value that isn't held at its tick: a live write would be a no-op
    let mut c = crafted();
    let first = c.next_entry;
    let f = (K_FACT, fact(c.greg, c.attr_name, 5, true, &s("Nobody")));
    c.push(&[f], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::InvalidEntry { entry, .. } if *entry == EntryId(first)),
        "InvalidEntry",
    );
}

#[test]
fn scanning_continues_past_a_bad_txn_with_a_valid_begin_record() {
    // tx 4: wrong commit checksum; tx 5: a fact on an undefined attribute.
    // Both problems are reported.
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::BadChecksum);
    let first5 = c.next_entry;
    let bad = (K_FACT, fact(c.greg, 9999, 6, false, &s("x")));
    c.push(&[bad], Commit::Good);
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(4)),
        "ChecksumMismatch tx 4",
    );
    assert!(
        any(&rep, |f| matches!(f, Finding::DanglingAttr { entry, attr }
            if *entry == EntryId(first5) && *attr == AttrId(9999))),
        "the later problem is reported too: {:#?}",
        rep.findings
    );
}

#[test]
fn scanning_continues_past_a_txn_without_a_commit() {
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::Missing);
    let first5 = c.next_entry;
    let ghost = c.greg + 50;
    let bad = (K_FACT, fact(ghost, c.attr_name, 6, false, &s("x")));
    c.push(&[bad], Commit::Good);
    let rep = c.report(&opts());
    assert!(any(
        &rep,
        |f| matches!(f, Finding::MissingCommit { tx, .. } if *tx == TxId(4))
    ));
    assert!(
        any(
            &rep,
            |f| matches!(f, Finding::DanglingId { entry, .. } if *entry == EntryId(first5))
        ),
        "{:#?}",
        rep.findings
    );
}

#[test]
fn doctor_log_never_modifies_the_file_or_its_directory() {
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::BadChecksum);
    c.write();
    let dir_before = all_files(c._dir.path());
    let before = std::fs::read(&c.path).unwrap();
    let rep = doctor_log(&c.path, &opts()).unwrap();
    assert!(!rep.is_clean());
    assert_eq!(std::fs::read(&c.path).unwrap(), before);
    assert_eq!(all_files(c._dir.path()), dir_before);
}

#[test]
fn doctor_log_on_a_clean_file_is_clean() {
    let tiny = tiny();
    assert_clean(&doctor_log(&tiny.path, &opts()).unwrap(), "tiny");
}

#[test]
fn doctor_log_on_a_non_kala_file_is_bad_magic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-kala");
    std::fs::write(&path, b"this is definitely not a kala log file, no sir").unwrap();
    assert_eq!(doctor_log(&path, &opts()), Err(Error::BadMagic));
}

#[test]
fn doctor_log_errs_on_other_files_it_cannot_scan() {
    let dir = tempfile::tempdir().unwrap();
    // missing file: Io
    let missing = dir.path().join("nope");
    assert!(matches!(
        doctor_log(&missing, &opts()),
        Err(Error::Io { .. })
    ));
    // a truncated header
    let short = dir.path().join("short");
    let full = common::header(4);
    std::fs::write(&short, &full[..5]).unwrap();
    assert!(doctor_log(&short, &opts()).is_err());
    // an old format version
    let old = dir.path().join("old");
    std::fs::write(&old, common::header(2)).unwrap();
    assert_eq!(doctor_log(&old, &opts()), Err(Error::UnsupportedVersion(2)));
    let v3 = dir.path().join("v3");
    std::fs::write(&v3, common::header(3)).unwrap();
    assert_eq!(doctor_log(&v3, &opts()), Err(Error::UnsupportedVersion(3)));
}

// ================================================================ index tampering

const INDEXES: [IndexName; 8] = [
    IndexName::E,
    IndexName::R,
    IndexName::ELatest,
    IndexName::RLatest,
    IndexName::V,
    IndexName::VLatest,
    IndexName::VFold,
    IndexName::VFoldLatest,
];

fn mismatch_on(rep: &Report, which: IndexName) -> bool {
    any(
        rep,
        |f| matches!(f, Finding::IndexMismatch { index, .. } if *index == which),
    )
}

/// the tampered index is reported, and no other
fn assert_only_mismatch(rep: &Report, which: IndexName, what: &str) {
    assert_has(
        rep,
        |f| matches!(f, Finding::IndexMismatch { index, .. } if *index == which),
        what,
    );
    for other in INDEXES {
        if other != which {
            assert!(
                !mismatch_on(rep, other),
                "{what}: {other:?} was not touched but is reported: {:#?}",
                rep.findings
            );
        }
    }
}

/// a well-formed E key (value Str) the log never produced
fn phantom_e_key(greg: EntityId, attr: AttrId, with_tx: bool) -> Vec<u8> {
    let mut k = vec![b'E'];
    k.extend_from_slice(&greg.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(&s("Phantom").encode().unwrap());
    k.extend_from_slice(&Tick(99).encode_inverted());
    if with_tx {
        k.extend_from_slice(&(!2u64).to_be_bytes());
    }
    k
}

/// a well-formed V key (value Str) the log never produced
fn phantom_v_key(attr: AttrId, entity: EntityId, with_tx: bool) -> Vec<u8> {
    let mut k = vec![b'V'];
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(&s("Phantom").encode().unwrap());
    k.extend_from_slice(&entity.to_bytes());
    k.extend_from_slice(&Tick(99).encode_inverted());
    if with_tx {
        k.extend_from_slice(&(!2u64).to_be_bytes());
    }
    k
}

/// a well-formed VFold key (folded text, then the original Str encoding)
/// the log never produced
fn phantom_vfold_key(attr: AttrId, entity: EntityId, with_tx: bool) -> Vec<u8> {
    let mut k = vec![b'F'];
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(&s("phantom").encode().unwrap());
    k.extend_from_slice(&s("Phantom").encode().unwrap());
    k.extend_from_slice(&entity.to_bytes());
    k.extend_from_slice(&Tick(99).encode_inverted());
    if with_tx {
        k.extend_from_slice(&(!2u64).to_be_bytes());
    }
    k
}

/// a well-formed R key the log never produced
fn phantom_r_key(target: EntityId, attr: AttrId, source: EntityId, with_tx: bool) -> Vec<u8> {
    let mut k = vec![b'R'];
    k.extend_from_slice(&target.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(&source.to_bytes());
    k.extend_from_slice(&Tick(99).encode_inverted());
    if with_tx {
        k.extend_from_slice(&(!2u64).to_be_bytes());
    }
    k
}

/// ids of the tiny world, read from its index dump (E rows: name is the
/// only str attribute, located_in the only ref one).
struct Ids {
    greg: EntityId,
    varrow: EntityId,
    name: AttrId,
    located_in: AttrId,
}

fn tiny_ids(w: &World) -> Ids {
    let d = w.index_dump();
    let greg = w.entity_by_handle("greg").unwrap();
    let varrow = w.entity_by_handle("varrow").unwrap();
    let name =
        d.e.iter()
            .find(|row| matches!(row.2, Value::Str(_)))
            .map(|row| row.1)
            .unwrap();
    let located_in =
        d.e.iter()
            .find(|row| matches!(row.2, Value::Ref(_)))
            .map(|row| row.1)
            .unwrap();
    Ids {
        greg,
        varrow,
        name,
        located_in,
    }
}

#[test]
fn the_tiny_worlds_are_clean_before_any_tampering() {
    assert_clean(&doctor(&tiny_mem(), &opts()), "in-memory");
    assert_clean(&doctor(&tiny().w, &opts()), "file");
}

#[test]
fn removing_a_row_from_each_index_is_an_index_mismatch_for_that_index() {
    for which in INDEXES {
        // the tiny world has one string fact: one V / V_latest row
        let nths: &[usize] = if matches!(
            which,
            IndexName::V | IndexName::VLatest | IndexName::VFold | IndexName::VFoldLatest
        ) {
            &[0]
        } else {
            &[0, 1]
        };
        for &nth in nths {
            let mut w = tiny_mem();
            w.tamper_index(Tamper::RemoveRow { index: which, nth });
            let rep = doctor(&w, &opts());
            assert_only_mismatch(&rep, which, &format!("RemoveRow {which:?} #{nth}"));
        }
    }
}

#[test]
fn flipping_the_op_of_a_row_in_each_index_is_an_index_mismatch_for_that_index() {
    for which in INDEXES {
        // the tiny world has one string fact: one V / V_latest row
        let nths: &[usize] = if matches!(
            which,
            IndexName::V | IndexName::VLatest | IndexName::VFold | IndexName::VFoldLatest
        ) {
            &[0]
        } else {
            &[0, 1]
        };
        for &nth in nths {
            let mut w = tiny_mem();
            w.tamper_index(Tamper::FlipOp { index: which, nth });
            let rep = doctor(&w, &opts());
            assert_only_mismatch(&rep, which, &format!("FlipOp {which:?} #{nth}"));
        }
    }
}

#[test]
fn inserting_a_row_the_log_never_produced_is_an_index_mismatch_for_that_index() {
    for which in INDEXES {
        let mut w = tiny_mem();
        let ids = tiny_ids(&w);
        let key = match which {
            IndexName::E => phantom_e_key(ids.greg, ids.name, true),
            IndexName::ELatest => phantom_e_key(ids.greg, ids.name, false),
            IndexName::R => phantom_r_key(ids.varrow, ids.located_in, ids.greg, true),
            IndexName::RLatest => phantom_r_key(ids.varrow, ids.located_in, ids.greg, false),
            IndexName::V => phantom_v_key(ids.name, ids.greg, true),
            IndexName::VLatest => phantom_v_key(ids.name, ids.greg, false),
            IndexName::VFold => phantom_vfold_key(ids.name, ids.greg, true),
            IndexName::VFoldLatest => phantom_vfold_key(ids.name, ids.greg, false),
        };
        w.tamper_index(Tamper::InsertRawKey { index: which, key });
        let rep = doctor(&w, &opts());
        assert_only_mismatch(&rep, which, &format!("InsertRawKey {which:?}"));
        assert!(
            !any(&rep, |f| matches!(f, Finding::IndexDecode { .. })),
            "a well-formed key must decode: {:#?}",
            rep.findings
        );
    }
}

#[test]
fn tampering_is_found_on_a_file_world_and_on_a_realistic_world() {
    for which in INDEXES {
        let mut tiny = tiny();
        tiny.w.tamper_index(Tamper::RemoveRow {
            index: which,
            nth: 0,
        });
        assert_only_mismatch(&doctor(&tiny.w, &opts()), which, &format!("file {which:?}"));

        let mut w = World::in_memory(schema()).unwrap();
        script(&mut w);
        assert_clean(&doctor(&w, &opts()), "control");
        w.tamper_index(Tamper::FlipOp {
            index: which,
            nth: 2,
        });
        assert_only_mismatch(&doctor(&w, &opts()), which, &format!("script {which:?}"));
    }
}

#[test]
fn an_undecodable_raw_key_is_an_index_decode_and_does_not_panic() {
    let garbage: Vec<Vec<u8>> = vec![
        b"E\x00garbage".to_vec(),
        b"R\x00garbage".to_vec(),
        b"E".to_vec(),
        b"R".to_vec(),
        b"Z".to_vec(),
        vec![],
        vec![0xFF; 40],
        // right length and prefix for an E key, but the value part is junk
        {
            let mut k = vec![b'E'];
            k.extend_from_slice(&[0u8; 12]);
            k.push(0xEE);
            k.extend_from_slice(&[0u8; 16]);
            k
        },
        // an R-shaped key that is one byte short
        {
            let mut k = vec![b'R'];
            k.extend_from_slice(&[1u8; 8 + 4 + 8 + 16 - 1]);
            k
        },
    ];
    for which in INDEXES {
        for key in &garbage {
            let mut w = tiny_mem();
            w.tamper_index(Tamper::InsertRawKey {
                index: which,
                key: key.clone(),
            });
            let rep = doctor(&w, &opts());
            assert_has(
                &rep,
                |f| matches!(f, Finding::IndexDecode { index, key: k } if *index == which && k == key),
                &format!("IndexDecode {which:?} {key:?}"),
            );
        }
    }
}

#[test]
fn index_dump_skips_an_undecodable_key_instead_of_panicking() {
    let mut w = tiny_mem();
    let before = w.index_dump();
    assert!(!before.e.is_empty() && !before.r.is_empty());
    for (which, key) in [
        (IndexName::E, b"E\x00garbage".to_vec()),
        (IndexName::R, b"R\x00garbage".to_vec()),
        (IndexName::E, vec![]),
        (IndexName::R, vec![0xFF; 40]),
    ] {
        w.tamper_index(Tamper::InsertRawKey { index: which, key });
    }
    assert_eq!(w.index_dump(), before, "garbage rows are skipped");
    // and the reads still work
    let greg = w.entity_by_handle("greg").unwrap();
    assert!(!w.history(greg, "located_in").unwrap().is_empty());
}

#[test]
fn doctor_reports_garbage_keys_without_losing_a_real_mismatch() {
    let mut w = tiny_mem();
    w.tamper_index(Tamper::InsertRawKey {
        index: IndexName::E,
        key: b"E\x00garbage".to_vec(),
    });
    w.tamper_index(Tamper::RemoveRow {
        index: IndexName::R,
        nth: 0,
    });
    let rep = doctor(&w, &opts());
    assert!(any(
        &rep,
        |f| matches!(f, Finding::IndexDecode { index, .. } if *index == IndexName::E)
    ));
    assert!(mismatch_on(&rep, IndexName::R));
}

// ================================================================ since

#[test]
fn since_hides_a_damaged_txn_at_or_before_it_and_without_since_it_is_reported() {
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let victim = spans(&b).into_iter().find(|s| s.tx == 3).unwrap();
    b[victim.body_start + 6] ^= 0x01;
    std::fs::write(&tiny.path, &b).unwrap();

    let is_tx3 = |f: &Finding| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(3));
    // without since: reported
    assert!(any(&doctor(&tiny.w, &opts()), is_tx3));
    assert!(any(&doctor_log(&tiny.path, &opts()).unwrap(), is_tx3));
    // since 2 (< 3): reported
    assert!(any(&doctor(&tiny.w, &since(2)), is_tx3));
    assert!(any(&doctor_log(&tiny.path, &since(2)).unwrap(), is_tx3));
    // since 3 and later: not reported, and nothing else is either
    for n in [3, 4, 5, 50] {
        assert_clean(&doctor(&tiny.w, &since(n)), &format!("doctor since {n}"));
        assert_clean(
            &doctor_log(&tiny.path, &since(n)).unwrap(),
            &format!("doctor_log since {n}"),
        );
    }
}

#[test]
fn since_reports_a_crafted_problem_only_in_txns_after_it() {
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::BadChecksum); // tx 4
    let rep = c.report(&since(3));
    assert!(any(
        &rep,
        |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(4))
    ));
    assert_clean(&c.report(&since(4)), "since 4 hides tx 4");
    assert_clean(&c.report(&since(9)), "since 9 hides tx 4");
    assert!(!c.report(&opts()).is_clean());

    // a dangling attr in tx 5 only: visible from since 4, hidden from since 5
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::Good);
    let bad = (K_FACT, fact(c.greg, 9999, 6, false, &s("x")));
    c.push(&[bad], Commit::Good);
    assert!(any(&c.report(&since(4)), |f| matches!(
        f,
        Finding::DanglingAttr { .. }
    )));
    assert_clean(&c.report(&since(5)), "since 5 hides tx 5");
    assert!(any(&c.report(&opts()), |f| matches!(
        f,
        Finding::DanglingAttr { .. }
    )));
}

#[test]
fn since_filters_index_rows_by_their_tx() {
    // remove an E row written by tx 3: reported for since < 3, hidden for since >= 3
    let mut w = tiny_mem();
    let dump = w.index_dump();
    let nth = dump.e.iter().position(|row| row.4 == TxId(3)).unwrap();
    w.tamper_index(Tamper::RemoveRow {
        index: IndexName::E,
        nth,
    });
    assert!(mismatch_on(&doctor(&w, &opts()), IndexName::E));
    assert!(mismatch_on(&doctor(&w, &since(2)), IndexName::E));
    assert_clean(&doctor(&w, &since(3)), "since 3");
    assert_clean(&doctor(&w, &since(4)), "since 4");

    // the same row of a later tx (5) is still visible at since 4
    let mut w = tiny_mem();
    let dump = w.index_dump();
    let nth = dump.e.iter().position(|row| row.4 == TxId(5)).unwrap();
    w.tamper_index(Tamper::RemoveRow {
        index: IndexName::E,
        nth,
    });
    assert!(mismatch_on(&doctor(&w, &since(4)), IndexName::E));
    assert_clean(&doctor(&w, &since(5)), "since 5");
}

#[test]
fn since_filters_the_latest_index_by_the_tx_of_the_row() {
    // the latest-revision keys carry no tx: they are judged by the tx of the
    // entry they currently hold. Remove ELatest row for tx 3's fact.
    let mut w = tiny_mem();
    let dump = w.index_dump();
    let name_row = dump.e.iter().find(|row| row.4 == TxId(3)).unwrap().clone();
    // ELatest rows are in key order like E's, and with no corrections they
    // are the same keys minus tx
    let nth = dump
        .e
        .iter()
        .position(|row| {
            row.0 == name_row.0 && row.1 == name_row.1 && row.2 == name_row.2 && row.3 == name_row.3
        })
        .unwrap();
    w.tamper_index(Tamper::RemoveRow {
        index: IndexName::ELatest,
        nth,
    });
    assert!(mismatch_on(&doctor(&w, &opts()), IndexName::ELatest));
    assert!(mismatch_on(&doctor(&w, &since(2)), IndexName::ELatest));
    assert_clean(&doctor(&w, &since(3)), "since 3");
}

#[test]
fn a_clean_world_with_since_zero_is_clean() {
    let mut w = World::in_memory(schema()).unwrap();
    script(&mut w);
    assert_clean(&doctor(&w, &since(0)), "since 0");
}

// ================================================================ a spent world is still doctor-checkable

#[test]
fn doctor_survives_a_world_whose_log_writer_failed() {
    let mut tiny = tiny();
    let greg = tiny.w.entity_by_handle("greg").unwrap();
    tiny.w.inject_log_fault(LogFault::FailNextSync);
    assert!(tiny.w.assert(greg, "level", Value::I64(1), t(2)).is_err());
    // the failed commit left nothing behind: file and memory still agree
    assert_clean(&doctor(&tiny.w, &opts()), "after a failed sync");
}

// ================================================================ slice 3 item 8, confirm round

/// every finding of a damaged-log report is an integrity Error, and there is one
fn assert_errors_only(rep: &Report, what: &str) {
    assert!(!rep.is_clean(), "{what}: expected findings");
    assert!(!rep.errors().is_empty(), "{what}: expected an error");
    assert!(rep.warnings().is_empty(), "{what}: {:#?}", rep.findings);
}

#[test]
fn a_damaged_genesis_body_is_a_checksum_mismatch_for_tx_0() {
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let genesis = spans(&b).into_iter().find(|s| s.tx == 0).unwrap();
    b[genesis.body_start + 6] ^= 0x01;
    std::fs::write(&tiny.path, &b).unwrap();
    for (label, rep) in [
        ("doctor", doctor(&tiny.w, &opts())),
        ("doctor_log", doctor_log(&tiny.path, &opts()).unwrap()),
    ] {
        assert_has(
            &rep,
            |f| matches!(f, Finding::ChecksumMismatch { tx, .. } if *tx == TxId(0)),
            &format!("{label}: ChecksumMismatch tx 0"),
        );
        assert_errors_only(&rep, label);
    }
}

#[test]
fn a_damaged_genesis_begin_record_is_reported_without_panicking() {
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let genesis = spans(&b).into_iter().find(|s| s.tx == 0).unwrap();
    b[genesis.start + 3] ^= 0x40; // begin_crc now fails, bytes after are not zero
    std::fs::write(&tiny.path, &b).unwrap();
    assert_errors_only(&doctor(&tiny.w, &opts()), "doctor");
    assert_errors_only(&doctor_log(&tiny.path, &opts()).unwrap(), "doctor_log");
}

#[test]
fn a_repeated_tx_is_a_tx_gap() {
    let mut c = crafted();
    let f = c.good_fact();
    let first = c.next_entry;
    c.push_raw(3, first, &[f], Commit::Good); // tx 3 again; 4 expected
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::TxGap { expected, found } if *expected == TxId(4) && *found == TxId(3)),
        "TxGap{expected 4, found 3}",
    );
}

#[test]
fn a_backwards_tx_is_a_tx_gap() {
    let mut c = crafted();
    let f = c.good_fact();
    c.push(&[f], Commit::Good); // tx 4
    let f2 = (K_FACT, fact(c.greg, c.attr_name, 6, false, &s("Gregory")));
    let first = c.next_entry;
    c.push_raw(2, first, &[f2], Commit::Good); // 5 expected
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |f| matches!(f, Finding::TxGap { expected, found } if *expected == TxId(5) && *found == TxId(2)),
        "TxGap{expected 5, found 2}",
    );
}

#[test]
fn backwards_entry_ids_are_an_entry_id_gap() {
    let mut c = crafted();
    let f = c.good_fact();
    let (tx, expected) = (c.next_tx, c.next_entry);
    c.push_raw(tx, 2, &[f], Commit::Good); // ids restart at 2
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| {
            matches!(x, Finding::EntryIdGap { expected: e, found }
            if *e == EntryId(expected) && *found == EntryId(2))
        },
        "EntryIdGap{expected next, found 2}",
    );
}

#[test]
fn a_commit_whose_tx_differs_from_its_begin_records_is_an_invalid_entry() {
    let mut c = crafted();
    let (tx, first) = (c.next_tx, c.next_entry);
    let f = c.good_fact();
    let mut body = frame(f.0, first, &f.1);
    let commit = commit_frame(&body, first + 1, tx + 50); // valid checksum, wrong tx
    body.extend(commit);
    c.bytes.extend(txn(tx, &body));
    let rep = c.report(&opts());
    assert_has(
        &rep,
        |x| matches!(x, Finding::InvalidEntry { .. }),
        "InvalidEntry (Commit tx differs)",
    );
    assert_errors_only(&rep, "commit tx");
    assert!(World::open(&c.path).is_err());
}

#[test]
fn a_correction_pointing_at_an_entry_replay_refused_is_a_bad_correction() {
    for variant in ["deleted", "revised"] {
        let mut c = crafted();
        // tx 4: a retract of a value that isn't held: replay refuses it
        let refused = c.next_entry;
        let bad = (K_FACT, fact(c.greg, c.attr_name, 5, true, &s("Nobody")));
        c.push(&[bad], Commit::Good);
        // tx 5: a correction (valid attribute, so nothing else is wrong
        // with it) of that entry
        let correction_entry = c.next_entry;
        let corr = if variant == "deleted" {
            (
                K_DELETED,
                deleted(refused, 1, c.greg, c.attr_name, 5, &s("Nobody")),
            )
        } else {
            (
                K_REVISED,
                revised(refused, 1, c.greg, c.attr_name, 5, false, &s("Nobody")),
            )
        };
        c.push(&[corr], Commit::Good);
        let rep = c.report(&opts());
        assert_has(
            &rep,
            |x| matches!(x, Finding::BadCorrection { entry, .. } if *entry == EntryId(correction_entry)),
            &format!("BadCorrection ({variant} of a refused entry)"),
        );
        assert!(any(
            &rep,
            |x| matches!(x, Finding::InvalidEntry { entry, .. } if *entry == EntryId(refused))
        ));
    }
}

#[test]
fn a_damaged_begin_record_mid_file_stops_the_scan_with_a_finding() {
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let victim = spans(&b).into_iter().find(|s| s.tx == 3).unwrap();
    b[victim.start + 2] ^= 0x10; // inside the tx field; begin_crc fails
    std::fs::write(&tiny.path, &b).unwrap();
    assert_errors_only(&doctor(&tiny.w, &opts()), "doctor");
    let rep = doctor_log(&tiny.path, &opts()).unwrap();
    assert_errors_only(&rep, "doctor_log");
    // txns before the damage are fine: nothing is blamed on tx 1 or 2
    assert!(!any(&rep, |f| matches!(f,
        Finding::ChecksumMismatch { tx, .. } if *tx < TxId(3))));
}

#[test]
fn an_unknown_record_tag_stops_the_scan_with_a_finding() {
    // at the end of the file
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    b.extend(common::begin_raw(0x09, 6, 0, 0));
    b.extend_from_slice(&[7u8; 10]);
    std::fs::write(&tiny.path, &b).unwrap();
    assert_errors_only(&doctor(&tiny.w, &opts()), "doctor, tail");
    assert_errors_only(
        &doctor_log(&tiny.path, &opts()).unwrap(),
        "doctor_log, tail",
    );

    // in the middle: tx 3's begin record gets tag 0x09 (crc fixed)
    let tiny = self::tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let victim = spans(&b).into_iter().find(|s| s.tx == 3).unwrap();
    let len = (victim.end - victim.body_start) as u64;
    let crc = common::crc(&b[victim.body_start..victim.end]);
    let begin = common::begin_raw(0x09, 3, len, crc);
    b[victim.start..victim.body_start].copy_from_slice(&begin);
    std::fs::write(&tiny.path, &b).unwrap();
    assert_errors_only(&doctor(&tiny.w, &opts()), "doctor, middle");
    assert_errors_only(
        &doctor_log(&tiny.path, &opts()).unwrap(),
        "doctor_log, middle",
    );
}

#[test]
fn since_combined_with_an_incomplete_tail_judges_the_tail_by_its_tx() {
    // the last txn (tx 5) is torn: it is the would-be tx 5
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    let last = *spans(&b).last().unwrap();
    assert_eq!(last.tx, 5);
    b.truncate(last.body_start + 3);
    std::fs::write(&tiny.path, &b).unwrap();
    let is_tail = |f: &Finding| matches!(f, Finding::IncompleteTail { .. });
    for n in [0, 3, 4] {
        assert!(
            any(&doctor_log(&tiny.path, &since(n)).unwrap(), is_tail),
            "since {n} reports the torn tx 5"
        );
        assert!(any(&doctor(&tiny.w, &since(n)), is_tail), "since {n}");
    }
    for n in [5, 6, 50] {
        assert_clean(
            &doctor_log(&tiny.path, &since(n)).unwrap(),
            &format!("doctor_log since {n}"),
        );
    }
}

#[test]
fn since_combined_with_a_zero_filled_tail_judges_it_as_the_next_tx() {
    let tiny = tiny();
    let mut b = std::fs::read(&tiny.path).unwrap();
    b.extend_from_slice(&[0u8; 40]); // the would-be tx 6
    std::fs::write(&tiny.path, &b).unwrap();
    let is_tail = |f: &Finding| matches!(f, Finding::IncompleteTail { .. });
    for n in [0, 4, 5] {
        assert!(
            any(&doctor_log(&tiny.path, &since(n)).unwrap(), is_tail),
            "since {n}"
        );
    }
    for n in [6, 7] {
        assert_clean(
            &doctor_log(&tiny.path, &since(n)).unwrap(),
            &format!("since {n}"),
        );
    }
}

#[test]
fn tamper_index_past_the_end_is_a_no_op() {
    for which in INDEXES {
        for nth in [10_000usize, usize::MAX] {
            let mut w = tiny_mem();
            let before = w.index_dump();
            w.tamper_index(Tamper::RemoveRow { index: which, nth });
            w.tamper_index(Tamper::FlipOp { index: which, nth });
            assert_eq!(w.index_dump(), before, "{which:?} #{nth}");
            assert_clean(&doctor(&w, &opts()), &format!("{which:?} #{nth}"));
        }
    }
}

/// greg's name and location, each asserted and then deleted by a correction
fn world_with_deleted_rows() -> World {
    let mut w = tiny_mem();
    let greg = w.entity_by_handle("greg").unwrap();
    let varrow = w.entity_by_handle("varrow").unwrap();
    let n = one(w.assert(greg, "name", s("Gregor"), t(5)).unwrap());
    w.correct(n, Correction::Delete).unwrap();
    let l = one(w.assert(greg, "ally_of", r(varrow), t(5)).unwrap());
    w.correct(l, Correction::Delete).unwrap();
    w
}

#[test]
fn flipping_a_deleted_row_is_an_index_mismatch_for_that_index() {
    let w = world_with_deleted_rows();
    assert_clean(&doctor(&w, &opts()), "control");
    let dump = w.index_dump();
    let e_nth = dump
        .e
        .iter()
        .position(|row| row.5 == Mark::Deleted && matches!(row.2, Value::Str(_)))
        .expect("a DELETED row in E");
    let r_nth = dump
        .r
        .iter()
        .position(|row| row.5 == Mark::Deleted)
        .expect("a DELETED row in R");

    let mut w = world_with_deleted_rows();
    w.tamper_index(Tamper::FlipOp {
        index: IndexName::E,
        nth: e_nth,
    });
    assert_only_mismatch(&doctor(&w, &opts()), IndexName::E, "flip DELETED in E");

    let mut w = world_with_deleted_rows();
    w.tamper_index(Tamper::FlipOp {
        index: IndexName::R,
        nth: r_nth,
    });
    assert_only_mismatch(&doctor(&w, &opts()), IndexName::R, "flip DELETED in R");
}

// ---------------------------------------------------------------- doctor is clean after random writes

mod random_sequences {
    use super::*;
    use proptest::prelude::{ProptestConfig, Strategy, prop_assert, prop_oneof, proptest};
    use proptest::test_runner::RngSeed;

    /// every attribute is cardinality many and no edge gets properties, so
    /// no schema-rule warning can appear: the report must be EMPTY.
    fn schema() -> Schema {
        Schema::new()
            .node_type("CHARACTER")
            .node_type("PLACE")
            .attribute(AttrDef::new(
                "knows",
                ValueType::Ref,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(
                AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many)
                    .bidirectional(),
            )
            .attribute(AttrDef::new(
                "tag",
                ValueType::Str,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(AttrDef::new(
                "level",
                ValueType::I64,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(AttrDef::new(
                "bought",
                ValueType::Ref,
                Kind::Event,
                Cardinality::Many,
            ))
    }

    #[derive(Clone, Debug)]
    enum Step {
        Assert {
            who: u8,
            attr: u8,
            val: u8,
            tick: i8,
        },
        Retract {
            who: u8,
            attr: u8,
            val: u8,
            tick: i8,
        },
        Delete {
            pick: u8,
        },
        Replace {
            pick: u8,
            tick: Option<i8>,
            val: Option<u8>,
            op: Option<bool>,
        },
    }

    fn step() -> impl Strategy<Value = Step> {
        use proptest::option::of;
        prop_oneof![
            4 => (0u8..3, 0u8..5, 0u8..4, -6i8..6)
                .prop_map(|(who, attr, val, tick)| Step::Assert { who, attr, val, tick }),
            3 => (0u8..3, 0u8..5, 0u8..4, -6i8..6)
                .prop_map(|(who, attr, val, tick)| Step::Retract { who, attr, val, tick }),
            1 => (0u8..40).prop_map(|pick| Step::Delete { pick }),
            2 => (0u8..40, of(-6i8..6), of(0u8..4), of(proptest::bool::ANY))
                .prop_map(|(pick, tick, val, op)| Step::Replace { pick, tick, val, op }),
        ]
    }

    const ATTRS: [&str; 5] = ["knows", "ally_of", "tag", "level", "bought"];

    fn run(w: &mut World, steps: &[Step]) {
        let mut ents: Vec<EntityId> = Vec::new();
        for i in 0..3 {
            ents.push(
                w.create_entity("CHARACTER", Some(&format!("c{i}")))
                    .unwrap(),
            );
        }
        for i in 0..2 {
            ents.push(w.create_entity("PLACE", Some(&format!("p{i}"))).unwrap());
        }
        let value = |attr: u8, v: u8| match attr {
            0 | 1 | 4 => Value::Ref(ents[(v as usize + 1) % ents.len()]),
            2 => Value::Str(format!("t{v}")),
            _ => Value::I64(i64::from(v)),
        };
        let mut entries: Vec<EntryId> = Vec::new();
        for st in steps {
            let res = match *st {
                Step::Assert {
                    who,
                    attr,
                    val,
                    tick,
                } => w.assert(
                    ents[who as usize],
                    ATTRS[attr as usize],
                    value(attr, val),
                    Some(Tick(i64::from(tick))),
                ),
                Step::Retract {
                    who,
                    attr,
                    val,
                    tick,
                } => w.retract(
                    ents[who as usize],
                    ATTRS[attr as usize],
                    value(attr, val),
                    Some(Tick(i64::from(tick))),
                ),
                Step::Delete { pick } => {
                    if entries.is_empty() {
                        continue;
                    }
                    w.correct(entries[pick as usize % entries.len()], Correction::Delete)
                }
                Step::Replace {
                    pick,
                    tick,
                    val,
                    op,
                } => {
                    if entries.is_empty() {
                        continue;
                    }
                    // the value's type follows the corrected entry's attr
                    // only by luck: a wrong-type replace is a refused write
                    w.correct(
                        entries[pick as usize % entries.len()],
                        Correction::Replace {
                            value: val.map(|v| value(0, v)),
                            tick: tick.map(|x| Tick(i64::from(x))),
                            op: op.map(|b| if b { Op::Retract } else { Op::Assert }),
                            precision: None,
                        },
                    )
                }
            };
            if let Ok(res) = res {
                entries.extend(res.entries);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 40,
            rng_seed: RngSeed::Fixed(0x4b41_4c41_0008),
            failure_persistence: None,
            .. ProptestConfig::default()
        })]

        #[test]
        fn doctor_is_clean_after_random_writes_and_corrections(
            steps in proptest::collection::vec(step(), 1..40),
        ) {
            // in memory
            let mut w = World::in_memory(schema()).unwrap();
            run(&mut w, &steps);
            let rep = doctor(&w, &opts());
            prop_assert!(rep.is_clean(), "in-memory: {:#?}", rep.findings);

            // on file: live, by log, and after reopen
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("w.kala");
            let mut w = World::create(&path, schema()).unwrap();
            run(&mut w, &steps);
            let rep = doctor(&w, &opts());
            prop_assert!(rep.is_clean(), "file: {:#?}", rep.findings);
            let rep = doctor_log(&path, &opts()).unwrap();
            prop_assert!(rep.is_clean(), "file log: {:#?}", rep.findings);
            drop(w);
            let w = World::open(&path).unwrap();
            let rep = doctor(&w, &opts());
            prop_assert!(rep.is_clean(), "reopened: {:#?}", rep.findings);
        }
    }
}

// ---------------------------------------------------------------- unreadable log

/// A file world's log that can't be read at all (here: deleted after the world
/// was opened) is `Finding::LogUnreadable { reason }`, a `Severity::Error`,
/// not the earlier stopgap `InvalidEntry { entry: 0 }`.
fn unreadable_world_in(dir: &std::path::Path) -> (std::path::PathBuf, World) {
    let path = dir.join("w.kala");
    let mut w = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        )),
    )
    .unwrap();
    let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(g, "name", Value::Str("Greg".into()), Some(Tick(1)))
        .unwrap();
    (path, w)
}

#[test]
fn a_deleted_log_file_is_a_log_unreadable_error() {
    let dir = tempfile::tempdir().unwrap();
    let (path, w) = unreadable_world_in(dir.path());
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
    std::fs::remove_file(&path).unwrap();

    let rep = doctor(&w, &DoctorOptions::default());
    let hit = rep
        .findings
        .iter()
        .find(|f| matches!(f, Finding::LogUnreadable { .. }))
        .unwrap_or_else(|| panic!("expected LogUnreadable, got {:#?}", rep.findings));
    assert_eq!(hit.severity(), Severity::Error);
    match hit {
        Finding::LogUnreadable { reason } => assert!(!reason.is_empty()),
        _ => unreachable!(),
    }
    assert_eq!(rep.errors().len(), 1, "{:#?}", rep.findings);
    assert!(rep.warnings().is_empty());
    // the stopgap is gone
    assert!(
        !rep.findings
            .iter()
            .any(|f| matches!(f, Finding::InvalidEntry { .. })),
        "{:#?}",
        rep.findings
    );
    // and with `since` (a finding with no tx is always reported)
    let rep = doctor(
        &w,
        &DoctorOptions {
            since: Some(TxId(100)),
            ..DoctorOptions::default()
        },
    );
    assert!(
        rep.findings
            .iter()
            .any(|f| matches!(f, Finding::LogUnreadable { .. })),
        "{:#?}",
        rep.findings
    );
}
