//! Immutable database names and lazily cached, committed read-only snapshots.
use crate::error::{DATABASE, Failure, escape};
use kala_core::World;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
struct Database {
    path: PathBuf,
    cached: Mutex<Option<(Stamp, Arc<World>)>>,
    /// Held while a snapshot is reloaded, so concurrent RUNs that need the
    /// new snapshot share one replay instead of each replaying the world
    /// (peak memory stays at about two snapshots, not one per connection).
    loading: Mutex<()>,
    writer: Mutex<Option<World>>,
    /// How often the reader snapshot was replayed from the file, and how
    /// often a writable handle was. Each is a replay of the whole log, so
    /// tests pin how many a sequence of requests may cost.
    reader_opens: AtomicUsize,
    writer_opens: AtomicUsize,
}

/// What a cached snapshot was loaded from. The length alone is not enough:
/// recovery can cut a log and a later commit can grow it back to the same
/// length with different content. The last bytes of the file hold the last
/// transaction's commit checksum, so they change with any different last
/// transaction; the modification time covers the rest.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    /// `(dev, ino)` of the file the tail was read from (Unix).
    file: Option<(u64, u64)>,
    tail: Vec<u8>,
}

/// Bytes read from the end of the file for [`Stamp::tail`].
const TAIL_BYTES: u64 = 64;

