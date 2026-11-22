//! Slice 4, item 3: unindexed strings (docs/slice-4-decisions.md "Item 3"; CLAUDE.md
//! "Value types (v1)": long strings, `indexed: false`). RED until the implementation
//! lands: this file fails to COMPILE.
//!
//! API expected (all re-exported at the crate root):
//!
//! ```text
//! pub const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;       // module log
//! Error::EntryTooLarge { attr: String, len: usize, max: usize }
//!     // len = the entry's whole encoded payload length (> max); refused before
//!     // anything is written; poisons a Txn; the file is unchanged
//! #[cfg(feature = "test-hooks")]
//! World::set_content_hash(&mut self, f: fn(&[u8]) -> u64)    // rebuilds indexes + value table
//! ```
//!
//! Contract: unindexed string values have no 256-byte limit, stay in the log in full,
//! never sit inside index keys (E / E_latest carry a fixed-width (hash, ordinal)
//! reference), and every public answer orders values by full encoding. Under
//! `set_content_hash(|_| 0)` (every value collides) every answer equals a normal world's.
//! A frame whose declared payload length exceeds MAX_ENTRY_BYTES is Corrupt on
//! open / read_log (never a torn tail, even at the end of the file) and a doctor_log finding.
mod common;

use kala_core::log::{EntryBody, MAX_ENTRY_BYTES, read_log};
use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- helpers

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(
            AttrDef::new("notes", ValueType::Str, Kind::State, Cardinality::Many).unindexed(),
        )
}

fn enc(v: &Value) -> Vec<u8> {
    v.encode().unwrap()
}

/// `n` ASCII bytes, distinguished by `tag` at the start.
fn long(tag: &str, n: usize) -> String {
    let mut x = format!("{tag}:");
    while x.len() < n {
        x.push_str("the grey king walked the long road to Varrow. ");
    }
    x.truncate(n);
    x
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

/// Doctor finds nothing but cardinality-one overlaps (which a script may leave
/// on purpose: cardinality is a label, not a rule).
fn integrity_clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(
        rep.findings
            .iter()
            .all(|f| matches!(f, Finding::CardinalityOverlap(_))),
        "{:#?}",
        rep.findings
    );
}

