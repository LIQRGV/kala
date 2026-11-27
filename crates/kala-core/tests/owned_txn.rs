//! The owned transaction (slice 15c, stage 1): a transaction that owns its
//! world handle, so it can be kept in a struct and borrowed again for each
//! call (a Bolt session keeps one open across several messages). Everything
//! it stages, records and reads must survive between borrows, a reader of
//! its world must see the committed snapshot only, and what it commits must
//! be what an ordinary [`Txn`] making the same operations commits, byte for
//! byte.

mod common;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kala_core::doctor::{DoctorOptions, doctor, doctor_log};
use kala_core::*;

fn schema() -> Schema {
    let attr = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("tag", ValueType::Str, Kind::State, Cardinality::Many))
        .attribute(attr("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("present", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(attr(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
}

#[derive(Clone, Copy)]
struct Cast {
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

/// A new world: the cast, Greg and Edwin in Varrow from 600, and a fact at
/// 601, the latest tick.
fn seed(path: &Path) -> (World, Cast) {
    let mut w = World::create(path, schema()).unwrap();
    let mut t = w.begin();
    let cast = Cast {
        greg: t.create_entity("CHARACTER", Some("greg")).unwrap(),
        edwin: t.create_entity("CHARACTER", Some("edwin")).unwrap(),
        varrow: t.create_entity("PLACE", Some("varrow")).unwrap(),
        ashford: t.create_entity("PLACE", Some("ashford")).unwrap(),
    };
    for who in [cast.greg, cast.edwin] {
        t.assert(who, "located_in", Value::Ref(cast.varrow), Some(Tick(600)))
            .unwrap();
    }
    t.assert(cast.varrow, "tag", s("founded"), Some(Tick(601)))
        .unwrap();
    t.commit().unwrap();
    (w, cast)
}

struct Seeded {
    _dir: tempfile::TempDir,
    path: PathBuf,
    w: World,
    c: Cast,
}

fn seeded() -> Seeded {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let (w, c) = seed(&path);
    Seeded {
        _dir: dir,
        path,
        w,
        c,
    }
}

/// The bytes of a seeded file after one writer ran `f` on it.
fn serial(f: impl FnOnce(&mut World, Cast)) -> Vec<u8> {
    let s = seeded();
    let mut w = s.w;
    f(&mut w, s.c);
    bytes(&s.path)
}

fn s(text: &str) -> Value {
    Value::Str(text.into())
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn clean(w: &World, path: &Path) {
    let report = doctor(w, &DoctorOptions::default());
    assert!(report.is_clean(), "{report:?}");
    let report = doctor_log(path, &DoctorOptions::default()).unwrap();
    assert!(report.is_clean(), "{report:?}");
}

fn far() -> Tick {
    Tick(i64::MAX)
}

/// Holds the log file's writer lock the way another process would.
fn hold_lock(path: &Path) -> File {
    let f = OpenOptions::new().read(true).open(path).unwrap();
    f.try_lock().expect("the lock is free");
    f
}

/// `:set` at the native level: read what holds, retract it, assert the new.
fn set_location(t: &mut Txn<'_>, who: EntityId, to: EntityId) {
    let tick = t.write_tick(None);
    let meter = WorkMeter::bounded(10_000, None);
    let held = t
        .values_at(who, "located_in", tick, 16, 1 << 20, &meter)
        .unwrap();
    for value in held.values {
        t.retract(who, "located_in", value, Some(tick)).unwrap();
    }
    t.assert(who, "located_in", Value::Ref(to), Some(tick))
        .unwrap();
}

// ------------------------------------------------- state between borrows

/// The operations of the "several borrows" tests, through any transaction.
/// `step` picks which part runs, so the owned form can make each part in
/// its own borrow and the ordinary form all of them in one.
fn story(t: &mut Txn<'_>, c: Cast, step: usize) {
    match step {
        0 => {
            let lisa = t.create_entity("CHARACTER", Some("lisa")).unwrap();
            assert_eq!(t.entity_by_handle("lisa"), Some(lisa));
        }
        1 => {
            // The handle staged in an earlier borrow resolves here.
            let lisa = t.entity_by_handle("lisa").expect("staged in borrow 0");
            t.assert(lisa, "tag", s("new"), Some(Tick(5))).unwrap();
            t.assert(lisa, "present", Value::Ref(c.varrow), Some(Tick(6)))
                .unwrap();
        }
        2 => {
            // The edge interned in an earlier borrow is found, and carries
            // a property.
            let lisa = t.entity_by_handle("lisa").unwrap();
            let edge = t
                .edge_id(lisa, "present", c.varrow)
                .expect("interned in borrow 1");
            t.assert(edge.into(), "disguised", Value::Bool(true), Some(Tick(6)))
                .unwrap();
            // A write judged against a write staged in an earlier borrow.
            let again = t.assert(lisa, "tag", s("new"), Some(Tick(5))).unwrap();
            assert_eq!(again.changed, 0, "already staged in borrow 1");
            // A tick-less write: the latest tick the transaction began with.
            let res = t.assert(c.greg, "tag", s("late"), None).unwrap();
            assert_eq!(res.tick, Tick(601));
        }
        _ => unreachable!(),
    }
}

#[test]
fn staged_state_survives_between_borrows_and_commits_the_same_bytes() {
    let sd = seeded();
    let c = sd.c;
    let mut owned = sd.w.into_transaction();
    assert_eq!(owned.snapshot_tx(), TxId(1));
    for step in 0..3 {
        owned.with(|t| story(t, c, step));
    }
    assert_eq!(owned.with(|t| t.operation_count()), 6);
    assert_eq!(owned.with(|t| t.staged_entry_count()), 4);
    let before = bytes(&sd.path);
    assert_eq!(before, serial(|_, _| {}), "nothing written before commit");
    let (w, result) = owned.commit();
    let result = result.unwrap();
    assert!(result.committed);
    assert_eq!(result.tx, TxId(2));
    assert_eq!(result.changed, 4);
    assert_eq!(result.entries.len(), 4);
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.finalized, None);

    let expected = serial(|w, c| {
        let mut t = w.begin();
        for step in 0..3 {
            story(&mut t, c, step);
        }
        t.commit().unwrap();
    });
    assert_eq!(
        bytes(&sd.path),
        expected,
        "byte-identical to an ordinary Txn"
    );

    // The returned handle has the commit applied and is usable.
    let lisa = w.entity_by_handle("lisa").unwrap();
    assert_eq!(w.as_of(lisa, "tag", far()).unwrap(), vec![s("new")]);
    assert_eq!(w.latest_tx(), TxId(2));
    clean(&w, &sd.path);
    let mut w = w;
    w.assert(lisa, "tag", s("after"), Some(Tick(9))).unwrap();
    assert_eq!(w.latest_tx(), TxId(3));
}

#[test]
fn an_owned_transaction_is_send_and_works_in_memory() {
    fn is_send<T: Send>() {}
    is_send::<OwnedTxn>();
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut owned = w.into_transaction();
    owned.with(|t| {
        t.assert(greg, "tag", s("a"), Some(Tick(1)))
            .map(drop)
            .unwrap()
    });
    owned.with(|t| {
        t.assert(greg, "tag", s("b"), Some(Tick(1)))
            .map(drop)
            .unwrap()
    });
    assert!(owned.world().as_of(greg, "tag", far()).unwrap().is_empty());
    let (w, result) = owned.commit();
    assert_eq!(result.unwrap().changed, 2);
    assert_eq!(w.as_of(greg, "tag", far()).unwrap(), vec![s("a"), s("b")]);
}

// ------------------------------------------------- rollback, drop, poison

#[test]
fn rollback_and_drop_write_nothing_and_leave_the_handle_usable() {
    let sd = seeded();
    let c = sd.c;
    let before = bytes(&sd.path);
    let stage = |owned: &mut OwnedTxn| {
        owned.with(|t| {
            t.create_entity("CHARACTER", Some("lisa")).unwrap();
            t.declare_node_type("FACTION").unwrap();
            t.declare_attribute(AttrDef::new(
                "height",
                ValueType::I64,
                Kind::State,
                Cardinality::One,
            ))
            .unwrap();
        });
        owned.with(|t| {
            t.assert(c.greg, "height", Value::I64(180), Some(Tick(1)))
                .unwrap();
        });
    };

    // Rollback returns the handle: nothing of the transaction is in it.
    let mut owned = sd.w.into_transaction();
    stage(&mut owned);
    let mut w = owned.rollback();
    assert_eq!(bytes(&sd.path), before);
    assert_eq!(w.entity_by_handle("lisa"), None);
    assert!(w.attribute("height").is_none());
    assert!(!w.node_types().any(|n| n == "FACTION"));
    assert_eq!(w.latest_tx(), TxId(1));
    // The handle, the names and the IDs are free again.
    let mut t = w.begin();
    let lisa = t.create_entity("CHARACTER", Some("lisa")).unwrap();
    t.declare_node_type("FACTION").unwrap();
    t.commit().unwrap();
    assert_eq!(lisa.seq(), 3, "the rolled-back create took no number");
    clean(&w, &sd.path);
    drop(w);

    // Drop: the same, seen through another handle, and no lock is left.
    let after_first = bytes(&sd.path);
    let mut owned = World::open(&sd.path).unwrap().into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("dropped"), Some(Tick(1)))
            .unwrap();
        t.declare_node_type("GUILD").unwrap();
    });
    drop(owned);
    assert_eq!(bytes(&sd.path), after_first);
    let mut other = World::open_waiting(&sd.path, Duration::ZERO).unwrap();
    other
        .assert(c.greg, "tag", s("kept"), Some(Tick(1)))
        .unwrap();
    assert_eq!(other.as_of(c.greg, "tag", far()).unwrap(), vec![s("kept")]);
}

