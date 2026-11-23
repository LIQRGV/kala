//! `kala doctor <world> [--canon C] [--since C] [--checks FILE]`.
//!
//! Always [`kala_core::doctor_log`] on the file, never `World::open`: an
//! open would cut a torn tail (and refuse a corrupt log), and doctor must
//! report damage, not repair it. Read-only: the file and its directory are
//! left exactly as they are.
//!
//! Story checks (CLAUDE.md "Doctor", third layer): with `--checks FILE`,
//! each named Datalog query of the file that returns rows is a hint. They
//! run only on a store without errors, through `World::open_read_only`
//! (which never changes the file). Every check is prepared before any runs,
//! so a broken file prints no partial report.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::{DoctorOptions, Finding, IndexName, Severity, TxId, doctor_log};
use kala_datalog::{MAX_CHECKS_BYTES, Prepared};
use kala_query::{CanonSource, Limits, Params, QueryError, Usage};
use serde::Serialize;

use super::Done;
use super::query::{query_err, read_text_file, row_text};
use super::read::open_world;
use crate::cli::Canon;
use crate::error::CliError;
use crate::escape;
use crate::names::{self, LogNames, Names};
use crate::out::{JInner, JValue, Labeler, Text, json_size_within, render, value_text};

/// Rows of one hint shown in the text output.
const TEXT_ROWS: usize = 5;
/// Rows of one hint carried in the JSON output (`rows_total` is exact).
const JSON_ROWS: usize = 100;
const TOTAL_ROWS: usize = 10_000;
const TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// Size the row before rendering. String escaping and interval output can
/// expand substantially; neither needs a temporary rendered allocation.
fn text_row_size(row: &[JValue], limit: usize) -> Option<usize> {
    let mut bytes = 5usize; // indentation and newline
    for (i, cell) in row.iter().enumerate() {
        bytes = bytes.checked_add(if i == 0 { 0 } else { 2 })?;
        let remaining = limit.checked_sub(bytes)?;
        let n = match &cell.value {
            JInner::Str(s) => escape::quoted_len_within(s, remaining)?,
            JInner::Intervals(intervals) => {
                let mut n = 2usize; // braces
                for (i, (from, until)) in intervals.iter().enumerate() {
                    let end = until.map_or_else(|| "∞".to_owned(), |u| u.to_string());
                    n = n.checked_add(
                        from.to_string().len() + end.len() + 4 + usize::from(i > 0) * 2,
                    )?;
                    if n > remaining {
                        return None;
                    }
                }
                n
            }
            _ => value_text(cell).len(), // bounded scalars / truncated labels
        };
        bytes = bytes.checked_add(n)?;
        if bytes > limit {
            return None;
        }
    }
    (bytes <= limit).then_some(bytes)
}

/// `kala doctor`'s options.
pub struct Options<'a> {
    pub canon: Option<Canon>,
    pub since: Option<Canon>,
    pub checks: Option<&'a Path>,
    pub json: bool,
}

#[derive(Serialize)]
struct JFinding {
    /// The [`Finding`] variant, snake_case (`cardinality_overlap`).
    kind: &'static str,
    /// One line naming what is wrong and where, already escaped.
    message: String,
}

/// A story check that returned rows.
#[derive(Serialize)]
struct JHint {
    /// `story_check`, `budget_exhausted` (the shared step budget ran out;
    /// this and later checks didn't run), `memory_exhausted` (this check
    /// exceeded the memory budget and was skipped) or `output_truncated`
    /// (rows were omitted from this check on by the total output cap).
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// The check's name, without the `:`.
    check: String,
    /// How many rows the check returned.
    rows_total: usize,
    /// As `kala query --json` names them.
    columns: Vec<String>,
    /// Up to [`JSON_ROWS`] rows, as `kala query --json` prints them; fewer
    /// when the total output cap ([`TOTAL_ROWS`], [`TOTAL_BYTES`]) omitted
    /// some (an `output_truncated` hint then follows the first such check).
    rows: Vec<Vec<JValue>>,
    /// Text output retains only what it displays, never hidden reference
    /// names or other full JSON metadata behind a shortened label.
    #[serde(skip)]
    text_rows: Vec<String>,
}

#[derive(Serialize)]
struct Doctor {
    command: &'static str,
    canon: Option<u64>,
    since: Option<u64>,
    clean: bool,
    errors: Vec<JFinding>,
    warnings: Vec<JFinding>,
    hints: Vec<JHint>,
    /// `--checks` was given but not run, because the store has errors.
    checks_skipped: bool,
}