fn clean_file(p: &Path) {
    let rep = doctor_log(p, &DoctorOptions::default()).unwrap();
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

fn mem() -> (World, EntityId, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    (w, a, b)
}

fn file(dir: &tempfile::TempDir) -> (World, PathBuf, EntityId, EntityId) {
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    (w, p, a, b)
}

fn bio_attr(w: &World, with: &str) -> AttrId {
    w.index_dump()
        .e
        .iter()
        .find(|r| matches!(&r.2, Value::Str(x) if x == with))
        .expect("an E row holding the value")
        .1
}

fn sorted_by_encoding(vs: &[Value]) -> bool {
    vs.windows(2).all(|p| enc(&p[0]) < enc(&p[1]))
}

// ---------------------------------------------------------------- long biographies

fn biography_round_trip(n: usize) {
    let (mut w, a, _) = mem();
    let bio = long("bio", n);
    let r = w.assert(a, "bio", s(&bio), t(-5)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.as_of(a, "bio", Tick(-6)).unwrap(), vec![]);
    assert_eq!(w.as_of(a, "bio", Tick(-5)).unwrap(), vec![s(&bio)]);
    assert_eq!(w.as_of(a, "bio", Tick(i64::MAX)).unwrap(), vec![s(&bio)]);
    let h = w.history(a, "bio").unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(
        (h[0].tick, h[0].op, &h[0].value),
        (Tick(-5), Op::Assert, &s(&bio))
    );
    // asserting it again is a no-op; retracting a different long value too
    assert_eq!(w.assert(a, "bio", s(&bio), t(0)).unwrap().changed, 0);
    let other = long("other", n);
    assert_eq!(w.retract(a, "bio", s(&other), t(0)).unwrap().changed, 0);
    // retract ends it from its tick on, and not before
    assert_eq!(w.retract(a, "bio", s(&bio), t(10)).unwrap().changed, 1);
    assert_eq!(w.as_of(a, "bio", Tick(9)).unwrap(), vec![s(&bio)]);
    assert_eq!(w.as_of(a, "bio", Tick(10)).unwrap(), vec![]);
    let h = w.history(a, "bio").unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(
        (h[1].tick, h[1].op, &h[1].value),
        (Tick(10), Op::Retract, &s(&bio))
    );
    clean(&w);
}

#[test]
fn a_10_kb_biography_asserts_reads_history_and_retracts() {
    biography_round_trip(10 * 1024);
}

#[test]
fn a_1_mb_biography_asserts_reads_history_and_retracts() {
    biography_round_trip(1024 * 1024);
}

#[test]
fn an_unindexed_string_just_over_256_bytes_is_accepted_and_an_indexed_one_is_not() {
    let (mut w, a, _) = mem();
    w.assert(a, "bio", s(&long("x", 257)), t(0)).unwrap();
    w.assert(a, "notes", s(&long("x", 100_000)), t(0)).unwrap();
    assert!(w.assert(a, "name", s(&long("x", 256)), t(0)).is_ok());
    let err = w.assert(a, "name", s(&long("y", 257)), t(0)).unwrap_err();
    assert!(
        matches!(err, Error::Value(ValueError::StringTooLong { len: 257 })),
        "{err:?}"
    );
    // unchanged for the lookup side too
    assert_eq!(
        w.lookup("name", &s(&long("y", 257)), Tick(0)).unwrap(),
        vec![]
    );
    clean(&w);
}

#[test]
fn the_full_value_is_in_the_e_rows_and_never_in_v_or_v_fold() {
    let (mut w, a, _) = mem();
    let bio = long("bio", 5_000);
    w.assert(a, "bio", s(&bio), t(0)).unwrap();
    w.assert(a, "name", s("Edwin"), t(0)).unwrap();
    let d = w.index_dump();
    let e_rows: Vec<_> =
        d.e.iter()
            .filter(|r| matches!(&r.2, Value::Str(x) if *x == bio))
            .collect();
    assert_eq!(e_rows.len(), 1, "one E row, with the full value");
    let bio_attr = e_rows[0].1;
    assert!(d.v.iter().all(|r| r.0 != bio_attr), "no V rows for bio");
    assert!(
        d.v_fold.iter().all(|r| r.0 != bio_attr),
        "no VFold rows for bio"
    );
    // while the indexed name does have them
    assert_eq!(d.v.len(), 1);
    assert_eq!(d.v_fold.len(), 1);
}

#[test]
fn lookup_and_lookup_ci_on_an_unindexed_string_are_not_indexed() {
    let (mut w, a, _) = mem();
    let bio = long("bio", 5_000);
    w.assert(a, "bio", s(&bio), t(0)).unwrap();
    assert_eq!(
        w.lookup("bio", &s(&bio), Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
    assert_eq!(
        w.lookup_ci("bio", "x", Tick(0)),
        Err(Error::NotIndexed("bio".into()))
    );
}

#[test]
fn a_file_world_reopens_with_the_biography_byte_identical_and_read_log_has_it_in_full() {
    for n in [10 * 1024, 1024 * 1024] {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, p, a, _) = file(&dir);
        let bio = long("bio", n);
        w.assert(a, "bio", s(&bio), t(-5)).unwrap();
        w.assert(a, "notes", s(&long("n1", n)), t(1)).unwrap();
        w.retract(a, "notes", s(&long("n1", n)), t(2)).unwrap();
        let before = (
            w.as_of(a, "bio", Tick(0)).unwrap(),
            w.history(a, "bio").unwrap(),
            w.history(a, "notes").unwrap(),
        );
        drop(w);
        let log_before = std::fs::read(&p).unwrap();
        // the log has the whole value
        let entries = read_log(&p).unwrap();
        let facts: Vec<&Value> = entries
            .iter()
            .filter_map(|e| match &e.body {
                EntryBody::Fact { value, .. } => Some(value),
                _ => None,
            })
            .collect();
        assert!(
            facts.iter().any(|v| **v == s(&bio)),
            "{n}: full value in log"
        );
        // reopen: same answers, same bytes
        let w = World::open(&p).unwrap();
        let after = (
            w.as_of(a, "bio", Tick(0)).unwrap(),
            w.history(a, "bio").unwrap(),
            w.history(a, "notes").unwrap(),
        );
        assert_eq!(before, after);
        assert_eq!(w.as_of(a, "bio", Tick(0)).unwrap(), vec![s(&bio)]);
        clean(&w);
        drop(w);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            log_before,
            "open writes nothing"
        );
        clean_file(&p);
    }
}

#[test]
fn the_index_dump_e_rows_show_the_full_value_after_reopen_too() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, _) = file(&dir);
    let bio = long("bio", 20_000);
    w.assert(a, "bio", s(&bio), t(0)).unwrap();
    drop(w);
    let w = World::open(&p).unwrap();
    let d = w.index_dump();
    assert!(d.e.iter().any(|r| r.2 == s(&bio)));
    let bio_attr = bio_attr(&w, &bio);
    assert!(d.v.iter().all(|r| r.0 != bio_attr));
}

// ---------------------------------------------------------------- distinct values, ordering

fn prefix_values() -> Vec<Value> {
    let pre = "P".repeat(300);
    ["", "a", "b", "z", "a\0", "ab", "Z"]
        .iter()
        .map(|suf| s(&format!("{pre}{suf}")))
        .collect()
}

