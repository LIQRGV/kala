//! Slice 5 item 8: the acceptance story. An author drives the real `kala`
//! binary, only through its command line, on `varrow.yaml`: `kala load`, then
//! reads, then `doctor`. Expected answers are HARD-CODED from the story in the
//! fixture (see its comments). Each constant is also cross-checked against the
//! kala-core API on an in-memory twin, so a wrong constant fails with a clear
//! "story constant disagrees with kala-core" message instead of blaming the CLI.
//!
//! Canon numbers (tx#1 is the schema/definitions, so fixture transaction N is
//! canon N+1): the draft with Greg in Varrow AND Ashford is tx#8, the retcon is
//! tx#12, the canon just before it is tx#11. `ticks`: one tick is one year.

mod common;
#[path = "common/damage.rs"]
mod damage;

use common::*;
use damage::*;
use kala_core::{Direction, EntityId, Op, Tick, TxId, Value, World, load_fixture};
use serde_json::Value as Json;
use std::path::PathBuf;

const DRAFT: u64 = 8;
const BEFORE_RETCON: u64 = 11;
const RETCON: u64 = 12;

/// A world built by the CLI (`kala load`), plus its in-memory twin.
struct Story {
    _dir: tempfile::TempDir,
    path: PathBuf,
    twin: World,
}

fn story() -> Story {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let o = kala([
        std::ffi::OsString::from("load"),
        VARROW.into(),
        path.clone().into(),
    ]);
    assert_eq!(code(&o), 0, "kala load: {}", stderr(&o));
    let twin = load_fixture(&std::fs::read_to_string(VARROW).unwrap()).unwrap();
    Story {
        _dir: dir,
        path,
        twin,
    }
}

impl Story {
    fn out(&self, cmd: &str, rest: &[&str]) -> std::process::Output {
        let mut a: Vec<std::ffi::OsString> = vec![cmd.into(), self.path.clone().into()];
        a.extend(rest.iter().map(|s| (*s).into()));
        kala(a)
    }

    fn json(&self, cmd: &str, rest: &[&str]) -> Json {
        let mut r = rest.to_vec();
        r.push("--json");
        let o = self.out(cmd, &r);
        assert_eq!(code(&o), 0, "kala {cmd} {rest:?}: {}", stderr(&o));
        parse_one_json(&stdout(&o))
    }

    fn id(&self, h: &str) -> EntityId {
        self.twin.entity_by_handle(h).unwrap()
    }

    fn handle(&self, e: EntityId) -> String {
        self.twin
            .handle_of(e)
            .map(str::to_string)
            .unwrap_or_else(|| self.twin.format_id(e))
    }

    /// Neighbors through the CLI as sorted handles.
    fn cli_neighbors(
        &self,
        ent: &str,
        attr: &str,
        dir: &str,
        t: i64,
        canon: Option<u64>,
    ) -> Vec<String> {
        let at = format!("@{t}");
        let c = canon.map(|c| format!("tx#{c}"));
        let mut args = vec![ent, attr, dir, at.as_str()];
        if let Some(c) = &c {
            args.extend(["--canon", c.as_str()]);
        }
        let j = self.json("neighbors", &args);
        assert_eq!(j["tick"], t);
        assert_eq!(j["canon"], canon.unwrap_or(RETCON));
        let mut v: Vec<String> = j["neighbors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["entity"]["handle"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    }

    /// Neighbors through kala-core as sorted handles.
    fn api_neighbors(
        &self,
        ent: &str,
        attr: &str,
        dir: Direction,
        t: i64,
        canon: Option<u64>,
    ) -> Vec<String> {
        let v = self.twin.at_canon(TxId(canon.unwrap_or(RETCON)));
        let mut h: Vec<String> = v
            .neighbors(self.id(ent), attr, dir, Tick(t))
            .unwrap()
            .iter()
            .map(|(n, _)| self.handle(*n))
            .collect();
        h.sort();
        h
    }

    /// The hard-coded story answer, checked against the API and the CLI.
    #[allow(clippy::too_many_arguments)]
    fn neighbors_is(
        &self,
        ent: &str,
        attr: &str,
        flag: &str,
        dir: Direction,
        t: i64,
        canon: Option<u64>,
        story: &[&str],
    ) {
        let mut want: Vec<String> = story.iter().map(|s| (*s).to_string()).collect();
        want.sort();
        assert_eq!(
            self.api_neighbors(ent, attr, dir, t, canon),
            want,
            "story constant disagrees with kala-core: {ent} {attr} {flag} @{t} canon {canon:?}"
        );
        assert_eq!(
            self.cli_neighbors(ent, attr, flag, t, canon),
            want,
            "CLI answer differs from the story: kala neighbors {ent} {attr} {flag} @{t} canon {canon:?}"
        );
    }

    fn allies(&self, t: i64, canon: Option<u64>, story: &[&str]) {
        self.neighbors_is(
            "edwin",
            "ally_of",
            "--both",
            Direction::Both,
            t,
            canon,
            story,
        );
    }

    fn in_varrow(&self, t: i64, canon: Option<u64>, story: &[&str]) {
        self.neighbors_is(
            "varrow",
            "located_in",
            "--in",
            Direction::In,
            t,
            canon,
            story,
        );
    }

    /// `history` rows as `(tick, op, value)` through the CLI.
    fn cli_history(&self, ent: &str, attr: &str, canon: Option<u64>) -> Vec<(i64, String, String)> {
        let c = canon.map(|c| format!("tx#{c}"));
        let mut args = vec![ent, attr];
        if let Some(c) = &c {
            args.extend(["--canon", c.as_str()]);
        }
        let j = self.json("history", &args);
        assert_eq!(j["canon"], canon.unwrap_or(RETCON));
        j["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let v = &c["value"]["value"];
                let val = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v["handle"].as_str().unwrap().to_string());
                (
                    c["tick"].as_i64().unwrap(),
                    c["op"].as_str().unwrap().to_string(),
                    val,
                )
            })
            .collect()
    }

