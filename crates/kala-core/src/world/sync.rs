//! Sharing a log file with other handles: catching up with what they
//! appended, and what a commit does once it holds the writer lock.
//!
//! A world handle is a copy of a prefix of its log file. Other handles, in
//! this process or others, may append to the file at any time; none may
//! append or cut without the file's lock, and the lock is held only for
//! the length of one commit (or one open, or the creation of the file).
//!
//! **Catch-up** ([`World::catch_up`]) reads what follows the last
//! transaction this handle applied and applies it as a replay would. It
//! never waits and never takes the writer lock:
//!
//! - when the file is longer than the handle's log, the tail is read under
//!   the file's **shared** lock, taken without waiting. Writers hold the
//!   exclusive lock across write, fsync and any rollback, so a reader never
//!   applies a transaction that isn't durable yet (and that a failed fsync
//!   would take back). If a writer holds its lock right now, the catch-up
//!   applies nothing ("not yet") and a later one finds what was committed;
//! - an incomplete last transaction (torn, or all there but failing its
//!   checksum) is left alone: nothing of it is applied, nothing on disk is
//!   touched. Only a commit, under the writer lock, cuts one;
//! - a file that no longer holds the handle's last transaction (shorter,
//!   or other bytes there: see "The checksum chain" in [`crate::log`]) is
//!   [`Error::LogDiverged`], and the handle stops following the file;
//! - anything the recovery rules call corrupt is [`Error::Corrupt`].
//!
//! **Under the lock** (a commit) the same read is authoritative: no writer
//! is appending, so an incomplete tail is a dead writer's. A torn one is
//! cut (quarantine file and [`World::recovery`] report, as
//! [`World::open`] does); a damaged one is cut only once the caller has
//! seen it reported (by [`World::open`], or by a commit's refusal,
//! [`Error::RecoveryRequired`]) and called
//! [`World::acknowledge_recovery`]. No open cuts a damaged tail, so every
//! handle on the file meets it here and none writes past it unasked.

use super::*;
use std::time::Duration;

impl World {
    /// Applies the transactions other handles committed to this world's log
    /// file since this handle last looked, and returns how many there were.
    /// Afterwards the handle answers every read, at every canon, as a world
    /// opened now would. Works on worlds from [`World::open`],
    /// [`World::create`] and [`World::open_read_only`]; an in-memory world
    /// has nothing to catch up with (`Ok(0)`).
    ///
    /// The new transactions are validated exactly as [`World::open`]
    /// validates a log (same framing rules, same limits, the checksum chain
    /// continued from the last transaction this handle applied, every entry
    /// judged as a live write would be). The cost is that of the new
    /// transactions plus one re-read of the handle's last transaction, which
    /// proves the log before it unchanged; never that of the whole log.
    ///
    /// Never waits, never takes the writer lock and never modifies the
    /// file. A transaction is applied only once it is durable: when the
    /// file has grown, its tail is read under the file's shared lock, and
    /// if a writer holds its exclusive lock at that moment (a commit, an
    /// open or a cut in progress) nothing is applied and this returns
    /// `Ok(0)`; a later call applies what that writer committed. When the
    /// file hasn't grown no lock is taken at all. A last transaction that
    /// is incomplete (cut short, or all there but failing its checksum) is
    /// left alone and not counted. Only a commit, which holds the writer
    /// lock, cuts such a tail (see [`Txn::commit`]).
    ///
    /// Fails with:
    /// - [`Error::LogDiverged`] if the file is no longer the log this handle
    ///   applied (shorter than it, or its last transaction replaced: another
    ///   process cut a tail this handle had applied, or the file was
    ///   replaced). Nothing is applied, and from then on every `catch_up`
    ///   and every write on this handle fails with the same error; reads
    ///   keep answering from what the handle had applied. Reopen the world.
    /// - [`Error::Corrupt`] if what follows is damaged other than as an
    ///   incomplete last transaction, doesn't continue the checksum chain,
    ///   or holds a transaction no live write could have produced. Nothing
    ///   stays applied (the handle is as it was before the call). If the
    ///   damage was only found while applying a transaction, the handle
    ///   also refuses every later `catch_up` and write with that error.
    /// - [`Error::Io`] if the file can't be read.
    pub fn catch_up(&mut self) -> Result<usize, Error> {
        self.gate.follows_log()?;
        match self.read_tail(None)? {
            Some(tail) => self.apply_tail(tail, None),
            None => Ok(0),
        }
    }

