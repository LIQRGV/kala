//! The journal: a world's position on the author timeline. The committed
//! log entries it keeps, the latest tx, the correction chains and the log
//! file they are appended to.

use super::*;

/// The correction state of one revision chain (keyed by its base entry in
/// [`Journal::chains`]). A chain no correction has touched has none: see
/// [`Chain::untouched`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Chain {
    /// The chain's last revision (0 = only the base entry).
    pub(super) rev: u32,
    /// The entry holding the chain's current fact (the base or its last
    /// `Revised`), or `None` if the last correction deleted it.
    pub(super) current: Option<EntryId>,
}

impl Chain {
    /// The state of the chain of base entry `base` before any correction.
    fn untouched(base: EntryId) -> Chain {
        Chain {
            rev: 0,
            current: Some(base),
        }
    }
}

/// Everything a world knows about what was committed, in author order: the
/// kept entries, the latest tx, the correction chains and, for a
/// file-backed world, the log writer. It holds no schema ([`Catalog`]) and
/// no facts by key ([`Indexes`]).
#[derive(Debug)]
pub(super) struct Journal {
    /// The last committed transaction (0 = none yet).
    latest_tx: TxId,
    /// The correction state of every revision chain a correction touched,
    /// by base entry. Derived from the log; maintained on commit and replay.
    chains: HashMap<EntryId, Chain>,
    /// Every committed log entry, in log order: entry `i` has ID `i + 1`.
    /// Both kinds of world keep them (milestone 0), and an in-memory world
    /// numbers them exactly as a file-backed one, genesis included. Doctor
    /// replays these for an in-memory world.
    entries: Vec<EntryInfo>,
    /// The log file, for a file-backed world; `None` in memory. Visible to
    /// the module for the `test_hooks` (fault injection, rebuilds).
    pub(super) log: Option<LogWriter>,
    /// For a world opened read-only (no log writer): the file it was read
    /// from and the last committed transaction this handle applied.
    pub(super) reader: Option<LogReader>,
    /// Bytes of the log file read since the handle was opened (catch-ups
    /// and commit checks): what a test measures a catch-up's cost by.
    pub(super) bytes_read: u64,
}

/// Where a read-only world's log is and how far the handle has read it.
#[derive(Clone, Debug)]
pub(super) struct LogReader {
    /// The log file's path: every catch-up opens it anew.
    pub(super) path: PathBuf,
    /// The last committed transaction applied.
    pub(super) tip: Option<log::ChainTip>,
}

impl Journal {
    /// An empty journal with no log file: nothing committed yet.
    pub(super) fn in_memory() -> Journal {
        Journal {
            latest_tx: TxId(0),
            chains: HashMap::new(),
            entries: Vec::new(),
            log: None,
            reader: None,
            bytes_read: 0,
        }
    }

    /// The last committed transaction (0 = none yet).
    pub(super) fn latest_tx(&self) -> TxId {
        self.latest_tx
    }

    /// Moves the latest tx: to a transaction just committed or replayed,
    /// or (doctor's collecting replay only) to just before the one about
    /// to be replayed.
    pub(super) fn set_latest_tx(&mut self, tx: TxId) {
        self.latest_tx = tx;
    }

    /// The transaction number the next commit gets.
    pub(super) fn next_tx(&self) -> TxId {
        TxId(self.latest_tx.0 + 1)
    }

    /// The ID the next appended entry gets.
    pub(super) fn next_entry(&self) -> EntryId {
        // A Vec's length always fits a u64.
        EntryId(self.entries.len() as u64 + 1)
    }

    /// The link the next transaction chains from: the chained checksum of
    /// the last committed transaction, or the header's before genesis (see
    /// "The checksum chain" in [`crate::log`]). Taken from the kept entries,
    /// which hold exactly what was committed: a failed append records
    /// nothing, so the chain can't run ahead of the file.
    pub(super) fn chain_checksum(&self) -> u32 {
        match self.entries.last().map(|e| &e.body) {
            Some(EntryBody::Commit { checksum, .. }) => *checksum,
            _ => log::header_checksum(),
        }
    }

    /// The last committed transaction of a file-backed world's log, as this
    /// handle knows it; `None` in memory.
    pub(super) fn chain_tip(&self) -> Option<log::ChainTip> {
        match &self.log {
            Some(log) => log.tip(),
            None => self.reader.as_ref().and_then(|r| r.tip),
        }
    }

