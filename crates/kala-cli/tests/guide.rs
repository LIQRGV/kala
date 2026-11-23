//! The guide mirrors these real-binary examples and their checked answers.
mod common;
use common::*;
use serde_json::Value as Json;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

const EXAMPLES: &[(&str, &str)] = &[
    (
        include_str!("acceptance/01-point-in-time.edn"),
        include_str!("acceptance/01-point-in-time.expected"),
    ),
    (
        include_str!("acceptance/02-edwins-allies.edn"),
        include_str!("acceptance/02-edwins-allies.expected"),
    ),
    (
        include_str!("acceptance/03-alliance-reachability.edn"),
        include_str!("acceptance/03-alliance-reachability.expected"),
    ),
    (
        include_str!("acceptance/05-greg-location-changes.edn"),
        include_str!("acceptance/05-greg-location-changes.expected"),
    ),
    (
        include_str!("acceptance/07-greg-draft-overlap.edn"),
        include_str!("acceptance/07-greg-draft-overlap.expected"),
    ),
    (
        include_str!("acceptance/09-retcon-old-canon.edn"),
        include_str!("acceptance/09-retcon-old-canon.expected"),
    ),
    (
        include_str!("acceptance/10-retcon-new-canon.edn"),
        include_str!("acceptance/10-retcon-new-canon.expected"),
    ),
    (
        include_str!("acceptance/12-allies-lost.edn"),
        include_str!("acceptance/12-allies-lost.expected"),
    ),
    (
        include_str!("acceptance/14-distinct-visitors-of-varrow.edn"),
        include_str!("acceptance/14-distinct-visitors-of-varrow.expected"),
    ),
    (
        include_str!("acceptance/11-with-edwin-in-varrow.edn"),
        include_str!("acceptance/11-with-edwin-in-varrow.expected"),
    ),
];

const SCENE: &str = "[:at 800 [:create :CHARACTER :nerys] [:assert :nerys :name \"Nerys\"] [:assert :nerys :located_in :varrow]]";
const LEAVE: &str = "[:retract :nerys :located_in :varrow :at 850]";
const KAEL_QUERY: &str = "[:find ?n ?p :at 800 :where [:nerys :name ?n] [:nerys :located_in ?p]]";

fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn guide() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../GUIDE.md"))
        .expect("GUIDE.md is the tracked story-author tutorial")
}

