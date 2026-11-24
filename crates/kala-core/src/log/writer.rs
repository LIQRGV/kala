//! The append side of the log file: creating it, opening it, cutting an
//! incomplete tail to a quarantine file and appending durable
//! transactions; and the reads of the file that handles sharing it make
//! (a whole-file scan, the tail after a handle's last transaction).

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::encode::EncodedTxn;
use super::fault::ArmedFaults;
use super::file::{open_regular, read_to_len, sync_parent_dir};
use super::format::{BEGIN_LEN, HEADER_LEN};
use super::scan::{Scan, ScanFrom, check_header, is_same_txn, scan, scan_txns};
use super::{ChainTip, LOG_FORMAT_VERSION, LOG_MAGIC, LogEntry};
use crate::error::Error;
use crate::world::{CutCause, RecoveryReport};

/// How long a writer waits for the log file's lock unless told otherwise
/// (`World::set_busy_timeout`): 5 seconds, like SQLite's customary busy
/// timeout.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest pause between two attempts to take the lock.
const MAX_LOCK_POLL: Duration = Duration::from_millis(20);

/// The log file's exclusive lock, held: whoever has one may append to the
/// log or cut it, nobody else. Dropping it releases the lock, so an error
/// return or a panic under the lock can't keep it.
///
/// The lock is **advisory** (`flock`-style, on the log file itself): it
/// keeps out other Kala handles, in this process or another, and older
/// Kala binaries that hold it for a world's lifetime, but not programs
/// that ignore it. It belongs to the open file, so it also ends when the
/// process dies.
#[derive(Debug)]
pub(crate) struct WriterLock {
    file: Arc<File>,
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Released explicitly, not by closing: a child process forked
        // meanwhile shares the open file until it execs, and closing our
        // copy alone would leave the lock held through that window.
        let _ = self.file.unlock();
    }
}

/// Takes the exclusive advisory lock on a log file, waiting up to `timeout`
/// for another holder to release it: [`Error::WorldLocked`] if it is still
/// held then. There is no timed lock in the standard library, so this
/// polls: one attempt, then a sleep (1 ms, doubling up to
/// [`MAX_LOCK_POLL`], never past the deadline) between attempts, so a
/// waiter doesn't spin. A zero timeout is exactly one attempt.
fn lock(file: &Arc<File>, timeout: Duration) -> Result<WriterLock, Error> {
    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(WriterLock { file: file.clone() }),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(Error::from(e)),
        }
        let left = timeout.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(Error::WorldLocked);
        }
        std::thread::sleep(pause.min(left));
        pause = (pause * 2).min(MAX_LOCK_POLL);
    }
}

/// The log file's shared lock, held by a reader for the length of one read
/// of the file's tail: no writer is between its write and its fsync (or
/// its rollback) while it is held, so what the reader sees past the last
/// committed transaction is durable or a dead writer's. Dropping it
/// releases the lock.
struct ReadLock<'f>(&'f File);

impl Drop for ReadLock<'_> {
    fn drop(&mut self) {
        // Explicitly, as for `WriterLock`.
        let _ = self.0.unlock();
    }
}

/// Takes the shared advisory lock on a log file if no writer holds the
/// exclusive one: one attempt, never waiting. `None` if a writer does (a
/// commit, an open or a cut is in progress).
///
/// `file` must not be an open file whose exclusive lock the caller holds:
/// a lock belongs to the open file, so this would turn it into a shared
/// one.
fn try_read_lock(file: &File) -> Result<Option<ReadLock<'_>>, Error> {
    match file.try_lock_shared() {
        Ok(()) => Ok(Some(ReadLock(file))),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(Error::from(e)),
    }
}