#[test]
fn a_failed_operation_poisons_across_borrows() {
    let sd = seeded();
    let c = sd.c;
    let before = bytes(&sd.path);
    let mut owned = sd.w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("ok"), Some(Tick(1))).unwrap();
    });
    let err = owned.with(|t| {
        t.assert(c.greg, "no_such", s("x"), Some(Tick(1)))
            .unwrap_err()
    });
    assert_eq!(err, Error::UnknownAttribute("no_such".into()));
    // A later borrow finds the transaction aborted.
    let err = owned.with(|t| {
        t.assert(c.greg, "tag", s("later"), Some(Tick(2)))
            .unwrap_err()
    });
    assert_eq!(err, Error::TransactionAborted);
    let (mut w, result) = owned.commit();
    assert_eq!(result.unwrap_err(), Error::TransactionAborted);
    assert_eq!(bytes(&sd.path), before, "nothing written");
    // The handle survives the failed commit.
    w.assert(c.greg, "tag", s("next"), Some(Tick(3))).unwrap();
    assert_eq!(w.as_of(c.greg, "tag", far()).unwrap(), vec![s("next")]);

    // `Txn::abort` (an adapter's own failure) is remembered too.
    let mut owned = w.into_transaction();
    owned.with(|t| t.abort());
    let err = owned.with(|t| t.create_entity("PLACE", None).unwrap_err());
    assert_eq!(err, Error::TransactionAborted);
}

