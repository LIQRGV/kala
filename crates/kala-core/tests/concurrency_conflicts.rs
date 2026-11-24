//! Committing over newer commits (slice 15b, stage 3): a transaction begun
//! before another handle's commit is finalized against the caught-up world
//! (final IDs, a reused edge, the store rules judged again, the latest tick
//! it began with), and an optimistic one conflicts when something it read
//! was written meanwhile. Weak mode keeps only the store rules.
//!
//! Most tests compare the shared file, byte for byte, with a file written
//! by one writer making the same operations one transaction after the
//! other: "the result equals committing them serially".

use std::path::{Path, PathBuf};

use kala_core::doctor::{DoctorOptions, doctor, doctor_log};
use kala_core::log::EntryBody;
use kala_core::*;

fn schema() -> Schema {
    let attr = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("tag", ValueType::Str, Kind::State, Cardinality::Many))
        .attribute(attr("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("present", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(attr("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(attr(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(attr("age", ValueType::I64, Kind::State, Cardinality::One))
}

/// The entities every world here starts with.
#[derive(Clone, Copy)]
struct Cast {
    greg: EntityId,
    edwin: EntityId,
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

/// A new world: the cast, Greg and Edwin in Varrow from 600, Edwin named,
/// and a fact at 601, the latest tick.
fn seed(path: &Path) -> (World, Cast) {
    let mut w = World::create(path, schema()).unwrap();
    let mut t = w.begin();
    let cast = Cast {
        greg: t.create_entity("CHARACTER", Some("greg")).unwrap(),
        edwin: t.create_entity("CHARACTER", Some("edwin")).unwrap(),
        mara: t.create_entity("CHARACTER", Some("mara")).unwrap(),
        varrow: t.create_entity("PLACE", Some("varrow")).unwrap(),
        ashford: t.create_entity("PLACE", Some("ashford")).unwrap(),
    };
    for who in [cast.greg, cast.edwin] {
        t.assert(who, "located_in", Value::Ref(cast.varrow), Some(Tick(600)))
            .unwrap();
    }
    t.assert(cast.varrow, "tag", s("founded"), Some(Tick(601)))
        .unwrap();
    t.assert(cast.edwin, "name", s("Edwin"), Some(Tick(0)))
        .unwrap();
    t.commit().unwrap();
    (w, cast)
}

/// Two handles on one seeded file.
struct Pair {
    _dir: tempfile::TempDir,
    path: PathBuf,
    a: World,
    b: World,
    c: Cast,
}

fn pair() -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let (a, c) = seed(&path);
    let b = World::open(&path).unwrap();
    Pair {
        _dir: dir,
        path,
        a,
        b,
        c,
    }
}

/// The bytes of a seeded file after one writer ran `f` on it.
fn serial(f: impl FnOnce(&mut World, Cast)) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let (mut w, c) = seed(&path);
    f(&mut w, c);
    bytes(&path)
}

fn s(text: &str) -> Value {
    Value::Str(text.into())
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn clean(p: &mut Pair) {
    p.a.catch_up().unwrap();
    p.b.catch_up().unwrap();
    for w in [&p.a, &p.b] {
        let report = doctor(w, &DoctorOptions::default());
        assert!(report.is_clean(), "{report:?}");
    }
    let report = doctor_log(&p.path, &DoctorOptions::default()).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(p.a.index_dump(), p.b.index_dump());
}

fn conflict(result: Result<CommitResult, Error>) -> Conflict {
    match result {
        Err(Error::Conflict(c)) => c,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

fn edge_defines(w: &World) -> Vec<(EdgeId, EntityId, EntityId)> {
    w.entries()
        .filter_map(|e| match e.body {
            EntryBody::DefineEdge {
                id, source, target, ..
            } => Some((id, source, target)),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------ blind writes

#[test]
fn blind_writes_from_two_snapshots_both_commit_like_serial() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.assert(c.greg, "tag", s("from a"), Some(Tick(5)))
        .unwrap();
    tb.assert(c.greg, "tag", s("from b"), Some(Tick(3)))
        .unwrap();
    tb.retract(c.edwin, "located_in", Value::Ref(c.varrow), Some(Tick(700)))
        .unwrap();
    let ra = ta.commit().unwrap();
    let rb = tb.commit().unwrap();
    assert!(ra.committed && rb.committed);
    assert_eq!(rb.tx, TxId(ra.tx.0 + 1));
    assert_eq!(rb.changed, 2);
    assert!(rb.ids.is_empty());
    assert_eq!(rb.latest_tick_moved, None);
    assert_eq!(rb.finalized.as_ref().map(|f| f.applied), Some(1));
    assert_eq!(ra.finalized, None, "nothing was appended before a's commit");
    let expected = serial(|w, c| {
        w.assert(c.greg, "tag", s("from a"), Some(Tick(5))).unwrap();
        let mut t = w.begin();
        t.assert(c.greg, "tag", s("from b"), Some(Tick(3))).unwrap();
        t.retract(c.edwin, "located_in", Value::Ref(c.varrow), Some(Tick(700)))
            .unwrap();
        t.commit().unwrap();
    });
    assert_eq!(bytes(&p.path), expected);
    clean(&mut p);
}

// ------------------------------------------------------------ provisional IDs

#[test]
fn two_creates_of_one_type_renumber_the_second_and_everything_on_it() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    let lisa = ta.create_entity("CHARACTER", Some("lisa")).unwrap();
    ta.assert(lisa, "tag", s("a's"), Some(Tick(1))).unwrap();
    let kael = tb.create_entity("CHARACTER", Some("kael")).unwrap();
    assert_eq!(kael, lisa, "both snapshots hand out the same next number");
    // A fact on it, a ref to it, an edge from it and a property on that edge.
    tb.assert(kael, "tag", s("b's"), Some(Tick(1))).unwrap();
    tb.assert(c.greg, "ally_of", Value::Ref(kael), Some(Tick(2)))
        .unwrap();
    tb.assert(kael, "present", Value::Ref(c.varrow), Some(Tick(3)))
        .unwrap();
    let edge = tb.edge_id(kael, "present", c.varrow).unwrap();
    tb.assert(edge.into(), "disguised", Value::Bool(true), Some(Tick(3)))
        .unwrap();
    ta.commit().unwrap();
    let rb = tb.commit().unwrap();

    let final_kael = p.b.entity_by_handle("kael").unwrap();
    assert_eq!(final_kael.seq(), lisa.seq() + 1);
    assert_eq!(rb.ids.entities, vec![(kael, final_kael)]);
    assert_eq!(rb.ids.resolve(kael), final_kael);
    assert_eq!(rb.ids.resolve(c.greg), c.greg);
    assert_eq!(rb.changed, 4);
    let far = Tick(i64::MAX);
    assert_eq!(p.b.as_of(final_kael, "tag", far).unwrap(), vec![s("b's")]);
    assert_eq!(
        p.b.as_of(c.greg, "ally_of", far).unwrap(),
        vec![Value::Ref(final_kael)]
    );
    let final_edge = p.b.edge_id(final_kael, "present", c.varrow).unwrap();
    assert_eq!(rb.ids.resolve_edge(edge), final_edge);
    assert_eq!(
        p.b.as_of(final_edge.into(), "disguised", far).unwrap(),
        vec![Value::Bool(true)]
    );
    // Lisa is untouched by b's facts.
    p.a.catch_up().unwrap();
    let lisa_now = p.a.entity_by_handle("lisa").unwrap();
    assert_eq!(lisa_now, lisa);
    assert_eq!(p.a.as_of(lisa, "tag", far).unwrap(), vec![s("a's")]);
    assert!(p.a.as_of(lisa, "present", far).unwrap().is_empty());

    let expected = serial(|w, c| {
        let mut t = w.begin();
        let lisa = t.create_entity("CHARACTER", Some("lisa")).unwrap();
        t.assert(lisa, "tag", s("a's"), Some(Tick(1))).unwrap();
        t.commit().unwrap();
        let mut t = w.begin();
        let kael = t.create_entity("CHARACTER", Some("kael")).unwrap();
        t.assert(kael, "tag", s("b's"), Some(Tick(1))).unwrap();
        t.assert(c.greg, "ally_of", Value::Ref(kael), Some(Tick(2)))
            .unwrap();
        t.assert(kael, "present", Value::Ref(c.varrow), Some(Tick(3)))
            .unwrap();
        let edge = t.edge_id(kael, "present", c.varrow).unwrap();
        t.assert(edge.into(), "disguised", Value::Bool(true), Some(Tick(3)))
            .unwrap();
        t.commit().unwrap();
    });
    assert_eq!(bytes(&p.path), expected);
    clean(&mut p);
}

#[test]
fn different_new_edges_both_commit_and_the_second_is_remapped() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(1)))
        .unwrap();
    tb.assert(c.edwin, "present", Value::Ref(c.ashford), Some(Tick(1)))
        .unwrap();
    let ea = ta.edge_id(c.greg, "present", c.varrow).unwrap();
    let eb = tb.edge_id(c.edwin, "present", c.ashford).unwrap();
    assert_eq!(ea, eb, "both snapshots hand out the same next edge");
    tb.assert(eb.into(), "disguised", Value::Bool(false), Some(Tick(1)))
        .unwrap();
    ta.commit().unwrap();
    let rb = tb.commit().unwrap();
    let final_eb = p.b.edge_id(c.edwin, "present", c.ashford).unwrap();
    assert_eq!(final_eb.seq(), ea.seq() + 1);
    assert_eq!(rb.ids.edges, vec![(eb, final_eb)]);
    assert_eq!(rb.edges, vec![final_eb]);
    assert_eq!(p.b.edge_id(c.greg, "present", c.varrow), Some(ea));
    assert_eq!(
        p.b.as_of(final_eb.into(), "disguised", Tick(1)).unwrap(),
        vec![Value::Bool(false)]
    );
    assert!(
        p.b.as_of(ea.into(), "disguised", Tick(1))
            .unwrap()
            .is_empty()
    );
    clean(&mut p);
}

#[test]
fn the_same_edge_triple_is_reused_with_one_define() {
    let mut p = pair();
    let c = p.c;
    let before = edge_defines(&p.a).len();
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    // a makes another edge first, so the shared one gets different numbers.
    ta.assert(c.mara, "present", Value::Ref(c.ashford), Some(Tick(1)))
        .unwrap();
    ta.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(1)))
        .unwrap();
    tb.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(4)))
        .unwrap();
    let eb = tb.edge_id(c.greg, "present", c.varrow).unwrap();
    tb.assert(eb.into(), "disguised", Value::Bool(true), Some(Tick(4)))
        .unwrap();
    ta.commit().unwrap();
    let rb = tb.commit().unwrap();
    let shared = p.b.edge_id(c.greg, "present", c.varrow).unwrap();
    assert_ne!(shared, eb);
    assert_eq!(rb.ids.edges, vec![(eb, shared)]);
    assert!(rb.edges.is_empty(), "b interned nothing new");
    // The assert at 4 is redundant (he is present from 1): only the property.
    assert_eq!(rb.changed, 1);
    let defines = edge_defines(&p.b);
    assert_eq!(defines.len(), before + 2);
    // Besides the seed's located_in edge between the same two.
    assert_eq!(
        defines
            .iter()
            .filter(|(_, src, tgt)| (*src, *tgt) == (c.greg, c.varrow))
            .count(),
        2,
        "exactly one DEFINE for the shared triple"
    );
    assert_eq!(
        p.b.as_of(shared.into(), "disguised", Tick(4)).unwrap(),
        vec![Value::Bool(true)]
    );
    clean(&mut p);
}

