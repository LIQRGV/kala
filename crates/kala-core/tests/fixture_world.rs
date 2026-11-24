//! Acceptance tests on a realistic world: `tests/fixtures/varrow.yaml`, "the Varrow
//! chronicle" (33 characters, 14 places, a retcon, a prequel, a cardinality overlap
//! that is written and later resolved).
//!
//! Every expected answer below is written by hand from the story told in the fixture's
//! comments (and, for the 24 generated villagers, from the pattern described there),
//! never computed with the code under test. Ticks are years.
//!
//! The fixture's transactions, in author order. The entities are created in tx 1, so
//! fixture transaction `n` (1-based) is canon tx `n + 1`:
//!
//! | tx | what                                                         |
//! |----|--------------------------------------------------------------|
//! |  1 | every entity of the entities section                         |
//! |  2 | places named, rulers                                         |
//! |  3 | the old guard (Osric, Tomas, Hild, Mara, Wren)               |
//! |  4 | 24 villagers                                                 |
//! |  5 | Edwin's story                                                |
//! |  6 | Greg in Varrow @200                                          |
//! |  7 | Lisa's war                                                   |
//! |  8 | Greg in Ashford @601: overlap with Varrow (draft)            |
//! |  9 | prequel: Edwin's childhood, an early alliance, Greg's youth  |
//! | 10 | Greg left Varrow @250: overlap resolved                      |
//! | 11 | kael created, a second "Edwin"                               |
//! | 12 | the retcon                                                   |
mod common;

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use kala_fixture::{load_fixture, load_fixture_to};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/varrow.yaml");
const LATEST: TxId = TxId(12);
const OVERLAP_WRITTEN: TxId = TxId(8);
const PREQUEL: TxId = TxId(9);
const OVERLAP_RESOLVED: TxId = TxId(10);
const KAEL_CREATED: TxId = TxId(11);
const RETCON: TxId = TxId(12);
const BEFORE_RETCON: TxId = TxId(11);

const CORE: [&str; 8] = [
    "edwin", "greg", "mara", "lisa", "tomas", "hild", "osric", "wren",
];
const PLACES: [&str; 14] = [
    "varrow",
    "ashford",
    "carnath",
    "eldoria",
    "dunmere",
    "highkeep",
    "saltmarsh",
    "oakhollow",
    "redcliff",
    "stonebridge",
    "thornwick",
    "whitefall",
    "mistral",
    "graymoor",
];

fn yaml() -> String {
    std::fs::read_to_string(FIXTURE).expect("the fixture file")
}

fn villagers() -> Vec<String> {
    (1..=24).map(|i| format!("v{i:02}")).collect()
}

/// A loaded world plus a handle table, so answers can be compared as sets of handles.
struct Fx {
    w: World,
    by_id: HashMap<EntityId, String>,
}

impl Fx {
    fn new(w: World) -> Fx {
        let mut handles: Vec<String> = CORE.iter().map(|s| s.to_string()).collect();
        handles.extend(villagers());
        handles.push("kael".into());
        handles.extend(PLACES.iter().map(|s| s.to_string()));
        handles.extend(
            ["sword", "crown", "battle_of_ashford"]
                .iter()
                .map(|s| s.to_string()),
        );
        let by_id = handles
            .into_iter()
            .map(|h| (w.entity_by_handle(&h).unwrap_or_else(|| panic!("{h}")), h))
            .collect();
        Fx { w, by_id }
    }
    fn load() -> Fx {
        Fx::new(load_fixture(&yaml()).unwrap_or_else(|e| panic!("{e}")))
    }
    fn id(&self, h: &str) -> EntityId {
        self.w.entity_by_handle(h).unwrap_or_else(|| panic!("{h}"))
    }
    fn name(&self, e: EntityId) -> String {
        self.by_id
            .get(&e)
            .cloned()
            .unwrap_or_else(|| format!("?{e:?}"))
    }
    fn set(&self, ids: impl IntoIterator<Item = EntityId>) -> BTreeSet<String> {
        ids.into_iter().map(|e| self.name(e)).collect()
    }
    fn vals(&self, vs: &[Value]) -> BTreeSet<String> {
        vs.iter()
            .map(|v| match v {
                Value::Ref(e) => self.name(*e),
                Value::Str(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }
    /// `(tick, op, value as a handle or a string)` rows, with the precision.
    fn rows(&self, cs: Vec<Change>) -> Vec<(i64, Op, String, Precision)> {
        cs.into_iter()
            .map(|c| {
                let v = match &c.value {
                    Value::Ref(e) => self.name(*e),
                    Value::Str(s) => s.clone(),
                    Value::Bool(b) => b.to_string(),
                    other => format!("{other:?}"),
                };
                (c.tick.0, c.op, v, c.precision)
            })
            .collect()
    }
}

fn want(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// Allies of `who` at `t`, read at canon `tx` (`None` = latest), through `neighbors` Out.
fn allies(fx: &Fx, who: &str, t: i64, tx: Option<TxId>) -> BTreeSet<String> {
    let e = fx.id(who);
    let v = fx.w.at_canon(tx.unwrap_or(LATEST));
    fx.set(
        v.neighbors(e, "ally_of", Direction::Out, Tick(t))
            .unwrap()
            .into_iter()
            .map(|(x, _)| x),
    )
}

/// Who is in `place` at `t` (reverse read of `located_in`).
fn in_place(fx: &Fx, place: &str, t: i64) -> BTreeSet<String> {
    fx.set(
        fx.w.neighbors(fx.id(place), "located_in", Direction::In, Tick(t))
            .unwrap()
            .into_iter()
            .map(|(x, _)| x),
    )
}

// ================================================================ size and shape

#[test]
fn the_fixture_loads_and_has_the_size_the_story_promises() {
    let t0 = Instant::now();
    let fx = Fx::load();
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "loading took {:?}",
        t0.elapsed()
    );
    let count = |n: &str| fx.w.entities_of_type(n).unwrap().count();
    assert_eq!(count("CHARACTER"), 33, "8 core + 24 villagers + kael");
    assert_eq!(count("PLACE"), 14);
    assert_eq!(count("ITEM"), 2);
    assert_eq!(count("EVENT"), 1);
    assert_eq!(fx.w.latest_tx(), LATEST);
    assert_eq!(fx.w.latest_tick(), Tick(700), "Lisa becomes a Deity @700");
    // ids: type + sequence in list order
    assert_eq!(fx.w.format_id(fx.id("edwin")), "CHARACTER#1");
    assert_eq!(fx.w.format_id(fx.id("greg")), "CHARACTER#2");
    assert_eq!(fx.w.format_id(fx.id("v24")), "CHARACTER#32");
    assert_eq!(fx.w.format_id(fx.id("kael")), "CHARACTER#33");
    assert_eq!(fx.w.format_id(fx.id("varrow")), "PLACE#1");
    assert_eq!(fx.w.format_id(fx.id("graymoor")), "PLACE#14");
}

#[test]
fn the_log_holds_hundreds_of_facts_and_the_story_features() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("varrow.kala");
    drop(load_fixture_to(&yaml(), &path).unwrap());
    let es = read_log(&path).unwrap();
    let facts = es
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Fact { .. }))
        .count();
    let corrections = es
        .iter()
        .filter(|e| {
            matches!(
                e.body,
                EntryBody::Deleted { .. } | EntryBody::Revised { .. }
            )
        })
        .count();
    let commits = es
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Commit { .. }))
        .count();
    assert!(facts >= 300, "{facts} fact entries");
    assert_eq!(
        corrections, 5,
        "two replaces (Deleted + Revised each) and one delete"
    );
    assert_eq!(commits, 13, "genesis + entities + 11 transactions");
    let precisions: BTreeSet<_> = es
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Fact { precision, .. } => Some(format!("{precision:?}")),
            _ => None,
        })
        .collect();
    assert!(
        precisions.contains("Year") && precisions.contains("Decade"),
        "{precisions:?}"
    );
}

