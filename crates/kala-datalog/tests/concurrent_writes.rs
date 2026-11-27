//! Write forms from two handles on one world file (slice 15b, stage 3):
//! the shared applier's reads (`:set`, a conditional create) are what an
//! optimistic commit checks, and a form committed over a newer commit
//! reports what was committed, not what it staged.

use std::path::PathBuf;

use kala_core::{
    Concurrency, ConflictKey, Error, IdMap, Schema, Tick, TickMoved, Value, World,
    doctor::{DoctorOptions, doctor},
};
use kala_datalog::apply::{execute, stage};
use kala_datalog::parse_write;

/// Two handles on a world with Greg in Varrow from 600 and a latest tick of
/// 601.
fn pair() -> (tempfile::TempDir, PathBuf, World, World) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut a = World::create(&path, Schema::new()).unwrap();
    let seed = parse_write(
        "[[:attribute :located_in :ref :one] [:attribute :tag :string :many]
          [:create :CHARACTER :greg] [:create :PLACE :varrow] [:create :PLACE :ashford]
          [:create :PLACE :carnath]
          [:assert :greg :located_in :varrow :at 600]
          [:assert :varrow :tag \"founded\" :at 601]]",
    )
    .unwrap();
    execute(&mut a, &seed, None).unwrap();
    let b = World::open(&path).unwrap();
    (dir, path, a, b)
}

