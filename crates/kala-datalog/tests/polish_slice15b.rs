//! Polish of slice 15b (PR #20): what the code review found in write forms
//! committed over other handles' commits.

use std::path::PathBuf;

use kala_core::{
    Concurrency, ConflictKey, Error, Schema, Tick, Value, World,
    doctor::{DoctorOptions, doctor},
};
use kala_datalog::apply::{
    Failure, WriteError, WritePolicy, commit, execute, execute_in, stage, stage_with,
};
use kala_datalog::parse_write;

/// Two handles on a world with Greg (age 29 from tick 5) and Varrow.
fn pair() -> (tempfile::TempDir, PathBuf, World, World) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut a = World::create(&path, Schema::new()).unwrap();
    let seed = parse_write(
        "[[:attribute :age :i64 :one] [:attribute :tag :string :many]
          [:create :CHARACTER :greg] [:create :PLACE :varrow]
          [:assert :greg :age 29 :at 5]
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

fn run(w: &mut World, form: &str) {
    execute(w, &parse_write(form).unwrap(), None).unwrap();
}

fn greg(w: &World) -> kala_core::EntityId {
    w.entity_by_handle("greg").unwrap()
}

// ---- G: the mode is set once ----

#[test]
fn a_weak_default_with_an_optimistic_form_is_a_blind_write_not_a_read_of_everything() {
    let (_d, _path, mut a, mut b) = pair();
    let form =
        parse_write("[[:concurrency :optimistic] [:assert :greg :tag \"five\" :at 10]]").unwrap();
    // What `kala transact --concurrency weak` does with the form
    // (`execute_in`), with another process committing before its commit.
    let mut tb = b.begin();
    tb.set_concurrency(Concurrency::Weak);
    let staged = stage_with(&mut tb, &form, None, WritePolicy::default()).unwrap();
    assert_eq!(tb.concurrency(), Concurrency::Optimistic);
    run(&mut a, "[:assert :varrow :tag \"unrelated\" :at 700]");
    let report = commit(tb, staged).expect("a blind write never conflicts");
    assert_eq!(report.changed, 1);
    assert_eq!(
        b.as_of(greg(&b), "tag", Tick(10)).unwrap(),
        [Value::Str("five".into())]
    );
    clean(&b);

    // The whole call, for the mode it ends up in.
    let report = execute_in(
        &mut b,
        &parse_write("[[:concurrency :optimistic] [:assert :greg :tag \"six\" :at 11]]").unwrap(),
        None,
        WritePolicy::default(),
        Concurrency::Weak,
    )
    .unwrap();
    assert_eq!(report.changed, 1);
}

#[test]
fn going_back_to_optimistic_after_weak_reads_still_counts_as_reading_everything() {
    let (_d, _path, mut a, mut b) = pair();
    let mut tb = b.begin();
    tb.set_concurrency(Concurrency::Weak);
    // A read under weak mode isn't recorded...
    let staged = stage(
        &mut tb,
        &parse_write("[:set :greg :age 30 :at 10]").unwrap(),
        None,
    )
    .unwrap();
    // ...so optimistic can't vouch for it afterwards.
    tb.set_concurrency(Concurrency::Optimistic);
    run(&mut a, "[:assert :varrow :tag \"unrelated\" :at 700]");
    let Err(WriteError {
        cause: Failure::Store(Error::Conflict(conflict)),
        ..
    }) = commit(tb, staged)
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflict.key, ConflictKey::All);
}

// ---- H: undeclared names ----

#[test]
fn renaming_an_attribute_away_and_redeclaring_its_name_is_no_undeclared_read() {
    let (_d, _path, mut a, mut b) = pair();
    let form = parse_write(
        "[[:rename :age :age_text] [:attribute :age :i64] [:set :greg :age 30 :at 10]]",
    )
    .unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    run(&mut a, "[:assert :varrow :tag \"unrelated\" :at 700]");
    let report = commit(tb, staged).expect("nobody declared anything");
    assert_eq!(report.changed, 1);
    assert_eq!(
        b.as_of(greg(&b), "age", Tick(10)).unwrap(),
        [Value::I64(30)]
    );
    assert_eq!(
        b.as_of(greg(&b), "age_text", Tick(10)).unwrap(),
        [Value::I64(29)]
    );
    clean(&b);
}