#[test]
fn entities_of_type_counts_follow_the_canon() {
    let fx = Fx::load();
    let chars = |tx: u64| {
        fx.w.at_canon(TxId(tx))
            .entities_of_type("CHARACTER")
            .unwrap()
            .count()
    };
    let main = |tx: u64| {
        fx.w.at_canon_main(TxId(tx))
            .entities_of_type("CHARACTER")
            .unwrap()
            .count()
    };
    for tx in [0u64, 1, 5, 10, 11, 12, 99] {
        assert_eq!(chars(tx), main(tx), "both read paths, canon {tx}");
    }
    assert_eq!(chars(0), 0);
    assert_eq!(chars(1), 32, "all but kael are created by tx 1");
    assert_eq!(chars(10), 32);
    assert_eq!(chars(11), 33, "kael is created in tx 11");
    assert_eq!(chars(12), 33);
    // ascending by sequence: edwin first, kael last
    let all: Vec<_> = fx.w.entities_of_type("CHARACTER").unwrap().collect();
    assert_eq!(all[0], fx.id("edwin"));
    assert_eq!(all[32], fx.id("kael"));
    assert!(all.windows(2).all(|p| p[0] < p[1]));
    let places = fx.set(fx.w.entities_of_type("PLACE").unwrap());
    assert_eq!(places, PLACES.iter().map(|s| s.to_string()).collect());
    assert_eq!(
        fx.set(fx.w.entities_of_type("ITEM").unwrap()),
        want(&["sword", "crown"])
    );
    assert_eq!(
        fx.set(fx.w.entities_of_type("EVENT").unwrap()),
        want(&["battle_of_ashford"])
    );
}

// ================================================================ Edwin's allies

#[test]
fn edwins_allies_at_several_ticks_at_the_latest_canon() {
    let fx = Fx::load();
    // tomas from -300 for good; greg [-320,-300) and [-280,-200) (retconned end);
    // hild [-260,-240); mara [-208,-150) and [-60,..); wren was deleted
    let table: &[(i64, &[&str])] = &[
        (-1000, &[]),
        (-330, &[]),
        (-320, &["greg"]),
        (-310, &["greg"]),
        (-300, &["tomas"]),
        (-290, &["tomas"]),
        (-280, &["tomas", "greg"]),
        (-260, &["tomas", "greg", "hild"]),
        (-250, &["tomas", "greg", "hild"]),
        (-241, &["tomas", "greg", "hild"]),
        (-240, &["tomas", "greg"]),
        (-210, &["tomas", "greg"]),
        (-208, &["tomas", "greg", "mara"]),
        (-201, &["tomas", "greg", "mara"]),
        (-200, &["tomas", "mara"]),
        (-170, &["tomas", "mara"]),
        (-151, &["tomas", "mara"]),
        (-150, &["tomas"]),
        (-100, &["tomas"]),
        (-61, &["tomas"]),
        (-60, &["tomas", "mara"]),
        (700, &["tomas", "mara"]),
        (i64::MAX, &["tomas", "mara"]),
    ];
    for (t, expect) in table {
        assert_eq!(allies(&fx, "edwin", *t, None), want(expect), "edwin @{t}");
        // the same answer through as_of (the values of edwin.ally_of)
        let vals = fx.w.as_of(fx.id("edwin"), "ally_of", Tick(*t)).unwrap();
        assert_eq!(fx.vals(&vals), want(expect), "as_of edwin @{t}");
    }
}

#[test]
fn an_alliance_reads_the_same_from_either_side() {
    let fx = Fx::load();
    // mara: edwin during [-208,-150) and [-60,..)
    for (t, e) in [
        (-209, vec![]),
        (-208, vec!["edwin"]),
        (-150, vec![]),
        (-61, vec![]),
        (-60, vec!["edwin"]),
    ] {
        assert_eq!(allies(&fx, "mara", t, None), want(&e), "mara @{t}");
    }
    // greg: edwin during [-320,-300) and [-280,-200)
    for (t, e) in [
        (-321, vec![]),
        (-320, vec!["edwin"]),
        (-300, vec![]),
        (-290, vec![]),
        (-280, vec!["edwin"]),
        (-201, vec!["edwin"]),
        (-200, vec![]),
    ] {
        assert_eq!(allies(&fx, "greg", t, None), want(&e), "greg @{t}");
    }
    // tomas: edwin from -300, lisa from 590 (the steward's betrayal)
    for (t, e) in [
        (-301, vec![]),
        (-300, vec!["edwin"]),
        (589, vec!["edwin"]),
        (590, vec!["edwin", "lisa"]),
    ] {
        assert_eq!(allies(&fx, "tomas", t, None), want(&e), "tomas @{t}");
    }
    // Lisa's only ally is Tomas; Edwin never was her ally
    assert_eq!(allies(&fx, "lisa", 700, None), want(&["tomas"]));
    // In reads the same set for a bidirectional attribute
    let ins =
        fx.w.neighbors(fx.id("edwin"), "ally_of", Direction::In, Tick(700))
            .unwrap();
    assert_eq!(
        fx.set(ins.into_iter().map(|x| x.0)),
        want(&["tomas", "mara"])
    );
    // one edge per pair, whichever side wrote it
    let e1 = fx.w.edge_id(fx.id("tomas"), "ally_of", fx.id("edwin"));
    let e2 = fx.w.edge_id(fx.id("edwin"), "ally_of", fx.id("tomas"));
    assert!(e1.is_some());
    assert_eq!(e1, e2);
}