    fn api_history(&self, ent: &str, attr: &str, canon: Option<u64>) -> Vec<(i64, String, String)> {
        self.twin
            .at_canon(TxId(canon.unwrap_or(RETCON)))
            .history(self.id(ent), attr)
            .unwrap()
            .iter()
            .map(|c| {
                let val = match &c.value {
                    Value::Str(s) => s.clone(),
                    Value::Ref(e) => self.handle(*e),
                    other => panic!("unexpected {other:?}"),
                };
                let op = if c.op == Op::Assert {
                    "assert"
                } else {
                    "retract"
                };
                (c.tick.0, op.to_string(), val)
            })
            .collect()
    }

    fn history_is(&self, ent: &str, attr: &str, canon: Option<u64>, story: &[(i64, &str, &str)]) {
        let want: Vec<(i64, String, String)> = story
            .iter()
            .map(|(t, o, v)| (*t, (*o).to_string(), (*v).to_string()))
            .collect();
        assert_eq!(
            self.api_history(ent, attr, canon),
            want,
            "story constant disagrees with kala-core: history {ent} {attr} canon {canon:?}"
        );
        assert_eq!(
            self.cli_history(ent, attr, canon),
            want,
            "CLI answer differs from the story: kala history {ent} {attr} canon {canon:?}"
        );
    }

