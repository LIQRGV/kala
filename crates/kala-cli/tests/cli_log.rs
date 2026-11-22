//! Slice 5 item 6: `kala log` and `kala info`, plus exit 3 on a file that is
//! not a Kala log. End to end on the real binary. Expected answers come from
//! `World::open(path).entries()` on the same file.

mod common;
#[path = "common/damage.rs"]
mod damage;

use common::*;
use damage::*;
use kala_core::log::{EntryBody, LOG_FORMAT_VERSION};
use kala_core::{AttrId, CutCause, EntityId, Op, Precision, Value, World};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, HashMap};

// ------------------------------------------------------------ expectations

struct Log {
    w: World,
    attrs: HashMap<AttrId, String>,
    by_tx: BTreeMap<u64, Vec<kala_core::world::EntryInfo>>,
}

fn open_log(fx: &Fx) -> Log {
    let w = World::open(&fx.path).expect("open");
    let mut attrs = HashMap::new();
    let mut by_tx: BTreeMap<u64, Vec<_>> = BTreeMap::new();
    for e in w.entries() {
        if let EntryBody::DefineAttr { id, def } = &e.body {
            attrs.insert(*id, def.name.clone());
        }
        by_tx.entry(e.tx.0).or_default().push(e);
    }
    Log { w, attrs, by_tx }
}

impl Log {
    fn label(&self, e: EntityId) -> String {
        self.w
            .handle_of(e)
            .map(str::to_string)
            .unwrap_or_else(|| self.w.format_id(e))
    }
    fn ent(&self, e: EntityId) -> Json {
        let handle = self.w.handle_of(e).map(str::to_string);
        json!({"id": self.w.format_id(e), "handle": handle, "name": null, "label": self.label(e)})
    }
    fn val(&self, v: &Value) -> Json {
        match v {
            Value::Bool(b) => json!({"type": "bool", "value": b}),
            Value::I64(i) => json!({"type": "i64", "value": i}),
            Value::F64(f) => json!({"type": "f64", "value": f}),
            Value::Str(s) => json!({"type": "string", "value": s}),
            Value::Ref(e) => json!({"type": "ref", "value": self.ent(*e)}),
        }
    }
    fn val_text(&self, v: &Value) -> String {
        match v {
            Value::Bool(b) => b.to_string(),
            Value::I64(i) => i.to_string(),
            Value::F64(f) => f.to_string(),
            Value::Str(s) => s.chars().take(20).collect(),
            Value::Ref(e) => self.label(*e),
        }
    }
    fn selected(&self, from: Option<u64>, limit: Option<usize>) -> Vec<u64> {
        self.by_tx
            .keys()
            .copied()
            .filter(|t| from.is_none_or(|f| *t >= f))
            .take(limit.unwrap_or(usize::MAX))
            .collect()
    }
}

fn prec(p: Precision) -> &'static str {
    prec_str(p)
}

fn op_str(o: Op) -> &'static str {
    match o {
        Op::Assert => "assert",
        Op::Retract => "retract",
    }
}

/// `needle` occurs in `line` and is not followed by a digit (so `entry#5`
/// doesn't match `entry#50`).
fn has_tok(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(i) = line[from..].find(needle) {
        let end = from + i + needle.len();
        if !line[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            return true;
        }
        from = from + i + 1;
    }
    false
}

fn lead_num(s: &str, prefix: &str) -> Option<u64> {
    let rest = s.trim_start().strip_prefix(prefix)?;
    let d: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}