fn clean(w: &World) {
    let report = doctor(w, &DoctorOptions::default());
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn set_against_set_conflicts_writes_nothing_and_reruns_at_the_new_latest_tick() {
    let (_d, path, mut a, mut b) = pair();
    let to_ashford = parse_write("[:set :greg :located_in :ashford]").unwrap();
    let to_carnath = parse_write("[:set :greg :located_in :carnath]").unwrap();
    let mut ta = a.begin();
    let mut tb = b.begin();
    let staged_a = stage(&mut ta, &to_ashford, None).unwrap();
    let staged_b = stage(&mut tb, &to_carnath, None).unwrap();
    assert_eq!(staged_b.ops[0].changed, 2, "retract Varrow, assert Carnath");
    // a also moves the latest tick.
    stage(
        &mut ta,
        &parse_write("[:assert :varrow :tag \"later\" :at 650]").unwrap(),
        None,
    )
    .unwrap();
    let ra = staged_a.complete(&ta.commit().unwrap());
    assert_eq!(ra.ops[0].tick, Tick(601));
    let before = std::fs::read(&path).unwrap();
    let Err(Error::Conflict(conflict)) = tb.commit() else {
        panic!("the second :set must conflict");
    };
    assert!(
        matches!(&conflict.key, ConflictKey::EntityAttribute { attr, .. } if attr == "located_in"),
        "{conflict:?}"
    );
    // The read belongs to the form's only op.
    assert!(staged_b.spans[0].contains(&conflict.operation));
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");

    // Run the form again: it reads Ashford and the new latest tick.
    let report = execute(&mut b, &to_carnath, None).unwrap();
    assert_eq!(report.ops[0].tick, Tick(650));
    assert_eq!(report.changed, 2);
    assert_eq!(report.latest_tick_moved, None);
    let greg = b.entity_by_handle("greg").unwrap();
    let ashford = b.entity_by_handle("ashford").unwrap();
    let carnath = b.entity_by_handle("carnath").unwrap();
    assert_eq!(
        b.as_of(greg, "located_in", Tick(649)).unwrap(),
        [Value::Ref(ashford)]
    );
    assert_eq!(
        b.as_of(greg, "located_in", Tick(650)).unwrap(),
        [Value::Ref(carnath)]
    );
    assert!(b.overlaps("located_in").unwrap().is_empty());
    clean(&b);
}

#[test]
fn set_against_set_in_weak_mode_is_a_lost_update_with_an_overlap_warning() {
    let (_d, _path, mut a, mut b) = pair();
    let mut tb = b.begin();
    tb.set_concurrency(Concurrency::Weak);
    let staged = stage(
        &mut tb,
        &parse_write("[:set :greg :located_in :carnath]").unwrap(),
        None,
    )
    .unwrap();
    execute(
        &mut a,
        &parse_write("[:set :greg :located_in :ashford]").unwrap(),
        None,
    )
    .unwrap();
    let report = staged.complete(&tb.commit().unwrap());
    // Its retract of Varrow at 601 was already written by the other form.
    assert_eq!(report.ops[0].changed, 1);
    assert_eq!(report.ops[0].entries, report.entries);
    assert_eq!(report.warnings.introduced.len(), 1);
    assert_eq!(b.overlaps("located_in").unwrap().len(), 1);
}

#[test]
fn a_conditional_create_of_the_same_free_handle_conflicts_then_exists() {
    let (_d, path, mut a, mut b) = pair();
    let form =
        parse_write("[[:create :CHARACTER :lisa :if-missing] [:assert :lisa :tag \"b\"]]").unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    assert_eq!(staged.ops[0].created, Some(true));
    let first = execute(
        &mut a,
        &parse_write("[:create :CHARACTER :lisa :if-missing]").unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(first.ops[0].created, Some(true));
    let before = std::fs::read(&path).unwrap();
    let Err(Error::Conflict(conflict)) = tb.commit() else {
        panic!("the handle read was invalidated");
    };
    assert_eq!(
        conflict.key,
        ConflictKey::Handle {
            handle: "lisa".into()
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let again = execute(&mut b, &form, None).unwrap();
    assert_eq!(again.ops[0].created, Some(false), "exists");
    assert_eq!(again.ops[0].entity, first.ops[0].entity);
    assert_eq!(again.changed, 1);
    clean(&b);
}

#[test]
fn a_conditional_create_in_weak_mode_settles_to_exists_in_the_report() {
    let (_d, _path, mut a, mut b) = pair();
    let form = parse_write(
        "[[:create :CHARACTER :anon1] [:create :CHARACTER :lisa :if-missing]
          [:assert :lisa :tag \"b\"]]",
    )
    .unwrap();
    let mut tb = b.begin();
    tb.set_concurrency(Concurrency::Weak);
    let staged = stage(&mut tb, &form, None).unwrap();
    let provisional_lisa = staged.ops[1].entity.unwrap();
    execute(
        &mut a,
        &parse_write("[:create :CHARACTER :lisa]").unwrap(),
        None,
    )
    .unwrap();
    let report = staged.complete(&tb.commit().unwrap());
    let lisa = b.entity_by_handle("lisa").unwrap();
    let anon = b.entity_by_handle("anon1").unwrap();
    assert_ne!(lisa, provisional_lisa);
    assert_eq!(report.ops[0].entity, Some(anon));
    assert_eq!(report.ops[1].entity, Some(lisa));
    assert_eq!(report.ops[1].created, Some(false));
    assert_eq!(report.ids.resolve(provisional_lisa), lisa);
    assert_eq!(
        b.as_of(lisa, "tag", Tick(601)).unwrap(),
        [Value::Str("b".into())]
    );
    assert_eq!(b.entities_of_type("CHARACTER").unwrap().count(), 3);
    clean(&b);
}

#[test]
fn a_form_committed_over_a_newer_commit_reports_what_was_committed() {
    let (_d, _path, mut a, mut b) = pair();
    let form = parse_write(
        "[[:create :CHARACTER :kael]
          [:assert :greg :tag \"same\"]
          [:assert :kael :located_in :varrow :precision :year]
          [:assert [:kael :located_in :varrow] :disguised true]]",
    )
    .unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    let provisional = staged.ops[0].entity.unwrap();
    assert_eq!(staged.ops[1].changed, 1);
    // The other handle: a character (takes kael's number), the same tag at
    // the same (latest) tick, and a later fact that moves the latest tick.
    execute(
        &mut a,
        &parse_write(
            "[[:create :CHARACTER :lisa] [:assert :greg :tag \"same\"]
              [:assert :lisa :tag \"x\" :at 610]]",
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let committed = tb.commit().unwrap();
    let report = staged.complete(&committed);
    let kael = b.entity_by_handle("kael").unwrap();
    assert_ne!(kael, provisional);
    assert_eq!(report.ops[0].entity, Some(kael));
    assert_eq!(
        report.ids,
        IdMap {
            entities: vec![(provisional, kael)],
            edges: vec![]
        }
    );
    // The tag is `0 changed` now: no entries, no precision.
    assert_eq!(report.ops[1].changed, 0);
    assert!(report.ops[1].entries.is_empty());
    assert_eq!(report.ops[1].precision, None);
    assert_eq!(report.ops[1].tick, Tick(601));
    assert_eq!(report.ops[2].entries, [report.entries[0]]);
    assert_eq!(report.ops[2].precision, Some(kala_core::Precision::Year));
    assert_eq!(report.ops[3].entries, [report.entries[1]]);
    assert_eq!(report.changed, 2);
    assert_eq!(
        report.latest_tick_moved,
        Some(TickMoved {
            from: Tick(601),
            to: Tick(610)
        })
    );
    let varrow = b.entity_by_handle("varrow").unwrap();
    let edge = b.edge_id(kael, "located_in", varrow).unwrap();
    assert_eq!(report.edges, [edge]);
    assert_eq!(
        b.as_of(edge.into(), "disguised", Tick(601)).unwrap(),
        [Value::Bool(true)]
    );
    clean(&b);
}

#[test]
fn a_store_rule_broken_by_a_newer_commit_is_its_ordinary_error() {
    let (_d, path, mut a, mut b) = pair();
    let mut tb = b.begin();
    stage(
        &mut tb,
        &parse_write("[:create :CHARACTER :lisa]").unwrap(),
        None,
    )
    .unwrap();
    execute(
        &mut a,
        &parse_write("[:create :PLACE :lisa]").unwrap(),
        None,
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        tb.commit().unwrap_err(),
        Error::DuplicateHandle("lisa".into())
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // And through `execute`, as a store failure of the commit.
    let mut tb = b.begin();
    stage(
        &mut tb,
        &parse_write("[:assert :greg :tag \"t\" :at 9]").unwrap(),
        None,
    )
    .unwrap();
    execute(
        &mut a,
        &parse_write("[[:assert :greg :tag \"t\" :at 5] [:retract :greg :tag \"t\" :at 9]]")
            .unwrap(),
        None,
    )
    .unwrap();
    assert!(matches!(
        tb.commit(),
        Err(Error::SameTickConflict { tick: Tick(9), .. })
    ));
}
