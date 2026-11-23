//! Private staging and no-clobber publication shared by create/dump/export.
use crate::{error::CliError, escape};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(super) fn io(e: std::io::Error) -> CliError {
    CliError::io(e.kind(), e.to_string())
}
pub(super) fn absent(path: &Path) -> Result<(), CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(CliError::Usage(format!(
            "{} already exists",
            escape::path(path)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(e)),
    }
}
pub(super) fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
/// A private staging directory on the destination filesystem. Its cleanup
/// covers parsing, validation, log-write and publication errors alike.
pub(super) struct Staging {
    dir: PathBuf,
    pub path: PathBuf,
}
impl Staging {
    pub fn new(target: &Path) -> Result<Self, CliError> {
        let name = target
            .file_name()
            .ok_or_else(|| CliError::Usage("expected a file path".into()))?;
        if !target
            .as_os_str()
            .as_encoded_bytes()
            .ends_with(name.as_encoded_bytes())
        {
            return Err(CliError::Usage("expected a file path".into()));
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        for n in 0..32 {
            let dir =
                parent(target).join(format!(".kala-transfer-{}-{nonce}-{n}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        path: dir.join("payload"),
                        dir,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io(e)),
            }
        }
        Err(CliError::Store(
            "cannot allocate a private staging directory".into(),
        ))
    }
    /// Publish the staged file. `atomic_only` refuses the copy fallback:
    /// an accounts database must never be visible half-written, since a
    /// running server would read its committed prefix (whose first account
    /// is the trust-authenticated bootstrap root).
    pub fn place(&self, target: &Path, force: bool, atomic_only: bool) -> Result<(), CliError> {
        if force {
            fs::rename(&self.path, target).map_err(io)?;
        } else {
            match fs::hard_link(&self.path, target) {
                // Published: the staged name is cleaned up best-effort (Drop
                // removes the staging directory anyway).
                Ok(()) => {
                    let _ = fs::remove_file(&self.path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(CliError::Usage("destination already exists".into()));
                }
                // Some file systems (FAT, exFAT, some network mounts) have no
                // hard links: publish with an exclusive create and a copy,
                // still never clobbering, and remove a partial copy on error.
                Err(_) if atomic_only => {
                    return Err(CliError::User(
                        "restoring an accounts database needs a file system with hard links".into(),
                    ));
                }
                Err(_) => copy_new(&self.path, target)?,
            }
        }
        // The file is in place; a failed directory sync must not report the
        // publication as failed (a retry would then refuse "already exists").
        if let Err(e) = sync_parent(target) {
            eprintln!(
                "warning: published {} but could not sync its directory: {}",
                escape::path(target),
                e.message()
            );
        }
        Ok(())
    }
}
fn copy_new(from: &Path, target: &Path) -> Result<(), CliError> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = fs::metadata(from).map_err(io)?.permissions().mode() & 0o777;
        opts.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }
    let mut out = opts.open(target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            CliError::Usage("destination already exists".into())
        } else {
            io(e)
        }
    })?;
    let copied = File::open(from)
        .and_then(|mut src| std::io::copy(&mut src, &mut out))
        .and_then(|_| out.sync_all());
    if let Err(e) = copied {
        // Remove the partial copy only if the path still names our file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ours = out.metadata().ok().map(|m| (m.dev(), m.ino()));
            let there = fs::symlink_metadata(target)
                .ok()
                .map(|m| (m.dev(), m.ino()));
            if ours.is_some() && ours == there {
                let _ = fs::remove_file(target);
            }
        }
        #[cfg(not(unix))]
        {
            drop(out);
            let _ = fs::remove_file(target);
        }
        return Err(io(e));
    }
    Ok(())
}
/// Make a publication durable. Directories can only be opened and synced on
/// Unix; elsewhere the platform's own rename/link durability applies.
fn sync_parent(target: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        File::open(parent(target))
            .and_then(|d| d.sync_all())
            .map_err(io)?;
    }
    #[cfg(not(unix))]
    let _ = target;
    Ok(())
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
pub(super) fn private_file(path: &Path) -> Result<File, CliError> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path).map_err(io)
}
pub(super) fn output(path: Option<&Path>, text: &str, force: bool) -> Result<String, CliError> {
    let Some(path) = path else {
        return Ok(text.to_owned());
    };
    if !force {
        absent(path)?;
    }
    let stage = Staging::new(path)?;
    let mut file = private_file(&stage.path)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io)?;
    stage.place(path, force, false)?;
    Ok(String::new())
}
/// Local restore inputs have no size cap, but must be finite regular files
/// (or explicit stdin). Read only the opened file's observed length.
pub(super) fn input(path: &Path) -> Result<String, CliError> {
    let mut text = String::new();
    if path == Path::new("-") {
        std::io::stdin().read_to_string(&mut text).map_err(io)?;
    } else {
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let file = opts.open(path).map_err(io)?;
        let meta = file.metadata().map_err(io)?;
        if !meta.is_file() {
            return Err(CliError::User("input must be a regular file".into()));
        }
        file.take(meta.len())
            .read_to_string(&mut text)
            .map_err(io)?;
    }
    Ok(text)
}
