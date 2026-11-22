#![allow(dead_code)]
//! Shared helpers for the `kala` binary end-to-end tests.

use kala_core::World;
use kala_core::log::EntryBody;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

pub const VARROW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../kala-core/tests/fixtures/varrow.yaml"
);

/// Runs the real `kala` binary with `args`.
pub fn kala<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(args)
        .env("NO_COLOR", "")
        .output()
        .expect("spawn kala")
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub fn code(o: &Output) -> i32 {
    o.status.code().expect("exited by signal")
}

/// Counts as the contract defines them, computed through the public API.
pub struct Counts {
    pub entities: usize,
    pub facts: usize,
    pub transactions: usize,
}

pub fn counts(w: &World) -> Counts {
    let mut c = Counts {
        entities: 0,
        facts: 0,
        transactions: 0,
    };
    for e in w.entries() {
        match e.body {
            EntryBody::DefineEntity { .. } => c.entities += 1,
            EntryBody::Fact { .. } | EntryBody::Revised { .. } => c.facts += 1,
            EntryBody::Commit { .. } => c.transactions += 1,
            _ => {}
        }
    }
    c
}

/// Every overlap at the latest canon, as `(handle-or-id, attribute)`.
pub fn overlap_pairs(w: &World) -> Vec<(String, String)> {
    let attrs: Vec<String> = w.attributes().map(|a| a.name.clone()).collect();
    let mut out = Vec::new();
    for a in attrs {
        for o in w.overlaps(&a).expect("overlaps") {
            let who = w
                .handle_of(o.entity)
                .map(str::to_string)
                .unwrap_or_else(|| w.format_id(o.entity));
            out.push((who, a.clone()));
        }
    }
    out
}

pub fn warning_lines(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("warning: ")).collect()
}

pub fn no_raw_controls(s: &str) {
    assert!(!s.bytes().any(|b| b == 0x1b), "raw ESC in {s:?}");
    assert!(!s.contains('\u{9b}'), "raw U+009B in {s:?}");
}

pub fn dir_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

pub const OVERLAP_FIXTURE: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
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
"#;

pub const RESOLVED_FIXTURE: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
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
  - tick: 250
    ops:
      - retract: [greg, located_in, varrow]
"#;

pub const OTHER_FIXTURE: &str = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: name, type: string }
entities:
  CHARACTER: [zed, yara]
transactions:
  - tick: 5
    ops:
      - assert: [zed, name, "Zed"]
"#;

/// Second transaction's first op uses an unknown handle.
pub const BAD_OP_FIXTURE: &str = r#"
schema:
  node_types: [CHARACTER]
  attributes:
    - { name: name, type: string }
entities:
  CHARACTER: [zed]
transactions:
  - tick: 5
    ops:
      - assert: [zed, name, "Zed"]
  - ops:
      - assert: [nobody, name, "Nobody"]
        label: bad-op
        tick: 6
"#;

// ---------------------------------------------------------------------------
// Slice 5 items 3, 4, 7: read-command helpers. The expected answers are
// computed through the kala-core API on an in-memory twin of the world file.
// ---------------------------------------------------------------------------

use kala_core::{
    Direction, EntityId, Precision, Tick, TxId, Value, ValueType, load_fixture, load_fixture_to,
};
use serde_json::{Value as Json, json};
use std::path::PathBuf;

/// A world file on disk (for the binary) plus its in-memory twin (for the
/// expected answers). Both come from the same fixture, so IDs and txs match.
pub struct Fx {
    pub dir: tempfile::TempDir,
    pub path: PathBuf,
    pub mem: World,
}

pub fn build(yaml: &str) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    // Dropped at once: the file lock must be free for the binary.
    drop(load_fixture_to(yaml, &path).expect("fixture loads to disk"));
    let mem = load_fixture(yaml).expect("fixture loads in memory");
    Fx { dir, path, mem }
}

pub fn varrow() -> Fx {
    build(&std::fs::read_to_string(VARROW).unwrap())
}

