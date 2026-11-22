//! Slice 3, item 9: doctor, the schema-rules layer (docs/slice-3-decisions.md,
//! "Item 9"; CLAUDE.md "Doctor", "Edges", "Attribute schema"). RED until the
//! implementation lands: this file fails to COMPILE because the item-9
//! `Finding` variants don't exist.
//!
//! API expected, added to `Finding` (all `Severity::Warning`):
//!
//! ```text
//! Finding::CardinalityOverlap(Overlap)
//! Finding::EdgePropertyOnUnassertedEdge { entry: EntryId, edge: EdgeId, attr: String, tick: Tick }
//! Finding::WrongValueType { entry: EntryId, attr: String, expected: ValueType, got: ValueType }
//! Finding::RetractedEvent { entry: EntryId, attr: String }
//! ```
//!
//! Semantics the tests rely on (flagged in the report where the contract is
//! silent):
//! - `CardinalityOverlap` findings are exactly `World::overlaps(attr)` of the
//!   canon doctor reads at (`opts.canon`, default latest), for every
//!   cardinality-one / reverse-one attribute.
//! - `EdgePropertyOnUnassertedEdge`: `entry` is the property ASSERT entry
//!   (the current one at the canon), `edge` the edge it sits on, `tick` the
//!   property assert's tick. The edge's triple is judged at that tick at that
//!   canon. Property retracts are never flagged. An edge entity's property
//!   whose assert is deleted by a correction is not current, so not flagged.
//! - `WrongValueType` / `RetractedEvent` come only from crafted logs
//!   (`doctor_log`); the bad entry is skipped and the replay goes on.
//! - `since: Some(N)`: schema findings only for entities touched by txns > N
//!   (for a reverse overlap, the target of a ref fact counts as touched).
mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

const K_FACT: u8 = 4;
const K_DELETED: u8 = 7;
const K_REVISED: u8 = 8;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

fn one(w: WriteResult) -> EntryId {
    assert_eq!(w.entries.len(), 1, "{w:?}");
    w.entries[0]
}

fn opts() -> DoctorOptions {
    DoctorOptions::default()
}

fn canon(n: TxId) -> DoctorOptions {
    DoctorOptions {
        canon: Some(n),
        ..DoctorOptions::default()
    }
}

fn since(n: TxId) -> DoctorOptions {
    DoctorOptions {
        since: Some(n),
        ..DoctorOptions::default()
    }
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ruled_by", ValueType::Ref, Kind::State, Cardinality::One).reverse_one(),
        )
        .attribute(
            AttrDef::new("married_to", ValueType::Ref, Kind::State, Cardinality::One)
                .bidirectional(),
        )
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(AttrDef::new(
            "present",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "rank",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "omen",
            ValueType::Str,
            Kind::Event,
            Cardinality::Many,
        ))
}

// ---------------------------------------------------------------- report helpers

fn overlaps_in(rep: &Report) -> Vec<Overlap> {
    rep.findings
        .iter()
        .filter_map(|f| match f {
            Finding::CardinalityOverlap(o) => Some(o.clone()),
            _ => None,
        })
        .collect()
}

fn edge_props_in(rep: &Report) -> Vec<(EntryId, EdgeId, String, Tick)> {
    rep.findings
        .iter()
        .filter_map(|f| match f {
            Finding::EdgePropertyOnUnassertedEdge {
                entry,
                edge,
                attr,
                tick,
            } => Some((*entry, *edge, attr.clone(), *tick)),
            _ => None,
        })
        .collect()
}

/// only warnings: no error, every finding Severity::Warning, warnings() lists them all
fn assert_only_warnings(rep: &Report) {
    assert!(rep.errors().is_empty(), "{:#?}", rep.findings);
    assert_eq!(
        rep.warnings().len(),
        rep.findings.len(),
        "{:#?}",
        rep.findings
    );
    for f in &rep.findings {
        assert_eq!(f.severity(), Severity::Warning, "{f:?}");
    }
}

fn assert_clean(rep: &Report, label: &str) {
    assert!(
        rep.is_clean(),
        "{label}: expected clean, got {:#?}",
        rep.findings
    );
}

// ================================================================ cardinality overlaps