#[test]
fn the_villagers_alliances_follow_the_pattern() {
    // pair k = (v(2k-1), v(2k)): ally from -300+k; odd k end at -150+k, even k never
    let fx = Fx::load();
    for k in 1..=12i64 {
        let (a, b) = (format!("v{:02}", 2 * k - 1), format!("v{:02}", 2 * k));
        let from = -300 + k;
        assert_eq!(
            allies(&fx, &a, from - 1, None),
            want(&[]),
            "{a} before pair {k}"
        );
        assert_eq!(
            allies(&fx, &a, from, None),
            want(&[b.as_str()]),
            "{a} pair {k}"
        );
        assert_eq!(
            allies(&fx, &b, from, None),
            want(&[a.as_str()]),
            "{b} pair {k}"
        );
        if k % 2 == 1 {
            let until = -150 + k;
            assert_eq!(
                allies(&fx, &b, until - 1, None),
                want(&[a.as_str()]),
                "pair {k} just before its end"
            );
            assert_eq!(allies(&fx, &a, until, None), want(&[]), "pair {k} broken");
            assert_eq!(
                allies(&fx, &b, 700, None),
                want(&[]),
                "pair {k} still broken"
            );
        } else {
            assert_eq!(
                allies(&fx, &a, 700, None),
                want(&[b.as_str()]),
                "pair {k} lasts"
            );
        }
    }
}

// ================================================================ retcon and canons

#[test]
fn before_the_retcon_edwin_still_had_wren_and_greg_until_190() {
    let fx = Fx::load();
    let c = Some(BEFORE_RETCON);
    let table: &[(i64, &[&str])] = &[
        (-320, &["greg"]),
        (-300, &["tomas"]),
        (-280, &["tomas", "greg"]),
        (-208, &["tomas", "greg", "mara"]),
        (-200, &["tomas", "greg", "mara"]),
        (-191, &["tomas", "greg", "mara"]),
        (-190, &["tomas", "mara"]),
        (-171, &["tomas", "mara"]),
        (-170, &["tomas", "mara", "wren"]),
        (-150, &["tomas", "wren"]),
        (-100, &["tomas", "wren"]),
        (-60, &["tomas", "mara", "wren"]),
        (700, &["tomas", "mara", "wren"]),
    ];
    for (t, expect) in table {
        assert_eq!(
            allies(&fx, "edwin", *t, c),
            want(expect),
            "canon 11, edwin @{t}"
        );
    }
    // wren's side of the deleted edge
    assert_eq!(allies(&fx, "wren", 0, c), want(&["edwin"]));
    assert_eq!(allies(&fx, "wren", 0, None), want(&[]));
}

#[test]
fn before_the_prequel_the_early_alliance_and_childhood_do_not_exist() {
    let fx = Fx::load();
    let c = Some(TxId(8));
    assert_eq!(allies(&fx, "edwin", -320, c), want(&[]));
    assert_eq!(allies(&fx, "edwin", -310, c), want(&[]));
    assert_eq!(allies(&fx, "edwin", -300, c), want(&["tomas"]));
    let v = fx.w.at_canon(TxId(8));
    assert!(
        v.as_of(fx.id("edwin"), "located_in", Tick(-340))
            .unwrap()
            .is_empty()
    );
    assert!(v.history(fx.id("edwin"), "born_in").unwrap().is_empty());
    // the prequel arrived in tx 9
    let v9 = fx.w.at_canon(PREQUEL);
    assert_eq!(
        fx.vals(&v9.as_of(fx.id("edwin"), "located_in", Tick(-340)).unwrap()),
        want(&["oakhollow"])
    );
    assert_eq!(v9.history(fx.id("edwin"), "born_in").unwrap().len(), 1);
}

#[test]
fn the_history_of_edwins_alliances_before_and_after_the_retcon() {
    let fx = Fx::load();
    use Op::*;
    use Precision::*;
    let r = |tx: TxId| {
        fx.rows(
            fx.w.at_canon(tx)
                .history(fx.id("edwin"), "ally_of")
                .unwrap(),
        )
    };
    // same-tick rows are ordered by value encoding: greg (CHARACTER#2) before tomas (#5)
    let after = vec![
        (-320, Assert, "greg".to_string(), Exact),
        (-300, Retract, "greg".into(), Exact),
        (-300, Assert, "tomas".into(), Exact),
        (-280, Assert, "greg".into(), Exact),
        (-260, Assert, "hild".into(), Exact),
        (-240, Retract, "hild".into(), Exact),
        (-208, Assert, "mara".into(), Year),
        (-200, Retract, "greg".into(), Exact),
        (-150, Retract, "mara".into(), Exact),
        (-60, Assert, "mara".into(), Exact),
    ];
    assert_eq!(r(RETCON), after);
    assert_eq!(r(LATEST), after);
    let before = vec![
        (-320, Assert, "greg".to_string(), Exact),
        (-300, Retract, "greg".into(), Exact),
        (-300, Assert, "tomas".into(), Exact),
        (-280, Assert, "greg".into(), Exact),
        (-260, Assert, "hild".into(), Exact),
        (-240, Retract, "hild".into(), Exact),
        (-208, Assert, "mara".into(), Year),
        (-190, Retract, "greg".into(), Exact),
        (-170, Assert, "wren".into(), Exact),
        (-150, Retract, "mara".into(), Exact),
        (-60, Assert, "mara".into(), Exact),
    ];
    assert_eq!(r(BEFORE_RETCON), before);
}

