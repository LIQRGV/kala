//! Cypher over the real Bolt socket, independently decoded PackStream values.
#[path = "common/client.rs"]
mod client;
mod common;
use client::{Client, map, string, structure};
use common::Running;
use kala_bolt::packstream::Value;

#[test]
fn cypher_default_and_explicit_prefix_return_nodes_and_labels() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    for prefix in ["", "// kala:cypher\n"] {
        c.run(
            &format!("{prefix}MATCH (p {{kala_handle: 'varrow'}}) AT TICK $tick RETURN p"),
            map(&[("tick", Value::Integer(-210))]),
            map(&[]),
        );
        let rows = c.pull(-1).0;
        assert_eq!(rows.len(), 1);
        let node = structure(rows[0][0].clone(), 0x4e);
        let Value::List(labels) = &node[1] else {
            panic!("labels")
        };
        assert!(labels.contains(&string("PLACE")) && labels.contains(&string("City")));
        let Value::Map(props) = &node[2] else {
            panic!("properties")
        };
        assert_eq!(props["name"], string("Varrow"));
        assert_eq!(props["kala_handle"], string("varrow"));
    }
    assert_eq!(c.query(client::ALLIES).len(), 2);
}

#[test]
fn cypher_relationship_null_lists_order_and_slice_keep_wire_values() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let rows = c.query("MATCH (e {kala_handle: 'edwin'})-[r:present]->(p) AT TICK -240 RETURN r, r.disguised, p.name");
    assert_eq!(rows.len(), 1);
    let rel = structure(rows[0][0].clone(), 0x52);
    assert_eq!(rel.len(), 8);
    assert_eq!(rel[3], string("present"));
    assert_eq!(rel[4], map(&[("disguised", Value::Bool(true))]));
    assert_eq!(rel[5], string("edge#98"));
    assert_eq!(rows[0][1], Value::Bool(true));
    assert_eq!(rows[0][2], string("Carnath"));
    assert_eq!(
        c.query("MATCH (c {kala_handle: 'kael'}) RETURN c.biography"),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        c.query("MATCH (e {kala_handle: 'edwin'}) AT TICK -210 RETURN e.alias"),
        vec![vec![Value::List(vec![
            string("Ed"),
            string("Eddie"),
            string("the Lost Knight")
        ])]]
    );
    assert_eq!(
        c.query("MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name DESC SKIP 1 LIMIT 2"),
        vec![vec![string("Wren Åberg")], vec![string("Villager 24")]]
    );
}

#[test]
fn cypher_syntax_type_missing_parameter_and_gremlin_errors() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    for (text, params, suffix) in [
        ("MATCH (n RETURN n", map(&[]), "SyntaxError"),
        ("MATCH (n) RETURN properties(n)", map(&[]), "SyntaxError"),
        (
            "MATCH (n) AT TICK $tick RETURN n",
            map(&[]),
            "ParameterMissing",
        ),
        (
            "MATCH (n) AT TICK $tick RETURN n",
            map(&[("tick", string("wrong"))]),
            "TypeError",
        ),
        (
            "MATCH (n) RETURN n LIMIT $n",
            map(&[("n", Value::Integer(-1))]),
            "TypeError",
        ),
        (
            "MATCH (n) RETURN n SKIP $n",
            map(&[("n", string("wrong"))]),
            "TypeError",
        ),
    ] {
        c.send(0x10, vec![string(text), params, map(&[])]);
        let error = c.failure(&format!("Neo.ClientError.Statement.{suffix}"));
        if suffix == "SyntaxError" {
            assert!(
                matches!(&error["message"],Value::String(s) if s.contains("line") && s.contains("column"))
            );
        }
        c.reset();
    }
    c.send(
        0x10,
        vec![string("// kala:gremlin\ng.V()"), map(&[]), map(&[])],
    );
    let error = c.failure("Neo.ClientError.Statement.SyntaxError");
    assert!(
        matches!(&error["message"],Value::String(s) if s.contains("Gremlin is not supported yet"))
    );
    c.reset();
    assert_eq!(c.query(client::ALLIES).len(), 2);
}

