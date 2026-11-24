//! User attributes named `handle` have temporal history independently of the
//! immutable entity handle exposed by `kala_handle`.
use kala_core::{
    AttrDef, Cardinality, Interval, IntervalSet, Kind, Op, Schema, Tick, Value, ValueType, World,
};
use kala_datalog::{ErrorKind, parse, prepare};
use kala_query::{Datum, run};

fn rows(world: &World, query: &str) -> Vec<Vec<Datum>> {
    let prepared = prepare(world, query).unwrap_or_else(|e| panic!("{query}: {e}"));
    run(
        world,
        &prepared.plan,
        &kala_query::Params::new(),
        &Default::default(),
    )
    .unwrap()
    .rows
}

#[test]
fn handle_is_ordinary_in_history_and_during_syntax() {
    for query in [
        "[:find ?h ?t :where [:edwin :handle ?h ?t]]",
        "[:find ?h ?s :where (during [:edwin :handle ?h] ?s)]",
    ] {
        assert!(
            parse(query).is_ok(),
            "user handle attribute must parse: {query}"
        );
    }
}

#[test]
fn undefined_handle_gets_the_rename_hint_only_for_that_name() {
    let world = World::in_memory(Schema::new()).unwrap();
    let error = prepare(&world, r#"[:find ?e :where [?e :handle "edwin"]]"#)
        .expect_err("handle is no longer a pseudo-attribute");
    assert_eq!(error.kind, ErrorKind::UnknownAttribute);
    assert!(
        error
            .message
            .contains("the built-in handle attribute is now :kala_handle"),
        "{error}"
    );
    let other = prepare(&world, r#"[:find ?e :where [?e :unknown "edwin"]]"#)
        .expect_err("unknown attribute");
    assert_eq!(other.kind, ErrorKind::UnknownAttribute);
    assert!(!other.message.contains("kala_handle"), "{other}");
}

#[test]
fn user_handle_history_during_and_pseudo_attribute_are_independent() {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "handle",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    let mut world = World::in_memory(schema).expect("handle is an ordinary schema name");
    let edwin = world.create_entity("CHARACTER", Some("edwin")).unwrap();
    for (tick, value, op) in [
        (i64::MIN, "old", Op::Assert),
        (-1, "old", Op::Retract),
        (0, "new", Op::Assert),
        (i64::MAX, "new", Op::Retract),
    ] {
        let value = Value::Str(value.into());
        match op {
            Op::Assert => world.assert(edwin, "handle", value, Some(Tick(tick))),
            Op::Retract => world.retract(edwin, "handle", value, Some(Tick(tick))),
        }
        .unwrap();
    }
    for (tick, expected) in [
        (i64::MIN, Some("old")),
        (-2, Some("old")),
        (-1, None),
        (0, Some("new")),
        (i64::MAX - 1, Some("new")),
        (i64::MAX, None),
    ] {
        let query = format!("[:find ?h :at {tick} :where [:edwin :handle ?h]]");
        let expected: Vec<Vec<Datum>> = expected
            .into_iter()
            .map(|value| vec![Value::Str(value.into()).into()])
            .collect();
        assert_eq!(rows(&world, &query), expected, "at {tick}");
        let query = format!("[:find ?e :at {tick} :where [?e :kala_handle \"edwin\"]]");
        assert_eq!(
            rows(&world, &query),
            vec![vec![Datum::Value(Value::Ref(edwin))]]
        );
    }
    let mut expected_history: Vec<Vec<Datum>> = [
        (i64::MIN, "old", Op::Assert),
        (-1, "old", Op::Retract),
        (0, "new", Op::Assert),
        (i64::MAX, "new", Op::Retract),
    ]
    .into_iter()
    .map(|(tick, value, op)| {
        vec![
            Value::Str(value.into()).into(),
            Value::I64(tick).into(),
            Datum::Op(op),
        ]
    })
    .collect();
    expected_history.sort();
    assert_eq!(
        rows(
            &world,
            "[:find ?h ?t ?op :where [:edwin :handle ?h ?t ?op]]"
        ),
        expected_history
    );
    let intervals = |from, until| {
        Datum::Intervals(IntervalSet::from_intervals([Interval {
            from: Tick(from),
            until: Some(Tick(until)),
        }]))
    };
    assert_eq!(
        rows(
            &world,
            "[:find ?h ?s :where (during [:edwin :handle ?h] ?s)]"
        ),
        vec![
            vec![Value::Str("new".into()).into(), intervals(0, i64::MAX)],
            vec![Value::Str("old".into()).into(), intervals(i64::MIN, -1)],
        ]
    );
    let prepared = prepare(
        &world,
        "[:find ?h :at 0 :where [?e :kala_handle \"edwin\"] [?e :handle ?h]]",
    )
    .unwrap();
    assert_eq!(prepared.plan.attribute_names(), vec!["handle"]);
}