fn fixture_through_cli() -> Fx {
    let f = varrow();
    fs::remove_file(&f.path).unwrap();
    let o = kala(["load".as_ref(), VARROW.as_ref(), f.path.as_os_str()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    f
}

#[test]
fn ten_guide_queries_match_the_documented_varrow_answers() {
    let prose = normalized(&guide());
    let f = fixture_through_cli();
    for (source, expected) in EXAMPLES {
        let query = source
            .lines()
            .filter(|l| !l.trim_start().starts_with(';'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prose.contains(&normalized(&query)),
            "guide must mirror checked query: {query}"
        );
        assert!(
            prose.contains(&normalized(expected)),
            "guide must mirror the checked answer for: {query}"
        );
        let o = f.run("query", &[&query]);
        assert_eq!(code(&o), 0, "{query}: {}", stderr(&o));
        assert_eq!(stdout(&o), *expected, "{query}");
    }
}

#[test]
fn guide_load_scene_repl_correct_old_canon_and_doctor_work() {
    let prose = normalized(&guide());
    for command in [
        "kala load crates/kala-core/tests/fixtures/varrow.yaml varrow.kala",
        "kala transact varrow.kala scene.edn --json",
        "kala transact varrow.kala leave.edn --json",
        "kala transact varrow.kala correct.edn --json",
        "kala repl varrow.kala --write",
        "kala doctor varrow.kala",
    ] {
        assert!(prose.contains(command), "guide command missing: {command}");
    }
    for form in [SCENE, LEAVE, KAEL_QUERY] {
        assert!(
            prose.contains(&normalized(form)),
            "guide form missing: {form}"
        );
    }
    let f = fixture_through_cli();
    let scene = f.dir.path().join("scene.edn");
    fs::write(&scene, SCENE).unwrap();
    let scene_result = f.json("transact", &[scene.to_str().unwrap()]);
    assert_eq!(scene_result["tx"], 13);
    assert_eq!(scene_result["changed"], 2);
    assert_eq!(scene_result["ops"].as_array().unwrap().len(), 3);
    assert_eq!(scene_result["ops"][1]["tick"], 800);
    assert!(
        scene_result["warnings"]["introduced"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&f.path)
        .arg("--write")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{KAEL_QUERY}\n.quit\n").as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("\"Nerys\"  varrow"), "{}", stdout(&o));
    // The guide pins the original fixture's canon when exploring after
    // creating Nerys, so later world edits do not change its example answers.
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&f.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut session = String::from(".canon 12\n");
    for (source, _) in EXAMPLES {
        session.push_str(source);
        session.push('\n');
    }
    session.push_str(".quit\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(session.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        EXAMPLES
            .iter()
            .map(|(_, answer)| *answer)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let leave = f.dir.path().join("leave.edn");
    fs::write(&leave, LEAVE).unwrap();
    let j = f.json("transact", &[leave.to_str().unwrap()]);
    let entry = j["ops"][0]["entries"][0].as_u64().unwrap();
    let canon = j["tx"].as_u64().unwrap();
    assert_eq!(canon, 14);
    let correction = f.dir.path().join("correct.edn");
    fs::write(&correction, format!("[:correct {entry} :replace :at 830]")).unwrap();
    let j = f.json("transact", &[correction.to_str().unwrap()]);
    assert_eq!(j["ops"][0]["tick"], 830);
    assert_eq!(j["changed"], 2);
    let q = format!("[:find ?p :at 840 :canon {canon} :where [:nerys :located_in ?p]]");
    let before = f.json("query", &[&q]);
    assert_eq!(before["rows"][0][0]["value"]["handle"], "varrow");
    let after = f.json(
        "query",
        &["[:find ?p :at 840 :where [:nerys :located_in ?p]]"],
    );
    assert!(after["rows"].as_array().unwrap().is_empty());
    let doctor = f.run("doctor", &[]);
    assert_eq!(code(&doctor), 0, "{}", stderr(&doctor));
    let doctor_json: Json = f.json("doctor", &[]);
    assert!(doctor_json["errors"].as_array().unwrap().is_empty());
}

#[test]
fn guide_changing_schema_infers_renames_previews_and_migrates() {
    let prose = normalized(&guide());
    for command in [
        "kala transact varrow.kala age.edn",
        "kala transact varrow.kala rename-age.edn",
        "kala transact varrow.kala migrate-age.edn --dry-run",
        "kala transact varrow.kala migrate-age.edn",
        "kala schema varrow.kala",
        "kala schema varrow.kala --canon tx#12",
        "kala asof varrow.kala edwin years --at 0",
        "kala asof varrow.kala edwin years@old --at 0",
    ] {
        assert!(prose.contains(command), "guide command missing: {command}");
    }
    const AGE: &str = "[:assert :edwin :age 42 :at 0]";
    const RENAME: &str = "[:rename :age :years]";
    const MIGRATE: &str = "[:migrate :years :string]";
    for form in [AGE, RENAME, MIGRATE] {
        assert!(prose.contains(form), "guide form missing: {form}");
    }
    let f = fixture_through_cli();
    for (file, source, expected) in [
        ("age.edn", AGE, "declared :age as i64 (inferred)"),
        ("rename-age.edn", RENAME, "renamed :age to :years"),
    ] {
        let input = f.dir.path().join(file);
        fs::write(&input, source).unwrap();
        let o = f.run("transact", &[input.to_str().unwrap()]);
        assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
        assert!(stdout(&o).contains(expected), "{}", stdout(&o));
    }
    let input = f.dir.path().join("migrate-age.edn");
    fs::write(&input, MIGRATE).unwrap();
    let before = fs::read(&f.path).unwrap();
    let preview = f.run("transact", &[input.to_str().unwrap(), "--dry-run"]);
    assert_eq!(code(&preview), 0, "{}", stderr(&preview));
    assert!(
        stdout(&preview).contains("1 change points copied"),
        "{}",
        stdout(&preview)
    );
    assert_eq!(fs::read(&f.path).unwrap(), before);
    let applied = f.run("transact", &[input.to_str().unwrap()]);
    assert_eq!(code(&applied), 0, "{}", stderr(&applied));
    assert!(
        stdout(&applied).contains("1 change points copied"),
        "{}",
        stdout(&applied)
    );
    let schema = f.run("schema", &[]);
    assert_eq!(code(&schema), 0, "{}", stderr(&schema));
    assert!(stdout(&schema).contains("years@old"), "{}", stdout(&schema));
    let old_schema = f.run("schema", &["--canon", "tx#12"]);
    assert_eq!(code(&old_schema), 0, "{}", stderr(&old_schema));
    assert!(
        !stdout(&old_schema).contains("years"),
        "{}",
        stdout(&old_schema)
    );
    let current = f.run("asof", &["edwin", "years", "--at", "0"]);
    assert_eq!(code(&current), 0, "{}", stderr(&current));
    assert!(
        stdout(&current).contains("years = \"42\""),
        "{}",
        stdout(&current)
    );
    let old = f.run("asof", &["edwin", "years@old", "--at", "0"]);
    assert_eq!(code(&old), 0, "{}", stderr(&old));
    assert!(stdout(&old).contains("years@old = 42"), "{}", stdout(&old));
    assert!(
        f.json("doctor", &[])["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
