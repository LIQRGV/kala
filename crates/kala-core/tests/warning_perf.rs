//! Slice 4, item 0 (a): write-warning cost (slice-3 finding M1).
//! docs: slice-4-decisions.md "Item 0 (a)".
//!
//! 1,000 distinct values of one cardinality-one state attribute on one
//! entity, each asserted in its own auto-commit at its own tick (so every
//! interval is `[k, inf)` and all of them overlap). Every single commit must
//! take < 100 ms, and the warnings must stay exactly what slice-3 item 7
//! says. Wall-clock exception to the "deterministic" rule: this test only.
//! Release only: `cargo test --release -p kala-core --test warning_perf`.

use kala_core::*;
use std::time::{Duration, Instant};

const N: i64 = 1000;
const LIMIT: Duration = Duration::from_millis(100);

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "rank",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
}

fn expected_introduced(greg: EntityId, k: i64) -> Vec<Overlap> {
    // value k, asserted at tick k, overlaps each earlier value j on [k, inf);
    // sorted by (entity, side, a, b, during.from): a = j ascending.
    (0..k)
        .map(|j| Overlap {
            entity: greg,
            attr: "rank".into(),
            side: Side::Forward,
            a: Value::I64(j),
            b: Value::I64(k),
            during: Interval {
                from: Tick(k),
                until: None,
            },
        })
        .collect()
}

#[test]
#[cfg_attr(debug_assertions, ignore = "perf test: run with --release")]
fn a_thousand_overlapping_values_each_commit_under_100ms() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut worst = Duration::ZERO;
    let mut slow: Vec<(i64, Duration)> = Vec::new();
    let total = Instant::now();
    for k in 0..N {
        let start = Instant::now();
        let res = w
            .assert(greg, "rank", Value::I64(k), Some(Tick(k)))
            .unwrap();
        let took = start.elapsed();
        worst = worst.max(took);
        eprintln!("commit {k}: {took:?}");
        // results unchanged by any optimisation (checked on a sample; the
        // comparison itself is excluded from the timing)
        if k < 4 || k % 97 == 0 || k == N - 1 {
            assert_eq!(res.changed, 1, "commit {k}");
            assert!(res.warnings.resolved.is_empty(), "commit {k}");
            assert_eq!(
                res.warnings.introduced,
                expected_introduced(greg, k)
                    .into_iter()
                    .take(MAX_COMMIT_WARNINGS)
                    .collect::<Vec<_>>(),
                "commit {k}: first capped overlaps retain their original sorted order"
            );
            assert_eq!(
                res.warnings.introduced_omitted,
                (k as usize).saturating_sub(MAX_COMMIT_WARNINGS) as u128
            );
        }
        if took >= LIMIT {
            slow.push((k, took));
            // fail fast: the quadratic code only gets slower
            panic!("commit {k} took {took:?} (>= {LIMIT:?}); worst so far {worst:?}");
        }
    }
    eprintln!("total {:?}, worst commit {worst:?}", total.elapsed());
    assert!(slow.is_empty());
    // the final state still reports every overlap: C(1000, 2) pairs
    assert_eq!(
        w.overlaps("rank").unwrap().len(),
        (N * (N - 1) / 2) as usize
    );
}

/// Cheap, always-on guard that results are right on a small instance (runs
/// in debug too), so the optimisation cannot change warnings.
#[test]
fn small_instance_warnings_match_the_item_7_order() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    for k in 0..12 {
        let res = w
            .assert(greg, "rank", Value::I64(k), Some(Tick(k)))
            .unwrap();
        assert_eq!(res.warnings.introduced, expected_introduced(greg, k));
        assert!(res.warnings.resolved.is_empty());
    }
    // retracting the first value at tick 20 resolves its 11 overlaps and
    // introduces the shortened ones, sorted by (a, b, from).
    let res = w
        .retract(greg, "rank", Value::I64(0), Some(Tick(20)))
        .unwrap();
    assert_eq!(res.warnings.resolved.len(), 11);
    assert_eq!(res.warnings.introduced.len(), 11);
    for (i, o) in res.warnings.introduced.iter().enumerate() {
        assert_eq!(o.a, Value::I64(0));
        assert_eq!(o.b, Value::I64(i as i64 + 1));
        assert_eq!(o.during.until, Some(Tick(20)));
        assert_eq!(o.during.from, Tick(i as i64 + 1));
    }
}

/// Run in an isolated process so the peak-memory proxy cannot inherit another
/// parallel test's allocations. On Linux, 2M materialized overlaps exceed this
/// generous 128 MiB growth budget; streaming pair counting retains only 100.
#[test]
fn two_thousand_overlaps_use_bounded_warning_memory() {
    const CHILD: &str = "KALA_WARNING_CAP_MEMORY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "two_thousand_overlaps_use_bounded_warning_memory",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    fn peak_kib() -> Option<usize> {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
    }
    let mut w = World::in_memory(schema()).unwrap();
    let e = w.create_entity("CHARACTER", Some("crowded")).unwrap();
    let mut tx = w.begin();
    for value in 0..2000 {
        tx.assert(e, "rank", Value::I64(value), Some(Tick(0)))
            .unwrap();
    }
    let before = peak_kib();
    let start = Instant::now();
    let result = tx.commit().unwrap();
    assert_eq!(result.warnings.introduced.len(), 100);
    assert_eq!(result.warnings.introduced_omitted, 1_999_000 - 100);
    assert!(result.warnings.resolved.is_empty());
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "generous pair-counting time limit exceeded"
    );
    if let (Some(before), Some(after)) = (before, peak_kib()) {
        assert!(
            after.saturating_sub(before) < 128 * 1024,
            "commit warning memory grew by {} KiB",
            after.saturating_sub(before)
        );
    }
}