#[test]
fn a_bidirectional_pair_written_in_opposite_orders_is_one_edge() {
    let mut p = pair();
    let c = p.c;
    let before = edge_defines(&p.a).len();
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.assert(c.edwin, "ally_of", Value::Ref(c.mara), Some(Tick(1)))
        .unwrap();
    tb.assert(c.mara, "ally_of", Value::Ref(c.edwin), Some(Tick(1)))
        .unwrap();
    let eb = tb.edge_id(c.mara, "ally_of", c.edwin).unwrap();
    tb.assert(eb.into(), "tag", s("public"), Some(Tick(1)))
        .unwrap();
    let ra = ta.commit().unwrap();
    let rb = tb.commit().unwrap();
    assert_eq!(edge_defines(&p.b).len(), before + 1);
    assert_eq!(rb.changed, 1, "the pair's assert is `0 changed`");
    assert!(rb.edges.is_empty());
    assert!(rb.ids.is_empty(), "same number, same edge");
    assert_eq!(p.b.edge_id(c.edwin, "ally_of", c.mara), Some(ra.edges[0]));
    assert_eq!(
        p.b.as_of(ra.edges[0].into(), "tag", Tick(1)).unwrap(),
        vec![s("public")]
    );
    clean(&mut p);
}

#[test]
fn with_ids_exposed_a_renumbering_or_merge_is_a_conflict_and_writes_nothing() {
    // (what a does, what b does): each makes b's provisional ID move.
    type Ops = fn(&mut Txn<'_>, Cast);
    let cases: [(&str, Ops, Ops); 3] = [
        (
            "entity renumbered",
            |t, _| t.create_entity("CHARACTER", None).map(drop).unwrap(),
            |t, _| t.create_entity("CHARACTER", None).map(drop).unwrap(),
        ),
        (
            "edge renumbered",
            |t, c| {
                t.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(1)))
                    .unwrap();
            },
            |t, c| {
                t.assert(c.mara, "present", Value::Ref(c.varrow), Some(Tick(1)))
                    .unwrap();
            },
        ),
        (
            "edge merged, same number",
            |t, c| {
                t.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(1)))
                    .unwrap();
            },
            |t, c| {
                t.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(9)))
                    .unwrap();
            },
        ),
    ];
    for (name, a_ops, b_ops) in cases {
        for mode in [Concurrency::Optimistic, Concurrency::Weak] {
            let mut p = pair();
            let c = p.c;
            let mut ta = p.a.begin();
            let mut tb = p.b.begin();
            tb.set_concurrency(mode);
            tb.set_ids_exposed(true);
            assert!(tb.ids_exposed());
            a_ops(&mut ta, c);
            b_ops(&mut tb, c);
            ta.commit().unwrap();
            let before = bytes(&p.path);
            let got = conflict(tb.commit());
            assert!(
                matches!(got.key, ConflictKey::ExposedId { .. }),
                "{name}: {got:?}"
            );
            assert_eq!(got.key.kind(), "exposed-id");
            assert_eq!((got.operation, got.applied), (0, 1), "{name}");
            assert_eq!(bytes(&p.path), before, "{name}: nothing written");
            // Run again on the caught-up world: commits.
            let mut tb = p.b.begin();
            tb.set_ids_exposed(true);
            b_ops(&mut tb, c);
            tb.commit().unwrap();
            clean(&mut p);
        }
    }
}

