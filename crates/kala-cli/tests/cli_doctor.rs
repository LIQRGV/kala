//! Slice 5 item 5: `kala doctor`. End to end on the real binary. Expected
//! findings come from `kala_core::doctor_log` on the same file (the command's
//! contract: it always goes through `doctor_log`, read-only).

mod common;
#[path = "common/damage.rs"]
mod damage;

use common::*;
use damage::*;
use kala_core::{DoctorOptions, Finding, Report, Severity, TxId, World, doctor_log};
use serde_json::{Value as Json, json};
use std::ffi::OsStr;
use std::path::Path;

/// Overlap world plus a third, unrelated transaction (so its tail can be torn
/// without losing the overlap).
const OVERLAP_THREE: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
    - { name: name, type: string }
entities:
  CHARACTER: [greg]
  PLACE: [varrow, ashford]
transactions:
  - tick: 200
    ops:
      - assert: [greg, located_in, varrow]
  - tick: 601
    ops:
      - assert: [greg, located_in, ashford]
  - tick: 5
    ops:
      - assert: [greg, name, "Greg"]
      - assert: [varrow, name, "Varrow"]
"#;

fn api(path: &Path, canon: Option<u64>, since: Option<u64>) -> Report {
    doctor_log(
        path,
        &DoctorOptions {
            canon: canon.map(TxId),
            since: since.map(TxId),
        },
    )
    .expect("doctor_log")
}

fn kind(f: &Finding) -> String {
    let d = format!("{f:?}");
    let name: String = d.chars().take_while(|c| c.is_alphanumeric()).collect();
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn kinds(r: &Report, s: Severity) -> Vec<String> {
    let mut v: Vec<String> = r
        .findings
        .iter()
        .filter(|f| f.severity() == s)
        .map(kind)
        .collect();
    v.sort();
    v
}

fn json_kinds(j: &Json, key: &str) -> Vec<String> {
    let mut v: Vec<String> = j[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} array in {j}"))
        .iter()
        .map(|f| {
            assert!(
                f["message"].as_str().is_some_and(|m| !m.is_empty()),
                "message in {f}"
            );
            f["kind"].as_str().expect("kind").to_string()
        })
        .collect();
    v.sort();
    v
}

fn doctor_cmd(p: &Path, extra: &[&str]) -> std::process::Output {
    let mut a: Vec<&OsStr> = vec![OsStr::new("doctor"), p.as_os_str()];
    a.extend(extra.iter().map(OsStr::new));
    kala(a)
}

/// Runs `doctor --json`, checks the JSON against the API report, returns it.
fn check_json_against_api(
    p: &Path,
    canon: Option<u64>,
    since: Option<u64>,
    want_exit: i32,
) -> Json {
    let mut extra: Vec<String> = vec!["--json".into()];
    if let Some(c) = canon {
        extra.extend(["--canon".into(), format!("tx#{c}")]);
    }
    if let Some(s) = since {
        extra.extend(["--since".into(), format!("tx#{s}")]);
    }
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let o = doctor_cmd(p, &refs);
    assert_eq!(code(&o), want_exit, "stderr: {}", stderr(&o));
    let j = parse_one_json(&stdout(&o));
    let rep = api(p, canon, since);
    assert_eq!(j["command"], "doctor");
    assert_eq!(j["canon"], json!(canon));
    assert_eq!(j["since"], json!(since));
    assert_eq!(j["clean"], json!(rep.is_clean()));
    assert_eq!(json_kinds(&j, "errors"), kinds(&rep, Severity::Error));
    assert_eq!(json_kinds(&j, "warnings"), kinds(&rep, Severity::Warning));
    j
}

fn norm(s: &str) -> String {
    s.to_lowercase().replace(['_', ' ', '-'], "")
}

fn expected_exit(r: &Report) -> i32 {
    i32::from(!r.errors().is_empty())
}

// ------------------------------------------------------------ clean / warnings

#[test]
fn a_clean_world_prints_clean_and_exits_0() {
    let fx = build(RESOLVED_FIXTURE);
    assert!(api(&fx.path, None, None).is_clean());
    let o = fx.run("doctor", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o).trim_end(), "clean");
    assert!(stdout(&o).ends_with('\n'));
    assert!(o.stderr.is_empty());
}