#[test]
fn returned_node_uses_default_tick_but_property_uses_binding_clause_tick() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let rows = c.query("MATCH (g {kala_handle: 'greg'}) AT TICK -210 MATCH (e {kala_handle: 'edwin'}) AT TICK 700 RETURN e, e.name");
    assert_eq!(rows.len(), 1);
    let node = structure(rows[0][0].clone(), 0x4e);
    let Value::Map(props) = &node[2] else {
        panic!("properties")
    };
    assert_eq!(props["name"], string("Edwin"));
    assert_eq!(rows[0][1], string("the Grey King"));
}

#[test]
fn cypher_list_and_null_parameters_preserve_values_and_list_equality() {
    let server = Running::start();
    let mut c = Client::ready(&server);
    let aliases = Value::List(vec![
        string("Ed"),
        string("Eddie"),
        string("the Lost Knight"),
    ]);
    c.run(
        "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 WHERE e.alias = $aliases RETURN e.name",
        map(&[("aliases", aliases.clone())]),
        map(&[]),
    );
    assert_eq!(c.pull(-1).0, vec![vec![string("Edwin")]]);
    c.run(
        "MATCH (e {kala_handle: 'edwin'}) RETURN $value",
        map(&[("value", Value::Null)]),
        map(&[]),
    );
    assert_eq!(c.pull(-1).0, vec![vec![Value::Null]]);
    c.run(
        "MATCH (e {kala_handle: 'edwin'}) RETURN $value",
        map(&[("value", aliases.clone())]),
        map(&[]),
    );
    assert_eq!(c.pull(-1).0, vec![vec![aliases]]);
}

fn float_world() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: name, type: string }
    - { name: height, type: f64 }
entities:
  CHARACTER: [alice, bob, cy, di]
transactions:
  - tick: 0
    ops:
      - assert: [alice, name, Alice]
      - assert: [alice, height, 2.0]
      - assert: [bob, name, Bob]
      - assert: [bob, height, 9007199254740992.0]
      - assert: [cy, name, Cy]
      - assert: [cy, height, -9007199254740992.0]
      - assert: [di, name, Di]
      - assert: [di, height, 9007199254740994.0]
"#;
    drop(kala_fixture::load_fixture_to(yaml, &dir.path().join("typed.kala")).unwrap());
    Running::in_dir(
        dir,
        kala_bolt::ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..kala_bolt::ServerConfig::default()
        },
    )
}

const HEIGHT_Q: &str = "MATCH (a) WHERE a.height = $p RETURN a.name";

#[test]
fn cypher_f64_attribute_accepts_integer_parameter() {
    let server = float_world();
    let mut c = Client::ready(&server);
    c.run(HEIGHT_Q, map(&[("p", Value::Integer(2))]), map(&[]));
    assert_eq!(c.pull(-1).0, vec![vec![string("Alice")]]);
}

#[test]
fn cypher_f64_integer_parameter_accepts_exactly_plus_minus_2_pow_53() {
    let server = float_world();
    let mut c = Client::ready(&server);
    c.run(HEIGHT_Q, map(&[("p", Value::Integer(1 << 53))]), map(&[]));
    assert_eq!(c.pull(-1).0, vec![vec![string("Bob")]]);
    c.run(
        HEIGHT_Q,
        map(&[("p", Value::Integer(-(1 << 53)))]),
        map(&[]),
    );
    assert_eq!(c.pull(-1).0, vec![vec![string("Cy")]]);
}

#[test]
fn cypher_f64_integer_parameter_beyond_2_pow_53_is_a_type_error() {
    let server = float_world();
    let mut c = Client::ready(&server);
    for n in [
        (1i64 << 53) + 1,
        (1i64 << 53) + 2,
        -(1i64 << 53) - 1,
        i64::MAX,
        i64::MIN,
    ] {
        c.send(
            0x10,
            vec![string(HEIGHT_Q), map(&[("p", Value::Integer(n))]), map(&[])],
        );
        c.failure("Neo.ClientError.Statement.TypeError");
        c.reset();
    }
}
