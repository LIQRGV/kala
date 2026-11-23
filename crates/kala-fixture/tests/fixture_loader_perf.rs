//! Performance guards for the fixture loader (security review H1, H2).
//!
//! Release only: `cargo test --release -p kala-fixture --test fixture_loader_perf
//! -- --include-ignored`. Each case also pins the stored data: the loaded log
//! is byte-identical to the same writes through the native API, so a speed-up
//! cannot change what is stored.
//!
//! H1: one transaction asserting k distinct values of a cardinality-one
//! attribute at one tick must not cost O(k^2) memory/time just to compute
//! write warnings the loader discards.
//! H2: in the bare `operations:` form, each commit must not rescan the whole
//! history of the attribute for warnings.

use kala_core::*;
use kala_fixture::{load_fixture, load_fixture_to};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

const HEADER: &str = "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: title, type: string }\nentities:\n  CHARACTER: [edwin]\n";

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "title",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ))
}

/// One transaction, `k` distinct values asserted at tick 5.
fn same_tick_yaml(k: usize) -> String {
    let mut y = String::from(HEADER);
    y.push_str("transactions:\n  - tick: 5\n    ops:\n");
    for i in 0..k {
        writeln!(y, "      - assert: [edwin, title, v{i}]").unwrap();
    }
    y
}

/// Bare form: each value asserted at 2i then retracted at 2i+1.
fn disjoint_yaml(k: usize) -> String {
    let mut y = String::from(HEADER);
    y.push_str("operations:\n");
    for i in 0..k {
        writeln!(y, "  - tick: {}\n    assert: [edwin, title, v{i}]", 2 * i).unwrap();
        writeln!(
            y,
            "  - tick: {}\n    retract: [edwin, title, v{i}]",
            2 * i + 1
        )
        .unwrap();
    }
    y
}

fn native_same_tick(k: usize, path: &Path) -> World {
    let mut w = World::create(path, schema()).unwrap();
    let mut tx = w.begin();
    tx.create_entity("CHARACTER", Some("edwin")).unwrap();
    tx.commit().unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    let mut tx = w.begin();
    for i in 0..k {
        tx.assert(edwin, "title", Value::Str(format!("v{i}")), Some(Tick(5)))
            .unwrap();
    }
    tx.commit().unwrap();
    w
}

fn native_disjoint(k: usize, path: &Path) -> World {
    let mut w = World::create(path, schema()).unwrap();
    let mut tx = w.begin();
    tx.create_entity("CHARACTER", Some("edwin")).unwrap();
    tx.commit().unwrap();
    let edwin = w.entity_by_handle("edwin").unwrap();
    for i in 0..k as i64 {
        let v = Value::Str(format!("v{i}"));
        w.assert(edwin, "title", v.clone(), Some(Tick(2 * i)))
            .unwrap();
        w.retract(edwin, "title", v, Some(Tick(2 * i + 1))).unwrap();
    }
    w
}

fn overlap_findings(w: &World) -> usize {
    doctor(w, &DoctorOptions::default())
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::CardinalityOverlap(_)))
        .count()
}

#[test]
#[cfg_attr(debug_assertions, ignore = "release only")]
fn h1_3000_values_one_tick_one_transaction_loads_fast() {
    let yaml = same_tick_yaml(3_000);
    let start = Instant::now();
    let w = load_fixture(&yaml).unwrap();
    let took = start.elapsed();
    eprintln!("h1 load_fixture(k=3000): {took:?}");
    assert!(took < Duration::from_millis(500), "took {took:?}");
    drop(w);
}

#[test]
#[cfg_attr(debug_assertions, ignore = "release only")]
fn h1_k50_overlaps_and_doctor_are_correct() {
    let w = load_fixture(&same_tick_yaml(50)).unwrap();
    assert!(!w.overlaps("title").unwrap().is_empty());
    assert_eq!(overlap_findings(&w), 50 * 49 / 2);
}

#[test]
#[cfg_attr(debug_assertions, ignore = "release only")]
fn h1_log_is_byte_identical_to_native_api() {
    for k in [3usize, 50, 400] {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("fixture.kala");
        let b = dir.path().join("native.kala");
        drop(load_fixture_to(&same_tick_yaml(k), &a).unwrap());
        drop(native_same_tick(k, &b));
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "k={k}"
        );
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "release only")]
fn h2_bare_form_5000_disjoint_values_loads_fast() {
    let yaml = disjoint_yaml(5_000);
    let start = Instant::now();
    let w = load_fixture(&yaml).unwrap();
    let took = start.elapsed();
    eprintln!("h2 load_fixture(5000 assert+retract): {took:?}");
    assert!(took < Duration::from_secs(2), "took {took:?}");
    drop(w);
}

#[test]
#[cfg_attr(debug_assertions, ignore = "release only")]
fn h2_disjoint_values_have_no_overlaps() {
    let w = load_fixture(&disjoint_yaml(100)).unwrap();
    assert!(w.overlaps("title").unwrap().is_empty());
    assert_eq!(overlap_findings(&w), 0);
}

#[test]
#[cfg_attr(debug_assertions, ignore = "release only")]
fn h2_log_is_byte_identical_to_native_api() {
    for k in [3usize, 100] {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("fixture.kala");
        let b = dir.path().join("native.kala");
        drop(load_fixture_to(&disjoint_yaml(k), &a).unwrap());
        drop(native_disjoint(k, &b));
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "k={k}"
        );
    }
}
