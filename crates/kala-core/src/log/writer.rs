//! The append side of the log file: creating it, opening it (replaying
//! what is committed and cutting an incomplete tail to a quarantine file)
//! and appending durable transactions.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::encode::EncodedTxn;
use super::fault::ArmedFaults;
use super::file::{open_regular, read_log_file, read_to_len, sync_parent_dir};
use super::format::HEADER_LEN;
use super::scan::{Scan, scan};
use super::{LOG_FORMAT_VERSION, LOG_MAGIC, LogEntry};
use crate::error::Error;
use crate::world::{CutCause, RecoveryReport};

/// The append side of an open log file.
///
/// It holds an exclusive lock on the file for its whole lifetime (released
/// when the file handle is closed, i.e. when the writer is dropped), so only
/// one [`World`](crate::World) at a time appends to, or cuts, a log. The lock
/// is **advisory** (`flock`-style): it keeps out other Kala worlds, in this
/// process or another, but not programs that ignore it. [`read_log`](super::read_log) takes
/// no lock.
#[derive(Debug)]
pub(crate) struct LogWriter {
    /// The log file, locked exclusively.
    file: File,
    /// Where the log file is: doctor re-reads it from disk.
    path: PathBuf,
    /// The file's length: just past the last committed transaction. A
    /// transaction whose fsync failed is cut back to it.
    len: u64,
    /// Set by a failed append or fsync: the file may end in a torn
    /// transaction, so nothing more is appended until the world is reopened.
    failed: Option<Error>,
    /// Faults injected by tests (see [`ArmedFaults`]); none in normal use.
    pub(super) faults: ArmedFaults,
}

