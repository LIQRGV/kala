//! Sixth history position exposes the precision of each change point.
use kala_core::{
    AttrDef, Cardinality, Correction, Kind, Precision, Schema, Tick, Value, ValueType, World,
    load_fixture,
};
use kala_datalog::prepare;
use kala_query::{Limits, Params, run};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

#[test]
fn sixth_history_position_exposes_year_and_exact() {
    let w = load_fixture(VARROW).unwrap();
    for (query, expected) in [
        (
            "[:find ?prec :where [:battle_of_ashford :name \"Battle of Ashford\" ?t ?op ?prec]]",
            "Year",
        ),
        (
            "[:find ?prec :where [:greg :located_in :varrow 200 :assert ?prec]]",
            "Exact",
        ),
    ] {
        let p = prepare(&w, query).unwrap_or_else(|e| panic!("precision query failed: {e}"));
        let r = run(&w, &p.plan, &Params::new(), &Limits::default()).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert!(
            format!("{:?}", r.rows[0][0]).contains(expected),
            "{:?}",
            r.rows
        );
    }
}

#[test]
fn precision_binds_in_rules() {
    let w = load_fixture(VARROW).unwrap();
    let q = "[:find ?p :rules [[(prec ?p) [:battle_of_ashford :name _ _ _ ?p]]] :where (prec ?p)]";
    let p = prepare(&w, q).unwrap();
    let r = run(&w, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert!(format!("{:?}", r.rows[0][0]).contains("Year"));
}

#[test]
fn every_precision_and_older_canon_survive_a_correction() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::Many,
    )))
    .unwrap();
    let e = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut last = None;
    for (index, precision) in [
        Precision::Exact,
        Precision::Day,
        Precision::Month,
        Precision::Year,
        Precision::Decade,
        Precision::Century,
    ]
    .into_iter()
    .enumerate()
    {
        let result = w
            .assert_with_precision(
                e,
                "age",
                Value::I64(index as i64),
                Some(Tick(index as i64)),
                precision,
            )
            .unwrap();
        last = result.entries.first().copied();
    }
    let old = w.latest_tx();
    let q = "[:find ?p :where [:greg :age ?v ?t ?op ?p]]";
    let p = prepare(&w, q).unwrap();
    let rows = run(&w, &p.plan, &Params::new(), &Limits::default())
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 6);
    for name in ["Exact", "Day", "Month", "Year", "Decade", "Century"] {
        assert!(
            rows.iter().any(|r| format!("{:?}", r[0]).contains(name)),
            "{name}: {rows:?}"
        );
    }
    w.correct(
        last.unwrap(),
        Correction::Replace {
            value: None,
            tick: None,
            op: None,
            precision: Some(Precision::Year),
        },
    )
    .unwrap();
    for (canon, expected) in [(old.0, "Century"), (w.latest_tx().0, "Year")] {
        let p = prepare(
            &w,
            &format!("[:find ?p :canon {canon} :where [:greg :age 5 ?t ?op ?p]]"),
        )
        .unwrap();
        let rows = run(&w, &p.plan, &Params::new(), &Limits::default())
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1);
        assert!(format!("{:?}", rows[0][0]).contains(expected));
    }
}

#[test]
fn precision_variable_can_join_across_histories_and_negation() {
    let w = load_fixture(VARROW).unwrap();
    let q = "[:find ?p :where [:battle_of_ashford :name _ _ _ ?p] [:eldoria :ruled_by :edwin _ _ ?p] (not [:greg :located_in :varrow _ _ ?p])]";
    let p = prepare(&w, q).unwrap();
    let rows = run(&w, &p.plan, &Params::new(), &Limits::default())
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    assert!(format!("{:?}", rows[0][0]).contains("Year"));
}