#[test]
fn a_panic_inside_a_borrow_aborts_the_transaction_and_restores_the_schema() {
    let sd = seeded();
    let mut owned = sd.w.into_transaction();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        owned.with(|t| {
            t.declare_node_type("FACTION").unwrap();
            panic!("the caller's closure failed");
        })
    }));
    assert!(caught.is_err());
    assert!(!owned.world().node_types().any(|n| n == "FACTION"));
    let err = owned.with(|t| t.create_entity("PLACE", None).unwrap_err());
    assert_eq!(err, Error::TransactionAborted);
    let (w, result) = owned.commit();
    assert_eq!(result.unwrap_err(), Error::TransactionAborted);
    assert!(!w.node_types().any(|n| n == "FACTION"));
}

// ------------------------------------------------- schema edits and world()

#[test]
fn staged_schema_edits_are_visible_to_later_staged_ops_but_not_through_world() {
    let sd = seeded();
    let c = sd.c;
    let height = || AttrDef::new("height", ValueType::I64, Kind::State, Cardinality::One);
    let mut owned = sd.w.into_transaction();
    owned.with(|t| {
        t.declare_attribute(height()).unwrap();
        t.declare_node_type("FACTION").unwrap();
        t.rename_attribute("tag", "label").unwrap();
        t.world_setting("tick_unit", "day").unwrap();
    });
    // A reader of the snapshot sees the committed schema only.
    let committed_schema = |w: &World| {
        assert!(w.attribute("height").is_none());
        assert!(w.attribute("label").is_none());
        assert!(w.attribute("tag").is_some());
        assert!(!w.node_types().any(|n| n == "FACTION"));
        assert_eq!(
            w.as_of(c.varrow, "tag", far()).unwrap(),
            vec![s("founded")],
            "the old name still reads"
        );
        assert!(matches!(
            w.as_of(c.greg, "height", far()),
            Err(Error::UnknownAttribute(_))
        ));
        assert_eq!(w.world_setting("tick_unit"), None);
    };
    committed_schema(owned.world());
    // Later staged operations use the provisional schema.
    owned.with(|t| {
        t.assert(c.greg, "height", Value::I64(180), Some(Tick(1)))
            .unwrap();
        let guild = t.create_entity("FACTION", Some("guild")).unwrap();
        t.assert(guild, "label", s("old"), Some(Tick(2))).unwrap();
        assert_eq!(
            t.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap_err(),
            Error::UnknownAttribute("tag".into()),
            "renamed away inside the transaction"
        );
    });
    committed_schema(owned.world());
    let before = bytes(&sd.path);

    // The same operations through an ordinary Txn: the same bytes.
    let expected = serial(|w, c| {
        let mut t = w.begin();
        t.declare_attribute(height()).unwrap();
        t.declare_node_type("FACTION").unwrap();
        t.rename_attribute("tag", "label").unwrap();
        t.world_setting("tick_unit", "day").unwrap();
        t.assert(c.greg, "height", Value::I64(180), Some(Tick(1)))
            .unwrap();
        let guild = t.create_entity("FACTION", Some("guild")).unwrap();
        t.assert(guild, "label", s("old"), Some(Tick(2))).unwrap();
        let _ = t.assert(c.greg, "tag", s("x"), Some(Tick(1)));
        drop(t);
    });
    assert_eq!(
        before, expected,
        "nothing written yet, as after a dropped Txn"
    );
}

