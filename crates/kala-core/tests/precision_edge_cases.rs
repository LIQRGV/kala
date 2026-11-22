//! Slice 4, item 5 edge cases (precision), written after the implementation landed.
use kala_core::*;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn rep_p(p: Precision) -> Correction {
    Correction::Replace {
        value: None,
        tick: None,
        op: None,
        precision: Some(p),
    }
}
fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}
fn prec(cs: Vec<Change>) -> Vec<(Value, Precision)> {
    cs.into_iter().map(|c| (c.value, c.precision)).collect()
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
}

#[test]
fn a_precision_only_replace_of_an_edge_property_changes_only_that_property() {
    let mut w = World::in_memory(schema()).unwrap();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let c = w.create_entity("PLACE", Some("carnath")).unwrap();
    w.assert(e, "present", Value::Ref(c), t(-300)).unwrap();
    let edge: EntityId = w.edge_id(e, "present", c).unwrap().into();
    let p = w
        .assert_with_precision(
            edge,
            "disguised",
            Value::Bool(true),
            t(-300),
            Precision::Day,
        )
        .unwrap();
    let before = w.latest_tx();
    let r = w.correct(p.entries[0], rep_p(Precision::Year)).unwrap();
    assert_eq!((r.changed, r.entries.len()), (1, 1));
    assert_eq!(
        prec(w.as_of_detail(edge, "disguised", Tick(-300)).unwrap()),
        vec![(Value::Bool(true), Precision::Year)]
    );
    assert_eq!(
        w.at_canon(before)
            .as_of_detail(edge, "disguised", Tick(-300))
            .unwrap()[0]
            .precision,
        Precision::Day
    );
    assert_eq!(
        w.at_canon_main(w.latest_tx())
            .as_of_detail(edge, "disguised", Tick(-300))
            .unwrap()[0]
            .precision,
        Precision::Year
    );
    // the edge itself still reports Exact, and still holds
    assert_eq!(
        w.history(e, "present").unwrap()[0].precision,
        Precision::Exact
    );
    assert_eq!(
        w.neighbors(e, "present", Direction::Out, Tick(-300))
            .unwrap()
            .len(),
        1
    );
    clean(&w);
}

#[test]
fn as_of_detail_on_a_bidirectional_attribute_reads_the_partner_precision_at_every_canon() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let b = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let r = w
        .assert_with_precision(a, "ally_of", Value::Ref(b), t(-208), Precision::Day)
        .unwrap();
    let old = w.latest_tx();
    w.correct(r.entries[0], rep_p(Precision::Century)).unwrap();
    let new = w.latest_tx();
    for (me, other) in [(a, b), (b, a)] {
        assert_eq!(
            prec(
                w.at_canon(old)
                    .as_of_detail(me, "ally_of", Tick(0))
                    .unwrap()
            ),
            vec![(Value::Ref(other), Precision::Day)],
            "latest-path, old canon, {me:?}"
        );
        assert_eq!(
            prec(
                w.at_canon_main(old)
                    .as_of_detail(me, "ally_of", Tick(0))
                    .unwrap()
            ),
            vec![(Value::Ref(other), Precision::Day)],
            "main-path, old canon, {me:?}"
        );
        assert_eq!(
            prec(
                w.at_canon(new)
                    .as_of_detail(me, "ally_of", Tick(0))
                    .unwrap()
            ),
            vec![(Value::Ref(other), Precision::Century)],
            "latest, new canon, {me:?}"
        );
        assert_eq!(
            prec(
                w.at_canon_main(new)
                    .as_of_detail(me, "ally_of", Tick(0))
                    .unwrap()
            ),
            vec![(Value::Ref(other), Precision::Century)],
            "main, new canon, {me:?}"
        );
        // before the alliance: nothing
        assert!(
            w.as_of_detail(me, "ally_of", Tick(-209))
                .unwrap()
                .is_empty()
        );
    }
    clean(&w);
}

#[test]
fn an_unindexed_string_with_a_vague_tick_survives_reopen_and_doctor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let long = "x".repeat(5000);
    let (g, before);
    {
        let mut w = World::create(&path, schema()).unwrap();
        g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let r = w
            .assert_with_precision(
                g,
                "bio",
                Value::Str(long.clone()),
                t(-400),
                Precision::Decade,
            )
            .unwrap();
        clean(&w);
        w.correct(r.entries[0], rep_p(Precision::Century)).unwrap();
        w.retract_with_precision(g, "bio", Value::Str(long.clone()), t(-300), Precision::Year)
            .unwrap();
        clean(&w);
        before = (
            w.history(g, "bio").unwrap(),
            w.as_of_detail(g, "bio", Tick(-350)).unwrap(),
            w.index_dump(),
        );
        assert_eq!(before.1[0].precision, Precision::Century);
    }
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
    let w = World::open(&path).unwrap();
    assert_eq!(*w.recovery(), RecoveryReport::default());
    assert_eq!(
        (
            w.history(g, "bio").unwrap(),
            w.as_of_detail(g, "bio", Tick(-350)).unwrap(),
            w.index_dump()
        ),
        before
    );
    assert_eq!(
        w.as_of_detail(g, "bio", Tick(-350)).unwrap()[0].value,
        Value::Str(long)
    );
    clean(&w);
}

#[test]
fn a_replace_moving_onto_a_same_op_key_writes_only_deleted_and_the_existing_precision_stays() {
    let mut w = World::in_memory(schema()).unwrap();
    let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
    // B first (@5, Year), then A (@1, Day): out of order, so A is stored
    let b = w
        .assert_with_precision(g, "name", s("Greg"), t(5), Precision::Year)
        .unwrap();
    let a = w
        .assert_with_precision(g, "name", s("Greg"), t(1), Precision::Day)
        .unwrap();
    assert_eq!(a.changed, 1);
    let r = w
        .correct(
            b.entries[0],
            Correction::Replace {
                value: None,
                tick: t(1),
                op: None,
                precision: None,
            },
        )
        .unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.entries.len(), 1, "only the Deleted marker");
    assert!(matches!(
        w.entry(r.entries[0]).unwrap().body,
        log::EntryBody::Deleted { tick: Tick(5), .. }
    ));
    let h: Vec<_> = w
        .history(g, "name")
        .unwrap()
        .into_iter()
        .map(|c| (c.tick.0, c.op, c.precision))
        .collect();
    assert_eq!(h, vec![(1, Op::Assert, Precision::Day)]);
    assert_eq!(
        prec(w.as_of_detail(g, "name", Tick(9)).unwrap()),
        vec![(s("Greg"), Precision::Day)]
    );
    clean(&w);
}