    /// Makes this the journal of a read-only world read from `reader`'s
    /// file up to its tip.
    pub(super) fn set_reader(&mut self, reader: Option<LogReader>) {
        self.reader = reader;
    }

    /// Takes the log file's writer lock for one commit, waiting up to the
    /// busy timeout ([`Error::WorldLocked`] after it). `None` for a world
    /// with no log writer (in memory, or read-only): nothing to lock.
    pub(super) fn lock(&self) -> Result<Option<WriterLock>, Error> {
        self.log.as_ref().map(LogWriter::lock).transpose()
    }

    /// How long [`Journal::lock`] waits (the default for a world with no
    /// log writer, which never locks).
    pub(super) fn busy_timeout(&self) -> std::time::Duration {
        self.log
            .as_ref()
            .map_or(log::DEFAULT_BUSY_TIMEOUT, LogWriter::busy_timeout)
    }

    /// Sets how long [`Journal::lock`] waits; ignored without a log writer.
    pub(super) fn set_busy_timeout(&mut self, timeout: std::time::Duration) {
        if let Some(log) = self.log.as_mut() {
            log.set_busy_timeout(timeout);
        }
    }

    /// Whether the log file is exactly as long as the log this handle has
    /// applied (one `stat`): then nothing was appended since. It can't see
    /// a cut followed by a rewrite of the same length; only reading the
    /// tail ([`Journal::read_tail`]) proves the prefix. `true` in memory;
    /// `false` if the file can't be examined.
    pub(super) fn file_ends_at_tip(&self) -> bool {
        let (len, tip) = match (&self.log, &self.reader) {
            (Some(log), _) => (log.file_len(), log.tip()),
            (None, Some(reader)) => (log::file_len(&reader.path), reader.tip),
            (None, None) => return true,
        };
        matches!((len, tip), (Ok(len), Some(tip)) if len == tip.end)
    }

    /// What the log file holds past the last transaction this handle
    /// applied (see `log::Tail`); `None` in memory. `held` is the writer
    /// lock if the caller has it. Applies nothing and modifies nothing.
    pub(super) fn read_tail(&mut self, held: Option<&WriterLock>) -> Result<Option<Tail>, Error> {
        let next_entry = self.next_entry().0;
        let tail = match (&self.log, &self.reader) {
            (Some(log), _) => log.read_tail(held, next_entry)?,
            (None, Some(reader)) => {
                let tip = reader.tip.ok_or(Error::LogDiverged)?;
                log::read_tail_at(&reader.path, &tip, next_entry)?
            }
            (None, None) => return Ok(None),
        };
        self.bytes_read += tail.bytes_read;
        Ok(Some(tail))
    }

    /// Moves the handle's place in the file past transactions it has just
    /// applied from a tail: `tip` is the last of them.
    pub(super) fn advance(&mut self, tip: log::ChainTip) {
        match (self.log.as_mut(), self.reader.as_mut()) {
            (Some(log), _) => log.advance(tip),
            (None, Some(reader)) => reader.tip = Some(tip),
            (None, None) => {}
        }
    }

    /// Cuts an incomplete tail off the log file under the lock (see
    /// `LogWriter::cut_tail`).
    pub(super) fn cut_tail(
        &mut self,
        held: &WriterLock,
        tail: &[u8],
        cause: CutCause,
    ) -> Result<RecoveryReport, Error> {
        match self.log.as_mut() {
            Some(log) => log.cut_tail(held, tail, cause),
            None => Ok(RecoveryReport::default()),
        }
    }

    /// Test hook point: an armed fault fires now that a commit holds the
    /// lock (see `LogWriter::after_lock`).
    pub(super) fn after_lock(&mut self, held: &WriterLock) {
        if let Some(log) = self.log.as_mut() {
            log.after_lock(held);
        }
    }

    /// Replaces what this journal knows was committed (the kept entries,
    /// the latest tx, the correction chains) by `rebuilt`'s, keeping its
    /// own log file, reader and counters: the committed state rolled back
    /// to a replay of a prefix of its entries.
    pub(super) fn restore_committed(&mut self, rebuilt: Journal) {
        self.latest_tx = rebuilt.latest_tx;
        self.chains = rebuilt.chains;
        self.entries = rebuilt.entries;
    }

