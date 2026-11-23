use kala_core::*;
use std::sync::atomic::{AtomicBool, Ordering};

fn world(n: usize) -> World {
    let schema = Schema::new().node_type("N").attribute(AttrDef::new(
        "score",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let mut tx = w.begin();
    for i in 0..n {
        let e = tx.create_entity("N", None).unwrap();
        tx.assert(e, "score", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    tx.commit().unwrap();
    w
}

fn limited<T: std::fmt::Debug>(result: Result<T, Error>, meter: &WorkMeter) {
    assert!(
        matches!(
            result,
            Err(Error::WorkInterrupted {
                reason: WorkInterruptReason::Limit
            })
        ),
        "expected key-bound interruption, got {} keys and {result:?}",
        meter.keys()
    );
    assert!(meter.keys() <= 101, "scan overshot: {} keys", meter.keys());
}

#[test]
fn attr_scan_stops_at_the_key_bound() {
    let w = world(10_000);
    let meter = WorkMeter::bounded(100, None);
    let result = w
        .at_canon(w.latest_tx())
        .metered(&meter)
        .attr_scan("score", Tick(0));
    // Keep a failed run's output small even when the scan wrongly completes.
    limited(result.map(|rows| rows.len()), &meter);
}

#[test]
fn history_scan_stops_at_the_key_bound() {
    let w = world(10_000);
    for view in [w.at_canon(w.latest_tx()), w.at_canon_main(w.latest_tx())] {
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .history_scan("score")
                .map(|rows| rows.len()),
            &meter,
        );
    }
}

#[test]
fn stop_flag_before_and_during_scan() {
    let w = world(100_000);
    for already_set in [true, false] {
        let stop = AtomicBool::new(already_set);
        let meter = WorkMeter::bounded(u64::MAX, Some(&stop));
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                w.at_canon(w.latest_tx())
                    .metered(&meter)
                    .attr_scan("score", Tick(0))
            });
            if !already_set {
                while meter.keys() < 100 && !reader.is_finished() {
                    std::thread::yield_now();
                }
                stop.store(true, Ordering::Relaxed);
            }
            let result = reader.join().unwrap();
            assert!(
                matches!(
                    result,
                    Err(Error::WorkInterrupted {
                        reason: WorkInterruptReason::Stopped
                    })
                ),
                "scan did not stop ({} keys)",
                meter.keys()
            );
            assert!(meter.keys() < 100_000, "scan ran to completion");
        });
    }
}

#[test]
fn entity_value_and_detail_reads_stop_at_the_key_bound() {
    let mut w = world(0);
    let mut tx = w.begin();
    let e = tx.create_entity("N", None).unwrap();
    for value in 0..1_000 {
        tx.assert(e, "score", Value::I64(value), Some(Tick(-1)))
            .unwrap();
    }
    tx.commit().unwrap();
    for view in [w.at_canon(w.latest_tx()), w.at_canon_main(w.latest_tx())] {
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .as_of(e, "score", Tick(0))
                .map(|v| v.len()),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .as_of_detail(e, "score", Tick(0))
                .map(|v| v.len()),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter).history(e, "score").map(|v| v.len()),
            &meter,
        );
    }
}

#[test]
fn historical_value_seeks_and_interval_reads_stop_at_the_key_bound() {
    let mut w = world(0);
    let e = w.create_entity("N", None).unwrap();
    let value = Value::I64(7);
    w.assert(e, "score", value.clone(), Some(Tick(i64::MIN)))
        .unwrap();
    let old = w.latest_tx();
    for tick in -256..256 {
        if tick % 2 == 0 {
            w.retract(e, "score", value.clone(), Some(Tick(tick)))
                .unwrap();
        } else {
            w.assert(e, "score", value.clone(), Some(Tick(tick)))
                .unwrap();
        }
    }
    w.retract(e, "score", value.clone(), Some(Tick(i64::MAX)))
        .unwrap();
    let meter = WorkMeter::bounded(100, None);
    limited(
        w.at_canon(old)
            .metered(&meter)
            .as_of_value(e, "score", &value, Tick(i64::MAX)),
        &meter,
    );
    for view in [
        w.at_canon(w.latest_tx()),
        w.at_canon_main(w.latest_tx()),
        w.at_canon(old),
    ] {
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter).holds_during(e, "score", &value),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter).history(e, "score").map(|v| v.len()),
            &meter,
        );
    }
}

