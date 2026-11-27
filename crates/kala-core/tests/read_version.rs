//! A transaction whose reads were made on an older snapshot than the one
//! its handle began on (PR #21, second polish): `Txn::set_read_version`
//! makes the commit's read-set check start from that older snapshot, and
//! `Txn::set_start_tick` gives the transaction the latest tick of the world
//! its caller began on.
//!
//! The caller here is what a server session does: it pins a read snapshot
//! at BEGIN, runs queries on it, and opens its writable handle only at the
//! first write, by which time other transactions may have committed.

use std::path::Path;

use kala_core::doctor::{DoctorOptions, doctor};
use kala_core::*;

fn schema() -> Schema {
    let attr = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("tag", ValueType::Str, Kind::State, Cardinality::Many))
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
}

struct Cast {
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
}

/// A new world: Greg and Edwin in Varrow from 600, the latest tick 601.
fn seed(path: &Path) -> (World, Cast) {
    let mut w = World::create(path, schema()).unwrap();
    let mut t = w.begin();
    let cast = Cast {
        greg: t.create_entity("CHARACTER", Some("greg")).unwrap(),
        edwin: t.create_entity("CHARACTER", Some("edwin")).unwrap(),
        varrow: t.create_entity("PLACE", Some("varrow")).unwrap(),
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

fn s(text: &str) -> Value {
    Value::Str(text.into())
}

fn clean(w: &World) {
    let report = doctor(w, &DoctorOptions::default());
    assert!(report.is_clean(), "{report:?}");
}

/// The file, a handle `a` that commits "meanwhile", and the tx a reader
/// pinned before those commits.
fn setup() -> (tempfile::TempDir, std::path::PathBuf, World, Cast, TxId) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let (a, c) = seed(&path);
    let pinned = a.latest_tx();
    (dir, path, a, c, pinned)
}

#[test]
fn reads_noted_at_version_n_conflict_with_an_entry_committed_at_n_plus_1() {
    let (_dir, path, mut a, c, pinned) = setup();
    // N+1 writes Greg's location; N+2 is unrelated.
    a.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(605)))
        .unwrap();
    a.assert(c.edwin, "tag", s("unrelated"), Some(Tick(5)))
        .unwrap();
    // The handle opens on N+2, but what it notes was read at N.
    let mut b = World::open(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut t = b.begin();
    assert_eq!(t.snapshot_tx(), TxId(pinned.0 + 2));
    t.note_reverse_read(c.varrow, "located_in").unwrap();
    t.set_read_version(pinned);
    t.assert(c.edwin, "tag", s("decided from the read"), Some(Tick(7)))
        .unwrap();
    let err = t.commit().unwrap_err();
    let Error::Conflict(conflict) = &err else {
        panic!("expected a conflict, got {err:?}");
    };
    assert_eq!(conflict.applied, 2, "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");
    clean(&b);
}

