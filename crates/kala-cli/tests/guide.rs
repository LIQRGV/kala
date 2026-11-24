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

#[test]
fn guide_backing_up_and_moving_a_world_replays_identical_bytes() {
    let prose = guide();
    for command in [
        "kala dump world.kala -o world.dump.edn",
        "kala transact --create moved.kala world.dump.edn",
        "kala export world.kala --at -208 -o snapshot.json",
    ] {
        assert!(
            prose.contains(command),
            "guide is missing tested command: {command}"
        );
    }
    let f = fixture_through_cli();
    let dump = f.dir.path().join("world.dump.edn");
    let moved = f.dir.path().join("moved.kala");
    let snapshot = f.dir.path().join("snapshot.json");
    let o = f.run("dump", &["-o", dump.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = kala([
        "transact",
        "--create",
        moved.to_str().unwrap(),
        dump.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(fs::read(&f.path).unwrap(), fs::read(moved).unwrap());
    let o = f.run(
        "export",
        &["--at", "-208", "-o", snapshot.to_str().unwrap()],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let j: Json = serde_json::from_slice(&fs::read(snapshot).unwrap()).unwrap();
    assert_eq!(j["tick"], -208);
}

#[test]
fn guide_cypher_examples_match_the_fixture() {
    let text = normalized(&guide());
    let f = fixture_through_cli();
    for (query, expected) in [
        (
            "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -210 RETURN DISTINCT a",
            vec!["greg", "tomas"],
        ),
        (
            "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name LIMIT 3",
            vec!["Edwin", "Greg", "Hild"],
        ),
    ] {
        assert!(
            text.contains(&normalized(query)),
            "missing checked Cypher example: {query}"
        );
        let o = f.run("query", &["--lang", "cypher", query]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        let output = stdout(&o);
        let mut last = 0;
        for name in expected {
            let at = output[last..]
                .find(name)
                .unwrap_or_else(|| panic!("{name} absent: {output}"));
            last += at + name.len();
        }
    }
}

#[test]
fn guide_cypher_aggregate_and_bounded_path_examples_match_the_fixture() {
    let text = normalized(&guide());
    let f = fixture_through_cli();
    let aggregate =
        "MATCH (e {kala_handle: 'edwin'})-[:ally_of]->(a) AT TICK -208 RETURN count(*) AS allies";
    let path = "MATCH p = (e {kala_handle: 'edwin'})-[:ally_of*1..2]-(a) AT TICK -208 RETURN a.kala_handle, length(p) ORDER BY a.kala_handle";
    for query in [aggregate, path] {
        assert!(
            text.contains(&normalized(query)),
            "missing checked Cypher example: {query}"
        );
    }
    let aggregate = f.json("query", &["--lang", "cypher", aggregate]);
    assert_eq!(aggregate["rows"][0][0]["value"], 3);
    let paths = f.json("query", &["--lang", "cypher", path]);
    assert_eq!(paths["rows"].as_array().unwrap().len(), 3);
    for (row, name) in paths["rows"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["greg", "mara", "tomas"])
    {
        assert_eq!(row[0]["value"], name);
        assert_eq!(row[1]["value"], 1);
    }
}

#[test]
fn guide_set_move_and_if_missing_script_are_rerunnable() {
    const MOVE: &str = "[:set :greg :located_in :ashford :at 225]";
    const SCRIPT: &str = "[:at 800 [:create :CHARACTER :kael2 :if-missing] [:assert :kael2 :name \"Kael\"] [:assert :kael2 :located_in :varrow]]";
    let f = fixture_through_cli();
    let move_file = f.dir.path().join("move.edn");
    fs::write(&move_file, MOVE).unwrap();
    let result = f.json("transact", &[move_file.to_str().unwrap()]);
    assert_eq!(result["changed"], 2);
    assert!(
        result["warnings"]["introduced"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let script_file = f.dir.path().join("rerunnable.edn");
    fs::write(&script_file, SCRIPT).unwrap();
    let first = f.json("transact", &[script_file.to_str().unwrap()]);
    assert_eq!(first["changed"], 2);
    let before = fs::read(&f.path).unwrap();
    let second = f.json("transact", &[script_file.to_str().unwrap()]);
    assert_eq!(second["changed"], 0);
    assert_eq!(fs::read(&f.path).unwrap(), before);
    let prose = normalized(&guide());
    for source in [MOVE, SCRIPT] {
        assert!(
            prose.contains(&normalized(source)),
            "guide form missing: {source}"
        );
    }
    for command in [
        "kala transact varrow.kala move.edn",
        "kala transact varrow.kala rerunnable.edn",
    ] {
        assert!(prose.contains(command), "guide command missing: {command}");
    }
}

/// "Two terminals, one world": a write session stays open in one process
/// while `kala transact` commits from another; each sees the other's
/// commits. (What a conflict prints is pinned in `src/cmd/transact.rs`:
/// two processes can't be made to collide on cue.)
#[test]
fn guide_two_terminals_share_one_world() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Duration;
    const ARRIVAL: &str =
        "[:at 900 [:create :CHARACTER :ilse] [:assert :ilse :located_in :varrow]]";
    const WHERE: &str = "[:find ?p :at 900 :where [:ilse :located_in ?p]]";
    const MOVE: &str = "[:set :ilse :located_in :ashford :at 910]";
    const WEAK: &str = "[[:concurrency :weak] [:set :ilse :located_in :carnath :at 920]]";
    let prose = normalized(&guide());
    assert!(prose.contains("## Two terminals, one world"));
    for form in [ARRIVAL, WHERE, MOVE, WEAK] {
        assert!(
            prose.contains(&normalized(form)),
            "guide form missing: {form}"
        );
    }
    for text in [
        "kala repl varrow.kala --write",
        "kala transact varrow.kala arrival.edn",
        "kala asof varrow.kala ilse located_in @910",
        "kala transact varrow.kala script.edn --concurrency weak",
        "`form 1: committed tx#13: 1 changed`",
        "`2 changed` at tx#14",
        "`.concurrency weak`",
        "--busy-timeout 30s",
        "exits with code 4",
        // The message the session prints for a conflict, as transact.rs pins it.
        "error: conflict: op 1 (:set) read :ilse :located_in, which another transaction \
         changed; nothing was written; run it again",
        "warning: latest tick moved 900 → 910 while this transaction ran",
    ] {
        assert!(prose.contains(text), "guide text missing: {text}");
    }

    let f = fixture_through_cli();
    // Terminal 1: a write session that stays open.
    let mut session = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&f.path)
        .arg("--write")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = session.stdin.take().unwrap();
    let output = session.stdout.take().unwrap();
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    // Types `text` in the session and, given `want`, reads its output up to
    // that line.
    let mut say = |text: &str, want: Option<&str>| {
        writeln!(input, "{text}").unwrap();
        input.flush().unwrap();
        while let Some(want) = want {
            let line = lines
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| panic!("`{text}`: no `{want}` in the output: {seen:?}"));
            let found = line == want;
            seen.push(line);
            if found {
                break;
            }
        }
    };

    // Terminal 2 commits while the session is open and idle.
    let arrival = f.dir.path().join("arrival.edn");
    fs::write(&arrival, ARRIVAL).unwrap();
    let o = f.run("transact", &[arrival.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).starts_with("form 1: committed tx#13: 1 changed\n"),
        "{}",
        stdout(&o)
    );
    // The session sees it on its next command, then writes itself.
    say(WHERE, Some("  varrow"));
    say(MOVE, Some("form 1: committed tx#14: 2 changed"));
    // Terminal 2 reads the session's commit at once.
    let held = f.text("asof", &["ilse", "located_in", "@910"]);
    assert!(
        held.contains("ashford") && !held.contains("varrow"),
        "{held}"
    );

    // Weak mode: for a session, a script, or one transaction.
    say(".concurrency weak", None);
    say(".concurrency", Some("concurrency: weak"));
    let script = f.dir.path().join("script.edn");
    fs::write(&script, WEAK).unwrap();
    let o = f.run(
        "transact",
        &[script.to_str().unwrap(), "--concurrency", "weak"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).starts_with("form 1: committed tx#15: 2 changed\n"),
        "{}",
        stdout(&o)
    );
    say(
        "[:find ?p :at 920 :where [:ilse :located_in ?p]]",
        Some("  carnath"),
    );
    say(".quit", None);
    let o = session.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = f.run("doctor", &[]);
    assert_eq!(code(&o), 0, "{}", stdout(&o));
}