#[test]
fn distinct_long_values_sharing_a_300_byte_prefix_stay_distinct() {
    let (mut w, a, _) = mem();
    let vals = prefix_values();
    for (i, v) in vals.iter().enumerate() {
        assert_eq!(
            w.assert(a, "notes", v.clone(), t(i as i64))
                .unwrap()
                .changed,
            1
        );
    }
    let all = w.as_of(a, "notes", Tick(100)).unwrap();
    assert_eq!(all.len(), vals.len());
    assert!(sorted_by_encoding(&all), "sorted by full encoding");
    // each one is its own history
    for v in &vals {
        let h: Vec<_> = w
            .history(a, "notes")
            .unwrap()
            .into_iter()
            .filter(|c| &c.value == v)
            .collect();
        assert_eq!(h.len(), 1);
    }
    // retract one: only that one goes
    assert_eq!(
        w.retract(a, "notes", vals[3].clone(), t(50))
            .unwrap()
            .changed,
        1
    );
    let left = w.as_of(a, "notes", Tick(100)).unwrap();
    assert_eq!(left.len(), vals.len() - 1);
    assert!(!left.contains(&vals[3]));
    // a no-op is judged on the full value
    assert_eq!(
        w.retract(a, "notes", vals[3].clone(), t(60))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        w.assert(a, "notes", vals[0].clone(), t(60))
            .unwrap()
            .changed,
        0
    );
    // same-tick conflict is judged on the full value too: the others don't clash
    w.assert(a, "notes", vals[3].clone(), t(70)).unwrap();
    assert!(matches!(
        w.retract(a, "notes", vals[3].clone(), t(70)),
        Err(Error::SameTickConflict { .. })
    ));
    assert_eq!(
        w.retract(a, "notes", vals[4].clone(), t(70))
            .unwrap()
            .changed,
        1
    );
    clean(&w);
}

#[test]
fn as_of_orders_by_full_encoding_whatever_the_hash_order_is() {
    // 60 values in scrambled write order, with shared prefixes and NULs
    let mut vals = prefix_values();
    for i in 0..53 {
        vals.push(s(&format!(
            "{}{:03}",
            "Q".repeat(40 + i % 7),
            (i * 37) % 53
        )));
    }
    type Hook = Option<fn(&[u8]) -> u64>;
    let hashes: [(&str, Hook); 3] = [
        ("real", None),
        ("zero", Some(|_| 0)),
        ("len%3", Some(|b| (b.len() % 3) as u64)),
    ];
    for (name, hook) in hashes {
        let (mut w, a, _) = mem();
        if let Some(f) = hook {
            w.set_content_hash(f);
        }
        for (i, v) in vals.iter().enumerate() {
            let tick = ((i * 7) % vals.len()) as i64;
            w.assert(a, "notes", v.clone(), t(tick)).unwrap();
        }
        let got = w.as_of(a, "notes", Tick(1_000)).unwrap();
        assert_eq!(got.len(), vals.len(), "{name}");
        assert!(
            sorted_by_encoding(&got),
            "{name}: as_of not in encoding order"
        );
        let mut expect = vals.clone();
        expect.sort_by_key(enc);
        assert_eq!(got, expect, "{name}");
        clean(&w);
    }
}

#[test]
fn cardinality_one_overlap_of_two_long_values_is_reported_with_a_below_b_by_full_encoding() {
    for hook in [None, Some((|_| 0) as fn(&[u8]) -> u64)] {
        let (mut w, a, _) = mem();
        if let Some(f) = hook {
            w.set_content_hash(f);
        }
        let pre = "P".repeat(300);
        let lo = s(&format!("{pre}a"));
        let hi = s(&format!("{pre}b"));
        // write the larger first so write order and encoding order disagree
        let r1 = w.assert(a, "bio", hi.clone(), t(0)).unwrap();
        assert!(r1.warnings.introduced.is_empty());
        let r2 = w.assert(a, "bio", lo.clone(), t(5)).unwrap();
        assert_eq!(r2.warnings.introduced.len(), 1);
        let o = &r2.warnings.introduced[0];
        assert_eq!((o.entity, o.attr.as_str()), (a, "bio"));
        assert_eq!((&o.a, &o.b), (&lo, &hi));
        assert!(enc(&o.a) < enc(&o.b));
        assert_eq!(w.overlaps("bio").unwrap().len(), 1);
        assert_eq!(w.overlaps("bio").unwrap()[0], *o);
        // the retract resolves it, and says so
        let r3 = w.retract(a, "bio", hi.clone(), t(9)).unwrap();
        assert_eq!(r3.warnings.resolved.len(), 1);
        assert_eq!(
            (&r3.warnings.resolved[0].a, &r3.warnings.resolved[0].b),
            (&lo, &hi)
        );
        // retract at 5 touching: [0,5) and [5,..) are fine
        let rep = doctor(&w, &DoctorOptions::default());
        assert!(
            rep.findings
                .iter()
                .all(|f| matches!(f, Finding::CardinalityOverlap(_))),
            "{:#?}",
            rep.findings
        );
    }
}

// ---------------------------------------------------------------- forced collisions