#[test]
fn ids_exposed_does_not_mind_other_commits_that_move_nothing() {
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    tb.set_ids_exposed(true);
    let kael = tb.create_entity("CHARACTER", Some("kael")).unwrap();
    p.a.create_entity("PLACE", None).unwrap();
    p.a.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap();
    let rb = tb.commit().unwrap();
    assert!(rb.ids.is_empty());
    assert_eq!(p.b.entity_by_handle("kael"), Some(kael));
    clean(&mut p);
}

// ------------------------------------------------------------ the latest tick

#[test]
fn a_tickless_write_stays_on_the_snapshots_latest_tick_and_warns() {
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    let staged = tb.assert(c.mara, "tag", s("late"), None).unwrap();
    assert_eq!(staged.tick, Tick(601));
    p.a.assert(c.greg, "tag", s("moves it"), Some(Tick(610)))
        .unwrap();
    let rb = tb.commit().unwrap();
    assert_eq!(rb.changed, 1);
    let moved = rb.latest_tick_moved.expect("the latest tick moved");
    assert_eq!(
        moved,
        TickMoved {
            from: Tick(601),
            to: Tick(610)
        }
    );
    assert_eq!(
        moved.to_string(),
        "latest tick moved 601 → 610 while this transaction ran"
    );
    let history = p.b.history(c.mara, "tag").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].tick, Tick(601));
    assert_eq!(p.b.latest_tick(), Tick(610));
    clean(&mut p);
}

#[test]
fn no_latest_tick_warning_without_a_tickless_write_or_without_a_move() {
    let mut p = pair();
    let c = p.c;
    // Explicit ticks only.
    let mut tb = p.b.begin();
    tb.assert(c.mara, "tag", s("explicit"), Some(Tick(5)))
        .unwrap();
    p.a.assert(c.greg, "tag", s("moves it"), Some(Tick(610)))
        .unwrap();
    assert_eq!(tb.commit().unwrap().latest_tick_moved, None);
    // Tick-less, but the other commit didn't move the latest tick.
    let mut tb = p.b.begin();
    tb.assert(c.mara, "tag", s("tickless"), None).unwrap();
    p.a.assert(c.greg, "tag", s("earlier"), Some(Tick(7)))
        .unwrap();
    let rb = tb.commit().unwrap();
    assert!(rb.finalized.is_some());
    assert_eq!(rb.latest_tick_moved, None);
    clean(&mut p);
}

#[test]
fn both_tickless_with_the_same_op_on_the_same_value_the_second_is_0_changed() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.assert(c.mara, "tag", s("same"), None).unwrap();
    assert_eq!(
        tb.assert(c.mara, "tag", s("same"), None).unwrap().changed,
        1
    );
    ta.commit().unwrap();
    let before = bytes(&p.path);
    let rb = tb.commit().unwrap();
    assert!(!rb.committed, "nothing left to append");
    assert_eq!(rb.changed, 0);
    assert_eq!(rb.finalized.unwrap().ops[0].changed, 0);
    assert_eq!(rb.latest_tick_moved, None);
    assert_eq!(bytes(&p.path), before);
    clean(&mut p);
}

#[test]
fn both_tickless_with_opposite_ops_is_the_ordinary_same_tick_error() {
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        let mut p = pair();
        let c = p.c;
        let mut ta = p.a.begin();
        let mut tb = p.b.begin();
        tb.set_concurrency(mode);
        // a asserts Greg present in Ashford at the latest tick (601); b,
        // after an assert of its own at 500, retracts that at 601.
        ta.assert(c.greg, "present", Value::Ref(c.ashford), None)
            .unwrap();
        tb.assert(c.greg, "present", Value::Ref(c.ashford), Some(Tick(500)))
            .unwrap();
        tb.retract(c.greg, "present", Value::Ref(c.ashford), None)
            .unwrap();
        ta.commit().unwrap();
        let before = bytes(&p.path);
        let err = tb.commit().unwrap_err();
        assert_eq!(
            err,
            Error::SameTickConflict {
                entity: c.greg,
                attr: "present".into(),
                tick: Tick(601)
            },
            "{mode:?}"
        );
        assert_eq!(bytes(&p.path), before, "nothing written");
        clean(&mut p);
    }
}

/// CLAUDE.md: "my tick-less `retract Edwin located_in Varrow` landed at 601
/// while another commit wrote the same retract at 610: both are stored (he
/// left at 601; the 610 retract is a harmless leftover)".
#[test]
fn the_worked_example_both_retracts_are_stored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut a = World::create(&path, schema()).unwrap();
    let edwin = a.create_entity("CHARACTER", Some("edwin")).unwrap();
    let varrow = a.create_entity("PLACE", Some("varrow")).unwrap();
    a.assert(edwin, "located_in", Value::Ref(varrow), Some(Tick(200)))
        .unwrap();
    a.assert(edwin, "tag", s("sets the latest tick"), Some(Tick(601)))
        .unwrap();
    let mut b = World::open(&path).unwrap();

    let mut mine = b.begin();
    let staged = mine
        .retract(edwin, "located_in", Value::Ref(varrow), None)
        .unwrap();
    assert_eq!((staged.tick, staged.changed), (Tick(601), 1));
    a.retract(edwin, "located_in", Value::Ref(varrow), Some(Tick(610)))
        .unwrap();
    let result = mine.commit().unwrap();
    assert_eq!(result.changed, 1);
    assert_eq!(
        result.latest_tick_moved,
        Some(TickMoved {
            from: Tick(601),
            to: Tick(610)
        })
    );
    let history: Vec<_> = b
        .history(edwin, "located_in")
        .unwrap()
        .into_iter()
        .map(|change| (change.tick, change.op))
        .collect();
    assert_eq!(
        history,
        vec![
            (Tick(200), Op::Assert),
            (Tick(601), Op::Retract),
            (Tick(610), Op::Retract)
        ]
    );
    assert!(b.as_of(edwin, "located_in", Tick(605)).unwrap().is_empty());
    assert!(doctor(&b, &DoctorOptions::default()).is_clean());
}

// ------------------------------------------------------------ read keys

/// `:set` at the native level: read what holds, retract it, assert the new.
fn set_location(t: &mut Txn<'_>, who: EntityId, to: EntityId) -> Tick {
    let tick = t.write_tick(None);
    let meter = WorkMeter::bounded(10_000, None);
    let held = t
        .values_at(who, "located_in", tick, 16, 1 << 20, &meter)
        .unwrap();
    for value in held.values {
        t.retract(who, "located_in", value, Some(tick)).unwrap();
    }
    t.assert(who, "located_in", Value::Ref(to), Some(tick))
        .unwrap();
    tick
}

