//! Doctor: a read-only integrity checker for a world and its log, like
//! `git fsck`. It reports and never fixes.
//!
//! Two layers (CLAUDE.md "Doctor"):
//!
//! **Store integrity**: [`Severity::Error`] findings, meaning Kala itself is
//! broken (a bug or disk damage), not that the story breaks a rule.
//!
//! - **The log.** Every transaction has a `COMMIT` and valid checksums; tx
//!   numbers and entry IDs run without gaps. The log is scanned with the same
//!   per-transaction judgement `World::open` uses, but collecting: the scan
//!   goes on past a bad transaction whenever its begin record is sound, and
//!   stops at a tail `World::open` would cut ([`Finding::IncompleteTail`]) or
//!   at a begin record that can't be trusted.
//! - **Replay.** Every entry is replayed with the same validation as
//!   `World::open`: IDs are defined before use, corrections point at real
//!   base facts at their chain's current key, revisions don't skip, and every
//!   other replay rule holds. An entry that fails is reported as the most
//!   specific finding and left out; the replay goes on without it.
//! - **Indexes.** For a [`World`], all four indexes (`E`, `R`, `E_latest`,
//!   `R_latest`) are rebuilt in memory from the log and each stored index is
//!   compared with its own rebuild, independently of the others. A stored
//!   key that doesn't decode is [`Finding::IndexDecode`]; a missing, extra or
//!   different row is [`Finding::IndexMismatch`].
//!
//! One root cause can surface as several findings: a skipped transaction
//! also makes the stored indexes differ from the rebuild, and later entries
//! that depended on it fail replay.
//!
//! **Schema rules**: [`Severity::Warning`] findings, meaning the data breaks
//! a declared rule. They are judged on the world rebuilt from the log, at
//! [`DoctorOptions::canon`]:
//!
//! - **Cardinality overlaps**: every overlap [`View::overlaps`] reports, for
//!   every attribute (the one overlap check write warnings use too).
//! - **Edge properties on an unasserted edge**: a property assert current at
//!   the canon, at a tick where the edge's own triple doesn't hold.
//! - **Wrong value types and retracted events**: entries a live writer would
//!   have refused for exactly those reasons (only a crafted or damaged log
//!   has them). The replay skips them, as any refused entry.
//!
//! Doctor suggests nothing and fixes nothing.
//!
//! [`View::overlaps`]: crate::View::overlaps

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound;
use std::path::Path;

use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId};
use crate::key;
use crate::log::{self, EntryId, SurveyedTxn, TaggedFinding};
use crate::overlap::Overlap;
use crate::tick::Tick;
use crate::value::{Value, ValueType};
use crate::world::{self, Index, ReplayFault, TxId, World};

/// What doctor checks.
#[derive(Clone, Debug, Default)]
pub struct DoctorOptions {
    /// The canon the schema-rule checks read at (default: the latest; a tx
    /// past the latest is the latest). The store-integrity checks always
    /// cover the whole log and ignore it.
    pub canon: Option<TxId>,
    /// Only report what changed after this tx: store-integrity findings of
    /// transactions `> since`, and index rows written by a tx `> since` (a
    /// latest-revision row by the tx of the entry it holds). Schema findings
    /// only where a transaction `> since` (and `<=` the canon) touched them:
    /// an overlap's `(entity, attr, side)` (a ref fact touches its target's
    /// reverse side, a bidirectional one both partners), an edge property's
    /// entry or its edge's triple, a refused entry's transaction. A finding
    /// with no tx to judge it by (an index key that doesn't decode, a log
    /// that can't be read) is always reported. `None`: everything.
    pub since: Option<TxId>,
}

/// How serious a [`Finding`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Kala is broken: a bug or disk damage.
    Error,
    /// The data breaks a declared schema rule.
    Warning,
}

/// One of the four stored indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexName {
    /// The forward index `E|entity|attr|value|^tick|^tx`.
    E,
    /// The reverse index `R|target|attr|source|^tick|^tx`.
    R,
    /// The latest-revision forward index `E|entity|attr|value|^tick`.
    ELatest,
    /// The latest-revision reverse index `R|target|attr|source|^tick`.
    RLatest,
}