#[test]
fn committed_schema_edits_staged_across_borrows_match_an_ordinary_txn() {
    let sd = seeded();
    let c = sd.c;
    let height = || AttrDef::new("height", ValueType::I64, Kind::State, Cardinality::One);
    let mut owned = sd.w.into_transaction();
    owned.with(|t| {
        t.declare_attribute(height()).unwrap();
        t.declare_node_type("FACTION").unwrap();
    });
    owned.with(|t| t.rename_attribute("tag", "label").unwrap());
    owned.with(|t| {
        t.assert(c.greg, "height", Value::I64(180), Some(Tick(1)))
            .unwrap();
        let guild = t.create_entity("FACTION", Some("guild")).unwrap();
        t.assert(guild, "label", s("old"), Some(Tick(2))).unwrap();
    });
    let (w, result) = owned.commit();
    let result = result.unwrap();
    assert_eq!((result.changed, result.declared), (2, 3));
    assert_eq!(result.schema.len(), 3);
    let expected = serial(|w, c| {
        let mut t = w.begin();
        t.declare_attribute(height()).unwrap();
        t.declare_node_type("FACTION").unwrap();
        t.rename_attribute("tag", "label").unwrap();
        t.assert(c.greg, "height", Value::I64(180), Some(Tick(1)))
            .unwrap();
        let guild = t.create_entity("FACTION", Some("guild")).unwrap();
        t.assert(guild, "label", s("old"), Some(Tick(2))).unwrap();
        t.commit().unwrap();
    });
    assert_eq!(bytes(&sd.path), expected);
    assert!(w.attribute("height").is_some());
    assert!(w.attribute("tag").is_none());
    assert_eq!(
        w.as_of(c.varrow, "label", far()).unwrap(),
        vec![s("founded")]
    );
    assert!(w.node_types().any(|n| n == "FACTION"));
    clean(&w, &sd.path);
}