#[test]
fn a_replace_conflicts_with_a_newer_entry_on_what_it_read_and_reruns_cleanly() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    set_location(&mut ta, c.greg, c.ashford);
    let place = tb.create_entity("PLACE", Some("carnath")).unwrap();
    set_location(&mut tb, c.greg, place);
    // a moves the latest tick too, so the re-run lands later.
    ta.assert(c.mara, "tag", s("later"), Some(Tick(650)))
        .unwrap();
    ta.commit().unwrap();
    let before = bytes(&p.path);
    let got = conflict(tb.commit());
    assert_eq!(
        got.key,
        ConflictKey::EntityAttribute {
            entity: c.greg,
            attr: "located_in".into()
        }
    );
    assert_eq!(got.key.kind(), "entity-attribute");
    // The read came after the create (operation 0), before the retract.
    assert_eq!((got.operation, got.applied), (1, 1));
    assert_eq!(bytes(&p.path), before, "a conflict writes nothing");
    assert_eq!(p.b.entity_by_handle("carnath"), None);

    // Run again on a new transaction: it reads Ashford and the new tick.
    let mut tb = p.b.begin();
    let place = tb.create_entity("PLACE", Some("carnath")).unwrap();
    assert_eq!(set_location(&mut tb, c.greg, place), Tick(650));
    let rb = tb.commit().unwrap();
    assert_eq!(rb.changed, 2);
    assert_eq!(rb.finalized, None);
    let moves: Vec<_> =
        p.b.history(c.greg, "located_in")
            .unwrap()
            .into_iter()
            .map(|ch| (ch.tick, ch.op, ch.value))
            .collect();
    assert!(moves.contains(&(Tick(650), Op::Retract, Value::Ref(c.ashford))));
    assert!(moves.contains(&(Tick(650), Op::Assert, Value::Ref(place))));
    assert_eq!(
        p.b.as_of(c.greg, "located_in", Tick(700)).unwrap(),
        vec![Value::Ref(place)]
    );
    clean(&mut p);
}

#[test]
fn weak_mode_lets_the_second_replace_through_as_a_lost_update() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    tb.set_concurrency(Concurrency::Weak);
    assert_eq!(tb.concurrency(), Concurrency::Weak);
    set_location(&mut ta, c.greg, c.ashford);
    let carnath = tb.create_entity("PLACE", Some("carnath")).unwrap();
    set_location(&mut tb, c.greg, carnath);
    ta.commit().unwrap();
    let rb = tb.commit().unwrap();
    // Its retract of Varrow is now redundant; its assert is stored: Greg is
    // in Ashford and Carnath (an ordinary overlap warning).
    assert_eq!(rb.changed, 1);
    assert_eq!(rb.warnings.introduced.len(), 1);
    assert_eq!(p.b.as_of(c.greg, "located_in", Tick(601)).unwrap().len(), 2);
}

#[test]
fn find_or_create_of_one_free_handle_conflicts_optimistic_and_settles_weak() {
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        let mut p = pair();
        let c = p.c;
        let mut ta = p.a.begin();
        let mut tb = p.b.begin();
        tb.set_concurrency(mode);
        let (lisa_a, made_a) = ta.find_or_create_entity("CHARACTER", "lisa").unwrap();
        // b creates another first, so its provisional lisa differs.
        tb.create_entity("CHARACTER", None).unwrap();
        let (lisa_b, made_b) = tb.find_or_create_entity("CHARACTER", "lisa").unwrap();
        assert!(made_a && made_b);
        tb.assert(lisa_b, "tag", s("b's lisa"), Some(Tick(1)))
            .unwrap();
        tb.assert(c.greg, "ally_of", Value::Ref(lisa_b), Some(Tick(1)))
            .unwrap();
        ta.commit().unwrap();
        let before = bytes(&p.path);
        match mode {
            Concurrency::Optimistic => {
                let got = conflict(tb.commit());
                assert_eq!(
                    got.key,
                    ConflictKey::Handle {
                        handle: "lisa".into()
                    }
                );
                assert_eq!(got.operation, 1);
                assert_eq!(bytes(&p.path), before);
                let mut tb = p.b.begin();
                assert_eq!(
                    tb.find_or_create_entity("CHARACTER", "lisa").unwrap(),
                    (lisa_a, false),
                    "run again: it exists"
                );
                assert!(!tb.commit().unwrap().committed);
            }
            Concurrency::Weak => {
                let rb = tb.commit().unwrap();
                let fin = rb.finalized.as_ref().unwrap();
                assert_eq!(fin.ops[1].created, Some(false), "settled to `exists`");
                assert_eq!(rb.ids.resolve(lisa_b), lisa_a);
                assert_eq!(p.b.entity_by_handle("lisa"), Some(lisa_a));
                let characters = p.b.entities_of_type("CHARACTER").unwrap().count();
                // The cast's three, lisa once, and b's anonymous one.
                assert_eq!(characters, 5, "no second lisa");
                assert_eq!(
                    p.b.as_of(lisa_a, "tag", Tick(1)).unwrap(),
                    vec![s("b's lisa")]
                );
                assert_eq!(
                    p.b.as_of(c.greg, "ally_of", Tick(1)).unwrap(),
                    vec![Value::Ref(lisa_a)]
                );
            }
        }
        clean(&mut p);
    }
}

/// b records `read`, a commits `write`, b (which also writes a tag, so it
/// has something to commit) commits: the outcome.
fn race(
    read: impl FnOnce(&Txn<'_>, Cast),
    write: impl FnOnce(&mut World, Cast),
) -> Result<CommitResult, Error> {
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    read(&tb, c);
    tb.assert(c.mara, "tag", s("b writes"), Some(Tick(1)))
        .unwrap();
    write(&mut p.a, c);
    let before = bytes(&p.path);
    let result = tb.commit();
    if result.is_err() {
        assert_eq!(bytes(&p.path), before, "a refused commit writes nothing");
    }
    clean(&mut p);
    result
}

#[test]
fn a_reverse_read_conflicts_with_a_newer_arrival_there_and_nowhere_else() {
    let read = |t: &Txn<'_>, c: Cast| t.note_reverse_read(c.varrow, "located_in").unwrap();
    let got = conflict(race(read, |w, c| {
        w.assert(c.mara, "located_in", Value::Ref(c.varrow), Some(Tick(5)))
            .unwrap();
    }));
    assert_eq!(got.key.kind(), "reverse");
    assert!(
        matches!(&got.key, ConflictKey::Reverse { attr, .. } if attr == "located_in"),
        "{got:?}"
    );
    // Someone leaving Varrow is a write through the same key.
    conflict(race(read, |w, c| {
        w.retract(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(700)))
            .unwrap();
    }));
    // Ashford, or another attribute pointing at Varrow: no conflict.
    race(read, |w, c| {
        w.assert(c.mara, "located_in", Value::Ref(c.ashford), Some(Tick(5)))
            .unwrap();
        w.assert(c.mara, "present", Value::Ref(c.varrow), Some(Tick(5)))
            .unwrap();
        w.assert(c.varrow, "tag", s("on varrow itself"), Some(Tick(5)))
            .unwrap();
    })
    .unwrap();
}

