//! Slice 4, item 0 follow-ups (slice-4-decisions.md "Item 0"):
//!  (b) `Error::RecoveryUnacknowledged` / `World::acknowledge_recovery`,
//!  (c) L4 (doctor_log on shifted genesis entry IDs never panics) and
//!      L5 (collecting replay and the world agree on EdgePropertyOnUnassertedEdge).
//!
//! API expected from the implementation (crate root):
//!   `Error::RecoveryUnacknowledged`                       (new variant)
//!   `World::acknowledge_recovery(&mut self)`              (no-op on any world without a Damaged cut)
//!
//! Open points flagged in the report (not guessed here): `Txn::commit` can only
//! be reached un-poisoned with an EMPTY transaction on an unacknowledged
//! world; the test pins that it fails with RecoveryUnacknowledged.
mod common;

use common::{frame_spans, spans};
use kala_core::*;
use std::path::{Path, PathBuf};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn open_ok(p: &Path) -> World {
    match World::open(p) {
        Ok(w) => w,
        Err(e) => panic!("open failed: {e:?}"),
    }
}

/// genesis + greg, varrow, name@1 (tx3), name@2 (tx4). Returns the entry of
/// the tx3 fact (survives a cut of tx4).
fn build() -> (tempfile::TempDir, PathBuf, EntryId) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let _v = w.create_entity("PLACE", Some("varrow")).unwrap();
    let r = w
        .assert(greg, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    let entry = r.entries[0];
    w.assert(greg, "name", Value::Str("Gregor".into()), t(2))
        .unwrap();
    assert_eq!(w.latest_tx(), TxId(4));
    drop(w);
    (dir, path, entry)
}

/// Flips a byte in the last committed txn's body: open cuts it, cause Damaged.
fn damage_last(path: &Path) {
    let mut b = bytes(path);
    let last = *spans(&b).last().unwrap();
    b[last.body_start + 6] ^= 0xFF;
    std::fs::write(path, &b).unwrap();
}

/// Chops the last txn in the middle: open cuts it, cause Torn.
fn tear_last(path: &Path) {
    let b = bytes(path);
    let last = *spans(&b).last().unwrap();
    std::fs::write(path, &b[..last.end - 7]).unwrap();
}

fn damaged_world() -> (tempfile::TempDir, PathBuf, World, EntryId) {
    let (d, path, entry) = build();
    damage_last(&path);
    let w = open_ok(&path);
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    (d, path, w, entry)
}

fn unack<T: std::fmt::Debug>(r: Result<T, Error>, what: &str) {
    match r {
        Err(Error::RecoveryUnacknowledged) => {}
        other => panic!("{what}: expected Err(RecoveryUnacknowledged), got {other:?}"),
    }
}

// ------------------------------------------------------------------ (b)

#[test]
fn damaged_cut_reads_work_without_acknowledgement() {
    let (_d, _p, w, _e) = damaged_world();
    let greg = w.entity_by_handle("greg").unwrap();
    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(
        w.as_of(greg, "name", Tick(5)).unwrap(),
        vec![Value::Str("Greg".into())]
    );
    assert_eq!(w.history(greg, "name").unwrap().len(), 1);
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
}

#[test]
fn damaged_cut_every_world_write_is_refused_and_changes_nothing() {
    let (_d, path, mut w, entry) = damaged_world();
    let greg = w.entity_by_handle("greg").unwrap();
    let varrow = w.entity_by_handle("varrow").unwrap();
    let file_before = bytes(&path);

    unack(
        w.assert(greg, "name", Value::Str("X".into()), t(9)),
        "World::assert",
    );
    unack(
        w.retract(greg, "name", Value::Str("Greg".into()), t(9)),
        "World::retract",
    );
    unack(
        w.correct(entry, Correction::Delete),
        "World::correct delete",
    );
    unack(
        w.correct(
            entry,
            Correction::Replace {
                value: Some(Value::Str("Y".into())),
                tick: None,
                op: None,
                precision: None,
            },
        ),
        "World::correct replace",
    );
    unack(w.create_entity("CHARACTER", Some("lisa")), "create_entity");
    // a no-op-looking write is still refused (the gate comes first)
    unack(
        w.retract(varrow, "name", Value::Str("nope".into()), t(0)),
        "World::retract (would be a no-op)",
    );

    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(w.entity_by_handle("lisa"), None);
    assert_eq!(bytes(&path), file_before, "nothing was appended");
    assert_eq!(w.as_of(greg, "name", Tick(5)).unwrap().len(), 1);
}