// ------------------------------------------------- the snapshot

#[test]
fn world_answers_at_the_snapshot_while_another_handle_commits() {
    for commit in [true, false] {
        let sd = seeded();
        let c = sd.c;
        let mut other = World::open(&sd.path).unwrap();
        let mut owned = sd.w.into_transaction();
        owned.with(|t| {
            t.assert(c.greg, "tag", s("mine"), Some(Tick(1))).unwrap();
        });
        other
            .assert(c.edwin, "tag", s("theirs"), Some(Tick(1)))
            .unwrap();
        other.create_entity("PLACE", Some("carnath")).unwrap();

        // Neither the newer commits nor the transaction's own writes.
        let snapshot = owned.world();
        assert_eq!(snapshot.latest_tx(), TxId(1));
        assert_eq!(owned.snapshot_tx(), TxId(1));
        assert!(snapshot.as_of(c.edwin, "tag", far()).unwrap().is_empty());
        assert!(snapshot.as_of(c.greg, "tag", far()).unwrap().is_empty());
        assert_eq!(snapshot.entity_by_handle("carnath"), None);
        // Another borrow doesn't move the snapshot either.
        owned.with(|t| {
            assert_eq!(t.snapshot_tx(), TxId(1));
            // (Looking "carnath" up through the transaction would be a
            // recorded read of a free handle, and conflict at commit.)
            assert!(t.edge_id(c.edwin, "located_in", c.ashford).is_none());
        });
        assert_eq!(owned.world().latest_tx(), TxId(1));

        let w = if commit {
            let (w, result) = owned.commit();
            let result = result.unwrap();
            assert_eq!(result.tx, TxId(4));
            assert_eq!(result.finalized.map(|f| f.applied), Some(2));
            assert_eq!(w.as_of(c.greg, "tag", far()).unwrap(), vec![s("mine")]);
            w
        } else {
            let w = owned.rollback();
            assert!(w.as_of(c.greg, "tag", far()).unwrap().is_empty());
            w
        };
        // Either way the handle has caught up with the other commits.
        assert_eq!(w.latest_tx(), TxId(if commit { 4 } else { 3 }));
        assert_eq!(w.as_of(c.edwin, "tag", far()).unwrap(), vec![s("theirs")]);
        assert!(w.entity_by_handle("carnath").is_some());
        clean(&w, &sd.path);
    }
}

#[test]
fn into_transaction_catches_up_first() {
    let sd = seeded();
    let c = sd.c;
    let mut other = World::open(&sd.path).unwrap();
    other
        .assert(c.edwin, "tag", s("theirs"), Some(Tick(700)))
        .unwrap();
    let mut owned = sd.w.into_transaction();
    assert_eq!(owned.snapshot_tx(), TxId(2));
    assert_eq!(
        owned.world().as_of(c.edwin, "tag", far()).unwrap(),
        vec![s("theirs")]
    );
    // The latest tick is the caught-up world's.
    let tick = owned.with(|t| t.assert(c.greg, "tag", s("x"), None).unwrap().tick);
    assert_eq!(tick, Tick(700));
}

// ------------------------------------------------- committing over newer commits