/// A script covering writes, no-ops, conflicts, overlaps and corrections; returns
/// one line per step. Values share long prefixes.
fn script(w: &mut World, a: EntityId, b: EntityId) -> Vec<String> {
    let pre = "S".repeat(300);
    let v = |x: &str| s(&format!("{pre}{x}"));
    let mut out = Vec::new();
    let mut ids: Vec<EntryId> = Vec::new();
    macro_rules! step {
        ($e:expr) => {{
            let r = $e;
            if let Ok(wr) = &r {
                ids.extend(wr.entries.iter().copied());
            }
            out.push(format!("{r:?}"));
        }};
    }
    step!(w.assert(a, "bio", v("one"), t(0)));
    step!(w.assert(a, "bio", v("one"), t(0))); // repeat: no-op
    step!(w.assert(a, "bio", v("one"), t(3))); // holds already: no-op
    step!(w.assert(a, "bio", v("two"), t(2))); // overlap warning
    step!(w.retract(a, "bio", v("one"), t(2))); // touching
    step!(w.retract(a, "bio", v("one"), t(2))); // repeated no-op
    step!(w.retract(a, "bio", v("two"), t(2))); // same-tick conflict
    step!(w.retract(a, "bio", v("never"), t(2))); // not asserted: no-op
    step!(w.assert(a, "bio", v("three"), t(-4))); // prequel overlap with one
    step!(w.assert(a, "notes", v("n1"), t(0)));
    step!(w.assert(a, "notes", v("n2"), t(0)));
    step!(w.assert(b, "notes", v("n1"), t(1)));
    step!(w.retract(a, "notes", v("n1"), t(5)));
    step!(w.retract(a, "notes", v("n1"), t(-1))); // out-of-order retract judged at its tick: no-op
    step!(w.assert(a, "notes", v("n1"), None));
    for (i, &id) in ids.iter().enumerate().take(14) {
        let c = match i % 4 {
            0 => Correction::Delete,
            1 => Correction::Replace {
                value: Some(v("replaced")),
                tick: None,
                op: None,
                precision: None,
            },
            2 => Correction::Replace {
                value: None,
                tick: Some(Tick(i as i64 - 7)),
                op: None,
                precision: None,
            },
            _ => Correction::Replace {
                value: None,
                tick: None,
                op: Some(Op::Retract),
                precision: None,
            },
        };
        out.push(format!("correct #{i}: {:?}", w.correct(id, c)));
    }
    out.extend(snapshot(w, a, b));
    out
}

fn snapshot(w: &World, a: EntityId, b: EntityId) -> Vec<String> {
    let mut out = Vec::new();
    for e in [a, b] {
        for attr in ["bio", "notes"] {
            out.push(format!("hist {e:?} {attr} {:?}", w.history(e, attr)));
            for tick in [i64::MIN, -5, -4, -1, 0, 1, 2, 3, 5, 6, i64::MAX] {
                out.push(format!(
                    "as_of {e:?} {attr} @{tick} {:?}",
                    w.as_of(e, attr, Tick(tick))
                ));
            }
        }
    }
    out.push(format!("overlaps bio {:?}", w.overlaps("bio")));
    out.push(format!("overlaps notes {:?}", w.overlaps("notes")));
    out.push(format!("latest {:?} {:?}", w.latest_tx(), w.latest_tick()));
    out
}

#[test]
fn with_every_value_colliding_every_answer_equals_a_normal_world() {
    let (mut normal, a, b) = mem();
    let expect = script(&mut normal, a, b);
    integrity_clean(&normal);

    // hook set before any write
    let (mut w, a2, b2) = mem();
    w.set_content_hash(|_| 0);
    let got = script(&mut w, a2, b2);
    assert_eq!(got, expect);
    integrity_clean(&w);
}

#[test]
fn setting_the_hash_after_writes_rebuilds_to_the_same_answers() {
    let (mut normal, a, b) = mem();
    let expect = script(&mut normal, a, b);

    // script on a normal world, then swap the hash: the rebuilt world answers the same
    let (mut w, a2, b2) = mem();
    let first = script(&mut w, a2, b2);
    assert_eq!(first, expect);
    w.set_content_hash(|_| 0);
    assert_eq!(snapshot(&w, a2, b2), snapshot(&normal, a, b));
    integrity_clean(&w);
    // and the hooked world keeps working: further writes agree with the normal one
    let more = |w: &mut World, a: EntityId| {
        let pre = "S".repeat(300);
        vec![
            format!("{:?}", w.assert(a, "bio", s(&format!("{pre}late")), t(50))),
            format!("{:?}", w.assert(a, "bio", s(&format!("{pre}later")), t(40))),
            format!("{:?}", w.retract(a, "bio", s(&format!("{pre}late")), t(60))),
        ]
    };
    assert_eq!(more(&mut w, a2), more(&mut normal, a));
    assert_eq!(snapshot(&w, a2, b2), snapshot(&normal, a, b));
    integrity_clean(&w);
}

#[test]
fn a_colliding_world_on_a_file_reopens_with_the_real_hash_and_answers_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, b) = file(&dir);
    w.set_content_hash(|_| 0);
    let _ = script(&mut w, a, b);
    integrity_clean(&w);
    let before = snapshot(&w, a, b);
    drop(w);
    let rep = doctor_log(&p, &DoctorOptions::default()).unwrap();
    assert!(
        rep.findings
            .iter()
            .all(|f| matches!(f, Finding::CardinalityOverlap(_))),
        "{:#?}",
        rep.findings
    );
    let w = World::open(&p).unwrap();
    assert_eq!(snapshot(&w, a, b), before);
    integrity_clean(&w);
}