#[test]
fn damaged_cut_every_txn_write_is_refused_and_poisons_the_txn() {
    let (_d, path, mut w, entry) = damaged_world();
    let greg = w.entity_by_handle("greg").unwrap();
    let file_before = bytes(&path);

    {
        let mut tx = w.begin();
        unack(
            tx.assert(greg, "name", Value::Str("X".into()), t(9)),
            "Txn::assert",
        );
        // poisoned as usual
        assert!(matches!(
            tx.assert(greg, "name", Value::Str("Z".into()), t(10)),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    }
    {
        let mut tx = w.begin();
        unack(
            tx.retract(greg, "name", Value::Str("Greg".into()), t(9)),
            "Txn::retract",
        );
    }
    {
        let mut tx = w.begin();
        unack(tx.correct(entry, Correction::Delete), "Txn::correct");
    }
    {
        let mut tx = w.begin();
        unack(tx.create_entity("CHARACTER", None), "Txn::create_entity");
    }
    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(bytes(&path), file_before);
}

#[test]
fn damaged_cut_txn_commit_is_refused() {
    let (_d, path, mut w, _e) = damaged_world();
    let file_before = bytes(&path);
    let tx = w.begin();
    unack(tx.commit(), "Txn::commit (empty txn)");
    assert_eq!(w.latest_tx(), TxId(3));
    assert_eq!(bytes(&path), file_before);
}

#[test]
fn acknowledge_recovery_unlocks_writes_and_recovery_still_reports_the_cut() {
    let (_d, path, mut w, entry) = damaged_world();
    let report_before = w.recovery().clone();
    assert!(report_before.bytes_cut > 0);
    w.acknowledge_recovery();
    assert_eq!(
        *w.recovery(),
        report_before,
        "report survives acknowledgement"
    );
    let greg = w.entity_by_handle("greg").unwrap();

    let r = w
        .assert(greg, "name", Value::Str("Gregor".into()), t(2))
        .unwrap();
    assert_eq!(r.changed, 1);
    w.retract(greg, "name", Value::Str("Gregor".into()), t(3))
        .unwrap();
    w.correct(entry, Correction::Delete).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let mut tx = w.begin();
    tx.assert(lisa, "name", Value::Str("Lisa".into()), t(0))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(w.latest_tx(), TxId(8));
    assert_eq!(*w.recovery(), report_before);
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
    drop(w);

    // the file now holds the acknowledged writes and reopens clean
    let w2 = open_ok(&path);
    assert_eq!(*w2.recovery(), RecoveryReport::default());
    assert_eq!(w2.latest_tx(), TxId(8));
}

#[test]
fn acknowledge_recovery_twice_is_harmless() {
    let (_d, _p, mut w, _e) = damaged_world();
    w.acknowledge_recovery();
    w.acknowledge_recovery();
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "name", Value::Str("A".into()), t(7))
        .unwrap();
}

#[test]
fn reopening_the_truncated_file_needs_no_acknowledgement() {
    let (_d, path, w, _e) = damaged_world();
    drop(w); // never acknowledged
    let mut w2 = open_ok(&path);
    assert_eq!(*w2.recovery(), RecoveryReport::default());
    let greg = w2.entity_by_handle("greg").unwrap();
    w2.assert(greg, "name", Value::Str("A".into()), t(7))
        .unwrap();
}

#[test]
fn torn_cut_needs_no_acknowledgement() {
    let (_d, path, entry) = build();
    tear_last(&path);
    let mut w = open_ok(&path);
    assert_eq!(w.recovery().cause, Some(CutCause::Torn));
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "name", Value::Str("A".into()), t(7))
        .unwrap();
    w.correct(entry, Correction::Delete).unwrap();
    w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let mut tx = w.begin();
    tx.assert(greg, "name", Value::Str("B".into()), t(8))
        .unwrap();
    tx.commit().unwrap();
}