#[test]
fn the_typo_in_an_alias_is_replaced_but_older_canon_still_reads_it() {
    let fx = Fx::load();
    let edwin = fx.id("edwin");
    let at = |tx: TxId, t: i64| fx.vals(&fx.w.at_canon(tx).as_of(edwin, "alias", Tick(t)).unwrap());
    assert_eq!(at(LATEST, -310), want(&[]));
    assert_eq!(at(LATEST, -300), want(&["Ed"]));
    assert_eq!(at(LATEST, -290), want(&["Ed", "Eddie"]));
    assert_eq!(at(LATEST, -250), want(&["Ed", "Eddie", "the Lost Knight"]));
    assert_eq!(at(LATEST, -180), want(&["Ed", "Eddie"]));
    assert_eq!(at(BEFORE_RETCON, -290), want(&["Ed", "Eddy"]));
    // lookups follow the canon too
    let v_old = fx.w.at_canon(BEFORE_RETCON);
    assert_eq!(
        fx.set(v_old.lookup("alias", &s("Eddy"), Tick(0)).unwrap()),
        want(&["edwin"])
    );
    assert!(
        fx.w.lookup("alias", &s("Eddy"), Tick(0))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fx.set(fx.w.lookup("alias", &s("Eddie"), Tick(0)).unwrap()),
        want(&["edwin"])
    );
    assert_eq!(
        fx.set(fx.w.lookup_ci("alias", "EDDIE", Tick(0)).unwrap()),
        want(&["edwin"])
    );
}

#[test]
fn the_latest_index_and_the_main_index_give_the_same_answers_after_the_retcon() {
    let fx = Fx::load();
    let (a, b) = (fx.w.at_canon(LATEST), fx.w.at_canon_main(LATEST));
    for who in CORE {
        for attr in ["name", "alias", "located_in", "ally_of", "label"] {
            let e = fx.id(who);
            assert_eq!(
                a.history(e, attr).unwrap(),
                b.history(e, attr).unwrap(),
                "{who}.{attr}"
            );
            for t in [-400, -300, -208, -200, -190, -170, -100, 0, 250, 601, 700] {
                assert_eq!(
                    a.as_of(e, attr, Tick(t)).unwrap(),
                    b.as_of(e, attr, Tick(t)).unwrap(),
                    "{who}.{attr} @{t}"
                );
            }
        }
    }
}

// ================================================================ who is where

#[test]
fn who_is_in_varrow_at_several_ticks() {
    let fx = Fx::load();
    // osric from -400, tomas from -350, v01 [-395,-95) (then Highkeep), v15 [-325,-25),
    // edwin [-300,-250), hild [-270,-230), mara [-220,-190), v10 from -50, v24 from 20,
    // greg [200,250)
    let table: &[(i64, &[&str])] = &[
        (-401, &[]),
        (-400, &["osric"]),
        (-350, &["osric", "tomas", "v01"]),
        (-300, &["edwin", "osric", "tomas", "v01", "v15"]),
        (-260, &["edwin", "hild", "osric", "tomas", "v01", "v15"]),
        (-250, &["hild", "osric", "tomas", "v01", "v15"]),
        (-200, &["mara", "osric", "tomas", "v01", "v15"]),
        (-190, &["osric", "tomas", "v01", "v15"]),
        (-100, &["osric", "tomas", "v01", "v15"]),
        (-95, &["osric", "tomas", "v15"]),
        (-50, &["osric", "tomas", "v10", "v15"]),
        (-25, &["osric", "tomas", "v10"]),
        (20, &["osric", "tomas", "v10", "v24"]),
        (200, &["greg", "osric", "tomas", "v10", "v24"]),
        (250, &["osric", "tomas", "v10", "v24"]),
        (700, &["osric", "tomas", "v10", "v24"]),
    ];
    for (t, expect) in table {
        assert_eq!(in_place(&fx, "varrow", *t), want(expect), "varrow @{t}");
        // lookup on a ref value answers the same question
        let by_lookup =
            fx.w.lookup("located_in", &Value::Ref(fx.id("varrow")), Tick(*t))
                .unwrap();
        assert_eq!(fx.set(by_lookup), want(expect), "lookup varrow @{t}");
    }
}

#[test]
fn who_is_in_ashford_and_highkeep() {
    let fx = Fx::load();
    // ashford: v02 [-390,-90), v16 [-320,-20), v11 from -45; kael from 300; lisa from 600;
    // greg from 601
    for (t, e) in [
        (-391, vec![]),
        (-390, vec!["v02"]),
        (-321, vec!["v02"]),
        (-320, vec!["v02", "v16"]),
        (-91, vec!["v02", "v16"]),
        (-90, vec!["v16"]),
        (-46, vec!["v16"]),
        (-45, vec!["v11", "v16"]),
        (-20, vec!["v11"]),
        (299, vec!["v11"]),
        (300, vec!["kael", "v11"]),
        (599, vec!["kael", "v11"]),
        (600, vec!["kael", "lisa", "v11"]),
        (601, vec!["greg", "kael", "lisa", "v11"]),
        (700, vec!["greg", "kael", "lisa", "v11"]),
    ] {
        assert_eq!(in_place(&fx, "ashford", t), want(&e), "ashford @{t}");
    }
    // highkeep: v06 [-370,-70), v20 [-300,0), then v01 from -95 and v15 from -25 for good
    for (t, e) in [
        (-371, vec![]),
        (-370, vec!["v06"]),
        (-300, vec!["v06", "v20"]),
        (-96, vec!["v06", "v20"]),
        (-95, vec!["v01", "v06", "v20"]),
        (-71, vec!["v01", "v06", "v20"]),
        (-70, vec!["v01", "v20"]),
        (-25, vec!["v01", "v15", "v20"]),
        (-1, vec!["v01", "v15", "v20"]),
        (0, vec!["v01", "v15"]),
        (700, vec!["v01", "v15"]),
    ] {
        assert_eq!(in_place(&fx, "highkeep", t), want(&e), "highkeep @{t}");
    }
    // dunmere: v05 [-375,-75), v19 [-305,-5), v14 from -30, lisa [500,600)
    assert_eq!(in_place(&fx, "dunmere", -400), want(&[]));
    assert_eq!(in_place(&fx, "dunmere", -375), want(&["v05"]));
    assert_eq!(in_place(&fx, "dunmere", -300), want(&["v05", "v19"]));
    assert_eq!(in_place(&fx, "dunmere", -50), want(&["v19"]));
    assert_eq!(in_place(&fx, "dunmere", -30), want(&["v14", "v19"]));
    assert_eq!(in_place(&fx, "dunmere", -5), want(&["v14"]));
    assert_eq!(in_place(&fx, "dunmere", 499), want(&["v14"]));
    assert_eq!(in_place(&fx, "dunmere", 500), want(&["lisa", "v14"]));
    assert_eq!(in_place(&fx, "dunmere", 600), want(&["v14"]));
}