/// One problem doctor found.
#[derive(Clone, Debug, PartialEq)]
pub enum Finding {
    /// The log ends in a torn or damaged transaction that `World::open`
    /// would cut. `offset` is where the valid prefix ends: the start of that
    /// transaction (or of the zero-filled tail).
    IncompleteTail {
        /// Byte offset in the file.
        offset: u64,
    },
    /// A transaction whose body checksum or `Commit` checksum doesn't match
    /// (or, at a begin record whose own checksum fails and isn't a
    /// zero-filled tail, the transaction expected there). It is skipped.
    ChecksumMismatch {
        /// The transaction's tx.
        tx: TxId,
        /// Byte offset of its begin record in the file.
        offset: u64,
    },
    /// A transaction whose body has no `Commit`. It is skipped.
    MissingCommit {
        /// The transaction's tx.
        tx: TxId,
        /// Byte offset of its begin record in the file.
        offset: u64,
    },
    /// A transaction's tx isn't the next one.
    TxGap {
        /// The tx that was next.
        expected: TxId,
        /// The tx found.
        found: TxId,
    },
    /// An entry ID isn't the next one.
    EntryIdGap {
        /// The entry ID that was next.
        expected: EntryId,
        /// The entry ID found.
        found: EntryId,
    },
    /// An entry uses an entity or edge ID that no earlier entry defines.
    DanglingId {
        /// The entry.
        entry: EntryId,
        /// The undefined ID.
        id: EntityId,
    },
    /// An entry uses an attribute ID the schema doesn't define.
    DanglingAttr {
        /// The entry.
        entry: EntryId,
        /// The undefined attribute ID.
        attr: AttrId,
    },
    /// A correction that points at a missing or non-fact entry, isn't at its
    /// chain's current key, or corrects an already deleted chain.
    BadCorrection {
        /// The correction entry.
        entry: EntryId,
        /// What is wrong with it.
        reason: String,
    },
    /// A correction whose revision isn't its chain's next.
    RevisionSkip {
        /// The correction entry.
        entry: EntryId,
        /// The chain's base entry.
        of: EntryId,
        /// The chain's next revision.
        expected: u32,
        /// The revision the entry carries.
        found: u32,
    },
    /// An entry that breaks any other rule a live writer obeys (a no-op
    /// fact, a value of the wrong type, an unparseable entry, ...).
    InvalidEntry {
        /// The entry (for an unparseable one, the ID it should have had).
        entry: EntryId,
        /// What is wrong with it.
        reason: String,
    },
    /// A stored index row the rebuild from the log doesn't have, one it has
    /// that the stored index lacks, or one it has with another mark.
    IndexMismatch {
        /// The index.
        index: IndexName,
        /// Which row and how it differs.
        detail: String,
    },
    /// A stored index key that doesn't decode as a key of its index.
    IndexDecode {
        /// The index.
        index: IndexName,
        /// The key's bytes.
        key: Vec<u8>,
    },
    /// A file-backed world's log can't be read or scanned at all (deleted,
    /// unreadable, a damaged header). Nothing is replayed and the indexes
    /// aren't compared.
    LogUnreadable {
        /// Why it can't be read.
        reason: String,
    },
    /// Two values of a side labelled cardinality one hold at the same time
    /// (a warning: cardinality is a label, not a rule).
    CardinalityOverlap(Overlap),
    /// A property assert on an edge, current at the canon, at a tick where
    /// the edge's own triple doesn't hold at that canon.
    EdgePropertyOnUnassertedEdge {
        /// The property's current entry (its base fact, or the last
        /// `Revised` entry of its chain).
        entry: EntryId,
        /// The edge the property is on.
        edge: EdgeId,
        /// The property's attribute name.
        attr: String,
        /// The property assert's tick.
        tick: Tick,
    },
    /// An entry storing a value of another type than its attribute's. It is
    /// skipped.
    WrongValueType {
        /// The entry.
        entry: EntryId,
        /// The attribute's name.
        attr: String,
        /// The attribute's declared type.
        expected: ValueType,
        /// The stored value's type.
        got: ValueType,
    },
    /// An entry retracting an event attribute (events are never retracted
    /// in story time). It is skipped.
    RetractedEvent {
        /// The entry.
        entry: EntryId,
        /// The attribute's name.
        attr: String,
    },
}