#[test]
fn a_forward_read_conflicts_only_with_that_entity_and_attribute() {
    let read = |t: &Txn<'_>, c: Cast| t.note_read(c.greg, "tag").unwrap();
    let got = conflict(race(read, |w, c| {
        w.assert(c.greg, "tag", s("x"), Some(Tick(5))).unwrap();
    }));
    assert_eq!(got.key.kind(), "entity-attribute");
    race(read, |w, c| {
        w.assert(c.edwin, "tag", s("x"), Some(Tick(5))).unwrap();
        w.assert(c.greg, "name", s("Greg"), Some(Tick(5))).unwrap();
    })
    .unwrap();
}

#[test]
fn a_bidirectional_read_conflicts_through_either_end_of_the_pair() {
    // Mara's allies: the pair is stored once, under the smaller ID (Edwin).
    let read = |t: &Txn<'_>, c: Cast| t.note_read(c.mara, "ally_of").unwrap();
    conflict(race(read, |w, c| {
        w.assert(c.edwin, "ally_of", Value::Ref(c.mara), Some(Tick(5)))
            .unwrap();
    }));
    conflict(race(read, |w, c| {
        w.assert(c.mara, "ally_of", Value::Ref(c.greg), Some(Tick(5)))
            .unwrap();
    }));
    let read_greg = |t: &Txn<'_>, c: Cast| t.note_reverse_read(c.greg, "ally_of").unwrap();
    conflict(race(read_greg, |w, c| {
        w.assert(c.greg, "ally_of", Value::Ref(c.mara), Some(Tick(5)))
            .unwrap();
    }));
    race(read, |w, c| {
        w.assert(c.edwin, "ally_of", Value::Ref(c.greg), Some(Tick(5)))
            .unwrap();
    })
    .unwrap();
}

#[test]
fn a_value_read_conflicts_with_a_newer_entry_for_that_value_only() {
    let read = |t: &Txn<'_>, _: Cast| t.note_value_read("name", &s("Edwin")).unwrap();
    let got = conflict(race(read, |w, c| {
        w.assert(c.mara, "name", s("Edwin"), Some(Tick(5))).unwrap();
    }));
    assert_eq!(
        got.key,
        ConflictKey::Value {
            attr: "name".into()
        }
    );
    conflict(race(read, |w, c| {
        w.retract(c.edwin, "name", s("Edwin"), Some(Tick(50)))
            .unwrap();
    }));
    race(read, |w, c| {
        w.assert(c.mara, "name", s("Mara"), Some(Tick(5))).unwrap();
        w.assert(c.mara, "tag", s("Edwin"), Some(Tick(5))).unwrap();
    })
    .unwrap();
    // A ref value is the reverse read of its target.
    let read_ref = |t: &Txn<'_>, c: Cast| {
        t.note_value_read("located_in", &Value::Ref(c.varrow))
            .unwrap();
    };
    let got = conflict(race(read_ref, |w, c| {
        w.assert(c.mara, "located_in", Value::Ref(c.varrow), Some(Tick(5)))
            .unwrap();
    }));
    assert_eq!(got.key.kind(), "reverse");
}

#[test]
fn a_type_scan_conflicts_with_a_new_entity_of_that_type_only() {
    let read = |t: &Txn<'_>, _: Cast| t.note_type_scan("CHARACTER").unwrap();
    let got = conflict(race(read, |w, _| {
        w.create_entity("CHARACTER", None).unwrap();
    }));
    assert_eq!(
        got.key,
        ConflictKey::TypeScan {
            node_type: "CHARACTER".into()
        }
    );
    race(read, |w, c| {
        w.create_entity("PLACE", Some("carnath")).unwrap();
        w.assert(c.greg, "tag", s("x"), Some(Tick(5))).unwrap();
    })
    .unwrap();
}

#[test]
fn an_attribute_read_conflicts_with_any_newer_entry_or_schema_change_on_it() {
    let read = |t: &Txn<'_>, _: Cast| t.note_read_attribute("tag").unwrap();
    let got = conflict(race(read, |w, c| {
        w.assert(c.varrow, "tag", s("x"), Some(Tick(5))).unwrap();
    }));
    assert_eq!(got.key, ConflictKey::Attribute { attr: "tag".into() });
    let got = conflict(race(read, |w, _| {
        let mut t = w.begin();
        t.rename_attribute("tag", "label").unwrap();
        t.commit().unwrap();
    }));
    assert_eq!(
        got.key,
        ConflictKey::Attribute {
            attr: "label".into()
        }
    );
    race(read, |w, c| {
        w.assert(c.greg, "name", s("Greg"), Some(Tick(5))).unwrap();
        w.create_entity("PLACE", None).unwrap();
    })
    .unwrap();
}

#[test]
fn a_schema_change_of_an_attribute_conflicts_with_exact_reads_of_it() {
    let read = |t: &Txn<'_>, c: Cast| t.note_read(c.greg, "located_in").unwrap();
    let got = conflict(race(read, |w, _| {
        let mut t = w.begin();
        t.set_cardinality("located_in", Cardinality::Many).unwrap();
        t.commit().unwrap();
    }));
    assert_eq!(got.key.kind(), "attribute");
}

#[test]
fn read_all_conflicts_with_any_newer_commit() {
    let read = |t: &Txn<'_>, _: Cast| t.note_read_all();
    let got = conflict(race(read, |w, _| {
        w.create_entity("PLACE", None).unwrap();
    }));
    assert_eq!(got.key, ConflictKey::All);
    assert_eq!(
        got.to_string(),
        "conflict: another transaction committed after this one read the world (1 newer transaction(s), read before operation 1); nothing was written, run the transaction again"
    );
    // No newer commit: no conflict.
    race(read, |_, _| {}).unwrap();
}

#[test]
fn a_newer_correction_on_a_read_key_conflicts_like_a_newer_entry() {
    fn entry_of(w: &World, who: EntityId) -> EntryId {
        w.entries()
            .find(|e| {
                matches!(&e.body, EntryBody::Fact { entity, tick, .. }
                    if *entity == who && *tick == Tick(600))
            })
            .unwrap()
            .id
    }
    let forward = |t: &Txn<'_>, c: Cast| t.note_read(c.greg, "located_in").unwrap();
    conflict(race(forward, |w, c| {
        let entry = entry_of(w, c.greg);
        w.correct(entry, Correction::Delete).unwrap();
    }));
    let reverse = |t: &Txn<'_>, c: Cast| t.note_reverse_read(c.ashford, "located_in").unwrap();
    // A correction that moves Edwin's fact to Ashford writes through the
    // Ashford key (its new entry), though nothing there was read before.
    conflict(race(reverse, |w, c| {
        let entry = entry_of(w, c.edwin);
        let moved = Correction::Replace {
            value: Some(Value::Ref(c.ashford)),
            tick: None,
            op: None,
            precision: None,
        };
        w.correct(entry, moved).unwrap();
    }));
    // Correcting Edwin's fact doesn't touch a read of Greg's.
    race(forward, |w, c| {
        let entry = entry_of(w, c.edwin);
        w.correct(entry, Correction::Delete).unwrap();
    })
    .unwrap();
}