    /// `find` through the CLI as sorted handles.
    fn find_is(&self, value: &str, ci: bool, t: i64, story: &[&str]) {
        let at = format!("@{t}");
        let mut args = vec!["name", value, at.as_str()];
        if ci {
            args.push("-i");
        }
        let j = self.json("find", &args);
        let mut got: Vec<String> = j["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["handle"].as_str().unwrap().to_string())
            .collect();
        got.sort();
        let mut want: Vec<String> = story.iter().map(|s| (*s).to_string()).collect();
        want.sort();
        // cross-check: scan every character and place with the API
        let v = self.twin.at_canon(TxId(RETCON));
        let fold = |s: &str| s.to_lowercase();
        let mut api: Vec<String> = ["CHARACTER", "PLACE", "ITEM", "EVENT"]
            .iter()
            .flat_map(|ty| v.entities_of_type(ty).unwrap().collect::<Vec<_>>())
            .filter(|e| {
                v.as_of(*e, "name", Tick(t))
                    .unwrap_or_default()
                    .iter()
                    .any(|n| match n {
                        Value::Str(s) => {
                            if ci {
                                fold(s) == fold(value)
                            } else {
                                s == value
                            }
                        }
                        _ => false,
                    })
            })
            .map(|e| self.handle(e))
            .collect();
        api.sort();
        assert_eq!(
            api, want,
            "story constant disagrees with kala-core: find name {value:?} ci={ci} @{t}"
        );
        assert_eq!(
            got, want,
            "CLI answer differs from the story: find name {value:?} ci={ci} @{t}"
        );
    }
}

// ------------------------------------------------------------ the retcon canons

#[test]
fn the_canon_numbers_used_here_are_the_ones_in_the_log() {
    let s = story();
    assert_eq!(
        s.twin.latest_tx().0,
        RETCON,
        "the retcon is the last transaction"
    );
    let revised = |tx: u64| {
        s.twin
            .entries()
            .filter(|e| e.tx.0 == tx)
            .filter(|e| matches!(e.body, kala_core::log::EntryBody::Revised { .. }))
            .count()
    };
    assert!(revised(RETCON) > 0, "tx#{RETCON} holds the corrections");
    assert_eq!(revised(BEFORE_RETCON), 0);
    // the draft: Greg is in Varrow and Ashford at 601 only until the author ends Varrow at 250
    let greg = s.id("greg");
    let at = |c: u64| {
        s.twin
            .at_canon(TxId(c))
            .overlaps("located_in")
            .unwrap()
            .iter()
            .any(|o| o.entity == greg)
    };
    assert!(at(DRAFT) && !at(DRAFT - 1) && at(DRAFT + 1) && !at(DRAFT + 2));
}

// ------------------------------------------------------------ Edwin's allies

#[test]
fn edwins_allies_through_the_story_at_the_latest_canon() {
    let s = story();
    // Alliances (one edge each, written from either side):
    //   greg  [-320,-300) prequel, [-280,-200) after the retcon (was -190)
    //   tomas [-300, inf)      hild [-260,-240)
    //   mara  [-208,-150) and [-60, inf)       wren: struck by the retcon
    s.allies(-400, None, &[]);
    s.allies(-330, None, &[]);
    s.allies(-320, None, &["greg"]);
    s.allies(-301, None, &["greg"]);
    s.allies(-300, None, &["tomas"]); // greg's prequel alliance ended at -300
    s.allies(-290, None, &["tomas"]);
    s.allies(-280, None, &["tomas", "greg"]);
    s.allies(-260, None, &["tomas", "greg", "hild"]);
    s.allies(-241, None, &["tomas", "greg", "hild"]);
    s.allies(-240, None, &["tomas", "greg"]); // hild retracted at -240
    s.allies(-209, None, &["tomas", "greg"]);
    s.allies(-208, None, &["tomas", "greg", "mara"]);
    s.allies(-201, None, &["tomas", "greg", "mara"]);
    s.allies(-200, None, &["tomas", "mara"]); // the retcon moved greg's end here
    s.allies(-190, None, &["tomas", "mara"]);
    s.allies(-170, None, &["tomas", "mara"]); // wren never was an ally
    s.allies(-151, None, &["tomas", "mara"]);
    s.allies(-150, None, &["tomas"]);
    s.allies(-61, None, &["tomas"]);
    s.allies(-60, None, &["tomas", "mara"]); // Mara re-allies, written from her side
    s.allies(700, None, &["tomas", "mara"]);
}

#[test]
fn edwins_allies_before_the_retcon_differ_exactly_where_it_changed_things() {
    let s = story();
    let c = Some(BEFORE_RETCON);
    s.allies(-320, c, &["greg"]);
    s.allies(-280, c, &["tomas", "greg"]);
    s.allies(-208, c, &["tomas", "greg", "mara"]);
    s.allies(-200, c, &["tomas", "greg", "mara"]); // still allied: the end was -190
    s.allies(-195, c, &["tomas", "greg", "mara"]);
    s.allies(-190, c, &["tomas", "mara"]);
    s.allies(-170, c, &["tomas", "mara", "wren"]); // wren's alliance not yet struck
    s.allies(-151, c, &["tomas", "mara", "wren"]);
    s.allies(-150, c, &["tomas", "wren"]);
    s.allies(-60, c, &["tomas", "wren", "mara"]);
    s.allies(700, c, &["tomas", "wren", "mara"]);
    // before the retcon-touched range nothing differs
    s.allies(-260, c, &["tomas", "greg", "hild"]);
    s.allies(-240, c, &["tomas", "greg"]);
}

#[test]
fn the_same_tick_answers_differently_on_either_side_of_the_retcon() {
    let s = story();
    let latest = s.cli_neighbors("edwin", "ally_of", "--both", -195, None);
    let before = s.cli_neighbors("edwin", "ally_of", "--both", -195, Some(BEFORE_RETCON));
    assert_eq!(latest, ["mara", "tomas"]);
    assert_eq!(before, ["greg", "mara", "tomas"]);
    // the bidirectional edge reads the same from the other end
    let from_mara = s.cli_neighbors("mara", "ally_of", "--both", -195, None);
    assert!(from_mara.contains(&"edwin".to_string()), "{from_mara:?}");
    let from_greg = s.cli_neighbors("greg", "ally_of", "--both", -195, None);
    assert!(!from_greg.contains(&"edwin".to_string()), "{from_greg:?}");
    let from_greg_before = s.cli_neighbors("greg", "ally_of", "--both", -195, Some(BEFORE_RETCON));
    assert!(
        from_greg_before.contains(&"edwin".to_string()),
        "{from_greg_before:?}"
    );
}

// ------------------------------------------------------------ who is in Varrow

#[test]
fn who_is_in_varrow_through_the_ages() {
    let s = story();
    // osric from -400 and tomas from -350 stay; v01 [-395,-95); edwin [-300,-250);
    // hild [-270,-230); mara [-220,-190); v15 [-325,-25); v10 from -50; v24 from 20;
    // greg [200,250) once the author ends it.
    s.in_varrow(-401, None, &[]);
    s.in_varrow(-400, None, &["osric"]);
    s.in_varrow(-395, None, &["osric", "v01"]);
    s.in_varrow(-350, None, &["osric", "v01", "tomas"]);
    s.in_varrow(-300, None, &["osric", "v01", "tomas", "v15", "edwin"]);
    s.in_varrow(
        -260,
        None,
        &["osric", "v01", "tomas", "v15", "edwin", "hild"],
    );
    s.in_varrow(-250, None, &["osric", "v01", "tomas", "v15", "hild"]); // edwin left at -250
    s.in_varrow(-230, None, &["osric", "v01", "tomas", "v15"]); // hild left, mara not yet
    s.in_varrow(-220, None, &["osric", "v01", "tomas", "v15", "mara"]);
    s.in_varrow(-190, None, &["osric", "v01", "tomas", "v15"]);
    s.in_varrow(-50, None, &["osric", "tomas", "v15", "v10"]);
    s.in_varrow(0, None, &["osric", "tomas", "v10"]);
    s.in_varrow(20, None, &["osric", "tomas", "v10", "v24"]);
    s.in_varrow(200, None, &["osric", "tomas", "v10", "v24", "greg"]);
    s.in_varrow(249, None, &["osric", "tomas", "v10", "v24", "greg"]);
    s.in_varrow(250, None, &["osric", "tomas", "v10", "v24"]); // Greg left Varrow @250
    s.in_varrow(601, None, &["osric", "tomas", "v10", "v24"]);
}

#[test]
fn greg_in_two_places_is_a_draft_and_resolves_when_the_author_ends_varrow() {
    let s = story();
    // CLAUDE.md example: Varrow @200, Ashford @601, then "left Varrow @250"
    s.in_varrow(601, Some(DRAFT), &["osric", "tomas", "v10", "v24", "greg"]);
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        601,
        Some(DRAFT),
        &["varrow", "ashford"],
    );
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        601,
        Some(DRAFT + 1),
        &["varrow", "ashford"],
    ); // prequel did not fix it
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        601,
        Some(DRAFT + 2),
        &["ashford"],
    );
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        601,
        None,
        &["ashford"],
    );
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        300,
        Some(DRAFT),
        &["varrow"],
    );
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        300,
        None,
        &[],
    );
    // the prequel: Greg was in Carnath [-100,150)
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        0,
        None,
        &["carnath"],
    );
    s.neighbors_is(
        "greg",
        "located_in",
        "--out",
        Direction::Out,
        150,
        None,
        &[],
    );
    // before tx#8 Ashford was unknown
    s.in_varrow(300, Some(DRAFT), &["osric", "tomas", "v10", "v24", "greg"]);
    // Tomas ruled Varrow until -190, then Hild
    s.neighbors_is(
        "varrow",
        "ruled_by",
        "--out",
        Direction::Out,
        -191,
        None,
        &["tomas"],
    );
    s.neighbors_is(
        "varrow",
        "ruled_by",
        "--out",
        Direction::Out,
        -190,
        None,
        &["hild"],
    );
}