impl Finding {
    /// How serious the finding is.
    pub fn severity(&self) -> Severity {
        match self {
            Finding::IncompleteTail { .. }
            | Finding::ChecksumMismatch { .. }
            | Finding::MissingCommit { .. }
            | Finding::TxGap { .. }
            | Finding::EntryIdGap { .. }
            | Finding::DanglingId { .. }
            | Finding::DanglingAttr { .. }
            | Finding::BadCorrection { .. }
            | Finding::RevisionSkip { .. }
            | Finding::InvalidEntry { .. }
            | Finding::IndexMismatch { .. }
            | Finding::IndexDecode { .. }
            | Finding::LogUnreadable { .. } => Severity::Error,
            Finding::CardinalityOverlap(_)
            | Finding::EdgePropertyOnUnassertedEdge { .. }
            | Finding::WrongValueType { .. }
            | Finding::RetractedEvent { .. } => Severity::Warning,
        }
    }
}

/// What doctor found, in the order it found it: log scan findings, then
/// replay findings (refused entries, schema warnings among them) in log
/// order, then index findings index by index, then cardinality overlaps
/// (attribute by attribute in schema order, as [`View::overlaps`] sorts
/// them), then edge-property findings by entry.
///
/// [`View::overlaps`]: crate::View::overlaps
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Every finding.
    pub findings: Vec<Finding>,
}

impl Report {
    /// Whether doctor found nothing at all (no error, no warning).
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// The [`Severity::Error`] findings.
    pub fn errors(&self) -> Vec<&Finding> {
        self.of_severity(Severity::Error)
    }

    /// The [`Severity::Warning`] findings.
    pub fn warnings(&self) -> Vec<&Finding> {
        self.of_severity(Severity::Warning)
    }

    fn of_severity(&self, s: Severity) -> Vec<&Finding> {
        self.findings.iter().filter(|f| f.severity() == s).collect()
    }
}

/// A way to damage one stored index in memory, for doctor's tests (see
/// [`World::tamper_index`]). Not part of the supported API.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tamper {
    /// Inserts `key` as is, marked as an assert.
    InsertRawKey {
        /// The index.
        index: IndexName,
        /// The raw key bytes.
        key: Vec<u8>,
    },
    /// Removes the `nth` row (in key order).
    RemoveRow {
        /// The index.
        index: IndexName,
        /// The row number, from 0.
        nth: usize,
    },
    /// Flips the `nth` row's op (in key order): assert <-> retract; a
    /// `DELETED` marker becomes an assert.
    FlipOp {
        /// The index.
        index: IndexName,
        /// The row number, from 0.
        nth: usize,
    },
}

/// Checks a world: its log, a replay of the log, and its four stored
/// indexes against a rebuild from the log.
///
/// A file-backed world's log is re-read from disk (without taking its lock),
/// so damage that happened after the world was opened is found, and the
/// world keeps working meanwhile. An in-memory world's kept entries are
/// replayed instead. Read-only: it writes nothing, cuts nothing and changes
/// nothing in the world; it works on a world whose log writer failed. Never
/// panics: anything it can't read is a finding. If the log can't be read at
/// all, that is [`Finding::LogUnreadable`] and nothing else is checked
/// (there is nothing to rebuild the indexes from).
pub fn doctor(world: &World, opts: &DoctorOptions) -> Report {
    let mut found = Vec::new();
    let txns = match world.log_path() {
        Some(path) => match std::fs::read(path)
            .map_err(Error::from)
            .and_then(|bytes| log::survey(&bytes))
        {
            Ok((txns, scanned)) => {
                found.extend(scanned);
                Some(txns)
            }
            Err(e) => {
                let f = Finding::LogUnreadable {
                    reason: format!("the log file {} can't be scanned: {e}", path.display()),
                };
                found.push((None, f));
                None
            }
        },
        None => Some(world.kept_txns()),
    };
    if let Some(rebuilt) = txns.and_then(|txns| rebuild(txns, &mut found)) {
        compare_indexes(world, &rebuilt, &mut found);
        schema_rules(&rebuilt, opts, &mut found);
    }
    finish(found, opts)
}

/// Checks a log file that may not open as a world: its transactions and a
/// replay of its entries (there are no stored indexes to compare). Never
/// modifies the file or its directory.
///
/// Fails only if the file can't be scanned at all: [`Error::Io`] if it
/// can't be read, [`Error::BadMagic`], [`Error::UnsupportedVersion`], or
/// [`Error::Corrupt`] for a truncated header. Everything else is a finding.
pub fn doctor_log(path: &Path, opts: &DoctorOptions) -> Result<Report, Error> {
    let bytes = std::fs::read(path)?;
    let (txns, mut found) = log::survey(&bytes)?;
    if let Some(rebuilt) = rebuild(txns, &mut found) {
        schema_rules(&rebuilt, opts, &mut found);
    }
    Ok(finish(found, opts))
}

