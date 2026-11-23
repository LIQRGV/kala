//! Opening and reading the log file: only a regular file, never without
//! bound, and the directory fsync that makes a new file durable.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

use crate::error::Error;

/// Reads the bytes of the log file at `path`, for the readers that take no
/// lock ([`read_log`](super::read_log), [`World::open_read_only`](crate::World::open_read_only),
/// doctor). Refuses anything but a regular file as [`open_regular`] does,
/// and reads at most the length the opened file had (a snapshot: bytes a
/// writer appends meanwhile are left for the next read).
pub(crate) fn read_log_file(path: &Path) -> Result<Vec<u8>, Error> {
    let mut file = open_regular(path, OpenOptions::new().read(true))?;
    read_to_len(&mut file)
}

/// Opens `path` with `opts`, refusing anything but a regular file (after
/// following symlinks) with [`Error::Io`] of kind `InvalidInput`.
///
/// Two checks, because a log reader must never hang or read without bound:
/// - Before opening, the path's metadata: opening a FIFO for reading
///   blocks until a writer shows up, so a FIFO (or a device such as
///   `/dev/zero`, which never ends) must be turned away before `open`.
/// - After opening, the handle's own metadata (`fstat`): that is the file
///   actually read, so a path swapped between the two calls is still
///   refused. (Only a swap to a FIFO in that window could still make the
///   `open` itself wait; that needs write access to the directory.)
pub(super) fn open_regular(path: &Path, opts: &OpenOptions) -> Result<File, Error> {
    refuse_non_regular(&std::fs::metadata(path)?)?;
    let file = opts.open(path)?;
    refuse_non_regular(&file.metadata()?)?;
    Ok(file)
}

/// `Err(Io { kind: InvalidInput })` unless `meta` is a regular file's.
fn refuse_non_regular(meta: &std::fs::Metadata) -> Result<(), Error> {
    let ft = meta.file_type();
    if ft.is_file() {
        return Ok(());
    }
    let what = if ft.is_dir() {
        "a directory"
    } else if ft.is_symlink() {
        "a symlink"
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if ft.is_fifo() {
                "a FIFO"
            } else if ft.is_char_device() || ft.is_block_device() {
                "a device"
            } else if ft.is_socket() {
                "a socket"
            } else {
                "not a regular file"
            }
        }
        #[cfg(not(unix))]
        {
            "not a regular file"
        }
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("not a regular file ({what})"),
    )
    .into())
}

/// Reads `file` from its current position up to the length its metadata
/// gives now, never more: a file that grows while being read (or claims an
/// endless length) can't make this read without bound. The buffer is
/// reserved fallibly, so a length too big for memory is an error, not an
/// abort.
pub(super) fn read_to_len(file: &mut File) -> Result<Vec<u8>, Error> {
    let len = file.metadata()?.len();
    let too_big = || {
        Error::from(std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!("log file of {len} bytes doesn't fit in memory"),
        ))
    };
    let cap = usize::try_from(len).map_err(|_| too_big())?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(cap).map_err(|_| too_big())?;
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Fsyncs the directory holding `path`, so a newly created file's directory
/// entry survives a crash.
pub(super) fn sync_parent_dir(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
