//! `[:concurrency :weak]` / `[:concurrency :optimistic]`: a transaction
//! form's own concurrency mode (slice 15b, stage 4). It is the first op of
//! a transaction vector and nothing else; the applier sets the native
//! transaction's mode from it.

use std::path::PathBuf;

use kala_core::{Concurrency, Error, Schema, Tick, Value, World};
use kala_datalog::apply::{Failure, WritePolicy, commit, execute, execute_in, stage};
use kala_datalog::{ErrorKind, FormKind, form_kind, parse_write, parse_writes, restore_forms};

#[test]
fn the_mode_is_the_first_op_of_a_transaction_vector() {
    for (text, mode, ops, at) in [
        (
            "[[:concurrency :weak] [:set :greg :located_in :ashford]]",
            Some(Concurrency::Weak),
            1,
            None,
        ),
        (
            "[[:concurrency :optimistic] [:set :greg :located_in :ashford]]",
            Some(Concurrency::Optimistic),
            1,
            None,
        ),
        // After a leading `:at`, it is still the first op.
        (
            "[:at 601 [:concurrency :weak] [:assert :greg :age 3] [:retract :greg :age 2]]",
            Some(Concurrency::Weak),
            2,
            Some(Tick(601)),
        ),
        // A form that doesn't say leaves the choice to the caller.
        ("[[:set :greg :located_in :ashford]]", None, 1, None),
        ("[:set :greg :located_in :ashford]", None, 1, None),
    ] {
        let form = parse_write(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(form.concurrency, mode, "{text}");
        assert_eq!(form.ops.len(), ops, "{text}: the mode is not an op");
        assert_eq!(form.at, at, "{text}");
        assert_eq!(form_kind(text).unwrap(), FormKind::Write, "{text}");
    }
}

#[test]
fn anywhere_else_twice_or_another_value_is_a_syntax_error_naming_the_rule() {
    const PLACE: &str = "must be the first op of a transaction, once";
    const VALUE: &str = "expected :weak or :optimistic";
    for (text, want) in [
        // Second position.
        (
            "[[:set :greg :located_in :ashford] [:concurrency :weak]]",
            PLACE,
        ),
        // Twice (the same or another mode).
        (
            "[[:concurrency :weak] [:concurrency :weak] [:assert :greg :age 3]]",
            PLACE,
        ),
        (
            "[[:concurrency :weak] [:concurrency :optimistic] [:assert :greg :age 3]]",
            PLACE,
        ),
        // Before `:at` is not a transaction's shape at all; after an op is
        // the same rule.
        ("[:at 5 [:assert :greg :age 3] [:concurrency :weak]]", PLACE),
        // Other values.
        (
            "[[:concurrency :pessimistic] [:assert :greg :age 3]]",
            "concurrency mode :pessimistic is not supported yet; expected :weak or :optimistic",
        ),
        (
            "[[:concurrency :banana] [:assert :greg :age 3]]",
            "unknown concurrency mode :banana; expected :weak or :optimistic",
        ),
        ("[[:concurrency] [:assert :greg :age 3]]", VALUE),
        ("[[:concurrency \"weak\"] [:assert :greg :age 3]]", VALUE),
        ("[[:concurrency :weak :weak] [:assert :greg :age 3]]", VALUE),
        // It is not a transaction by itself, bare or alone in a vector.
        (
            "[:concurrency :weak]",
            "put it first in a vector of ops: [[:concurrency :weak] [:set …]]",
        ),
        (
            "[[:concurrency :weak]]",
            "a transaction needs at least one op",
        ),
    ] {
        let error = parse_write(text).expect_err(text);
        assert_eq!(error.kind, ErrorKind::Syntax, "{text}: {error}");
        assert!(
            error.to_string().contains(want),
            "{text}: `{error}` must contain `{want}`"
        );
    }
}

#[test]
fn each_form_of_a_file_names_its_own_mode() {
    let forms = parse_writes(
        "[[:concurrency :weak] [:assert :greg :age 3]]
         [:assert :greg :age 4]
         [[:concurrency :optimistic] [:assert :greg :age 5]]",
    )
    .unwrap();
    let modes: Vec<_> = forms.iter().map(|f| f.concurrency).collect();
    assert_eq!(
        modes,
        [Some(Concurrency::Weak), None, Some(Concurrency::Optimistic)]
    );
}

#[test]
fn a_genesis_never_takes_a_mode() {
    let error = restore_forms("[:genesis [:concurrency :weak] [:node-type :CHARACTER]]")
        .next()
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("genesis accepts only declared schema forms and world settings"),
        "{error}"
    );
}