#[test]
fn acknowledge_recovery_is_a_noop_on_a_torn_world() {
    let (_d, path, _e) = build();
    tear_last(&path);
    let mut w = open_ok(&path);
    let rep = w.recovery().clone();
    w.acknowledge_recovery();
    assert_eq!(*w.recovery(), rep);
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "name", Value::Str("A".into()), t(7))
        .unwrap();
}

#[test]
fn clean_open_create_and_in_memory_need_no_acknowledgement() {
    let (_d, path, _e) = build();
    let mut w = open_ok(&path);
    assert_eq!(*w.recovery(), RecoveryReport::default());
    let greg = w.entity_by_handle("greg").unwrap();
    w.assert(greg, "name", Value::Str("A".into()), t(7))
        .unwrap();
    w.acknowledge_recovery(); // no-op
    w.assert(greg, "name", Value::Str("B".into()), t(8))
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut c = World::create(&dir.path().join("c.kala"), schema()).unwrap();
    c.create_entity("CHARACTER", None).unwrap();
    c.acknowledge_recovery();
    c.create_entity("CHARACTER", None).unwrap();

    let mut m = World::in_memory(schema()).unwrap();
    m.create_entity("CHARACTER", None).unwrap();
    m.acknowledge_recovery();
    m.create_entity("CHARACTER", None).unwrap();
}

// ------------------------------------------------------------------ (c) L4

/// Rewrites the log so that every entry ID of the txns with index `< upto`
/// is shifted by `delta` (framing, Commit checksums and begin records are
/// recomputed, so only the ID sequence is wrong).
fn shift_entry_ids(path: &Path, delta: u64, upto: usize) {
    let src = bytes(path);
    let sp = spans(&src);
    let mut out = src[..common::HEADER_LEN].to_vec();
    for (i, s) in sp.iter().enumerate() {
        let shift = if i < upto { delta } else { 0 };
        let mut body = Vec::new();
        let fs = frame_spans(&src, *s);
        for (j, (start, end, kind)) in fs.iter().enumerate() {
            let id = u64::from_le_bytes(src[start + 5..start + 13].try_into().unwrap()) + shift;
            if j + 1 == fs.len() {
                assert_eq!(*kind, 5, "last frame is the Commit");
                body.extend(common::commit_frame(&body.clone(), id, s.tx));
            } else {
                body.extend(common::frame(*kind, id, &src[start + 13..*end]));
            }
        }
        out.extend(common::txn(s.tx, &body));
    }
    std::fs::write(path, &out).unwrap();
}

/// L4: a log whose genesis entry IDs are shifted (the genesis does not start
/// at entry#1) used to trip `record_one`'s debug_assert in doctor's rebuild.
/// Variants: only the genesis shifted; the whole log shifted consistently.
#[test]
fn l4_doctor_log_on_shifted_genesis_entry_ids_returns_findings_not_a_panic() {
    for (delta, upto) in [
        (1u64, 1usize),
        (7, 1),
        (1_000, 1),
        (1, usize::MAX),
        (5, usize::MAX),
    ] {
        let (_d, path, _e) = build();
        shift_entry_ids(&path, delta, upto);
        let report = doctor_log(&path, &DoctorOptions::default()).unwrap_or_else(|e| {
            panic!("delta {delta} upto {upto}: doctor_log returned Err({e:?})")
        });
        assert!(
            report
                .findings
                .iter()
                .any(|f| matches!(f, Finding::EntryIdGap { .. } | Finding::InvalidEntry { .. })),
            "delta {delta} upto {upto}: expected EntryIdGap or InvalidEntry, got {:?}",
            report.findings
        );
        assert!(!report.errors().is_empty());
        // and never a panic through the other entry point either
        assert!(World::open(&path).is_err());
    }
}

// ------------------------------------------------------------------ (c) L5
//
// Slice-3 finding L5: the collecting replay (doctor) drops an edge from
// `unasserted_edges` when it sees a `Revised` assert, BEFORE the Revised's
// chain / key checks. A live replay (World::open) fails at that entry, so the
// early removal is invisible there; the collecting replay refuses the entry
// and goes on, so the edge looks asserted when it is not.

fn schema5() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "rank",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
}

