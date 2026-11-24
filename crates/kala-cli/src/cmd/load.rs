//! `kala load <fixture.yaml> <world.kala> [--force]`.
//!
//! The world is built in a temporary sibling file and moved into place only
//! once the whole fixture has loaded, so a failing fixture never leaves a
//! world behind and `--force` with a failing fixture keeps the old world.
//!
//! `--force` replaces a world only while holding its writer lock: the old
//! world is opened right before the rename and its lock held across it, so
//! it is never replaced in the middle of another process's commit. The
//! same goes for an old file that doesn't open as a world, and the lock is
//! checked to be the lock of the file the path still names (see
//! [`replace`]). A
//! process that merely has the old world open holds no lock (the lock is
//! per transaction); its next write or catch-up finds its log gone
//! (`LogDiverged`) and writes nothing. A directory is never replaced.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use kala_core::World;
use kala_core::log::EntryBody;
use kala_fixture::MAX_FIXTURE_BYTES;
use kala_fixture::{FixtureError, FixtureErrorKind};

use crate::cli::LoadArgs;
use crate::error::CliError;
use crate::escape;
use crate::names;

/// Runs `kala load` and returns the text to print on stdout.
pub fn run(args: &LoadArgs) -> Result<String, CliError> {
    let (fixture, world, force) = (args.fixture.as_path(), args.world.as_path(), args.force);
    let file_name = world_file_name(world)?;
    if file_name == "system.kala" {
        return Err(CliError::User("system is reserved; use `kala user`".into()));
    }
    if force && exists(world) {
        refuse_system_file(world)?;
    }
    if is_dir(world) {
        return Err(refuse_dir(world));
    }
    if !force && exists(world) {
        return Err(refuse_existing(world));
    }
    let yaml = read_fixture(fixture)?;
    let dir = match world.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };

    let (tmp, loaded) = load_to_temp(&yaml, fixture, world, &dir, file_name)?;
    if let Err(error) = super::transact::refuse_system(&loaded) {
        drop(loaded);
        remove_quietly(&tmp);
        return Err(error);
    }
    // Summarise before dropping the world: it must be closed before the
    // file is moved into place.
    let summary = summary(&loaded, fixture, world);
    drop(loaded);

    let placed = if force {
        replace(&tmp, world, args.busy_timeout)
    } else {
        place(&tmp, world, false)
    };
    placed.inspect_err(|_| remove_quietly(&tmp))?;
    // Make the rename durable; best effort (the log itself is already
    // fsynced by kala-core).
    if let Ok(d) = File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(summary)
}

/// The world path's file name. Refused (a user error) when there is none
/// (`/`) or when the path as typed doesn't end in it (`new/`, `new/.`): a
/// trailing separator names a directory, and `Path::file_name` would
/// silently drop it.
fn world_file_name(world: &Path) -> Result<&std::ffi::OsStr, CliError> {
    let not_a_file = || {
        CliError::User(format!(
            "{}: not a file path for a world",
            escape::path(world)
        ))
    };
    let name = world.file_name().ok_or_else(not_a_file)?;
    if !world
        .as_os_str()
        .as_encoded_bytes()
        .ends_with(name.as_encoded_bytes())
    {
        return Err(not_a_file());
    }
    Ok(name)
}

/// Whether `p` is a directory (or a symlink to one).
fn is_dir(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_dir())
}

fn refuse_dir(world: &Path) -> CliError {
    CliError::User(format!(
        "{} is a directory; a world is a file, and a directory is never replaced",
        escape::path(world)
    ))
}

/// How often `--force` looks again because the file at the world's path
/// was replaced by another process while it waited for its lock.
const REPLACE_ATTEMPTS: usize = 4;