/// Two handles on a world with Greg in Varrow from 600 and a latest tick of
/// 601 (the pattern of `concurrent_writes.rs`).
fn pair() -> (tempfile::TempDir, PathBuf, World, World) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut a = World::create(&path, Schema::new()).unwrap();
    let seed = parse_write(
        "[[:attribute :located_in :ref :one]
          [:create :CHARACTER :greg] [:create :PLACE :varrow] [:create :PLACE :ashford]
          [:create :PLACE :carnath]
          [:assert :greg :located_in :varrow :at 600]
          [:assert :varrow :located_in :carnath :at 601]]",
    )
    .unwrap();
    execute(&mut a, &seed, None).unwrap();
    let b = World::open(&path).unwrap();
    (dir, path, a, b)
}

const TO_ASHFORD: &str = "[:set :greg :located_in :ashford]";

/// Stages `text` on `b`, lets `a` move Greg to Ashford meanwhile, and
/// commits `b`'s transaction.
fn commit_over_a_newer_set(text: &str) -> (Result<usize, Error>, World, tempfile::TempDir) {
    let (dir, _path, mut a, mut b) = pair();
    let form = parse_write(text).unwrap();
    let result = {
        let mut tb = b.begin();
        let staged = stage(&mut tb, &form, None).unwrap();
        execute(&mut a, &parse_write(TO_ASHFORD).unwrap(), None).unwrap();
        tb.commit().map(|result| staged.complete(&result).changed)
    };
    (result, b, dir)
}

#[test]
fn a_weak_form_commits_where_the_same_form_without_it_conflicts() {
    let (plain, ..) = commit_over_a_newer_set("[[:set :greg :located_in :carnath]]");
    assert!(
        matches!(plain, Err(Error::Conflict(_))),
        "without the mode the read is checked: {plain:?}"
    );
    let (optimistic, ..) =
        commit_over_a_newer_set("[[:concurrency :optimistic] [:set :greg :located_in :carnath]]");
    assert!(
        matches!(optimistic, Err(Error::Conflict(_))),
        "{optimistic:?}"
    );

    let (weak, b, _dir) =
        commit_over_a_newer_set("[[:concurrency :weak] [:set :greg :located_in :carnath]]");
    // Its retract of Varrow was already written by the other form: only
    // the assert of Carnath is stored (a lost update, as weak mode allows).
    assert_eq!(weak.unwrap(), 1);
    let greg = b.entity_by_handle("greg").unwrap();
    let held = b.as_of(greg, "located_in", Tick(601)).unwrap();
    assert_eq!(held.len(), 2, "Ashford and Carnath overlap: {held:?}");
    assert!(held.contains(&Value::Ref(b.entity_by_handle("carnath").unwrap())));
}

#[test]
fn the_callers_default_applies_unless_the_form_names_a_mode() {
    let carnath = "[:set :greg :located_in :carnath]";
    let weak_form = "[[:concurrency :weak] [:set :greg :located_in :carnath]]";
    let optimistic_form = "[[:concurrency :optimistic] [:set :greg :located_in :carnath]]";
    for (text, default, commits) in [
        (carnath, Concurrency::Optimistic, false),
        (carnath, Concurrency::Weak, true),
        (weak_form, Concurrency::Optimistic, true),
        (optimistic_form, Concurrency::Weak, false),
    ] {
        let (_dir, _path, mut a, mut b) = pair();
        let form = parse_write(text).unwrap();
        // What `execute_in` does, with the other handle's commit between
        // its `begin` and its commit.
        let mut tb = b.begin();
        tb.set_concurrency(default);
        let staged = stage(&mut tb, &form, None).unwrap();
        assert_eq!(
            tb.concurrency(),
            form.concurrency.unwrap_or(default),
            "{text}"
        );
        execute(&mut a, &parse_write(TO_ASHFORD).unwrap(), None).unwrap();
        let result = commit(tb, staged);
        assert_eq!(
            result.is_ok(),
            commits,
            "{text} under {default:?}: {result:?}"
        );
    }
}