impl Fx {
    /// `kala <cmd> <world> <rest...>`.
    pub fn run(&self, cmd: &str, rest: &[&str]) -> Output {
        let mut args: Vec<&OsStr> = vec![cmd.as_ref(), self.path.as_os_str()];
        args.extend(rest.iter().map(|s| OsStr::new(*s)));
        kala(args)
    }

    /// Runs with `--json` appended; asserts exit 0 and exactly one JSON document.
    pub fn json(&self, cmd: &str, rest: &[&str]) -> Json {
        let mut r: Vec<&str> = rest.to_vec();
        // Before a `--` (what follows it is positional).
        let at = r.iter().position(|a| *a == "--").unwrap_or(r.len());
        r.insert(at, "--json");
        let o = self.run(cmd, &r);
        assert_eq!(code(&o), 0, "kala {cmd} {rest:?}: stderr: {}", stderr(&o));
        parse_one_json(&stdout(&o))
    }

    pub fn text(&self, cmd: &str, rest: &[&str]) -> String {
        let o = self.run(cmd, rest);
        assert_eq!(code(&o), 0, "kala {cmd} {rest:?}: stderr: {}", stderr(&o));
        stdout(&o)
    }

    /// Asserts a user error (exit `want`), empty stdout, `kala: error: ` on
    /// stderr (exit 1 only), and returns stderr.
    pub fn fails(&self, want: i32, cmd: &str, rest: &[&str]) -> String {
        let o = self.run(cmd, rest);
        assert_eq!(
            code(&o),
            want,
            "kala {cmd} {rest:?}: stderr: {}",
            stderr(&o)
        );
        assert!(
            o.stdout.is_empty(),
            "stdout must be empty on error: {}",
            stdout(&o)
        );
        let err = stderr(&o);
        assert!(!err.trim().is_empty());
        assert!(!err.contains("not implemented"), "stub answered: {err}");
        if want == 1 {
            assert!(err.starts_with("kala: error: "), "stderr: {err}");
        }
        no_raw_controls(&err);
        err
    }

    pub fn id(&self, handle: &str) -> EntityId {
        self.mem
            .entity_by_handle(handle)
            .unwrap_or_else(|| panic!("no handle {handle}"))
    }

    pub fn exp(&self) -> Exp<'_> {
        Exp::new(&self.mem, None, None)
    }
}

pub fn parse_one_json(out: &str) -> Json {
    assert!(
        out.ends_with('\n') && !out.ends_with("\n\n"),
        "exactly one trailing newline: {out:?}"
    );
    serde_json::from_str(&out[..out.len() - 1])
        .unwrap_or_else(|e| panic!("not one JSON document ({e}): {out}"))
}

pub fn prec_str(p: Precision) -> &'static str {
    match p {
        Precision::Exact => "exact",
        Precision::Day => "day",
        Precision::Month => "month",
        Precision::Year => "year",
        Precision::Decade => "decade",
        Precision::Century => "century",
    }
}

pub fn tick_obj(t: Tick, p: Precision) -> Json {
    json!({"tick": t.0, "precision": prec_str(p)})
}

/// Expected JSON fragments for one command's `(canon, tick)`.
pub struct Exp<'a> {
    pub w: &'a World,
    pub canon: TxId,
    pub tick: Tick,
}

impl<'a> Exp<'a> {
    /// `canon`/`tick` of `None` mean the defaults: latest tx, and the latest
    /// tick at that canon.
    pub fn new(w: &'a World, canon: Option<u64>, tick: Option<i64>) -> Exp<'a> {
        let v = w.at_canon(TxId(canon.unwrap_or(w.latest_tx().0)));
        Exp {
            w,
            canon: v.canon(),
            tick: tick.map(Tick).unwrap_or_else(|| v.latest_tick()),
        }
    }

    pub fn view(&self) -> kala_core::View<'a> {
        self.w.at_canon(self.canon)
    }