/// Applies the `since` filter and builds the report.
fn finish(found: Vec<TaggedFinding>, opts: &DoctorOptions) -> Report {
    let findings = found
        .into_iter()
        .filter(|(tx, _)| match (opts.since, tx) {
            (Some(since), Some(tx)) => *tx > since,
            _ => true,
        })
        .map(|(_, f)| f)
        .collect();
    Report { findings }
}

/// Replays `txns` (the genesis first) into a new in-memory world, recording
/// every refused entry, and returns the rebuilt world; `None` if there is no
/// usable genesis transaction to start from.
fn rebuild(txns: Vec<SurveyedTxn>, found: &mut Vec<TaggedFinding>) -> Option<World> {
    let mut txns = txns.into_iter();
    let genesis = match txns.next() {
        Some(t) if t.tx == TxId(0) => t,
        // Nothing replays without a schema; the scan already reported why
        // the genesis is missing (a gap, a bad checksum, ...), if it could.
        _ => {
            let f = Finding::InvalidEntry {
                entry: EntryId(1),
                reason: "no committed genesis transaction tx#0: nothing can be replayed".to_owned(),
            };
            found.push((Some(TxId(0)), f));
            return None;
        }
    };
    let mut world = match World::from_genesis(&genesis.entries) {
        Ok(w) => w,
        Err(e) => {
            let f = Finding::InvalidEntry {
                entry: genesis.entries.first().map_or(EntryId(1), |e| e.id),
                reason: e.to_string(),
            };
            found.push((Some(TxId(0)), f));
            return None;
        }
    };
    for t in txns {
        let tx = t.tx;
        world.replay_txn_collecting(tx, &t.entries, |entry, fault| {
            found.push((Some(tx), finding_of(entry, fault)));
        });
    }
    Some(world)
}

/// The finding for an entry replay refused.
fn finding_of(entry: EntryId, fault: ReplayFault) -> Finding {
    match fault {
        ReplayFault::DanglingId(id) => Finding::DanglingId { entry, id },
        ReplayFault::DanglingAttr(attr) => Finding::DanglingAttr { entry, attr },
        ReplayFault::BadCorrection(reason) => Finding::BadCorrection { entry, reason },
        ReplayFault::RevisionSkip {
            of,
            expected,
            found,
        } => Finding::RevisionSkip {
            entry,
            of,
            expected,
            found,
        },
        // Schema-rule warnings: the data breaks a declared rule, which a
        // live writer refuses but which says nothing about Kala's own state.
        ReplayFault::Refused(Error::WrongValueType {
            attr,
            expected,
            got,
        }) => Finding::WrongValueType {
            entry,
            attr,
            expected,
            got,
        },
        ReplayFault::Refused(Error::RetractEvent(attr)) => Finding::RetractedEvent { entry, attr },
        ReplayFault::Refused(e) => Finding::InvalidEntry {
            entry,
            reason: e.to_string(),
        },
        ReplayFault::Invalid(reason) => Finding::InvalidEntry { entry, reason },
    }
}

/// The schema-rule checks on `rebuilt` (the world replayed from the log) at
/// `opts.canon`: cardinality overlaps, then edge properties on an
/// unasserted edge, each tagged for `since` with the newest tx that touched
/// it. (Wrong value types and retracted events come from the replay.)
fn schema_rules(rebuilt: &World, opts: &DoctorOptions, found: &mut Vec<TaggedFinding>) {
    let view = rebuilt.at_canon(opts.canon.unwrap_or(rebuilt.latest_tx()));
    for (tx, o) in view.tagged_overlaps() {
        found.push((Some(tx), Finding::CardinalityOverlap(o)));
    }
    for (tx, entry, edge, attr, tick) in view.unasserted_edge_properties() {
        let f = Finding::EdgePropertyOnUnassertedEdge {
            entry,
            edge,
            attr,
            tick,
        };
        found.push((Some(tx), f));
    }
}