impl Text for Doctor {
    fn text(&self) -> String {
        if self.clean {
            return "clean\n".to_string();
        }
        let mut s = String::new();
        for (head, list) in [("errors", &self.errors), ("warnings", &self.warnings)] {
            if list.is_empty() {
                continue;
            }
            let _ = writeln!(s, "{head} ({}):", list.len());
            for f in list {
                let _ = writeln!(s, "  {}: {}", f.kind, f.message);
            }
        }
        if self.checks_skipped {
            s.push_str("hints: skipped (the store has errors)\n");
        } else if !self.hints.is_empty() {
            let _ = writeln!(s, "hints ({}):", self.hints.len());
            for h in &self.hints {
                if let Some(message) = &h.message {
                    let _ = writeln!(s, "  {message}");
                    continue;
                }
                let k = h.rows_total;
                let _ = writeln!(
                    s,
                    "  {}: :{}: {k} {}",
                    h.kind,
                    escape::value(&h.check),
                    if k == 1 { "row" } else { "rows" }
                );
                for row in &h.text_rows {
                    let _ = writeln!(s, "    {row}");
                }
                if k > h.text_rows.len() {
                    let _ = writeln!(s, "    ... ({} more)", k - h.text_rows.len());
                }
            }
        }
        s
    }
}

pub fn run(world: &Path, o: Options<'_>) -> Result<Done, CliError> {
    // The file's shape is checked whatever the store's state, before any
    // report is printed.
    let checks = o.checks.map(read_checks).transpose()?;
    let opts = DoctorOptions {
        canon: o.canon.map(|c| TxId(c.0)),
        since: o.since.map(|c| TxId(c.0)),
    };
    let report = doctor_log(world, &opts).map_err(|e| CliError::at_path(world, &e))?;
    // Labels come from the committed entries, read without a lock and
    // without changing the file. A log damaged before its end doesn't read
    // (that damage is a finding): findings then show raw IDs.
    let names = kala_core::log::read_log(world)
        .map(|entries| LogNames::from_entries(&entries))
        .unwrap_or_default();
    let mut out = Doctor {
        command: "doctor",
        canon: o.canon.map(|c| c.0),
        since: o.since.map(|c| c.0),
        clean: report.is_clean(),
        errors: Vec::new(),
        warnings: Vec::new(),
        hints: Vec::new(),
        checks_skipped: false,
    };
    for f in &report.findings {
        let j = JFinding {
            kind: kind(f),
            message: message(&names, f),
        };
        match f.severity() {
            Severity::Error => out.errors.push(j),
            Severity::Warning => out.warnings.push(j),
        }
    }
    if let Some(checks) = checks {
        if out.errors.is_empty() {
            out.hints = run_checks(world, &checks, o.canon, o.json)?;
            out.clean &= out.hints.is_empty();
        } else {
            out.checks_skipped = true;
        }
    }
    Ok(Done {
        findings: !out.errors.is_empty(),
        text: render(&out, o.json),
    })
}

/// The checks file, read like `query -f` and parsed (not compiled).
/// Returns the file text too, so compile errors can be located in it.
fn read_checks(path: &Path) -> Result<(String, Vec<kala_datalog::Check>), CliError> {
    let text = read_text_file(path, MAX_CHECKS_BYTES, "the checks file")?;
    // Datalog messages escape the input they echo; `main` escapes the
    // whole line again.
    let checks = kala_datalog::parse_checks(&text)
        .map_err(|e| CliError::User(format!("{}: {e}", escape::path(path))))?;
    Ok((text, checks))
}

/// `check :name` for messages.
fn whose(c: &kala_datalog::Check) -> String {
    format!("check :{}", escape::value(&c.name))
}