/// Greg is in Varrow from 200 (tx#4), in Ashford from 601 (tx#5), and
/// leaves Varrow at 250 (tx#6): CLAUDE.md "Attribute schema", tx#50/55/57.
struct Greg {
    w: World,
    greg: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

fn greg_world(w: World) -> Greg {
    let mut w = w;
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    assert_eq!(w.latest_tx(), TxId(3));
    Greg {
        w,
        greg,
        varrow,
        ashford,
    }
}

fn greg_overlap(g: &Greg) -> Overlap {
    Overlap {
        entity: g.greg,
        attr: "located_in".into(),
        side: Side::Forward,
        a: r(g.varrow),
        b: r(g.ashford),
        during: Interval {
            from: Tick(601),
            until: None,
        },
    }
}

#[test]
fn greg_in_two_places_appears_disappears_and_reappears_at_an_older_canon() {
    for file in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let w = if file {
            World::create(&dir.path().join("w.kala"), schema()).unwrap()
        } else {
            World::in_memory(schema()).unwrap()
        };
        let mut g = greg_world(w);
        assert_clean(&doctor(&g.w, &opts()), "empty");
        g.w.assert(g.greg, "located_in", r(g.varrow), t(200))
            .unwrap(); // tx 4
        assert_clean(&doctor(&g.w, &opts()), "one place");
        g.w.assert(g.greg, "located_in", r(g.ashford), t(601))
            .unwrap(); // tx 5
        assert_eq!(g.w.latest_tx(), TxId(5));

        let rep = doctor(&g.w, &opts());
        assert_eq!(overlaps_in(&rep), vec![greg_overlap(&g)], "file={file}");
        assert_eq!(rep.findings.len(), 1);
        assert_only_warnings(&rep);

        g.w.retract(g.greg, "located_in", r(g.varrow), t(250))
            .unwrap(); // tx 6
        assert_eq!(g.w.latest_tx(), TxId(6));
        assert_clean(&doctor(&g.w, &opts()), "after the retract, latest canon");
        assert_clean(&doctor(&g.w, &canon(TxId(6))), "canon 6");

        // an older canon sees the draft state again
        let rep = doctor(&g.w, &canon(TxId(5)));
        assert_eq!(overlaps_in(&rep), vec![greg_overlap(&g)], "canon 5");
        assert_only_warnings(&rep);
        assert_clean(&doctor(&g.w, &canon(TxId(4))), "canon 4: one place only");
    }
}

#[test]
fn touching_intervals_and_gaps_are_not_overlaps() {
    let mut g = greg_world(World::in_memory(schema()).unwrap());
    let (greg, varrow, ashford) = (g.greg, g.varrow, g.ashford);
    g.w.assert(greg, "located_in", r(varrow), t(-300)).unwrap();
    g.w.retract(greg, "located_in", r(varrow), t(-250)).unwrap();
    g.w.assert(greg, "located_in", r(ashford), t(-250)).unwrap(); // touching
    g.w.retract(greg, "located_in", r(ashford), t(-200))
        .unwrap();
    g.w.assert(greg, "located_in", r(varrow), t(100)).unwrap(); // after a gap
    assert_clean(&doctor(&g.w, &opts()), "touching + gap");
}

#[test]
fn a_forward_overlap_on_a_reverse_labelled_attribute_is_reported_on_the_source() {
    // ruled_by is cardinality one forward too: Carnath with two rulers
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    w.assert(carnath, "ruled_by", r(edwin), t(10)).unwrap();
    assert_clean(&doctor(&w, &opts()), "one ruler");
    w.assert(carnath, "ruled_by", r(mara), t(20)).unwrap();
    let rep = doctor(&w, &opts());
    assert_eq!(
        overlaps_in(&rep),
        vec![Overlap {
            entity: carnath,
            attr: "ruled_by".into(),
            side: Side::Forward,
            a: r(edwin),
            b: r(mara),
            during: Interval {
                from: Tick(20),
                until: None
            },
        }]
    );
    assert_eq!(rep.findings.len(), 1, "{:#?}", rep.findings);
    assert_only_warnings(&rep);
}

#[test]
fn a_reverse_one_overlap_has_the_item_6_shape() {
    // a ruler (target) ruling two cities (sources) at once
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(carnath, "ruled_by", r(edwin), t(10)).unwrap();
    w.assert(varrow, "ruled_by", r(edwin), t(20)).unwrap();
    let rep = doctor(&w, &opts());
    assert_eq!(
        overlaps_in(&rep),
        vec![Overlap {
            entity: edwin,
            attr: "ruled_by".into(),
            side: Side::Reverse,
            a: r(carnath),
            b: r(varrow),
            during: Interval {
                from: Tick(20),
                until: None
            },
        }]
    );
    assert_eq!(rep.findings.len(), 1, "{:#?}", rep.findings);
    assert_only_warnings(&rep);
    // ended again: clean
    w.retract(carnath, "ruled_by", r(edwin), t(20)).unwrap();
    assert_clean(&doctor(&w, &opts()), "resolved");
}

#[test]
fn a_bidirectional_one_overlap_is_reported_for_the_entity_with_two_partners() {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    w.assert(edwin, "married_to", r(mara), t(10)).unwrap();
    assert_clean(&doctor(&w, &opts()), "one marriage");
    w.assert(lisa, "married_to", r(edwin), t(20)).unwrap(); // written from the other side
    let rep = doctor(&w, &opts());
    assert_eq!(
        overlaps_in(&rep),
        vec![Overlap {
            entity: edwin,
            attr: "married_to".into(),
            side: Side::Forward,
            a: r(mara),
            b: r(lisa),
            during: Interval {
                from: Tick(20),
                until: None
            },
        }]
    );
    assert_eq!(rep.findings.len(), 1, "{:#?}", rep.findings);
    assert_only_warnings(&rep);
    // and it is the same set World::overlaps gives
    assert_eq!(overlaps_in(&rep), w.overlaps("married_to").unwrap());
    // the partner's side overlaps too if she has two: Mara marries Lisa at 30
    w.assert(mara, "married_to", r(lisa), t(30)).unwrap();
    let rep = doctor(&w, &opts());
    assert_eq!(overlaps_in(&rep), w.overlaps("married_to").unwrap());
    assert!(overlaps_in(&rep).len() >= 2, "{:#?}", rep.findings);
}