    /// Sets how long this handle waits for the log file's writer lock when
    /// another handle holds it (a commit in progress, an open or a recovery
    /// cut): each commit tries, sleeps briefly and tries again until
    /// `timeout` has passed, then fails with [`Error::WorldLocked`]. Zero
    /// means one attempt, no waiting. The default is
    /// [`DEFAULT_BUSY_TIMEOUT`](log::DEFAULT_BUSY_TIMEOUT) (5 s), or what
    /// [`World::open_waiting`] was given. Ignored by a world that never
    /// locks (in memory, read-only).
    pub fn set_busy_timeout(&mut self, timeout: Duration) {
        self.journal.set_busy_timeout(timeout);
    }

    /// How long this handle waits for the writer lock (see
    /// [`World::set_busy_timeout`]).
    pub fn busy_timeout(&self) -> Duration {
        self.journal.busy_timeout()
    }

    /// Runs `f` while this world's log file is locked against every writer
    /// (waiting up to the busy timeout for the lock; [`Error::WorldLocked`]
    /// after it, and `f` isn't run): for work on the file as a whole that no
    /// commit may interleave with, such as replacing it. The lock is
    /// released when `f` returns or panics. The world can't be used inside
    /// `f` (it is borrowed), and nothing is read or caught up here. A world
    /// with no log writer (in memory, read-only) runs `f` without a lock.
    ///
    /// A handle whose file was replaced under the lock finds out at its next
    /// commit or catch-up ([`Error::LogDiverged`]).
    pub fn with_writer_lock<T>(&mut self, f: impl FnOnce() -> T) -> Result<T, Error> {
        let _held = self.journal.lock()?;
        Ok(f())
    }

    /// Brings the log file to its committed end now, without committing
    /// anything: takes the writer lock (waiting up to the busy timeout),
    /// applies what other handles appended and cuts an incomplete last
    /// transaction exactly as a commit would (see [`Txn::commit`]): a torn
    /// one always, a damaged one only if this handle reported it (at
    /// [`World::open`] or by a refused commit) and
    /// [`World::acknowledge_recovery`] was called since. Returns
    /// [`World::recovery`], which then names the quarantine file.
    ///
    /// For a caller that acknowledged a damaged tail and wants it out of
    /// the file before (or without) its first commit, such as a command
    /// run with `--acknowledge-recovery`. With nothing to cut it changes
    /// nothing on disk.
    ///
    /// Fails as a commit's locking does, the file untouched:
    /// [`Error::ReadOnly`], [`Error::RecoveryUnacknowledged`],
    /// [`Error::RecoveryRequired`] (a damaged tail nobody acknowledged
    /// on this handle), [`Error::WorldLocked`], [`Error::LogDiverged`],
    /// [`Error::Corrupt`] or [`Error::Io`].
    pub fn cut_acknowledged_tail(&mut self) -> Result<&RecoveryReport, Error> {
        self.writable()?;
        if let Some(held) = self.lock_log()?
            && let Some(tail) = self.read_tail(Some(&held))?
        {
            self.apply_tail(tail, Some(&held))?;
        }
        Ok(self.recovery())
    }

    /// The catch-up [`World::begin`] makes: nothing but one `stat` when the
    /// file is exactly as long as the log this handle applied. Errors are
    /// left for the commit, which checks again under the lock (a diverged
    /// log is remembered and refuses the transaction's first write).
    pub(super) fn catch_up_if_grown(&mut self) {
        if !self.journal.file_ends_at_tip() {
            let _ = self.catch_up();
        }
    }

    /// Takes the writer lock for one commit and brings the handle to the
    /// file's state under it: [`World::lock_log`], then everything other
    /// handles appended is applied and an incomplete tail is cut (see
    /// [`World::apply_tail`]). `None`, having done nothing, for a world
    /// that refuses writes anyway or has no log file: the write then fails
    /// (or goes ahead in memory) as before.
    pub(super) fn lock_and_catch_up(&mut self) -> Result<Option<WriterLock>, Error> {
        if self.writable().is_err() {
            return Ok(None);
        }
        let Some(held) = self.lock_log()? else {
            return Ok(None);
        };
        if let Some(tail) = self.read_tail(Some(&held))? {
            self.apply_tail(tail, Some(&held))?;
        }
        Ok(Some(held))
    }