#[test]
fn a_commit_over_a_newer_commit_remaps_ids_and_returns_the_mapping() {
    let sd = seeded();
    let c = sd.c;
    let mut other = World::open(&sd.path).unwrap();
    let mut owned = sd.w.into_transaction();
    let kael = owned.with(|t| t.create_entity("CHARACTER", Some("kael")).unwrap());
    owned.with(|t| {
        t.assert(kael, "present", Value::Ref(c.varrow), Some(Tick(3)))
            .unwrap();
    });
    let edge = owned.with(|t| {
        let edge = t.edge_id(kael, "present", c.varrow).unwrap();
        t.assert(edge.into(), "disguised", Value::Bool(true), Some(Tick(3)))
            .unwrap();
        edge
    });
    // Another handle takes the same numbers first.
    let mut t = other.begin();
    let lisa = t.create_entity("CHARACTER", Some("lisa")).unwrap();
    t.assert(lisa, "present", Value::Ref(c.ashford), Some(Tick(1)))
        .unwrap();
    t.commit().unwrap();
    assert_eq!(lisa, kael, "both snapshots hand out the same next number");

    let (w, result) = owned.commit();
    let result = result.unwrap();
    let final_kael = w.entity_by_handle("kael").unwrap();
    assert_ne!(final_kael, kael);
    assert_eq!(result.ids.entities, vec![(kael, final_kael)]);
    let final_edge = w.edge_id(final_kael, "present", c.varrow).unwrap();
    assert_eq!(result.ids.edges, vec![(edge, final_edge)]);
    assert_eq!(result.ids.resolve(kael), final_kael);
    assert_eq!(result.ids.resolve_edge(edge), final_edge);
    assert_eq!(result.finalized.as_ref().map(|f| f.applied), Some(1));
    assert_eq!(
        w.as_of(final_edge.into(), "disguised", far()).unwrap(),
        vec![Value::Bool(true)]
    );

    let expected = serial(|w, c| {
        let mut t = w.begin();
        let lisa = t.create_entity("CHARACTER", Some("lisa")).unwrap();
        t.assert(lisa, "present", Value::Ref(c.ashford), Some(Tick(1)))
            .unwrap();
        t.commit().unwrap();
        let mut t = w.begin();
        let kael = t.create_entity("CHARACTER", Some("kael")).unwrap();
        t.assert(kael, "present", Value::Ref(c.varrow), Some(Tick(3)))
            .unwrap();
        let edge = t.edge_id(kael, "present", c.varrow).unwrap();
        t.assert(edge.into(), "disguised", Value::Bool(true), Some(Tick(3)))
            .unwrap();
        t.commit().unwrap();
    });
    assert_eq!(bytes(&sd.path), expected);
    clean(&w, &sd.path);
}

#[test]
fn an_invalidated_read_is_a_conflict_that_writes_nothing_and_returns_the_handle() {
    let sd = seeded();
    let c = sd.c;
    let mut other = World::open(&sd.path).unwrap();
    let mut owned = sd.w.into_transaction();
    // The read is recorded in one borrow, the write made in another.
    owned.with(|t| t.note_reverse_read(c.varrow, "located_in").unwrap());
    owned.with(|t| {
        t.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(650)))
            .unwrap();
    });
    other
        .retract(c.edwin, "located_in", Value::Ref(c.varrow), Some(Tick(650)))
        .unwrap();
    let before = bytes(&sd.path);
    let (w, result) = owned.commit();
    let Err(Error::Conflict(conflict)) = result else {
        panic!("expected a conflict, got {result:?}");
    };
    assert_eq!(conflict.key.kind(), "reverse");
    assert_eq!((conflict.operation, conflict.applied), (0, 1));
    assert_eq!(bytes(&sd.path), before, "a conflict writes nothing");

    // The handle is caught up; the transaction run again commits.
    assert_eq!(w.latest_tx(), TxId(2));
    let mut owned = w.into_transaction();
    owned.with(|t| t.note_reverse_read(c.varrow, "located_in").unwrap());
    owned.with(|t| {
        t.assert(c.greg, "tag", s("alone"), Some(Tick(650)))
            .unwrap();
    });
    let (w, result) = owned.commit();
    assert_eq!(result.unwrap().finalized, None);
    clean(&w, &sd.path);
}