#[test]
fn places_carry_extra_labels_and_villagers_are_born_where_they_arrive() {
    let fx = Fx::load();
    let by = |l: &str| fx.set(fx.w.lookup("label", &s(l), Tick(0)).unwrap());
    assert_eq!(by("City"), want(&["varrow", "ashford"]));
    assert_eq!(by("Port"), want(&["dunmere", "mistral"]));
    assert_eq!(by("Fortress"), want(&["carnath", "highkeep"]));
    assert_eq!(by("Capital"), want(&["eldoria"]));
    assert_eq!(by("Marsh"), want(&["saltmarsh"]));
    assert_eq!(
        by("Village"),
        want(&[
            "oakhollow",
            "redcliff",
            "stonebridge",
            "thornwick",
            "whitefall",
            "graymoor"
        ])
    );
    // v05: born in dunmere @-395 (the decade), arrives @-375
    assert_eq!(
        fx.rows(fx.w.history(fx.id("v05"), "born_in").unwrap()),
        vec![(-395, Op::Assert, "dunmere".to_string(), Precision::Decade)]
    );
    // Lisa's title ends with a decade-vague retract
    assert_eq!(
        fx.rows(fx.w.history(fx.id("lisa"), "alias").unwrap()),
        vec![
            (
                480,
                Op::Assert,
                "the Ashen Queen".to_string(),
                Precision::Exact
            ),
            (
                700,
                Op::Retract,
                "the Ashen Queen".to_string(),
                Precision::Decade
            ),
        ]
    );
    assert_eq!(
        fx.vals(&fx.w.as_of(fx.id("lisa"), "alias", Tick(699)).unwrap()),
        want(&["the Ashen Queen"])
    );
    assert_eq!(
        fx.vals(&fx.w.as_of(fx.id("lisa"), "alias", Tick(700)).unwrap()),
        want(&[])
    );
}

#[test]
fn villagers_move_with_an_explicit_retract_and_assert() {
    let fx = Fx::load();
    // (villager, home, arrives, leaves, new place)
    for (v, home, ta, tl, new) in [
        ("v01", "varrow", -395, -95, "highkeep"),
        ("v10", "stonebridge", -350, -50, "varrow"),
        ("v14", "graymoor", -330, -30, "dunmere"),
        ("v24", "stonebridge", -280, 20, "varrow"),
    ] {
        let e = fx.id(v);
        let at = |t: i64| fx.vals(&fx.w.as_of(e, "located_in", Tick(t)).unwrap());
        assert_eq!(at(ta - 1), want(&[]), "{v} before");
        assert_eq!(at(ta), want(&[home]), "{v} arrives");
        assert_eq!(at(tl - 1), want(&[home]), "{v} still home");
        assert_eq!(
            at(tl),
            want(&[new]),
            "{v} moved: never in both places at once"
        );
        assert_eq!(at(700), want(&[new]));
        assert!(
            fx.w.overlaps("located_in")
                .unwrap()
                .iter()
                .all(|o| o.entity != e)
        );
    }
}

#[test]
fn edwins_path_and_the_places_he_was_never_in() {
    let fx = Fx::load();
    let at = |t: i64| fx.vals(&fx.w.as_of(fx.id("edwin"), "located_in", Tick(t)).unwrap());
    for (t, e) in [
        (-341, vec![]),
        (-340, vec!["oakhollow"]),
        (-301, vec!["oakhollow"]),
        (-300, vec!["varrow"]),
        (-251, vec!["varrow"]),
        (-250, vec!["carnath"]),
        (-181, vec!["carnath"]),
        (-180, vec!["eldoria"]),
        (700, vec!["eldoria"]),
    ] {
        assert_eq!(at(t), want(&e), "edwin @{t}");
    }
    use Op::*;
    let rows: Vec<(i64, Op, String)> = fx
        .rows(fx.w.history(fx.id("edwin"), "located_in").unwrap())
        .into_iter()
        .map(|(t, o, v, _)| (t, o, v))
        .collect();
    // same-tick rows by value encoding: varrow (PLACE#1) < carnath (#3) < eldoria (#4) < oakhollow (#8)
    assert_eq!(
        rows,
        vec![
            (-340, Assert, "oakhollow".to_string()),
            (-300, Assert, "varrow".into()),
            (-300, Retract, "oakhollow".into()),
            (-250, Retract, "varrow".into()),
            (-250, Assert, "carnath".into()),
            (-180, Retract, "carnath".into()),
            (-180, Assert, "eldoria".into()),
        ]
    );
}

// ================================================================ names

#[test]
fn find_by_name_case_insensitively_follows_the_story() {
    let fx = Fx::load();
    let ci = |q: &str, t: i64| fx.set(fx.w.lookup_ci("name", q, Tick(t)).unwrap());
    // Edwin is "Edwin" in [-300,-180), then "the Grey King"
    assert_eq!(ci("edwin", -301), want(&[]));
    assert_eq!(ci("edwin", -300), want(&["edwin"]));
    assert_eq!(ci("EDWIN", -200), want(&["edwin"]));
    assert_eq!(ci("eDwIn", -181), want(&["edwin"]));
    assert_eq!(ci("edwin", -180), want(&[]));
    assert_eq!(ci("edwin", 0), want(&[]));
    // a second Edwin: kael, from 300 (names are not IDs)
    assert_eq!(ci("edwin", 299), want(&[]));
    assert_eq!(ci("edwin", 300), want(&["kael"]));
    assert_eq!(ci("edwin", 700), want(&["kael"]));
    // the Grey King
    assert_eq!(ci("THE GREY KING", -181), want(&[]));
    assert_eq!(ci("THE GREY KING", -180), want(&["edwin"]));
    assert_eq!(ci("the grey king", 700), want(&["edwin"]));
    // Mara became "Mara Voss" at -100 (retract + assert at one tick)
    assert_eq!(ci("mara", -101), want(&["mara"]));
    assert_eq!(ci("mara", -100), want(&[]));
    assert_eq!(ci("MARA VOSS", -101), want(&[]));
    assert_eq!(ci("mara voss", -100), want(&["mara"]));
    // Unicode: folded after NFC, so composed and decomposed queries agree
    assert_eq!(ci("wren \u{e5}berg", 0), want(&["wren"]));
    assert_eq!(ci("WREN \u{c5}BERG", 0), want(&["wren"]));
    assert_eq!(ci("wren a\u{30a}berg", 0), want(&["wren"]));
    // places and things are found by name too (different node types, one attribute)
    assert_eq!(ci("varrow", 0), want(&["varrow"]));
    assert_eq!(ci("crown of eldoria", 0), want(&["crown"]));
    assert_eq!(ci("nobody", 0), want(&[]));
    // the first villager and a villager that does not exist
    assert_eq!(ci("villager 07", 0), want(&["v07"]));
    assert_eq!(ci("villager 25", 0), want(&[]));
}

