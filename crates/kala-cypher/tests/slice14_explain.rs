//! Stable public explain text for every new row operator.
use kala_fixture::load_fixture;

#[test]
fn new_row_operator_explain_text_is_stable_and_escaped() {
    let world = load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml")).unwrap();
    let cases = [
        (
            "optional",
            "OPTIONAL MATCH (a:Absent) RETURN a",
            "Find ?a  at latest  canon latest\n  Output ?a\n    Optional\n      Unit\n      Row nodes ?a :Absent @latest\n        Unit\n",
        ),
        (
            "group",
            "MATCH (a:CHARACTER) RETURN count(*)",
            "Find ?count(*)  at latest  canon latest\n  Output ?#1\n    Row group  ?#1 = count(*)\n      Row nodes ?a :CHARACTER @latest\n        Unit\n",
        ),
        (
            "project",
            "MATCH (a:CHARACTER) WITH a AS b RETURN b",
            "Find ?b  at latest  canon latest\n  Output ?#3\n    Row bind ?#3 = ?#2\n      Row project Output ?#2\n        Row bind ?#2 = ?a\n          Row nodes ?a :CHARACTER @latest\n            Row bind ?#1 = 0\n              Unit\n",
        ),
        (
            "unwind",
            "UNWIND [2, 1] AS x RETURN x",
            "Find ?x  at latest  canon latest\n  Output ?#3\n    Row bind ?#3 = ?#2\n      Row unwind [2, 1] as ?#2\n        Row bind ?#1 = 0\n          Unit\n",
        ),
        (
            "trail",
            "MATCH p = (a)-[:ally_of*0..2]-(b) RETURN p",
            "Find ?p  at latest  canon latest\n  Output ?#4\n    Row bind ?#4 = ?#3\n      Row bind ?#3 = path(?a, ?#1)\n        Row bind ?#2 = relationships(?#1)\n          Row trail [?a :ally_of*0..2 ?b] as ?#1 either @latest\n            Row nodes ?a @latest\n              Unit\n",
        ),
        (
            "source",
            "CALL db.labels()",
            "Find ?label  at latest  canon latest\n  Output ?#3\n    Row bind ?#3 = ?#2\n      Row source labels ?#2 @latest\n        Row bind ?#1 = 0\n          Unit\n",
        ),
    ];
    let mut differences = String::new();
    for (name, query, expected) in cases {
        let actual = kala_cypher::prepare(&world, query)
            .unwrap()
            .plan
            .explain(&world);
        assert!(
            actual.chars().all(|c| c == '\n' || !c.is_control()),
            "{actual:?}"
        );
        for debug in [
            "Var(",
            "RowAction",
            "OutputSpec",
            "BagAggregate",
            "Property {",
        ] {
            assert!(!actual.contains(debug), "{debug} in {actual}");
        }
        if actual != expected {
            differences.push_str(&format!("EXPLAIN {name}\n{actual}END EXPLAIN\n"));
        }
    }
    assert!(differences.is_empty(), "{differences}");
}
