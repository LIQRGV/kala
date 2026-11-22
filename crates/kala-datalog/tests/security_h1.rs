//! Security finding H1: an `(at ?t [?x :a ?y])` scan pattern with nothing
//! bound must not keep every per-tick `attr_scan` result alive until the
//! query ends. Memory-bound work runs in a capped child process so a failure
//! cannot endanger the host.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, QueryError, run};
use std::time::{Duration, Instant};

const CHILD: &str = "KALA_SEC_H1_CHILD";
const TICKS: usize = 600;
const BIOS: usize = 20;
const BIO_BYTES: usize = 100_000;

fn big_world() -> World {
    let schema = Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "year",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed());
    let mut w = World::in_memory(schema).unwrap();
    // Distinct years: one scan per distinct ?t. Years are >= 0 and the bios
    // hold from tick -1, so every scan sees all BIOS strings.
    for i in 0..TICKS {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "year", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    for i in 0..BIOS {
        let e = w.create_entity("CHARACTER", None).unwrap();
        let s = format!("{i:04}{}", "x".repeat(BIO_BYTES - 4));
        w.assert(e, "bio", Value::Str(s), Some(Tick(-1))).unwrap();
    }
    w
}

const QUERY: &str = r#"[:find ?n :where [?a :year ?t] (at ?t [?x :bio ?n]) [(= ?n "nope")]]"#;

fn child_body() {
    let w = big_world();
    let p = prepare(&w, QUERY).unwrap_or_else(|e| panic!("prepare failed: {e}"));
    match run(&w, &p.plan, &Params::new(), &Limits::default()) {
        Ok(rs) => assert_eq!(rs.rows.len(), 0, "the correct answer is 0 rows"),
        // A clean limit error is acceptable; an abort is not.
        Err(QueryError::RowLimit { .. }) | Err(QueryError::StepBudget { .. }) => {}
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

/// Inner worker; only does work when re-exec'd by the guard below.
#[test]
fn h1_inner_worker() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    child_body();
}

#[test]
fn per_tick_scans_are_not_all_retained_under_a_memory_cap() {
    if std::env::var_os(CHILD).is_some() {
        return; // the child runs h1_inner_worker only
    }
    let exe = std::env::current_exe().unwrap();
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(
            "ulimit -v 1500000 && exec \"$0\" --exact h1_inner_worker --test-threads=1 --nocapture",
        )
        .arg(&exe)
        .env(CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            eprintln!("skipped: no sh");
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child did not finish in 120 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.success(),
            "child failed ({status}, signal {:?}); abort/kill means per-tick scans were retained:\n{err}",
            status.signal()
        );
    }
}

/// Guards the fix's grouping: rows must equal native `attr_scan` per tick,
/// with duplicate and interleaved ?t values across input rows.
#[test]
fn at_scan_with_many_distinct_and_repeated_ticks_matches_native_attr_scan() {
    let schema = Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "year",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "score",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ));
    let mut w = World::in_memory(schema).unwrap();
    // Interleaved, repeated years (including negative and a tick with no data).
    let years: [i64; 14] = [3, -5, 3, 1, 40, -5, 2, 1, 3, 7, 40, 2, -100, 7];
    let mut ents = Vec::new();
    for y in years {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "year", Value::I64(y), Some(Tick(0))).unwrap();
        ents.push(e);
    }
    // score changes over time: values come and go at different ticks.
    for (i, e) in ents.iter().enumerate() {
        let i = i as i64;
        w.assert(*e, "score", Value::I64(i), Some(Tick(-10 + i)))
            .unwrap();
        w.assert(*e, "score", Value::I64(100 + i), Some(Tick(i * 3)))
            .unwrap();
        w.retract(*e, "score", Value::I64(i), Some(Tick(i * 2 + 1)))
            .unwrap();
    }
    let p = prepare(
        &w,
        "[:find ?a ?t ?x ?y :where [?a :year ?t] (at ?t [?x :score ?y])]",
    )
    .unwrap_or_else(|e| panic!("prepare failed: {e}"));
    let rs = run(&w, &p.plan, &Params::new(), &Limits::default()).unwrap();

    let mut expected: Vec<Vec<Value>> = Vec::new();
    for (e, y) in ents.iter().zip(years) {
        for (x, v) in w.attr_scan("score", Tick(y)).unwrap() {
            expected.push(vec![Value::Ref(*e), Value::I64(y), Value::Ref(x), v]);
        }
    }
    expected.sort();
    expected.dedup();
    assert!(!expected.is_empty());
    assert_eq!(rs.rows, expected);
}
