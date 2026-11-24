//! Slice 14 polish: a MATCH on a variable that WITH, UNWIND, YIELD or an
//! OPTIONAL MATCH bound to NULL (or to something that is not a node) matches
//! nothing instead of failing or keeping the row, and such variables still
//! get index seeks.
use kala_core::{Value, World};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, QueryError, QueryParams, run};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
fn world() -> World {
    load_fixture(VARROW).unwrap()
}
fn try_query(world: &World, text: &str) -> Result<Vec<Vec<Datum>>, QueryError> {
    let p = kala_cypher::prepare(world, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(world, &p.plan, &Params::new(), &Limits::default()).map(|r| r.rows)
}
fn query(world: &World, text: &str) -> Vec<Vec<Datum>> {
    try_query(world, text).unwrap_or_else(|e| panic!("{text}: {e}"))
}
fn explain(world: &World, text: &str) -> String {
    kala_cypher::prepare(world, text)
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .plan
        .explain(world)
}
fn string(value: &str) -> Datum {
    Value::Str(value.into()).into()
}
fn int(value: i64) -> Datum {
    Value::I64(value).into()
}
fn pairs(rows: &[(&str, &str)]) -> Vec<Vec<Datum>> {
    rows.iter()
        .map(|(a, b)| vec![string(a), string(b)])
        .collect()
}
fn sorted(mut rows: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    rows.sort();
    rows
}

// At tick -208 four characters have no `located_in` (NULL), 29 have one, and
// five of those are in Varrow, the only place with a ruler (Tomas).

/// Finding 1: a NULL start matches nothing; it is not a type error.
#[test]
fn a_null_start_from_with_or_unwind_matches_nothing() {
    let world = world();
    // An edge from a WITH expression that is NULL for some rows.
    assert_eq!(
        sorted(query(
            &world,
            "MATCH (c:CHARACTER) AT TICK -208 WITH c, c.located_in AS p MATCH (p)-[:ruled_by]->(r) RETURN c.kala_handle, r.kala_handle"
        )),
        pairs(&[
            ("mara", "tomas"),
            ("osric", "tomas"),
            ("tomas", "tomas"),
            ("v01", "tomas"),
            ("v15", "tomas"),
        ])
    );
    // The review's repro: at -400 every `p` but one is NULL and nobody rules.
    assert_eq!(
        query(
            &world,
            "MATCH (c:CHARACTER) AT TICK -400 WITH c.located_in AS p MATCH (p)-[:ruled_by]->(r) RETURN count(*)"
        ),
        vec![vec![int(0)]]
    );
    // A handle seek on a variable that is NULL for some rows.
    assert_eq!(
        query(
            &world,
            "MATCH (c:CHARACTER) AT TICK -400 WITH c.located_in AS p MATCH (p {kala_handle:'varrow'}) RETURN count(*)"
        ),
        vec![vec![int(1)]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (c:CHARACTER) AT TICK -208 WITH c.located_in AS p MATCH (p {kala_handle:'varrow'}) RETURN count(*)"
        ),
        vec![vec![int(5)]]
    );
    // An unwound NULL.
    assert!(
        query(
            &world,
            "UNWIND [null] AS n MATCH (n)-[:ally_of]->(m) RETURN m"
        )
        .is_empty()
    );
    assert_eq!(
        query(
            &world,
            "MATCH (a {kala_handle:'edwin'}) WITH a UNWIND [a, null] AS n MATCH (n)-[:located_in]->(x) RETURN x.kala_handle"
        ),
        vec![vec![string("eldoria")]]
    );
    // The incoming side.
    assert!(
        query(
            &world,
            "UNWIND [null] AS n MATCH (m)-[:ally_of]->(n) RETURN m"
        )
        .is_empty()
    );
    // OPTIONAL MATCH keeps the row, as before.
    assert_eq!(
        query(
            &world,
            "UNWIND [null] AS n OPTIONAL MATCH (n)-[:ally_of]->(m) RETURN n, m"
        ),
        vec![vec![Datum::Null, Datum::Null]]
    );
}

/// Finding 2: a required MATCH drops a row whose already-bound variable is
/// NULL, and refuses a value that is not a node.
#[test]
fn a_bound_null_fails_a_required_match_and_a_non_node_is_a_type_error() {
    let world = world();
    assert!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle:'nobody'}) MATCH (c) RETURN c"
        )
        .is_empty()
    );
    assert!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle:'nobody'}) MATCH p = (c) RETURN p"
        )
        .is_empty()
    );
    assert_eq!(
        query(
            &world,
            "MATCH (c:CHARACTER) AT TICK -400 WITH c.located_in AS p MATCH (p) RETURN count(*), count(p)"
        ),
        vec![vec![int(1), int(1)]]
    );
    assert_eq!(
        query(
            &world,
            "MATCH (c:CHARACTER) AT TICK -208 WITH c.located_in AS p MATCH (p) RETURN count(*), count(p)"
        ),
        vec![vec![int(29), int(29)]]
    );
    // An OPTIONAL MATCH on the NULL variable still keeps the row.
    assert_eq!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle:'nobody'}) OPTIONAL MATCH (c) RETURN c"
        ),
        vec![vec![Datum::Null]]
    );
    // A matched optional variable passes a later required MATCH.
    assert_eq!(
        query(
            &world,
            "OPTIONAL MATCH (c {kala_handle:'edwin'}) MATCH (c) RETURN c.kala_handle"
        ),
        vec![vec![string("edwin")]]
    );
    // Not a node: a string, an integer, a path.
    for text in [
        "MATCH (c:CHARACTER) WITH c.name AS p MATCH (p) RETURN p",
        "MATCH (c:CHARACTER) WITH c.name AS p MATCH (p:CHARACTER) RETURN p",
        "MATCH (c:CHARACTER) WITH c.name AS p MATCH (p)-[:ally_of]->(x) RETURN p",
        "MATCH (c:CHARACTER) WITH c.name AS p MATCH (p)-[:ally_of]-(x) RETURN p",
        "MATCH (c:CHARACTER) WITH c.name AS p MATCH (p {kala_handle:'edwin'}) RETURN p",
        "UNWIND [1] AS x MATCH (x) RETURN x",
        "CALL db.labels() YIELD label MATCH (label) RETURN label",
        "CALL db.labels() YIELD label MATCH (label)-[:ally_of]->(x) RETURN label",
        "MATCH p = (a {kala_handle:'edwin'}) MATCH (p) RETURN a",
        "MATCH p = (a {kala_handle:'edwin'}) MATCH (p)-[:ally_of]->(b) RETURN a",
    ] {
        match try_query(&world, text) {
            Err(QueryError::TypeMismatch(m)) => {
                assert!(
                    m.starts_with("a node pattern needs a node, got "),
                    "{text}: {m}"
                )
            }
            other => panic!("{text}: expected a type error, got {other:?}"),
        }
    }
    assert_eq!(
        try_query(
            &world,
            "MATCH (c:CHARACTER) WITH c.name AS p MATCH (p) RETURN p"
        )
        .unwrap_err()
        .to_string(),
        "type mismatch: a node pattern needs a node, got string"
    );
}

