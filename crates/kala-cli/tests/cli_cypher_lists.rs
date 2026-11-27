//! List parameters reach UNWIND through the same front-end as Bolt.
mod common;
use common::{VARROW, code, kala, stderr, stdout};

fn world() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala").to_str().unwrap().to_owned();
    let output = kala(["load", VARROW, &path]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    (dir, path)
}

#[test]
fn cli_list_parameter_unwinds_to_matching_names() {
    let (_dir, path) = world();
    let output = kala([
        "query",
        "--lang",
        "cypher",
        &path,
        "UNWIND $names AS n MATCH (c:CHARACTER {name: n}) RETURN c.kala_handle ORDER BY c.kala_handle",
        "--param",
        "names=[\"Greg\",\"Hild\",\"Nobody\"]",
        "--json",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(json["rows"].as_array().unwrap().len(), 2);
    assert_eq!(json["rows"][0][0]["value"], "greg");
    assert_eq!(json["rows"][1][0]["value"], "hild");
}

#[test]
fn cli_empty_null_nested_and_invalid_list_parameters() {
    let (_dir, path) = world();
    for value in ["xs=[]", "xs=null"] {
        let output = kala([
            "query",
            "--lang",
            "cypher",
            &path,
            "UNWIND $xs AS x RETURN x",
            "--param",
            value,
            "--json",
        ]);
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(json["rows"], serde_json::json!([]));
    }
    let output = kala([
        "query",
        "--lang",
        "cypher",
        &path,
        "UNWIND $xs AS xs UNWIND xs AS x RETURN x",
        "--param",
        "xs=[[2,1],[3]]",
        "--json",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(json["rows"].as_array().unwrap().len(), 3);
    for value in [
        "names=[\"Greg\",3]",
        "names=[\"Greg\",]",
        "names=[nan]",
        "names=[inf]",
    ] {
        let output = kala([
            "query",
            "--lang",
            "cypher",
            &path,
            "UNWIND $names AS n MATCH (c:CHARACTER {name:n}) RETURN c",
            "--param",
            value,
        ]);
        assert_ne!(code(&output), 0, "{value}: {}", stdout(&output));
    }
}

#[test]
fn cli_world_procedures_work_and_server_procedures_name_bolt() {
    let (_dir, path) = world();
    let output = kala([
        "query",
        "--lang",
        "cypher",
        &path,
        "CALL db.labels() YIELD label WHERE label = 'City' RETURN label",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("City"));
    for query in ["CALL dbms.components()", "SHOW DATABASES"] {
        let output = kala(["query", "--lang", "cypher", &path, query]);
        assert_ne!(code(&output), 0);
        assert!(
            stderr(&output).contains("only over Bolt"),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn cli_list_parameter_supports_row_bound_indices_in_read_projections() {
    let (_dir, path) = world();
    let output = kala([
        "query",
        "--lang",
        "cypher",
        &path,
        "UNWIND [1,0,-1,2] AS i RETURN $names[i] AS name",
        "--param",
        r#"names=["Aster","Briar"]"#,
        "--json",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0]["value"], "Briar");
    assert_eq!(rows[1][0]["value"], "Aster");
    assert_eq!(rows[2][0]["value"], "Briar");
    assert!(rows[3][0]["value"].is_null());
}