    /// Takes the log file's writer lock, waiting up to the busy timeout
    /// ([`Error::WorldLocked`] after it); `None` for a world with no log
    /// writer. The lock is released when the returned guard is dropped.
    pub(super) fn lock_log(&mut self) -> Result<Option<WriterLock>, Error> {
        let held = self.journal.lock()?;
        if let Some(held) = &held {
            self.journal.after_lock(held);
        }
        Ok(held)
    }

    /// What the log file holds past the last transaction this handle
    /// applied; `None` in memory. A diverged file stops the handle from
    /// following it (see [`World::catch_up`]).
    pub(super) fn read_tail(&mut self, held: Option<&WriterLock>) -> Result<Option<Tail>, Error> {
        let tail = self.journal.read_tail(held);
        if let Err(Error::LogDiverged) = tail {
            self.gate.stop_following(Error::LogDiverged);
        }
        tail
    }

    /// Applies the committed transactions of `tail` (from
    /// [`World::read_tail`]) exactly as a replay does and returns how many
    /// there were. If one doesn't replay, the world is rolled back to what
    /// it was (all of the tail's transactions, not just the bad one), stops
    /// following the file and returns the [`Error::Corrupt`].
    ///
    /// Then the incomplete last transaction, if there is one. Without the
    /// lock (`held` is `None`) it is ignored: it may be an append in
    /// progress. With it, a torn one is cut to a quarantine file, and a
    /// damaged one is cut only if this handle reported it before (its open,
    /// or a commit's refusal) and that was acknowledged; else this fails with
    /// [`Error::RecoveryRequired`], the file untouched. Either cut becomes
    /// the handle's [`World::recovery`] report.
    ///
    /// No transaction of this handle may be in progress with schema edits
    /// applied provisionally: the tail is applied to the committed schema.
    pub(super) fn apply_tail(
        &mut self,
        tail: Tail,
        held: Option<&WriterLock>,
    ) -> Result<usize, Error> {
        let applied = tail.txns();
        if let Some(tip) = tail.tip {
            let kept = self.journal.entries().len();
            let mut txn = ReplayTxn::default();
            for entry in &tail.entries {
                if let Err(e) = self.replay_entry(entry, &mut txn) {
                    self.roll_back_to(kept);
                    self.gate.stop_following(e.clone());
                    return Err(e);
                }
            }
            self.journal.advance(tip);
        }
        let Some(held) = held else {
            return Ok(applied);
        };
        // An in-memory buffer's length always fits a u64.
        let rest_len = tail.rest.len() as u64;
        // The tail starts where the committed log, now all applied, ends.
        let rest_at = self.journal.chain_tip().map_or(0, |tip| tip.end);
        match tail.cause {
            Some(CutCause::Damaged) if !self.gate.may_cut_damaged_tail((rest_at, rest_len)) => {
                return Err(Error::RecoveryRequired { bytes: rest_len });
            }
            Some(cause) => {
                let report = self.journal.cut_tail(held, &tail.rest, cause)?;
                self.gate.set_recovery(report);
            }
            None => {}
        }
        self.gate.no_damaged_tail();
        Ok(applied)
    }

    /// Rebuilds the world from its first `kept` kept entries, dropping
    /// whatever a failed catch-up applied after them. The log file, the
    /// write gate and the value hash stay this handle's. Costs a replay of
    /// those entries: only a corrupt tail gets here.
    fn roll_back_to(&mut self, kept: usize) {
        let mut entries = self.journal.log_entries();
        entries.truncate(kept);
        let rebuilt = World::replay_with_hash(&entries, self.indexes.content_hash)
            .expect("invariant: the entries a world applied replay cleanly");
        self.catalog = rebuilt.catalog;
        self.indexes = rebuilt.indexes;
        self.journal.restore_committed(rebuilt.journal);
    }
}