#[test]
fn many_cardinality_and_event_attributes_never_overlap() {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    w.assert(edwin, "ally_of", r(mara), t(1)).unwrap();
    w.assert(edwin, "ally_of", r(lisa), t(1)).unwrap();
    w.assert(edwin, "present", r(carnath), t(1)).unwrap();
    w.assert(edwin, "present", r(mara), t(1)).unwrap();
    w.assert(edwin, "omen", s("storm"), t(1)).unwrap();
    w.assert(edwin, "omen", s("comet"), t(1)).unwrap();
    assert_clean(&doctor(&w, &opts()), "many + event");
}

#[test]
fn doctor_reports_every_overlap_the_world_has() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let p1 = w.create_entity("PLACE", Some("p1")).unwrap();
    let p2 = w.create_entity("PLACE", Some("p2")).unwrap();
    let p3 = w.create_entity("PLACE", Some("p3")).unwrap();
    for who in [a, b] {
        w.assert(who, "located_in", r(p1), t(0)).unwrap();
        w.assert(who, "located_in", r(p2), t(5)).unwrap();
    }
    w.assert(a, "located_in", r(p3), t(-5)).unwrap(); // a third place, from -5
    let rep = doctor(&w, &opts());
    let mut got = overlaps_in(&rep);
    let mut want = w.overlaps("located_in").unwrap();
    assert!(want.len() >= 3, "{want:#?}");
    got.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    want.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    assert_eq!(got, want);
    assert_eq!(rep.findings.len(), want.len());
    assert_only_warnings(&rep);
}

// ================================================================ edge properties

struct EdgeWorld {
    w: World,
    edwin: EntityId,
    carnath: EntityId,
    edge: EdgeId,
    /// `Edwin present Carnath @-300`
    assert_entry: EntryId,
    /// `retract @-250`
    retract_entry: EntryId,
}

fn edge_world() -> EdgeWorld {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    let assert_entry = one(w.assert(edwin, "present", r(carnath), t(-300)).unwrap());
    let retract_entry = one(w.retract(edwin, "present", r(carnath), t(-250)).unwrap());
    let edge = w.edge_id(edwin, "present", carnath).unwrap();
    EdgeWorld {
        w,
        edwin,
        carnath,
        edge,
        assert_entry,
        retract_entry,
    }
}

impl EdgeWorld {
    fn prop(&mut self, v: i64, tick: i64) -> EntryId {
        one(self
            .w
            .assert(EntityId::from(self.edge), "rank", Value::I64(v), t(tick))
            .unwrap())
    }

    fn unprop(&mut self, v: i64, tick: i64) -> EntryId {
        one(self
            .w
            .retract(EntityId::from(self.edge), "rank", Value::I64(v), t(tick))
            .unwrap())
    }

    fn flagged(&self, o: &DoctorOptions) -> Vec<(EntryId, EdgeId, String, Tick)> {
        let rep = doctor(&self.w, o);
        assert_only_warnings(&rep);
        assert!(
            rep.findings
                .iter()
                .all(|f| matches!(f, Finding::EdgePropertyOnUnassertedEdge { .. })),
            "{:#?}",
            rep.findings
        );
        edge_props_in(&rep)
    }
}

fn flag(entry: EntryId, edge: EdgeId, tick: i64) -> (EntryId, EdgeId, String, Tick) {
    (entry, edge, "rank".to_owned(), Tick(tick))
}

#[test]
fn a_property_inside_the_edges_life_is_clean() {
    let mut e = edge_world();
    e.prop(1, -280);
    e.prop(2, -260);
    assert_clean(&doctor(&e.w, &opts()), "inside");
    // exactly at the edge's assert tick: the edge holds there
    let mut e = edge_world();
    e.prop(1, -300);
    assert_clean(&doctor(&e.w, &opts()), "at the assert tick");
}

#[test]
fn a_property_asserted_before_the_edge_is_flagged_with_all_its_fields() {
    let mut e = edge_world();
    let p = e.prop(1, -310);
    assert_eq!(e.flagged(&opts()), vec![flag(p, e.edge, -310)]);
}

#[test]
fn a_property_asserted_after_the_edge_ended_is_flagged_including_at_the_retract_tick() {
    let mut e = edge_world();
    let late = e.prop(1, -200);
    let at_end = e.prop(2, -250); // the edge's retract tick: it no longer holds
    let got = e.flagged(&opts());
    assert_eq!(got.len(), 2, "{got:#?}");
    assert!(got.contains(&flag(late, e.edge, -200)));
    assert!(got.contains(&flag(at_end, e.edge, -250)));
}

#[test]
fn a_property_retract_is_never_flagged_even_after_the_edge_ended() {
    let mut e = edge_world();
    e.prop(1, -280);
    e.unprop(1, -250); // at the edge's retract tick: normal cleanup
    assert_clean(&doctor(&e.w, &opts()), "retract at the edge's end");
    let mut e = edge_world();
    e.prop(1, -280);
    e.unprop(1, -200); // well after the edge ended
    assert_clean(&doctor(&e.w, &opts()), "retract after the edge's end");
}

#[test]
fn a_property_in_the_gap_of_a_twice_asserted_edge_is_flagged() {
    let mut e = edge_world();
    let (edwin, carnath) = (e.edwin, e.carnath);
    e.w.assert(edwin, "present", r(carnath), t(-200)).unwrap();
    e.w.retract(edwin, "present", r(carnath), t(-100)).unwrap();
    let gap = e.prop(1, -220);
    e.prop(2, -150); // inside the second life
    let after = e.prop(3, -90);
    let got = e.flagged(&opts());
    assert_eq!(got.len(), 2, "{got:#?}");
    assert!(got.contains(&flag(gap, e.edge, -220)));
    assert!(got.contains(&flag(after, e.edge, -90)));
}

