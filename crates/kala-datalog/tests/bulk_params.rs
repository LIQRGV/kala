//! Datalog write parameters preserve the literal form's log representation.
use kala_core::{Schema, Value, World};
use kala_datalog::{apply::execute, parse_write};
use kala_query::Params;

#[test]
fn parameterized_assert_equals_the_literal_log_entries() {
    let world = || {
        let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
        w.create_entity("NPC", Some("rook")).unwrap();
        w
    };
    let mut literal = world();
    let mut parameterized = world();
    let literal_form = parse_write(r#"[:assert :rook :name "Rook" :at -17]"#).unwrap();
    execute(&mut literal, &literal_form, None).unwrap();
    let params = Params::from([
        ("e".into(), Value::Str("rook".into())),
        ("v".into(), Value::Str("Rook".into())),
        ("t".into(), Value::I64(-17)),
    ]);
    let parsed = kala_datalog::prepare_write_forms("[:assert ?e :name ?v :at ?t]")
        .unwrap()
        .next()
        .unwrap();
    assert!(
        parsed.is_ok(),
        "handle, value, and tick parameters should parse: {:?}",
        parsed.err()
    );
    let form = parsed.unwrap().bind(&parameterized, &params).unwrap();
    execute(&mut parameterized, &form, None).unwrap();
    assert_eq!(
        literal.entries().collect::<Vec<_>>(),
        parameterized.entries().collect::<Vec<_>>()
    );
}

#[test]
fn parameterized_write_structure_is_fixed_by_the_form() {
    for text in [
        "[?verb :NPC :rook]",
        "[:create ?node_type :rook]",
        "[:assert :rook ?attribute 4]",
        "[:assert :rook :name ?v :precision ?precision]",
        "[:attribute ?name :string]",
        "[:attribute :score ?type]",
        "[:world-setting :schema ?mode]",
    ] {
        let result =
            kala_datalog::prepare_write_forms(text).and_then(|mut forms| forms.next().unwrap());
        assert!(
            result.is_err(),
            "accepted a parameter in structural position: {text}"
        );
    }
}

#[test]
fn parameterized_batch_resolves_handles_created_earlier_and_keeps_text_as_data() {
    let mut world = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    let name = "the \"Wren\"]\nsecond line";
    let params = Params::from([
        ("bird".into(), Value::Str("wren".into())),
        ("name".into(), Value::Str(name.into())),
        ("from".into(), Value::I64(i64::MIN)),
        ("to".into(), Value::I64(i64::MAX)),
    ]);
    let source = "[:at ?from [:create :NPC ?bird] [:assert ?bird :name ?name] [:retract ?bird :name ?name :at ?to]]";
    let template = kala_datalog::prepare_write_forms(source)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let form = template.bind(&world, &params).unwrap();
    execute(&mut world, &form, None).unwrap();
    let e = world.entity_by_handle("wren").unwrap();
    assert_eq!(
        world.as_of(e, "name", kala_core::Tick(i64::MIN)).unwrap(),
        [Value::Str(name.into())]
    );
    assert!(
        world
            .as_of(e, "name", kala_core::Tick(i64::MAX))
            .unwrap()
            .is_empty()
    );
    assert!(
        kala_core::doctor(&world, &Default::default())
            .errors()
            .is_empty()
    );
}

fn apply_params(world: &mut World, text: &str, params: &Params) {
    let template = kala_datalog::prepare_write_forms(text)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let form = template.bind(world, params).unwrap();
    execute(world, &form, None).unwrap();
}

#[test]
fn parameterized_refs_set_members_and_corrected_entry_match_literal_forms() {
    let init = || {
        let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
        for text in [
            "[:attribute :knows :ref :state :many :directed]",
            "[:attribute :alias :string :state :many]",
        ] {
            execute(&mut w, &parse_write(text).unwrap(), None).unwrap();
        }
        w
    };
    let mut literal = init();
    let mut bound = init();
    let setup = "[[:create :NPC :rook] [:create :NPC :wren] [:create :NPC :finch] [:set :rook :alias [\"R\" \"Bird\"] :at 0] [:assert :rook :knows :wren :at -1]]";
    execute(&mut literal, &parse_write(setup).unwrap(), None).unwrap();
    let mut params = Params::from([
        ("a".into(), Value::Str("rook".into())),
        ("b".into(), Value::Str("wren".into())),
        ("c".into(), Value::Str("finch".into())),
        ("alias1".into(), Value::Str("R".into())),
        ("alias2".into(), Value::Str("Bird".into())),
        ("t".into(), Value::I64(-1)),
    ]);
    apply_params(
        &mut bound,
        "[[:create :NPC ?a] [:create :NPC ?b] [:create :NPC ?c] [:set ?a :alias [?alias1 ?alias2] :at 0] [:assert ?a :knows ?b :at ?t]]",
        &params,
    );
    let entry = bound
        .entries()
        .find(|e| {
            matches!(
                e.body,
                kala_core::log::EntryBody::Fact {
                    tick: kala_core::Tick(-1),
                    ..
                }
            )
        })
        .unwrap()
        .id;
    params.insert("entry".into(), Value::I64(i64::try_from(entry.0).unwrap()));
    let literal_correction = format!("[:correct {} :replace :value :finch]", entry.0);
    execute(
        &mut literal,
        &parse_write(&literal_correction).unwrap(),
        None,
    )
    .unwrap();
    apply_params(&mut bound, "[:correct ?entry :replace :value ?c]", &params);
    assert_eq!(
        bound.entries().collect::<Vec<_>>(),
        literal.entries().collect::<Vec<_>>()
    );
}

#[test]
fn every_bound_occurrence_charges_the_form_budget_before_staging() {
    let world = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    let template =
        kala_datalog::prepare_write_forms("[[:assert :rook :name ?v] [:assert :wren :name ?v]]")
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
    let params = Params::from([("v".into(), Value::Str("a".repeat(1024)))]);
    let mut remaining = 1536;
    let error = template
        .bind_with_budget(&world, &params, &mut remaining)
        .unwrap_err();
    assert_eq!(error.kind, kala_datalog::ErrorKind::Limit, "{error}");
}
