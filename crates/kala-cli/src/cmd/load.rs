//! `kala load <fixture.yaml> <world.kala> [--force]`.
//!
//! The world is built in a temporary sibling file and moved into place only
//! once the whole fixture has loaded, so a failing fixture never leaves a
//! world behind and `--force` with a failing fixture keeps the old world.
//!
//! `--force` replaces a world only while holding its lock: the old world is
//! opened (taking kala-core's exclusive lock) right before the rename and
//! kept open across it, so a world open in another process is never
//! replaced under it. A directory is never replaced.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use kala_core::fixture::MAX_FIXTURE_BYTES;
use kala_core::log::EntryBody;
use kala_core::{FixtureError, FixtureErrorKind, World};

use crate::error::CliError;
use crate::escape;
use crate::names;

/// Runs `kala load` and returns the text to print on stdout.
pub fn run(fixture: &Path, world: &Path, force: bool) -> Result<String, CliError> {
    let file_name = world_file_name(world)?;
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
    // Summarise before dropping the world: dropping releases its file lock,
    // which must happen before the file is moved into place.
    let summary = summary(&loaded, fixture, world);
    drop(loaded);

    // The old world's lock, held until the rename is done and the
    // directory synced (dropped at the end of this function).
    let _held = if force { lock_old(world) } else { Ok(None) }
        .and_then(|held| place(&tmp, world, force).map(|()| held))
        .inspect_err(|_| remove_quietly(&tmp))?;
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

/// Before `--force` replaces `world`: opens the old world to take its lock,
/// so one open in another process is refused, and returns it so the caller
/// holds the lock across the rename. Nothing there (or gone meanwhile):
/// `None`. An old file that doesn't open as a world (corrupt, not a Kala
/// log, unreadable) has no lock to respect and is replaced as asked:
/// `None` too. A directory is refused.
///
/// Opening runs kala-core's normal recovery, so an old world with a torn
/// tail is cut first and its tail saved to a `.torn-` quarantine file next
/// to it, which stays after the replace.
fn lock_old(world: &Path) -> Result<Option<World>, CliError> {
    if !exists(world) {
        return Ok(None);
    }
    if is_dir(world) {
        return Err(refuse_dir(world));
    }
    match World::open(world) {
        Ok(w) => Ok(Some(w)),
        Err(kala_core::Error::WorldLocked) => Err(CliError::User(format!(
            "{} is open in another process (its lock is held); close it before replacing it with --force",
            escape::path(world)
        ))),
        Err(_) => Ok(None),
    }
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
        match kala_core::load_fixture_to(yaml, &tmp) {
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