#[test]
fn execute_in_accepts_a_mode_and_a_form_with_one() {
    for (text, mode) in [
        (
            "[:set :greg :located_in :carnath :at 700]",
            Concurrency::Weak,
        ),
        (
            "[[:concurrency :weak] [:set :greg :located_in :carnath :at 700]]",
            Concurrency::Optimistic,
        ),
    ] {
        let (_dir, _path, _a, mut b) = pair();
        let form = parse_write(text).unwrap();
        let report = execute_in(&mut b, &form, None, WritePolicy::default(), mode).unwrap();
        assert_eq!(report.changed, 2, "{text}: retract Varrow, assert Carnath");
        assert_eq!(
            report.ops.len(),
            1,
            "{text}: the mode is not reported as an op"
        );
        assert_eq!(report.ops[0].position, 1, "{text}");
    }
}

#[test]
fn a_conflict_is_reported_at_the_op_that_read() {
    let (_dir, path, mut a, mut b) = pair();
    // Op 1 is a blind write; op 2 reads.
    let form = parse_write(
        "[[:assert :varrow :located_in :ashford :at 700]
          [:set :greg :located_in :carnath]]",
    )
    .unwrap();
    let mut tb = b.begin();
    let staged = stage(&mut tb, &form, None).unwrap();
    execute(&mut a, &parse_write(TO_ASHFORD).unwrap(), None).unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = commit(tb, staged).unwrap_err();
    assert_eq!(error.operation, Some((2, "set")));
    assert!(matches!(error.cause, Failure::Store(Error::Conflict(_))));
    assert!(
        error.to_string().starts_with("op 2 (:set): conflict: "),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");
}

/// The "latest tick moved" warning is about writes that landed on the
/// latest tick the transaction began with. A form whose facts all carry a
/// tick (their own, or the form's `:at`), or that only creates and
/// declares, used no such tick: creates and schema ops are merely reported
/// at it.
#[test]
fn only_a_fact_on_the_starting_latest_tick_raises_the_tick_warning() {
    for (text, warns) in [
        ("[:assert :greg :located_in :carnath]", true),
        ("[:set :carnath :located_in :varrow]", true),
        (
            "[[:create :CHARACTER :kael] [:assert :kael :located_in :varrow]]",
            true,
        ),
        // A form that stores nothing has nothing to warn about.
        ("[:retract :greg :located_in :carnath]", false),
        ("[:assert :greg :located_in :carnath :at 601]", false),
        ("[:at 601 [:assert :greg :located_in :carnath]]", false),
        ("[:set :carnath :located_in :varrow :at 605]", false),
        ("[:create :CHARACTER :kael]", false),
        ("[[:attribute :age :i64] [:create :CHARACTER :kael]]", false),
        (
            "[[:create :CHARACTER :kael] [:assert :kael :located_in :varrow :at 3]]",
            false,
        ),
    ] {
        let (_dir, _path, mut a, mut b) = pair();
        let form = parse_write(text).unwrap();
        let mut tb = b.begin();
        let staged = stage(&mut tb, &form, None).unwrap();
        // The other handle moves the latest tick from 601 to 650.
        execute(
            &mut a,
            &parse_write("[:assert :ashford :located_in :carnath :at 650]").unwrap(),
            None,
        )
        .unwrap();
        let report = commit(tb, staged).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(
            report.latest_tick_moved.map(|m| (m.from, m.to)),
            warns.then_some((Tick(601), Tick(650))),
            "{text}"
        );
    }
}