/// Finding 3: `{kala_handle: h}` with `h` bound by UNWIND or WITH is a
/// handle seek, not a scan of every node per value.
#[test]
fn a_handle_from_a_bound_variable_is_a_seek() {
    let world = world();
    let mut handles: Vec<Datum> = (0..2_500).map(|i| string(&format!("nobody-{i}"))).collect();
    handles[10] = string("edwin");
    handles[900] = string("varrow");
    handles[901] = string("edwin");
    handles[2_499] = string("greg");
    let mut params = QueryParams::new();
    params.insert("hs".into(), Datum::List(handles));
    let run_with = |text: &str, params: &QueryParams| {
        let p = kala_cypher::prepare(&world, text).unwrap_or_else(|e| panic!("{text}: {e}"));
        run(&world, &p.plan, params, &Limits::default()).map(|r| r.rows)
    };
    assert_eq!(
        run_with(
            "UNWIND $hs AS h MATCH (c {kala_handle: h}) RETURN count(*)",
            &params
        )
        .unwrap(),
        vec![vec![int(4)]]
    );
    assert_eq!(
        run_with(
            "UNWIND $hs AS h WITH h AS k MATCH (c:CHARACTER {kala_handle: k}) RETURN c.kala_handle, count(*) ORDER BY c.kala_handle",
            &params
        )
        .unwrap(),
        vec![vec![string("edwin"), int(2)], vec![string("greg"), int(1)]]
    );
    // The plan seeks: no node scan, no handle comparison per node.
    let text = explain(
        &world,
        "UNWIND $hs AS h MATCH (c {kala_handle: h}) RETURN count(*)",
    );
    assert_eq!(
        text,
        "Find ?count(*)  in $hs  at latest  canon latest\n  Output ?#3\n    Row check $hs :string\n      Row group  ?#3 = count(*)\n        Row seek ?c by handle ?#2\n          Row unwind $hs as ?#2\n            Row bind ?#1 = 0\n              Unit\n"
    );
    // A parameter list keeps its existing rule: every element must be a string.
    params.insert("hs".into(), Datum::List(vec![string("edwin"), int(3)]));
    assert_eq!(
        run_with(
            "UNWIND $hs AS h MATCH (c {kala_handle: h}) RETURN count(*)",
            &params
        )
        .unwrap_err()
        .to_string(),
        "type mismatch: parameter list elements must be string"
    );
    // Any other variable: NULL or a non-string equals no handle.
    assert_eq!(
        query(
            &world,
            "UNWIND [null, 1, true, 'edwin', 'nobody'] AS h MATCH (c {kala_handle: h}) RETURN c.name"
        ),
        vec![vec![string("the Grey King")]]
    );
    // A node that is already bound is compared, not looked up again.
    assert_eq!(
        query(
            &world,
            "MATCH (c:PLACE) UNWIND ['varrow', 'edwin', null] AS h MATCH (c {kala_handle: h}) RETURN c.kala_handle"
        ),
        vec![vec![string("varrow")]]
    );
}

