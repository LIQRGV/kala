//! `kala doctor <world> [--canon C] [--since C]`.
//!
//! Always [`kala_core::doctor_log`] on the file, never `World::open`: an
//! open would cut a torn tail (and refuse a corrupt log), and doctor must
//! report damage, not repair it. Read-only: the file and its directory are
//! left exactly as they are.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::{DoctorOptions, Finding, IndexName, Severity, TxId, ValueType, doctor_log};
use serde::Serialize;

use super::Done;
use crate::cli::Canon;
use crate::error::CliError;
use crate::escape;
use crate::names::{self, LogNames, Names};
use crate::out::{Text, render};

#[derive(Serialize)]
struct JFinding {
    /// The [`Finding`] variant, snake_case (`cardinality_overlap`).
    kind: &'static str,
    /// One line naming what is wrong and where, already escaped.
    message: String,
}

#[derive(Serialize)]
struct Doctor {
    command: &'static str,
    canon: Option<u64>,
    since: Option<u64>,
    clean: bool,
    errors: Vec<JFinding>,
    warnings: Vec<JFinding>,
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
        s
    }
}

pub fn run(
    world: &Path,
    canon: Option<Canon>,
    since: Option<Canon>,
    json: bool,
) -> Result<Done, CliError> {
    let opts = DoctorOptions {
        canon: canon.map(|c| TxId(c.0)),
        since: since.map(|c| TxId(c.0)),
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
        canon: canon.map(|c| c.0),
        since: since.map(|c| c.0),
        clean: report.is_clean(),
        errors: Vec::new(),
        warnings: Vec::new(),
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
    Ok(Done {
        findings: !out.errors.is_empty(),
        text: render(&out, json),
    })
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