/// `--force`: moves the loaded file `tmp` over `world` while holding the
/// writer lock of the file that is there, so no commit to it is in
/// progress while it is replaced:
///
/// - an old file that opens as a world is locked through that world
///   ([`World::with_writer_lock`]);
/// - one that doesn't (corrupt, not a Kala log) may still be a log that a
///   process opened earlier and is committing to, so it is locked as a
///   file ([`kala_core::log::with_file_lock`]), waiting as long;
/// - with nothing there (or nothing that can be locked: not a regular
///   file, or unreadable by this user), the file is moved into place.
///
/// Either lock is the lock of one file. If the path names another file
/// once the lock is held (another `load --force` got there first), nothing
/// is replaced under the wrong lock: the file now at the path is looked at
/// and locked instead, a few times over.
fn replace(
    tmp: &Path,
    world: &Path,
    busy_timeout: Option<std::time::Duration>,
) -> Result<(), CliError> {
    let wait = busy_timeout.unwrap_or(kala_core::log::DEFAULT_BUSY_TIMEOUT);
    for _ in 0..REPLACE_ATTEMPTS {
        let placed = match lock_old(world, busy_timeout)? {
            Some(mut old) => old.with_writer_lock(|| place(tmp, world, true)),
            None => match kala_core::log::with_file_lock(world, wait, || place(tmp, world, true)) {
                Ok(Some(placed)) => Ok(placed),
                // Nothing a lock can be taken on.
                Ok(None) | Err(kala_core::Error::Io { .. }) => Ok(place(tmp, world, true)),
                Err(e) => Err(e),
            },
        };
        match placed {
            Ok(placed) => return placed,
            Err(kala_core::Error::LogDiverged) => {}
            Err(e) => return Err(locked_or(world, &e)),
        }
    }
    Err(CliError::Store(format!(
        "{}: another process keeps replacing it; nothing was written",
        escape::path(world)
    )))
}

/// Before `--force` replaces `world`: opens the old world for writing (so
/// the caller can take its writer lock for the rename; the open itself
/// waits for a commit in progress in another process, up to `busy_timeout`
/// (`--busy-timeout`; `None` is kala-core's default, 5 s), and is refused
/// if the lock stays held). Nothing there (or gone meanwhile): `None`. An old file
/// that doesn't open as a world (corrupt, not a Kala log, unreadable) is
/// `None` too: the caller locks it as a file. A directory is refused.
///
/// A process that merely has the old world open, without a commit in
/// progress, holds no lock (the lock is per transaction): the world is
/// replaced, and that process's next write or catch-up reports that its
/// log is gone (`LogDiverged`) instead of writing to the replaced file.
///
/// Opening runs kala-core's normal recovery, so an old world with a torn
/// tail is cut first and its tail saved to a `.torn-` quarantine file next
/// to it, which stays after the replace.
fn lock_old(
    world: &Path,
    busy_timeout: Option<std::time::Duration>,
) -> Result<Option<World>, CliError> {
    if !exists(world) {
        return Ok(None);
    }
    if is_dir(world) {
        return Err(refuse_dir(world));
    }
    refuse_system_file(world)?;
    let wait = busy_timeout.unwrap_or(kala_core::log::DEFAULT_BUSY_TIMEOUT);
    match World::open_waiting(world, wait) {
        Ok(w) => {
            super::transact::refuse_system(&w)?;
            Ok(Some(w))
        }
        Err(e @ kala_core::Error::WorldLocked) => Err(locked_or(world, &e)),
        Err(_) => Ok(None),
    }
}

/// The error of a `--force` replace that couldn't take the old world's
/// writer lock.
fn locked_or(world: &Path, e: &kala_core::Error) -> CliError {
    match e {
        kala_core::Error::WorldLocked => CliError::User(format!(
            "{} is being written by another process (its lock is held); close it before replacing it with --force",
            escape::path(world)
        )),
        e => CliError::at_path(world, e),
    }
}

/// Check content before opening a writer, which could otherwise recover the log.
fn refuse_system_file(path: &Path) -> Result<(), CliError> {
    if let Ok(world) = World::open_read_only(path) {
        super::transact::refuse_system(&world)?;
    }
    Ok(())
}

/// Whether anything (a file, a directory, a dangling symlink) is at `p`.
fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn refuse_existing(world: &Path) -> CliError {
    CliError::User(format!(
        "{} already exists; use --force to replace it",
        escape::path(world)
    ))
}

/// Reads the fixture whole, refusing files over kala-core's loader limit
/// before reading them (and while reading, if the file grows meanwhile).
fn read_fixture(path: &Path) -> Result<String, CliError> {
    let shown = escape::path(path);
    let io_err =
        |e: std::io::Error| CliError::User(format!("{shown}: cannot read the fixture: {e}"));
    let file = File::open(path).map_err(io_err)?;
    let meta = file.metadata().map_err(io_err)?;
    let limit = MAX_FIXTURE_BYTES as u64;
    let too_large = || {
        CliError::User(format!(
            "{shown}: the fixture is larger than the {MAX_FIXTURE_BYTES}-byte limit"
        ))
    };
    if meta.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    String::from_utf8(bytes)
        .map_err(|_| CliError::User(format!("{shown}: the fixture is not valid UTF-8")))
}

/// A temporary file name next to the world, unlikely to collide.
fn temp_path(dir: &Path, file_name: &std::ffi::OsStr, attempt: u32) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut name = std::ffi::OsString::from(".");
    name.push(file_name);
    name.push(format!(
        ".load-{}-{nanos}-{attempt}.tmp",
        std::process::id()
    ));
    dir.join(name)
}