/// Prepares every check, then runs them one after another. The step budget
/// is shared by all checks: running out of it is one hint covering this and
/// every later check. Memory is released between checks, so each check gets
/// the full byte limit, and running out of it only stops that check.
fn run_checks(
    path: &Path,
    (text, checks): &(String, Vec<kala_datalog::Check>),
    canon: Option<Canon>,
    json: bool,
) -> Result<Vec<JHint>, CliError> {
    let world = open_world(path)?;
    let mut prepared: Vec<Prepared> = Vec::with_capacity(checks.len());
    for c in checks {
        // `prepare` locates errors in the query text; relocate them in the
        // checks file, where the author can find them.
        let mut p = kala_datalog::prepare(&world, &c.query).map_err(|e| {
            let e = e.offset_by(c.query_span.start).locate(text);
            CliError::User(format!("{}: {e}", whose(c)))
        })?;
        if !p.params.is_empty() {
            return Err(CliError::User(format!(
                "{}: a story check takes no `:in` parameters",
                whose(c)
            )));
        }
        if let Some(canon) = canon {
            p.plan.canon = CanonSource::Const(canon.0);
        }
        prepared.push(p);
    }
    let mut hints = Vec::new();
    let mut remaining = Limits::default();
    let (mut rendered_rows, mut rendered_bytes) = (0usize, 0usize);
    let mut truncated = false;
    for (c, p) in checks.iter().zip(&prepared) {
        let result = if remaining.max_steps == 0 {
            Err(QueryError::StepBudget { limit: 0 })
        } else {
            kala_query::run_with(
                &world,
                &p.plan,
                &Params::new(),
                kala_query::RunOptions::new(remaining),
            )
        };
        let (rs, usage) = match result {
            Ok(result) => result,
            Err(e @ (QueryError::StepBudget { .. } | QueryError::MemoryBudget { .. }))
                if !stops_later_checks(&e) =>
            {
                hints.push(JHint {
                    kind: "memory_exhausted",
                    message: Some(format!(
                        "story check :{} skipped: it exceeded the memory budget",
                        escape::value(&c.name)
                    )),
                    check: c.name.clone(),
                    rows_total: 0,
                    columns: Vec::new(),
                    rows: Vec::new(),
                    text_rows: Vec::new(),
                });
                continue;
            }
            Err(QueryError::StepBudget { .. } | QueryError::MemoryBudget { .. }) => {
                hints.push(JHint {
                    kind: "budget_exhausted",
                    message: Some(format!(
                        "story checks stopped: budget exhausted at :{}",
                        escape::value(&c.name)
                    )),
                    check: c.name.clone(),
                    rows_total: 0,
                    columns: Vec::new(),
                    rows: Vec::new(),
                    text_rows: Vec::new(),
                });
                break;
            }
            Err(e) => return Err(query_err(e, &whose(c))),
        };
        remaining = after_check(&remaining, &usage);
        if rs.rows.is_empty() {
            continue;
        }
        let rows_total = rs.rows.len();
        let columns = rs
            .columns
            .iter()
            .map(|v| super::query::column_name(&v.0))
            .collect();
        let mut rows = Vec::new();
        let mut text_rows = Vec::new();
        // A row that doesn't fit what is left of the output allowance is
        // skipped on its own; later (smaller) rows still print. Only the
        // total row cap ends rendering for every later check.
        let mut omitted = false;
        if rendered_rows < TOTAL_ROWS {
            let labeler = Labeler::new(&world, rs.canon, rs.tick);
            for row in rs
                .rows
                .iter()
                .take(if json { JSON_ROWS } else { TEXT_ROWS })
            {
                if rendered_rows == TOTAL_ROWS {
                    omitted = true;
                    break;
                }
                let row: Vec<JValue> = row.iter().map(|d| labeler.datum(d)).collect();
                let allowance = TOTAL_BYTES - rendered_bytes;
                let bytes = if json {
                    // Include a separator per row, conservatively even
                    // for the first row of a hint's array.
                    allowance
                        .checked_sub(1)
                        .and_then(|n| json_size_within(&row, n))
                        .map(|n| n + 1)
                } else {
                    text_row_size(&row, allowance)
                };
                let Some(bytes) = bytes else {
                    omitted = true;
                    continue;
                };
                rendered_rows += 1;
                rendered_bytes += bytes;
                if json {
                    rows.push(row);
                } else {
                    text_rows.push(row_text(&row));
                }
            }
        } else {
            omitted = true;
        }
        hints.push(JHint {
            kind: "story_check",
            message: None,
            check: c.name.clone(),
            rows_total,
            columns,
            rows,
            text_rows,
        });
        // One notice, after the first check that lost rows to the cap.
        if omitted && !truncated {
            truncated = true;
            hints.push(JHint {
                kind: "output_truncated",
                message: Some(format!(
                    "story-check rows omitted from :{} on (output cap: {TOTAL_ROWS} rows, {} MiB)",
                    escape::value(&c.name),
                    TOTAL_BYTES >> 20
                )),
                check: c.name.clone(),
                rows_total: 0,
                columns: Vec::new(),
                rows: Vec::new(),
                text_rows: Vec::new(),
            });
        }
    }
    Ok(hints)
}