#[test]
fn a_set_read_in_one_borrow_conflicts_and_weak_mode_lets_it_through() {
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        let sd = seeded();
        let c = sd.c;
        let mut other = World::open(&sd.path).unwrap();
        let mut owned = sd.w.into_transaction();
        owned.with(|t| t.set_concurrency(mode));
        owned.with(|t| {
            assert_eq!(t.concurrency(), mode, "the mode survives a borrow");
            set_location(t, c.greg, c.ashford);
        });
        let mut t = other.begin();
        let carnath = t.create_entity("PLACE", Some("carnath")).unwrap();
        set_location(&mut t, c.greg, carnath);
        t.commit().unwrap();
        let before = bytes(&sd.path);
        let (w, result) = owned.commit();
        match mode {
            Concurrency::Optimistic => {
                let Err(Error::Conflict(conflict)) = result else {
                    panic!("expected a conflict, got {result:?}");
                };
                assert_eq!(conflict.key.kind(), "entity-attribute");
                assert_eq!(bytes(&sd.path), before);
            }
            Concurrency::Weak => {
                // The lost update: the retract of Varrow is redundant now,
                // the assert of Ashford is stored next to Carnath.
                let result = result.unwrap();
                assert_eq!(result.changed, 1);
                let held = w.as_of(c.greg, "located_in", far()).unwrap();
                assert_eq!(held.len(), 2, "{held:?}");
            }
        }
        // (Weak mode left an overlap, which doctor reports as a warning.)
        let report = doctor(&w, &DoctorOptions::default());
        assert_eq!(report.is_clean(), mode == Concurrency::Optimistic);
        assert!(doctor_log(&sd.path, &DoctorOptions::default()).is_ok());
    }
}

#[test]
fn ids_exposed_set_in_one_borrow_makes_a_renumbering_a_conflict() {
    let sd = seeded();
    let mut other = World::open(&sd.path).unwrap();
    let mut owned = sd.w.into_transaction();
    owned.with(|t| {
        t.create_entity("CHARACTER", Some("kael")).unwrap();
    });
    owned.with(|t| t.set_ids_exposed(true));
    assert!(owned.with(|t| t.ids_exposed()));
    other.create_entity("CHARACTER", Some("lisa")).unwrap();
    let before = bytes(&sd.path);
    let (w, result) = owned.commit();
    let Err(Error::Conflict(conflict)) = result else {
        panic!("expected a conflict, got {result:?}");
    };
    assert!(matches!(conflict.key, ConflictKey::ExposedId { .. }));
    assert_eq!(bytes(&sd.path), before);
    assert_eq!(w.entity_by_handle("kael"), None);
    assert!(w.entity_by_handle("lisa").is_some());
}

// ------------------------------------------------- locks and errors at commit

#[test]
fn the_busy_timeout_applies_at_commit_and_the_handle_comes_back() {
    let sd = seeded();
    let c = sd.c;
    let mut w = sd.w;
    w.set_busy_timeout(Duration::from_millis(30));
    let mut owned = w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap();
    });
    let before = bytes(&sd.path);
    let lock = hold_lock(&sd.path);
    let started = std::time::Instant::now();
    let (w, result) = owned.commit();
    assert_eq!(result.unwrap_err(), Error::WorldLocked);
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert_eq!(bytes(&sd.path), before);
    assert_eq!(w.busy_timeout(), Duration::from_millis(30));
    drop(lock);
    // No lock was needed while the transaction was open; the handle works.
    let mut owned = w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap();
    });
    let (w, result) = owned.commit();
    assert!(result.unwrap().committed);
    clean(&w, &sd.path);
}

#[test]
fn an_open_owned_transaction_holds_no_lock() {
    let sd = seeded();
    let c = sd.c;
    let mut owned = sd.w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap();
    });
    // Another handle with no patience commits while it is open.
    let mut other = World::open_waiting(&sd.path, Duration::ZERO).unwrap();
    other.assert(c.edwin, "tag", s("y"), Some(Tick(1))).unwrap();
    let lock = hold_lock(&sd.path);
    drop(lock);
    let (_, result) = owned.commit();
    assert!(result.unwrap().committed);
}