/// Checks the text output of `kala log` against the API for the selected txs.
fn check_text(l: &Log, out: &str, sel: &[u64]) {
    no_raw_controls(out);
    let mut cur: Option<u64> = None;
    let mut seen_tx = Vec::new();
    let mut seen_entries: Vec<(u64, u64)> = Vec::new(); // (tx, entry)
    for line in out.lines() {
        if let Some(n) = lead_num(line, "tx#") {
            seen_tx.push(n);
            cur = Some(n);
        } else if let Some(id) = lead_num(line, "entry#") {
            let tx = cur.expect("an entry line before any tx header");
            let exp = l.by_tx[&tx]
                .iter()
                .find(|e| e.id.0 == id)
                .unwrap_or_else(|| panic!("entry#{id} is not in tx#{tx}: {line}"));
            check_entry_line(l, &exp.body, exp.revision(), line);
            seen_entries.push((tx, id));
        }
    }
    assert_eq!(seen_tx, sel, "transaction headers:\n{out}");
    let want: Vec<(u64, u64)> = sel
        .iter()
        .flat_map(|t| {
            l.by_tx[t]
                .iter()
                .filter(|e| !matches!(e.body, EntryBody::Commit { .. }))
                .map(move |e| (*t, e.id.0))
        })
        .collect();
    let got: Vec<(u64, u64)> = seen_entries
        .iter()
        .copied()
        .filter(|(t, id)| {
            !matches!(
                l.by_tx[t].iter().find(|e| e.id.0 == *id).unwrap().body,
                EntryBody::Commit { .. }
            )
        })
        .collect();
    assert_eq!(got, want, "entries (commit lines may be omitted):\n{out}");
    // entry order is increasing across the whole output
    assert!(seen_entries.windows(2).all(|w| w[0].1 < w[1].1));
}