#[test]
fn find_by_name_exactly_is_case_sensitive() {
    let fx = Fx::load();
    let ex = |q: &str, t: i64| fx.set(fx.w.lookup("name", &s(q), Tick(t)).unwrap());
    assert_eq!(ex("Edwin", -200), want(&["edwin"]));
    assert_eq!(ex("edwin", -200), want(&[]));
    assert_eq!(ex("Edwin", 300), want(&["kael"]));
    assert_eq!(ex("the Grey King", 0), want(&["edwin"]));
    assert_eq!(ex("The Grey King", 0), want(&[]));
    assert_eq!(ex("Mara", -101), want(&["mara"]));
    assert_eq!(ex("Mara", -100), want(&[]));
    assert_eq!(ex("Mara Voss", 0), want(&["mara"]));
    assert_eq!(ex("Villager 24", 0), want(&["v24"]));
    // at older canons: before kael existed nobody was "Edwin" at tick 300
    let before_kael = fx.w.at_canon(OVERLAP_RESOLVED);
    assert!(
        before_kael
            .lookup("name", &s("Edwin"), Tick(300))
            .unwrap()
            .is_empty()
    );
    assert!(
        before_kael
            .lookup_ci("name", "edwin", Tick(300))
            .unwrap()
            .is_empty()
    );
    let with_kael = fx.w.at_canon(KAEL_CREATED);
    assert_eq!(
        fx.set(with_kael.lookup("name", &s("Edwin"), Tick(300)).unwrap()),
        want(&["kael"])
    );
    assert_eq!(
        fx.set(with_kael.lookup_ci("name", "edwin", Tick(300)).unwrap()),
        want(&["kael"])
    );
}

#[test]
fn aliases_labels_and_the_unindexed_biography() {
    let fx = Fx::load();
    let ex = |a: &str, q: &str, t: i64| fx.set(fx.w.lookup(a, &s(q), Tick(t)).unwrap());
    assert_eq!(ex("alias", "Old Osric", 0), want(&["osric"]));
    assert_eq!(ex("alias", "the Innkeeper", -391), want(&[]));
    assert_eq!(ex("alias", "the Innkeeper", -390), want(&["osric"]));
    assert_eq!(ex("alias", "V07", 0), want(&["v07"]));
    assert_eq!(ex("alias", "the Lost Knight", -251), want(&[]));
    assert_eq!(ex("alias", "the Lost Knight", -250), want(&["edwin"]));
    assert_eq!(ex("alias", "the Lost Knight", -180), want(&[]));
    // extra labels: cardinality many
    assert_eq!(ex("label", "Knight", -300), want(&["edwin"]));
    assert_eq!(ex("label", "King", -181), want(&[]));
    assert_eq!(ex("label", "King", -180), want(&["edwin"]));
    assert_eq!(ex("label", "Deity", 699), want(&[]));
    assert_eq!(ex("label", "Deity", 700), want(&["lisa"]));
    assert_eq!(ex("label", "Warlord", 700), want(&["lisa"]));
    assert_eq!(ex("label", "Villager", 0).len(), 24);
    assert_eq!(
        fx.vals(&fx.w.as_of(fx.id("lisa"), "label", Tick(700)).unwrap()),
        want(&["Warlord", "Deity"])
    );
    assert_eq!(
        fx.vals(&fx.w.as_of(fx.id("lisa"), "label", Tick(479)).unwrap()),
        want(&[])
    );
    // the biography has no V entries, so it can't be looked up; it is still a fact
    assert!(matches!(
        fx.w.lookup("biography", &s("anything"), Tick(0)),
        Err(Error::NotIndexed(_))
    ));
    assert!(matches!(
        fx.w.lookup_ci("biography", "anything", Tick(0)),
        Err(Error::NotIndexed(_))
    ));
    let bio = fx.w.as_of(fx.id("edwin"), "biography", Tick(0)).unwrap();
    assert_eq!(bio.len(), 1);
    let Value::Str(text) = &bio[0] else {
        panic!("{bio:?}")
    };
    assert!(
        text.starts_with("Edwin was the second son of a Varrow cooper"),
        "{text}"
    );
    assert!(text.ends_with("one very large unpaid tab."), "{text}");
    assert!(
        text.len() > 256,
        "longer than an indexed string may be: {}",
        text.len()
    );
    assert!(
        fx.w.as_of(fx.id("edwin"), "biography", Tick(-301))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_history_of_names_carries_the_precision() {
    let fx = Fx::load();
    use Op::*;
    use Precision::*;
    assert_eq!(
        fx.rows(fx.w.history(fx.id("edwin"), "name").unwrap()),
        vec![
            (-300, Assert, "Edwin".to_string(), Exact),
            // same tick: "Edwin" < "the Grey King" by bytes
            (-180, Retract, "Edwin".into(), Year),
            (-180, Assert, "the Grey King".into(), Year),
        ]
    );
    assert_eq!(
        fx.rows(fx.w.history(fx.id("mara"), "name").unwrap()),
        vec![
            (-400, Assert, "Mara".to_string(), Exact),
            (-100, Retract, "Mara".into(), Decade),
            (-100, Assert, "Mara Voss".into(), Decade),
        ]
    );
    assert_eq!(
        fx.rows(fx.w.history(fx.id("edwin"), "born_in").unwrap()),
        vec![(-240, Assert, "varrow".to_string(), Decade)]
    );
    // as_of_detail carries the precision of the change that makes a value hold
    let d = fx.w.as_of_detail(fx.id("edwin"), "name", Tick(0)).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].tick, d[0].precision), (Tick(-180), Year));
    // queries still treat the tick as exact
    assert!(
        fx.w.as_of(fx.id("edwin"), "name", Tick(-181))
            .unwrap()
            .contains(&s("Edwin"))
    );
}

// ================================================================ rulers, edges, events

