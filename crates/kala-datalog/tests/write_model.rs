//! Replacement is compared with a naive per-value change-point model at every tick.
use kala_core::{
    AttrDef, Cardinality, DoctorOptions, Kind, Op, Schema, Tick, Value, ValueType, World, doctor,
};
use kala_datalog::{apply::execute, parse_write};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn world(cardinality: Cardinality) -> World {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "v",
        ValueType::I64,
        Kind::State,
        cardinality,
    )))
    .unwrap();
    w.create_entity("CHARACTER", Some("e")).unwrap();
    w
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn random_sets_equal_naive_replace_at_every_tick(
        actions in prop::collection::vec((-8i64..=8, prop::collection::vec(0i64..=5, 0..6)), 1..35),
        many in any::<bool>(),
    ) {
        let mut w = world(if many { Cardinality::Many } else { Cardinality::One });
        let e = w.entity_by_handle("e").unwrap();
        let mut model: Vec<(i64, i64, bool)> = Vec::new();
        for (tick, members) in actions {
            let desired: BTreeSet<_> = members.into_iter().collect();
            let held: BTreeSet<_> = (0..=5).filter(|value| model.iter().filter(|(at, v, _)| *at <= tick && v == value).max_by_key(|(at, _, _)| *at).is_some_and(|(_, _, asserted)| *asserted)).collect();
            let mut writes: Vec<_> = held.difference(&desired).map(|v| (*v, false)).collect();
            writes.extend(desired.difference(&held).map(|v| (*v, true)));
            let conflict = writes.iter().any(|(value, asserted)| model.iter().any(|(at, v, a)| *at == tick && v == value && a != asserted));
            let source = format!("[:set :e :v [{}] :at {tick}]", desired.iter().map(i64::to_string).collect::<Vec<_>>().join(" "));
            let before = w.latest_tx();
            let result = execute(&mut w, &parse_write(&source).unwrap(), None);
            if conflict {
                prop_assert!(result.is_err(), "accepted same-tick contradiction {source}");
                prop_assert_eq!(w.latest_tx(), before);
            } else {
                let result = result.unwrap();
                prop_assert_eq!(result.changed, writes.len());
                model.extend(writes.into_iter().map(|(v, a)| (tick, v, a)));
            }
            for at in -9..=9 {
                let expected: Vec<_> = (0..=5).filter(|value| model.iter().filter(|(tick, v, _)| *tick <= at && v == value).max_by_key(|(tick, _, _)| *tick).is_some_and(|(_, _, a)| *a)).map(Value::I64).collect();
                prop_assert_eq!(w.as_of(e, "v", Tick(at)).unwrap(), expected, "tick {} after {}", at, source);
            }
            let report = doctor(&w, &DoctorOptions::default());
            // Cardinality-one multi-member sets deliberately produce warnings.
            prop_assert!(report.errors().is_empty(), "doctor found store errors: {report:?}");
        }
        let history = w.history(e, "v").unwrap();
        prop_assert_eq!(history.len(), model.len());
        for (tick, value, asserted) in model {
            prop_assert!(history.iter().any(|c| c.tick == Tick(tick) && c.value == Value::I64(value) && c.op == if asserted { Op::Assert } else { Op::Retract }), "missing modeled change point");
        }
    }
}