fn stamp(path: &Path) -> Result<Stamp, Failure> {
    use std::io::{Read, Seek, SeekFrom};
    let metadata = std::fs::symlink_metadata(path).map_err(Failure::store)?;
    if !metadata.file_type().is_file() {
        return Err(Failure::store("world is not a regular file"));
    }
    let mut file = std::fs::File::open(path).map_err(Failure::store)?;
    // Describe the file actually opened (fstat), not the path checked
    // before (lstat): length, mtime and tail all come from one inode, and a
    // swap between the two calls is refused rather than half-described.
    let opened = file.metadata().map_err(Failure::store)?;
    if !opened.file_type().is_file() {
        return Err(Failure::store("world is not a regular file"));
    }
    #[cfg(unix)]
    let file_id = {
        use std::os::unix::fs::MetadataExt;
        if (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
            return Err(Failure::store("world file changed while being checked"));
        }
        Some((opened.dev(), opened.ino()))
    };
    #[cfg(not(unix))]
    let file_id = None;
    let len = opened.len();
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .map_err(Failure::store)?;
    file.take(TAIL_BYTES)
        .read_to_end(&mut tail)
        .map_err(Failure::store)?;
    Ok(Stamp {
        len,
        modified: opened.modified().ok(),
        file: file_id,
        tail,
    })
}
fn bookmark_unavailable() -> Failure {
    Failure::new(
        crate::write::CONFLICT,
        "bookmark is not available within the busy timeout",
    )
}
/// Whether a registry serves the reserved `system` database.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemDb {
    /// Register `system.kala` (account authentication reads it; admins query it).
    Serve,
    /// Never register, open or read `system.kala` (`--auth none`): the file is
    /// skipped by name, whatever it contains or whoever may read it.
    Skip,
}
pub(crate) struct Registry {
    databases: BTreeMap<String, Database>,
    default: Option<String>,
    write_transactions: Arc<Mutex<WriteSlots>>,
    /// `kala serve --acknowledge-recovery`: see [`Registry::recover`].
    acknowledge_recovery: bool,
}
/// How many explicit transactions hold a private writable handle: in total
/// and per account.
#[derive(Default)]
struct WriteSlots {
    total: usize,
    by_account: BTreeMap<String, usize>,
}
/// One open writing transaction's place under the cap; freed on drop.
pub(crate) struct WriteSlot {
    slots: Arc<Mutex<WriteSlots>>,
    account: String,
}
impl Drop for WriteSlot {
    fn drop(&mut self) {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.total = slots.total.saturating_sub(1);
        if let Some(held) = slots.by_account.get_mut(&self.account) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                slots.by_account.remove(&self.account);
            }
        }
    }
}
/// The most write slots one account may hold when the server allows `max`
/// in total: half of them (at least one), so that a single account can
/// never keep every other account from writing in explicit transactions.
pub(crate) fn account_write_slots(max: usize) -> usize {
    (max / 2).max(1)
}
impl Registry {
    /// `(reader, writer)`: how often each kind of handle of `name` was
    /// replayed from its file.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn opens(&self, name: &str) -> (usize, usize) {
        let db = &self.databases[name];
        (
            db.reader_opens.load(Ordering::Relaxed),
            db.writer_opens.load(Ordering::Relaxed),
        )
    }
    /// Whether this registry's writable handles cut a damaged tail by
    /// themselves (`kala serve --acknowledge-recovery`); off unless set.
    pub fn acknowledging_recovery(mut self, on: bool) -> Self {
        self.acknowledge_recovery = on;
        self
    }
    /// With `--acknowledge-recovery`: if `world` (a writable handle of
    /// `name`) has met a damaged last transaction, acknowledges it and cuts
    /// it to a quarantine file under the writer lock, as an operator's
    /// `kala transact --acknowledge-recovery` would, and logs one line per
    /// cut. Without the flag this does nothing: the handle keeps refusing
    /// writes and the bytes stay in the file.
    ///
    /// Called when a handle is opened (the open reports a damaged tail) and
    /// before each use of the cached handle (a tail that appeared since:
    /// the file is then longer than the log the handle has applied; that
    /// costs one `stat`, and only then is the lock taken to look). A torn
    /// tail needs none of this: a commit cuts it by itself. `system` never
    /// gets here: no writable handle of it is ever opened.
    fn recover(&self, name: &str, world: &mut World) -> Result<(), Failure> {
        use kala_core::{CutCause, Error};
        if !self.acknowledge_recovery {
            return Ok(());
        }
        let reported = {
            let report = world.recovery();
            report.cause == Some(CutCause::Damaged) && report.quarantine.is_none()
        };
        let unapplied = world.chain_tip().is_some_and(|tip| {
            std::fs::metadata(&self.databases[name].path).map_or(true, |m| m.len() > tip.end)
        });
        if !reported && !unapplied {
            return Ok(());
        }
        let mut acknowledged = false;
        let report = loop {
            match world.cut_acknowledged_tail() {
                Ok(report) => break report.clone(),
                Err(Error::RecoveryRequired { .. } | Error::RecoveryUnacknowledged)
                    if !acknowledged =>
                {
                    world.acknowledge_recovery();
                    acknowledged = true;
                }
                Err(error) => return Err(crate::write::store_error(error, true)),
            }
        };
        if acknowledged
            && report.cause == Some(CutCause::Damaged)
            && let Some(quarantine) = &report.quarantine
        {
            eprintln!(
                "event=recovery_acknowledged db={name} bytes={} quarantine={}",
                report.bytes_cut,
                escape(&quarantine.file_name().unwrap_or_default().to_string_lossy()),
            );
        }
        Ok(())
    }
    /// Runs `f` on the database's cached writable handle, caught up with
    /// the file and holding at least tx `at_least` (a bookmark; 0 for none).
    ///
    /// The handle is borrowed only while one statement stages and commits,
    /// never across client I/O. It is opened on first use and kept: an
    /// ordinary failed write (a store rule, a conflict, a syntax error in a
    /// later form) leaves it valid, and only an error that breaks the handle
    /// (`write::leaves_handle_unusable`, or a failed catch-up) drops it, so
    /// that a client repeating a bad request can't make every writer replay
    /// the log under this mutex.
    ///
    /// A bookmark the file hasn't reached yet is waited for, up to `busy`,
    /// with the mutex released between looks. The reader snapshot is not
    /// touched: a write neither needs nor refreshes it.
    pub fn write<R>(
        &self,
        name: &str,
        busy: Duration,
        at_least: u64,
        f: impl FnOnce(&mut World) -> Result<R, Failure>,
    ) -> Result<R, Failure> {
        self.require(name)?;
        let db = &self.databases[name];
        let until = Instant::now() + busy;
        let mut cached = loop {
            let mut cached = Self::lock_writer(db, until)?;
            if cached.is_none() {
                *cached = Some(self.open_writer(name, busy)?);
            }
            let world = cached.as_mut().ok_or_else(Failure::invalid)?;
            world.set_busy_timeout(busy);
            if let Err(error) = world.catch_up() {
                *cached = None;
                return Err(crate::write::store_error(error, true));
            }
            if let Err(failure) = self.recover(name, world) {
                *cached = None;
                return Err(failure);
            }
            if world.latest_tx().0 >= at_least {
                break cached;
            }
            drop(cached);
            if Instant::now() >= until {
                return Err(bookmark_unavailable());
            }
            std::thread::sleep(
                Duration::from_millis(5).min(until.saturating_duration_since(Instant::now())),
            );
        };
        let result = f(cached.as_mut().ok_or_else(Failure::invalid)?);
        if result.as_ref().is_err_and(|failure| failure.handle_lost) {
            *cached = None;
        }
        result
    }
    /// The database's writer slot, waiting for another statement until
    /// `until`. A poisoned mutex (a session thread panicked while it held
    /// the writer) is recovered by discarding the handle that thread was
    /// using: the next statement rebuilds one from the log, which is the
    /// source of truth, instead of the database refusing writes until the
    /// server restarts.
    fn lock_writer(
        db: &Database,
        until: Instant,
    ) -> Result<std::sync::MutexGuard<'_, Option<World>>, Failure> {
        loop {
            match db.writer.try_lock() {
                Ok(lock) => return Ok(lock),
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    let mut lock = poisoned.into_inner();
                    *lock = None;
                    db.writer.clear_poison();
                    return Ok(lock);
                }
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err(crate::write::store_error(
                        kala_core::Error::WorldLocked,
                        true,
                    ));
                }
            }
        }
    }
    pub fn open_writer(&self, name: &str, busy: Duration) -> Result<World, Failure> {
        self.require(name)?;
        if name == "system" {
            return Err(Failure::new(
                crate::error::FORBIDDEN,
                "the system database is written only with kala user",
            ));
        }
        let path = &self.databases[name].path;
        stamp(path)?;
        self.databases[name]
            .writer_opens
            .fetch_add(1, Ordering::Relaxed);
        let mut world =
            World::open_waiting(path, busy).map_err(|e| crate::write::store_error(e, true))?;
        if crate::system::is_system(&world) {
            return Err(Failure::new(
                crate::error::FORBIDDEN,
                "the system database is written only with kala user",
            ));
        }
        self.recover(name, &mut world)?;
        Ok(world)
    }
    /// What the `system` database's file looks like now, or `None` when
    /// this server doesn't serve one (`--auth none`). A connection compares
    /// it with the stamp taken when its grants were read: equal means no
    /// account changed since.
    pub fn accounts_stamp(&self) -> Option<Result<Stamp, Failure>> {
        self.databases.get("system").map(|db| stamp(&db.path))
    }
    /// Takes a slot for an explicit transaction that starts writing, for
    /// `account` (the authenticated `user@host`). Refused, with a code
    /// drivers retry, when `max` are held in total or the account already
    /// holds its share ([`account_write_slots`]).
    pub fn write_slot(&self, max: usize, account: &str) -> Result<WriteSlot, Failure> {
        let mut slots = self
            .write_transactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let held = slots.by_account.get(account).copied().unwrap_or(0);
        if slots.total >= max {
            return Err(Failure::new(
                crate::write::CONFLICT,
                "too many open write transactions",
            ));
        }
        if held >= account_write_slots(max) {
            return Err(Failure::new(
                crate::write::CONFLICT,
                "too many open write transactions for this account",
            ));
        }
        slots.total += 1;
        slots.by_account.insert(account.to_owned(), held + 1);
        Ok(WriteSlot {
            slots: Arc::clone(&self.write_transactions),
            account: account.to_owned(),
        })
    }
    /// Causal reads share the normal stamped snapshot refresh, with a bounded wait.
    pub fn at_least(&self, name: &str, tx: u64, busy: Duration) -> Result<Arc<World>, Failure> {
        let deadline = Instant::now() + busy;
        loop {
            let (_, world) = self.get(Some(name))?;
            if world.latest_tx().0 >= tx {
                return Ok(world);
            }
            if Instant::now() >= deadline {
                return Err(bookmark_unavailable());
            }
            std::thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    pub fn open(directory: &Path, default: Option<String>, system: SystemDb) -> io::Result<Self> {
        let mut databases = BTreeMap::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let text = name.to_string_lossy();
            let db = text.strip_suffix(".kala");
            if system == SystemDb::Skip && db == Some("system") {
                // Decided by name only, before any metadata or content is read.
                eprintln!(
                    "kala serve: skipping system.kala: the system database is never served without account authentication"
                );
            } else if entry.file_type()?.is_file()
                && db.is_some_and(kala_system::valid_database_name)
            {
                databases.insert(
                    db.unwrap_or_default().to_string(),
                    Database {
                        path: entry.path(),
                        cached: Mutex::new(None),
                        loading: Mutex::new(()),
                        writer: Mutex::new(None),
                        reader_opens: AtomicUsize::new(0),
                        writer_opens: AtomicUsize::new(0),
                    },
                );
            } else {
                eprintln!("event=bolt_database_skipped name={:?}", escape(&text));
            }
        }
        if default.as_ref().is_some_and(|d| !databases.contains_key(d)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "default database is not a registered world",
            ));
        }
        // The reserved system world is never an implicit user database.
        let user_names: Vec<_> = databases.keys().filter(|name| *name != "system").collect();
        let default = default.or_else(|| (user_names.len() == 1).then(|| user_names[0].clone()));
        Ok(Self {
            databases,
            default,
            write_transactions: Arc::default(),
            acknowledge_recovery: false,
        })
    }
    /// Resolve the name before loading a world, so authorization precedes I/O.
    pub fn name<'a>(&'a self, requested: Option<&'a str>) -> Result<&'a str, Failure> {
        requested
            .or(self.default.as_deref())
            .ok_or_else(|| Failure::new(DATABASE, "no default database; specify a database name"))
    }
    /// Registry names only: does not open any world.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.databases.keys().map(String::as_str)
    }
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }
    pub fn require(&self, name: &str) -> Result<(), Failure> {
        if self.databases.contains_key(name) {
            Ok(())
        } else {
            Err(Failure::new(
                DATABASE,
                format!("database {name:?} was not found"),
            ))
        }
    }
    /// The database's reader snapshot slot. A poisoned mutex (a session
    /// thread panicked while it held the slot or a reload) is recovered as
    /// [`Registry::lock_writer`] recovers the writer: the cached snapshot is
    /// discarded and the next read reloads it from the log, instead of the
    /// database refusing reads until the server restarts.
    fn lock_cached(db: &Database) -> std::sync::MutexGuard<'_, Option<(Stamp, Arc<World>)>> {
        db.cached.lock().unwrap_or_else(|poisoned| {
            let mut lock = poisoned.into_inner();
            *lock = None;
            db.cached.clear_poison();
            lock
        })
    }
    pub fn get(&self, requested: Option<&str>) -> Result<(String, Arc<World>), Failure> {
        let name = self.name(requested)?;
        let db = self
            .databases
            .get(name)
            .ok_or_else(|| Failure::new(DATABASE, format!("database {name:?} was not found")))?;
        let lock = || Self::lock_cached(db);
        // Take the stamp BEFORE loading: a commit after this observation
        // changes the stamp again and forces another refresh, so a snapshot
        // is never labelled with a newer stamp than its own.
        let stamp = stamp(&db.path)?;
        if let Some((s, world)) = lock().as_ref()
            && *s == stamp
        {
            return Ok((name.into(), Arc::clone(world)));
        }
        // Single-flight: one thread replays, the others wait for it and then
        // find the fresh snapshot. Queries whose stamp matches (above) never
        // wait behind a reload.
        let _loading = db.loading.lock().unwrap_or_else(|poisoned| {
            // A reload panicked: whatever it left is not trusted either.
            let lock = poisoned.into_inner();
            *Self::lock_cached(db) = None;
            db.loading.clear_poison();
            lock
        });
        let stamp = self::stamp(&db.path)?;
        if let Some((s, world)) = lock().as_ref()
            && *s == stamp
        {
            return Ok((name.into(), Arc::clone(world)));
        }
        db.reader_opens.fetch_add(1, Ordering::Relaxed);
        let world = Arc::new(World::open_read_only(&db.path).map_err(Failure::store)?);
        // Content, not a filename, identifies privileged worlds. A renamed
        // system file must never become readable using an ordinary read grant.
        if crate::system::is_system(&world) != (name == "system") {
            return Err(Failure::store("reserved system database marker mismatch"));
        }
        eprintln!(
            "event=bolt_database_loaded db={name} canon={}",
            world.latest_tx().0
        );
        *lock() = Some((stamp, Arc::clone(&world)));
        Ok((name.into(), world))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let fixture = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
        drop(kala_fixture::load_fixture_to(fixture, &dir.path().join("varrow.kala")).unwrap());
        let registry = Registry::open(dir.path(), None, SystemDb::Skip).unwrap();
        (dir, registry)
    }

    #[test]
    fn a_poisoned_writer_mutex_is_recovered_by_reopening_the_writer() {
        let (_dir, registry) = registry();
        let busy = Duration::from_secs(1);
        registry.write("varrow", busy, 0, |_| Ok(())).unwrap();
        let panicked = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    registry.write("varrow", busy, 0, |_| -> Result<(), Failure> {
                        panic!("a session thread panicked while holding the writer")
                    })
                })
                .join()
        });
        assert!(panicked.is_err());
        assert!(registry.databases["varrow"].writer.is_poisoned());
        // The handle the panic interrupted is discarded, not trusted; the
        // next write rebuilds one from the log and the database stays
        // writable without a restart.
        let tx = registry
            .write("varrow", busy, 0, |world| {
                let greg = world.entity_by_handle("greg").unwrap();
                let mut tx = world.begin();
                tx.assert(
                    greg,
                    "alias",
                    kala_core::Value::Str("After the panic".into()),
                    Some(kala_core::Tick(300)),
                )
                .unwrap();
                Ok(tx.commit().unwrap().tx)
            })
            .unwrap();
        assert_eq!(registry.opens("varrow").1, 2);
        assert!(!registry.databases["varrow"].writer.is_poisoned());
        registry.write("varrow", busy, tx.0, |_| Ok(())).unwrap();
        assert_eq!(registry.opens("varrow").1, 2);
    }

    #[test]
    fn poisoned_reader_mutexes_are_recovered_by_reloading_the_snapshot() {
        let (_dir, registry) = registry();
        registry.get(Some("varrow")).unwrap();
        assert_eq!(registry.opens("varrow").0, 1);
        let db = &registry.databases["varrow"];
        // A session thread panics while it holds each of the reader's
        // mutexes (a reload, and the snapshot slot).
        for poison_cached in [false, true] {
            let panicked = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let _loading = (!poison_cached).then(|| db.loading.lock().unwrap());
                        let _cached = poison_cached.then(|| db.cached.lock().unwrap());
                        panic!("a session thread panicked while holding a reader mutex")
                    })
                    .join()
            });
            assert!(panicked.is_err());
        }
        assert!(db.cached.is_poisoned() && db.loading.is_poisoned());
        // The snapshot the panic may have interrupted is discarded, not
        // trusted: the next read reloads it from the log, and the database
        // stays readable without a restart.
        let (_, world) = registry.get(Some("varrow")).unwrap();
        assert!(world.entity_by_handle("greg").is_some());
        assert_eq!(registry.opens("varrow").0, 2);
        assert!(!db.cached.is_poisoned() && !db.loading.is_poisoned());
        // Recovered once: later reads share the reloaded snapshot.
        registry.get(Some("varrow")).unwrap();
        registry
            .at_least("varrow", 0, Duration::from_secs(1))
            .unwrap();
        assert_eq!(registry.opens("varrow").0, 2);
    }

    #[test]
    fn only_errors_that_break_the_handle_drop_the_cached_writer() {
        use kala_core::Error;
        for (error, lost) in [
            (Error::LogDiverged, true),
            (Error::Corrupt("x".into()), true),
            (Error::BadMagic, true),
            (Error::UnsupportedVersion(9), true),
            (Error::RecoveryUnacknowledged, true),
            (Error::WorldLocked, false),
            (Error::TransactionAborted, false),
            (Error::DuplicateHandle("greg".into()), false),
            (Error::UnknownAttribute("nope".into()), false),
        ] {
            assert_eq!(
                crate::write::leaves_handle_unusable(&error),
                lost,
                "{error:?}"
            );
            let (_dir, registry) = registry();
            let busy = Duration::from_secs(1);
            let failed = registry.write("varrow", busy, 0, |_| -> Result<(), Failure> {
                Err(crate::write::store_error(error.clone(), true))
            });
            assert!(failed.is_err());
            registry.write("varrow", busy, 0, |_| Ok(())).unwrap();
            assert_eq!(
                registry.opens("varrow").1,
                1 + usize::from(lost),
                "{error:?}"
            );
        }
    }
}
