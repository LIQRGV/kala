//! World-backed procedures work without the server and use shared row stages.
use kala_core::{Tick, Value, World};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, run};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
fn strings(text: &str) -> Vec<String> {
    let world = load_fixture(VARROW).unwrap();
    strings_on(&world, text)
}
fn strings_on(world: &World, text: &str) -> Vec<String> {
    let p = kala_cypher::prepare(world, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(world, &p.plan, &Params::new(), &Limits::default())
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match &row[0] {
            Datum::Value(Value::Str(value)) => value.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn world_procedures_report_schema_and_visible_labels_in_order() {
    let labels = strings("CALL db.labels()");
    for name in ["CHARACTER", "PLACE", "City"] {
        assert!(labels.iter().any(|s| s == name), "{labels:?}");
    }
    assert!(labels.windows(2).all(|w| w[0] < w[1]));
    let rels = strings("CALL db.relationshipTypes()");
    for name in ["ally_of", "located_in"] {
        assert!(rels.iter().any(|s| s == name));
    }
    assert!(rels.windows(2).all(|w| w[0] < w[1]));
    let props = strings("CALL db.propertyKeys()");
    assert!(props.iter().any(|s| s == "name"));
    assert!(props.iter().any(|s| s == "kala_handle"));
    assert!(!props.iter().any(|s| s == "ally_of"));
    assert!(props.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn yield_alias_where_with_and_return_use_ordinary_pipeline() {
    assert_eq!(
        strings("CALL db.labels() YIELD label WHERE label = 'City' RETURN label"),
        vec!["City"]
    );
    assert_eq!(
        strings("CALL db.labels() YIELD label AS l WHERE l = 'City' WITH l AS x RETURN x"),
        vec!["City"]
    );
    let world = load_fixture(VARROW).unwrap();
    for query in [
        "CALL db.labels() YIELD unknown RETURN unknown",
        "CALL db.labels() YIELD label AS l RETURN label",
    ] {
        assert!(kala_cypher::prepare(&world, query).is_err(), "{query}");
    }
}

#[test]
fn procedure_catalogues_observe_query_tick_canon_inverse_and_deprecation() {
    let mut world = load_fixture("schema:\n  node_types: [NODE]\n  attributes:\n    - { name: parent, type: ref, inverse: child }\n    - { name: label, type: string, cardinality: many }\nentities:\n  NODE: [a, b]\ntransactions:\n  - tick: -5\n    ops:\n      - assert: [a, label, Old]\n      - assert: [a, parent, b]\n  - tick: 0\n    ops:\n      - retract: [a, label, Old]\n      - assert: [a, label, New]\n").unwrap();
    assert_eq!(
        strings_on(&world, "CALL db.labels() AT TICK -5"),
        vec!["NODE", "Old"]
    );
    assert_eq!(
        strings_on(&world, "CALL db.labels() AT TICK 0"),
        vec!["NODE", "New"]
    );
    assert_eq!(
        strings_on(&world, &format!("CALL db.labels() AT TICK {}", i64::MIN)),
        vec!["NODE"]
    );
    let old = world.latest_tx().0;
    let a = world.entity_by_handle("a").unwrap();
    let b = world.entity_by_handle("b").unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("later", &Value::I64(7)).unwrap();
    tx.assert(a, "later", Value::I64(7), Some(Tick(0))).unwrap();
    tx.infer_attribute("later_ref", &Value::Ref(b)).unwrap();
    tx.assert(a, "later_ref", Value::Ref(b), Some(Tick(0)))
        .unwrap();
    tx.deprecate_attribute("parent").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        strings_on(&world, "CALL db.relationshipTypes()"),
        vec!["child", "later_ref", "parent"]
    );
    assert_eq!(
        strings_on(
            &world,
            &format!("CALL db.relationshipTypes() AT TICK 0 CANON {old}")
        ),
        vec!["child", "parent"]
    );
    assert_eq!(
        strings_on(&world, "CALL db.propertyKeys()"),
        vec!["kala_handle", "label", "later"]
    );
    assert_eq!(
        strings_on(
            &world,
            &format!("CALL db.propertyKeys() AT TICK 0 CANON {old}")
        ),
        vec!["kala_handle", "label"]
    );
}