/// Whether running out of a budget stops every later story check: only the
/// shared step budget does; memory is per check.
fn stops_later_checks(e: &QueryError) -> bool {
    matches!(e, QueryError::StepBudget { .. })
}

/// The budget left for the next story check: steps are shared by all checks
/// (so a file of many checks can't use many budgets), while memory is
/// released when a check ends, so each check gets the full byte limit.
fn after_check(remaining: &Limits, usage: &Usage) -> Limits {
    Limits {
        max_steps: remaining.max_steps.saturating_sub(usage.steps),
        ..*remaining
    }
}

fn kind(f: &Finding) -> &'static str {
    match f {
        Finding::IncompleteTail { .. } => "incomplete_tail",
        Finding::ChecksumMismatch { .. } => "checksum_mismatch",
        Finding::MissingCommit { .. } => "missing_commit",
        Finding::TxGap { .. } => "tx_gap",
        Finding::EntryIdGap { .. } => "entry_id_gap",
        Finding::DanglingId { .. } => "dangling_id",
        Finding::DanglingAttr { .. } => "dangling_attr",
        Finding::BadCorrection { .. } => "bad_correction",
        Finding::RevisionSkip { .. } => "revision_skip",
        Finding::InvalidEntry { .. } => "invalid_entry",
        Finding::SchemaMismatch => "schema_mismatch",
        Finding::IndexMismatch { .. } => "index_mismatch",
        Finding::IndexDecode { .. } => "index_decode",
        Finding::LogUnreadable { .. } => "log_unreadable",
        Finding::CardinalityOverlap(_) => "cardinality_overlap",
        Finding::EdgePropertyOnUnassertedEdge { .. } => "edge_property_on_unasserted_edge",
        Finding::WrongValueType { .. } => "wrong_value_type",
        Finding::RetractedEvent { .. } => "retracted_event",
    }
}

/// What the finding says, with its tx, entry or byte offset; every piece
/// that came from the file is escaped.
fn message(n: &dyn Names, f: &Finding) -> String {
    // kala-core's reasons already cut echoed input; escape them whole.
    let reason = |r: &str| escape::line(r);
    match f {
        Finding::IncompleteTail { offset } => format!(
            "the log ends in a torn or damaged transaction at byte offset {offset}; \
             opening the world would cut it there"
        ),
        Finding::ChecksumMismatch { tx, offset } => format!(
            "tx#{} at byte offset {offset}: checksum mismatch; the transaction is skipped",
            tx.0
        ),
        Finding::MissingCommit { tx, offset } => format!(
            "tx#{} at byte offset {offset} has no COMMIT; the transaction is skipped",
            tx.0
        ),
        Finding::TxGap { expected, found } => {
            format!("expected tx#{}, found tx#{}", expected.0, found.0)
        }
        Finding::EntryIdGap { expected, found } => {
            format!("expected entry#{}, found entry#{}", expected.0, found.0)
        }
        Finding::DanglingId { entry, id } => format!(
            "entry#{} uses {}, which no earlier entry defines",
            entry.0,
            names::entity(n, *id)
        ),
        Finding::DanglingAttr { entry, attr } => format!(
            "entry#{} uses attribute ID {}, which the schema doesn't define",
            entry.0, attr.0
        ),
        Finding::BadCorrection { entry, reason: r } => {
            format!("entry#{}: {}", entry.0, reason(r))
        }
        Finding::RevisionSkip {
            entry,
            of,
            expected,
            found,
        } => format!(
            "entry#{} is rev {found} of entry#{}, expected rev {expected}",
            entry.0, of.0
        ),
        Finding::InvalidEntry { entry, reason: r } => {
            format!("entry#{}: {}", entry.0, reason(r))
        }
        Finding::SchemaMismatch => "derived schema does not match the log".to_owned(),
        Finding::IndexMismatch { index, detail } => {
            format!("index {}: {}", index_name(*index), reason(detail))
        }
        Finding::IndexDecode { index, key } => format!(
            "index {}: a {}-byte key doesn't decode",
            index_name(*index),
            key.len()
        ),
        Finding::LogUnreadable { reason: r } => reason(r),
        Finding::CardinalityOverlap(o) => names::overlap_line(n, o),
        Finding::EdgePropertyOnUnassertedEdge {
            entry,
            edge,
            attr,
            tick,
        } => format!(
            "entry#{}: property {} on {} at tick {}, where the edge doesn't hold",
            entry.0,
            escape::value(attr),
            names::entity(n, kala_core::EntityId::from(*edge)),
            tick.0
        ),
        Finding::WrongValueType {
            entry,
            attr,
            expected,
            got,
        } => format!(
            "entry#{}: attribute {} holds {} values, got a {} value; the entry is skipped",
            entry.0,
            escape::value(attr),
            expected.name(),
            got.name()
        ),
        Finding::RetractedEvent { entry, attr } => format!(
            "entry#{}: retracts event attribute {}; the entry is skipped",
            entry.0,
            escape::value(attr)
        ),
    }
}