#[test]
fn a_clean_world_json() {
    let fx = build(RESOLVED_FIXTURE);
    let j = check_json_against_api(&fx.path, None, None, 0);
    assert_eq!(j["clean"], json!(true));
    assert_eq!(j["errors"], json!([]));
    assert_eq!(j["warnings"], json!([]));
    assert_eq!(j["canon"], Json::Null);
    assert_eq!(j["since"], Json::Null);
}

#[test]
fn an_unresolved_overlap_is_one_warning_naming_entity_and_attribute_exit_0() {
    let fx = build(OVERLAP_FIXTURE);
    let rep = api(&fx.path, None, None);
    assert_eq!(rep.warnings().len(), 1);
    assert!(rep.errors().is_empty());
    let o = fx.run("doctor", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    no_raw_controls(&out);
    assert!(out.lines().any(|l| l == "warnings (1):"), "{out}");
    assert!(!out.contains("errors ("), "{out}");
    assert!(!out.lines().any(|l| l == "clean"), "{out}");
    let line = out
        .lines()
        .skip_while(|l| *l != "warnings (1):")
        .nth(1)
        .expect("a finding line");
    assert!(
        line.contains("greg") && line.contains("located_in"),
        "{line}"
    );
    assert!(
        norm(line).contains("cardinalityoverlap"),
        "kind named: {line}"
    );
}

#[test]
fn an_unresolved_overlap_json() {
    let fx = build(OVERLAP_FIXTURE);
    let j = check_json_against_api(&fx.path, None, None, 0);
    assert_eq!(j["clean"], json!(false));
    assert_eq!(j["errors"], json!([]));
    let w = j["warnings"].as_array().unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0]["kind"], "cardinality_overlap");
    let m = w[0]["message"].as_str().unwrap();
    assert!(m.contains("greg") && m.contains("located_in"), "{m}");
}

#[test]
fn doctor_never_changes_the_world_file() {
    let fx = build(OVERLAP_FIXTURE);
    let before = dir_snapshot(fx.dir.path());
    for extra in [
        &[][..],
        &["--json"],
        &["--canon", "tx#1"],
        &["--since", "tx#1"],
    ] {
        let o = fx.run("doctor", extra);
        assert_eq!(code(&o), 0, "{extra:?}: {}", stderr(&o));
    }
    assert_eq!(dir_snapshot(fx.dir.path()), before);
}

// ------------------------------------------------------------ varrow: canon, since