/// Runs `f` while the file at `path` is locked as a log file is for a
/// commit (its exclusive advisory lock, waited for up to `busy_timeout`):
/// for replacing or removing a file that may be a world another process is
/// writing, when it can't be opened as a world (`World::with_writer_lock`
/// is for one that can). Returns `f`'s result, or `None`, `f` not run, if
/// there is nothing to lock: no file at `path`, or not a regular file.
///
/// Once the lock is held the path is checked to still name the locked
/// file; if it doesn't (it was replaced meanwhile), `f` isn't run and this
/// is [`Error::LogDiverged`]. [`Error::WorldLocked`] if the lock stays
/// held; [`Error::Io`] if the file can't be opened for reading. The lock
/// is released when `f` returns or panics.
pub fn with_file_lock<T>(
    path: &Path,
    busy_timeout: Duration,
    f: impl FnOnce() -> T,
) -> Result<Option<T>, Error> {
    let file = match open_regular(path, OpenOptions::new().read(true)) {
        Ok(file) => Arc::new(file),
        Err(Error::Io {
            kind: std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput,
            ..
        }) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let _held = lock(&file, busy_timeout)?;
    if !names_file(path, &file)? {
        return Err(Error::LogDiverged);
    }
    Ok(Some(f()))
}

/// Whether `path` names the open `file` (by device and inode, on Unix; one
/// `stat` and one `fstat`). Elsewhere always `true`.
fn names_file(path: &Path, file: &File) -> Result<bool, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let at_path = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let open = file.metadata()?;
        Ok((at_path.dev(), at_path.ino()) == (open.dev(), open.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, file);
        Ok(true)
    }
}

/// What a handle found past its [`ChainTip`] (see [`read_tail`]).
#[derive(Debug)]
pub(crate) struct Tail {
    /// The entries of the committed transactions appended after the tip,
    /// in log order.
    pub(crate) entries: Vec<LogEntry>,
    /// The last of those transactions; `None` if there are none.
    pub(crate) tip: Option<ChainTip>,
    /// The bytes after the last committed transaction: an incomplete
    /// (torn or damaged) last transaction. Empty for a clean end.
    pub(crate) rest: Vec<u8>,
    /// Why `rest` isn't a committed transaction; `None` if it is empty.
    pub(crate) cause: Option<CutCause>,
    /// How many bytes of the file were read to learn this.
    pub(crate) bytes_read: u64,
}

impl Tail {
    /// Nothing to apply (yet), nothing read.
    fn nothing() -> Tail {
        Tail {
            entries: Vec::new(),
            tip: None,
            rest: Vec::new(),
            cause: None,
            bytes_read: 0,
        }
    }

    /// How many committed transactions the tail holds.
    pub(crate) fn txns(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.body, super::EntryBody::Commit { .. }))
            .count()
    }
}

/// Reads what was appended to the log `file` after `tip`, the last
/// transaction a handle applied; `next_entry` is the entry ID the next
/// entry must have.
///
/// First the prefix is proved unchanged, at the cost of one transaction
/// (see "The checksum chain" in the module docs): the header must be this
/// format's and the bytes at `tip.offset..tip.end` must still be the
/// committed transaction `tip` describes. If the file is shorter than
/// `tip.end`, or they aren't, this is [`Error::LogDiverged`]. Then the
/// bytes from `tip.end` to the file's length (as it is now: bytes appended
/// meanwhile are left for the next read) are judged by the same recovery
/// rules as a whole-file scan, chained from `tip.checksum`. Anything those
/// rules call corrupt is [`Error::Corrupt`]; an incomplete last transaction
/// is returned in [`Tail::rest`] for the caller to judge.
///
/// `locked` says the caller holds the file's exclusive lock (on this very
/// open file). A caller that doesn't must never be shown a transaction a
/// writer has written but not yet fsynced: if that fsync fails the writer
/// cuts the transaction off again, and a reader that had applied it would
/// have shown data and entry IDs that never were committed. So when the
/// file is longer than the tip, the read happens under the file's
/// **shared** lock, taken without waiting: writers hold the exclusive lock
/// across write, fsync and any rollback, so no append is in flight while
/// it is held. If a writer holds its lock right now, this returns an empty
/// tail ("not yet": a later read finds what that writer committed). When
/// the file ends exactly at the tip (the common case) no lock is taken at
/// all: the read is bounded by that length, so it can't reach an append
/// that starts meanwhile, and readers cost a writer nothing.
///
/// Reads the header (12 bytes), the tip transaction and the tail, never
/// the rest of the log; the buffer is sized by the file's real length and
/// reserved fallibly. Modifies nothing.
fn read_tail(
    mut file: &File,
    tip: &ChainTip,
    next_entry: u64,
    locked: bool,
) -> Result<Tail, Error> {
    let mut len = file.metadata()?.len();
    let _shared = if !locked && len > tip.end {
        let Some(shared) = try_read_lock(file)? else {
            return Ok(Tail::nothing());
        };
        // Stable now: nothing appends or cuts while the lock is held.
        len = file.metadata()?.len();
        Some(shared)
    } else {
        None
    };
    if len < tip.end || tip.offset < HEADER_LEN as u64 || tip.end <= tip.offset {
        return Err(Error::LogDiverged);
    }
    let mut header = [0u8; HEADER_LEN];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    if check_header(&header).is_err() {
        return Err(Error::LogDiverged);
    }
    let too_big = || {
        Error::from(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!(
                "a log tail of {} bytes doesn't fit in memory",
                len - tip.offset
            ),
        ))
    };
    let want = usize::try_from(len - tip.offset).map_err(|_| too_big())?;
    let tip_len = usize::try_from(tip.end - tip.offset).map_err(|_| too_big())?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(want).map_err(|_| too_big())?;
    file.seek(SeekFrom::Start(tip.offset))?;
    file.take(len - tip.offset).read_to_end(&mut bytes)?;
    // An in-memory buffer's length always fits a u64.
    let bytes_read = (HEADER_LEN + bytes.len()) as u64;
    // Shorter than the tip: the file shrank between the length and the read.
    if bytes.len() < tip_len || !is_same_txn(&bytes[..tip_len], tip.tx.0, tip.checksum) {
        return Err(Error::LogDiverged);
    }
    let Scan {
        entries,
        valid_len,
        cause,
        tip: new_tip,
    } = scan_txns(
        &bytes[tip_len..],
        ScanFrom {
            offset: tip.end,
            next_entry,
            next_tx: tip.tx.0 + 1,
            link: tip.checksum,
        },
    )?;
    Ok(Tail {
        entries,
        tip: new_tip,
        rest: bytes.split_off(tip_len + valid_len),
        cause,
        bytes_read,
    })
}