/// Finding 4: an edge from an UNWIND or YIELD variable seeks from it.
#[test]
fn an_edge_from_an_unwound_variable_is_a_seek() {
    let world = world();
    let text = "MATCH p=(e {kala_handle:'edwin'})-[:ally_of*1..1]-(b) AT TICK -208 UNWIND nodes(p) AS n MATCH (n)-[:located_in]->(x) RETURN n.kala_handle, x.kala_handle";
    // Edwin three times (once per path), Mara and Tomas; Greg is nowhere.
    assert_eq!(
        sorted(query(&world, text)),
        pairs(&[
            ("edwin", "carnath"),
            ("edwin", "carnath"),
            ("edwin", "carnath"),
            ("mara", "varrow"),
            ("tomas", "varrow"),
        ])
    );
    // The unwound value is checked to be a node, then the edge seeks from it.
    assert_eq!(
        explain(&world, text),
        "Find ?n.kala_handle ?x.kala_handle  at -208  canon latest\n  Output ?#6 ?#7\n    Row bind ?#7 = ?x.kala_handle@latest\n      Row bind ?#6 = ?#5.kala_handle@latest\n        Expand [?#5 :located_in ?x] @latest via E\n          Row nodes ?#5 @latest\n            Row unwind nodes(?#4) as ?#5\n              Row bind ?#4 = path(?e, ?#2)\n                Row bind ?#3 = relationships(?#2)\n                  Row trail [?e :ally_of*1..1 ?b] as ?#2 either @latest\n                    Expand [?e :kala_handle \"edwin\"] via handle\n                      Row bind ?#1 = 0\n                        Unit\n"
    );
    // The incoming direction seeks through the reverse index.
    let plan = explain(
        &world,
        "UNWIND [null] AS n MATCH (m)-[:ally_of]->(n) RETURN m",
    );
    assert!(
        plan.contains("Expand [?m :ally_of ?#2] @latest via R\n      Row nodes ?#2 @latest\n"),
        "{plan}"
    );
}