#[test]
fn lookup_and_case_folded_lookup_stop_at_the_key_bound() {
    let schema = Schema::new().node_type("N").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::Many,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let mut tx = w.begin();
    for _ in 0..1_000 {
        let e = tx.create_entity("N", None).unwrap();
        tx.assert(e, "name", Value::Str("Éowyn".into()), Some(Tick(0)))
            .unwrap();
    }
    tx.commit().unwrap();
    for view in [w.at_canon(w.latest_tx()), w.at_canon_main(w.latest_tx())] {
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .lookup("name", &Value::Str("Éowyn".into()), Tick(0))
                .map(|v| v.len()),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .lookup_ci("name", "éOWYN", Tick(0))
                .map(|v| v.len()),
            &meter,
        );
    }
}

#[test]
fn relationship_reads_cover_forward_reverse_inverse_and_bidirectional_paths() {
    let schema = Schema::new()
        .node_type("N")
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        );
    let mut w = World::in_memory(schema).unwrap();
    let mut tx = w.begin();
    let ids: Vec<_> = (0..1_001)
        .map(|_| tx.create_entity("N", None).unwrap())
        .collect();
    let center = ids[500];
    for &e in ids.iter().filter(|&&e| e != center) {
        tx.assert(center, "parent_of", Value::Ref(e), Some(Tick(0)))
            .unwrap();
        tx.assert(e, "parent_of", Value::Ref(center), Some(Tick(0)))
            .unwrap();
        tx.assert(e, "ally_of", Value::Ref(center), Some(Tick(0)))
            .unwrap();
    }
    tx.commit().unwrap();
    for view in [w.at_canon(w.latest_tx()), w.at_canon_main(w.latest_tx())] {
        for (attr, direction) in [
            ("parent_of", Direction::Out),
            ("child_of", Direction::In),
            ("ally_of", Direction::Both),
        ] {
            let meter = WorkMeter::bounded(100, None);
            limited(
                view.metered(&meter)
                    .neighbors(center, attr, direction, Tick(0))
                    .map(|v| v.len()),
                &meter,
            );
        }
        for attr in ["parent_of", "ally_of"] {
            let meter = WorkMeter::bounded(100, None);
            limited(
                view.metered(&meter)
                    .lookup(attr, &Value::Ref(center), Tick(0))
                    .map(|v| v.len()),
                &meter,
            );
            let meter = WorkMeter::bounded(100, None);
            limited(
                view.metered(&meter).history_scan(attr).map(|v| v.len()),
                &meter,
            );
        }
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .history_scan("child_of")
                .map(|v| v.len()),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .attr_scan("child_of", Tick(0))
                .map(|v| v.len()),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .as_of(center, "ally_of", Tick(0))
                .map(|v| v.len()),
            &meter,
        );
        let meter = WorkMeter::bounded(100, None);
        limited(
            view.metered(&meter)
                .history(center, "ally_of")
                .map(|v| v.len()),
            &meter,
        );
    }
}

#[test]
fn lazy_entity_walk_stops_once_and_historical_latest_tick_is_bounded() {
    let w = world(1_000);
    let view = w.at_canon_main(w.latest_tx());
    let meter = WorkMeter::bounded(100, None);
    let metered = view.metered(&meter);
    let mut ids = metered.entities_of_type("N").unwrap();
    for _ in 0..100 {
        assert!(ids.next().unwrap().is_ok());
    }
    assert!(matches!(
        ids.next(),
        Some(Err(Error::WorkInterrupted {
            reason: WorkInterruptReason::Limit
        }))
    ));
    assert!(ids.next().is_none());
    let meter = WorkMeter::bounded(100, None);
    limited(view.metered(&meter).latest_tick(), &meter);
}

#[test]
fn cancellation_covers_lazy_id_reads_and_empty_prefix_walks() {
    let mut w = world(0);
    let mut tx = w.begin();
    for _ in 0..1_000 {
        tx.create_entity("N", None).unwrap();
    }
    tx.commit().unwrap();
    let stop = AtomicBool::new(false);
    let meter = WorkMeter::bounded(u64::MAX, Some(&stop));
    let view = w.at_canon(w.latest_tx()).metered(&meter);
    let mut ids = view.entities_of_type("N").unwrap();
    assert!(ids.next().unwrap().is_ok());
    stop.store(true, Ordering::Relaxed);
    assert!(matches!(
        ids.next(),
        Some(Err(Error::WorkInterrupted {
            reason: WorkInterruptReason::Stopped
        }))
    ));
    assert!(ids.next().is_none());
    let meter = WorkMeter::bounded(100, None);
    limited(
        w.at_canon(w.latest_tx())
            .metered(&meter)
            .attr_scan("score", Tick(0))
            .map(|v| v.len()),
        &meter,
    );
}

