//! Slice 2, item 8: "Edwin's allies at tick T", milestone 0's headline query,
//! through the native API on a small hand-built world.
//!
//! Cast (creation order, so EntityId order is edwin < mara < lisa < greg).
//! `ally_of` is bidirectional; `trusts` and `loyal_to` are directed.
//!
//! Writes, in author (tx) order, with the story tick of each. The spelling is
//! the order the caller wrote the pair in:
//!
//! ```text
//!  w1  lisa  ally_of edwin   @-208 assert     (larger id first)
//!  w2  mara  ally_of edwin   @-100 assert
//!  w3  greg  ally_of edwin   @-500 assert     prequel, written after w1/w2
//!  w4  edwin ally_of greg    @-400 retract    end of the prequel alliance
//!  w5  edwin ally_of lisa    @-150 retract    end written before the re-formation
//!  w6  lisa  ally_of edwin   @-50  assert     re-formation (needs w5 first)
//!  w7  edwin ally_of mara    @-80  retract
//!  w8  lisa  ally_of mara    @-300 assert     prequel for another pair
//!  w9  mara  ally_of lisa    @-250 retract
//!  w10 mara  ally_of edwin   @30   assert     re-formed after w7
//!  traitor: mara trusts edwin @-100, edwin loyal_to lisa @-100 (secret)
//! ```
//!
//! Resulting intervals (half-open, latest canon):
//! edwin~greg [-500,-400), edwin~lisa [-208,-150) and [-50,inf),
//! edwin~mara [-100,-80) and [30,inf), mara~lisa [-300,-250).
mod common;

use kala_core::*;
use std::path::PathBuf;