/// Loads the fixture into a fresh temporary file in `dir`. On failure no
/// temporary file is left behind.
fn load_to_temp(
    yaml: &str,
    fixture: &Path,
    world: &Path,
    dir: &Path,
    file_name: &std::ffi::OsStr,
) -> Result<(PathBuf, World), CliError> {
    const ATTEMPTS: u32 = 8;
    for attempt in 0..ATTEMPTS {
        let tmp = temp_path(dir, file_name, attempt);
        match kala_fixture::load_fixture_to(yaml, &tmp) {
            Ok(w) => return Ok((tmp, w)),
            // `World::create` refused the name: someone else's file, so it
            // isn't removed; try another name.
            Err(FixtureError {
                at: None,
                kind:
                    FixtureErrorKind::Store(kala_core::Error::Io {
                        kind: ErrorKind::AlreadyExists,
                        ..
                    }),
            }) => continue,
            Err(e) => {
                remove_quietly(&tmp);
                return Err(fixture_error(&e, fixture, world));
            }
        }
    }
    Err(CliError::Store(format!(
        "{}: cannot create a temporary file next to it",
        escape::path(world)
    )))
}

fn remove_quietly(p: &Path) {
    let _ = fs::remove_file(p);
}

/// Moves the loaded temporary file to `world`. Without `force` an existing
/// world is never replaced, even one created while the fixture loaded: the
/// hard link fails if the name is taken.
fn place(tmp: &Path, world: &Path, force: bool) -> Result<(), CliError> {
    let shown = escape::path(world);
    let io_err = |e: std::io::Error| CliError::io(e.kind(), format!("{shown}: {e}"));
    if force {
        return fs::rename(tmp, world).map_err(io_err);
    }
    match fs::hard_link(tmp, world) {
        Ok(()) => {
            remove_quietly(tmp);
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(refuse_existing(world)),
        // No hard links on this file system: fall back to a checked rename.
        Err(_) if !exists(world) => fs::rename(tmp, world).map_err(io_err),
        Err(_) => Err(refuse_existing(world)),
    }
}

/// `<fixture>: transaction N, op M (label `l`): <why>`, positions 1-based.
fn fixture_error(e: &FixtureError, fixture: &Path, world: &Path) -> CliError {
    let mut msg = escape::path(fixture);
    msg.push_str(": ");
    if let Some(at) = &e.at {
        let _ = write!(msg, "transaction {}, op {}", at.txn + 1, at.op + 1);
        if let Some(l) = &at.label {
            let _ = write!(msg, " (label `{}`)", escape::value(l));
        }
        msg.push_str(": ");
    }
    match &e.kind {
        // The world file couldn't be created: name the world, not the
        // fixture.
        FixtureErrorKind::Store(core @ kala_core::Error::Io { .. }) if e.at.is_none() => {
            CliError::core(
                core,
                format!("{}: cannot create the world: {core}", escape::path(world)),
            )
        }
        FixtureErrorKind::Store(core) => {
            let _ = write!(msg, "{core}");
            CliError::core(core, msg)
        }
        kind => {
            let _ = write!(msg, "{kind}");
            CliError::User(msg)
        }
    }
}

/// The counts line, latest tx and tick, and one `warning:` line per overlap
/// at the latest canon.
fn summary(w: &World, fixture: &Path, world: &Path) -> String {
    let (mut entities, mut facts, mut transactions) = (0usize, 0usize, 0usize);
    for e in w.entries() {
        match e.body {
            EntryBody::DefineEntity { .. } => entities += 1,
            EntryBody::Fact { .. } | EntryBody::Revised { .. } => facts += 1,
            EntryBody::Commit { .. } => transactions += 1,
            _ => {}
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "loaded {} into {}",
        escape::path(fixture),
        escape::path(world)
    );
    let _ = writeln!(
        out,
        "{entities} entities, {facts} facts, {transactions} transactions"
    );
    let _ = writeln!(
        out,
        "latest tx#{}, latest tick {}",
        w.latest_tx().0,
        w.latest_tick().0
    );
    let names: Vec<String> = w.attributes().map(|a| a.name.clone()).collect();
    for attr in names {
        // Every name comes from the schema, so this can't fail; skip
        // rather than abort the summary if it ever does.
        let Ok(overlaps) = w.overlaps(&attr) else {
            continue;
        };
        for o in &overlaps {
            let _ = writeln!(out, "warning: {}", names::overlap_line(w, o));
        }
    }
    out
}
