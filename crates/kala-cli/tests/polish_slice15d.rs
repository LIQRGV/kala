//! PR #22 review: `kala transact --lang cypher` output.
mod common;

use common::{code, stderr, stdout};
use serde_json::Value;
use std::fs;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn transact(fx: &common::Fx, name: &str, text: &str, extra: &[&str]) -> std::process::Output {
    let input = fx.dir.path().join(name);
    fs::write(&input, text).unwrap();
    let mut args = extra.to_vec();
    args.push(input.to_str().unwrap());
    fx.run("transact", &args)
}

#[test]
fn cypher_statements_are_called_statements_and_creates_show_their_handle() {
    let fx = common::build(VARROW);
    let script = "CREATE (:CHARACTER {kala_handle:'kael1'});\n\
                  MERGE (n:CHARACTER {kala_handle:'kael2'});\n\
                  MERGE (n:CHARACTER {kala_handle:'kael2'});";
    let o = transact(&fx, "s.cypher", script, &["--lang", "cypher"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("statement 1: committed tx#"), "{out}");
    assert_eq!(
        lines[1],
        "  op 1 (:create): tick 700, 0 changed, created CHARACTER#34 kael1"
    );
    assert!(lines[2].starts_with("statement 2: committed tx#"), "{out}");
    assert_eq!(
        lines[3],
        "  op 1 (:create): tick 700, 0 changed, created CHARACTER#35 kael2"
    );
    assert_eq!(lines[4], "statement 3: nothing committed: 0 changed");
    assert_eq!(
        lines[5],
        "  op 1 (:create): tick 700, 0 changed, exists CHARACTER#35 kael2"
    );
    assert!(!out.contains("form "), "{out}");

    // JSON keeps its field names.
    let o = transact(
        &fx,
        "j.cypher",
        "CREATE (:CHARACTER {kala_handle:'kael3'});",
        &["--lang", "cypher", "--json"],
    );
    let report: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(report["form"], 1);

    // A type constraint runs as a statement of the script.
    let o = transact(
        &fx,
        "c.cypher",
        "CREATE CONSTRAINT FOR (n:CHARACTER) REQUIRE n.rank IS :: INTEGER;",
        &["--lang", "cypher"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).starts_with("statement 1: committed tx#")
            && stdout(&o).contains("0 changed, 1 declared"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn datalog_forms_keep_their_wording() {
    let fx = common::build(VARROW);
    let o = transact(&fx, "f.edn", "[:create :CHARACTER :kael1]", &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with("form 1: committed tx#"), "{out}");
    assert!(
        out.contains("  op 1 (:create): tick 700, 0 changed, created CHARACTER#34\n"),
        "{out}"
    );
}

#[test]
fn a_handle_string_for_a_ref_attribute_resolves_in_a_script() {
    let fx = common::build(VARROW);
    let o = transact(
        &fx,
        "h.cypher",
        "MATCH (w {kala_handle:'wren'}) SET w.located_in = 'varrow' AT TICK 900;",
        &["--lang", "cypher", "--json"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = transact(
        &fx,
        "h2.cypher",
        "MATCH (w {kala_handle:'wren'}) SET w.located_in = 'nowhere' AT TICK 901;",
        &["--lang", "cypher"],
    );
    assert_ne!(code(&o), 0);
    assert!(
        stderr(&o).contains("unknown entity `nowhere`"),
        "{}",
        stderr(&o)
    );
}
