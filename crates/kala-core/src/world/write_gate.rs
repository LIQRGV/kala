//! The write gate: whether a world accepts writes at all, and what opening
//! its log did to recover it.

use super::*;

/// Why a world refuses writes regardless of what is written: it was opened
/// read-only, or its open cut a damaged tail nobody acknowledged yet. Also
/// keeps the recovery report the open produced. A failed log append is the
/// log writer's own state (see `Journal::check_log`).
#[derive(Debug, Default)]
pub(super) struct WriteGate {
    /// What opening the log cut off (default unless [`World::open`] cut a
    /// torn tail).
    recovery: RecoveryReport,
    /// Writes are refused with [`Error::RecoveryUnacknowledged`]: the open
    /// cut a damaged tail and [`World::acknowledge_recovery`] hasn't been
    /// called yet. Never set for a torn cut, a clean open or a new world.
    recovery_unacknowledged: bool,
    /// Opened by [`World::open_read_only`]: every write is refused with
    /// [`Error::ReadOnly`]. Such a world has no log writer.
    read_only: bool,
    /// Why the handle can't follow its log file any more
    /// ([`Error::LogDiverged`], or the [`Error::Corrupt`] of a tail that
    /// didn't replay): every later catch-up and write fails with it.
    broken: Option<Error>,
    /// A commit found a damaged tail under the lock and refused to cut it
    /// ([`Error::RecoveryRequired`]); nobody acknowledged it yet.
    damaged_tail_seen: bool,
    /// [`World::acknowledge_recovery`] was called after such a commit: the
    /// next commit may cut the damaged tail.
    may_cut_damaged: bool,
}

impl WriteGate {
    /// The gate of a new world: writes accepted, nothing recovered.
    pub(super) fn open() -> WriteGate {
        WriteGate::default()
    }

    /// The gate of a world opened for writing whose open reported
    /// `recovery`: a damaged-tail cut must be acknowledged before writes.
    pub(super) fn after_recovery(recovery: RecoveryReport) -> WriteGate {
        WriteGate {
            recovery_unacknowledged: recovery.cause == Some(CutCause::Damaged),
            recovery,
            ..WriteGate::default()
        }
    }

    /// The gate of a world opened read-only, with what a writable open
    /// would cut.
    pub(super) fn read_only(recovery: RecoveryReport) -> WriteGate {
        WriteGate {
            recovery,
            read_only: true,
            ..WriteGate::default()
        }
    }

    /// `Err(ReadOnly)` for a world from [`World::open_read_only`], else
    /// `Err(RecoveryUnacknowledged)` until a damaged-tail cut is
    /// acknowledged (see [`World::acknowledge_recovery`]).
    pub(super) fn accepts_writes(&self) -> Result<(), Error> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        self.follows_log()?;
        if self.recovery_unacknowledged {
            return Err(Error::RecoveryUnacknowledged);
        }
        Ok(())
    }

    /// Allows writes again after a damaged-tail cut (see
    /// [`World::acknowledge_recovery`]). A read-only world stays read-only.
    pub(super) fn acknowledge_recovery(&mut self) {
        self.recovery_unacknowledged = false;
        // Only a damaged tail a commit has already reported can be
        // acknowledged: acknowledging in advance licenses nothing.
        self.may_cut_damaged = self.damaged_tail_seen;
    }

    /// `Err` once the handle has lost its log file (see
    /// [`WriteGate::stop_following`]).
    pub(super) fn follows_log(&self) -> Result<(), Error> {
        match &self.broken {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Records that the handle can't follow its log file any more, and why:
    /// every later catch-up and write fails with `why`.
    pub(super) fn stop_following(&mut self, why: Error) {
        self.broken.get_or_insert(why);
    }

    /// A commit holding the lock found a damaged tail: whether it may cut
    /// it (a commit refused it before and the refusal was acknowledged).
    /// If not, the refusal is remembered, for an acknowledgement to
    /// license the next commit.
    pub(super) fn may_cut_damaged_tail(&mut self) -> bool {
        self.damaged_tail_seen = true;
        self.may_cut_damaged
    }

    /// A commit holding the lock found no damaged tail (or cut it): a
    /// licence to cut one is spent.
    pub(super) fn no_damaged_tail(&mut self) {
        self.damaged_tail_seen = false;
        self.may_cut_damaged = false;
    }

    /// Records a cut a commit made under the lock: what
    /// [`World::recovery`] reports from now on.
    pub(super) fn set_recovery(&mut self, recovery: RecoveryReport) {
        self.recovery = recovery;
    }

    /// What opening the log did to recover it.
    pub(super) fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }
}
