//! Slice 3, item 5: the latest-revision index (docs/slice-3-decisions.md,
//! "Item 5"). RED until the implementation lands: `World::at_canon_main` does
//! not exist yet, so this file fails to COMPILE (`no method named
//! at_canon_main found for struct World`).
//!
//! API expected:
//!
//! ```text
//! #[doc(hidden)]
//! pub fn World::at_canon_main(&self, tx: TxId) -> View<'_>
//! ```
//!
//! It must read through the MAIN indexes (`E` / `R` with the canon skip) even
//! when `tx == latest_tx`, while `World::as_of_value / as_of / history /
//! neighbors` and `at_canon(tx >= latest_tx)` read through `E_latest` /
//! `R_latest`. The index is internal, so this comparison is the only thing
//! that proves it: until the index exists, both sides run the same code and
//! the property is TRIVIALLY TRUE. It only has teeth because `at_canon_main`
//! is forced to be the main path and `World` reads the latest path.
//!
//! The property: after any random sequence of assert / retract / correct
//! (Delete and Replace, same-key op flips, tick moves, value moves, bidirectional
//! renormalisation, single writes and transactions, good and failing ones),
//! for every entity / attribute / value / tick probe, `as_of_value`, `as_of`,
//! `history` and `neighbors` agree between the world's latest reads, the
//! latest-canon `at_canon`, and `at_canon_main(latest_tx)`; and again after
//! `World::open` on the file (the index is rebuilt on replay), and after more
//! writes on the reopened world.

use kala_core::log::EntryId;
use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

// ---------------------------------------------------------------- world

const NAMES: [&str; 3] = ["Greg", "Gregor", "Edwin"];

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

const ATTRS: [&str; 3] = ["located_in", "ally_of", "name"];

/// Ticks with deliberate collisions, both extremes and negatives.
const TICKS: [i64; 8] = [i64::MIN, -300, -250, -1, 0, 5, 100, i64::MAX];

fn populate(w: &mut World) -> Vec<EntityId> {
    // greg < edwin < mara (CHARACTER), varrow < ashford (PLACE)
    vec![
        w.create_entity("CHARACTER", Some("greg")).unwrap(),
        w.create_entity("CHARACTER", Some("edwin")).unwrap(),
        w.create_entity("CHARACTER", Some("mara")).unwrap(),
        w.create_entity("PLACE", Some("varrow")).unwrap(),
        w.create_entity("PLACE", Some("ashford")).unwrap(),
    ]
}

fn value_for(ents: &[EntityId], attr: usize, v: usize) -> Value {
    if attr == 2 {
        Value::Str(NAMES[v % NAMES.len()].into())
    } else {
        Value::Ref(ents[v % ents.len()])
    }
}

// ---------------------------------------------------------------- probes