/// [`read_tail`] for a handle with no open file (a read-only world): the
/// log file at `path` is opened for this one read, as a regular file only.
/// A path that no longer names a Kala log of this format is
/// [`Error::LogDiverged`]; a missing file is [`Error::Io`].
pub(crate) fn read_tail_at(path: &Path, tip: &ChainTip, next_entry: u64) -> Result<Tail, Error> {
    let file = open_regular(path, OpenOptions::new().read(true))?;
    read_tail(&file, tip, next_entry, false)
}

/// The length of the log file at `path` now (one `stat`).
pub(crate) fn file_len(path: &Path) -> Result<u64, Error> {
    Ok(std::fs::metadata(path)?.len())
}

/// A whole log file as one scan found it (see [`scan_file`]).
#[derive(Debug)]
pub(crate) struct Scanned {
    /// The entries of the committed transactions, in log order.
    pub(crate) entries: Vec<LogEntry>,
    /// The last committed transaction; `None` if there is none.
    pub(crate) tip: Option<ChainTip>,
    /// The bytes after it: an incomplete last transaction, or empty.
    pub(crate) rest: Vec<u8>,
    /// Why `rest` isn't a committed transaction; `None` if it is empty.
    pub(crate) cause: Option<CutCause>,
}

/// Reads the whole log `file`, up to the length it has now, and judges it
/// by the recovery rules: fails as [`read_log`](super::read_log) does.
/// Takes no lock and modifies nothing.
pub(crate) fn scan_file(mut file: &File) -> Result<Scanned, Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = read_to_len(file)?;
    let Scan {
        entries,
        valid_len,
        cause,
        tip,
    } = scan(&bytes)?;
    let rest = bytes.split_off(valid_len);
    Ok(Scanned {
        entries,
        tip,
        cause: cause.filter(|_| !rest.is_empty()),
        rest,
    })
}

/// How long a read-only open waits for the shared lock it settles its
/// first, lock-free read under (see `World::open_read_only`). A commit
/// holds the writer lock for a write and an fsync; an open that can't have
/// the shared lock within this goes with what it read.
pub(crate) const READ_SETTLE_WAIT: Duration = Duration::from_millis(100);

/// A log file open for reading only, for a world that never writes it.
#[derive(Debug)]
pub(crate) struct LogFile(File);

/// The shared lock of a [`LogFile`], held (see [`ReadLock`]).
pub(crate) struct SettleLock<'f>(#[allow(dead_code)] ReadLock<'f>);

impl LogFile {
    /// Opens the log file at `path` for reading, as a regular file only.
    pub(crate) fn open(path: &Path) -> Result<LogFile, Error> {
        Ok(LogFile(open_regular(path, OpenOptions::new().read(true))?))
    }