#[test]
fn colliding_values_keep_separate_histories_across_canons() {
    let (mut w, a, _) = mem();
    w.set_content_hash(|_| 0);
    let x = s(&long("x", 4_000));
    let y = s(&long("y", 4_000));
    w.assert(a, "notes", x.clone(), t(0)).unwrap(); // tx 3 (after 2 creates + genesis)
    let after_x = w.latest_tx();
    w.assert(a, "notes", y.clone(), t(0)).unwrap();
    w.retract(a, "notes", x.clone(), t(4)).unwrap();
    assert_eq!(w.as_of(a, "notes", Tick(0)).unwrap().len(), 2);
    assert_eq!(w.as_of(a, "notes", Tick(4)).unwrap(), vec![y.clone()]);
    // an older canon sees only x
    let old = w.at_canon(after_x);
    assert_eq!(old.as_of(a, "notes", Tick(4)).unwrap(), vec![x.clone()]);
    assert_eq!(w.at_canon(after_x).history(a, "notes").unwrap().len(), 1);
    integrity_clean(&w);
}

// ---------------------------------------------------------------- randomized equivalence

#[derive(Clone, Debug)]
enum Step {
    Assert(u8, u8, u8, Option<i8>),
    Retract(u8, u8, u8, Option<i8>),
    Correct(u8, u8, i8),
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u8..2, 0u8..2, 0u8..7, prop::option::weighted(0.9, -3i8..4))
            .prop_map(|(e, a, v, k)| Step::Assert(e, a, v, k)),
        (0u8..2, 0u8..2, 0u8..7, prop::option::weighted(0.9, -3i8..4))
            .prop_map(|(e, a, v, k)| Step::Retract(e, a, v, k)),
        (0u8..32, 0u8..4, -3i8..4).prop_map(|(i, k, x)| Step::Correct(i, k, x)),
    ]
}

fn run_random(w: &mut World, ents: [EntityId; 2], steps: &[Step]) -> Vec<String> {
    let pre = "R".repeat(300);
    let val = |i: u8| match i {
        0 => s(""),
        1 => s("short"),
        n => s(&format!("{pre}{n}")),
    };
    let attr = |i: u8| if i == 0 { "bio" } else { "notes" };
    let mut out = Vec::new();
    let mut ids: Vec<EntryId> = Vec::new();
    for st in steps {
        let r = match st {
            Step::Assert(e, a, v, k) => w.assert(
                ents[*e as usize],
                attr(*a),
                val(*v),
                k.map(|x| Tick(x as i64)),
            ),
            Step::Retract(e, a, v, k) => w.retract(
                ents[*e as usize],
                attr(*a),
                val(*v),
                k.map(|x| Tick(x as i64)),
            ),
            Step::Correct(i, k, x) => {
                if ids.is_empty() {
                    continue;
                }
                let id = ids[*i as usize % ids.len()];
                let c = match k {
                    0 => Correction::Delete,
                    1 => Correction::Replace {
                        value: Some(val(*x as u8 % 7)),
                        tick: None,
                        op: None,
                        precision: None,
                    },
                    2 => Correction::Replace {
                        value: None,
                        tick: Some(Tick(*x as i64)),
                        op: None,
                        precision: None,
                    },
                    _ => Correction::Replace {
                        value: None,
                        tick: None,
                        op: Some(if *x % 2 == 0 { Op::Assert } else { Op::Retract }),
                        precision: None,
                    },
                };
                w.correct(id, c)
            }
        };
        if let Ok(wr) = &r {
            ids.extend(wr.entries.iter().copied());
        }
        out.push(format!("{r:?}"));
    }
    out.extend(snapshot(w, ents[0], ents[1]));
    out
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x1d0c_4003),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn prop_colliding_hashes_answer_exactly_like_the_real_hash(
        steps in prop::collection::vec(step_strategy(), 1..40)
    ) {
        let (mut normal, a, b) = mem();
        let expect = run_random(&mut normal, [a, b], &steps);
        let rep = doctor(&normal, &DoctorOptions::default());
        prop_assert!(
            rep.findings.iter().all(|f| matches!(f, Finding::CardinalityOverlap(_))),
            "normal: {:#?}", rep.findings
        );

        let hooks: [fn(&[u8]) -> u64; 2] = [|_| 0, |b| (b.len() % 3) as u64];
        for hook in hooks {
            let (mut w, a, b) = mem();
            w.set_content_hash(hook);
            let got = run_random(&mut w, [a, b], &steps);
            prop_assert_eq!(&got, &expect);
            let rep = doctor(&w, &DoctorOptions::default());
            prop_assert!(
                rep.findings.iter().all(|f| matches!(f, Finding::CardinalityOverlap(_))),
                "hooked: {:#?}", rep.findings
            );
        }
    }
}

// ---------------------------------------------------------------- scale

fn ingest(n: usize) -> std::time::Duration {
    let (mut w, a, _) = mem();
    w.set_content_hash(|_| 0);
    let start = std::time::Instant::now();
    let mut txn = w.begin();
    for i in 0..n {
        txn.assert(a, "notes", s(&format!("{}{i:06}", "V".repeat(300))), t(0))
            .unwrap();
    }
    txn.commit().unwrap();
    let took = start.elapsed();
    assert_eq!(w.as_of(a, "notes", Tick(0)).unwrap().len(), n);
    took
}