#[test]
fn rulers_are_a_directed_attribute_with_a_reverse_label() {
    let fx = Fx::load();
    let ruler = |p: &str, t: i64| fx.vals(&fx.w.as_of(fx.id(p), "ruled_by", Tick(t)).unwrap());
    assert_eq!(ruler("varrow", -351), want(&[]));
    assert_eq!(ruler("varrow", -350), want(&["tomas"]));
    assert_eq!(ruler("varrow", -191), want(&["tomas"]));
    assert_eq!(ruler("varrow", -190), want(&["hild"]));
    assert_eq!(ruler("varrow", 700), want(&["hild"]));
    assert_eq!(ruler("eldoria", -181), want(&[]));
    assert_eq!(ruler("eldoria", -180), want(&["edwin"]));
    // who rules what, read from the ruler's side
    let rules = |who: &str, t: i64| {
        fx.set(
            fx.w.neighbors(fx.id(who), "ruled_by", Direction::In, Tick(t))
                .unwrap()
                .into_iter()
                .map(|x| x.0),
        )
    };
    assert_eq!(rules("tomas", -350), want(&["varrow"]));
    assert_eq!(rules("tomas", -190), want(&[]));
    assert_eq!(rules("hild", -190), want(&["varrow"]));
    assert_eq!(rules("edwin", -180), want(&["eldoria"]));
    assert_eq!(rules("lisa", 649), want(&[]));
    assert_eq!(rules("lisa", 650), want(&["ashford"]));
}

#[test]
fn the_present_edge_carries_a_disguise_that_changes() {
    let fx = Fx::load();
    let edwin = fx.id("edwin");
    let present = |t: i64| {
        fx.w.neighbors(edwin, "present", Direction::Out, Tick(t))
            .unwrap()
    };
    assert!(present(-251).is_empty());
    assert_eq!(present(-250).len(), 1);
    assert_eq!(present(-181).len(), 1);
    assert!(present(-180).is_empty(), "Edwin left Carnath");
    let (target, edge) = present(-220)[0];
    assert_eq!(fx.name(target), "carnath");
    assert_eq!(Some(edge), fx.w.edge_id(edwin, "present", fx.id("carnath")));
    let e: EntityId = edge.into();
    let dis = |t: i64| fx.w.as_of(e, "disguised", Tick(t)).unwrap();
    assert!(dis(-251).is_empty());
    assert_eq!(dis(-250), vec![Value::Bool(true)]);
    assert_eq!(dis(-221), vec![Value::Bool(true)]);
    assert_eq!(
        dis(-220),
        vec![Value::Bool(false)],
        "dropped and replaced at -220"
    );
    assert_eq!(dis(-181), vec![Value::Bool(false)]);
    use Op::*;
    let rows: Vec<_> = fx
        .rows(fx.w.history(e, "disguised").unwrap())
        .into_iter()
        .map(|(t, o, v, _)| (t, o, v))
        .collect();
    assert_eq!(
        rows,
        vec![
            (-250, Assert, "true".to_string()),
            (-220, Assert, "false".into()),
            (-220, Retract, "true".into()),
        ]
    );
    // Mara's lodging at Graymoor: an edge with its own property
    let me =
        fx.w.edge_id(fx.id("mara"), "present", fx.id("graymoor"))
            .unwrap();
    assert_eq!(
        fx.w.as_of(me.into(), "disguised", Tick(0)).unwrap(),
        vec![Value::Bool(false)]
    );
    assert_ne!(EntityId::from(me), e);
}

#[test]
fn events_are_never_retracted() {
    let fx = Fx::load();
    let bought = |who: &str, t: i64| {
        fx.set(
            fx.w.neighbors(fx.id(who), "bought", Direction::Out, Tick(t))
                .unwrap()
                .into_iter()
                .map(|x| x.0),
        )
    };
    assert_eq!(bought("edwin", -209), want(&[]));
    assert_eq!(bought("edwin", -208), want(&["sword"]));
    assert_eq!(bought("edwin", 700), want(&["sword"]));
    assert_eq!(bought("tomas", 700), want(&["crown"]));
    let buyers =
        fx.w.neighbors(fx.id("sword"), "bought", Direction::In, Tick(0))
            .unwrap();
    assert_eq!(fx.set(buyers.into_iter().map(|x| x.0)), want(&["edwin"]));
    assert_eq!(
        fx.set(
            fx.w.lookup("name", &s("Battle of Ashford"), Tick(600))
                .unwrap()
        ),
        want(&["battle_of_ashford"])
    );
}

// ================================================================ Greg and the overlap

#[test]
fn where_greg_is_at_the_latest_canon() {
    let fx = Fx::load();
    let at = |t: i64| fx.vals(&fx.w.as_of(fx.id("greg"), "located_in", Tick(t)).unwrap());
    for (t, e) in [
        (-101, vec![]),
        (-100, vec!["carnath"]),
        (149, vec!["carnath"]),
        (150, vec![]),
        (199, vec![]),
        (200, vec!["varrow"]),
        (249, vec!["varrow"]),
        (250, vec![]),
        (600, vec![]),
        (601, vec!["ashford"]),
        (i64::MAX, vec!["ashford"]),
    ] {
        assert_eq!(at(t), want(&e), "greg @{t}");
    }
    use Op::*;
    let rows: Vec<_> = fx
        .rows(fx.w.history(fx.id("greg"), "located_in").unwrap())
        .into_iter()
        .map(|(t, o, v, _)| (t, o, v))
        .collect();
    assert_eq!(
        rows,
        vec![
            (-100, Assert, "carnath".to_string()),
            (150, Retract, "carnath".into()),
            (200, Assert, "varrow".into()),
            (250, Retract, "varrow".into()),
            (601, Assert, "ashford".into()),
        ]
    );
    assert!(fx.w.overlaps("located_in").unwrap().is_empty());
}

#[test]
fn between_the_overlap_and_its_resolution_greg_is_in_both_places() {
    let fx = Fx::load();
    for tx in [OVERLAP_WRITTEN, PREQUEL] {
        let v = fx.w.at_canon(tx);
        let at = |t: i64| fx.vals(&v.as_of(fx.id("greg"), "located_in", Tick(t)).unwrap());
        assert_eq!(at(400), want(&["varrow"]));
        assert_eq!(at(600), want(&["varrow"]));
        assert_eq!(at(601), want(&["varrow", "ashford"]), "canon {tx:?}");
        assert_eq!(at(700), want(&["varrow", "ashford"]));
        // every index says so: from the places' side too
        let inn = |p: &str, t: i64| {
            fx.set(
                v.neighbors(fx.id(p), "located_in", Direction::In, Tick(t))
                    .unwrap()
                    .into_iter()
                    .map(|x| x.0),
            )
        };
        assert!(inn("varrow", 700).contains("greg"));
        assert!(inn("ashford", 700).contains("greg"));
        assert!(!inn("ashford", 600).contains("greg"));
        let ov = v.overlaps("located_in").unwrap();
        assert_eq!(ov.len(), 1, "{ov:?}");
        assert_eq!(ov[0].entity, fx.id("greg"));
        assert_eq!(
            ov[0].during,
            Interval {
                from: Tick(601),
                until: None
            }
        );
    }
    // before the overlap was written, and after it was resolved
    let before = fx.w.at_canon(TxId(7));
    assert_eq!(
        fx.vals(
            &before
                .as_of(fx.id("greg"), "located_in", Tick(700))
                .unwrap()
        ),
        want(&["varrow"])
    );
    assert!(before.overlaps("located_in").unwrap().is_empty());
}

