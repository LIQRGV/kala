//! A small independent author-time schema model, checked after every commit.

use kala_core::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
struct ModelAttr {
    name: String,
    definitions: Vec<(TxId, AttrDef)>,
    facts: Vec<(TxId, Change)>,
}

impl ModelAttr {
    fn definition(&self, canon: TxId) -> Option<AttrDef> {
        let mut def = self
            .definitions
            .iter()
            .rev()
            .find(|(tx, _)| *tx <= canon)?
            .1
            .clone();
        def.name.clone_from(&self.name);
        Some(def)
    }

    fn current(&self) -> AttrDef {
        self.definition(TxId(u64::MAX)).unwrap()
    }

    fn values(&self, canon: TxId, tick: Tick) -> Vec<Value> {
        let mut latest = BTreeMap::<Value, (Tick, Op)>::new();
        for (tx, fact) in &self.facts {
            if *tx <= canon && fact.tick <= tick {
                let entry = latest
                    .entry(fact.value.clone())
                    .or_insert((fact.tick, fact.op));
                if entry.0 < fact.tick {
                    *entry = (fact.tick, fact.op);
                }
            }
        }
        latest
            .into_iter()
            .filter_map(|(value, (_, op))| (op == Op::Assert).then_some(value))
            .collect()
    }
}

