//! Polish fix H (CLI): `kala explain --lang cypher` prints stable text with no
//! control characters or Rust Debug output.
mod common;
use common::{VARROW, code, kala, stderr, stdout};

fn world() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala").to_str().unwrap().to_owned();
    let out = kala(["load", VARROW, &path]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    (dir, path)
}

#[test]
fn guard_cypher_explain_with_order_by_has_no_nul_or_control_bytes() {
    let (_dir, path) = world();
    let out = kala([
        "explain",
        "--lang",
        "cypher",
        &path,
        "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name DESC, c.alias LIMIT 3",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(!out.stdout.contains(&0), "NUL byte in {:?}", stdout(&out));
    assert!(
        out.stdout.iter().all(|&b| b == b'\n' || b >= b' '),
        "control byte in {:?}",
        stdout(&out)
    );
}

#[test]
fn cypher_explain_renders_generated_variables_and_rows_stably() {
    let (_dir, path) = world();
    let out = kala([
        "explain",
        "--lang",
        "cypher",
        &path,
        "MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name DESC LIMIT 3",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        text,
        "Find ?c.name  at latest  canon latest\n  Output ?#1  order ?#1 desc  limit 3\n    Row bind ?#1 = ?c.name@latest\n      Row nodes ?c :CHARACTER @latest\n        Unit\n"
    );
}

#[test]
fn cypher_explain_does_not_print_rust_debug() {
    let (_dir, path) = world();
    let out = kala([
        "explain",
        "--lang",
        "cypher",
        &path,
        "MATCH (c:CHARACTER)-[r:present]->(p) WHERE p.name <> 'x' RETURN c, r ORDER BY p.name",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    for debug in [
        "OutputSpec",
        "Var(",
        "Bind {",
        "Select(",
        "Nodes {",
        "Edge {",
    ] {
        assert!(!text.contains(debug), "{debug} in {text}");
    }
}

/// Guard: the Datalog explain through the CLI is unchanged.
#[test]
fn guard_datalog_explain_through_the_cli_is_unchanged() {
    let (_dir, path) = world();
    let out = kala([
        "explain",
        &path,
        "[:find ?a :at -210 :where [?e :kala_handle \"edwin\"] [?e :ally_of ?a]]",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "Find ?a  at -210  canon latest\n  Project ?a\n    Expand [?e :ally_of ?a] @latest via E\n      Expand [?e :kala_handle \"edwin\"] via handle\n        Unit\n"
    );
}