    /// [`scan_file`] of this file.
    pub(crate) fn scan(&self) -> Result<Scanned, Error> {
        scan_file(&self.0)
    }

    /// Takes the file's shared lock, trying for up to `wait`: `None` if a
    /// writer held its exclusive lock all that time.
    pub(crate) fn settle_lock(&self, wait: Duration) -> Result<Option<SettleLock<'_>>, Error> {
        let started = Instant::now();
        let mut pause = Duration::from_millis(1);
        loop {
            if let Some(lock) = try_read_lock(&self.0)? {
                return Ok(Some(SettleLock(lock)));
            }
            let left = wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(pause.min(left));
            pause = (pause * 2).min(MAX_LOCK_POLL);
        }
    }

    /// [`read_tail`] of this file after `tip`, under the shared lock
    /// `_held`: what is read is the file's state, no append is in flight.
    pub(crate) fn read_tail(
        &self,
        _held: &SettleLock<'_>,
        tip: &ChainTip,
        next_entry: u64,
    ) -> Result<Tail, Error> {
        read_tail(&self.0, tip, next_entry, true)
    }
}

/// The append side of an open log file.
///
/// It keeps the file open but holds its lock only while it works on it:
/// creating it, the end of an open (which may cut a torn tail) and each
/// commit take a [`WriterLock`] and release it when done, so several
/// handles, in this process or others, take turns appending to one log.
/// Everything that appends or cuts asks for the held lock as proof.
/// [`read_log`](super::read_log) and catch-ups take no lock.
#[derive(Debug)]
pub(crate) struct LogWriter {
    /// The log file, open for reading and appending.
    file: Arc<File>,
    /// Where the log file is: doctor re-reads it from disk.
    path: PathBuf,
    /// The file's length: just past the last committed transaction. A
    /// transaction whose fsync failed is cut back to it.
    len: u64,
    /// The last committed transaction in the file: where it is and its
    /// chained checksum. It moves only when an append has been fsynced, so
    /// a failed append (cut back to `len`) leaves it where it was.
    tip: Option<ChainTip>,
    /// Set by a failed append or fsync: the file may end in a torn
    /// transaction, so nothing more is appended until the world is reopened.
    failed: Option<Error>,
    /// How long [`LogWriter::lock`] waits for another holder.
    busy_timeout: Duration,
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
        let file = Arc::new(
            OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(path)?,
        );
        // Under the lock from its first byte: a writer that opens the new
        // file meanwhile waits here instead of judging a half-written
        // genesis. Released when this returns.
        let _lock = lock(&file, DEFAULT_BUSY_TIMEOUT)?;
        (&*file).write_all(&buf)?;
        file.sync_data()?;
        sync_parent_dir(path)?;
        // An in-memory buffer's length always fits a u64.
        let len = buf.len() as u64;
        Ok(LogWriter {
            file,
            path: path.to_owned(),
            len,
            tip: tip_of(genesis, HEADER_LEN as u64, len),
            failed: None,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            faults: ArmedFaults::default(),
        })
    }

    /// Opens the existing log file at `path` for reading and appending
    /// (never creating it; only a regular file, see `open_regular`), without
    /// reading it and without its lock. The writer knows nothing of the
    /// file's contents yet: [`LogWriter::scan`] reads them and
    /// [`LogWriter::stand_at`] puts the writer at their end. Commits wait up
    /// to `busy_timeout` for the lock.
    ///
    /// One handle for reading, cutting and appending, so the bytes replayed
    /// are the bytes appended after. `append` without `create` never
    /// creates the file.
    pub(crate) fn open_file(path: &Path, busy_timeout: Duration) -> Result<LogWriter, Error> {
        let file = Arc::new(open_regular(
            path,
            OpenOptions::new().read(true).append(true),
        )?);
        Ok(LogWriter {
            file,
            path: path.to_owned(),
            len: HEADER_LEN as u64,
            tip: None,
            failed: None,
            busy_timeout,
            faults: ArmedFaults::default(),
        })
    }

    /// Reads the whole file as it is now and judges it by the recovery
    /// rules (see [`scan_file`]). Without the writer lock the result is a
    /// first look that the caller settles under it; with it, it is the
    /// file's state.
    pub(crate) fn scan(&self) -> Result<Scanned, Error> {
        scan_file(&self.file)
    }

    /// Puts the writer at the end of the committed log a scan found: `tip`
    /// is its last transaction.
    pub(crate) fn stand_at(&mut self, tip: Option<ChainTip>) {
        self.tip = tip;
        self.len = tip.map_or(HEADER_LEN as u64, |tip| tip.end);
    }

    /// Cuts `tail`, the incomplete last transaction that follows the last
    /// committed one (everything past [`LogWriter::tip`]), off the file,
    /// under the lock: the bytes are first saved, durably, to a quarantine
    /// file (see [`quarantine`]), so if the tail was misjudged nothing
    /// committed is lost for good; if that fails this fails with
    /// [`Error::Io`] and the log is not cut. Returns what was cut, why
    /// (`cause`) and where it went.
    pub(crate) fn cut_tail(
        &mut self,
        _held: &WriterLock,
        tail: &[u8],
        cause: CutCause,
    ) -> Result<RecoveryReport, Error> {
        let quarantine = quarantine(&self.path, &self.file, self.len, tail)?;
        // The size change is metadata: `sync_all` makes it durable. Appends
        // (O_APPEND) then start at the new end.
        self.file.set_len(self.len)?;
        self.file.sync_all()?;
        Ok(RecoveryReport {
            // An in-memory buffer's length always fits a u64.
            bytes_cut: tail.len() as u64,
            quarantine: Some(quarantine),
            cause: Some(cause),
        })
    }

    /// Takes the log file's lock for one commit, waiting up to the busy
    /// timeout: [`Error::WorldLocked`] if another handle still holds it
    /// then.
    pub(crate) fn lock(&self) -> Result<WriterLock, Error> {
        lock(&self.file, self.busy_timeout)
    }

    /// How long [`LogWriter::lock`] waits.
    pub(crate) fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    /// Sets how long [`LogWriter::lock`] waits (zero: one attempt).
    pub(crate) fn set_busy_timeout(&mut self, timeout: Duration) {
        self.busy_timeout = timeout;
    }

    /// What was appended to the file after this writer's tip (see
    /// [`read_tail`]), read through the writer's own open file.
    ///
    /// The path is also checked to still name that open file (by device
    /// and inode, on Unix; one `stat`): a file renamed over it would take
    /// every later writer's appends while this one kept reading, locking
    /// and writing the old, unlinked one, so that is [`Error::LogDiverged`]
    /// too. `held` is the writer lock if the caller has it: then the read
    /// is authoritative (see `World::apply_tail` for what may be done about
    /// an incomplete tail). Without it the tail is read under the shared
    /// lock, or not at all while a writer holds its own (see
    /// [`read_tail`]).
    pub(crate) fn read_tail(
        &self,
        held: Option<&WriterLock>,
        next_entry: u64,
    ) -> Result<Tail, Error> {
        let tip = self.tip.ok_or(Error::LogDiverged)?;
        if !self.path_is_this_file()? {
            return Err(Error::LogDiverged);
        }
        // The lock passed in is this writer's own: it was taken on this
        // very open file, so the shared lock must not be (it would replace
        // the exclusive one).
        debug_assert!(held.is_none_or(|h| Arc::ptr_eq(&h.file, &self.file)));
        read_tail(&self.file, &tip, next_entry, held.is_some())
    }

    /// Whether the writer's path still names the file it has open.
    pub(crate) fn path_is_this_file(&self) -> Result<bool, Error> {
        names_file(&self.path, &self.file)
    }

    /// Test hook: fails as an armed "after lock" fault says (a panic, or
    /// the death of the process), right after a commit took the lock.
    pub(crate) fn after_lock(&mut self, _held: &WriterLock) {
        self.faults.after_lock();
    }

    /// The open file's length now (one `fstat`).
    pub(crate) fn file_len(&self) -> Result<u64, Error> {
        Ok(self.file.metadata()?.len())
    }

    /// Moves the writer past transactions another handle appended and this
    /// one has now applied: `tip` is the last of them.
    pub(crate) fn advance(&mut self, tip: ChainTip) {
        self.len = tip.end;
        self.tip = Some(tip);
    }

    /// The last committed transaction in the file (see [`ChainTip`]).
    pub(crate) fn tip(&self) -> Option<ChainTip> {
        self.tip
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
    /// continue the log) with a single write, then fsyncs, under the lock
    /// `_held` (taken for this commit, after which the caller made sure the
    /// file ends exactly at this writer's tip). Returns only
    /// once the transaction is durable. On failure the writer is marked
    /// failed and refuses further appends. If the write went through but the
    /// fsync failed, the file is first cut back (best effort) to its length
    /// before the transaction: the caller is told the transaction failed, so
    /// it must not reappear on reopen if the bytes reach the disk anyway. A
    /// failed write may leave a torn tail, which reopening cuts.
    ///
    /// Before anything is written the file and this writer must agree (see
    /// [`LogWriter::continues_file`]); if they don't, nothing is written or
    /// cut, the writer is marked failed and this is [`Error::LogDiverged`].
    pub(crate) fn append(&mut self, _held: &WriterLock, txn: &EncodedTxn) -> Result<(), Error> {
        self.check()?;
        #[cfg(feature = "test-hooks")]
        super::fault::fire(super::fault::LogHook::BeforeAppend);
        let buf = &txn.bytes;
        if !self.continues_file(txn)? {
            return Err(self.refuse(Error::LogDiverged));
        }
        let res = self
            .faults
            .before_write()
            .and_then(|()| (&*self.file).write_all(buf));
        let res = res.and_then(|()| {
            #[cfg(feature = "test-hooks")]
            super::fault::fire(super::fault::LogHook::BeforeSync);
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
        let end = self.len + buf.len() as u64;
        self.tip = tip_of(txn, self.len, end);
        self.len = end;
        Ok(())
    }
}

impl LogWriter {
    /// Whether appending `txn` now continues the log this writer knows,
    /// checked under the lock right before the write:
    ///
    /// - the file is exactly as long as the log the writer has applied
    ///   (one `fstat`). The write is an append and a failed fsync is rolled
    ///   back with a truncation to that length, so a file that is longer
    ///   (bytes this writer hasn't seen) or shorter would end up holding a
    ///   transaction that doesn't follow what is before it, or lose another
    ///   writer's committed one to the rollback;
    /// - `txn` is the transaction after the writer's tip: its tx is the
    ///   tip's plus one and its `Commit` checksum chains from the tip's.
    ///
    /// The lock makes all of this true by construction. It is checked
    /// anyway, because the lock is advisory and not every file system
    /// honours it (some network mounts, a Windows drive seen from WSL), a
    /// program that ignores it may write the file, and a caller that caught
    /// a panic between an append and its bookkeeping would otherwise go on
    /// with a stale length.
    fn continues_file(&self, txn: &EncodedTxn) -> Result<bool, Error> {
        let Some(tip) = self.tip else {
            return Ok(false);
        };
        let Some(super::EntryBody::Commit { tx, checksum }) = txn.entries.last().map(|e| &e.body)
        else {
            return Ok(false);
        };
        let Some(covered) = txn
            .bytes
            .len()
            .checked_sub(4)
            .and_then(|end| txn.bytes.get(BEGIN_LEN..end))
        else {
            return Ok(false);
        };
        Ok(self.len == tip.end
            && self.file.metadata()?.len() == self.len
            && tip.tx.0.checked_add(1) == Some(tx.0)
            && super::chained_checksum(tip.checksum, covered) == *checksum)
    }

    /// Marks the writer failed with `why` (nothing more is appended until
    /// the world is reopened) and returns `why`.
    pub(crate) fn refuse(&mut self, why: Error) -> Error {
        self.failed.get_or_insert(why.clone());
        why
    }
}

/// The chain tip after `txn` was written at `offset..end` of the file.
fn tip_of(txn: &EncodedTxn, offset: u64, end: u64) -> Option<ChainTip> {
    let last = txn.entries.last()?;
    let super::EntryBody::Commit { tx, checksum } = last.body else {
        return None;
    };
    Some(ChainTip {
        tx,
        offset,
        end,
        checksum,
    })
}

/// Durably saves `tail`, the bytes about to be cut off the log at `path`
/// (open as `source`) after its first `valid_len` bytes, to a new file next to it:
/// `<path>.torn-<valid_len>`, or `<path>.torn-<valid_len>-1`, `-2`, ... if
/// that exists (an earlier quarantine is never overwritten). The file and
/// its directory entry are fsynced before this returns, so the log may be
/// cut afterwards. Returns the new file's path. On failure the new file is
/// removed (best effort).
fn quarantine(path: &Path, source: &File, valid_len: u64, tail: &[u8]) -> Result<PathBuf, Error> {
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