#[test]
fn ten_thousand_distinct_values_under_total_collision_complete_and_read_back() {
    let (mut w, a, _) = mem();
    w.set_content_hash(|_| 0);
    let mut txn = w.begin();
    for i in 0..10_000 {
        txn.assert(
            a,
            "notes",
            s(&format!("{}{i:06}", "V".repeat(300))),
            t(i % 5),
        )
        .unwrap();
    }
    txn.commit().unwrap();
    let got = w.as_of(a, "notes", Tick(4)).unwrap();
    assert_eq!(got.len(), 10_000);
    assert!(sorted_by_encoding(&got));
    // a later one is a new value, an existing one is a no-op
    assert_eq!(
        w.assert(a, "notes", s(&format!("{}{:06}", "V".repeat(300), 7)), t(9))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        w.assert(a, "notes", s(&format!("{}x", "V".repeat(300))), t(9))
            .unwrap()
            .changed,
        1
    );
}

/// Release only: 4x the values must cost about 4x, not 16x (a linear scan of
/// colliding values per insert would be quadratic).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: run with --release")]
fn ingesting_under_total_collision_is_not_quadratic() {
    let best = |n: usize| (0..3).map(|_| ingest(n)).min().unwrap();
    let small = best(2_500);
    let big = best(10_000);
    let ratio = big.as_secs_f64() / small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 9.0,
        "2,500 values took {small:?}, 10,000 took {big:?} (ratio {ratio:.1}, 16 = quadratic)"
    );
}

// ---------------------------------------------------------------- MAX_ENTRY_BYTES

#[test]
fn max_entry_bytes_is_16_mib_and_re_exported_at_the_root() {
    assert_eq!(MAX_ENTRY_BYTES, 16 * 1024 * 1024);
    assert_eq!(kala_core::log::MAX_ENTRY_BYTES, MAX_ENTRY_BYTES);
}

/// The longest 'x' string whose Fact entry fits, from the error's own numbers.
fn largest_fitting_len(w: &mut World, a: EntityId) -> usize {
    let err = w
        .assert(a, "bio", s(&"x".repeat(MAX_ENTRY_BYTES)), t(0))
        .unwrap_err();
    let Error::EntryTooLarge { attr, len, max } = err else {
        panic!("expected EntryTooLarge, got {err:?}");
    };
    assert_eq!(attr, "bio");
    assert_eq!(max, MAX_ENTRY_BYTES);
    assert!(len > max, "len {len} must exceed max {max}");
    // payload = fixed + string length, so one fixed overhead of (len - MAX)
    2 * MAX_ENTRY_BYTES - len
}

#[test]
fn the_largest_string_whose_entry_fits_is_accepted_and_one_byte_more_is_too_large() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, _) = file(&dir);
    let before = std::fs::read(&p).unwrap();
    let tx_before = w.latest_tx();
    let fit = largest_fitting_len(&mut w, a);
    // the refusal wrote nothing
    assert_eq!(std::fs::read(&p).unwrap(), before);
    assert_eq!(w.latest_tx(), tx_before);

    // one byte more: the entry is exactly MAX + 1
    let err = w
        .assert(a, "bio", s(&"x".repeat(fit + 1)), t(0))
        .unwrap_err();
    assert_eq!(
        err,
        Error::EntryTooLarge {
            attr: "bio".into(),
            len: MAX_ENTRY_BYTES + 1,
            max: MAX_ENTRY_BYTES
        }
    );
    assert_eq!(std::fs::read(&p).unwrap(), before);

    // a NUL costs two bytes in the encoding: fit - 1 x's plus a NUL is MAX + 1 too
    let nul = format!("{}\0", "x".repeat(fit - 1));
    assert_eq!(
        w.assert(a, "bio", s(&nul), t(0)).unwrap_err(),
        Error::EntryTooLarge {
            attr: "bio".into(),
            len: MAX_ENTRY_BYTES + 1,
            max: MAX_ENTRY_BYTES
        }
    );

    // the largest is accepted, reads back, survives a reopen byte for byte
    let big = "x".repeat(fit);
    let r = w.assert(a, "bio", s(&big), t(0)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.as_of(a, "bio", Tick(0)).unwrap(), vec![s(&big)]);
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.as_of(a, "bio", Tick(0)).unwrap(), vec![s(&big)]);
    drop(w);
    clean_file(&p);
    // and the world is still writable after refusals
    let mut w = World::open(&p).unwrap();
    assert_eq!(w.assert(a, "notes", s("small"), t(0)).unwrap().changed, 1);
}

#[test]
fn an_entry_too_large_poisons_a_txn_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, _) = file(&dir);
    let before = std::fs::read(&p).unwrap();
    let mut txn = w.begin();
    txn.assert(a, "notes", s("fine"), t(0)).unwrap();
    let err = txn
        .assert(a, "bio", s(&"x".repeat(MAX_ENTRY_BYTES + 1)), t(0))
        .unwrap_err();
    assert!(
        matches!(&err, Error::EntryTooLarge { attr, len, max }
            if attr == "bio" && *len > *max && *max == MAX_ENTRY_BYTES),
        "{err:?}"
    );
    assert_eq!(
        txn.assert(a, "notes", s("after"), t(0)).unwrap_err(),
        Error::TransactionAborted
    );
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
    assert_eq!(std::fs::read(&p).unwrap(), before);
    assert_eq!(w.as_of(a, "notes", Tick(0)).unwrap(), vec![]);
    clean(&w);
}

