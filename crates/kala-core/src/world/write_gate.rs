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
            read_only: false,
        }
    }

    /// The gate of a world opened read-only, with what a writable open
    /// would cut.
    pub(super) fn read_only(recovery: RecoveryReport) -> WriteGate {
        WriteGate {
            recovery,
            recovery_unacknowledged: false,
            read_only: true,
        }
    }

    /// `Err(ReadOnly)` for a world from [`World::open_read_only`], else
    /// `Err(RecoveryUnacknowledged)` until a damaged-tail cut is
    /// acknowledged (see [`World::acknowledge_recovery`]).
    pub(super) fn accepts_writes(&self) -> Result<(), Error> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if self.recovery_unacknowledged {
            return Err(Error::RecoveryUnacknowledged);
        }
        Ok(())
    }

    /// Allows writes again after a damaged-tail cut (see
    /// [`World::acknowledge_recovery`]). A read-only world stays read-only.
    pub(super) fn acknowledge_recovery(&mut self) {
        self.recovery_unacknowledged = false;
    }

    /// What opening the log did to recover it.
    pub(super) fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }
}