#[test]
fn two_first_writes_that_infer_the_same_attribute_both_commit() {
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        let (_d, _path, mut a, mut b) = pair();
        let mut tb = b.begin();
        tb.set_concurrency(mode);
        let staged = stage(
            &mut tb,
            &parse_write("[:assert :greg :mood \"grim\" :at 10]").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(staged.ops[0].declared, 1);
        run(&mut a, "[:assert :varrow :mood \"quiet\" :at 10]");
        // Blind writes never conflict: the second writer finds the
        // attribute declared exactly as it inferred it, and uses it.
        let report = commit(tb, staged).unwrap_or_else(|e| panic!("{mode:?}: {e:?}"));
        assert_eq!(report.changed, 1, "{mode:?}");
        assert_eq!(report.declared, 0, "{mode:?}: already declared");
        assert_eq!(
            b.as_of(greg(&b), "mood", Tick(10)).unwrap(),
            [Value::Str("grim".into())]
        );
        clean(&b);
    }
}

#[test]
fn two_first_blind_writes_that_infer_different_types_get_the_stores_error() {
    // The second writer read nothing (a blind assert never conflicts), so
    // its commit is its operations made again on the caught-up world: the
    // attribute exists now, as a string, and the store refuses an i64 for
    // it exactly as it would for one writer. In both modes.
    let form = parse_write("[:assert :greg :mood 5 :at 10]").unwrap();
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        let (_d, path, mut a, mut b) = pair();
        let mut tb = b.begin();
        tb.set_concurrency(mode);
        let staged = stage(&mut tb, &form, None).unwrap();
        run(&mut a, "[:assert :varrow :mood \"quiet\" :at 10]");
        let before = std::fs::read(&path).unwrap();
        let err = commit(tb, staged).unwrap_err();
        assert!(
            matches!(&err.cause, Failure::Store(Error::WrongValueType { .. })),
            "{mode:?}: {err:?}"
        );
        assert!(std::fs::read(&path).unwrap() == before, "nothing written");
    }
}

#[test]
fn explicit_declarations_of_one_name_racing_each_other() {
    // Identical: the second is redundant.
    let (_d, _path, mut a, mut b) = pair();
    let form =
        parse_write("[[:attribute :mood :string] [:set :greg :mood \"grim\" :at 10]]").unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    run(&mut a, "[:attribute :mood :string]");
    let report = commit(tb, staged).expect("the same declaration twice");
    assert_eq!((report.changed, report.declared), (1, 0));
    clean(&b);

    // Differing: the store's ordinary schema error, from making the
    // declaration again on the caught-up world.
    let (_d, _path, mut a, mut b) = pair();
    let mut tb = b.begin();
    let staged = stage(
        &mut tb,
        &parse_write("[[:attribute :mood :string] [:assert :greg :mood \"grim\" :at 10]]").unwrap(),
        None,
    )
    .unwrap();
    run(
        &mut a,
        "[[:attribute :mood :i64] [:assert :varrow :mood 3 :at 1]]",
    );
    let err = commit(tb, staged).unwrap_err();
    assert!(
        matches!(&err.cause, Failure::Store(Error::Schema(_))),
        "{err:?}"
    );
}