fn check(world: &World, reopened: &World, entity: EntityId, attrs: &[ModelAttr]) {
    assert_eq!(world.index_dump(), reopened.index_dump());
    assert_eq!(world.latest_tx(), reopened.latest_tx());
    for n in 0..=world.latest_tx().0 {
        let canon = TxId(n);
        let view = world.at_canon(canon);
        let replay = reopened.at_canon(canon);
        let expected = attrs
            .iter()
            .filter_map(|a| a.definition(canon))
            .collect::<Vec<_>>();
        assert_eq!(
            view.attributes().cloned().collect::<Vec<_>>(),
            expected,
            "canon {n}"
        );
        assert_eq!(
            replay.attributes().cloned().collect::<Vec<_>>(),
            expected,
            "replay canon {n}"
        );
        for attr in attrs {
            if attr.definition(canon).is_none() {
                assert!(
                    view.as_of(entity, &attr.name, Tick(0)).is_err(),
                    "{} must not exist at {n}",
                    attr.name
                );
                continue;
            }
            for tick in [i64::MIN, -2, -1, 0, 1, 2, i64::MAX] {
                let values = if n == 0 {
                    Vec::new()
                } else {
                    attr.values(canon, Tick(tick))
                };
                assert_eq!(
                    view.as_of(entity, &attr.name, Tick(tick)).unwrap(),
                    values,
                    "{} at {tick}, canon {n}",
                    attr.name
                );
                assert_eq!(
                    replay.as_of(entity, &attr.name, Tick(tick)).unwrap(),
                    values,
                    "replay {} at {tick}, canon {n}",
                    attr.name
                );
            }
        }
        for w in [world, reopened] {
            let report = doctor(
                w,
                &DoctorOptions {
                    canon: Some(canon),
                    ..DoctorOptions::default()
                },
            );
            assert!(
                report
                    .findings
                    .iter()
                    .all(|f| matches!(f, Finding::CardinalityOverlap(_))),
                "canon {n}: {report:?}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn random_schema_writes_reopen_and_match_every_canon(
        operations in prop::collection::vec((0_u8..9, any::<u8>(), -2_i64..=2, -2_i64..=2), 1..24)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        let mut world = World::create(&path, Schema::new().node_type("CHARACTER")).unwrap();
        let entity = world.create_entity("CHARACTER", Some("edwin")).unwrap();
        let mut attrs: Vec<ModelAttr> = Vec::new();
        for (step, (op, choose, number, time)) in operations.into_iter().enumerate() {
            let before = world.latest_tx();
            let next = TxId(before.0 + 1);
            let mut changed = true;
            if attrs.is_empty() || op <= 1 {
                let name = format!("a{step}");
                let inferred = op != 1;
                let mut def = AttrDef::new(&name, ValueType::I64, Kind::State, Cardinality::One);
                def.inferred = inferred;
                let mut tx = world.begin();
                if inferred {
                    tx.infer_attribute(&name, &Value::I64(number)).unwrap();
                } else {
                    tx.declare_attribute(def.clone()).unwrap();
                }
                let committed = tx.commit().unwrap();
                assert_eq!(committed.tx, next);
                attrs.push(ModelAttr { name, definitions: vec![(next, def)], facts: Vec::new() });
            } else {
                let index = choose as usize % attrs.len();
                let mut def = attrs[index].current();
                let name = attrs[index].name.clone();
                let mut tx = world.begin();
                match op {
                    2 => {
                        let new_name = format!("renamed{step}");
                        tx.rename_attribute(&name, &new_name).unwrap();
                        attrs[index].name = new_name.clone();
                        def.name = new_name;
                        attrs[index].definitions.push((next, def));
                    }
                    3 => {
                        tx.deprecate_attribute(&name).unwrap();
                        changed = !def.deprecated;
                        if changed {
                            def.deprecated = true;
                            attrs[index].definitions.push((next, def));
                        }
                    }
                    4 if !def.deprecated => {
                        let cardinality = if choose % 2 == 0 { Cardinality::One } else { Cardinality::Many };
                        tx.set_cardinality(&name, cardinality).unwrap();
                        changed = def.cardinality != cardinality;
                        if changed {
                            def.cardinality = cardinality;
                            attrs[index].definitions.push((next, def));
                        }
                    }
                    5 if !def.deprecated => {
                        let to = if def.value_type == ValueType::I64 { ValueType::Str } else { ValueType::I64 };
                        tx.migrate_attribute(&name, MigrationOptions { value_type: Some(to), ..MigrationOptions::default() }).unwrap();
                        let mut old_name = format!("{name}@old");
                        let mut suffix = 2;
                        while attrs.iter().any(|a| a.name == old_name) {
                            old_name = format!("{name}@old{suffix}");
                            suffix += 1;
                        }
                        attrs[index].name = old_name;
                        let mut old_def = def.clone();
                        old_def.deprecated = true;
                        attrs[index].definitions.push((next, old_def));
                        def.value_type = to;
                        def.inferred = false;
                        let facts = attrs[index].facts.iter().map(|(_, fact)| {
                            let mut copy = fact.clone();
                            copy.value = match &fact.value {
                                Value::I64(n) => Value::Str(n.to_string()),
                                Value::Str(s) => Value::I64(s.parse().unwrap()),
                                other => panic!("model generated unexpected value {other:?}"),
                            };
                            (next, copy)
                        }).collect();
                        attrs.push(ModelAttr { name, definitions: vec![(next, def)], facts });
                    }
                    6 | 7 if !def.deprecated => {
                        let value = if def.value_type == ValueType::I64 { Value::I64(number) } else { Value::Str(number.to_string()) };
                        let tick = Tick(match choose % 16 { 0 => i64::MIN, 1 => i64::MAX, _ => time });
                        let operation = if op == 6 { Op::Assert } else { Op::Retract };
                        let same_tick = attrs[index].facts.iter().find(|(_, f)| f.tick == tick && f.value == value).map(|(_, f)| f.op);
                        if same_tick.is_some_and(|old| old != operation) {
                            let result = if op == 6 { tx.assert(entity, &name, value, Some(tick)) } else { tx.retract(entity, &name, value, Some(tick)) };
                            assert!(result.is_err());
                            assert!(tx.commit().is_err());
                            let reopened = World::open_read_only(&path).unwrap();
                            check(&world, &reopened, entity, &attrs);
                            continue;
                        }
                        let holds = attrs[index].values(before, tick).contains(&value);
                        changed = (operation == Op::Assert) != holds;
                        if same_tick.is_some() { changed = false; }
                        let result = if op == 6 { tx.assert(entity, &name, value.clone(), Some(tick)) } else { tx.retract(entity, &name, value.clone(), Some(tick)) }.unwrap();
                        assert_eq!(result.changed, usize::from(changed));
                        if changed {
                            attrs[index].facts.push((next, Change { tick, op: operation, value, precision: Precision::Exact }));
                        }
                    }
                    8 if !def.deprecated => {
                        tx.declare_attribute(def.clone()).unwrap();
                        changed = def.inferred;
                        if changed {
                            def.inferred = false;
                            attrs[index].definitions.push((next, def));
                        }
                    }
                    _ => { changed = false; }
                }
                assert_eq!(tx.commit().unwrap().tx, if changed { next } else { before });
            }
            let reopened = World::open_read_only(&path).unwrap();
            check(&world, &reopened, entity, &attrs);
        }
    }
}
