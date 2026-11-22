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

use kala_core::{DoctorOptions, Finding, IndexName, Severity, TxId, ValueType, doctor_log};
use kala_datalog::{MAX_CHECKS_BYTES, Prepared};
use kala_query::{CanonSource, Limits, Params, QueryError};
use serde::Serialize;

use super::Done;
use super::query::{query_err, read_text_file, row_text, table};
use super::read::open_world;
use crate::cli::Canon;
use crate::error::CliError;
use crate::escape;
use crate::names::{self, LogNames, Names};
use crate::out::{JValue, Text, render};

/// Rows of one hint shown in the text output.
const TEXT_ROWS: usize = 5;
/// Rows of one hint carried in the JSON output (`rows_total` is exact).
const JSON_ROWS: usize = 100;

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
    /// `story_check` or `budget_exhausted`.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// The check's name, without the `:`.
    check: String,
    /// How many rows the check returned.
    rows_total: usize,
    /// As `kala query --json` names them.
    columns: Vec<String>,
    /// The first [`JSON_ROWS`] rows, as `kala query --json` prints them.
    rows: Vec<Vec<JValue>>,
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
                for r in h.rows.iter().take(TEXT_ROWS) {
                    let _ = writeln!(s, "    {}", row_text(r));
                }
                if k > TEXT_ROWS {
                    let _ = writeln!(s, "    ... ({} more)", k - TEXT_ROWS);
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

/// Prepares every check, then shares one steps/bytes budget across all runs.
/// Peak live bytes from each completed run are conservatively deducted from
/// the batch budget. Exhaustion is one hint covering this and later checks.
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
    for (c, p) in checks.iter().zip(&prepared) {
        let result = if remaining.max_steps == 0 || remaining.max_bytes == 0 {
            Err(QueryError::StepBudget { limit: 0 })
        } else {
            kala_query::run_with_usage(&world, &p.plan, &Params::new(), &remaining)
        };
        let (mut rs, usage) = match result {
            Ok(result) => result,
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
                });
                break;
            }
            Err(e) => return Err(query_err(e, &whose(c))),
        };
        remaining.max_steps = remaining.max_steps.saturating_sub(usage.steps);
        remaining.max_bytes = remaining.max_bytes.saturating_sub(usage.peak_bytes);
        if rs.rows.is_empty() {
            continue;
        }
        let rows_total = rs.rows.len();
        // Cut before label resolution/rendering, including in text mode.
        rs.rows.truncate(if json { JSON_ROWS } else { TEXT_ROWS });
        let (columns, rows) = table(&world, &rs);
        hints.push(JHint {
            kind: "story_check",
            message: None,
            check: c.name.clone(),
            rows_total,
            columns,
            rows,
        });
    }
    Ok(hints)
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
            type_name(*expected),
            type_name(*got)
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

/// The value type as the CLI's JSON values name it.
fn type_name(t: ValueType) -> &'static str {
    match t {
        ValueType::Bool => "bool",
        ValueType::I64 => "i64",
        ValueType::F64 => "f64",
        ValueType::Str => "string",
        ValueType::Ref => "ref",
    }
}