    pub fn with_tick(&self, t: Tick) -> Exp<'a> {
        Exp {
            w: self.w,
            canon: self.canon,
            tick: t,
        }
    }

    pub fn name_of(&self, id: EntityId) -> Option<String> {
        if self.w.attribute("name").map(|a| a.value_type) != Some(ValueType::Str) {
            return None;
        }
        match self
            .view()
            .as_of(id, "name", self.tick)
            .unwrap_or_default()
            .as_slice()
        {
            [Value::Str(s)] => Some(s.clone()),
            _ => None,
        }
    }

    pub fn entity(&self, id: EntityId) -> Json {
        let handle = self.w.handle_of(id).map(str::to_string);
        let name = self.name_of(id);
        let label = handle
            .clone()
            .or_else(|| name.clone())
            .unwrap_or_else(|| self.w.format_id(id));
        json!({"id": self.w.format_id(id), "handle": handle, "name": name, "label": label})
    }

    pub fn value(&self, v: &Value) -> Json {
        match v {
            Value::Bool(b) => json!({"type": "bool", "value": b}),
            Value::I64(i) => json!({"type": "i64", "value": i}),
            Value::F64(f) if f.is_infinite() => {
                json!({"type": "f64", "value": if *f > 0.0 { "inf" } else { "-inf" }})
            }
            Value::F64(f) => json!({"type": "f64", "value": f}),
            Value::Str(s) => json!({"type": "string", "value": s}),
            Value::Ref(e) => json!({"type": "ref", "value": self.entity(*e)}),
        }
    }

    /// `{"attr", "values": [{"value", "since"}]}` as `asof` prints it.
    pub fn attr_entry(&self, e: EntityId, attr: &str) -> Json {
        let values: Vec<Json> = self
            .view()
            .as_of_detail(e, attr, self.tick)
            .expect("as_of_detail")
            .iter()
            .map(|c| json!({"value": self.value(&c.value), "since": tick_obj(c.tick, c.precision)}))
            .collect();
        json!({"attr": attr, "values": values})
    }

    /// Every schema attribute (schema order) holding at least one value.
    pub fn attributes(&self, e: EntityId) -> Vec<Json> {
        let names: Vec<String> = self.w.attributes().map(|a| a.name.clone()).collect();
        names
            .iter()
            .map(|a| self.attr_entry(e, a))
            .filter(|j| !j["values"].as_array().unwrap().is_empty())
            .collect()
    }

    pub fn asof(&self, e: EntityId, attr: Option<&str>) -> Json {
        let attributes = match attr {
            Some(a) => vec![self.attr_entry(e, a)],
            None => self.attributes(e),
        };
        json!({"command": "asof", "canon": self.canon.0, "tick": self.tick.0,
               "entity": self.entity(e), "attributes": attributes})
    }

    pub fn neighbors(&self, e: EntityId, attr: &str, dir: Direction) -> Json {
        let mut ns = self
            .view()
            .neighbors(e, attr, dir, self.tick)
            .expect("neighbors");
        ns.sort_by_key(|(n, _)| *n);
        let list: Vec<Json> = ns
            .iter()
            .map(|(n, edge)| json!({"entity": self.entity(*n), "edge": self.w.format_id(EntityId::from(*edge))}))
            .collect();
        let d = match dir {
            Direction::Out => "out",
            Direction::In => "in",
            Direction::Both => "both",
        };
        json!({"command": "neighbors", "canon": self.canon.0, "tick": self.tick.0,
               "entity": self.entity(e), "attr": attr, "direction": d, "neighbors": list})
    }

    /// Handles of the neighbors, sorted by entity ID.
    pub fn neighbor_handles(&self, e: EntityId, attr: &str, dir: Direction) -> Vec<String> {
        let mut ns = self
            .view()
            .neighbors(e, attr, dir, self.tick)
            .expect("neighbors");
        ns.sort_by_key(|(n, _)| *n);
        ns.iter()
            .map(|(n, _)| {
                self.w
                    .handle_of(*n)
                    .map(str::to_string)
                    .unwrap_or_else(|| self.w.format_id(*n))
            })
            .collect()
    }

    /// `history`: entity names are resolved at the canon's latest tick.
    pub fn history(&self, e: EntityId, attr: &str) -> Json {
        let at_latest = self.with_tick(self.view().latest_tick());
        let changes: Vec<Json> = self
            .view()
            .history(e, attr)
            .expect("history")
            .iter()
            .map(|c| {
                let mut o = tick_obj(c.tick, c.precision);
                o["op"] = json!(match c.op {
                    kala_core::Op::Assert => "assert",
                    kala_core::Op::Retract => "retract",
                });
                o["value"] = at_latest.value(&c.value);
                o
            })
            .collect();
        json!({"command": "history", "canon": self.canon.0, "entity": at_latest.entity(e),
               "attr": attr, "changes": changes})
    }

    pub fn find(&self, attr: &str, v: &Value, ci: bool, ids: Vec<EntityId>) -> Json {
        let mut ids = ids;
        ids.sort();
        json!({"command": "find", "canon": self.canon.0, "tick": self.tick.0, "attr": attr,
               "value": self.value(v), "case_insensitive": ci,
               "entities": ids.iter().map(|i| self.entity(*i)).collect::<Vec<_>>()})
    }

    pub fn list(&self, node_type: &str) -> Json {
        let mut ids: Vec<EntityId> = self
            .view()
            .entities_of_type(node_type)
            .expect("type")
            .collect();
        ids.sort();
        json!({"command": "list", "canon": self.canon.0, "tick": self.tick.0, "node_type": node_type,
               "entities": ids.iter().map(|i| self.entity(*i)).collect::<Vec<_>>()})
    }

    pub fn edge(&self, src: EntityId, attr: &str, tgt: EntityId) -> Json {
        let eid = self.w.edge_id(src, attr, tgt).expect("edge exists");
        let holds = self
            .view()
            .as_of_value(src, attr, &Value::Ref(tgt), self.tick)
            .expect("as_of_value");
        json!({"command": "edge", "canon": self.canon.0, "tick": self.tick.0,
               "edge": self.w.format_id(EntityId::from(eid)),
               "source": self.entity(src), "attr": attr, "target": self.entity(tgt),
               "holds": holds, "properties": self.attributes(EntityId::from(eid))})
    }
}