#[test]
fn a_deleted_property_assert_is_no_longer_current_and_not_flagged() {
    let mut e = edge_world();
    let p = e.prop(1, -310);
    assert_eq!(e.flagged(&opts()).len(), 1);
    e.w.correct(p, Correction::Delete).unwrap();
    assert_clean(&doctor(&e.w, &opts()), "property deleted");
}

#[test]
fn a_property_on_a_non_edge_entity_is_not_an_edge_property_finding() {
    let mut e = edge_world();
    let edwin = e.edwin;
    e.w.assert(edwin, "rank", Value::I64(1), t(-999)).unwrap();
    assert_clean(&doctor(&e.w, &opts()), "ordinary entity");
}

#[test]
fn a_bidirectional_edge_is_judged_by_its_normalised_triple() {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    // written from Mara's side; the stored pair is (min, max)
    w.assert(mara, "ally_of", r(edwin), t(-300)).unwrap();
    w.retract(edwin, "ally_of", r(mara), t(-250)).unwrap();
    let edge = w.edge_id(edwin, "ally_of", mara).unwrap();
    assert_eq!(Some(edge), w.edge_id(mara, "ally_of", edwin));
    let id = EntityId::from(edge);
    w.assert(id, "rank", Value::I64(1), t(-280)).unwrap(); // inside
    assert_clean(&doctor(&w, &opts()), "inside");
    let early = one(w.assert(id, "rank", Value::I64(2), t(-310)).unwrap());
    let late = one(w.assert(id, "rank", Value::I64(3), t(-200)).unwrap());
    let rep = doctor(&w, &opts());
    assert_only_warnings(&rep);
    let got = edge_props_in(&rep);
    assert_eq!(got.len(), 2, "{:#?}", rep.findings);
    assert!(got.contains(&flag(early, edge, -310)));
    assert!(got.contains(&flag(late, edge, -200)));
}