#[test]
fn the_limit_is_per_entry_so_a_transaction_may_hold_several_large_ones() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, b) = file(&dir);
    let x = s(&long("x", 9 * 1024 * 1024));
    let y = s(&long("y", 9 * 1024 * 1024));
    let mut txn = w.begin();
    txn.assert(a, "notes", x.clone(), t(0)).unwrap();
    txn.assert(b, "notes", y.clone(), t(0)).unwrap();
    txn.commit().unwrap();
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.as_of(a, "notes", Tick(0)).unwrap(), vec![x]);
    assert_eq!(w.as_of(b, "notes", Tick(0)).unwrap(), vec![y]);
    drop(w);
    clean_file(&p);
}

#[test]
fn a_replace_that_makes_the_entry_too_large_is_refused_and_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, _) = file(&dir);
    let r = w.assert(a, "bio", s("small"), t(0)).unwrap();
    let before = std::fs::read(&p).unwrap();
    let err = w
        .correct(
            r.entries[0],
            Correction::Replace {
                value: Some(s(&"x".repeat(MAX_ENTRY_BYTES + 1))),
                tick: None,
                op: None,
                precision: None,
            },
        )
        .unwrap_err();
    assert!(
        matches!(&err, Error::EntryTooLarge { attr, len, max }
            if attr == "bio" && *len > *max),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&p).unwrap(), before);
    assert_eq!(w.as_of(a, "bio", Tick(0)).unwrap(), vec![s("small")]);
}

// ---------------------------------------------------------------- crafted logs

/// Replaces transaction `txn_idx` (0 = genesis) with `body`, keeping its tx, and
/// re-seals its begin record.
fn put_body(bytes: &[u8], txn_idx: usize, body: &[u8]) -> Vec<u8> {
    let sp = common::spans(bytes)[txn_idx];
    let mut out = bytes[..sp.start].to_vec();
    out.extend_from_slice(&common::txn(sp.tx, body));
    out.extend_from_slice(&bytes[sp.end..]);
    out
}

/// A file whose transaction `txn_idx` holds a VALID Fact (and a valid Commit) with
/// the unindexed value stretched so the Fact's payload is exactly `payload_len`.
fn stretch_fact(bytes: &[u8], txn_idx: usize, payload_len: usize) -> Vec<u8> {
    let sp = common::spans(bytes)[txn_idx];
    let frames = common::frame_spans(bytes, sp);
    let (fs, fe, kind) = frames[0];
    assert_eq!(kind, 4, "the txn must start with a Fact");
    let (cs, ce, ckind) = *frames.last().unwrap();
    assert_eq!(ckind, 5);
    let payload = &bytes[fs + 4..fe];
    let run = payload.windows(4).position(|w| w == b"xxxx").unwrap();
    assert!(payload_len >= payload.len());
    let mut new_payload = payload[..run].to_vec();
    new_payload.extend(std::iter::repeat_n(b'x', payload_len - payload.len()));
    new_payload.extend_from_slice(&payload[run..]);
    assert_eq!(new_payload.len(), payload_len);
    let mut prior = (new_payload.len() as u32).to_le_bytes().to_vec();
    prior.extend_from_slice(&new_payload);
    // anything between the first frame and the commit stays as is
    prior.extend_from_slice(&bytes[fe..cs]);
    let commit_id = u64::from_le_bytes(bytes[cs + 5..cs + 13].try_into().unwrap());
    let commit = common::commit_frame(&prior, commit_id, sp.tx);
    assert_eq!(ce - cs, commit.len());
    let mut body = prior;
    body.extend_from_slice(&commit);
    put_body(bytes, txn_idx, &body)
}

/// A world on disk with three single-fact transactions on `bio`/`notes`
/// (values start with "xxxx"), closed. Returns the path and the txn index of
/// the first fact.
fn three_fact_log(dir: &tempfile::TempDir) -> (PathBuf, usize) {
    let (mut w, p, a, b) = file(dir);
    w.assert(a, "bio", s(&"x".repeat(10)), t(0)).unwrap();
    w.assert(b, "bio", s(&"x".repeat(11)), t(1)).unwrap();
    w.assert(a, "notes", s(&"x".repeat(12)), t(2)).unwrap();
    drop(w);
    let n = common::spans(&std::fs::read(&p).unwrap()).len();
    (p, n - 3)
}