#[test]
fn reads_of_unknown_names_are_errors_that_do_not_poison() {
    let mut p = pair();
    let c = p.c;
    let mut t = p.b.begin();
    assert!(matches!(
        t.note_read(c.greg, "nope"),
        Err(Error::UnknownAttribute(_))
    ));
    assert!(matches!(
        t.note_type_scan("NOPE"),
        Err(Error::UnknownNodeType(_))
    ));
    assert!(matches!(
        t.note_read_attribute("nope"),
        Err(Error::UnknownAttribute(_))
    ));
    t.assert(c.greg, "tag", s("still fine"), Some(Tick(1)))
        .unwrap();
    assert!(t.commit().unwrap().committed);
}

#[test]
fn write_skew_cannot_commit_twice_under_optimistic_and_does_under_weak() {
    // "Someone must stay in Varrow": each transaction sees two characters
    // there and moves a different one out.
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        let mut p = pair();
        let c = p.c;
        let mut ta = p.a.begin();
        let mut tb = p.b.begin();
        for (t, who) in [(&mut ta, c.greg), (&mut tb, c.edwin)] {
            t.set_concurrency(mode);
            t.note_reverse_read(c.varrow, "located_in").unwrap();
            t.retract(who, "located_in", Value::Ref(c.varrow), Some(Tick(700)))
                .unwrap();
        }
        ta.commit().unwrap();
        let second = tb.commit();
        let left_in_varrow =
            |w: &World| w.neighbors(c.varrow, "located_in", Direction::In, Tick(700));
        p.b.catch_up().unwrap();
        match mode {
            Concurrency::Optimistic => {
                assert_eq!(conflict(second).key.kind(), "reverse");
                assert_eq!(left_in_varrow(&p.b).unwrap().len(), 1);
            }
            Concurrency::Weak => {
                assert!(second.unwrap().committed);
                assert!(left_in_varrow(&p.b).unwrap().is_empty(), "the skew");
            }
        }
        clean(&mut p);
    }
}

// ------------------------------------------------------------ coarsening

/// A world with enough entities to read more keys than the bound.
fn crowd(extra: usize) -> (Pair, Vec<EntityId>) {
    let mut p = pair();
    let mut t = p.a.begin();
    let crowd: Vec<_> = (0..extra)
        .map(|_| t.create_entity("CHARACTER", None).unwrap())
        .collect();
    t.commit().unwrap();
    p.b.catch_up().unwrap();
    (p, crowd)
}

#[test]
fn past_the_bound_the_read_set_is_kept_per_attribute() {
    assert_eq!(MAX_READ_KEYS, 1024);
    for (keys, conflicts) in [(MAX_READ_KEYS, false), (MAX_READ_KEYS + 1, true)] {
        let (mut p, crowd) = crowd(MAX_READ_KEYS + 2);
        let c = p.c;
        let outsider = crowd[MAX_READ_KEYS + 1];
        let mut tb = p.b.begin();
        for who in &crowd[..keys] {
            tb.note_read(*who, "tag").unwrap();
        }
        tb.assert(c.mara, "name", s("Mara"), Some(Tick(1))).unwrap();
        // The same attribute, an entity that wasn't read; and another
        // attribute, which never conflicts.
        p.a.assert(outsider, "tag", s("x"), Some(Tick(1))).unwrap();
        p.a.assert(outsider, "age", Value::I64(3), Some(Tick(1)))
            .unwrap();
        let result = tb.commit();
        if conflicts {
            assert_eq!(
                conflict(result).key,
                ConflictKey::Attribute { attr: "tag".into() },
                "{keys} keys"
            );
        } else {
            assert!(result.unwrap().committed, "{keys} keys");
        }
        clean(&mut p);
    }
}

#[test]
fn past_the_bound_type_scans_and_handles_become_any_new_entity() {
    let (mut p, crowd) = crowd(MAX_READ_KEYS + 1);
    let c = p.c;
    let mut tb = p.b.begin();
    tb.note_type_scan("CHARACTER").unwrap();
    for who in &crowd {
        tb.note_read(*who, "tag").unwrap();
    }
    tb.assert(c.mara, "name", s("Mara"), Some(Tick(1))).unwrap();
    p.a.create_entity("PLACE", None).unwrap();
    assert_eq!(conflict(tb.commit()).key, ConflictKey::AnyDefine);
    clean(&mut p);
}

// ------------------------------------------------------------ store rules

#[test]
fn store_rules_fail_with_their_ordinary_errors_in_both_modes() {
    for mode in [Concurrency::Optimistic, Concurrency::Weak] {
        // A duplicate handle on a plain create.
        let mut p = pair();
        let mut tb = p.b.begin();
        tb.set_concurrency(mode);
        tb.create_entity("CHARACTER", Some("lisa")).unwrap();
        p.a.create_entity("PLACE", Some("lisa")).unwrap();
        let before = bytes(&p.path);
        assert_eq!(
            tb.commit().unwrap_err(),
            Error::DuplicateHandle("lisa".into()),
            "{mode:?}"
        );
        assert_eq!(bytes(&p.path), before);
        clean(&mut p);

        // The opposite op at the same tick.
        let mut p = pair();
        let c = p.c;
        let mut tb = p.b.begin();
        tb.set_concurrency(mode);
        tb.assert(c.greg, "tag", s("t"), Some(Tick(9))).unwrap();
        p.a.assert(c.greg, "tag", s("t"), Some(Tick(5))).unwrap();
        p.a.retract(c.greg, "tag", s("t"), Some(Tick(9))).unwrap();
        assert!(
            matches!(
                tb.commit(),
                Err(Error::SameTickConflict { tick: Tick(9), .. })
            ),
            "{mode:?}"
        );
        clean(&mut p);

        // A deprecated attribute, and a value of the wrong type.
        let mut p = pair();
        let c = p.c;
        let mut tb = p.b.begin();
        tb.set_concurrency(mode);
        tb.assert(c.greg, "tag", s("t"), Some(Tick(9))).unwrap();
        let mut ta = p.a.begin();
        ta.deprecate_attribute("tag").unwrap();
        ta.commit().unwrap();
        assert_eq!(
            tb.commit().unwrap_err(),
            Error::DeprecatedAttribute("tag".into()),
            "{mode:?}"
        );
        clean(&mut p);

        // Strict schema set meanwhile: an inferred declaration is refused.
        let mut p = pair();
        let c = p.c;
        let mut tb = p.b.begin();
        tb.set_concurrency(mode);
        tb.infer_attribute("mood", &s("grim")).unwrap();
        tb.assert(c.greg, "mood", s("grim"), Some(Tick(9))).unwrap();
        let mut ta = p.a.begin();
        ta.world_setting("schema", "strict").unwrap();
        ta.commit().unwrap();
        assert!(matches!(tb.commit(), Err(Error::Schema(_))), "{mode:?}");
        clean(&mut p);
    }
}