// ------------------------------------------------------------ find

#[test]
fn find_by_name_exact_and_case_insensitive() {
    let s = story();
    // exact bytes: only a capital-E "Edwin"
    s.find_is("Edwin", false, -200, &["edwin"]);
    s.find_is("Edwin", false, -300, &["edwin"]);
    s.find_is("Edwin", false, 299, &[]); // renamed at -180, kael not yet born
    s.find_is("Edwin", false, 300, &["kael"]); // a second Edwin: names are not IDs
    s.find_is("Edwin", false, 700, &["kael"]);
    s.find_is("edwin", false, -200, &[]); // no folding without -i
    s.find_is("edwin", true, -200, &["edwin"]);
    s.find_is("EDWIN", true, -200, &["edwin"]);
    s.find_is("edwin", true, 700, &["kael"]);
    s.find_is("the grey king", true, -180, &["edwin"]);
    s.find_is("the grey king", false, -180, &[]);
    s.find_is("the Grey King", false, 0, &["edwin"]);
    // only a case-insensitive match
    s.find_is("wren åberg", true, 0, &["wren"]);
    s.find_is("wren åberg", false, 0, &[]);
    s.find_is("Wren Åberg", false, 0, &["wren"]);
    s.find_is("MARA", true, -150, &["mara"]);
    s.find_is("MARA", true, -100, &[]); // she is "Mara Voss" from -100
    s.find_is("mara voss", true, -100, &["mara"]);
    s.find_is("VARROW", true, 0, &["varrow"]);
    // nobody before the world is named
    s.find_is("Greg", false, -401, &[]);
    s.find_is("Greg", false, -400, &["greg"]);
}