#[test]
fn history_scan_bounds_ids_defined_after_the_requested_canon() {
    let w = world(1_000);
    // Every entity was introduced in the final transaction, so an older
    // canon sees no rows. Skipping those invisible IDs still costs work.
    let old = w.at_canon(TxId(w.latest_tx().0 - 1));
    let meter = WorkMeter::bounded(100, None);
    limited(
        old.metered(&meter).history_scan("score").map(|v| v.len()),
        &meter,
    );
}

#[test]
fn sufficient_and_unbounded_meters_preserve_answers_and_key_counts() {
    let mut w = world(12);
    let e = w.entities_of_type("N").unwrap().next().unwrap();
    w.retract(e, "score", Value::I64(0), Some(Tick(1))).unwrap();
    w.assert(e, "score", Value::I64(-2), Some(Tick(i64::MIN)))
        .unwrap();
    w.retract(e, "score", Value::I64(-2), Some(Tick(i64::MAX)))
        .unwrap();
    for view in [w.at_canon(w.latest_tx()), w.at_canon_main(w.latest_tx())] {
        let unbounded = WorkMeter::new();
        let bounded = WorkMeter::bounded(u64::MAX, None);
        let a = view.metered(&unbounded);
        let b = view.metered(&bounded);
        for tick in [i64::MIN, -1, 0, 1, 2, i64::MAX] {
            assert_eq!(
                a.attr_scan("score", Tick(tick)).unwrap(),
                b.attr_scan("score", Tick(tick)).unwrap()
            );
            assert_eq!(
                a.as_of(e, "score", Tick(tick)).unwrap(),
                b.as_of(e, "score", Tick(tick)).unwrap()
            );
            assert_eq!(
                a.as_of_detail(e, "score", Tick(tick)).unwrap(),
                b.as_of_detail(e, "score", Tick(tick)).unwrap()
            );
            assert_eq!(
                a.lookup("score", &Value::I64(0), Tick(tick)).unwrap(),
                b.lookup("score", &Value::I64(0), Tick(tick)).unwrap()
            );
            assert_eq!(unbounded.take(), bounded.take());
        }
        assert_eq!(
            a.history_scan("score").unwrap(),
            b.history_scan("score").unwrap()
        );
        assert_eq!(
            a.history(e, "score").unwrap(),
            b.history(e, "score").unwrap()
        );
        assert_eq!(
            a.holds_during(e, "score", &Value::I64(-2)).unwrap(),
            b.holds_during(e, "score", &Value::I64(-2)).unwrap()
        );
        assert_eq!(a.latest_tick().unwrap(), b.latest_tick().unwrap());
        assert_eq!(
            a.entities_of_type("N")
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            b.entities_of_type("N")
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        );
        assert_eq!(unbounded.take(), bounded.take());
    }
}

#[test]
fn exact_key_limit_succeeds_and_take_resets_an_interrupted_meter() {
    let w = world(12);
    let view = w.at_canon(w.latest_tx());
    let ordinary = WorkMeter::new();
    let expected = view.metered(&ordinary).attr_scan("score", Tick(0)).unwrap();
    let keys = ordinary.keys();
    assert!(keys > 12);
    let exact = WorkMeter::bounded(keys, None);
    assert_eq!(
        view.metered(&exact).attr_scan("score", Tick(0)).unwrap(),
        expected
    );
    assert_eq!(exact.keys(), keys);
    let small = WorkMeter::bounded(keys - 1, None);
    assert!(matches!(
        view.metered(&small).attr_scan("score", Tick(0)),
        Err(Error::WorkInterrupted {
            reason: WorkInterruptReason::Limit
        })
    ));
    assert_eq!(small.take(), keys);
    small.set_max_keys(keys);
    assert_eq!(
        view.metered(&small).attr_scan("score", Tick(0)).unwrap(),
        expected
    );
    assert_eq!(small.keys(), keys);
}