#[test]
fn correcting_an_entry_the_other_handle_corrected() {
    fn gregs_entry(w: &World, c: Cast) -> EntryId {
        w.entries()
            .find(|e| matches!(&e.body, EntryBody::Fact { entity, .. } if *entity == c.greg))
            .unwrap()
            .id
    }
    // Weak: an ordinary store error, saying the entry was corrected
    // meanwhile (PR #20 review, item J: one error whether the other writer
    // deleted or replaced; this was `AlreadyDeleted`).
    let mut p = pair();
    let entry = gregs_entry(&p.a, p.c);
    let mut tb = p.b.begin();
    tb.set_concurrency(Concurrency::Weak);
    tb.correct(entry, Correction::Delete).unwrap();
    p.a.correct(entry, Correction::Delete).unwrap();
    let before = bytes(&p.path);
    assert_eq!(
        tb.commit().unwrap_err(),
        Error::AlreadyCorrected { entry, rev: 1 }
    );
    assert_eq!(bytes(&p.path), before);
    clean(&mut p);

    // Optimistic: a correction read the chain it corrects.
    let mut p = pair();
    let entry = gregs_entry(&p.a, p.c);
    let mut tb = p.b.begin();
    tb.correct(entry, Correction::Delete).unwrap();
    p.a.correct(entry, Correction::Delete).unwrap();
    let got = conflict(tb.commit());
    assert_eq!(got.key, ConflictKey::Entry { entry });
    // Run again: now the ordinary error.
    let mut tb = p.b.begin();
    assert_eq!(
        tb.correct(entry, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(entry)
    );
    drop(tb);
    clean(&mut p);

    // Weak, a replace over the other's replace. This used to commit as the
    // next revision of the chain ("as one writer correcting twice"), which
    // silently overrode the other writer's correction with one decided on
    // the fact before it. Decided in the PR #20 review (item J): refused,
    // like the delete above; the caller looks again and corrects again.
    let mut p = pair();
    let c = p.c;
    let entry = gregs_entry(&p.a, c);
    let at = |tick| Correction::Replace {
        value: None,
        tick: Some(Tick(tick)),
        op: None,
        precision: None,
    };
    let mut tb = p.b.begin();
    tb.set_concurrency(Concurrency::Weak);
    tb.correct(entry, at(590)).unwrap();
    p.a.correct(entry, at(580)).unwrap();
    assert_eq!(
        tb.commit().unwrap_err(),
        Error::AlreadyCorrected { entry, rev: 1 }
    );
    // Only the other writer's correction is in the file; made again on the
    // caught-up world, this one is its next revision.
    let expected = serial(|w, _| {
        w.correct(entry, at(580)).unwrap();
    });
    assert_eq!(bytes(&p.path), expected);
    p.b.correct(entry, at(590)).unwrap();
    let expected = serial(|w, _| {
        w.correct(entry, at(580)).unwrap();
        w.correct(entry, at(590)).unwrap();
    });
    assert_eq!(bytes(&p.path), expected);
    clean(&mut p);
}

// ------------------------------------------------------------ schema races

#[test]
fn the_same_new_attribute_declared_twice_is_redundant_the_second_time() {
    let mut p = pair();
    let c = p.c;
    let mood = || AttrDef::new("mood", ValueType::Str, Kind::State, Cardinality::One);
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.declare_attribute(mood()).unwrap();
    tb.declare_attribute(mood()).unwrap();
    tb.declare_node_type("FACTION").unwrap();
    let guild = tb.create_entity("FACTION", Some("guild")).unwrap();
    tb.assert(c.greg, "mood", s("grim"), Some(Tick(1))).unwrap();
    tb.assert(guild, "mood", s("rich"), Some(Tick(1))).unwrap();
    ta.declare_node_type("ITEM").unwrap();
    let ra = ta.commit().unwrap();
    assert_eq!(ra.declared, 2);
    let rb = tb.commit().unwrap();
    assert_eq!(rb.declared, 1, "only FACTION is new");
    let fin = rb.finalized.unwrap();
    assert_eq!(fin.ops[0].declared, 0, "the attribute is already declared");
    assert_eq!(fin.ops[1].declared, 1);
    // FACTION took the type ID after ITEM, so the guild's ID moved with it.
    let final_guild = p.b.entity_by_handle("guild").unwrap();
    assert_ne!(final_guild, guild);
    assert_eq!(rb.ids.entities, vec![(guild, final_guild)]);
    assert_eq!(p.b.format_id(final_guild), "FACTION#1");
    assert_eq!(
        p.b.as_of(final_guild, "mood", Tick(1)).unwrap(),
        vec![s("rich")]
    );
    let expected = serial(|w, c| {
        let mut t = w.begin();
        t.declare_attribute(mood()).unwrap();
        t.declare_node_type("ITEM").unwrap();
        t.commit().unwrap();
        let mut t = w.begin();
        t.declare_attribute(mood()).unwrap();
        t.declare_node_type("FACTION").unwrap();
        let guild = t.create_entity("FACTION", Some("guild")).unwrap();
        t.assert(c.greg, "mood", s("grim"), Some(Tick(1))).unwrap();
        t.assert(guild, "mood", s("rich"), Some(Tick(1))).unwrap();
        t.commit().unwrap();
    });
    assert_eq!(bytes(&p.path), expected);
    clean(&mut p);
}

#[test]
fn the_same_new_attribute_with_another_type_is_the_ordinary_error() {
    let mut p = pair();
    let c = p.c;
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.declare_attribute(AttrDef::new(
        "mood",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    ta.assert(c.greg, "mood", Value::I64(3), Some(Tick(1)))
        .unwrap();
    tb.declare_attribute(AttrDef::new(
        "mood",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    tb.assert(c.greg, "mood", s("grim"), Some(Tick(2))).unwrap();
    ta.commit().unwrap();
    let before = bytes(&p.path);
    // What one writer gets declaring it again as a string, with facts there.
    let expected = {
        let mut t = p.a.begin();
        t.declare_attribute(AttrDef::new(
            "mood",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .unwrap_err()
    };
    let err = tb.commit().unwrap_err();
    assert_eq!(err, expected);
    assert_eq!(bytes(&p.path), before);
    clean(&mut p);
}

#[test]
fn both_rename_or_both_deprecate_the_same_attribute() {
    // Rename to the same new name: the old name is gone for the second.
    let mut p = pair();
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.rename_attribute("tag", "label").unwrap();
    tb.rename_attribute("tag", "label").unwrap();
    ta.commit().unwrap();
    assert_eq!(
        tb.commit().unwrap_err(),
        Error::UnknownAttribute("tag".into())
    );
    clean(&mut p);

    // Deprecate twice: the second has nothing left to do.
    let mut p = pair();
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    ta.deprecate_attribute("tag").unwrap();
    tb.deprecate_attribute("tag").unwrap();
    ta.commit().unwrap();
    let before = bytes(&p.path);
    let rb = tb.commit().unwrap();
    assert!(!rb.committed);
    assert_eq!(rb.declared, 0);
    assert_eq!(bytes(&p.path), before);
    clean(&mut p);
}

#[test]
fn a_migration_conflicts_with_any_commit_it_did_not_see() {
    let options = || MigrationOptions {
        value_type: Some(ValueType::Str),
        ..MigrationOptions::default()
    };
    // Optimistic: a write to the attribute meanwhile would be missing from
    // the copy, so the migration is refused and run again.
    let mut p = pair();
    let c = p.c;
    p.a.assert(c.greg, "age", Value::I64(40), Some(Tick(1)))
        .unwrap();
    let mut tb = p.b.begin();
    tb.migrate_attribute("age", options()).unwrap();
    p.a.assert(c.edwin, "age", Value::I64(30), Some(Tick(1)))
        .unwrap();
    let before = bytes(&p.path);
    assert_eq!(conflict(tb.commit()).key, ConflictKey::All);
    assert_eq!(bytes(&p.path), before);
    let mut tb = p.b.begin();
    let report = tb.migrate_attribute("age", options()).unwrap();
    assert_eq!(report.change_points, 2);
    tb.commit().unwrap();
    assert_eq!(p.b.as_of(c.edwin, "age", Tick(1)).unwrap(), vec![s("30")]);
    clean(&mut p);

    // Weak: the migration is made again on the caught-up world, so the
    // newer fact is copied too.
    let mut p = pair();
    let c = p.c;
    p.a.assert(c.greg, "age", Value::I64(40), Some(Tick(1)))
        .unwrap();
    let mut tb = p.b.begin();
    tb.set_concurrency(Concurrency::Weak);
    tb.migrate_attribute("age", options()).unwrap();
    p.a.assert(c.edwin, "age", Value::I64(30), Some(Tick(1)))
        .unwrap();
    let rb = tb.commit().unwrap();
    assert_eq!(rb.changed, 2);
    assert_eq!(p.b.as_of(c.edwin, "age", Tick(1)).unwrap(), vec![s("30")]);
    clean(&mut p);

    // A write racing a committed migration: it lands on the replacement
    // attribute, or fails as one writer's would (wrong type now).
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    tb.assert(c.greg, "age", Value::I64(41), Some(Tick(2)))
        .unwrap();
    let mut ta = p.a.begin();
    ta.migrate_attribute("age", options()).unwrap();
    ta.commit().unwrap();
    assert!(matches!(tb.commit(), Err(Error::WrongValueType { .. })));
    clean(&mut p);
}

#[test]
fn an_undeclared_name_a_transaction_relied_on_conflicts_once_declared() {
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    // A retract of an unknown attribute is dropped: "0 changed".
    assert!(
        !tb.infer_write_attribute("mood", &s("grim"), Op::Retract)
            .unwrap()
    );
    tb.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap();
    let mut ta = p.a.begin();
    ta.infer_attribute("mood", &s("grim")).unwrap();
    ta.assert(c.greg, "mood", s("grim"), Some(Tick(0))).unwrap();
    ta.commit().unwrap();
    assert_eq!(
        conflict(tb.commit()).key,
        ConflictKey::Undeclared {
            name: "mood".into()
        }
    );
    clean(&mut p);
}

// ------------------------------------------------------------ odds and ends

#[test]
fn a_transaction_that_staged_nothing_takes_no_lock_and_reports_nothing() {
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    // Already holds at the snapshot: "0 changed".
    let staged = tb
        .assert(c.greg, "located_in", Value::Ref(c.varrow), Some(Tick(700)))
        .unwrap();
    assert_eq!(staged.changed, 0);
    p.a.assert(c.greg, "tag", s("x"), Some(Tick(1))).unwrap();
    let held = std::fs::File::open(&p.path).unwrap();
    held.try_lock().unwrap();
    let rb = tb.commit().unwrap();
    assert!(!rb.committed);
    assert_eq!(rb.finalized, None);
}

#[test]
fn the_lock_is_free_after_every_refused_commit() {
    let mut p = pair();
    let c = p.c;
    p.b.set_busy_timeout(std::time::Duration::ZERO);
    p.a.set_busy_timeout(std::time::Duration::ZERO);
    // A conflict, then a store-rule error, then an exposed ID: after each,
    // the other handle commits at once.
    let mut tb = p.b.begin();
    tb.note_read_all();
    tb.assert(c.greg, "tag", s("b"), Some(Tick(1))).unwrap();
    p.a.assert(c.greg, "tag", s("a1"), Some(Tick(1))).unwrap();
    conflict(tb.commit());
    p.a.assert(c.greg, "tag", s("a2"), Some(Tick(1))).unwrap();

    let mut tb = p.b.begin();
    tb.create_entity("PLACE", Some("dup")).unwrap();
    p.a.create_entity("PLACE", Some("dup")).unwrap();
    tb.commit().unwrap_err();
    p.a.assert(c.greg, "tag", s("a3"), Some(Tick(1))).unwrap();

    let mut tb = p.b.begin();
    tb.set_ids_exposed(true);
    tb.create_entity("PLACE", None).unwrap();
    p.a.create_entity("PLACE", None).unwrap();
    conflict(tb.commit());
    p.a.assert(c.greg, "tag", s("a4"), Some(Tick(1))).unwrap();
    clean(&mut p);
}

#[test]
fn reads_of_a_provisional_edge_conflict_if_the_edge_was_interned_meanwhile() {
    let mut p = pair();
    let c = p.c;
    let mut tb = p.b.begin();
    tb.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(1)))
        .unwrap();
    let edge = tb.edge_id(c.greg, "present", c.varrow).unwrap();
    // It reads the new edge's properties (none) and decides from that.
    let meter = WorkMeter::bounded(1_000, None);
    let held = tb
        .values_at(edge.into(), "disguised", Tick(1), 4, 1 << 10, &meter)
        .unwrap();
    assert!(held.values.is_empty());
    tb.assert(edge.into(), "disguised", Value::Bool(true), Some(Tick(1)))
        .unwrap();
    let mut ta = p.a.begin();
    ta.assert(c.greg, "present", Value::Ref(c.varrow), Some(Tick(1)))
        .unwrap();
    let theirs = ta.edge_id(c.greg, "present", c.varrow).unwrap();
    ta.assert(
        theirs.into(),
        "disguised",
        Value::Bool(false),
        Some(Tick(1)),
    )
    .unwrap();
    ta.commit().unwrap();
    let got = conflict(tb.commit());
    assert_eq!(got.key.kind(), "entity-attribute");
    clean(&mut p);
}

#[test]
fn reads_of_an_entity_only_this_transaction_knows_never_conflict() {
    let mut p = pair();
    let mut ta = p.a.begin();
    let mut tb = p.b.begin();
    // Both get the same provisional number; a writes on its own.
    let theirs = ta.create_entity("CHARACTER", None).unwrap();
    ta.assert(theirs, "tag", s("a's"), Some(Tick(1))).unwrap();
    ta.assert(p.c.greg, "ally_of", Value::Ref(theirs), Some(Tick(1)))
        .unwrap();
    let mine = tb.create_entity("CHARACTER", None).unwrap();
    assert_eq!(mine, theirs);
    tb.note_read(mine, "tag").unwrap();
    tb.note_reverse_read(mine, "ally_of").unwrap();
    tb.assert(mine, "tag", s("b's"), Some(Tick(1))).unwrap();
    ta.commit().unwrap();
    let rb = tb.commit().unwrap();
    assert_eq!(rb.ids.entities.len(), 1);
    clean(&mut p);
}
