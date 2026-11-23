//! Incremental property reads let protocol adapters enforce output budgets
//! before collecting a cardinality-many attribute.
use kala_core::{Error, Tick, TxId, Value, WorkInterruptReason, WorkMeter};
use kala_fixture::load_fixture;
use std::sync::atomic::{AtomicBool, Ordering};

const WORLD: &str = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: alias, type: string, cardinality: many }
    - { name: biography, type: string, cardinality: many, indexed: false }
    - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
entities:
  CHARACTER: [edwin, mara]
transactions:
  - tick: -9223372036854775808
    ops:
      - assert: [edwin, alias, first]
      - assert: [edwin, biography, first]
      - assert: [edwin, ally_of, mara]
  - tick: -1
    ops:
      - assert: [edwin, alias, second]
      - assert: [edwin, biography, second]
  - tick: 0
    ops:
      - retract: [edwin, alias, first]
      - retract: [edwin, biography, first]
  - tick: 9223372036854775807
    ops:
      - assert: [edwin, alias, third]
      - assert: [edwin, biography, third]
"#;

#[test]
fn visitor_matches_as_of_at_extreme_ticks_and_older_canons() {
    let world = load_fixture(WORLD).unwrap();
    let edwin = world.entity_by_handle("edwin").unwrap();
    for canon in 1..=world.latest_tx().0 {
        let view = world.at_canon(TxId(canon));
        for tick in [i64::MIN, i64::MIN + 1, -2, -1, 0, 1, i64::MAX] {
            for attr in ["alias", "biography"] {
                let meter = WorkMeter::bounded(1000, None);
                let mut values = vec![];
                view.metered(&meter)
                    .visit_non_ref_values(edwin, attr, Tick(tick), |value| {
                        values.push(value);
                        Ok(())
                    })
                    .unwrap();
                values.sort();
                assert_eq!(
                    values,
                    view.as_of(edwin, attr, Tick(tick)).unwrap(),
                    "canon={canon} tick={tick} attr={attr}"
                );
            }
        }
    }
}

#[test]
fn visitor_propagates_callback_error_without_further_callbacks() {
    let world = load_fixture(WORLD).unwrap();
    let edwin = world.entity_by_handle("edwin").unwrap();
    let meter = WorkMeter::bounded(1000, None);
    let mut calls = 0;
    let error = world
        .at_canon(world.latest_tx())
        .metered(&meter)
        .visit_non_ref_values(edwin, "alias", Tick(i64::MAX), |_| {
            calls += 1;
            Err(Error::UnknownAttribute("visitor-stopped".into()))
        })
        .unwrap_err();
    assert_eq!(calls, 1);
    assert_eq!(error, Error::UnknownAttribute("visitor-stopped".into()));
}

#[test]
fn visitor_obeys_work_and_cancellation_limits() {
    let world = load_fixture(WORLD).unwrap();
    let edwin = world.entity_by_handle("edwin").unwrap();
    let view = world.at_canon(world.latest_tx());
    let meter = WorkMeter::bounded(0, None);
    let mut calls = 0;
    let result = view
        .metered(&meter)
        .visit_non_ref_values(edwin, "alias", Tick(-1), |_| {
            calls += 1;
            Ok(())
        });
    assert_eq!(calls, 0);
    assert_eq!(
        result,
        Err(Error::WorkInterrupted {
            reason: WorkInterruptReason::Limit
        })
    );
    let stop = AtomicBool::new(false);
    let meter = WorkMeter::bounded(1000, Some(&stop));
    let mut calls = 0;
    let result = view
        .metered(&meter)
        .visit_non_ref_values(edwin, "alias", Tick(-1), |_: Value| {
            calls += 1;
            stop.store(true, Ordering::Relaxed);
            Ok(())
        });
    assert_eq!(calls, 1);
    assert_eq!(
        result,
        Err(Error::WorkInterrupted {
            reason: WorkInterruptReason::Stopped
        })
    );
}

#[test]
fn visitor_rejects_ref_attributes_without_invoking_callback() {
    let world = load_fixture(WORLD).unwrap();
    let edwin = world.entity_by_handle("edwin").unwrap();
    let meter = WorkMeter::bounded(1000, None);
    let result = world
        .at_canon(world.latest_tx())
        .metered(&meter)
        .visit_non_ref_values(edwin, "ally_of", Tick(-1), |_| {
            panic!("reference values must not reach the callback")
        });
    assert!(result.is_err());
}
