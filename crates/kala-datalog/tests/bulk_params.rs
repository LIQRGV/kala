//! Datalog write parameters preserve the literal form's log representation.
use kala_core::{Schema, Value, World};
use kala_datalog::{apply::execute, parse_write};
use kala_query::Params;

#[test]
fn parameterized_assert_equals_the_literal_log_entries() {
    let world = || {
        let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
        w.create_entity("NPC", Some("rook")).unwrap();
        // A string parameter needs its attribute declared (a string cannot
        // say whether it is a handle or text).
        execute(
            &mut w,
            &parse_write("[:attribute :name :string]").unwrap(),
            None,
        )
        .unwrap();
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
    let source = "[:at ?from [:attribute :name :string] [:create :NPC ?bird] [:assert ?bird :name ?name] [:retract ?bird :name ?name :at ?to]]";
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
    let mut world = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    execute(
        &mut world,
        &parse_write("[:attribute :name :string]").unwrap(),
        None,
    )
    .unwrap();
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

fn template(text: &str) -> kala_datalog::WriteTemplate {
    kala_datalog::prepare_write_forms(text)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
}

fn two_birds() -> World {
    let mut w = World::in_memory(Schema::new().node_type("NPC")).unwrap();
    execute(
        &mut w,
        &parse_write("[[:create :NPC :rook] [:create :NPC :wren]]").unwrap(),
        None,
    )
    .unwrap();
    w
}

#[test]
fn a_string_parameter_for_an_undeclared_attribute_is_refused_before_staging() {
    let pair = Params::from([
        ("a".into(), Value::Str("rook".into())),
        ("b".into(), Value::Str("wren".into())),
    ]);
    let world = two_birds();
    let before = world.latest_tx();
    for text in [
        "[:assert ?a :friend_of ?b :at 700]",
        "[:retract ?a :friend_of ?b :at 700]",
        "[:set ?a :friend_of ?b :at 700]",
        "[:set ?a :friend_of [?b] :at 700]",
        // The first use decides; a literal after it cannot rescue it.
        "[[:assert ?a :friend_of ?b :at 700] [:assert :rook :friend_of :wren :at 701]]",
    ] {
        let error = template(text).bind(&world, &pair).unwrap_err();
        assert_eq!(error.kind, kala_datalog::ErrorKind::TypeMismatch, "{text}");
        let message = error.to_string();
        for part in [
            "?b",
            ":friend_of",
            "[:attribute :friend_of :ref]",
            "[:attribute :friend_of :string]",
        ] {
            assert!(message.contains(part), "{text}: missing {part}: {message}");
        }
        assert!(!message.contains("wren"), "{text}: {message}");
    }
    assert_eq!(world.latest_tx(), before);
    assert!(world.attribute("friend_of").is_none());

    // Numbers and bools say what they are: they infer as a literal would.
    for (attr, value, literal) in [
        ("age", Value::I64(3), "3"),
        ("height", Value::f64(1.5).unwrap(), "1.5"),
        ("tame", Value::Bool(true), "true"),
    ] {
        let (mut bound, mut plain) = (two_birds(), two_birds());
        let params = Params::from([("a".into(), Value::Str("rook".into())), ("v".into(), value)]);
        apply_params(
            &mut bound,
            &format!("[:assert ?a :{attr} ?v :at 1]"),
            &params,
        );
        execute(
            &mut plain,
            &parse_write(&format!("[:assert :rook :{attr} {literal} :at 1]")).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(
            bound.entries().collect::<Vec<_>>(),
            plain.entries().collect::<Vec<_>>(),
            "{attr}"
        );
    }
}

#[test]
fn a_string_parameter_follows_an_attribute_defined_earlier_in_the_form_or_the_transaction() {
    let pair = Params::from([
        ("a".into(), Value::Str("rook".into())),
        ("b".into(), Value::Str("wren".into())),
    ]);
    // Declared, or first written by a literal, earlier in the same form.
    for (bound_form, literal_form) in [
        (
            "[[:attribute :friend_of :ref] [:assert ?a :friend_of ?b :at 700]]",
            "[[:attribute :friend_of :ref] [:assert :rook :friend_of :wren :at 700]]",
        ),
        (
            "[[:assert :wren :friend_of :rook :at 700] [:assert ?a :friend_of ?b :at 700]]",
            "[[:assert :wren :friend_of :rook :at 700] [:assert :rook :friend_of :wren :at 700]]",
        ),
        (
            "[[:assert :wren :motto \"caw\" :at 700] [:assert ?a :motto ?b :at 700]]",
            "[[:assert :wren :motto \"caw\" :at 700] [:assert :rook :motto \"wren\" :at 700]]",
        ),
    ] {
        let (mut bound, mut plain) = (two_birds(), two_birds());
        apply_params(&mut bound, bound_form, &pair);
        execute(&mut plain, &parse_write(literal_form).unwrap(), None).unwrap();
        assert_eq!(
            bound.entries().collect::<Vec<_>>(),
            plain.entries().collect::<Vec<_>>(),
            "{bound_form}"
        );
    }

    // Declared by an earlier form of the same transaction: the form is bound
    // against the transaction's own view, just before it is staged.
    let (mut bound, mut plain) = (two_birds(), two_birds());
    {
        let mut tx = bound.begin();
        kala_datalog::apply::stage(
            &mut tx,
            &parse_write("[[:attribute :friend_of :ref] [:attribute :weight :f64]]").unwrap(),
            None,
        )
        .unwrap();
        let later =
            template("[[:assert ?a :friend_of ?b :at 700] [:assert ?a :weight ?w :at 700]]");
        let mut kinds_seen = Vec::new();
        let params = later
            .bind_parameters(
                tx.base_world(),
                |kinds| {
                    kinds_seen.extend(kinds.iter().cloned());
                    Ok::<_, kala_datalog::DatalogError>(kala_query::QueryParams::from(
                        Params::from([
                            ("a".into(), Value::Str("rook".into())),
                            ("b".into(), Value::Str("wren".into())),
                            ("w".into(), Value::f64(80.0).unwrap()),
                        ]),
                    ))
                },
                |e| e,
            )
            .unwrap();
        assert!(
            kinds_seen.contains(&(
                "w".to_owned(),
                kala_datalog::ParamKind::Value(kala_core::ValueType::F64)
            )),
            "{kinds_seen:?}"
        );
        let form = later.bind(tx.base_world(), &params).unwrap();
        kala_datalog::apply::stage(&mut tx, &form, None).unwrap();
        tx.commit().unwrap();
    }
    execute(
        &mut plain,
        &parse_write(
            "[[:attribute :friend_of :ref] [:attribute :weight :f64] [:assert :rook :friend_of :wren :at 700] [:assert :rook :weight 80.0 :at 700]]",
        )
        .unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(
        bound.entries().collect::<Vec<_>>(),
        plain.entries().collect::<Vec<_>>()
    );
}