impl LogWriter {
    /// Creates a new log file at `path` (failing if anything exists there)
    /// holding the header and `genesis`, the encoded genesis transaction
    /// tx#0 (see [`encode_txn`](super::encode_txn)), and makes it durable: the file's data and
    /// the directory entry are fsynced.
    pub(crate) fn create(path: &Path, genesis: &EncodedTxn) -> Result<LogWriter, Error> {
        let mut buf = Vec::with_capacity(HEADER_LEN + genesis.bytes.len());
        buf.extend_from_slice(&LOG_MAGIC);
        buf.extend_from_slice(&LOG_FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&genesis.bytes);
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)?;
        lock(&file)?;
        file.write_all(&buf)?;
        file.sync_data()?;
        sync_parent_dir(path)?;
        Ok(LogWriter {
            file,
            path: path.to_owned(),
            // An in-memory buffer's length always fits a u64.
            len: buf.len() as u64,
            failed: None,
            faults: ArmedFaults::default(),
        })
    }

    /// Opens an existing log file at `path` for reading and appending (never
    /// creating it), passes the entries of its committed transactions (in
    /// log order) to `replay`, and returns `replay`'s result with a writer
    /// that appends after them.
    ///
    /// The file is locked exclusively before anything is read; if another
    /// writer holds the lock this fails with [`Error::WorldLocked`] and the
    /// file is untouched.
    ///
    /// Fails as [`read_log`](super::read_log) does, or with `replay`'s error. Only once both
    /// succeeded is a torn tail (bytes past the last committed transaction,
    /// see the module docs) cut off, so the next append lands right after
    /// the last commit. The cut bytes are first saved, durably, to a
    /// quarantine file (see [`quarantine`]); if that fails, the open fails
    /// with [`Error::Io`] and the log is not cut. On any error the log is
    /// untouched. A clean log is never modified, so opening is idempotent.
    /// The returned [`RecoveryReport`] says what was cut and where it went.
    ///
    /// With `cut_damaged` false, a *damaged* tail (see
    /// [`CutCause::Damaged`]) is not cut: the open fails with
    /// [`Error::RecoveryRequired`] under the lock, before anything is
    /// quarantined or cut, and the file is untouched. Torn tails are cut
    /// either way.
    pub(crate) fn open<T>(
        path: &Path,
        replay: impl FnOnce(&[LogEntry]) -> Result<T, Error>,
        cut_damaged: bool,
    ) -> Result<(T, LogWriter, RecoveryReport), Error> {
        // One handle for reading, cutting and appending, so the bytes
        // replayed are the bytes appended after. `append` without `create`
        // never creates the file.
        // Only a regular file: see `open_regular`.
        let mut file = open_regular(path, OpenOptions::new().read(true).append(true))?;
        lock(&file)?;
        // The length is taken under the lock, so no writer appends past it.
        let bytes = read_to_len(&mut file)?;
        let Scan {
            entries,
            valid_len,
            cause,
        } = scan(&bytes)?;
        if !cut_damaged && valid_len != bytes.len() && cause == Some(CutCause::Damaged) {
            return Err(Error::RecoveryRequired {
                bytes: (bytes.len() - valid_len) as u64,
            });
        }
        let replayed = replay(&entries)?;
        let mut report = RecoveryReport::default();
        if valid_len != bytes.len() {
            // Keep the cut bytes before cutting: if the tail was misjudged
            // torn, nothing committed is lost for good.
            let tail = &bytes[valid_len..];
            report.quarantine = Some(quarantine(path, &file, valid_len, tail)?);
            // `valid_len <= bytes.len()`, a `usize` that came from an
            // in-memory buffer, so it fits a `u64`. The size change is
            // metadata: `sync_all` makes it durable. Appends (O_APPEND) then
            // start at the new end.
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
            report.bytes_cut = tail.len() as u64;
            report.cause = cause;
        }
        Ok((
            replayed,
            LogWriter {
                file,
                path: path.to_owned(),
                len: valid_len as u64,
                failed: None,
                faults: ArmedFaults::default(),
            },
            report,
        ))
    }

    /// The read-only counterpart of [`LogWriter::open`]: reads the log file
    /// at `path` (never creating it) without taking the lock, passes the
    /// entries of its committed transactions to `replay`, and returns
    /// `replay`'s result with the [`RecoveryReport`] that `open` would
    /// produce for the same bytes, minus the quarantine: `bytes_cut` and
    /// `cause` say what `open` would cut, `quarantine` is `None`. Nothing is
    /// written, cut or created.
    ///
    /// Fails exactly as `open` does before it would modify the file (as
    /// [`read_log`](super::read_log) does, or with `replay`'s error); never with
    /// [`Error::WorldLocked`].
    pub(crate) fn read_committed<T>(
        path: &Path,
        replay: impl FnOnce(&[LogEntry]) -> Result<T, Error>,
    ) -> Result<(T, RecoveryReport), Error> {
        let bytes = read_log_file(path)?;
        let Scan {
            entries,
            valid_len,
            cause,
        } = scan(&bytes)?;
        let replayed = replay(&entries)?;
        let mut report = RecoveryReport::default();
        if valid_len != bytes.len() {
            // As in `open`: a `usize` length of an in-memory buffer fits a u64.
            report.bytes_cut = (bytes.len() - valid_len) as u64;
            report.cause = cause;
        }
        Ok((replayed, report))
    }

    /// The path the log file was created or opened at.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `Err` if an earlier append failed: the world refuses further writes.
    pub(crate) fn check(&self) -> Result<(), Error> {
        match &self.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Appends one encoded transaction (from [`encode_txn`](super::encode_txn), numbered to
    /// continue the log) with a single write, then fsyncs. Returns only
    /// once the transaction is durable. On failure the writer is marked
    /// failed and refuses further appends. If the write went through but the
    /// fsync failed, the file is first cut back (best effort) to its length
    /// before the transaction: the caller is told the transaction failed, so
    /// it must not reappear on reopen if the bytes reach the disk anyway. A
    /// failed write may leave a torn tail, which reopening cuts.
    pub(crate) fn append(&mut self, txn: &EncodedTxn) -> Result<(), Error> {
        self.check()?;
        let buf = &txn.bytes;
        let res = self
            .faults
            .before_write()
            .and_then(|()| self.file.write_all(buf));
        let res = res.and_then(|()| {
            let synced = self
                .faults
                .before_sync()
                .and_then(|()| self.file.sync_data());
            if synced.is_err() {
                // Best effort: if the cut fails too, the writer is failed
                // anyway and reopening judges whatever reached the disk.
                let _ = self.file.set_len(self.len);
                let _ = self.file.sync_data();
            }
            synced
        });
        if let Err(e) = res {
            let err = Error::from(e);
            self.failed = Some(match &err {
                Error::Io { kind, message } => Error::Io {
                    kind: *kind,
                    message: format!(
                        "an earlier log append failed ({message}); reopen the world to recover"
                    ),
                },
                other => other.clone(),
            });
            return Err(err);
        }
        // An in-memory buffer's length always fits a u64.
        self.len += buf.len() as u64;
        Ok(())
    }
}

/// Takes the exclusive advisory lock on a log file without waiting:
/// [`Error::WorldLocked`] if another handle holds it.
fn lock(file: &File) -> Result<(), Error> {
    file.try_lock().map_err(|e| match e {
        TryLockError::WouldBlock => Error::WorldLocked,
        TryLockError::Error(e) => Error::from(e),
    })
}

/// Durably saves `tail`, the bytes about to be cut off the log at `path`
/// after its first `valid_len` bytes, to a new file next to it:
/// `<path>.torn-<valid_len>`, or `<path>.torn-<valid_len>-1`, `-2`, ... if
/// that exists (an earlier quarantine is never overwritten). The file and
/// its directory entry are fsynced before this returns, so the log may be
/// cut afterwards. Returns the new file's path. On failure the new file is
/// removed (best effort).
fn quarantine(path: &Path, source: &File, valid_len: usize, tail: &[u8]) -> Result<PathBuf, Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // Recovery copies potentially private data. Apply the locked source's
    // permissions at creation, before any bytes reach the quarantine file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(source.metadata()?.permissions().mode() & 0o777);
    }
    #[cfg(not(unix))]
    let _ = source;
    let base = {
        let mut s = OsString::from(path.as_os_str());
        s.push(format!(".torn-{valid_len}"));
        s
    };
    let mut suffix = 0u64;
    let (qpath, mut qfile) = loop {
        let mut candidate = base.clone();
        if suffix > 0 {
            candidate.push(format!("-{suffix}"));
        }
        let candidate = PathBuf::from(candidate);
        match options.open(&candidate) {
            Ok(f) => break (candidate, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix = suffix.checked_add(1).ok_or_else(|| {
                    Error::from(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
                })?;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let res = qfile
        .write_all(tail)
        .and_then(|()| qfile.sync_all())
        .map_err(Error::from)
        .and_then(|()| sync_parent_dir(&qpath));
    if let Err(e) = res {
        drop(qfile);
        // Best effort: the log is left untouched either way.
        let _ = std::fs::remove_file(&qpath);
        return Err(e);
    }
    Ok(qpath)
}
