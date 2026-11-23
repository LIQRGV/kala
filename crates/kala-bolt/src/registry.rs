//! Immutable database names and lazily cached, committed read-only snapshots.
use crate::error::{DATABASE, Failure, escape};
use kala_core::World;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
struct Database {
    path: PathBuf,
    cached: Mutex<Option<(Stamp, Arc<World>)>>,
    /// Held while a snapshot is reloaded, so concurrent RUNs that need the
    /// new snapshot share one replay instead of each replaying the world
    /// (peak memory stays at about two snapshots, not one per connection).
    loading: Mutex<()>,
}

/// What a cached snapshot was loaded from. The length alone is not enough:
/// recovery can cut a log and a later commit can grow it back to the same
/// length with different content. The last bytes of the file hold the last
/// transaction's commit checksum, so they change with any different last
/// transaction; the modification time covers the rest.
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
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
pub(crate) struct Registry {
    databases: BTreeMap<String, Database>,
    default: Option<String>,
}
impl Registry {
    pub fn open(directory: &Path, default: Option<String>) -> io::Result<Self> {
        let mut databases = BTreeMap::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let text = name.to_string_lossy();
            let db = text.strip_suffix(".kala");
            if entry.file_type()?.is_file() && db.is_some_and(valid_name) {
                databases.insert(
                    db.unwrap_or_default().to_string(),
                    Database {
                        path: entry.path(),
                        cached: Mutex::new(None),
                        loading: Mutex::new(()),
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
        Ok(Self { databases, default })
    }
    /// Resolve the name before loading a world, so authorization precedes I/O.
    pub fn name<'a>(&'a self, requested: Option<&'a str>) -> Result<&'a str, Failure> {
        requested
            .or(self.default.as_deref())
            .ok_or_else(|| Failure::new(DATABASE, "no default database; specify a database name"))
    }
    pub fn get(&self, requested: Option<&str>) -> Result<(String, Arc<World>), Failure> {
        let name = self.name(requested)?;
        let db = self
            .databases
            .get(name)
            .ok_or_else(|| Failure::new(DATABASE, format!("database {name:?} was not found")))?;
        let lock = || {
            db.cached
                .lock()
                .map_err(|_| Failure::new(crate::error::STORE, "the world could not be read"))
        };
        // Take the stamp BEFORE loading: a commit after this observation
        // changes the stamp again and forces another refresh, so a snapshot
        // is never labelled with a newer stamp than its own.
        let stamp = stamp(&db.path)?;
        if let Some((s, world)) = lock()?.as_ref()
            && *s == stamp
        {
            return Ok((name.into(), Arc::clone(world)));
        }
        // Single-flight: one thread replays, the others wait for it and then
        // find the fresh snapshot. Queries whose stamp matches (above) never
        // wait behind a reload.
        let _loading = db
            .loading
            .lock()
            .map_err(|_| Failure::new(crate::error::STORE, "the world could not be read"))?;
        let stamp = self::stamp(&db.path)?;
        if let Some((s, world)) = lock()?.as_ref()
            && *s == stamp
        {
            return Ok((name.into(), Arc::clone(world)));
        }
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
        *lock()? = Some((stamp, Arc::clone(&world)));
        Ok((name.into(), world))
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