// ------------------------------------------------------------ history

#[test]
fn history_of_edwins_alliances_with_and_without_the_retcon() {
    let s = story();
    s.history_is(
        "edwin",
        "ally_of",
        None,
        &[
            (-320, "assert", "greg"),
            (-300, "retract", "greg"),
            (-300, "assert", "tomas"),
            (-280, "assert", "greg"),
            (-260, "assert", "hild"),
            (-240, "retract", "hild"),
            (-208, "assert", "mara"),
            (-200, "retract", "greg"), // moved from -190
            (-150, "retract", "mara"),
            (-60, "assert", "mara"),
        ],
    );
    s.history_is(
        "edwin",
        "ally_of",
        Some(BEFORE_RETCON),
        &[
            (-320, "assert", "greg"),
            (-300, "retract", "greg"),
            (-300, "assert", "tomas"),
            (-280, "assert", "greg"),
            (-260, "assert", "hild"),
            (-240, "retract", "hild"),
            (-208, "assert", "mara"),
            (-190, "retract", "greg"),
            (-170, "assert", "wren"), // struck by the retcon
            (-150, "retract", "mara"),
            (-60, "assert", "mara"),
        ],
    );
}

#[test]
fn history_of_edwins_aliases_shows_the_typo_fixed_only_after_the_retcon() {
    let s = story();
    s.history_is(
        "edwin",
        "alias",
        None,
        &[
            (-300, "assert", "Ed"),
            (-290, "assert", "Eddie"),
            (-250, "assert", "the Lost Knight"),
            (-180, "retract", "the Lost Knight"),
        ],
    );
    s.history_is(
        "edwin",
        "alias",
        Some(BEFORE_RETCON),
        &[
            (-300, "assert", "Ed"),
            (-290, "assert", "Eddy"),
            (-250, "assert", "the Lost Knight"),
            (-180, "retract", "the Lost Knight"),
        ],
    );
    // the prequel (tx#9) is not there at the canon just before it
    s.history_is(
        "edwin",
        "located_in",
        Some(DRAFT),
        &[
            (-300, "assert", "varrow"),
            (-250, "retract", "varrow"),
            (-250, "assert", "carnath"),
            (-180, "retract", "carnath"),
            (-180, "assert", "eldoria"),
        ],
    );
    s.history_is(
        "edwin",
        "located_in",
        None,
        &[
            (-340, "assert", "oakhollow"),
            (-300, "assert", "varrow"),
            (-300, "retract", "oakhollow"),
            (-250, "retract", "varrow"),
            (-250, "assert", "carnath"),
            (-180, "retract", "carnath"),
            (-180, "assert", "eldoria"),
        ],
    );
}

// ------------------------------------------------------------ doctor