fn dbg<T: std::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// Every read of `w` at the latest canon, through the three routes.
fn assert_paths_agree(w: &World, ents: &[EntityId], label: &str) {
    let canon = w.latest_tx();
    let main = w.at_canon_main(canon);
    let latest_view = w.at_canon(canon);
    for &e in ents {
        for (ai, attr) in ATTRS.iter().enumerate() {
            let h = dbg(w.history(e, attr));
            assert_eq!(
                h,
                dbg(main.history(e, attr)),
                "{label}: history {e:?} {attr}"
            );
            assert_eq!(
                h,
                dbg(latest_view.history(e, attr)),
                "{label}: history(view) {e:?} {attr}"
            );
            for &tk in &TICKS {
                let t = Tick(tk);
                let a = dbg(w.as_of(e, attr, t));
                assert_eq!(
                    a,
                    dbg(main.as_of(e, attr, t)),
                    "{label}: as_of {e:?} {attr} @{tk}"
                );
                assert_eq!(
                    a,
                    dbg(latest_view.as_of(e, attr, t)),
                    "{label}: as_of(view) {e:?} {attr} @{tk}"
                );
                for vi in 0..ents.len().max(NAMES.len()) {
                    let v = value_for(ents, ai, vi);
                    let x = dbg(w.as_of_value(e, attr, &v, t));
                    assert_eq!(
                        x,
                        dbg(main.as_of_value(e, attr, &v, t)),
                        "{label}: as_of_value {e:?} {attr} {v:?} @{tk}"
                    );
                    assert_eq!(
                        x,
                        dbg(latest_view.as_of_value(e, attr, &v, t)),
                        "{label}: as_of_value(view) {e:?} {attr} {v:?} @{tk}"
                    );
                }
                if ai < 2 {
                    for dir in [Direction::Out, Direction::In, Direction::Both] {
                        let n = dbg(w.neighbors(e, attr, dir, t));
                        assert_eq!(
                            n,
                            dbg(main.neighbors(e, attr, dir, t)),
                            "{label}: neighbors {e:?} {attr} {dir:?} @{tk}"
                        );
                        assert_eq!(
                            n,
                            dbg(latest_view.neighbors(e, attr, dir, t)),
                            "{label}: neighbors(view) {e:?} {attr} {dir:?} @{tk}"
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- actions

#[derive(Clone, Debug)]
enum Act {
    Assert {
        e: usize,
        a: usize,
        v: usize,
        t: usize,
    },
    Retract {
        e: usize,
        a: usize,
        v: usize,
        t: usize,
    },
    Delete {
        n: usize,
    },
    Replace {
        n: usize,
        v: Option<usize>,
        t: Option<usize>,
        op: Option<bool>,
    },
}

#[derive(Clone, Debug)]
struct Step {
    acts: Vec<Act>,
    as_txn: bool,
}

fn act_strategy() -> impl Strategy<Value = Act> {
    let e = 0usize..5;
    let a = 0usize..3;
    let v = 0usize..5;
    let t = 0usize..TICKS.len();
    prop_oneof![
        4 => (e.clone(), a.clone(), v.clone(), t.clone())
            .prop_map(|(e, a, v, t)| Act::Assert { e, a, v, t }),
        3 => (e, a, v, t.clone()).prop_map(|(e, a, v, t)| Act::Retract { e, a, v, t }),
        2 => (0usize..64).prop_map(|n| Act::Delete { n }),
        5 => (
            0usize..64,
            prop::option::of(0usize..5),
            prop::option::of(t),
            prop::option::of(any::<bool>()),
        )
            .prop_map(|(n, v, t, op)| Act::Replace { n, v, t, op }),
    ]
}

fn step_strategy() -> impl Strategy<Value = Step> {
    (prop::collection::vec(act_strategy(), 1..4), any::<bool>())
        .prop_map(|(acts, as_txn)| Step { acts, as_txn })
}

/// The entries the run has seen so far, with the attribute index each
/// belongs to (a correction needs a value of the right type).
#[derive(Default)]
struct Seen(Vec<(EntryId, usize)>);

impl Seen {
    fn pick(&self, n: usize) -> Option<(EntryId, usize)> {
        if self.0.is_empty() {
            None
        } else {
            Some(self.0[n % self.0.len()])
        }
    }
}

fn correction(
    ents: &[EntityId],
    attr: usize,
    v: Option<usize>,
    t: Option<usize>,
    op: Option<bool>,
) -> Correction {
    Correction::Replace {
        value: v.map(|v| value_for(ents, attr, v)),
        tick: t.map(|t| Tick(TICKS[t])),
        op: op.map(|r| if r { Op::Retract } else { Op::Assert }),
        precision: None,
    }
}

/// Applies one action to an auto-commit world (errors and no-ops are fine:
/// the property is about reads, not about which writes succeed).
fn apply_auto(w: &mut World, ents: &[EntityId], seen: &mut Seen, act: &Act) {
    let r = match act {
        Act::Assert { e, a, v, t } => w
            .assert(
                ents[*e],
                ATTRS[*a],
                value_for(ents, *a, *v),
                Some(Tick(TICKS[*t])),
            )
            .map(|r| (r, *a)),
        Act::Retract { e, a, v, t } => w
            .retract(
                ents[*e],
                ATTRS[*a],
                value_for(ents, *a, *v),
                Some(Tick(TICKS[*t])),
            )
            .map(|r| (r, *a)),
        Act::Delete { n } => match seen.pick(*n) {
            Some((id, a)) => w.correct(id, Correction::Delete).map(|r| (r, a)),
            None => return,
        },
        Act::Replace { n, v, t, op } => match seen.pick(*n) {
            Some((id, a)) => w
                .correct(id, correction(ents, a, *v, *t, *op))
                .map(|r| (r, a)),
            None => return,
        },
    };
    if let Ok((r, a)) = r {
        seen.0.extend(r.entries.iter().map(|id| (*id, a)));
    }
}

fn apply_txn(w: &mut World, ents: &[EntityId], seen: &mut Seen, acts: &[Act]) {
    let mut tx = w.begin();
    let mut attrs_in_txn = Vec::new();
    for act in acts {
        let ok = match act {
            Act::Assert { e, a, v, t } => {
                attrs_in_txn.push(*a);
                tx.assert(
                    ents[*e],
                    ATTRS[*a],
                    value_for(ents, *a, *v),
                    Some(Tick(TICKS[*t])),
                )
                .is_ok()
            }
            Act::Retract { e, a, v, t } => {
                attrs_in_txn.push(*a);
                tx.retract(
                    ents[*e],
                    ATTRS[*a],
                    value_for(ents, *a, *v),
                    Some(Tick(TICKS[*t])),
                )
                .is_ok()
            }
            Act::Delete { n } => match seen.pick(*n) {
                Some((id, a)) => {
                    attrs_in_txn.push(a);
                    tx.correct(id, Correction::Delete).is_ok()
                }
                None => true,
            },
            Act::Replace { n, v, t, op } => match seen.pick(*n) {
                Some((id, a)) => {
                    attrs_in_txn.push(a);
                    tx.correct(id, correction(ents, a, *v, *t, *op)).is_ok()
                }
                None => true,
            },
        };
        if !ok {
            break; // poisoned: commit reports TransactionAborted
        }
    }
    // the entry ids of a committed txn go back into `seen`; attribute index
    // is not known per entry here, so use the commit's entries via lookup
    if let Ok(c) = tx.commit() {
        for id in c.entries {
            if let Some(a) = attr_index_of(w, id) {
                seen.0.push((id, a));
            }
        }
    }
}

fn attr_index_of(w: &World, id: EntryId) -> Option<usize> {
    use kala_core::log::EntryBody;
    let attr = match w.entry(id)?.body {
        EntryBody::Fact { attr, .. }
        | EntryBody::Deleted { attr, .. }
        | EntryBody::Revised { attr, .. } => attr,
        _ => return None,
    };
    // resolve the attribute id to its index in ATTRS through the schema
    // entries (DefineAttr), which sit before any fact
    let mut i = 0;
    loop {
        let info = w.entry(EntryId(i))?;
        if let EntryBody::DefineAttr { id, def } = info.body
            && id == attr
        {
            return ATTRS.iter().position(|n| *n == def.name);
        }
        i += 1;
        if i > 4096 {
            return None;
        }
    }
}

fn run_steps(w: &mut World, ents: &[EntityId], seen: &mut Seen, steps: &[Step], label: &str) {
    for (i, s) in steps.iter().enumerate() {
        if s.as_txn {
            apply_txn(w, ents, seen, &s.acts);
        } else {
            for a in &s.acts {
                apply_auto(w, ents, seen, a);
            }
        }
        assert_paths_agree(w, ents, &format!("{label} step {i} {s:?}"));
    }
}

fn run(batch1: Vec<Step>, batch2: Vec<Step>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let ents = populate(&mut w);
    let mut seen = Seen::default();
    run_steps(&mut w, &ents, &mut seen, &batch1, "live");
    let before = (w.latest_tx(), w.latest_tick());
    drop(w);

    let mut w = World::open(&path).unwrap();
    assert_eq!((w.latest_tx(), w.latest_tick()), before);
    assert_paths_agree(&w, &ents, "reopened");
    run_steps(&mut w, &ents, &mut seen, &batch2, "after reopen");
    drop(w);
    let w = World::open(&path).unwrap();
    assert_paths_agree(&w, &ents, "reopened twice");
}

proptest! {
    // A fixed seed keeps the run deterministic (failures also persist to the
    // regressions file like model.rs); shrinking is on.
    #![proptest_config(ProptestConfig {
        cases: 48,
        rng_seed: RngSeed::Fixed(0x4b41_4c41_0005),
        .. ProptestConfig::default()
    })]

    #[test]
    fn latest_index_reads_equal_main_index_reads_before_and_after_reopen(
        batch1 in prop::collection::vec(step_strategy(), 1..14),
        batch2 in prop::collection::vec(step_strategy(), 0..5),
    ) {
        run(batch1, batch2);
    }
}

// ---------------------------------------------------------------- examples

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn mem() -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = populate(&mut w);
    (w, e)
}

fn rep(value: Option<Value>, tick: Option<Tick>, op: Option<Op>) -> Correction {
    Correction::Replace {
        value,
        tick,
        op,
        precision: None,
    }
}

fn only(r: WriteResult) -> EntryId {
    assert_eq!(r.entries.len(), 1);
    r.entries[0]
}

#[test]
fn greg_walk_with_a_moved_retract_agrees_and_reads_the_current_revision() {
    let (mut w, e) = mem();
    let (greg, varrow) = (e[0], Value::Ref(e[3]));
    w.assert(greg, "located_in", varrow.clone(), t(-300))
        .unwrap();
    let r = only(
        w.retract(greg, "located_in", varrow.clone(), t(-250))
            .unwrap(),
    );
    assert_paths_agree(&w, &e, "walk");
    w.correct(r, rep(None, t(-280), None)).unwrap();
    assert_paths_agree(&w, &e, "moved");
    let main = w.at_canon_main(w.latest_tx());
    for view_holds in [
        main.as_of_value(greg, "located_in", &varrow, Tick(-290))
            .unwrap(),
        w.as_of_value(greg, "located_in", &varrow, Tick(-290))
            .unwrap(),
    ] {
        assert!(view_holds);
    }
    for view_holds in [
        main.as_of_value(greg, "located_in", &varrow, Tick(-260))
            .unwrap(),
        w.as_of_value(greg, "located_in", &varrow, Tick(-260))
            .unwrap(),
    ] {
        assert!(!view_holds, "the moved retract already applies at -260");
    }
    // the older canon is unchanged (main path; the latest index never serves it)
    let old = w.at_canon(TxId(w.latest_tx().0 - 1));
    assert!(
        old.as_of_value(greg, "located_in", &varrow, Tick(-260))
            .unwrap()
    );
}

#[test]
fn a_deleted_key_is_absent_and_a_later_reassert_at_it_is_present() {
    let (mut w, e) = mem();
    let (greg, name) = (e[0], Value::Str("Greg".into()));
    let a = only(w.assert(greg, "name", name.clone(), t(-5)).unwrap());
    w.correct(a, Correction::Delete).unwrap();
    assert_paths_agree(&w, &e, "deleted");
    assert!(!w.as_of_value(greg, "name", &name, Tick(0)).unwrap());
    assert!(w.history(greg, "name").unwrap().is_empty());
    w.assert(greg, "name", name.clone(), t(-5)).unwrap();
    assert_paths_agree(&w, &e, "re-asserted");
    assert!(w.as_of_value(greg, "name", &name, Tick(0)).unwrap());
    assert_eq!(w.history(greg, "name").unwrap().len(), 1);
}

#[test]
fn an_older_deleted_entry_under_a_newer_one_at_the_same_key_does_not_leak() {
    let (mut w, e) = mem();
    let (greg, varrow) = (e[0], Value::Ref(e[3]));
    // assert @-300 stored, deleted, stored again at the same key, deleted again
    let a1 = only(
        w.assert(greg, "located_in", varrow.clone(), t(-300))
            .unwrap(),
    );
    w.correct(a1, Correction::Delete).unwrap();
    let a2 = only(
        w.assert(greg, "located_in", varrow.clone(), t(-300))
            .unwrap(),
    );
    assert_paths_agree(&w, &e, "second assert");
    assert!(w.as_of_value(greg, "located_in", &varrow, Tick(0)).unwrap());
    w.correct(a2, Correction::Delete).unwrap();
    assert_paths_agree(&w, &e, "second delete");
    assert!(!w.as_of_value(greg, "located_in", &varrow, Tick(0)).unwrap());
    assert!(w.history(greg, "located_in").unwrap().is_empty());
}

#[test]
fn a_bidirectional_replace_that_swaps_the_pair_agrees_from_both_sides() {
    let (mut w, e) = mem();
    let (greg, edwin, mara) = (e[0], e[1], e[2]);
    let pair = only(w.assert(edwin, "ally_of", Value::Ref(mara), t(5)).unwrap());
    assert_paths_agree(&w, &e, "pair");
    // value -> greg (< edwin): renormalised to (greg, edwin)
    w.correct(pair, rep(Some(Value::Ref(greg)), None, None))
        .unwrap();
    assert_paths_agree(&w, &e, "swapped");
    assert!(
        w.as_of_value(greg, "ally_of", &Value::Ref(edwin), Tick(5))
            .unwrap()
    );
    assert!(
        w.as_of_value(edwin, "ally_of", &Value::Ref(greg), Tick(5))
            .unwrap()
    );
    assert!(
        !w.as_of_value(edwin, "ally_of", &Value::Ref(mara), Tick(5))
            .unwrap()
    );
    assert!(w.as_of(mara, "ally_of", Tick(5)).unwrap().is_empty());
}

#[test]
fn a_move_onto_a_deleted_key_and_the_extreme_ticks_agree() {
    let (mut w, e) = mem();
    let (greg, varrow) = (e[0], Value::Ref(e[3]));
    w.assert(greg, "located_in", varrow.clone(), t(-300))
        .unwrap();
    let r = only(
        w.retract(greg, "located_in", varrow.clone(), t(-250))
            .unwrap(),
    );
    let a2 = only(w.assert(greg, "located_in", varrow.clone(), t(-1)).unwrap());
    w.correct(a2, Correction::Delete).unwrap();
    for target in [-1, i64::MIN, i64::MAX, 0] {
        w.correct(r, rep(None, t(target), None)).unwrap();
        assert_paths_agree(&w, &e, &format!("retract moved to {target}"));
    }
    // final: retract @0, assert @-300
    assert!(
        w.as_of_value(greg, "located_in", &varrow, Tick(-1))
            .unwrap()
    );
    assert!(!w.as_of_value(greg, "located_in", &varrow, Tick(0)).unwrap());
}

#[test]
fn a_file_world_rebuilds_the_latest_index_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    let (greg, varrow) = (e[0], Value::Ref(e[3]));
    w.assert(greg, "located_in", varrow.clone(), t(-300))
        .unwrap();
    let r = only(
        w.retract(greg, "located_in", varrow.clone(), t(-250))
            .unwrap(),
    );
    w.correct(r, rep(None, t(-280), None)).unwrap();
    let n = only(
        w.assert(greg, "name", Value::Str("Greg".into()), t(1))
            .unwrap(),
    );
    w.correct(n, Correction::Delete).unwrap();
    assert_paths_agree(&w, &e, "live");
    drop(w);
    let w = World::open(&path).unwrap();
    assert_paths_agree(&w, &e, "reopened");
    assert!(
        !w.as_of_value(greg, "located_in", &varrow, Tick(-270))
            .unwrap()
    );
    assert!(w.history(greg, "name").unwrap().is_empty());
}

#[test]
fn a_failed_or_noop_correction_leaves_both_paths_in_agreement() {
    let (mut w, e) = mem();
    let (greg, varrow) = (e[0], Value::Ref(e[3]));
    let a = only(
        w.assert(greg, "located_in", varrow.clone(), t(-300))
            .unwrap(),
    );
    let r = only(
        w.retract(greg, "located_in", varrow.clone(), t(-250))
            .unwrap(),
    );
    // same-tick conflict (assert moved onto the retract's tick) and a no-op replace
    assert!(w.correct(a, rep(None, t(-250), None)).is_err());
    assert_eq!(w.correct(r, rep(None, None, None)).unwrap().changed, 0);
    assert_paths_agree(&w, &e, "failed + noop");
    // a poisoned txn
    let mut tx = w.begin();
    tx.correct(r, rep(None, t(-280), None)).unwrap();
    assert!(tx.correct(r, rep(None, t(-270), None)).is_err());
    assert!(tx.commit().is_err());
    assert_paths_agree(&w, &e, "aborted txn");
    assert!(
        w.as_of_value(greg, "located_in", &varrow, Tick(-260))
            .unwrap()
    );
}

// ---------------------------------------------------------------- item 5 confirm

/// `at_canon(TxId(u64::MAX))` clamps to the latest canon: it must read the
/// latest path and agree with the world's reads and with the main path.
#[test]
fn at_canon_u64_max_agrees_with_world_reads_and_the_main_path() {
    let (mut w, e) = mem();
    let (greg, edwin, mara) = (e[0], e[1], e[2]);
    let (varrow, ashford) = (Value::Ref(e[3]), Value::Ref(e[4]));
    let a = only(
        w.assert(greg, "located_in", varrow.clone(), t(-300))
            .unwrap(),
    );
    let r = only(
        w.retract(greg, "located_in", varrow.clone(), t(-250))
            .unwrap(),
    );
    w.assert(greg, "located_in", ashford.clone(), t(-250))
        .unwrap();
    w.assert(edwin, "ally_of", Value::Ref(mara), t(5)).unwrap();
    w.correct(r, rep(None, t(-280), None)).unwrap();
    w.correct(a, Correction::Delete).unwrap();
    w.assert(greg, "name", Value::Str("Greg".into()), t(0))
        .unwrap();

    let max = w.at_canon(TxId(u64::MAX));
    assert_eq!(max.canon(), w.latest_tx());
    let main = w.at_canon_main(TxId(u64::MAX));
    assert_eq!(main.canon(), w.latest_tx());
    for &en in &e {
        for (ai, attr) in ATTRS.iter().enumerate() {
            let h = dbg(w.history(en, attr));
            assert_eq!(h, dbg(max.history(en, attr)), "history {en:?} {attr}");
            assert_eq!(
                h,
                dbg(main.history(en, attr)),
                "history(main) {en:?} {attr}"
            );
            for &tk in &TICKS {
                let tt = Tick(tk);
                let x = dbg(w.as_of(en, attr, tt));
                assert_eq!(x, dbg(max.as_of(en, attr, tt)), "as_of {en:?} {attr} @{tk}");
                assert_eq!(x, dbg(main.as_of(en, attr, tt)));
                for vi in 0..e.len().max(NAMES.len()) {
                    let v = value_for(&e, ai, vi);
                    let x = dbg(w.as_of_value(en, attr, &v, tt));
                    assert_eq!(x, dbg(max.as_of_value(en, attr, &v, tt)));
                    assert_eq!(x, dbg(main.as_of_value(en, attr, &v, tt)));
                }
                if ai < 2 {
                    for dir in [Direction::Out, Direction::In, Direction::Both] {
                        let n = dbg(w.neighbors(en, attr, dir, tt));
                        assert_eq!(n, dbg(max.neighbors(en, attr, dir, tt)));
                        assert_eq!(n, dbg(main.neighbors(en, attr, dir, tt)));
                    }
                }
            }
        }
    }
    // spot-check the actual answers: Greg left Varrow at -280 (moved), a1 deleted
    assert!(
        !max.as_of_value(greg, "located_in", &varrow, Tick(-290))
            .unwrap(),
        "the assert was deleted"
    );
    assert!(
        max.as_of_value(greg, "located_in", &ashford, Tick(-250))
            .unwrap()
    );
}

fn file_world() -> (tempfile::TempDir, std::path::PathBuf, World, Vec<EntityId>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    (dir, path, w, e)
}

/// Reads of the world after a failed commit: nothing of the failed
/// transaction on either path.
fn assert_unchanged(w: &World, e: &[EntityId], tx: TxId, label: &str) {
    assert_eq!(w.latest_tx(), tx, "{label}: latest_tx");
    assert_paths_agree(w, e, label);
    let (greg, varrow, ashford) = (e[0], Value::Ref(e[3]), Value::Ref(e[4]));
    let main = w.at_canon_main(tx);
    for (what, via_main) in [("latest", false), ("main", true)] {
        for tk in [-300, -250, 0, 1000, i64::MAX] {
            let tt = Tick(tk);
            let (v, a, n) = if via_main {
                (
                    main.as_of_value(greg, "located_in", &varrow, tt).unwrap(),
                    main.as_of_value(greg, "located_in", &ashford, tt).unwrap(),
                    main.as_of_value(greg, "name", &Value::Str("Gregor".into()), tt)
                        .unwrap(),
                )
            } else {
                (
                    w.as_of_value(greg, "located_in", &varrow, tt).unwrap(),
                    w.as_of_value(greg, "located_in", &ashford, tt).unwrap(),
                    w.as_of_value(greg, "name", &Value::Str("Gregor".into()), tt)
                        .unwrap(),
                )
            };
            assert_eq!(v, tk >= -300, "{label}/{what}: Varrow @{tk}");
            assert!(!a, "{label}/{what}: Ashford @{tk}");
            assert!(!n, "{label}/{what}: Gregor @{tk}");
        }
        let h = if via_main {
            main.history(greg, "located_in").unwrap()
        } else {
            w.history(greg, "located_in").unwrap()
        };
        assert_eq!(h.len(), 1, "{label}/{what}: history");
        let nb = if via_main {
            main.neighbors(e[3], "located_in", Direction::In, Tick(0))
                .unwrap()
        } else {
            w.neighbors(e[3], "located_in", Direction::In, Tick(0))
                .unwrap()
        };
        assert_eq!(nb.len(), 1, "{label}/{what}: neighbors");
    }
}

/// A failed append poisons the world until it is reopened (documented), so
/// each scenario runs on a freshly opened world.
fn failed_commit_shows_nothing(fault: LogFault) {
    for scenario in 0..5 {
        let (_d, path, mut w, e) = file_world();
        let (greg, varrow, ashford) = (e[0], Value::Ref(e[3]), Value::Ref(e[4]));
        let a = only(
            w.assert(greg, "located_in", varrow.clone(), t(-300))
                .unwrap(),
        );
        let tx = w.latest_tx();
        w.inject_log_fault(fault);
        let label = match scenario {
            0 => {
                assert!(
                    w.assert(greg, "located_in", ashford.clone(), t(-250))
                        .is_err()
                );
                "failed assert"
            }
            1 => {
                assert!(
                    w.retract(greg, "located_in", varrow.clone(), t(-250))
                        .is_err()
                );
                "failed retract"
            }
            2 => {
                assert!(w.correct(a, Correction::Delete).is_err());
                "failed delete"
            }
            3 => {
                assert!(w.correct(a, rep(None, t(-290), None)).is_err());
                "failed move"
            }
            _ => {
                let mut x = w.begin();
                x.assert(greg, "located_in", ashford.clone(), t(-100))
                    .unwrap();
                x.assert(greg, "name", Value::Str("Gregor".into()), t(-100))
                    .unwrap();
                assert!(x.commit().is_err());
                "failed txn"
            }
        };
        assert_unchanged(&w, &e, tx, label);

        // the same state after reopen, and the world still works
        drop(w);
        let mut w = World::open(&path).unwrap();
        assert_unchanged(&w, &e, tx, &format!("{label}, reopened"));
        w.assert(greg, "located_in", ashford.clone(), t(-250))
            .unwrap();
        assert!(
            w.as_of_value(greg, "located_in", &ashford, Tick(-250))
                .unwrap()
        );
    }
}

#[test]
fn a_failed_sync_leaves_no_trace_on_either_read_path() {
    failed_commit_shows_nothing(LogFault::FailNextSync);
}

#[test]
fn a_failed_write_leaves_no_trace_on_either_read_path() {
    failed_commit_shows_nothing(LogFault::FailNextWrite);
}

/// One key flipped fact -> DELETED -> fact -> DELETED -> fact by corrections
/// and re-asserts; every canon agrees with the hand-computed answer on both
/// paths, live and after reopen.
#[test]
fn a_key_flipped_between_fact_and_deleted_agrees_at_every_canon() {
    let (_d, path, mut w, e) = file_world();
    let (greg, name) = (e[0], Value::Str("Greg".into()));
    let mut expect: Vec<(TxId, bool)> = vec![(w.latest_tx(), false)];
    let mut cur = only(w.assert(greg, "name", name.clone(), t(-5)).unwrap());
    expect.push((w.latest_tx(), true));
    for _round in 0..3 {
        w.correct(cur, Correction::Delete).unwrap();
        expect.push((w.latest_tx(), false));
        cur = only(w.assert(greg, "name", name.clone(), t(-5)).unwrap());
        expect.push((w.latest_tx(), true));
    }
    w.correct(cur, Correction::Delete).unwrap();
    expect.push((w.latest_tx(), false));

    fn check(w: &World, e: &[EntityId], expect: &[(TxId, bool)], label: &str) {
        let (greg, name) = (e[0], Value::Str("Greg".into()));
        for &(c, holds) in expect {
            let main = w.at_canon_main(c);
            let any = w.at_canon(c);
            for tk in [i64::MIN, -6, -5, 0, i64::MAX] {
                let tt = Tick(tk);
                let want = holds && tk >= -5;
                assert_eq!(
                    main.as_of_value(greg, "name", &name, tt).unwrap(),
                    want,
                    "{label}: main canon {c:?} @{tk}"
                );
                assert_eq!(
                    any.as_of_value(greg, "name", &name, tt).unwrap(),
                    want,
                    "{label}: at_canon canon {c:?} @{tk}"
                );
            }
            assert_eq!(
                dbg(main.history(greg, "name")),
                dbg(any.history(greg, "name")),
                "{label}: history canon {c:?}"
            );
            assert_eq!(
                main.history(greg, "name").unwrap().len(),
                usize::from(holds),
                "{label}: history len canon {c:?}"
            );
        }
        let last = expect.last().unwrap();
        assert!(!last.1);
        assert!(
            !w.as_of_value(greg, "name", &name, Tick(0)).unwrap(),
            "{label}: world read at the latest canon"
        );
        assert!(w.history(greg, "name").unwrap().is_empty());
        assert_paths_agree(w, e, label);
    }
    check(&w, &e, &expect, "live");
    drop(w);
    let w = World::open(&path).unwrap();
    check(&w, &e, &expect, "reopened");
}
