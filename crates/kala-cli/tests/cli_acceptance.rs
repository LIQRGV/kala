//! Slice 7, item 9: the fixture-world acceptance suite.
//!
//! Each `acceptance/NN-name.edn` is a Datalog story question on
//! `kala-core/tests/fixtures/varrow.yaml`, run through the real `kala query`
//! binary; `NN-name.expected` is the exact stdout. The expected answers were
//! derived by hand from the YAML (the derivation is the leading `;` comment of
//! each .edn), never pasted from the binary's output.
//!
//! A header line `;; params: name=value` in an .edn passes `--param name=value`
//! (one line per parameter). `checks.edn` runs through
//! `kala doctor <world> --checks` instead.

mod common;

use common::*;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/acceptance")
}

fn params(edn: &str) -> Vec<String> {
    edn.lines()
        .filter_map(|l| l.strip_prefix(";; params:"))
        .map(|p| p.trim().to_string())
        .collect()
}

fn diff_message(cmd: &str, want: &str, got: &str, stderr: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let mut s = format!("acceptance mismatch\ncommand: {cmd}\n");
    for i in 0..w.len().max(g.len()) {
        let (a, b) = (w.get(i), g.get(i));
        if a != b {
            s.push_str(&format!(
                "line {}:\n  expected: {:?}\n  actual:   {:?}\n",
                i + 1,
                a,
                b
            ));
        }
    }
    if want.ends_with('\n') != got.ends_with('\n') {
        s.push_str("trailing newline differs\n");
    }
    if !stderr.is_empty() {
        s.push_str(&format!("stderr: {stderr}\n"));
    }
    s
}

fn check(fx: &Fx, name: &str) {
    let edn = dir().join(format!("{name}.edn"));
    let want = std::fs::read_to_string(dir().join(format!("{name}.expected")))
        .unwrap_or_else(|e| panic!("{name}.expected: {e}"));
    let text = std::fs::read_to_string(&edn).unwrap();
    let (cmd, args): (&str, Vec<String>) = if name == "checks" {
        (
            "doctor",
            vec!["--checks".into(), edn.to_str().unwrap().into()],
        )
    } else {
        let mut a = vec!["-f".to_string(), edn.to_str().unwrap().to_string()];
        for p in params(&text) {
            a.push("--param".into());
            a.push(p);
        }
        ("query", a)
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = fx.run(cmd, &refs);
    let got = stdout(&o);
    let shown = format!("kala {cmd} <varrow world> {}", args.join(" "));
    assert_eq!(code(&o), 0, "{shown}: {}", stderr(&o));
    assert!(
        got == want,
        "{}",
        diff_message(&shown, &want, &got, &stderr(&o))
    );
}

macro_rules! acceptance {
    ($($test:ident => $name:literal),* $(,)?) => {
        $(#[test] fn $test() { check(&varrow(), $name); })*
        const REGISTERED: &[&str] = &[$($name),*];
    };
}

acceptance! {
    q01_point_in_time => "01-point-in-time",
    q02_edwins_allies => "02-edwins-allies",
    q03_alliance_reachability => "03-alliance-reachability",
    q04_craftsfolk_two_bodies => "04-craftsfolk-two-bodies",
    q05_greg_location_changes => "05-greg-location-changes",
    q06_first_arrival_in_varrow => "06-first-arrival-in-varrow",
    q07_greg_draft_overlap => "07-greg-draft-overlap",
    q08_greg_draft_resolved => "08-greg-draft-resolved",
    q09_retcon_old_canon => "09-retcon-old-canon",
    q10_retcon_new_canon => "10-retcon-new-canon",
    q11_with_edwin_in_varrow => "11-with-edwin-in-varrow",
    q12_allies_lost => "12-allies-lost",
    q13_villagers_per_place => "13-villagers-per-place",
    q14_distinct_visitors_of_varrow => "14-distinct-visitors-of-varrow",
    q15_edge_property_disguised => "15-edge-property-disguised",
    q16_prequel => "16-prequel",
    q17_prequel_before_it_was_written => "17-prequel-before-it-was-written",
    q18_purchases_events => "18-purchases-events",
    q19_born_in_varrow => "19-born-in-varrow",
    q20_ruler_handover => "20-ruler-handover",
    doctor_story_checks => "checks",
}

#[test]
fn every_acceptance_file_is_registered_and_has_its_pair() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().into_string().unwrap();
            n.strip_suffix(".edn").map(str::to_string)
        })
        .collect();
    on_disk.sort();
    let mut reg: Vec<String> = REGISTERED.iter().map(|s| s.to_string()).collect();
    reg.sort();
    assert_eq!(on_disk, reg, "an .edn is missing from the acceptance! list");
    for n in &reg {
        assert!(
            dir().join(format!("{n}.expected")).is_file(),
            "{n}.expected"
        );
        let t = std::fs::read_to_string(dir().join(format!("{n}.edn"))).unwrap();
        assert!(t.starts_with(';'), "{n}.edn must start with a `;` comment");
    }
}