fn check_entry_line(l: &Log, body: &EntryBody, rev: Option<(kala_core::EntryId, u32)>, line: &str) {
    let must = |s: &str| assert!(line.contains(s), "{s:?} missing in {line:?}");
    match body {
        EntryBody::Commit { .. } => {}
        EntryBody::DefineNodeType { name, .. } => {
            must("define");
            must(name);
        }
        EntryBody::DefineAttr { def, .. } => {
            must("define");
            must(&def.name);
        }
        EntryBody::DefineEntity { id, handle } => {
            must("define entity");
            must(&l.w.format_id(*id));
            if let Some(h) = handle {
                must(h);
            }
        }
        EntryBody::DefineEdge {
            id,
            source,
            attr,
            target,
        } => {
            must("define edge");
            must(&l.w.format_id(EntityId::from(*id)));
            must(&l.label(*source));
            must(&l.attrs[attr]);
            must(&l.label(*target));
        }
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
        } => {
            must(op_str(*op));
            must(&l.label(*entity));
            must(&l.attrs[attr]);
            must(&l.val_text(value));
            assert!(has_tok(line, &format!("@{}", tick.0)), "tick in {line:?}");
            if *precision != Precision::Exact {
                must(&format!("({})", prec(*precision)));
            } else {
                assert!(!line.contains("(exact)"), "{line:?}");
            }
        }
        EntryBody::Deleted {
            entity,
            attr,
            value,
            tick,
            ..
        } => {
            must("DELETED");
            let (of, r) = rev.unwrap();
            assert!(
                has_tok(line, &format!("rev {r} of entry#{}", of.0)),
                "{line:?}"
            );
            must(&l.label(*entity));
            must(&l.attrs[attr]);
            must(&l.val_text(value));
            assert!(has_tok(line, &format!("@{}", tick.0)), "tick in {line:?}");
        }
        EntryBody::Revised {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
            ..
        } => {
            let (of, r) = rev.unwrap();
            assert!(
                has_tok(line, &format!("rev {r} of entry#{}", of.0)),
                "{line:?}"
            );
            must(op_str(*op));
            must(&l.label(*entity));
            must(&l.attrs[attr]);
            must(&l.val_text(value));
            assert!(has_tok(line, &format!("@{}", tick.0)), "tick in {line:?}");
            if *precision != Precision::Exact {
                must(&format!("({})", prec(*precision)));
            }
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

fn id_of(j: &Json) -> String {
    match j {
        Json::String(s) => s.clone(),
        o => o["id"]
            .as_str()
            .expect("id string or entity object")
            .to_string(),
    }
}

/// Subset check of one JSON entry against its expectation.
fn check_json_entry(l: &Log, e: &kala_core::world::EntryInfo, j: &Json) {
    assert_eq!(j["entry"], json!(e.id.0), "{j}");
    let kind = j["kind"].as_str().unwrap_or_else(|| panic!("kind in {j}"));
    let rev = e.revision();
    match &e.body {
        EntryBody::Commit { .. } => assert_eq!(kind, "commit"),
        EntryBody::DefineNodeType { id, name } => {
            assert_eq!(kind, "define_node_type");
            assert_eq!(j["id"], json!(id.0));
            assert_eq!(j["name"], json!(name));
        }
        EntryBody::DefineAttr { id, def } => {
            assert_eq!(kind, "define_attr");
            assert_eq!(j["id"], json!(id.0));
            assert_eq!(j["name"], json!(def.name));
        }
        EntryBody::DefineEntity { id, handle } => {
            assert_eq!(kind, "define_entity");
            assert_eq!(id_of(&j["id"]), l.w.format_id(*id));
            assert_eq!(j["handle"], json!(handle));
        }
        EntryBody::DefineEdge {
            id,
            source,
            attr,
            target,
        } => {
            assert_eq!(kind, "define_edge");
            assert_eq!(id_of(&j["edge"]), l.w.format_id(EntityId::from(*id)));
            assert_eq!(j["source"], l.ent(*source));
            assert_eq!(j["attr"], json!(l.attrs[attr]));
            assert_eq!(j["target"], l.ent(*target));
        }
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
        } => {
            assert_eq!(kind, "fact");
            assert_eq!(j["entity"], l.ent(*entity));
            assert_eq!(j["attr"], json!(l.attrs[attr]));
            assert_eq!(j["value"], l.val(value));
            assert_eq!(j["tick"], json!(tick.0));
            assert_eq!(j["precision"], json!(prec(*precision)));
            assert_eq!(j["op"], json!(op_str(*op)));
        }
        EntryBody::Deleted {
            entity,
            attr,
            value,
            tick,
            ..
        } => {
            assert_eq!(kind, "deleted");
            let (of, r) = rev.unwrap();
            assert_eq!(j["of"], json!(of.0));
            assert_eq!(j["rev"], json!(r));
            assert_eq!(j["entity"], l.ent(*entity));
            assert_eq!(j["attr"], json!(l.attrs[attr]));
            assert_eq!(j["value"], l.val(value));
            assert_eq!(j["tick"], json!(tick.0));
        }
        EntryBody::Revised {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
            ..
        } => {
            assert_eq!(kind, "revised");
            let (of, r) = rev.unwrap();
            assert_eq!(j["of"], json!(of.0));
            assert_eq!(j["rev"], json!(r));
            assert_eq!(j["entity"], l.ent(*entity));
            assert_eq!(j["attr"], json!(l.attrs[attr]));
            assert_eq!(j["value"], l.val(value));
            assert_eq!(j["tick"], json!(tick.0));
            assert_eq!(j["precision"], json!(prec(*precision)));
            assert_eq!(j["op"], json!(op_str(*op)));
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

fn check_json(l: &Log, j: &Json, sel: &[u64]) {
    assert_eq!(j["command"], "log");
    let txs = j["transactions"].as_array().expect("transactions array");
    let got: Vec<u64> = txs.iter().map(|t| t["tx"].as_u64().unwrap()).collect();
    assert_eq!(got, sel);
    let mut total = 0;
    for (t, tj) in sel.iter().zip(txs) {
        let want = &l.by_tx[t];
        let es = tj["entries"].as_array().expect("entries array");
        assert_eq!(es.len(), want.len(), "entry count of tx#{t}");
        for (e, ej) in want.iter().zip(es) {
            check_json_entry(l, e, ej);
        }
        total += es.len();
    }
    let api_total: usize = sel.iter().map(|t| l.by_tx[t].len()).sum();
    assert_eq!(total, api_total);
}

// ------------------------------------------------------------ log: varrow

#[test]
fn log_text_lists_every_transaction_and_entry_of_varrow() {
    let fx = varrow();
    let l = open_log(&fx);
    let out = fx.text("log", &[]);
    let all = l.selected(None, None);
    assert!(all.len() >= 11, "varrow has its 11 author edits");
    check_text(&l, &out, &all);
}

#[test]
fn log_json_has_every_entry_with_the_contract_fields() {
    let fx = varrow();
    let l = open_log(&fx);
    let j = fx.json("log", &[]);
    check_json(&l, &j, &l.selected(None, None));
    // every kind of the contract occurs in varrow
    let kinds: std::collections::BTreeSet<String> = j["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["entries"].as_array().unwrap().clone())
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    for k in [
        "define_node_type",
        "define_attr",
        "define_entity",
        "define_edge",
        "fact",
        "deleted",
        "revised",
        "commit",
    ] {
        assert!(kinds.contains(k), "kind {k} missing, got {kinds:?}");
    }
}

#[test]
fn the_retcon_transaction_shows_rev_1_of_the_corrected_entries() {
    let fx = varrow();
    let l = open_log(&fx);
    let last = *l.by_tx.keys().next_back().unwrap();
    let corrections: Vec<_> = l.by_tx[&last]
        .iter()
        .filter_map(|e| e.revision().map(|(of, r)| (e.id.0, of.0, r)))
        .collect();
    assert!(
        corrections.len() >= 4,
        "moved tick = 2 entries, delete, value edit"
    );
    assert!(corrections.iter().all(|(_, _, r)| *r == 1));
    // the base of the moved-tick correction is "retract edwin ally_of greg @-190"
    let bases: Vec<u64> = corrections.iter().map(|c| c.1).collect();
    let base_is_greg_end = bases.iter().any(|b| {
        l.by_tx.values().flatten().any(|e| {
            e.id.0 == *b
                && matches!(&e.body, EntryBody::Fact { op: Op::Retract, tick, entity, value, .. }
                    if tick.0 == -190
                        && ((l.label(*entity) == "edwin" && *value == Value::Ref(fx.id("greg")))
                            || (l.label(*entity) == "greg" && *value == Value::Ref(fx.id("edwin")))))
        })
    });
    assert!(base_is_greg_end, "a correction points at the -190 retract");

    let out = fx.text("log", &["--from", &format!("tx#{last}")]);
    check_text(&l, &out, &[last]);
    for (_, of, r) in &corrections {
        assert!(
            has_tok(&out, &format!("rev {r} of entry#{of}")),
            "rev {r} of entry#{of} missing:\n{out}"
        );
    }
    assert!(out.contains("DELETED"), "{out}");
    let j = fx.json("log", &["--from", &format!("tx#{last}")]);
    check_json(&l, &j, &[last]);
}

#[test]
fn from_selects_transactions_at_or_after_n() {
    let fx = varrow();
    let l = open_log(&fx);
    let all = l.selected(None, None);
    for from in [all[0], all[4], all[all.len() - 1]] {
        let sel = l.selected(Some(from), None);
        assert_eq!(sel[0], from);
        let out = fx.text("log", &["--from", &format!("tx#{from}")]);
        check_text(&l, &out, &sel);
        check_json(
            &l,
            &fx.json("log", &["--from", &format!("tx#{from}")]),
            &sel,
        );
    }
}

#[test]
fn limit_shows_at_most_n_transactions_from_the_start() {
    let fx = varrow();
    let l = open_log(&fx);
    for n in [1usize, 3] {
        let sel = l.selected(None, Some(n));
        assert_eq!(sel.len(), n);
        check_text(&l, &fx.text("log", &["--limit", &n.to_string()]), &sel);
        check_json(&l, &fx.json("log", &["--limit", &n.to_string()]), &sel);
    }
    // a limit past the end shows everything
    let all = l.selected(None, None);
    check_text(&l, &fx.text("log", &["--limit", "100000"]), &all);
}

#[test]
fn from_and_limit_combine() {
    let fx = varrow();
    let l = open_log(&fx);
    let all = l.selected(None, None);
    let from = all[2];
    let sel = l.selected(Some(from), Some(2));
    assert_eq!(sel.len(), 2);
    let args = ["--from", &format!("tx#{from}"), "--limit", "2"];
    check_text(&l, &fx.text("log", &args), &sel);
    check_json(&l, &fx.json("log", &args), &sel);
}

#[test]
fn log_shows_precision_and_negative_ticks_of_a_small_world() {
    let fx = build(OVERLAP_FIXTURE);
    let l = open_log(&fx);
    let out = fx.text("log", &[]);
    check_text(&l, &out, &l.selected(None, None));
    assert!(
        out.lines().any(|x| x.contains("assert")
            && x.contains("greg")
            && x.contains("located_in")
            && x.contains("varrow")
            && x.contains("@200")),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|x| x.contains("define entity") && x.contains("greg")),
        "{out}"
    );
}

#[test]
fn log_json_is_one_document_and_log_is_read_only() {
    let fx = varrow();
    let before = std::fs::read(&fx.path).unwrap();
    let o = fx.run("log", &["--json"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    parse_one_json(&stdout(&o));
    assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    assert_eq!(dir_names(fx.dir.path()), vec!["w.kala".to_string()]);
}

// ------------------------------------------------------------ info

fn count_of(l: &Log, f: impl Fn(&EntryBody) -> bool) -> usize {
    l.by_tx.values().flatten().filter(|e| f(&e.body)).count()
}

fn tokens(line: &str) -> Vec<String> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '#'))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A line mentioning `label` (case-insensitive, `_` as space) and the number `n`.
fn has_stat(out: &str, label: &str, n: impl ToString) -> bool {
    let n = n.to_string();
    out.lines().any(|l| {
        let low = l.to_lowercase().replace('_', " ");
        low.contains(label) && tokens(l).iter().any(|t| *t == n || *t == format!("tx#{n}"))
    })
}

#[test]
fn info_json_reports_version_latest_counts_and_a_clean_recovery() {
    let fx = varrow();
    let l = open_log(&fx);
    let j = fx.json("info", &[]);
    assert_eq!(j["command"], "info");
    let p = j["path"].as_str().expect("path");
    assert!(p.ends_with("w.kala"), "{p}");
    assert_eq!(j["format_version"], json!(LOG_FORMAT_VERSION));
    assert_eq!(j["latest_tx"], json!(l.w.latest_tx().0));
    assert_eq!(j["latest_tick"], json!(l.w.latest_tick().0));
    let c = &j["counts"];
    assert_eq!(
        c["node_types"],
        json!(count_of(&l, |b| matches!(
            b,
            EntryBody::DefineNodeType { .. }
        )))
    );
    assert_eq!(
        c["attributes"],
        json!(count_of(&l, |b| matches!(b, EntryBody::DefineAttr { .. })))
    );
    assert_eq!(
        c["entities"],
        json!(count_of(&l, |b| matches!(
            b,
            EntryBody::DefineEntity { .. }
        )))
    );
    assert_eq!(
        c["edges"],
        json!(count_of(&l, |b| matches!(b, EntryBody::DefineEdge { .. })))
    );
    assert_eq!(
        c["facts"],
        json!(count_of(&l, |b| matches!(
            b,
            EntryBody::Fact { .. } | EntryBody::Revised { .. }
        )))
    );
    assert_eq!(
        c["transactions"],
        json!(count_of(&l, |b| matches!(b, EntryBody::Commit { .. })))
    );
    assert!(c["entities"].as_u64().unwrap() > 40 && c["edges"].as_u64().unwrap() > 0);
    assert_eq!(
        j["recovery"],
        json!({"bytes_cut": 0, "cause": null, "quarantine": null})
    );
}

#[test]
fn info_text_shows_the_same_numbers() {
    let fx = varrow();
    let l = open_log(&fx);
    let out = fx.text("info", &[]);
    no_raw_controls(&out);
    assert!(
        out.contains(fx.path.to_str().unwrap()) || out.contains("w.kala"),
        "{out}"
    );
    assert!(has_stat(&out, "format", LOG_FORMAT_VERSION), "{out}");
    assert!(has_stat(&out, "latest tx", l.w.latest_tx().0), "{out}");
    assert!(has_stat(&out, "latest tick", l.w.latest_tick().0), "{out}");
    let j = fx.json("info", &[]);
    for (label, key) in [
        ("node type", "node_types"),
        ("attribute", "attributes"),
        ("entit", "entities"),
        ("edge", "edges"),
        ("fact", "facts"),
        ("transaction", "transactions"),
    ] {
        assert!(
            has_stat(&out, label, j["counts"][key].as_u64().unwrap()),
            "{label}:\n{out}"
        );
    }
    assert!(
        !out.contains("acknowledge"),
        "no hint on a clean log:\n{out}"
    );
}

/// Truncates a copy of varrow's file inside the last transaction.
fn torn_copy(fx: &Fx, into: usize) -> (tempfile::TempDir, std::path::PathBuf, usize, Vec<u8>) {
    let bytes = std::fs::read(&fx.path).unwrap();
    let last = *spans(&bytes).last().unwrap();
    let cut = &bytes[..last.start + into];
    let (d, p) = file_with(cut);
    (d, p, last.start, cut.to_vec())
}

// Decision (PM, slice 5 follow-up): `info` is read-only like every read
// command. It reports what the next open for writing WOULD cut: `bytes_cut`
// and `cause` as before, `quarantine: null` (nothing is written), and the
// file and its directory stay as they were.
#[test]
fn info_on_a_torn_log_reports_the_cut_and_leaves_the_file_alone() {
    let fx = varrow();
    let (dir, p, keep, torn) = torn_copy(&fx, 40);
    let dir_before = dir_snapshot(dir.path());
    let o = kala([OsStr::new("info"), p.as_os_str(), OsStr::new("--json")]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let j = parse_one_json(&stdout(&o));
    let r = &j["recovery"];
    assert_eq!(r["bytes_cut"], json!(torn.len() - keep));
    assert_eq!(r["cause"], json!("torn"));
    assert_eq!(r["quarantine"], json!(null), "nothing is saved by a read");
    assert_eq!(std::fs::read(&p).unwrap(), torn, "file not cut by info");
    assert_eq!(dir_snapshot(dir.path()), dir_before, "no quarantine file");
    assert_eq!(dir_names(dir.path()).len(), 1);
    // the world ends one tx earlier (what a normal open would give)
    let (_d2, p2) = file_with(&torn);
    let w = World::open(&p2).unwrap();
    assert_eq!(j["latest_tx"], json!(w.latest_tx().0));
}

#[test]
fn info_text_on_a_torn_log_names_the_cut_without_an_acknowledge_hint() {
    let fx = varrow();
    let (dir, p, keep, torn) = torn_copy(&fx, 40);
    let o = kala([OsStr::new("info"), p.as_os_str()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.to_lowercase().contains("torn"), "{out}");
    assert!(out.contains(&(torn.len() - keep).to_string()), "{out}");
    assert!(
        out.to_lowercase().contains("would"),
        "the tail would be cut: {out}"
    );
    assert!(
        !out.contains(".torn-"),
        "no quarantine path: nothing was saved:\n{out}"
    );
    assert!(!out.contains("acknowledge"), "{out}");
    assert_eq!(std::fs::read(&p).unwrap(), torn);
    assert_eq!(dir_names(dir.path()).len(), 1);
}

/// A copy whose last transaction is fully present with a flipped body byte
/// (cut by open as Damaged).
fn damaged_copy(fx: &Fx) -> (tempfile::TempDir, std::path::PathBuf, usize) {
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let last = *spans(&bytes).last().unwrap();
    bytes[last.body_start + 6] ^= 0x01;
    let (d, p) = file_with(&bytes);
    // precondition, on a scratch copy: the library calls this a Damaged cut
    let (_d2, p2) = file_with(&bytes);
    let w = World::open(&p2).expect("open of a damaged last tx recovers");
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    (d, p, last.start)
}

#[test]
fn info_on_a_damaged_cut_says_damaged_and_hints_to_acknowledge() {
    let fx = varrow();
    let (dir, p, keep) = damaged_copy(&fx);
    let total = std::fs::metadata(&p).unwrap().len() as usize;
    let o = kala([OsStr::new("info"), p.as_os_str(), OsStr::new("--json")]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let j = parse_one_json(&stdout(&o));
    assert_eq!(j["recovery"]["cause"], json!("damaged"));
    assert_eq!(j["recovery"]["bytes_cut"], json!(total - keep));
    assert_eq!(
        j["recovery"]["quarantine"],
        json!(null),
        "info saves nothing"
    );
    assert_eq!(dir_names(dir.path()).len(), 1, "no quarantine file");
    assert_eq!(
        std::fs::metadata(&p).unwrap().len() as usize,
        total,
        "not cut"
    );

    let (_d2, p2, _) = damaged_copy(&fx);
    let o = kala([OsStr::new("info"), p2.as_os_str()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.to_lowercase().contains("damaged"), "{out}");
    assert!(out.contains("acknowledge"), "hint to acknowledge:\n{out}");
    assert!(out.to_lowercase().contains("would"), "would be cut:\n{out}");
    assert!(!out.contains(".torn-"), "{out}");
}

#[test]
fn info_on_a_corrupt_world_exits_3() {
    let fx = varrow();
    let mut bytes = std::fs::read(&fx.path).unwrap();
    let mid = spans(&bytes).iter().find(|s| s.tx == 3).copied().unwrap();
    bytes[mid.body_start + 6] ^= 0x01;
    let (_d, p) = file_with(&bytes);
    assert!(matches!(World::open(&p), Err(kala_core::Error::Corrupt(_))));
    for cmd in ["info", "log"] {
        let o = kala([OsStr::new(cmd), p.as_os_str()]);
        assert_eq!(code(&o), 3, "{cmd}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
        assert!(!stderr(&o).contains("not implemented"));
        assert!(o.stdout.is_empty());
    }
    assert_eq!(std::fs::read(&p).unwrap(), bytes, "corrupt file untouched");
}

#[test]
fn info_and_log_on_a_missing_world_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.kala");
    for cmd in ["info", "log"] {
        let o = kala([OsStr::new(cmd), p.as_os_str()]);
        assert_eq!(code(&o), 1, "{cmd}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
        assert!(!stderr(&o).contains("not implemented"));
        assert!(o.stdout.is_empty());
    }
    assert!(!p.exists(), "nothing created");
}

// ------------------------------------------------------------ not a Kala file

use std::ffi::OsStr;

#[test]
fn a_non_kala_file_exits_3_for_asof_log_and_info() {
    let (_d, p) = file_with(b"this is definitely not a kala world, just some text\n");
    let before = std::fs::read(&p).unwrap();
    let cmds: [&[&str]; 3] = [&["asof", "edwin"], &["log"], &["info"]];
    for c in cmds {
        let mut args: Vec<&OsStr> = vec![OsStr::new(c[0]), p.as_os_str()];
        args.extend(c[1..].iter().map(OsStr::new));
        let o = kala(args);
        assert_eq!(code(&o), 3, "{c:?}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
        assert!(!stderr(&o).contains("not implemented"));
        assert!(o.stdout.is_empty());
        no_raw_controls(&stderr(&o));
    }
    assert_eq!(std::fs::read(&p).unwrap(), before, "file untouched");
}

#[test]
fn a_log_of_an_unsupported_version_exits_3() {
    let fx = varrow();
    let mut bytes = std::fs::read(&fx.path).unwrap();
    bytes[8..12].copy_from_slice(&(LOG_FORMAT_VERSION + 50).to_le_bytes());
    let (_d, p) = file_with(&bytes);
    for cmd in ["log", "info"] {
        let o = kala([OsStr::new(cmd), p.as_os_str()]);
        assert_eq!(code(&o), 3, "{cmd}: {}", stderr(&o));
        assert!(stderr(&o).starts_with("kala: error: "), "{}", stderr(&o));
        assert!(!stderr(&o).contains("not implemented"));
    }
}
