//! PR #22 review: what a node deletion enumerates (`Txn::delete_facts`).
use kala_core::*;

fn schema() -> Schema {
    let attr = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("old_home", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("born_in", ValueType::Ref, Kind::Event, Cardinality::Many).directed())
        .attribute(attr("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(attr("nick", ValueType::Str, Kind::State, Cardinality::Many))
}

fn world() -> (World, EntityId, EntityId) {
    let mut world = World::in_memory(schema()).unwrap();
    let greg = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = world.create_entity("PLACE", Some("varrow")).unwrap();
    world
        .assert(greg, "name", Value::Str("Greg".into()), Some(Tick(1)))
        .unwrap();
    (world, greg, varrow)
}

fn names(facts: &[(EntityId, String, Value)]) -> Vec<&str> {
    facts.iter().map(|(_, attr, _)| attr.as_str()).collect()
}

#[test]
fn deprecated_attributes_are_left_as_they_are_and_never_block_a_delete() {
    let (mut world, greg, varrow) = world();
    world
        .assert(greg, "nick", Value::Str("G".into()), Some(Tick(1)))
        .unwrap();
    world
        .assert(greg, "old_home", Value::Ref(varrow), Some(Tick(1)))
        .unwrap();
    let mut tx = world.begin();
    tx.deprecate_attribute("nick").unwrap();
    tx.deprecate_attribute("old_home").unwrap();
    tx.commit().unwrap();
    let tx = world.begin();
    let meter = WorkMeter::bounded(u64::MAX, None);
    for detach in [false, true] {
        // The deprecated ref is neither a relationship that blocks a plain
        // delete nor a state to retract (its retract would be refused).
        for node in [greg, varrow] {
            let facts = tx.delete_facts(node, Tick(5), detach, 100, &meter).unwrap();
            assert_eq!(
                names(&facts),
                if node == greg { vec!["name"] } else { vec![] },
                "detach={detach}"
            );
        }
    }
}

#[test]
fn events_never_block_a_delete_and_are_never_retracted() {
    let (mut world, greg, varrow) = world();
    world
        .assert(greg, "born_in", Value::Ref(varrow), Some(Tick(0)))
        .unwrap();
    let tx = world.begin();
    let meter = WorkMeter::bounded(u64::MAX, None);
    for detach in [false, true] {
        assert_eq!(
            names(&tx.delete_facts(greg, Tick(5), detach, 100, &meter).unwrap()),
            ["name"]
        );
        assert!(
            tx.delete_facts(varrow, Tick(5), detach, 100, &meter)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn a_delete_reads_only_the_staged_entries_about_its_node() {
    let (mut world, greg, varrow) = world();
    let other = world.create_entity("PLACE", Some("ashford")).unwrap();
    let mut tx = world.begin();
    // 5,000 staged entries that have nothing to do with Greg, and two that
    // point at Varrow.
    for i in 0..5_000 {
        tx.assert(other, "nick", Value::Str(format!("n{i}")), Some(Tick(2)))
            .unwrap();
    }
    tx.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(2)))
        .unwrap();
    let before = tx.staged_entry_count();
    assert_eq!(before, 5_001);
    for (node, expected) in [(greg, 2), (varrow, 1)] {
        let meter = WorkMeter::bounded(64, None);
        let facts = tx.delete_facts(node, Tick(5), true, 100, &meter).unwrap();
        assert_eq!(facts.len(), expected);
        assert!(meter.keys() <= 64, "{}", meter.keys());
    }
    // A staged retract of the incoming ref is seen too.
    tx.retract(greg, "located_in", Value::Ref(varrow), Some(Tick(4)))
        .unwrap();
    let meter = WorkMeter::bounded(64, None);
    assert!(
        tx.delete_facts(varrow, Tick(5), true, 100, &meter)
            .unwrap()
            .is_empty()
    );
}