/// The first canon whose latest tick differs from the world's latest tick.
pub fn canon_with_other_latest_tick(w: &World) -> u64 {
    (1..=w.latest_tx().0)
        .find(|&tx| w.at_canon(TxId(tx)).latest_tick() != w.latest_tick())
        .expect("some earlier canon has a smaller latest tick")
}

/// Typed values, an inverse name, a name with control characters.
pub const TYPED: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name,      type: string }
    - { name: age,       type: i64 }
    - { name: height,    type: f64 }
    - { name: brave,     type: bool }
    - { name: home,      type: ref }
    - { name: parent_of, type: ref, cardinality: many, inverse: child_of }
entities:
  CHARACTER: [alice, bob, carol]
  PLACE: [keep]
transactions:
  - tick: 10
    ops:
      - assert: [alice, name, "Alice"]
      - assert: [alice, age, -5]
      - assert: [alice, height, 1.5]
      - assert: [alice, brave, true]
      - assert: [alice, home, keep]
      - assert: [bob, age, 7]
      - assert: [bob, name, "Bob"]
      - assert: [alice, parent_of, bob]
      - assert: [alice, parent_of, carol]
"#;

/// A name carrying ESC, U+009B (C1 CSI) and U+202E (bidi override).
pub const CTRL_NAME: &str = "A\u{1b}[31mB\u{9b}C\u{202e}D";

/// Fixture holding [`CTRL_NAME`] (YAML `\u` escapes keep the source plain).
pub fn ctrl_fixture() -> String {
    let yaml_name = ["A", "\\u001B", "[31mB", "\\u009B", "C", "\\u202E", "D"].concat();
    format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: name, type: string }}\nentities:\n  CHARACTER: [ctrl]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [ctrl, name, \"{yaml_name}\"]\n"
    )
}
