//! Immutable database names and lazily cached, committed read-only snapshots.
use crate::error::{DATABASE, Failure, escape};
use kala_core::World;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
struct Database {
    path: PathBuf,
    cached: Mutex<Option<(u64, Arc<World>)>>,
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
        let default = default.or_else(|| {
            (databases.len() == 1)
                .then(|| databases.keys().next().cloned())
                .flatten()
        });
        Ok(Self { databases, default })
    }
    pub fn get(&self, requested: Option<&str>) -> Result<(String, Arc<World>), Failure> {
        let name = requested.or(self.default.as_deref()).ok_or_else(|| {
            Failure::new(DATABASE, "no default database; specify a database name")
        })?;
        let db = self
            .databases
            .get(name)
            .ok_or_else(|| Failure::new(DATABASE, format!("database {name:?} was not found")))?;
        let mut cached = db
            .cached
            .lock()
            .map_err(|_| Failure::new(crate::error::STORE, "the world could not be read"))?;
        // Save the BEFORE length: a concurrent commit after this observation
        // forces another refresh. Never stamp a stale snapshot with a newer length.
        let metadata = std::fs::symlink_metadata(&db.path).map_err(Failure::store)?;
        if !metadata.file_type().is_file() {
            return Err(Failure::store("world is not a regular file"));
        }
        let length = metadata.len();
        if cached.as_ref().is_none_or(|(len, _)| *len != length) {
            let world = Arc::new(World::open_read_only(&db.path).map_err(Failure::store)?);
            eprintln!(
                "event=bolt_database_loaded db={name} canon={}",
                world.latest_tx().0
            );
            *cached = Some((length, world));
        }
        match cached.as_ref() {
            Some((_, world)) => Ok((name.into(), Arc::clone(world))),
            None => Err(Failure::new(
                crate::error::STORE,
                "the world could not be read",
            )),
        }
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