fn flagged(rep: &Report) -> Vec<(EntryId, EdgeId, String, Tick)> {
    let mut v: Vec<_> = rep
        .findings
        .iter()
        .filter_map(|f| match f {
            Finding::EdgePropertyOnUnassertedEdge {
                entry,
                edge,
                attr,
                tick,
            } => Some((*entry, *edge, attr.clone(), *tick)),
            _ => None,
        })
        .collect();
    v.sort_by_key(|f| f.0);
    v
}

/// The world's own answer: for every property assert in `props`
/// `(entry, edge, tick)`, flagged iff the edge's triple does not hold at the
/// tick according to `as_of` (the public read path).
fn flagged_by_world(
    w: &World,
    props: &[(EntryId, EdgeId, i64)],
) -> Vec<(EntryId, EdgeId, String, Tick)> {
    let mut v = Vec::new();
    for &(entry, edge, tick) in props {
        let (src, attr, tgt) = w.edge(edge).unwrap();
        let holds = w
            .as_of(src, &attr, Tick(tick))
            .unwrap()
            .contains(&Value::Ref(tgt));
        if !holds {
            v.push((entry, edge, "rank".to_owned(), Tick(tick)));
        }
    }
    v.sort_by_key(|f| f.0);
    v
}

/// A valid log: an edge whose only assert is a `Revised` entry (the edge was
/// interned by a value-moving correction), with property asserts, then the
/// chain is flipped to a retract and deleted. At every step doctor (world and
/// file) must flag exactly what the world's own reads say.
#[test]
fn l5_doctor_flags_edge_properties_exactly_as_the_world_reads_them_through_revised_edges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema5()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let base = w
        .assert(greg, "present", Value::Ref(varrow), t(10))
        .unwrap()
        .entries[0];
    let e1 = w.edge_id(greg, "present", varrow).unwrap();
    let p1 = w
        .assert(EntityId::from(e1), "rank", Value::I64(1), t(10))
        .unwrap()
        .entries[0];
    let mut props = vec![(p1, e1, 10)];

    let check = |w: &World, props: &[(EntryId, EdgeId, i64)], label: &str| {
        let want = flagged_by_world(w, props);
        let opts = DoctorOptions::default();
        assert_eq!(flagged(&doctor(w, &opts)), want, "{label}: doctor(world)");
        assert_eq!(
            flagged(&doctor_log(&path, &opts).unwrap()),
            want,
            "{label}: doctor_log"
        );
    };
    check(&w, &props, "control: property inside the edge");

    // move the value: Deleted + Revised(assert greg present lisa @10), edge#2 interned by the Revised
    let res = w
        .correct(
            base,
            Correction::Replace {
                value: Some(Value::Ref(lisa)),
                tick: None,
                op: None,
                precision: None,
            },
        )
        .unwrap();
    assert_eq!(res.edges.len(), 1, "the Revised interned an edge: {res:?}");
    let e2 = res.edges[0];
    assert_eq!(w.edge_id(greg, "present", lisa), Some(e2));
    let p2 = w
        .assert(EntityId::from(e2), "rank", Value::I64(2), t(10))
        .unwrap()
        .entries[0];
    props.push((p2, e2, 10));
    check(&w, &props, "edge#1 lost its assert, edge#2 has one");
    assert_eq!(flagged_by_world(&w, &props).len(), 1);
    assert_eq!(flagged_by_world(&w, &props)[0].1, e1);

    // flip the chain's current fact to a retract: edge#2 no longer holds at 10
    w.correct(
        base,
        Correction::Replace {
            value: None,
            tick: None,
            op: Some(Op::Retract),
            precision: None,
        },
    )
    .unwrap();
    check(&w, &props, "Revised retract");
    assert_eq!(flagged_by_world(&w, &props).len(), 2);

    // and flip it back to an assert, then delete the chain
    w.correct(
        base,
        Correction::Replace {
            value: None,
            tick: None,
            op: Some(Op::Assert),
            precision: None,
        },
    )
    .unwrap();
    check(&w, &props, "Revised flipped back");
    assert_eq!(flagged_by_world(&w, &props).len(), 1);
    w.correct(base, Correction::Delete).unwrap();
    check(&w, &props, "chain deleted");
    assert_eq!(flagged_by_world(&w, &props).len(), 2);
}

