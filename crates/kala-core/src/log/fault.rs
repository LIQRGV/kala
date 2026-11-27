//! Test hooks: failures injected into the next log append, and code a test
//! runs at a chosen point of an open or a commit. Everything the
//! `test-hooks` feature adds to the log lives in this file; the writer only
//! asks [`ArmedFaults`] before each write and fsync and calls `fire` at the
//! [`LogHook`] points, and with the feature off nothing can arm either
//! (`fire` isn't even compiled).

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
    /// The next commit panics right after taking the writer lock, before
    /// anything is appended.
    PanicAfterLock,
    /// The next commit aborts the whole process right after taking the
    /// writer lock, before anything is appended: a writer that dies.
    AbortAfterLock,
}

/// Faults injected by tests, consumed by the next append that reaches
/// them. All `false` in normal use.
#[derive(Debug, Default)]
pub(super) struct ArmedFaults {
    fail_next_write: bool,
    fail_next_sync: bool,
    /// The next commit panics once it holds the lock.
    panic_after_lock: bool,
    /// The next commit aborts the process once it holds the lock.
    abort_after_lock: bool,
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

    /// Called once a commit holds the writer lock, before it reads or
    /// appends anything: panics (once) or aborts the process if such a
    /// fault is armed.
    pub(super) fn after_lock(&mut self) {
        if std::mem::take(&mut self.abort_after_lock) {
            std::process::abort();
        }
        if std::mem::take(&mut self.panic_after_lock) {
            panic!("injected log fault: panic under the writer lock");
        }
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
            LogFault::PanicAfterLock => self.faults.panic_after_lock = true,
            LogFault::AbortAfterLock => self.faults.abort_after_lock = true,
        }
    }
}

/// A place in the log code where a test can run something of its own (see
/// [`on_log_hook`]): what another handle or process could do exactly then.
/// Not part of the supported API.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogHook {
    /// A writable open has replayed the log and is about to take the
    /// writer lock.
    OpenReplayed,
    /// A commit holds the writer lock and has caught up; its transaction
    /// is encoded and about to be written.
    BeforeAppend,
    /// A commit has written its transaction and is about to fsync it (or
    /// to fail an injected fsync), still under the writer lock.
    BeforeSync,
}

/// A hook armed for a point: what to run there, once.
#[cfg(feature = "test-hooks")]
type ArmedHook = (LogHook, Box<dyn FnOnce()>);

#[cfg(feature = "test-hooks")]
thread_local! {
    /// The hooks armed on this thread, each run once.
    static HOOKS: std::cell::RefCell<Vec<ArmedHook>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` the next time log code on **this thread** reaches `point`,
/// once. The hook is taken out before it runs, so what it does (a commit
/// on another handle, say) doesn't fire it again. Not part of the
/// supported API.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn on_log_hook(point: LogHook, f: impl FnOnce() + 'static) {
    HOOKS.with(|hooks| hooks.borrow_mut().push((point, Box::new(f))));
}

/// Runs the hook a test armed for `point` on this thread, if any.
#[cfg(feature = "test-hooks")]
pub(crate) fn fire(point: LogHook) {
    let hook = HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        let at = hooks.iter().position(|(p, _)| *p == point)?;
        Some(hooks.remove(at).1)
    });
    if let Some(hook) = hook {
        hook();
    }
}