fn schema() -> Schema {
    let a = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .attribute(a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(a("trusts", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(a("loyal_to", ValueType::Ref, Kind::State, Cardinality::Many).directed())
}

#[derive(Clone, Copy)]
struct Cast {
    edwin: EntityId,
    mara: EntityId,
    lisa: EntityId,
    greg: EntityId,
}

impl Cast {
    fn all(&self) -> [EntityId; 4] {
        [self.edwin, self.mara, self.lisa, self.greg]
    }
}

/// One alliance write: (tx it landed in, a, b as written, tick, is_assert).
type Rec = (TxId, EntityId, EntityId, i64, bool);

struct Story {
    c: Cast,
    writes: Vec<Rec>,
    /// latest_tx after the traitor writes
    last_tx: TxId,
}

fn write(w: &mut World, rec: &mut Vec<Rec>, a: EntityId, b: EntityId, t: i64, assert: bool) {
    let r = if assert {
        w.assert(a, "ally_of", Value::Ref(b), Some(Tick(t)))
    } else {
        w.retract(a, "ally_of", Value::Ref(b), Some(Tick(t)))
    }
    .unwrap();
    assert_eq!(r.changed, 1, "every story write must change something");
    assert_eq!(r.tick, Tick(t));
    rec.push((w.latest_tx(), a, b, t, assert));
}

fn build(w: &mut World) -> Story {
    let mk = |w: &mut World, h: &str| w.create_entity("CHARACTER", Some(h)).unwrap();
    let c = Cast {
        edwin: mk(w, "edwin"),
        mara: mk(w, "mara"),
        lisa: mk(w, "lisa"),
        greg: mk(w, "greg"),
    };
    assert!(c.edwin < c.mara && c.mara < c.lisa && c.lisa < c.greg);
    let mut r = Vec::new();
    write(w, &mut r, c.lisa, c.edwin, -208, true);
    write(w, &mut r, c.mara, c.edwin, -100, true);
    write(w, &mut r, c.greg, c.edwin, -500, true);
    write(w, &mut r, c.edwin, c.greg, -400, false);
    write(w, &mut r, c.edwin, c.lisa, -150, false);
    write(w, &mut r, c.lisa, c.edwin, -50, true);
    write(w, &mut r, c.edwin, c.mara, -80, false);
    write(w, &mut r, c.lisa, c.mara, -300, true);
    write(w, &mut r, c.mara, c.lisa, -250, false);
    write(w, &mut r, c.mara, c.edwin, 30, true);
    // the traitor example
    assert_eq!(
        w.assert(c.mara, "trusts", Value::Ref(c.edwin), Some(Tick(-100)))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        w.assert(c.edwin, "loyal_to", Value::Ref(c.lisa), Some(Tick(-100)))
            .unwrap()
            .changed,
        1
    );
    Story {
        c,
        writes: r,
        last_tx: w.latest_tx(),
    }
}

/// Naive reference: the pair holds at `t` at canon `canon` iff the latest op
/// with tick <= t among writes with tx <= canon is an assert.
fn model_holds(s: &Story, canon: TxId, a: EntityId, b: EntityId, t: i64) -> bool {
    let mut best: Option<(i64, bool)> = None;
    for &(tx, x, y, tick, asrt) in &s.writes {
        if tx > canon || tick > t || !((x == a && y == b) || (x == b && y == a)) {
            continue;
        }
        if best.is_none_or(|(bt, _)| tick >= bt) {
            best = Some((tick, asrt));
        }
    }
    best.is_some_and(|(_, asrt)| asrt)
}

fn model_allies(s: &Story, canon: TxId, x: EntityId, t: i64) -> Vec<EntityId> {
    s.c.all()
        .into_iter()
        .filter(|&p| model_holds(s, canon, x, p, t))
        .collect()
}

trait Reader {
    fn nb(&self, e: EntityId, attr: &str, d: Direction, t: i64) -> Vec<(EntityId, EdgeId)>;
}
impl Reader for World {
    fn nb(&self, e: EntityId, attr: &str, d: Direction, t: i64) -> Vec<(EntityId, EdgeId)> {
        self.neighbors(e, attr, d, Tick(t)).unwrap()
    }
}
impl Reader for View<'_> {
    fn nb(&self, e: EntityId, attr: &str, d: Direction, t: i64) -> Vec<(EntityId, EdgeId)> {
        self.neighbors(e, attr, d, Tick(t)).unwrap()
    }
}

fn ids(v: &[(EntityId, EdgeId)]) -> Vec<EntityId> {
    v.iter().map(|n| n.0).collect()
}

/// Ticks that straddle every boundary in the story, plus extremes.
fn probe_ticks() -> Vec<i64> {
    let mut v = vec![i64::MIN, i64::MIN + 1, -1000, i64::MAX - 1, i64::MAX, 0, 1];
    for b in [-500, -400, -300, -250, -208, -150, -100, -80, -50, 30] {
        v.extend([b - 1, b, b + 1]);
    }
    v.sort();
    v.dedup();
    v
}

fn check_latest(w: &World, s: &Story) {
    let c = s.c;
    let allies = |x: EntityId, t: i64| ids(&w.nb(x, "ally_of", Direction::Both, t));

    // Edwin: before any alliance
    assert!(allies(c.edwin, i64::MIN).is_empty());
    assert!(allies(c.edwin, -501).is_empty());
    // prequel alliance with Greg: assert tick included, retract tick excluded
    assert_eq!(allies(c.edwin, -500), vec![c.greg]);
    assert_eq!(allies(c.edwin, -450), vec![c.greg]);
    assert_eq!(allies(c.edwin, -401), vec![c.greg]);
    assert!(allies(c.edwin, -400).is_empty());
    // between
    assert!(allies(c.edwin, -300).is_empty());
    assert!(allies(c.edwin, -209).is_empty());
    // Lisa: [-208, -150)
    assert_eq!(allies(c.edwin, -208), vec![c.lisa]);
    assert_eq!(allies(c.edwin, -151), vec![c.lisa]);
    assert!(allies(c.edwin, -150).is_empty());
    assert!(allies(c.edwin, -101).is_empty());
    // Mara: [-100, -80)
    assert_eq!(allies(c.edwin, -100), vec![c.mara]);
    assert_eq!(allies(c.edwin, -81), vec![c.mara]);
    assert!(allies(c.edwin, -80).is_empty());
    assert!(allies(c.edwin, -51).is_empty());
    // Lisa again from -50; Mara again from 30
    assert_eq!(allies(c.edwin, -50), vec![c.lisa]);
    assert_eq!(allies(c.edwin, 29), vec![c.lisa]);
    assert_eq!(allies(c.edwin, 30), vec![c.mara, c.lisa]);
    assert_eq!(allies(c.edwin, i64::MAX), vec![c.mara, c.lisa]);

    // the partner's side agrees, whichever spelling was used to write
    assert_eq!(allies(c.greg, -500), vec![c.edwin]);
    assert!(allies(c.greg, -400).is_empty());
    assert_eq!(allies(c.lisa, -260), vec![c.mara]);
    assert_eq!(allies(c.lisa, -208), vec![c.edwin]);
    assert!(allies(c.lisa, -250).is_empty());
    assert_eq!(allies(c.lisa, 0), vec![c.edwin]);
    assert_eq!(allies(c.mara, -100), vec![c.edwin]);
    assert!(allies(c.mara, -80).is_empty());
    assert_eq!(allies(c.mara, -300), vec![c.lisa]);

    // direction does not matter for a bidirectional attribute, and edge IDs
    // are the same from both sides
    for t in probe_ticks() {
        for (x, p) in [
            (c.edwin, c.mara),
            (c.edwin, c.lisa),
            (c.edwin, c.greg),
            (c.mara, c.lisa),
        ] {
            let e = w.edge_id(x, "ally_of", p).unwrap();
            assert_eq!(w.edge_id(p, "ally_of", x), Some(e));
            for d in [Direction::Out, Direction::In, Direction::Both] {
                let from_x = w.nb(x, "ally_of", d, t).contains(&(p, e));
                let from_p = w.nb(p, "ally_of", d, t).contains(&(x, e));
                assert_eq!(from_x, from_p, "{d:?} t={t}");
            }
        }
    }

    // allies(-210) - allies(-200), and the reverse
    let diff = |x: EntityId, t1: i64, t2: i64| -> Vec<EntityId> {
        let b = allies(x, t2);
        allies(x, t1)
            .into_iter()
            .filter(|e| !b.contains(e))
            .collect()
    };
    assert!(diff(c.edwin, -210, -200).is_empty());
    assert_eq!(diff(c.edwin, -200, -210), vec![c.lisa]);
    // formed by -90 and gone by -60
    assert_eq!(diff(c.edwin, -90, -60), vec![c.mara]);
    // alliances gained between -90 and 0 minus those held at -90
    assert_eq!(diff(c.edwin, 0, -90), vec![c.lisa]);
    // -208 vs -150: Lisa's first alliance ended exactly at -150
    assert_eq!(diff(c.edwin, -208, -150), vec![c.lisa]);
    assert!(diff(c.edwin, -150, -208).is_empty());

    // the traitor: public alliance both ways, trust and loyalty one way
    let pub_ally = |t| ids(&w.nb(c.edwin, "ally_of", Direction::Both, t));
    assert_eq!(pub_ally(-100), vec![c.mara]);
    assert_eq!(
        ids(&w.nb(c.mara, "trusts", Direction::Out, -100)),
        vec![c.edwin]
    );
    assert!(w.nb(c.edwin, "trusts", Direction::Out, -100).is_empty());
    assert_eq!(
        ids(&w.nb(c.edwin, "trusts", Direction::In, -100)),
        vec![c.mara]
    );
    assert!(w.nb(c.mara, "trusts", Direction::In, -100).is_empty());
    assert!(w.nb(c.mara, "trusts", Direction::Out, -101).is_empty());
    assert_eq!(
        ids(&w.nb(c.edwin, "loyal_to", Direction::Out, -100)),
        vec![c.lisa]
    );
    assert!(w.nb(c.lisa, "loyal_to", Direction::Out, -100).is_empty());
    assert_eq!(
        ids(&w.nb(c.lisa, "loyal_to", Direction::In, -100)),
        vec![c.edwin]
    );
    // directed edges are separate edges from the alliance
    let t_edge = w.edge_id(c.mara, "trusts", c.edwin).unwrap();
    assert!(w.edge_id(c.edwin, "trusts", c.mara).is_none());
    assert_ne!(
        Some(t_edge),
        w.edge_id(c.edwin, "ally_of", c.mara),
        "trusts and ally_of are different edges"
    );
    // the secret loyalty to Lisa does not appear as an alliance at -100
    assert_eq!(allies(c.edwin, -100), vec![c.mara]);
}

/// Every canon, every probe tick, every entity, against the naive model.
fn check_against_model<R: Reader>(r: &R, s: &Story, canon: TxId) {
    for t in probe_ticks() {
        for x in s.c.all() {
            let want = model_allies(s, canon, x, t);
            for d in [Direction::Out, Direction::In, Direction::Both] {
                let got = ids(&r.nb(x, "ally_of", d, t));
                let mut want_sorted = want.clone();
                want_sorted.sort();
                assert_eq!(got, want_sorted, "canon {canon:?} {d:?} t={t}");
            }
        }
    }
}

fn check_canons(w: &World, s: &Story) {
    let c = s.c;
    // every canon that includes at least the cast
    let first = s.writes[0].0.0 - 1;
    for n in first..=s.last_tx.0 {
        let canon = TxId(n);
        check_against_model(&w.at_canon(canon), s, canon);
    }
    check_against_model(w, s, s.last_tx);

    let allies_at = |canon: TxId, x: EntityId, t: i64| {
        ids(&w
            .at_canon(canon)
            .neighbors(x, "ally_of", Direction::Both, Tick(t))
            .unwrap())
    };
    let tx_of = |i: usize| s.writes[i].0;

    // The prequel (w3: Greg @-500) is not visible before its tx even though
    // its tick is earlier than everything written before it.
    assert!(allies_at(tx_of(1), c.edwin, -500).is_empty());
    assert!(allies_at(tx_of(1), c.greg, -500).is_empty());
    assert_eq!(allies_at(tx_of(2), c.edwin, -500), vec![c.greg]);
    // ...and until w4 is written, Greg is still an ally "forever" after -500
    assert_eq!(allies_at(tx_of(2), c.edwin, -300), vec![c.greg]);
    assert_eq!(allies_at(tx_of(2), c.edwin, -208), vec![c.lisa, c.greg]);
    assert!(allies_at(tx_of(3), c.edwin, -300).is_empty());
    // Lisa's first alliance had no end until w5
    assert_eq!(allies_at(tx_of(3), c.edwin, 100), vec![c.mara, c.lisa]);
    assert_eq!(allies_at(tx_of(4), c.edwin, 100), vec![c.mara]);
    // re-formation w6
    assert_eq!(allies_at(tx_of(5), c.edwin, 100), vec![c.mara, c.lisa]);
    // before anything: empty
    assert!(allies_at(TxId(0), c.edwin, -208).is_empty());
    // the mara~lisa prequel
    assert!(allies_at(tx_of(6), c.lisa, -260).is_empty());
    assert_eq!(allies_at(tx_of(7), c.lisa, -260), vec![c.mara]);
    assert_eq!(allies_at(tx_of(7), c.lisa, 100), vec![c.edwin, c.mara]);
}

fn check_all(w: &World, s: &Story) {
    check_latest(w, s);
    check_canons(w, s);
}

#[test]
fn edwins_allies_in_memory() {
    let mut w = World::in_memory(schema()).unwrap();
    let s = build(&mut w);
    check_all(&w, &s);
}

fn file_world() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("world.kala");
    (dir, p)
}

#[test]
fn edwins_allies_file_backed() {
    let (_d, p) = file_world();
    let mut w = World::create(&p, schema()).unwrap();
    let s = build(&mut w);
    check_all(&w, &s);
}

#[test]
fn edwins_allies_identical_after_reopen() {
    let (_d, p) = file_world();
    let s = {
        let mut w = World::create(&p, schema()).unwrap();
        let s = build(&mut w);
        check_all(&w, &s);
        s
    };
    let w = World::open(&p).unwrap();
    assert_eq!(w.latest_tx(), s.last_tx);
    check_all(&w, &s);
    // handles still resolve to the same entities
    assert_eq!(w.entity_by_handle("edwin"), Some(s.c.edwin));
    // and a write after reopen keeps working on the same edge
    let mut w = w;
    let e = w.edge_id(s.c.edwin, "ally_of", s.c.greg).unwrap();
    assert_eq!(
        w.assert(s.c.greg, "ally_of", Value::Ref(s.c.edwin), Some(Tick(1000)))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        w.neighbors(s.c.edwin, "ally_of", Direction::Both, Tick(1000))
            .unwrap()
            .into_iter()
            .find(|n| n.0 == s.c.greg),
        Some((s.c.greg, e))
    );
}