#[test]
fn varrow_latest_matches_the_api() {
    let fx = varrow();
    let rep = api(&fx.path, None, None);
    assert!(rep.errors().is_empty());
    let o = fx.run("doctor", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    let n = rep.warnings().len();
    if n == 0 {
        assert_eq!(out.trim_end(), "clean");
    } else {
        assert!(
            out.lines().any(|l| l == format!("warnings ({n}):")),
            "{out}"
        );
    }
    check_json_against_api(&fx.path, None, None, 0);
}

/// The first canon whose overlaps include greg's located_in, and the world.
fn draft_canon(fx: &Fx) -> u64 {
    let greg = fx.id("greg");
    (1..=fx.mem.latest_tx().0)
        .find(|&tx| {
            fx.mem
                .at_canon(TxId(tx))
                .overlaps("located_in")
                .unwrap()
                .iter()
                .any(|o| o.entity == greg)
        })
        .expect("varrow has a draft canon with Greg in two places")
}

#[test]
fn canon_at_the_draft_shows_greg_overlap_and_the_latest_does_not() {
    let fx = varrow();
    let draft = draft_canon(&fx);
    let latest = fx.mem.latest_tx().0;
    assert!(draft < latest);
    let at_draft = api(&fx.path, Some(draft), None);
    let at_latest = api(&fx.path, None, None);
    assert!(at_draft.warnings().len() > at_latest.warnings().len());

    let o = fx.run("doctor", &["--canon", &format!("tx#{draft}")]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    let n = at_draft.warnings().len();
    assert!(
        out.lines().any(|l| l == format!("warnings ({n}):")),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("greg") && l.contains("located_in")),
        "{out}"
    );

    let latest_out = fx.text("doctor", &[]);
    assert!(
        !latest_out
            .lines()
            .any(|l| l.contains("greg") && l.contains("located_in")),
        "{latest_out}"
    );
    // a bare number is accepted too, as for every other command
    let o2 = fx.run("doctor", &["--canon", &draft.to_string()]);
    assert_eq!(code(&o2), 0);
    assert_eq!(stdout(&o2), out);

    let j = check_json_against_api(&fx.path, Some(draft), None, 0);
    assert_eq!(j["canon"], json!(draft));
    check_json_against_api(&fx.path, Some(latest), None, 0);
}

#[test]
fn since_matches_the_api_at_several_txs() {
    let fx = varrow();
    let latest = fx.mem.latest_tx().0;
    let draft = draft_canon(&fx);
    for since in [0, draft - 1, draft, latest - 1, latest] {
        let j = check_json_against_api(&fx.path, None, Some(since), 0);
        assert_eq!(j["since"], json!(since));
    }
    // nothing changed after the latest tx
    let o = fx.run("doctor", &["--since", &format!("tx#{latest}")]);
    assert_eq!(code(&o), 0);
    assert_eq!(stdout(&o).trim_end(), "clean");
    // since the tx before the draft sees what the draft touched, at the draft canon
    let j = check_json_against_api(&fx.path, Some(draft), Some(draft - 1), 0);
    assert!(!json_kinds(&j, "warnings").is_empty(), "{j}");
    assert_eq!(j["clean"], json!(false));
    // since the draft itself: the draft's own overlap is not new
    let j2 = check_json_against_api(&fx.path, Some(draft), Some(draft), 0);
    assert!(json_kinds(&j2, "warnings").len() < json_kinds(&j, "warnings").len());
}

// ------------------------------------------------------------ damaged logs

fn last_span(bytes: &[u8]) -> Span {
    *spans(bytes).last().unwrap()
}

#[test]
fn a_torn_tail_is_an_incomplete_tail_error_exit_1_and_the_file_is_unchanged() {
    let fx = build(OVERLAP_THREE);
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let last = last_span(&bytes);
    bytes.truncate(last.start + 10);
    let (dir, p) = file_with(&bytes);
    let before = dir_snapshot(dir.path());

    let rep = api(&p, None, None);
    assert!(matches!(
        rep.errors().as_slice(),
        [Finding::IncompleteTail { offset }] if *offset == last.start as u64
    ));

    let o = doctor_cmd(&p, &[]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let out = stdout(&o);
    no_raw_controls(&out);
    let lines: Vec<&str> = out.lines().collect();
    let e = lines.iter().position(|l| *l == "errors (1):").expect(&out);
    let w = lines
        .iter()
        .position(|l| l.starts_with("warnings ("))
        .unwrap_or_else(|| panic!("the overlap warning is still reported:\n{out}"));
    assert!(e < w, "errors first:\n{out}");
    assert_eq!(lines[w], "warnings (1):");
    let err_line = lines[e + 1];
    assert!(norm(err_line).contains("incompletetail"), "{err_line}");
    assert!(
        err_line.contains(&last.start.to_string()),
        "offset: {err_line}"
    );
    assert!(lines[w + 1].contains("greg"), "{out}");

    // the open that doctor must NOT do would have cut the file and made a quarantine file
    assert_eq!(
        dir_snapshot(dir.path()),
        before,
        "bytes and directory unchanged"
    );
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
}

#[test]
fn a_torn_tail_json_has_the_error_and_the_warning() {
    let fx = build(OVERLAP_THREE);
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let last = last_span(&bytes);
    bytes.truncate(last.start + 3);
    let (dir, p) = file_with(&bytes);
    let before = dir_snapshot(dir.path());
    let j = check_json_against_api(&p, None, None, 1);
    assert_eq!(j["clean"], json!(false));
    assert_eq!(json_kinds(&j, "errors"), vec!["incomplete_tail"]);
    assert_eq!(json_kinds(&j, "warnings"), vec!["cardinality_overlap"]);
    assert_eq!(dir_snapshot(dir.path()), before);
}

#[test]
fn a_torn_varrow_is_reported_not_repaired() {
    let fx = varrow();
    for into in [3usize, 40] {
        let mut bytes = std::fs::read(&fx.path).unwrap();
        let last = last_span(&bytes);
        bytes.truncate(last.start + into);
        let (dir, p) = file_with(&bytes);
        let j = check_json_against_api(&p, None, None, 1);
        assert_eq!(json_kinds(&j, "errors"), vec!["incomplete_tail"]);
        assert_eq!(dir_names(dir.path()), vec!["w.kala".to_string()]);
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
    }
}

#[test]
fn a_flipped_byte_in_a_committed_middle_tx_is_a_checksum_error_exit_1() {
    let fx = build(OVERLAP_THREE);
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let sp = spans(&bytes);
    assert!(sp.len() >= 3);
    let victim = sp[sp.len() - 2];
    bytes[victim.body_start + 6] ^= 0x01;
    let (dir, p) = file_with(&bytes);
    let before = dir_snapshot(dir.path());
    // the world itself refuses to open: that is why doctor reads the log directly
    assert!(matches!(World::open(&p), Err(kala_core::Error::Corrupt(_))));
    assert_eq!(
        dir_snapshot(dir.path()),
        before,
        "a refused open leaves the file"
    );

    let rep = api(&p, None, None);
    assert!(
        rep.findings
            .iter()
            .any(|f| matches!(f, Finding::ChecksumMismatch { tx, .. } if tx.0 == victim.tx))
    );
    let o = doctor_cmd(&p, &[]);
    assert_eq!(code(&o), expected_exit(&rep));
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.lines().any(|l| l.starts_with("errors (")), "{out}");
    let line = out
        .lines()
        .find(|l| norm(l).contains("checksummismatch"))
        .unwrap_or_else(|| panic!("a checksum_mismatch line:\n{out}"));
    assert!(line.contains(&format!("tx#{}", victim.tx)), "{line}");
    check_json_against_api(&p, None, None, 1);
    assert_eq!(dir_snapshot(dir.path()), before);
}

#[test]
fn a_flipped_byte_in_the_last_tx_is_an_incomplete_tail_and_is_not_cut() {
    let fx = varrow();
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let last = last_span(&bytes);
    bytes[last.body_start + 6] ^= 0x01;
    let (dir, p) = file_with(&bytes);
    let before = dir_snapshot(dir.path());
    let j = check_json_against_api(&p, None, None, 1);
    assert_eq!(json_kinds(&j, "errors"), vec!["incomplete_tail"]);
    assert_eq!(
        dir_snapshot(dir.path()),
        before,
        "damaged tail not cut, no quarantine"
    );
}

#[test]
fn a_zero_filled_tail_is_an_error_too() {
    let fx = build(RESOLVED_FIXTURE);
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let end = bytes.len();
    bytes.extend_from_slice(&[0u8; 40]);
    let (dir, p) = file_with(&bytes);
    let o = doctor_cmd(&p, &[]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.lines().any(|l| l == "errors (1):"), "{out}");
    assert!(
        out.contains(&end.to_string()),
        "offset of the valid prefix:\n{out}"
    );
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
    assert_eq!(dir_names(dir.path()), vec!["w.kala".to_string()]);
}

// ------------------------------------------------------------ not a log

#[test]
fn a_non_kala_file_exits_3() {
    let (_d, p) = file_with(b"just a text file, definitely not a kala log\n");
    assert!(doctor_log(&p, &DoctorOptions::default()).is_err());
    for extra in [&[][..], &["--json"]] {
        let o = doctor_cmd(&p, extra);
        assert_eq!(code(&o), 3, "{}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
        assert!(!stderr(&o).contains("not implemented"));
        assert!(o.stdout.is_empty(), "{}", stdout(&o));
    }
}

#[test]
fn an_unsupported_version_and_a_cut_header_exit_3() {
    let fx = build(RESOLVED_FIXTURE);
    let bytes = std::fs::read(&fx.path).unwrap();
    let mut v = bytes.clone();
    v[8..12].copy_from_slice(&999u32.to_le_bytes());
    let cut = bytes[..5].to_vec();
    for (what, b) in [("version", v), ("header", cut)] {
        let (_d, p) = file_with(&b);
        assert!(doctor_log(&p, &DoctorOptions::default()).is_err(), "{what}");
        let o = doctor_cmd(&p, &[]);
        assert_eq!(code(&o), 3, "{what}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
        assert!(!stderr(&o).contains("not implemented"));
    }
}

#[test]
fn a_missing_file_exits_1_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.kala");
    let o = doctor_cmd(&p, &[]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
    assert!(!stderr(&o).contains("not implemented"));
    assert!(o.stdout.is_empty());
    assert!(dir_names(dir.path()).is_empty());
}

#[test]
fn error_messages_escape_control_characters_in_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a\u{1b}[31mb");
    let o = doctor_cmd(&p, &[]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
    assert!(!stderr(&o).contains("not implemented"));
    no_raw_controls(&stderr(&o));
}

#[test]
fn bad_canon_argument_is_a_usage_error() {
    let fx = build(RESOLVED_FIXTURE);
    let o = fx.run("doctor", &["--canon", "banana"]);
    assert_eq!(code(&o), 2);
    // and a good doctor still works on the same file afterwards
    assert_eq!(code(&fx.run("doctor", &[])), 0);
}