    /// Keeps the entries of committed transaction `tx` (in log order, right
    /// after the ones already kept) for [`World::entry`].
    pub(super) fn record(&mut self, entries: impl IntoIterator<Item = LogEntry>, tx: TxId) {
        for e in entries {
            self.record_one(e, tx);
        }
    }

    /// Keeps one entry of committed transaction `tx` (see
    /// [`Journal::record`]).
    pub(super) fn record_one(&mut self, e: LogEntry, tx: TxId) {
        // `World::entry` finds an entry by position, so IDs must be
        // consecutive from 1: the log scan and `encode_txn` from
        // `next_entry` guarantee it.
        debug_assert_eq!(
            e.id,
            self.next_entry(),
            "invariant: entries are kept in log order without gaps"
        );
        self.keep(e, tx);
    }

    /// Keeps `entry` as part of transaction `tx` without checking its ID
    /// continues the kept ones: for genesis entries as a file has them and
    /// for doctor's collecting replay, whose kept entries may have ID gaps
    /// (see [`Journal::kept_entry`]).
    pub(super) fn keep(&mut self, entry: LogEntry, tx: TxId) {
        self.entries.push(EntryInfo {
            id: entry.id,
            tx,
            body: entry.body,
        });
    }

    /// Drops every kept entry (a world holding just its genesis, about to
    /// keep the genesis entries as its file has them).
    pub(super) fn clear_entries(&mut self) {
        self.entries.clear();
    }

    /// Every kept entry, in entry-ID order.
    pub(super) fn entries(&self) -> &[EntryInfo] {
        &self.entries
    }

    /// The kept entry with ID `id` (see [`World::entry`]), borrowed.
    pub(super) fn kept_entry(&self, id: EntryId) -> Option<&EntryInfo> {
        let i = usize::try_from(id.0.checked_sub(1)?).ok()?;
        match self.entries.get(i) {
            Some(e) if e.id == id => Some(e),
            // Only a doctor rebuild keeps entries with ID gaps (it leaves
            // refused entries out); they still ascend, so search by ID.
            _ => {
                let i = self.entries.binary_search_by_key(&id, |e| e.id).ok()?;
                self.entries.get(i)
            }
        }
    }

    /// The kept entries as log entries (without their tx): what a replay
    /// takes.
    pub(super) fn log_entries(&self) -> Vec<LogEntry> {
        self.entries.iter().map(as_log_entry).collect()
    }

    /// Every kept entry grouped by transaction, in log order: what doctor
    /// replays for an in-memory world (a file-backed one is re-read from
    /// disk instead).
    pub(super) fn kept_txns(&self) -> Vec<SurveyedTxn> {
        let mut txns: Vec<SurveyedTxn> = Vec::new();
        for e in &self.entries {
            let entry = as_log_entry(e);
            match txns.last_mut() {
                Some(t) if t.tx == e.tx => t.entries.push(entry),
                _ => txns.push(SurveyedTxn {
                    tx: e.tx,
                    entries: vec![entry],
                }),
            }
        }
        txns
    }

