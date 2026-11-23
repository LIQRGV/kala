//! Story checks share one default executor budget (M2).
mod common;
use common::*;
use kala_query::{Limits, Params};

const PRODUCT: &str = "[:find ?a ?b ?c :at 0 :where [?a :label \"Villager\"] [?b :label \"Villager\"] [?c :label \"Villager\"]]";

#[test]
fn checks_stop_once_when_the_shared_budget_is_exhausted() {
    let fx = varrow();
    let prepared = kala_datalog::prepare(&fx.mem, PRODUCT).unwrap();
    let (_, usage) =
        kala_query::run_with_usage(&fx.mem, &prepared.plan, &Params::new(), &Limits::default())
            .unwrap();
    let limit = Limits::default();
    // Steps are shared across checks; memory is released after each check,
    // so only the step budget decides how many checks complete (review round 1).
    assert!(usage.peak_bytes < limit.max_bytes);
    let completed = (limit.max_steps / usage.steps) as usize;
    assert!(
        (1..256).contains(&completed),
        "fixture must exhaust a shared budget: {usage:?}"
    );
    let text = format!(
        "[{}]",
        (0..256)
            .map(|i| format!("[:c{i:03} {PRODUCT}] "))
            .collect::<String>()
    );
    let path = fx.dir.path().join("checks.edn");
    std::fs::write(&path, text).unwrap();
    let out = fx.run("doctor", &["--checks", path.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let json = parse_one_json(&stdout(&out));
    let hints = json["hints"].as_array().unwrap();
    let story: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "story_check")
        .collect();
    let budget: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "budget_exhausted")
        .collect();
    let truncated: Vec<_> = hints
        .iter()
        .filter(|h| h["kind"] == "output_truncated")
        .collect();
    assert_eq!(story.len(), completed, "usage={usage:?}");
    assert_eq!(budget.len(), 1, "one hint for all checks not run");
    assert_eq!(truncated.len(), 1, "one hint for the total output cap");
    assert_eq!(hints.len(), completed + 2);
    assert_eq!(budget[0]["check"], format!("c{completed:03}"));
    assert_eq!(truncated[0]["check"], "c100");
    assert!(
        stdout(&out).contains(&format!(
            "story checks stopped: budget exhausted at :c{completed:03}"
        )),
        "{}",
        stdout(&out)
    );
    for (i, hint) in story.iter().enumerate() {
        assert_eq!(hint["check"], format!("c{i:03}"));
        assert_eq!(hint["rows_total"], 24usize.pow(3));
        assert_eq!(
            hint["rows"].as_array().unwrap().len(),
            if i < 100 { 100 } else { 0 }
        );
    }
}

#[test]
fn json_preserves_exact_count_but_prints_only_one_hundred_rows() {
    let fx = varrow();
    let path = fx.dir.path().join("checks.edn");
    std::fs::write(&path, format!("[[:product {PRODUCT}]]")).unwrap();
    let out = fx.run("doctor", &["--checks", path.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let json = parse_one_json(&stdout(&out));
    assert_eq!(json["hints"][0]["rows_total"], 24usize.pow(3));
    assert_eq!(json["hints"][0]["rows"].as_array().unwrap().len(), 100);
}

#[test]
fn checks_share_steps_even_when_the_byte_budget_is_not_exhausted() {
    let fx = varrow();
    let query = format!(
        "{} {}]",
        &PRODUCT[..PRODUCT.len() - 1],
        "[(= ?c ?c)] ".repeat(30)
    );
    let prepared = kala_datalog::prepare(&fx.mem, &query).unwrap();
    let limits = Limits::default();
    let (_, usage) =
        kala_query::run_with_usage(&fx.mem, &prepared.plan, &Params::new(), &limits).unwrap();
    let completed = (limits.max_steps / usage.steps) as usize;
    assert!((1..64).contains(&completed), "{usage:?}");
    assert!(
        (completed + 1) * usage.peak_bytes < limits.max_bytes,
        "steps, not bytes: {usage:?}"
    );
    let path = fx.dir.path().join("steps.edn");
    let text = format!(
        "[{}]",
        (0..64)
            .map(|i| format!("[:s{i:03} {query}] "))
            .collect::<String>()
    );
    std::fs::write(&path, text).unwrap();
    let out = fx.run("doctor", &["--checks", path.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let json = parse_one_json(&stdout(&out));
    assert_eq!(json["hints"].as_array().unwrap().len(), completed + 1);
    assert!(
        stdout(&out).contains(&format!(
            "story checks stopped: budget exhausted at :s{completed:03}"
        )),
        "{}",
        stdout(&out)
    );
}