fn index_name(i: IndexName) -> &'static str {
    match i {
        IndexName::E => "E",
        IndexName::R => "R",
        IndexName::ELatest => "E_latest",
        IndexName::RLatest => "R_latest",
        IndexName::V => "V",
        IndexName::VLatest => "V_latest",
        IndexName::VFold => "V_fold",
        IndexName::VFoldLatest => "V_fold_latest",
    }
}

#[cfg(test)]
mod budget_tests {
    use super::after_check;
    use kala_query::{Limits, Usage};

    // Review round 1: story checks share one step budget, but memory is
    // released between checks, so a memory-heavy check (e.g. a full memo
    // cache) must not starve the checks after it.
    #[test]
    fn steps_are_shared_but_memory_is_per_check() {
        let full = Limits::default();
        let usage = Usage {
            steps: 1_000,
            peak_bytes: full.max_bytes,
        };
        let next = after_check(&full, &usage);
        assert_eq!(next.max_steps, full.max_steps - 1_000);
        assert_eq!(next.max_bytes, full.max_bytes);
        assert_eq!(next.max_rows, full.max_rows);
    }
}

#[cfg(test)]
mod budget_flow_tests {
    use super::stops_later_checks;
    use kala_query::QueryError;

    // Review round 2: memory is per check, so running out of it only stops
    // that check; running out of the shared steps stops every later check.
    #[test]
    fn only_the_shared_step_budget_stops_later_checks() {
        assert!(stops_later_checks(&QueryError::StepBudget { limit: 1 }));
        assert!(!stops_later_checks(&QueryError::MemoryBudget { limit: 1 }));
    }
}

#[cfg(test)]
mod retained_text_tests {
    use super::{Doctor, TOTAL_BYTES, run_checks};
    use crate::out::Text;
    use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};

    #[test]
    fn text_hints_retain_only_rendered_labels_not_full_reference_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.kala");
        let mut name = AttrDef::new("name", ValueType::Str, Kind::State, Cardinality::One);
        name.indexed = false;
        let schema = Schema::new()
            .node_type("N")
            .attribute(name)
            .attribute(AttrDef::new(
                "flag",
                ValueType::Bool,
                Kind::State,
                Cardinality::One,
            ));
        let mut world = World::create(&path, schema).unwrap();
        let entity = world.create_entity("N", None).unwrap();
        world
            .assert(
                entity,
                "name",
                Value::Str("n".repeat(1024 * 1024)),
                Some(Tick(0)),
            )
            .unwrap();
        world
            .assert(entity, "flag", Value::Bool(true), Some(Tick(0)))
            .unwrap();
        drop(world);
        let source = format!(
            "[{}]",
            (0..16)
                .map(|i| format!("[:c{i} [:find ?e :where [?e :flag true]]]"))
                .collect::<String>()
        );
        let checks = kala_datalog::parse_checks(&source).unwrap();
        let hints = run_checks(&path, &(source, checks), None, false).unwrap();
        assert_eq!(hints.len(), 16);
        assert!(
            hints
                .iter()
                .all(|hint| hint.kind == "story_check" && hint.rows_total == 1)
        );
        // Text renders short labels. Retaining each full name and its
        // duplicated label in JValue would silently accumulate >32 MiB.
        let retained_json = serde_json::to_vec(&hints).unwrap();
        assert!(
            retained_json.len() < TOTAL_BYTES,
            "text hints retained {} bytes of hidden JSON payload",
            retained_json.len()
        );
        let report = Doctor {
            command: "doctor",
            canon: None,
            since: None,
            clean: false,
            errors: Vec::new(),
            warnings: Vec::new(),
            hints,
            checks_skipped: false,
        };
        let text = report.text();
        assert!(text.len() < TOTAL_BYTES);
        let rows: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("    "))
            .collect();
        assert_eq!(rows.len(), 16);
        assert!(
            rows.iter()
                .all(|line| line.len() <= crate::escape::MAX_VALUE_CHARS + 7)
        );
    }
}