#[test]
fn first_sets_of_an_inferred_attribute_conflict_only_where_they_read() {
    // Different entities: both commit, the attribute declared once.
    let (_d, _path, mut a, mut b) = pair();
    let mut tb = b.begin();
    let staged = stage(
        &mut tb,
        &parse_write("[:set :greg :mood \"grim\" :at 10]").unwrap(),
        None,
    )
    .unwrap();
    run(&mut a, "[:set :varrow :mood \"quiet\" :at 10]");
    let report = commit(tb, staged).expect("nothing it read was written");
    assert_eq!((report.changed, report.declared), (1, 0));
    clean(&b);

    // The same entity: the `:set` read "greg has no mood", and now he has.
    let (_d, _path, mut a, mut b) = pair();
    let form = parse_write("[:set :greg :mood \"grim\" :at 12]").unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    run(&mut a, "[:set :greg :mood \"quiet\" :at 10]");
    let Err(WriteError {
        cause: Failure::Store(Error::Conflict(conflict)),
        ..
    }) = commit(tb, staged)
    else {
        panic!("expected a conflict");
    };
    assert!(
        matches!(&conflict.key, ConflictKey::EntityAttribute { attr, .. } if attr == "mood"),
        "{conflict:?}"
    );
    // Run again: it replaces what the other writer set.
    let report = execute(&mut b, &form, None).unwrap();
    assert_eq!(report.changed, 2);
    assert!(b.overlaps("mood").unwrap().is_empty());
    clean(&b);

    // A differing inference under a read: the name it found free is taken.
    let (_d, _path, mut a, mut b) = pair();
    let mut tb = b.begin();
    let staged = stage(
        &mut tb,
        &parse_write("[:set :greg :mood 5 :at 10]").unwrap(),
        None,
    )
    .unwrap();
    run(&mut a, "[:set :varrow :mood \"quiet\" :at 10]");
    let Err(WriteError {
        cause: Failure::Store(Error::Conflict(conflict)),
        ..
    }) = commit(tb, staged)
    else {
        panic!("expected a conflict");
    };
    assert_eq!(
        conflict.key,
        ConflictKey::Undeclared {
            name: "mood".into()
        }
    );
}

// ---- I: a retract dropped for an undeclared attribute is made again ----

#[test]
fn a_weak_retract_dropped_as_undeclared_is_judged_again_at_commit() {
    let (_d, _path, mut a, mut b) = pair();
    let form = parse_write(
        "[[:concurrency :weak]
          [:retract :greg :mood \"grim\" :at 20]
          [:assert :varrow :tag \"x\" :at 700]]",
    )
    .unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    assert_eq!(staged.ops[0].changed, 0, "no such attribute yet: dropped");
    // Another writer declares the attribute and asserts that very value.
    run(&mut a, "[:assert :greg :mood \"grim\" :at 10]");
    let report = commit(tb, staged).unwrap();
    // What one writer making the form now stores: the retract counts.
    assert_eq!(report.ops[0].changed, 1, "{report:?}");
    assert_eq!(report.ops[1].changed, 1);
    assert_eq!(report.changed, 2);
    assert_eq!(b.as_of(greg(&b), "mood", Tick(19)).unwrap().len(), 1);
    assert!(b.as_of(greg(&b), "mood", Tick(20)).unwrap().is_empty());
    clean(&b);

    // Still "0 changed" when the value isn't held at its tick.
    let (_d, _path, mut a, mut b) = pair();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    run(&mut a, "[:assert :greg :mood \"calm\" :at 10]");
    let report = commit(tb, staged).unwrap();
    assert_eq!((report.ops[0].changed, report.changed), (0, 1));
    clean(&b);

    // A form that stages nothing is a read: it commits where it began,
    // before the other writer, where the retract changed nothing.
    let (_d, path, mut a, mut b) = pair();
    let mut tb = b.begin();
    let staged = stage(
        &mut tb,
        &parse_write("[[:concurrency :weak] [:retract :greg :mood \"grim\" :at 20]]").unwrap(),
        None,
    )
    .unwrap();
    run(&mut a, "[:assert :greg :mood \"grim\" :at 10]");
    let before = std::fs::read(&path).unwrap();
    let report = commit(tb, staged).unwrap();
    assert_eq!(report.changed, 0);
    assert!(std::fs::read(&path).unwrap() == before);
}
