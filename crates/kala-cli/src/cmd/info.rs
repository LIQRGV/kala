//! `kala info <world>`: format version, latest tx and tick, counts, and what
//! the next open for writing would recover.
//!
//! Read-only like every read command
//! ([`kala_core::World::open_read_only`]): no lock,
//! so it works while another process has the world open, and the file is
//! never changed. A torn or damaged tail is left in the file; `info`
//! reports what `World::open` would cut (its JSON `quarantine` is always
//! `null`: nothing is saved). (`kala doctor` reports damage too.)

use std::fmt::Write as _;
use std::path::Path;

use kala_core::CutCause;
use kala_core::log::{EntryBody, LOG_FORMAT_VERSION};
use serde::Serialize;

use super::read::open_world;
use crate::error::CliError;
use crate::escape;
use crate::out::{Text, render};

#[derive(Default, Serialize)]
struct Counts {
    node_types: usize,
    attributes: usize,
    entities: usize,
    edges: usize,
    /// `Fact` and `Revised` entries.
    facts: usize,
    /// `Commit` entries (the genesis included).
    transactions: usize,
}

/// What the next open for writing would cut off the log.
#[derive(Serialize)]
struct Recovery {
    bytes_cut: u64,
    /// `torn`, `damaged`, or `null` when nothing would be cut.
    cause: Option<&'static str>,
    /// Always `null`: a read saves nothing (only `World::open` writes a
    /// quarantine file, when it cuts).
    quarantine: Option<String>,
}

#[derive(Serialize)]
struct Info {
    command: &'static str,
    path: String,
    format_version: u32,
    latest_tx: u64,
    latest_tick: i64,
    counts: Counts,
    recovery: Recovery,
}

impl Text for Info {
    fn text(&self) -> String {
        let c = &self.counts;
        let mut s = String::new();
        // Paths shown whole (escaped, not cut): the author needs them.
        let _ = writeln!(s, "world: {}", escape::line(&self.path));
        let _ = writeln!(s, "format version: {}", self.format_version);
        let _ = writeln!(s, "latest tx: tx#{}", self.latest_tx);
        let _ = writeln!(s, "latest tick: {}", self.latest_tick);
        let _ = writeln!(s, "node types: {}", c.node_types);
        let _ = writeln!(s, "attributes: {}", c.attributes);
        let _ = writeln!(s, "entities: {}", c.entities);
        let _ = writeln!(s, "edges: {}", c.edges);
        let _ = writeln!(s, "facts: {}", c.facts);
        let _ = writeln!(s, "transactions: {}", c.transactions);
        let r = &self.recovery;
        let Some(cause) = r.cause else {
            s.push_str("recovery: clean (nothing to cut)\n");
            return s;
        };
        let _ = writeln!(
            s,
            "recovery: {} bytes of a {cause} last transaction are not committed; they \
             are left in the file and would be cut (and kept in a quarantine file) when \
             the world is next opened for writing",
            r.bytes_cut
        );
        if cause == "damaged" {
            // The CLI writes nothing, so it can't acknowledge for the author.
            s.push_str(
                "hint: those bytes look committed but fail their checksum. Once the world \
                 is opened for writing, writes are refused until the recovery is \
                 acknowledged: review the quarantine file, then acknowledge the recovery \
                 in your writing tool (World::acknowledge_recovery).\n",
            );
        }
        s
    }
}

fn counts(bodies: impl Iterator<Item = EntryBody>) -> Counts {
    let mut c = Counts::default();
    for b in bodies {
        match b {
            EntryBody::DefineNodeType { .. } => c.node_types += 1,
            EntryBody::DefineAttr { .. } => c.attributes += 1,
            EntryBody::DefineEntity { .. } => c.entities += 1,
            EntryBody::DefineEdge { .. } => c.edges += 1,
            EntryBody::Fact { .. } | EntryBody::Revised { .. } => c.facts += 1,
            EntryBody::Commit { .. } => c.transactions += 1,
            EntryBody::Deleted { .. } => {}
        }
    }
    c
}

pub fn run(world: &Path, json: bool) -> Result<String, CliError> {
    let w = open_world(world)?;
    let r = w.recovery();
    let out = Info {
        command: "info",
        path: world.display().to_string(),
        // `World::open_read_only` reads only this version.
        format_version: LOG_FORMAT_VERSION,
        latest_tx: w.latest_tx().0,
        latest_tick: w.latest_tick().0,
        counts: counts(w.entries().map(|e| e.body)),
        recovery: Recovery {
            bytes_cut: r.bytes_cut,
            cause: r.cause.map(|c| match c {
                CutCause::Torn => "torn",
                CutCause::Damaged => "damaged",
            }),
            quarantine: r.quarantine.as_ref().map(|p| p.display().to_string()),
        },
    };
    Ok(render(&out, json))
}