/// Finding 5: a relationship taken from a list is usable as a relationship
/// variable; it is compared with the relationships the pattern finds.
#[test]
fn an_unwound_relationship_can_be_a_relationship_variable() {
    let world = world();
    assert_eq!(
        sorted(query(
            &world,
            "MATCH (a {kala_handle:'edwin'})-[rs:ally_of*1..2]-(b) AT TICK -208 WITH rs UNWIND rs AS r MATCH (x)-[r:ally_of]-(y) RETURN x.kala_handle, y.kala_handle"
        )),
        pairs(&[
            ("edwin", "greg"),
            ("edwin", "mara"),
            ("edwin", "tomas"),
            ("greg", "edwin"),
            ("mara", "edwin"),
            ("tomas", "edwin"),
        ])
    );
    // With an end bound it is one seek per row.
    assert_eq!(
        sorted(query(
            &world,
            "MATCH (a {kala_handle:'edwin'})-[rs:ally_of*1..2]-(b) AT TICK -208 WITH a, rs UNWIND rs AS r MATCH (a)-[r:ally_of]-(y) RETURN a.kala_handle, y.kala_handle"
        )),
        pairs(&[("edwin", "greg"), ("edwin", "mara"), ("edwin", "tomas")])
    );
    // A WITH expression, and a variable-length pattern reusing a list.
    assert_eq!(
        sorted(query(
            &world,
            "MATCH (a {kala_handle:'edwin'})-[rs:ally_of*1..1]-(b) AT TICK -208 WITH a, [rs, rs] AS both UNWIND both AS again MATCH (a)-[again:ally_of*1..1]-(y) RETURN a.kala_handle, y.kala_handle"
        )),
        pairs(&[
            ("edwin", "greg"),
            ("edwin", "greg"),
            ("edwin", "mara"),
            ("edwin", "mara"),
            ("edwin", "tomas"),
            ("edwin", "tomas"),
        ])
    );
    // NULL, a node or a scalar is no relationship: nothing matches.
    for text in [
        "UNWIND [null] AS r MATCH (x)-[r:ally_of]-(y) AT TICK -208 RETURN x",
        "UNWIND [1, 'a'] AS r MATCH (x)-[r:ally_of]-(y) AT TICK -208 RETURN x",
        "MATCH (a {kala_handle:'edwin'}) UNWIND [a] AS r MATCH (a)-[r:ally_of]-(y) AT TICK -208 RETURN y",
    ] {
        assert!(query(&world, text).is_empty(), "{text}");
    }
    // A variable known to be a node (or a relationship) still cannot switch.
    for text in [
        "MATCH (a {kala_handle:'edwin'}) MATCH (x)-[a:ally_of]-(y) RETURN x",
        "MATCH (a {kala_handle:'edwin'}) WITH a MATCH (x)-[a:ally_of]-(y) RETURN x",
        "MATCH (a)-[r:ally_of]-(b) WITH r MATCH (r) RETURN r",
        "OPTIONAL MATCH (a {kala_handle:'nobody'}) MATCH (x)-[a:ally_of]-(y) RETURN x",
    ] {
        let e = kala_cypher::prepare(&world, text).unwrap_err();
        assert!(
            e.to_string()
                .contains("a variable cannot be both a node and a relationship"),
            "{text}: {e}"
        );
    }
}