#[test]
fn reads_at_an_older_version_pass_when_the_newer_commits_touched_nothing_read() {
    let (_dir, path, mut a, c, pinned) = setup();
    a.assert(c.edwin, "tag", s("unrelated"), Some(Tick(5)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    t.note_reverse_read(c.varrow, "located_in").unwrap();
    t.note_type_scan("PLACE").unwrap();
    t.set_read_version(pinned);
    t.assert(c.greg, "tag", s("fine"), Some(Tick(7))).unwrap();
    let r = t.commit().unwrap();
    assert!(r.committed);
    assert_eq!(r.changed, 1);
    assert_eq!(b.history(c.greg, "tag").unwrap().len(), 1);
    clean(&b);
}

#[test]
fn the_older_version_also_covers_commits_that_land_after_the_handle_began() {
    let (_dir, path, mut a, c, pinned) = setup();
    a.assert(c.edwin, "tag", s("unrelated"), Some(Tick(5)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    t.note_type_scan("CHARACTER").unwrap();
    t.set_read_version(pinned);
    t.assert(c.greg, "tag", s("x"), Some(Tick(7))).unwrap();
    // A phantom, after the handle's own snapshot.
    a.create_entity("CHARACTER", Some("mara")).unwrap();
    let err = t.commit().unwrap_err();
    let Error::Conflict(conflict) = &err else {
        panic!("expected a conflict, got {err:?}");
    };
    assert_eq!(conflict.applied, 2);
    clean(&b);
}

#[test]
fn a_phantom_and_a_read_all_before_the_handles_snapshot_conflict() {
    for all in [false, true] {
        let (_dir, path, mut a, c, pinned) = setup();
        a.create_entity("CHARACTER", Some("mara")).unwrap();
        let mut b = World::open(&path).unwrap();
        let mut t = b.begin();
        if all {
            t.note_read_all();
        } else {
            t.note_type_scan("CHARACTER").unwrap();
        }
        t.set_read_version(pinned);
        t.assert(c.greg, "tag", s("x"), Some(Tick(7))).unwrap();
        assert!(matches!(t.commit(), Err(Error::Conflict(_))), "all={all}");
    }
}

#[test]
fn weak_mode_ignores_the_read_version() {
    let (_dir, path, mut a, c, pinned) = setup();
    a.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(605)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    t.note_reverse_read(c.varrow, "located_in").unwrap();
    t.note_read_all();
    t.set_read_version(pinned);
    t.set_concurrency(Concurrency::Weak);
    t.assert(c.edwin, "tag", s("weak"), Some(Tick(7))).unwrap();
    assert!(t.commit().unwrap().committed);
    clean(&b);
}

#[test]
fn a_read_version_at_or_past_the_snapshot_changes_nothing() {
    let (_dir, path, mut a, c, _) = setup();
    a.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(605)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    let snapshot = t.snapshot_tx();
    t.note_reverse_read(c.varrow, "located_in").unwrap();
    t.set_read_version(snapshot);
    t.set_read_version(TxId(snapshot.0 + 5));
    t.assert(c.edwin, "tag", s("ok"), Some(Tick(7))).unwrap();
    let r = t.commit().unwrap();
    assert!(r.committed);
    assert!(r.finalized.is_none(), "an ordinary single-writer commit");
}

#[test]
fn the_read_version_only_ever_moves_back() {
    let (_dir, path, mut a, c, pinned) = setup();
    a.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(605)))
        .unwrap();
    a.assert(c.edwin, "tag", s("unrelated"), Some(Tick(5)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    t.note_reverse_read(c.varrow, "located_in").unwrap();
    t.set_read_version(pinned);
    // A later, newer version can't hide the commit the first one covers.
    t.set_read_version(TxId(pinned.0 + 1));
    t.assert(c.edwin, "tag", s("x"), Some(Tick(7))).unwrap();
    assert!(matches!(t.commit(), Err(Error::Conflict(_))));
}

#[test]
fn an_owned_transaction_keeps_its_read_version_between_borrows() {
    let (_dir, path, mut a, c, pinned) = setup();
    a.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(605)))
        .unwrap();
    let mut owned = World::open(&path).unwrap().into_transaction();
    owned.with(|t| {
        t.note_reverse_read(c.varrow, "located_in").unwrap();
        t.set_read_version(pinned);
    });
    owned.with(|t| {
        t.assert(c.edwin, "tag", s("x"), Some(Tick(7))).unwrap();
    });
    let (world, result) = owned.commit();
    assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
    clean(&world);
}

// ------------------------------------------------------------ the start tick

#[test]
fn a_start_tick_is_where_tickless_writes_land_and_a_moved_latest_tick_warns() {
    let (_dir, path, mut a, c, _) = setup();
    // The caller began at latest tick 601; then another commit moved it.
    a.assert(c.edwin, "tag", s("moves it"), Some(Tick(610)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    t.set_start_tick(Tick(601));
    let staged = t.assert(c.greg, "tag", s("late"), None).unwrap();
    assert_eq!(staged.tick, Tick(601));
    let r = t.commit().unwrap();
    assert!(r.committed);
    assert_eq!(
        r.latest_tick_moved,
        Some(TickMoved {
            from: Tick(601),
            to: Tick(610)
        })
    );
    assert_eq!(b.history(c.greg, "tag").unwrap()[0].tick, Tick(601));
    assert_eq!(b.latest_tick(), Tick(610));
    clean(&b);
}

#[test]
fn a_start_tick_warns_also_when_committing_over_newer_transactions() {
    let (_dir, path, mut a, c, _) = setup();
    a.assert(c.edwin, "tag", s("moves it"), Some(Tick(610)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    let mut t = b.begin();
    t.set_start_tick(Tick(601));
    t.assert(c.greg, "tag", s("late"), None).unwrap();
    a.assert(c.edwin, "tag", s("moves it again"), Some(Tick(620)))
        .unwrap();
    let r = t.commit().unwrap();
    assert_eq!(
        r.latest_tick_moved,
        Some(TickMoved {
            from: Tick(601),
            to: Tick(620)
        })
    );
    assert_eq!(b.history(c.greg, "tag").unwrap()[0].tick, Tick(601));
}

#[test]
fn no_warning_when_the_start_tick_is_the_latest_or_no_write_used_it() {
    let (_dir, path, mut a, c, _) = setup();
    a.assert(c.edwin, "tag", s("moves it"), Some(Tick(610)))
        .unwrap();
    let mut b = World::open(&path).unwrap();
    // Explicit ticks only.
    let mut t = b.begin();
    t.set_start_tick(Tick(601));
    t.assert(c.greg, "tag", s("explicit"), Some(Tick(3)))
        .unwrap();
    assert_eq!(t.commit().unwrap().latest_tick_moved, None);
    // The start tick is the latest tick.
    let mut t = b.begin();
    t.set_start_tick(Tick(610));
    t.assert(c.greg, "tag", s("tickless"), None).unwrap();
    assert_eq!(t.commit().unwrap().latest_tick_moved, None);
    // A transaction default takes precedence over the start tick.
    let mut t = b.begin();
    t.set_start_tick(Tick(601));
    t.set_default_tick(Tick(50));
    assert_eq!(
        t.assert(c.greg, "tag", s("default"), None).unwrap().tick,
        Tick(50)
    );
    assert_eq!(t.commit().unwrap().latest_tick_moved, None);
}

#[test]
fn the_start_tick_works_in_memory_too() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "tag", s("a"), Some(Tick(610))).unwrap();
    let mut t = w.begin();
    t.set_start_tick(Tick(601));
    assert_eq!(t.assert(greg, "tag", s("b"), None).unwrap().tick, Tick(601));
    let r = t.commit().unwrap();
    assert_eq!(
        r.latest_tick_moved,
        Some(TickMoved {
            from: Tick(601),
            to: Tick(610)
        })
    );
}