#[test]
fn log_divergence_surfaces_from_commit_with_the_handle_returned() {
    let sd = seeded();
    let c = sd.c;
    let seed_bytes = bytes(&sd.path);
    let mut w = sd.w;
    w.assert(c.greg, "tag", s("seen"), Some(Tick(1))).unwrap();
    let mut owned = w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("mine"), Some(Tick(2))).unwrap();
    });
    // The file loses a transaction this handle applied: no longer its log.
    std::fs::write(&sd.path, &seed_bytes).unwrap();
    let (mut w, result) = owned.commit();
    assert_eq!(result.unwrap_err(), Error::LogDiverged);
    assert_eq!(bytes(&sd.path), seed_bytes, "never written");
    // Reads still answer from what the handle had applied; writes and
    // catch-ups are refused until the world is reopened.
    assert_eq!(w.as_of(c.greg, "tag", far()).unwrap(), vec![s("seen")]);
    assert_eq!(w.catch_up().unwrap_err(), Error::LogDiverged);
    let mut owned = w.into_transaction();
    let err = owned.with(|t| t.assert(c.greg, "tag", s("x"), Some(Tick(3))).unwrap_err());
    assert_eq!(err, Error::LogDiverged);
    let (_, result) = owned.commit();
    assert!(result.is_err());
    assert_eq!(bytes(&sd.path), seed_bytes);
}

#[test]
fn a_damaged_tail_surfaces_from_commit_and_is_cut_only_once_acknowledged() {
    let sd = seeded();
    let c = sd.c;
    // The handle is behind a transaction that is then damaged on disk.
    let mut third = World::open(&sd.path).unwrap();
    third
        .assert(c.greg, "tag", s("damaged"), Some(Tick(1)))
        .unwrap();
    drop(third);
    let mut b = bytes(&sd.path);
    let last = *common::spans(&b).last().unwrap();
    b[last.body_start + 6] ^= 0xFF;
    std::fs::write(&sd.path, &b).unwrap();
    let damaged = b;

    let mut owned = sd.w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("mine"), Some(Tick(2))).unwrap();
    });
    let (mut w, result) = owned.commit();
    assert!(
        matches!(result, Err(Error::RecoveryRequired { .. })),
        "{result:?}"
    );
    assert_eq!(bytes(&sd.path), damaged, "a refused commit touches nothing");

    // Still refused on the returned handle until the app acknowledges.
    let mut owned = w.into_transaction();
    let staged = owned.with(|t| t.assert(c.greg, "tag", s("mine"), Some(Tick(2))));
    let (back, result) = owned.commit();
    assert!(staged.is_err() || result.is_err());
    assert_eq!(bytes(&sd.path), damaged);
    w = back;

    w.acknowledge_recovery();
    let mut owned = w.into_transaction();
    owned.with(|t| {
        t.assert(c.greg, "tag", s("mine"), Some(Tick(2))).unwrap();
    });
    let (w, result) = owned.commit();
    assert!(result.unwrap().committed);
    assert_eq!(w.as_of(c.greg, "tag", far()).unwrap(), vec![s("mine")]);
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    clean(&w, &sd.path);
}

#[test]
fn a_transaction_that_staged_nothing_commits_without_writing() {
    let sd = seeded();
    let c = sd.c;
    let before = bytes(&sd.path);
    let mut owned = sd.w.into_transaction();
    let res = owned.with(|t| {
        t.assert(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(601)))
            .unwrap()
    });
    assert_eq!(res.changed, 0);
    let (w, result) = owned.commit();
    let result = result.unwrap();
    assert!(!result.committed);
    assert_eq!(result.tx, TxId(1));
    assert_eq!(bytes(&sd.path), before);
    assert_eq!(w.latest_tx(), TxId(1));
}