    /// The fact the kept entry `current` (a chain's current fact) says.
    pub(super) fn current_fact(&self, current: EntryId) -> FactFields<'_> {
        self.kept_entry(current)
            .and_then(|info| as_fact(&info.body))
            .expect("invariant: a chain's current entry is a committed Fact or Revised entry")
    }

    /// The correction state of the chain of base entry `base`.
    pub(super) fn chain(&self, base: EntryId) -> Chain {
        self.chains
            .get(&base)
            .copied()
            .unwrap_or(Chain::untouched(base))
    }

    /// Records that revision `rev` of the chain of base entry `of` was
    /// applied: its current fact is now in entry `current`, or the chain is
    /// deleted (`None`).
    pub(super) fn set_chain(&mut self, of: EntryId, rev: u32, current: Option<EntryId>) {
        self.chains.insert(of, Chain { rev, current });
    }

    /// Replay: the state of the chain a correction names as `of`, which
    /// must be an earlier base `Fact` entry (a live correction only names
    /// committed entries, and resolves a chain to its base).
    pub(super) fn replay_chain(&self, of: EntryId) -> Result<Chain, String> {
        let base = self
            .kept_entry(of)
            .filter(|b| b.tx <= self.latest_tx)
            .ok_or_else(|| format!("corrects entry#{}, not an earlier entry", of.0))?;
        if !matches!(base.body, EntryBody::Fact { .. }) {
            return Err(format!("corrects entry#{}, not a base fact", of.0));
        }
        Ok(self.chain(of))
    }

    /// Replay: the op and precision of the committed fact entry `current`
    /// (a chain's current fact) if `fact` is at its key, else `None`.
    /// Values compare by encoding: exact bytes.
    pub(super) fn op_if_at_key(&self, current: EntryId, fact: &FactDef) -> Option<(Op, Precision)> {
        let (entity, attr, value, tick, op, precision) = self.current_fact(current);
        // Committed values were encoded once before, so they encode.
        let same = (entity, attr, tick) == (fact.entity, fact.attr, fact.tick)
            && value.encode().is_ok_and(|v| v == fact.value);
        same.then_some((op, precision))
    }

    /// Whether any kept entry is a retract of attribute `attr` (a base fact
    /// or a revised one, whatever a later correction did to it).
    pub(super) fn has_retract_of(&self, attr: AttrId) -> bool {
        self.entries.iter().any(|e| {
            as_fact(&e.body).is_some_and(|(_, a, _, _, op, _)| a == attr && op == Op::Retract)
        })
    }

    /// Makes `writer` the log file every later commit is appended to.
    pub(super) fn attach_log(&mut self, writer: LogWriter) {
        self.log = Some(writer);
    }

    /// The path of a file-backed world's log; `None` in memory.
    pub(super) fn log_path(&self) -> Option<&Path> {
        self.log.as_ref().map(LogWriter::path)
    }

    /// `Err` if a log append failed earlier (never in memory).
    pub(super) fn check_log(&self) -> Result<(), Error> {
        self.log.as_ref().map_or(Ok(()), LogWriter::check)
    }

    /// Appends one encoded transaction to the log file and fsyncs it, under
    /// the writer lock `held` (see [`Journal::lock`]); does nothing in
    /// memory. Refused with [`Error::LogDiverged`], nothing written and the
    /// log writer marked failed, unless the file ends where this handle's
    /// log does and `txn` is the transaction that follows it.
    pub(super) fn append(
        &mut self,
        held: Option<&WriterLock>,
        txn: &EncodedTxn,
    ) -> Result<(), Error> {
        let (chain, next_entry, next_tx) =
            (self.chain_checksum(), self.next_entry(), self.next_tx());
        let Some(log) = self.log.as_mut() else {
            return Ok(());
        };
        // The two views of the log agree: the file's last committed
        // transaction is the last one kept here, and `txn` was numbered to
        // follow it. True by construction; checked for real, because a
        // transaction appended to a file that isn't where this handle
        // thinks it is would be a duplicate tx or a broken link (see
        // `LogWriter::continues_file` for how that could come about).
        let follows = log
            .tip()
            .is_some_and(|tip| tip.checksum == chain && tip.tx.0.checked_add(1) == Some(next_tx.0))
            && txn.entries.first().is_some_and(|e| e.id == next_entry)
            && matches!(
                txn.entries.last().map(|e| &e.body),
                Some(EntryBody::Commit { tx, .. }) if *tx == next_tx
            );
        if !follows {
            return Err(log.refuse(Error::LogDiverged));
        }
        log.append(
            held.expect("invariant: a commit to a log file holds its writer lock"),
            txn,
        )
    }
}

/// A fact as a `Fact` or `Revised` entry says it: `(entity, attr, value,
/// tick, op, precision)`.
pub(super) type FactFields<'b> = (EntityId, AttrId, &'b Value, Tick, Op, Precision);

/// The fact a `Fact` or `Revised` entry says; `None` for any other entry.
pub(super) fn as_fact(body: &EntryBody) -> Option<FactFields<'_>> {
    match body {
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
        }
        | EntryBody::Revised {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
            ..
        } => Some((*entity, *attr, value, *tick, *op, *precision)),
        _ => None,
    }
}

/// A kept entry as the log entry it was read from or appended as.
pub(super) fn as_log_entry(e: &EntryInfo) -> LogEntry {
    LogEntry {
        id: e.id,
        body: e.body.clone(),
    }
}