#[test]
fn a_correction_that_moves_the_edges_interval_makes_a_finding_appear_and_disappear() {
    let mut e = edge_world();
    let p = e.prop(1, -310);
    assert_eq!(e.flagged(&opts()), vec![flag(p, e.edge, -310)]);
    // the edge really began at -320: the property is inside it now
    e.w.correct(
        e.assert_entry,
        Correction::Replace {
            value: None,
            tick: t(-320),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    assert_clean(&doctor(&e.w, &opts()), "edge moved earlier");
    // and moved later again: flagged again
    e.w.correct(
        e.assert_entry,
        Correction::Replace {
            value: None,
            tick: t(-290),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    assert_eq!(e.flagged(&opts()), vec![flag(p, e.edge, -310)]);
}

#[test]
fn deleting_the_edges_assert_flags_every_property_assert() {
    let mut e = edge_world();
    let p1 = e.prop(1, -280);
    let p2 = e.prop(2, -270);
    assert_clean(&doctor(&e.w, &opts()), "control");
    e.w.correct(e.assert_entry, Correction::Delete).unwrap();
    let got = e.flagged(&opts());
    assert_eq!(got.len(), 2, "{got:#?}");
    assert!(got.contains(&flag(p1, e.edge, -280)));
    assert!(got.contains(&flag(p2, e.edge, -270)));
}

#[test]
fn an_older_canon_uses_the_edges_interval_at_that_canon() {
    let mut e = edge_world();
    // the edge ended at -250; a property was written for -220 (flagged)
    let before_prop = e.w.latest_tx();
    let p = e.prop(1, -220);
    let at_prop = e.w.latest_tx();
    assert_eq!(e.flagged(&opts()), vec![flag(p, e.edge, -220)]);
    // the retcon: the edge actually ended at -200
    e.w.correct(
        e.retract_entry,
        Correction::Replace {
            value: None,
            tick: t(-200),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let at_fix = e.w.latest_tx();
    assert_clean(&doctor(&e.w, &opts()), "latest canon: edge holds at -220");
    assert_clean(&doctor(&e.w, &canon(at_fix)), "canon at the fix");
    // canon before the fix: the edge's interval then ended at -250
    assert_eq!(e.flagged(&canon(at_prop)), vec![flag(p, e.edge, -220)]);
    // canon before the property was written: it isn't current yet
    assert_clean(&doctor(&e.w, &canon(before_prop)), "before the property");
}

#[test]
fn an_older_canon_sees_a_property_that_a_later_edge_correction_made_fine() {
    // fine then, flagged now
    let mut e = edge_world();
    let q = e.prop(1, -280); // inside
    let at_q = e.w.latest_tx();
    e.w.correct(
        e.assert_entry,
        Correction::Replace {
            value: None,
            tick: t(-270),
            op: None,
            precision: None,
        },
    )
    .unwrap(); // the edge began at -270: -280 is before it
    assert_eq!(e.flagged(&opts()), vec![flag(q, e.edge, -280)]);
    assert_clean(&doctor(&e.w, &canon(at_q)), "canon before the correction");
}

// ================================================================ crafted logs

struct Crafted {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    greg: u64,
    attr_name: u32,
    attr_omen: u32,
    next_entry: u64,
    next_tx: u64,
}

/// genesis + greg + varrow + `Greg name "Greg" @1` (tx 3)
fn crafted() -> Crafted {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let greg;
    {
        let mut w = World::create(&path, schema()).unwrap();
        let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        w.create_entity("PLACE", Some("varrow")).unwrap();
        w.assert(g, "name", s("Greg"), t(1)).unwrap();
        assert_eq!(w.latest_tx(), TxId(3));
        greg = u64::from_be_bytes(g.to_bytes());
    }
    let log = read_log(&path).unwrap();
    let attr = |n: &str| {
        log.iter()
            .find_map(|e| match &e.body {
                EntryBody::DefineAttr { id, def } if def.name == n => Some(id.0),
                _ => None,
            })
            .unwrap()
    };
    Crafted {
        bytes: std::fs::read(&path).unwrap(),
        attr_name: attr("name"),
        attr_omen: attr("omen"),
        next_entry: log.last().unwrap().id.0 + 1,
        next_tx: 4,
        greg,
        path,
        _dir: dir,
    }
}

fn fact(entity: u64, attr: u32, tick: i64, retract: bool, v: &Value) -> (u8, Vec<u8>) {
    let mut b = entity.to_le_bytes().to_vec();
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(u8::from(retract));
    b.push(0); // precision byte (v4): Exact
    b.extend_from_slice(&v.encode().unwrap());
    (K_FACT, b)
}

impl Crafted {
    /// appends one good txn of `entries`; returns the first entry id used
    fn push(&mut self, entries: &[(u8, Vec<u8>)]) -> u64 {
        let first = self.next_entry;
        let mut body = Vec::new();
        let mut id = first;
        for (k, p) in entries {
            body.extend(frame(*k, id, p));
            id += 1;
        }
        let c = commit_frame(&body, id, self.next_tx);
        body.extend(c);
        self.bytes.extend(txn(self.next_tx, &body));
        self.next_entry = id + 1;
        self.next_tx += 1;
        first
    }

    fn report(&self, o: &DoctorOptions) -> Report {
        std::fs::write(&self.path, &self.bytes).unwrap();
        doctor_log(&self.path, o).unwrap()
    }
}

#[test]
fn a_wrong_typed_value_in_a_crafted_log_is_a_warning_and_the_entry_is_skipped() {
    let mut c = crafted();
    // tx 4: a number for a string attribute, then a good fact
    let bad = fact(c.greg, c.attr_name, 5, false, &Value::I64(5));
    let good = fact(c.greg, c.attr_name, 6, false, &s("Gregor"));
    // "Greg" ends where "Gregor" starts: no cardinality overlap on `name`
    let end_greg = fact(c.greg, c.attr_name, 6, true, &s("Greg"));
    let first = c.push(&[bad, good, end_greg]);
    // tx 5 depends on the good fact having been replayed
    c.push(&[fact(c.greg, c.attr_name, 9, true, &s("Gregor"))]);
    let rep = c.report(&opts());
    assert_eq!(
        rep.findings,
        vec![Finding::WrongValueType {
            entry: EntryId(first),
            attr: "name".into(),
            expected: ValueType::Str,
            got: ValueType::I64,
        }],
        "the later good entries are replayed; nothing else is wrong"
    );
    assert_eq!(rep.findings[0].severity(), Severity::Warning);
    assert!(rep.errors().is_empty());
    assert_eq!(rep.warnings().len(), 1);
    assert!(
        !rep.findings
            .iter()
            .any(|f| matches!(f, Finding::InvalidEntry { .. }))
    );
    // World::open refuses this very file
    assert!(World::open(&c.path).is_err());
}

#[test]
fn a_retracted_event_in_a_crafted_log_is_a_warning_and_the_entry_is_skipped() {
    let mut c = crafted();
    let first = c.push(&[
        fact(c.greg, c.attr_omen, 1, false, &s("storm")),
        fact(c.greg, c.attr_omen, 5, true, &s("storm")),
    ]);
    // later good entries are still replayed
    c.push(&[
        fact(c.greg, c.attr_name, 6, false, &s("Gregor")),
        fact(c.greg, c.attr_name, 6, true, &s("Greg")),
    ]);
    c.push(&[fact(c.greg, c.attr_name, 9, true, &s("Gregor"))]);
    let rep = c.report(&opts());
    assert_eq!(
        rep.findings,
        vec![Finding::RetractedEvent {
            entry: EntryId(first + 1),
            attr: "omen".into(),
        }]
    );
    assert_eq!(rep.findings[0].severity(), Severity::Warning);
    assert!(rep.errors().is_empty());
    assert!(World::open(&c.path).is_err());
}

#[test]
fn a_crafted_log_with_both_kinds_reports_both() {
    let mut c = crafted();
    let a = c.push(&[fact(c.greg, c.attr_name, 5, false, &Value::I64(1))]);
    let b = c.push(&[
        fact(c.greg, c.attr_omen, 1, false, &s("storm")),
        fact(c.greg, c.attr_omen, 5, true, &s("storm")),
    ]);
    let rep = c.report(&opts());
    assert_only_warnings(&rep);
    assert_eq!(rep.findings.len(), 2, "{:#?}", rep.findings);
    assert!(rep.findings.iter().any(|f| matches!(f,
        Finding::WrongValueType { entry, got, .. } if *entry == EntryId(a) && *got == ValueType::I64)));
    assert!(rep.findings.iter().any(|f| matches!(f,
        Finding::RetractedEvent { entry, .. } if *entry == EntryId(b + 1))));
}

#[test]
fn schema_warnings_follow_since_by_the_tx_of_the_bad_entry() {
    let mut c = crafted();
    c.push(&[fact(c.greg, c.attr_name, 5, false, &Value::I64(5))]); // tx 4
    assert_eq!(c.report(&since(TxId(3))).warnings().len(), 1);
    assert!(c.report(&since(TxId(4))).is_clean(), "tx 4 is not > 4");
    assert_eq!(c.report(&opts()).warnings().len(), 1);
}

// ================================================================ since

#[test]
fn since_limits_overlap_findings_to_entities_touched_by_later_txns() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    // Greg's old overlap, created by tx `old`
    w.assert(greg, "located_in", r(varrow), t(200)).unwrap();
    w.assert(greg, "located_in", r(ashford), t(601)).unwrap();
    let old = w.latest_tx();
    // an unrelated write
    w.assert(mara, "name", s("Mara"), t(1)).unwrap();
    let unrelated = w.latest_tx();
    // a new overlap, on Lisa
    w.assert(lisa, "located_in", r(varrow), t(1)).unwrap();
    w.assert(lisa, "located_in", r(ashford), t(2)).unwrap();
    let new = w.latest_tx();

    let who = |rep: &Report| -> Vec<EntityId> {
        let mut v: Vec<_> = overlaps_in(rep).into_iter().map(|o| o.entity).collect();
        v.sort_by_key(|e| e.to_bytes());
        v
    };
    // without since: both
    assert_eq!(who(&doctor(&w, &opts())), vec![greg, lisa]);
    // since before Greg's overlap was written: both
    assert_eq!(who(&doctor(&w, &since(TxId(old.0 - 2)))), vec![greg, lisa]);
    // since the tx that created Greg's overlap: only the new one
    assert_eq!(who(&doctor(&w, &since(old))), vec![lisa]);
    assert_eq!(who(&doctor(&w, &since(unrelated))), vec![lisa]);
    // since the tx before Lisa's second write: hers
    assert_eq!(who(&doctor(&w, &since(TxId(new.0 - 1)))), vec![lisa]);
    // since the latest tx: nothing
    assert_clean(&doctor(&w, &since(new)), "since latest");
    // the old one is still there for a full check
    assert!(who(&doctor(&w, &opts())).contains(&greg));
}

#[test]
fn since_reports_a_reverse_overlap_when_a_later_txn_touched_its_target() {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.assert(carnath, "ruled_by", r(edwin), t(10)).unwrap();
    let first = w.latest_tx();
    w.assert(varrow, "ruled_by", r(edwin), t(20)).unwrap();
    let second = w.latest_tx();
    let rep = doctor(&w, &since(first));
    assert_eq!(overlaps_in(&rep).len(), 1, "{:#?}", rep.findings);
    assert_clean(&doctor(&w, &since(second)), "nothing newer touched Edwin");
}

#[test]
fn since_limits_edge_property_findings_to_properties_written_by_later_txns() {
    let mut e = edge_world();
    let p = e.prop(1, -310);
    let at_p = e.w.latest_tx();
    // an unrelated later txn
    let edwin = e.edwin;
    e.w.assert(edwin, "name", s("Edwin"), t(1)).unwrap();
    let got = |o: &DoctorOptions| edge_props_in(&doctor(&e.w, o));
    assert_eq!(got(&opts()), vec![flag(p, e.edge, -310)]);
    assert_eq!(got(&since(TxId(at_p.0 - 1))), vec![flag(p, e.edge, -310)]);
    assert!(got(&since(at_p)).is_empty(), "p's tx is not > since");
}

// ================================================================ read-only, no suggestions

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

#[test]
fn doctor_changes_nothing_in_the_world_or_on_disk_even_with_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut g = greg_world(World::create(&path, schema()).unwrap());
    g.w.assert(g.greg, "located_in", r(g.varrow), t(200))
        .unwrap();
    g.w.assert(g.greg, "located_in", r(g.ashford), t(601))
        .unwrap();
    g.w.assert(g.greg, "present", r(g.varrow), t(0)).unwrap();
    g.w.retract(g.greg, "present", r(g.varrow), t(5)).unwrap();
    let edge = g.w.edge_id(g.greg, "present", g.varrow).unwrap();
    g.w.assert(EntityId::from(edge), "rank", Value::I64(1), t(50))
        .unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let files = all_files(dir.path());
    let dump = g.w.index_dump();
    let latest = g.w.latest_tx();
    let hist = g.w.history(g.greg, "located_in").unwrap();

    let rep = doctor(&g.w, &opts());
    assert_eq!(overlaps_in(&rep).len(), 1);
    assert_eq!(edge_props_in(&rep).len(), 1);
    assert_eq!(rep.findings.len(), 2, "{:#?}", rep.findings);
    assert_only_warnings(&rep);
    assert_eq!(doctor(&g.w, &opts()), rep, "deterministic");
    assert_eq!(
        doctor_log(&path, &opts()).unwrap(),
        rep,
        "doctor_log agrees"
    );

    assert_eq!(std::fs::read(&path).unwrap(), bytes, "log bytes");
    assert_eq!(all_files(dir.path()), files);
    assert_eq!(g.w.index_dump(), dump);
    assert_eq!(g.w.latest_tx(), latest);
    assert_eq!(g.w.history(g.greg, "located_in").unwrap(), hist);
    // still writable
    g.w.retract(g.greg, "located_in", r(g.varrow), t(250))
        .unwrap();
    assert!(overlaps_in(&doctor(&g.w, &opts())).is_empty());
}

#[test]
fn a_clean_world_with_schema_rules_in_play_is_clean() {
    let mut e = edge_world();
    e.prop(1, -300);
    e.prop(2, -260);
    let (edwin, carnath) = (e.edwin, e.carnath);
    e.w.assert(edwin, "located_in", r(carnath), t(-300))
        .unwrap();
    assert_clean(&doctor(&e.w, &opts()), "clean");
}

// ================================================================ item 9 confirm additions

#[test]
fn a_property_on_an_edge_whose_source_is_itself_an_edge_is_judged_by_that_edge() {
    let mut e = edge_world();
    // edge#2 = (edge#1 as an entity, present, carnath), asserted @-280 only (inside edge#1's own life [-300, -250))
    let outer_src = EntityId::from(e.edge);
    let carnath = e.carnath;
    e.w.assert(outer_src, "present", r(carnath), t(-280))
        .unwrap();
    let outer = e.w.edge_id(outer_src, "present", carnath).unwrap();
    assert_ne!(outer, e.edge);
    // property on the outer edge, before its life: flagged; inside it: fine
    let before = one(e
        .w
        .assert(EntityId::from(outer), "rank", Value::I64(1), t(-290))
        .unwrap());
    e.w.assert(EntityId::from(outer), "rank", Value::I64(2), t(-270))
        .unwrap();
    let rep = doctor(&e.w, &opts());
    assert_only_warnings(&rep);
    assert_eq!(
        edge_props_in(&rep),
        vec![(before, outer, "rank".to_owned(), Tick(-290))],
        "{:#?}",
        rep.findings
    );
    assert_eq!(rep.findings.len(), 1, "{:#?}", rep.findings);
}

#[test]
fn canon_below_since_reports_no_schema_warnings() {
    let mut e = edge_world();
    let edwin = e.edwin;
    let carnath = e.carnath;
    let before_all = e.w.latest_tx();
    // an overlap and an unasserted-edge property, written after `before_all`
    e.w.assert(edwin, "located_in", r(carnath), t(1)).unwrap();
    let carnath2 = e.w.create_entity("PLACE", Some("p2")).unwrap();
    e.w.assert(edwin, "located_in", r(carnath2), t(2)).unwrap();
    e.prop(1, -310);
    let latest = e.w.latest_tx();
    let full = doctor(&e.w, &opts());
    assert_eq!(full.warnings().len(), 2, "{:#?}", full.findings);
    // canon < since: nothing was written by a txn that is > since and <= canon
    let o = DoctorOptions {
        canon: Some(before_all),
        since: Some(latest),
    };
    assert_clean(&doctor(&e.w, &o), "canon < since");
    let o = DoctorOptions {
        canon: Some(TxId(latest.0 - 1)),
        since: Some(latest),
    };
    assert_clean(&doctor(&e.w, &o), "canon = since - 1");
}

#[test]
fn since_and_canon_together_use_the_txns_between_them() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let lisa = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    w.assert(greg, "located_in", r(varrow), t(200)).unwrap();
    w.assert(greg, "located_in", r(ashford), t(601)).unwrap();
    let greg_tx = w.latest_tx();
    w.assert(lisa, "located_in", r(varrow), t(1)).unwrap();
    let lisa_first = w.latest_tx();
    w.assert(lisa, "located_in", r(ashford), t(2)).unwrap();
    let lisa_tx = w.latest_tx();
    let who = |o: DoctorOptions| -> Vec<EntityId> {
        let rep = doctor(&w, &o);
        assert_only_warnings(&rep);
        overlaps_in(&rep).into_iter().map(|o| o.entity).collect()
    };
    let o = |canon: TxId, since: TxId| DoctorOptions {
        canon: Some(canon),
        since: Some(since),
    };
    assert_eq!(who(o(lisa_tx, greg_tx)), vec![lisa]);
    assert_eq!(who(o(lisa_tx, TxId(0))), vec![greg, lisa]);
    // canon hides Lisa's overlap (its second write is newer than the canon)
    assert!(who(o(lisa_first, greg_tx)).is_empty());
    // since hides Greg's, canon hides Lisa's
    assert_eq!(who(o(greg_tx, TxId(0))), vec![greg]);
    assert!(who(o(greg_tx, greg_tx)).is_empty());
    assert!(who(o(lisa_first, lisa_first)).is_empty());
}

#[test]
fn since_is_judged_per_entity_and_attribute_not_per_entity() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    w.assert(greg, "located_in", r(varrow), t(200)).unwrap();
    w.assert(greg, "located_in", r(ashford), t(601)).unwrap();
    let old = w.latest_tx();
    // a later write of ANOTHER attribute on the same entity
    w.assert(greg, "name", s("Greg"), t(1)).unwrap();
    w.assert(greg, "rank", Value::I64(3), t(1)).unwrap();
    assert_clean(
        &doctor(&w, &since(old)),
        "another attribute of Greg changed: located_in's old overlap is not re-reported",
    );
    assert_eq!(overlaps_in(&doctor(&w, &opts())).len(), 1);
    // touching the same (entity, attr) again does re-report it
    w.retract(greg, "located_in", r(ashford), t(700)).unwrap();
    let rep = doctor(&w, &since(old));
    // the retract at 700 shortens nothing of [601, inf) other than ending Ashford:
    // the overlap is now [601, 700)
    assert_eq!(overlaps_in(&rep).len(), 1, "{:#?}", rep.findings);
}

#[test]
fn a_moved_property_assert_is_flagged_by_its_revised_entry_not_the_base() {
    let mut e = edge_world();
    let base = e.prop(1, -280); // inside the edge's life [-300, -250)
    let at_base = e.w.latest_tx();
    // move it to -310, before the edge was asserted
    let res =
        e.w.correct(
            base,
            Correction::Replace {
                value: None,
                tick: Some(Tick(-310)),
                op: None,
                precision: None,
            },
        )
        .unwrap();
    assert_eq!(res.entries.len(), 2, "{res:?}"); // Deleted, Revised
    let revised = res.entries[1];
    assert_ne!(revised, base);
    assert_eq!(e.flagged(&opts()), vec![flag(revised, e.edge, -310)]);
    // at the canon before the correction the base entry is current and inside the edge's life
    assert_clean(&doctor(&e.w, &canon(at_base)), "older canon");
    // a second correction of the same chain: the finding follows the newest Revised entry
    let res =
        e.w.correct(
            base,
            Correction::Replace {
                value: None,
                tick: Some(Tick(-320)),
                op: None,
                precision: None,
            },
        )
        .unwrap();
    let revised2 = *res.entries.last().unwrap();
    assert_eq!(e.flagged(&opts()), vec![flag(revised2, e.edge, -320)]);
}

#[test]
fn a_wrong_typed_crafted_revised_entry_is_a_wrong_value_type_warning() {
    let mut c = crafted();
    let log = read_log(&c.path).unwrap();
    let name_fact = log
        .iter()
        .rev()
        .find(|e| matches!(e.body, EntryBody::Fact { .. }))
        .unwrap()
        .id
        .0;
    // Deleted at Greg's key, then a Revised at the same tick with a number as the name
    let mut del = name_fact.to_le_bytes().to_vec();
    del.extend_from_slice(&1u32.to_le_bytes());
    del.extend_from_slice(&c.greg.to_le_bytes());
    del.extend_from_slice(&c.attr_name.to_le_bytes());
    del.extend_from_slice(&1i64.to_le_bytes());
    del.extend_from_slice(&s("Greg").encode().unwrap());
    let mut rev = name_fact.to_le_bytes().to_vec();
    rev.extend_from_slice(&1u32.to_le_bytes());
    rev.extend_from_slice(&c.greg.to_le_bytes());
    rev.extend_from_slice(&c.attr_name.to_le_bytes());
    rev.extend_from_slice(&1i64.to_le_bytes());
    rev.push(0); // assert
    rev.push(0); // precision byte (v4): Exact
    rev.extend_from_slice(&Value::I64(7).encode().unwrap());
    let first = c.push(&[(K_DELETED, del), (K_REVISED, rev)]);
    let rep = c.report(&opts());
    assert_eq!(
        rep.findings,
        vec![Finding::WrongValueType {
            entry: EntryId(first + 1),
            attr: "name".into(),
            expected: ValueType::Str,
            got: ValueType::I64,
        }],
        "{:#?}",
        rep.findings
    );
    assert_eq!(rep.findings[0].severity(), Severity::Warning);
    assert!(World::open(&c.path).is_err());
}

#[test]
fn doctor_log_without_a_genesis_runs_no_schema_rules_and_does_not_panic() {
    use common::{HEADER_LEN, spans};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    {
        let mut g = greg_world(World::create(&path, schema()).unwrap());
        g.w.assert(g.greg, "located_in", r(g.varrow), t(200))
            .unwrap();
        g.w.assert(g.greg, "located_in", r(g.ashford), t(601))
            .unwrap();
        assert_eq!(overlaps_in(&doctor(&g.w, &opts())).len(), 1);
    }
    let bytes = std::fs::read(&path).unwrap();
    let genesis = spans(&bytes)[0];
    assert_eq!(genesis.tx, 0);
    let mut cut = bytes[..HEADER_LEN].to_vec();
    cut.extend_from_slice(&bytes[genesis.end..]);
    let cut_path = dir.path().join("no-genesis.kala");
    std::fs::write(&cut_path, &cut).unwrap();
    for o in [opts(), since(TxId(0)), canon(TxId(2)), canon(TxId(0))] {
        let rep = doctor_log(&cut_path, &o).expect("a scannable file is Ok, never a panic");
        assert!(
            rep.warnings().is_empty(),
            "no schema rule runs without a genesis: {:#?}",
            rep.findings
        );
        assert!(
            !rep.errors().is_empty(),
            "the missing genesis is an error: {:#?}",
            rep.findings
        );
    }
    // also: a header-only file (no transactions at all) doesn't panic
    let only_header = dir.path().join("only-header.kala");
    std::fs::write(&only_header, &bytes[..HEADER_LEN]).unwrap();
    let rep = doctor_log(&only_header, &opts()).unwrap();
    assert!(rep.warnings().is_empty(), "{:#?}", rep.findings);
}