// ================================================================ doctor

#[test]
fn doctor_is_completely_clean_at_the_latest_canon() {
    let fx = Fx::load();
    let r = doctor(&fx.w, &DoctorOptions::default());
    assert!(r.is_clean(), "{:#?}", r.findings);
}

#[test]
fn doctor_finds_exactly_the_one_overlap_between_its_writing_and_its_resolution() {
    let fx = Fx::load();
    let greg = fx.id("greg");
    let expected = Finding::CardinalityOverlap(Overlap {
        entity: greg,
        attr: "located_in".into(),
        side: Side::Forward,
        a: Value::Ref(fx.id("varrow")),
        b: Value::Ref(fx.id("ashford")),
        during: Interval {
            from: Tick(601),
            until: None,
        },
    });
    let at = |tx: TxId| {
        doctor(
            &fx.w,
            &DoctorOptions {
                canon: Some(tx),
                ..DoctorOptions::default()
            },
        )
        .findings
    };
    for tx in 1..=12u64 {
        let f = at(TxId(tx));
        if tx == OVERLAP_WRITTEN.0 || tx == PREQUEL.0 {
            assert_eq!(f, vec![expected.clone()], "canon {tx}");
        } else {
            assert!(f.is_empty(), "canon {tx}: {f:#?}");
        }
    }
    // --since: only what changed after tx 8 is reported
    let since = doctor(
        &fx.w,
        &DoctorOptions {
            canon: Some(OVERLAP_WRITTEN),
            since: Some(TxId(7)),
        },
    );
    assert_eq!(since.findings, vec![expected.clone()]);
    let since = doctor(
        &fx.w,
        &DoctorOptions {
            canon: Some(OVERLAP_WRITTEN),
            since: Some(OVERLAP_WRITTEN),
        },
    );
    assert!(since.findings.is_empty(), "{:?}", since.findings);
}

// ================================================================ file, reopen

#[test]
fn a_file_loaded_and_reopened_gives_identical_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("varrow.kala");
    let in_memory = load_fixture(&yaml()).unwrap();
    let a = battery_of(&in_memory);
    let on_disk = load_fixture_to(&yaml(), &path).unwrap();
    let b = battery_of(&on_disk);
    let dump_disk = on_disk.index_dump();
    drop(on_disk);
    let reopened = World::open(&path).unwrap();
    assert_eq!(*reopened.recovery(), RecoveryReport::default());
    let c = battery_of(&reopened);
    assert!(a.len() > 20_000, "a broad battery: {} reads", a.len());
    assert_eq!(a, b, "memory vs file");
    assert_eq!(b, c, "file vs reopened file");
    assert_eq!(in_memory.index_dump(), dump_disk);
    assert_eq!(dump_disk, reopened.index_dump());
    let r = doctor(&reopened, &DoctorOptions::default());
    assert!(r.is_clean(), "{:#?}", r.findings);
    let r = doctor_log(&path, &DoctorOptions::default()).unwrap();
    assert!(r.is_clean(), "{:#?}", r.findings);
}

/// A broad battery of reads, as strings (deterministic order).
fn battery_of(w: &World) -> Vec<String> {
    let ticks = [
        i64::MIN,
        -401,
        -300,
        -250,
        -200,
        -150,
        -100,
        -1,
        0,
        200,
        250,
        300,
        601,
        700,
        i64::MAX,
    ];
    let mut out = Vec::new();
    let id = |h: &str| w.entity_by_handle(h).unwrap();
    let mut people: Vec<String> = CORE.iter().map(|s| s.to_string()).collect();
    people.extend(villagers());
    people.push("kael".into());
    for tx in [0u64, 1, 5, 8, 9, 10, 11, 12] {
        let v = w.at_canon(TxId(tx));
        for p in &people {
            let e = id(p);
            for attr in [
                "name",
                "alias",
                "label",
                "biography",
                "located_in",
                "ally_of",
                "present",
                "bought",
                "born_in",
            ] {
                out.push(format!("{tx} hist {p}.{attr} {:?}", v.history(e, attr)));
                for t in ticks {
                    out.push(format!(
                        "{tx} asof {p}.{attr}@{t} {:?}",
                        v.as_of(e, attr, Tick(t))
                    ));
                }
            }
        }
        for pl in PLACES {
            for t in ticks {
                out.push(format!(
                    "{tx} in {pl}@{t} {:?}",
                    v.neighbors(id(pl), "located_in", Direction::In, Tick(t))
                ));
                out.push(format!(
                    "{tx} ruled {pl}@{t} {:?}",
                    v.as_of(id(pl), "ruled_by", Tick(t))
                ));
            }
        }
        for q in [
            "edwin",
            "THE GREY KING",
            "mara voss",
            "Wren \u{c5}berg",
            "villager 03",
            "nobody",
        ] {
            for t in ticks {
                out.push(format!(
                    "{tx} ci {q}@{t} {:?}",
                    v.lookup_ci("name", q, Tick(t))
                ));
            }
        }
        for q in ["Edwin", "Mara", "Osric"] {
            out.push(format!(
                "{tx} ex {q} {:?}",
                v.lookup("name", &s(q), Tick(0))
            ));
        }
        for t in ["CHARACTER", "PLACE", "ITEM", "EVENT", "NOPE"] {
            out.push(format!(
                "{tx} type {t} {:?}",
                v.entities_of_type(t).map(|i| i.collect::<Vec<_>>())
            ));
        }
        out.push(format!("{tx} overlaps {:?}", v.overlaps("located_in")));
    }
    out.push(format!(
        "doctor {:?}",
        doctor(w, &DoctorOptions::default()).findings
    ));
    out.push(format!("latest {:?} {:?}", w.latest_tx(), w.latest_tick()));
    out
}