/// Compares each of `world`'s stored indexes with the same index of
/// `rebuilt` (the world replayed from the log), one index at a time.
fn compare_indexes(world: &World, rebuilt: &World, found: &mut Vec<TaggedFinding>) {
    let (stored, fresh) = (world.indexes(), rebuilt.indexes());
    compare(
        IndexName::E,
        stored.e,
        fresh.e,
        describe_e,
        key::e_key_tx,
        found,
    );
    compare(
        IndexName::R,
        stored.r,
        fresh.r,
        describe_r,
        key::e_key_tx,
        found,
    );
    // A latest-revision key has no tx: `since` judges it by the tx of the
    // entry it holds, the newest entry at that key in a main index. The
    // rebuilt one gives it for a row the log has; a stored row the log
    // lacks holds the entry its own world's main index has there. Only the
    // `since` tag reads the stored main index; rows are never compared
    // across indexes.
    let latest_tx = |stored_main: &Index, fresh_main: &Index, k: &[u8]| {
        newest_tx(fresh_main, k).max(newest_tx(stored_main, k))
    };
    compare(
        IndexName::ELatest,
        stored.e_latest,
        fresh.e_latest,
        describe_e_latest,
        |k: &[u8]| latest_tx(stored.e, fresh.e, k),
        found,
    );
    compare(
        IndexName::RLatest,
        stored.r_latest,
        fresh.r_latest,
        describe_r_latest,
        |k: &[u8]| latest_tx(stored.r, fresh.r, k),
        found,
    );
}

/// Compares one stored index with its rebuild: a stored key that doesn't
/// decode (`describe` gives `None`) is [`Finding::IndexDecode`] and left out
/// of the comparison; every extra, different or missing row is
/// [`Finding::IndexMismatch`], tagged with `tx_of` its key.
fn compare<V: PartialEq + Debug>(
    index: IndexName,
    stored: &BTreeMap<Vec<u8>, V>,
    rebuilt: &BTreeMap<Vec<u8>, V>,
    describe: fn(&[u8]) -> Option<String>,
    tx_of: impl Fn(&[u8]) -> Option<TxId>,
    found: &mut Vec<TaggedFinding>,
) {
    let mismatch = |detail: String| Finding::IndexMismatch { index, detail };
    for (k, v) in stored {
        let Some(row) = describe(k) else {
            found.push((
                None,
                Finding::IndexDecode {
                    index,
                    key: k.clone(),
                },
            ));
            continue;
        };
        match rebuilt.get(k) {
            None => found.push((
                tx_of(k),
                mismatch(format!("{row} ({v:?}) is not in the log")),
            )),
            Some(want) if want != v => found.push((
                tx_of(k),
                mismatch(format!("{row} is {v:?}, the log says {want:?}")),
            )),
            Some(_) => {}
        }
    }
    for (k, want) in rebuilt {
        if !stored.contains_key(k) {
            // Rebuilt keys are built by the key encoders, so they decode.
            let row = describe(k).unwrap_or_else(|| format!("{k:02x?}"));
            found.push((tx_of(k), mismatch(format!("{row} ({want:?}) is missing"))));
        }
    }
}

/// The tx of the newest entry at latest-revision key `at_key` (an `E` or `R`
/// key without its `^tx`) in a main index; `None` if it has none.
fn newest_tx(main: &Index, at_key: &[u8]) -> Option<TxId> {
    // Within one key, `^tx` sorts the newest entry first.
    let (k, _) = main
        .range::<[u8], _>((Bound::Included(at_key), Bound::Unbounded))
        .next()?;
    let exact = k.starts_with(at_key) && k.len() == at_key.len() + 8;
    exact.then(|| key::e_key_tx(k)).flatten()
}

fn describe_e(k: &[u8]) -> Option<String> {
    let (entity, attr, value, tick, tx) = world::decode_e_row(k)?;
    Some(format!(
        "E row {entity:?} {attr:?} {value:?} @{} tx#{}",
        tick.0, tx.0
    ))
}

fn describe_r(k: &[u8]) -> Option<String> {
    let (target, attr, source, tick, tx) = key::decode_r_key(k).ok()?;
    Some(format!(
        "R row {target:?} {attr:?} {source:?} @{} tx#{}",
        tick.0, tx.0
    ))
}

fn describe_e_latest(k: &[u8]) -> Option<String> {
    let (entity, attr, value, tick) = key::decode_latest_e_key(k).ok()?;
    let value = Value::decode(value).ok()?;
    Some(format!(
        "E_latest row {entity:?} {attr:?} {value:?} @{}",
        tick.0
    ))
}

fn describe_r_latest(k: &[u8]) -> Option<String> {
    let (target, attr, source, tick) = key::decode_latest_r_key(k)?;
    Some(format!(
        "R_latest row {target:?} {attr:?} {source:?} @{}",
        tick.0
    ))
}