fn assert_corrupt_untouched(p: &Path, what: &str) {
    let before = std::fs::read(p).unwrap();
    let files_before: Vec<_> = std::fs::read_dir(p.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    match World::open(p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{what}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{what}: expected Corrupt, open succeeded"),
    }
    match read_log(p) {
        Err(Error::Corrupt(_)) => {}
        other => panic!(
            "{what}: read_log expected Corrupt, got {:?}",
            other.map(|v| v.len())
        ),
    }
    assert_eq!(std::fs::read(p).unwrap(), before, "{what}: file untouched");
    let mut files_after: Vec<_> = std::fs::read_dir(p.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut fb = files_before;
    fb.sort();
    files_after.sort();
    assert_eq!(files_after, fb, "{what}: no quarantine file");
    let rep = doctor_log(p, &DoctorOptions::default())
        .unwrap_or_else(|e| panic!("{what}: doctor_log must report, got Err({e:?})"));
    assert!(
        !rep.errors().is_empty(),
        "{what}: doctor_log finds it: {:?}",
        rep.findings
    );
}

#[test]
fn a_frame_longer_than_max_entry_bytes_is_corrupt_in_the_last_and_in_a_middle_transaction() {
    for (which, offset) in [("middle", 1usize), ("last", 2usize)] {
        let dir = tempfile::tempdir().unwrap();
        let (p, first) = three_fact_log(&dir);
        let bytes = std::fs::read(&p).unwrap();
        let crafted = stretch_fact(&bytes, first + offset, MAX_ENTRY_BYTES + 1);
        std::fs::write(&p, &crafted).unwrap();
        assert_corrupt_untouched(&p, &format!("{which}, one over"));
    }
}

#[test]
fn a_frame_far_over_the_limit_is_corrupt_not_a_torn_tail_even_at_the_end_of_the_file() {
    for (which, offset) in [("middle", 1usize), ("last", 2usize)] {
        let dir = tempfile::tempdir().unwrap();
        let (p, first) = three_fact_log(&dir);
        let bytes = std::fs::read(&p).unwrap();
        let idx = first + offset;
        let sp = common::spans(&bytes)[idx];
        // declared payload length u32::MAX, body is just those bytes + a little
        let mut body = u32::MAX.to_le_bytes().to_vec();
        body.extend_from_slice(&[4, 0, 0, 0]);
        let mut crafted = put_body(&bytes, idx, &body);
        // sanity: the txn we crafted is the one we meant
        assert_eq!(common::spans(&crafted)[idx].tx, sp.tx);
        std::fs::write(&p, &crafted).unwrap();
        assert_corrupt_untouched(&p, &format!("{which}, u32::MAX"));
        crafted.clear();
    }
}

#[test]
fn a_frame_of_exactly_max_entry_bytes_still_opens_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let (p, first) = three_fact_log(&dir);
    let bytes = std::fs::read(&p).unwrap();
    let crafted = stretch_fact(&bytes, first + 1, MAX_ENTRY_BYTES);
    std::fs::write(&p, &crafted).unwrap();
    let before = std::fs::read(&p).unwrap();
    let w = World::open(&p).unwrap();
    assert_eq!(w.recovery().bytes_cut, 0);
    let b = w.entity_by_handle("b").unwrap();
    let got = w.as_of(b, "bio", Tick(1)).unwrap();
    assert_eq!(got.len(), 1);
    match &got[0] {
        Value::Str(x) => {
            assert!(x.bytes().all(|c| c == b'x'));
            assert!(x.len() > MAX_ENTRY_BYTES - 100 && x.len() < MAX_ENTRY_BYTES);
        }
        v => panic!("{v:?}"),
    }
    clean(&w);
    drop(w);
    assert_eq!(std::fs::read(&p).unwrap(), before);
    assert!(read_log(&p).unwrap().len() > 3);
    clean_file(&p);
}

// ---------------------------------------------------------------- tampering

#[test]
fn tampering_an_e_row_of_an_unindexed_attribute_is_an_index_mismatch() {
    for index in [IndexName::E, IndexName::ELatest] {
        for flip in [true, false] {
            let (mut w, a, _) = mem();
            w.assert(a, "bio", s(&long("bio", 3_000)), t(0)).unwrap();
            clean(&w);
            let t = if flip {
                Tamper::FlipOp { index, nth: 0 }
            } else {
                Tamper::RemoveRow { index, nth: 0 }
            };
            w.tamper_index(t);
            let rep = doctor(&w, &DoctorOptions::default());
            assert!(
                rep.findings
                    .iter()
                    .any(|f| matches!(f, Finding::IndexMismatch { index: i, .. } if *i == index)),
                "{index:?} flip={flip}: {:#?}",
                rep.findings
            );
            for other in [
                IndexName::V,
                IndexName::VLatest,
                IndexName::VFold,
                IndexName::VFoldLatest,
            ] {
                assert!(
                    !rep.findings.iter().any(
                        |f| matches!(f, Finding::IndexMismatch { index: i, .. } if *i == other)
                    ),
                    "{other:?} must not be reported: {:#?}",
                    rep.findings
                );
            }
        }
    }
}

#[test]
fn a_tampered_world_with_colliding_values_is_still_caught() {
    let (mut w, a, _) = mem();
    w.set_content_hash(|_| 0);
    w.assert(a, "notes", s(&long("x", 3_000)), t(0)).unwrap();
    w.assert(a, "notes", s(&long("y", 3_000)), t(0)).unwrap();
    clean(&w);
    w.tamper_index(Tamper::FlipOp {
        index: IndexName::E,
        nth: 1,
    });
    let rep = doctor(&w, &DoctorOptions::default());
    assert!(
        rep.findings.iter().any(|f| matches!(
            f,
            Finding::IndexMismatch {
                index: IndexName::E,
                ..
            }
        )),
        "{:#?}",
        rep.findings
    );
}