const K_DEFINE_EDGE: u8 = 6;
const K_FACT: u8 = 4;
const K_REVISED: u8 = 8;

/// Crafts tx4 on top of a real log: `DefineEdge edge#1 = (greg, present,
/// varrow)`, then `revised` (a Revised assert of that triple), then a
/// property assert on the edge. Returns (path, first entry id of tx4).
fn crafted_revised(revised_of: u64) -> (tempfile::TempDir, PathBuf, u64) {
    use common::{commit_frame, frame, txn};
    use kala_core::log::{EntryBody, read_log};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let (greg, varrow);
    {
        let mut w = World::create(&path, schema5()).unwrap();
        let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let v = w.create_entity("PLACE", Some("varrow")).unwrap();
        w.assert(g, "rank", Value::I64(1), t(1)).unwrap();
        greg = g;
        varrow = v;
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
    let (present, rank) = (attr("present"), attr("rank"));
    let first = log.last().unwrap().id.0 + 1;
    let edge = EdgeId::new(1).unwrap().raw();

    let mut define = edge.to_le_bytes().to_vec();
    define.extend_from_slice(&greg.raw().to_le_bytes());
    define.extend_from_slice(&present.to_le_bytes());
    define.extend_from_slice(&varrow.raw().to_le_bytes());

    let mut rev = revised_of.to_le_bytes().to_vec();
    rev.extend_from_slice(&1u32.to_le_bytes());
    rev.extend_from_slice(&greg.raw().to_le_bytes());
    rev.extend_from_slice(&present.to_le_bytes());
    rev.extend_from_slice(&5i64.to_le_bytes());
    rev.push(0); // assert
    rev.push(0); // precision byte (v4): Exact
    rev.extend_from_slice(&Value::Ref(varrow).encode().unwrap());

    let mut prop = edge.to_le_bytes().to_vec();
    prop.extend_from_slice(&rank.to_le_bytes());
    prop.extend_from_slice(&5i64.to_le_bytes());
    prop.push(0); // assert
    prop.push(0); // precision byte (v4): Exact
    prop.extend_from_slice(&Value::I64(9).encode().unwrap());

    let mut body = Vec::new();
    let mut id = first;
    for (k, p) in [(K_DEFINE_EDGE, define), (K_REVISED, rev), (K_FACT, prop)] {
        body.extend(frame(k, id, &p));
        id += 1;
    }
    let c = commit_frame(&body, id, 4);
    body.extend(c);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend(txn(4, &body));
    std::fs::write(&path, &bytes).unwrap();
    (dir, path, first)
}

/// A crafted log: tx4 defines edge#1 and "asserts" it only through a
/// Revised that points at a missing entry. World::open refuses the file.
/// Doctor must refuse that entry AND still judge the transaction like a
/// fresh rebuild: the edge was defined in a transaction that doesn't assert
/// its triple (InvalidEntry on the Commit), and the property on that edge is
/// flagged as sitting on an unasserted edge.
#[test]
fn l5_a_refused_revised_assert_does_not_make_its_edge_look_asserted() {
    let (_d, path, first) = crafted_revised(9_999);
    assert!(matches!(World::open(&path), Err(Error::Corrupt(_))));
    let rep = doctor_log(&path, &DoctorOptions::default()).unwrap();
    let revised = EntryId(first + 1);
    let commit = EntryId(first + 3);
    assert!(
        rep.findings.iter().any(|f| matches!(
            f,
            Finding::BadCorrection { entry, .. } if *entry == revised
        )),
        "the Revised is refused: {:#?}",
        rep.findings
    );
    assert!(
        rep.findings.iter().any(|f| matches!(
            f,
            Finding::InvalidEntry { entry, reason }
                if *entry == commit && reason.contains("doesn't assert its triple")
        )),
        "edge#1 was defined in tx4 but nothing there asserts it: {:#?}",
        rep.findings
    );
    assert_eq!(
        flagged(&rep),
        vec![(
            EntryId(first + 2),
            EdgeId::new(1).unwrap(),
            "rank".to_owned(),
            Tick(5)
        )],
        "the property on the never-asserted edge is flagged: {:#?}",
        rep.findings
    );
}