#[test]
fn doctor_exit_codes_and_the_draft_canon() {
    let s = story();
    let o = s.out("doctor", &[]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let latest = s.json("doctor", &[]);
    assert!(latest["errors"].as_array().unwrap().is_empty(), "{latest}");
    let is_greg_overlap = |f: &Json| {
        let m = f["message"].as_str().unwrap();
        m.contains("greg") && m.contains("located_in")
    };
    assert!(
        !latest["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(is_greg_overlap),
        "{latest}"
    );

    let o = s.out("doctor", &["--canon", "tx#8", "--json"]);
    assert_eq!(code(&o), 0, "warnings alone exit 0: {}", stderr(&o));
    let draft = parse_one_json(&stdout(&o));
    assert_eq!(draft["canon"], DRAFT);
    assert!(draft["errors"].as_array().unwrap().is_empty());
    assert!(
        draft["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(is_greg_overlap),
        "the draft shows Greg in Varrow and Ashford: {draft}"
    );
    // the same through the text output
    let text = s.out("doctor", &["--canon", "tx#8"]);
    assert_eq!(code(&text), 0);
    assert!(
        stdout(&text)
            .lines()
            .any(|l| l.contains("greg") && l.contains("located_in")),
        "{}",
        stdout(&text)
    );
}

#[test]
fn doctor_on_a_world_whose_last_transaction_is_torn_exits_1() {
    let s = story();
    let mut bytes = std::fs::read(&s.path).unwrap();
    let last = *spans(&bytes).last().unwrap();
    assert_eq!(last.tx, RETCON);
    bytes.truncate(last.start + 10);
    let (_dir, p) = file_with(&bytes);
    let o = kala(["doctor".as_ref(), p.as_os_str()]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(!stdout(&o).is_empty());
    // the committed prefix is untouched by doctor
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
}

#[test]
fn doctor_on_a_file_that_is_not_a_kala_log_exits_3() {
    let (_dir, p) = file_with(b"this is the Varrow chronicle, in prose, not in a log\n");
    let o = kala(["doctor".as_ref(), p.as_os_str()]);
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(o.stdout.is_empty());
    assert!(!stderr(&o).trim().is_empty());
}

// ------------------------------------------------------------ log and info

#[test]
fn log_from_the_retcon_shows_what_it_revised() {
    let s = story();
    let o = s.out("log", &["--from", "tx#12"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    let revs: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("rev 1 of entry#"))
        .collect();
    assert!(!revs.is_empty(), "{out}");
    // greg's moved end (DELETED + new fact), wren's delete, the typo fix
    assert!(revs.iter().any(|l| l.contains("greg")), "{out}");
    assert!(revs.iter().any(|l| l.contains("wren")), "{out}");
    assert!(revs.iter().any(|l| l.contains("Eddie")), "{out}");
    // nothing earlier is listed
    assert!(!out.contains("tx#11"), "{out}");
    // the original entries it points at really exist and carry the old values
    let all = stdout(&s.out("log", &[]));
    assert!(all.contains("Eddy"), "{all}");
}

// ------------------------------------------------------------ --json everywhere

#[test]
fn every_read_command_prints_exactly_one_json_document() {
    let s = story();
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("asof", vec!["edwin", "@-200"]),
        ("asof", vec!["edwin", "alias", "@-200"]),
        ("neighbors", vec!["edwin", "ally_of", "--both", "@-200"]),
        ("history", vec!["edwin", "ally_of"]),
        ("find", vec!["name", "edwin", "-i", "@-200"]),
        ("list", vec!["CHARACTER", "@0"]),
        ("edge", vec!["edwin", "present", "carnath", "@-230"]),
        ("doctor", vec![]),
        ("log", vec![]),
        ("log", vec!["--from", "tx#12", "--limit", "1"]),
        ("info", vec![]),
    ];
    for (cmd, args) in cases {
        let mut a = args.clone();
        a.push("--json");
        let o = s.out(cmd, &a);
        assert_eq!(code(&o), 0, "kala {cmd} {args:?}: {}", stderr(&o));
        let j = parse_one_json(&stdout(&o));
        assert_eq!(j["command"], cmd, "{cmd} {args:?}");
        assert!(o.stderr.is_empty(), "{cmd}: {}", stderr(&o));
    }
}

#[test]
fn edwins_disguise_on_the_carnath_edge_changes_at_220() {
    let s = story();
    let disguised = |t: &str| -> Json {
        let j = s.json("edge", &["edwin", "present", "carnath", t]);
        j["properties"].clone()
    };
    let at = |t: &str| {
        let p = disguised(t);
        let v = &p
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["attr"] == "disguised")
            .unwrap()["values"][0]["value"]["value"];
        v.as_bool().unwrap()
    };
    assert!(at("@-230"), "disguised before -220");
    assert!(!at("@-220"), "disguise dropped at -220");
    let j = s.json("edge", &["edwin", "present", "carnath", "@-181"]);
    assert_eq!(j["holds"], true);
    let j = s.json("edge", &["edwin", "present", "carnath", "@-180"]);
    assert_eq!(j["holds"], false, "Edwin left Carnath at -180");
}
