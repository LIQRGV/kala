//! Test hooks: failures injected into the next log append. Everything the
//! `test-hooks` feature adds to the log lives in this file; the writer only
//! asks [`ArmedFaults`] before each write and fsync, and with the feature
//! off nothing can arm one.

#[cfg(feature = "test-hooks")]
use super::writer::LogWriter;

/// A failure to inject into the next log append, for tests of the
/// writer's failure handling (see `World::inject_log_fault`). Not part of
/// the supported API.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFault {
    /// The next append's fsync fails after its bytes were written.
    FailNextSync,
    /// The next append's write fails before any byte is written.
    FailNextWrite,
}

/// Faults injected by tests, consumed by the next append that reaches
/// them. Both `false` in normal use.
#[derive(Debug, Default)]
pub(super) struct ArmedFaults {
    fail_next_write: bool,
    fail_next_sync: bool,
}

impl ArmedFaults {
    /// Called before an append's write: `Err` (once) if a write fault is
    /// armed.
    pub(super) fn before_write(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.fail_next_write) {
            return Err(injected("write"));
        }
        Ok(())
    }

    /// Called before an append's fsync: `Err` (once) if a sync fault is
    /// armed.
    pub(super) fn before_sync(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(injected("fsync"));
        }
        Ok(())
    }
}

/// The error an injected `LogFault` produces.
fn injected(what: &str) -> std::io::Error {
    std::io::Error::other(format!("injected log fault: {what} failed"))
}

#[cfg(feature = "test-hooks")]
impl LogWriter {
    /// Arms a fault for the next append (see [`LogFault`]).
    pub(crate) fn inject_fault(&mut self, fault: LogFault) {
        match fault {
            LogFault::FailNextWrite => self.faults.fail_next_write = true,
            LogFault::FailNextSync => self.faults.fail_next_sync = true,
        }
    }
}
