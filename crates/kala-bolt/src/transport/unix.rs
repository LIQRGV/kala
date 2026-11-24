//! Filesystem socket ownership stays here, away from the Bolt session.
use super::{ListenAddr, Peer, ShutdownHandle, Transport, invalid, remaining};
use rustix::fs::{self as fs, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::net::{self as net, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::{
    ffi::OsStrExt,
    fs::PermissionsExt,
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) fn validate_path(path: &Path) -> io::Result<()> {
    // sockaddr_un's pathname must leave room for the terminating NUL.
    let limit = std::mem::size_of::<libc::sockaddr_un>()
        - std::mem::offset_of!(libc::sockaddr_un, sun_path)
        - 1;
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid("Unix listen path must be an absolute file path"));
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > limit {
        return Err(invalid(format!(
            "Unix socket path exceeds sun_path limit of {limit} bytes"
        )));
    }
    if bytes.iter().any(|b| b.is_ascii_control()) {
        return Err(invalid(
            "Unix socket path contains a NUL or control character",
        ));
    }
    Ok(())
}

/// A pinned directory, so pathname ancestors cannot redirect link/unlink/stat.
/// Linux also supports using the descriptor as a short bind/connect pathname.
fn directory_path(directory: &File, fallback: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let _ = fallback;
        PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        fallback.to_owned()
    }
}

// Linux pins every staging operation through procfs. Other Unix systems use
// canonical paths only after checking every ancestor's owner and write policy.
// This excludes untrusted ancestor renames, not just changes in the last parent.
#[cfg(not(target_os = "linux"))]
fn require_trusted_ancestry(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut ancestors: Vec<_> = path.ancestors().collect();
    ancestors.reverse();
    for ancestor in ancestors {
        let meta = std::fs::symlink_metadata(ancestor)?;
        if !meta.is_dir()
            || !trusted_parent(meta.uid(), meta.mode(), rustix::process::geteuid().as_raw())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unix socket path requires trusted directory ancestors on this platform",
            ));
        }
    }
    Ok(())
}

/// A new private directory. mkdir gets mode 0700 itself, so the directory
/// never exists with wider permissions, whatever the umask (tempfile's
/// default is 0777 minus the umask).
fn private_directory(prefix: &str, parent: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
}

fn staging_directory(parent: &Path, parent_stat: &Stat) -> io::Result<tempfile::TempDir> {
    #[cfg(target_os = "linux")]
    {
        let _ = parent_stat;
        private_directory(".kala-", parent).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Unix socket staging through /proc/self/fd: {e}"),
            )
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::unix::fs::MetadataExt;
        // A short private name in a writable ancestor on the same filesystem
        // avoids imposing an extra staging-name limit on the configured path.
        for candidate in parent.ancestors() {
            // dev_t is i32 on Apple targets and u64 on Linux; compare as u64.
            #[allow(clippy::unnecessary_cast)]
            let parent_dev = parent_stat.st_dev as u64;
            if std::fs::metadata(candidate)?.dev() != parent_dev {
                break;
            }
            match private_directory(".k-", candidate) {
                Ok(stage) if validate_path(&stage.path().join("s")).is_ok() => return Ok(stage),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => (),
                Err(e) => return Err(e),
            }
        }
        Err(invalid(
            "cannot stage Unix socket within the platform sun_path limit on this filesystem",
        ))
    }
}

/// How long a start waits for the lock file before reporting "in use".
/// Long enough for a server that is shutting down on the same path to let
/// go, short enough that a path that is really in use fails promptly.
const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// The lock file next to the socket, `.<socket name>.lock`, flock-ed
/// exclusively for the listener's whole lifetime (as PostgreSQL does).
///
/// Holding it is what proves that a socket already at the path does not
/// belong to a live Kala server, so the stale check does not have to infer
/// that from a connect error. It is a file of our own (mode 0600), not the
/// parent directory: any user who can open a directory read-only can flock
/// it and would block every start and cleanup.
struct SocketLock {
    file: File,
    parent: File,
    name: OsString,
    identity: Stat,
}
impl SocketLock {
    fn take(parent: &File, socket_name: &OsStr, socket_path: &Path) -> io::Result<Self> {
        let mut name = OsString::from(".");
        name.push(socket_name);
        name.push(".lock");
        let shown = socket_path.with_file_name(&name);
        let unusable = |kind, reason: &dyn std::fmt::Display| {
            io::Error::new(
                kind,
                format!(
                    "lock file {} cannot be used ({reason}); it must be a regular file owned by this user, and symlinks are refused: remove it and start again",
                    shown.display()
                ),
            )
        };
        let euid = rustix::process::geteuid().as_raw();
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            // Read-only is enough for flock. NOFOLLOW: never create or lock
            // through a symlink. NONBLOCK and NOCTTY: opening a FIFO or a
            // terminal someone planted there must neither hang nor have side
            // effects.
            let file = fs::openat(
                parent,
                &name,
                OFlags::RDONLY
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK
                    | OFlags::NOCTTY,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|e| {
                let e = io::Error::from(e);
                unusable(e.kind(), &e)
            })?;
            #[cfg(test)]
            tests::between_open_and_lock();
            let identity = fs::fstat(&file)?;
            if FileType::from_raw_mode(identity.st_mode) != FileType::RegularFile {
                return Err(unusable(
                    io::ErrorKind::AlreadyExists,
                    &"it is not a regular file",
                ));
            }
            if identity.st_uid != euid {
                return Err(unusable(
                    io::ErrorKind::PermissionDenied,
                    &"it belongs to another user",
                ));
            }
            match fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {
                    // A holder removes the file before it lets go (see Drop),
                    // so a lock on a file that is no longer at the name is
                    // worth nothing: open the current file and try again.
                    let current = fs::statat(parent, &name, AtFlags::SYMLINK_NOFOLLOW);
                    if current.is_ok_and(|c| same_inode(&c, &identity)) {
                        return Ok(Self {
                            file: File::from(file),
                            parent: parent.try_clone()?,
                            name,
                            identity,
                        });
                    }
                }
                Err(rustix::io::Errno::WOULDBLOCK) => {}
                Err(e) => {
                    let e = io::Error::from(e);
                    return Err(unusable(e.kind(), &e));
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "unix://{} is already in use by another kala serve (lock file {} is held)",
                        socket_path.display(),
                        shown.display()
                    ),
                ));
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }
}
impl Drop for SocketLock {
    fn drop(&mut self) {
        // Remove the file while the lock is still held, then let go. A start
        // that was waiting on this file then finds its name gone (or naming a
        // newer file) and retries, so two servers can never both hold "the"
        // lock. After a crash the empty file stays; the next start reuses it.
        let still_ours = fs::fstat(&self.parent).is_ok_and(|p| trusted_parent_stat(&p))
            && fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|current| same_inode(&current, &self.identity));
        if still_ours {
            let _ = fs::unlinkat(&self.parent, &self.name, AtFlags::empty());
        }
        let _ = fs::flock(&self.file, FlockOperation::Unlock);
    }
}

/// A nonblocking, close-on-exec Unix stream socket for the stale probe.
fn probe_socket() -> io::Result<OwnedFd> {
    // Apple targets have no SOCK_NONBLOCK / SOCK_CLOEXEC (rustix does not
    // define the flags there), so both are set with fcntl after creation.
    #[cfg(not(target_vendor = "apple"))]
    {
        Ok(net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )?)
    }
    #[cfg(target_vendor = "apple")]
    {
        let socket = net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )?;
        rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::CLOEXEC)?;
        fs::fcntl_setfl(&socket, OFlags::NONBLOCK)?;
        Ok(socket)
    }
}

/// What a connect to an existing socket says about its listener.
enum Probe {
    /// Something accepted the connection, or queued it.
    Live,
    /// Nothing listens on it.
    Stale,
    /// This user may not connect to it (or hard-link it), so nobody knows.
    Unknown,
}

/// Connects to the socket `old` at `name`, without blocking, through a short
/// private hard link: a long basename could make the pinned-directory alias
/// exceed sun_path even when the requested path fits, and the link pins the
/// checked inode so a replacement symlink is never followed.
fn probe(
    parent: &File,
    name: &OsStr,
    stage_dir: &File,
    stage_path: &Path,
    old: &Stat,
) -> io::Result<Probe> {
    use rustix::io::Errno;
    let socket = probe_socket()?;
    match fs::linkat(parent, name, stage_dir, "p", AtFlags::empty()) {
        Ok(()) => {}
        // protected_hardlinks refuses links to another user's socket.
        Err(Errno::PERM | Errno::ACCESS) => return Ok(Probe::Unknown),
        Err(e) => return Err(e.into()),
    }
    let linked = fs::statat(stage_dir, "p", AtFlags::SYMLINK_NOFOLLOW)?;
    if !same_socket(old, &linked) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Unix socket changed while checking stale path",
        ));
    }
    let address = SocketAddrUnix::new(directory_path(stage_dir, stage_path).join("p"))?;
    match net::connect(&socket, &address) {
        Ok(()) | Err(Errno::AGAIN | Errno::INPROGRESS) => Ok(Probe::Live),
        // On Linux this means no listener: a full backlog answers EAGAIN.
        // BSD-family kernels (macOS, FreeBSD) also answer ECONNREFUSED when a
        // live listener's backlog is full. A live Kala server is never taken
        // for stale that way, because the caller holds the lock file; what
        // remains there is a non-Kala listener with a full backlog on the
        // same path, whose socket file would be replaced. Accepted, and
        // documented in the README.
        Err(Errno::CONNREFUSED) => Ok(Probe::Stale),
        // Connecting needs write permission on the socket, which the server
        // that created it may have withheld from itself (--unix.mode 0000).
        Err(Errno::ACCESS | Errno::PERM) => Ok(Probe::Unknown),
        Err(e) => Err(e.into()),
    }
}

fn trusted_parent(uid: u32, mode: u32, euid: u32) -> bool {
    (uid == euid || uid == 0) && (mode & 0o022 == 0 || mode & 0o1000 != 0)
}

// mode_t is u16 on BSD and u32 on Linux; both casts are intentional.
#[allow(clippy::unnecessary_cast)]
fn trusted_parent_stat(stat: &Stat) -> bool {
    trusted_parent(
        stat.st_uid,
        stat.st_mode as u32,
        rustix::process::geteuid().as_raw(),
    )
}
#[allow(clippy::unnecessary_cast)]
fn socket_mode(mode: u32) -> Mode {
    // Server validation limits this to 0777, which fits either mode_t width.
    Mode::from_raw_mode(mode as fs::RawMode)
}

#[cfg(test)]
#[path = "unix_tests.rs"]
mod tests;

fn same_inode(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
fn same_socket(a: &Stat, b: &Stat) -> bool {
    FileType::from_raw_mode(b.st_mode) == FileType::Socket && same_inode(a, b)
}

pub(crate) struct BoundUnix {
    listener: UnixListener,
    pub(super) path: PathBuf,
    parent: File,
    name: OsString,
    identity: Stat,
    /// Declared last: released (and its file removed) only after `Drop` has
    /// removed the socket and the listener is closed.
    _lock: SocketLock,
}
impl BoundUnix {
    pub(super) fn bind(path: &Path, mode: u32) -> io::Result<Self> {
        validate_path(path)?;
        let parent_path = path
            .parent()
            .ok_or_else(|| invalid("Unix path needs a parent directory"))?;
        #[cfg(not(target_os = "linux"))]
        let canonical_parent = std::fs::canonicalize(parent_path)?;
        #[cfg(not(target_os = "linux"))]
        let parent_path = canonical_parent.as_path();
        #[cfg(not(target_os = "linux"))]
        require_trusted_ancestry(parent_path)?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Unix path needs a filename"))?
            .to_owned();
        let parent = File::from(fs::open(
            parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let parent_stat = fs::fstat(&parent)?;
        // POSIX has no conditional unlink-by-inode. Exclude untrusted rename
        // races: a writable shared parent must have sticky-directory protection.
        // Same-UID processes already share the account's filesystem authority.
        if !trusted_parent_stat(&parent_stat) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unix socket parent must belong to this user or root and not be group/other writable unless sticky",
            ));
        }
        // Held from here until this listener is dropped; also released, and
        // the lock file removed, on every early return below.
        let lock = SocketLock::take(&parent, &name, path)?;
        let parent_alias = directory_path(&parent, parent_path);
        let stage = staging_directory(&parent_alias, &parent_stat)?;
        // Created 0700 minus the umask: never wider, but a restrictive caller
        // umask can strip owner bits, so restore them. No other user ever
        // gains access to the staged socket.
        std::fs::set_permissions(stage.path(), std::fs::Permissions::from_mode(0o700))?;
        let stage_dir = File::open(stage.path())?;
        let staged_path = directory_path(&stage_dir, stage.path()).join("s");
        validate_path(&staged_path)?;
        let listener = UnixListener::bind(&staged_path)?;
        listener.set_nonblocking(true)?;
        fs::chmodat(&stage_dir, "s", socket_mode(mode), AtFlags::empty())?;
        let identity = fs::statat(&stage_dir, "s", AtFlags::SYMLINK_NOFOLLOW)?;
        match fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(old) => {
                if FileType::from_raw_mode(old.st_mode) != FileType::Socket {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Unix listen path exists and is not a socket (symlinks are refused)",
                    ));
                }
                if parent_stat.st_mode & 0o1000 != 0
                    && old.st_uid != rustix::process::geteuid().as_raw()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Unix socket in a sticky directory belongs to another user and cannot be replaced: remove it as that user or choose another path",
                    ));
                }
                let euid = rustix::process::geteuid().as_raw();
                match probe(&parent, &name, &stage_dir, stage.path(), &old)? {
                    Probe::Stale => (),
                    Probe::Live => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!(
                                "unix://{} is already in use: a listener that is not a kala serve answers on it",
                                path.display()
                            ),
                        ));
                    }
                    // We hold the Kala lock, so no live Kala server owns this
                    // socket; it is ours, in a trusted parent: a leftover.
                    Probe::Unknown if old.st_uid == euid => (),
                    Probe::Unknown => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "unix://{} holds a socket owned by another user (uid {}) that this user may not probe, so it cannot be told whether a server still uses it: remove the file or choose another path",
                                path.display(),
                                old.st_uid
                            ),
                        ));
                    }
                }
                let current = fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)?;
                if !same_socket(&old, &current) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Unix socket changed while checking stale path",
                    ));
                }
                fs::unlinkat(&parent, &name, AtFlags::empty())?;
            }
            Err(rustix::io::Errno::NOENT) => (),
            Err(e) => return Err(e.into()),
        }
        // linkat is no-clobber, unlike rename: a racing new destination is
        // never overwritten. Permissions are final before the public link exists.
        fs::linkat(&stage_dir, "s", &parent, &name, AtFlags::empty())?;
        drop(stage_dir);
        drop(stage);
        Ok(Self {
            listener,
            path: path.to_owned(),
            parent,
            name,
            identity,
            _lock: lock,
        })
    }
    pub(super) fn accept(&self) -> io::Result<super::Accepted> {
        let (stream, _) = self.listener.accept()?;
        Ok(super::Accepted::Unix(AcceptedUnix {
            stream,
            path: self.path.clone(),
        }))
    }
}

/// An accepted Unix connection before its per-socket setup.
pub(crate) struct AcceptedUnix {
    stream: UnixStream,
    path: PathBuf,
}
impl AcceptedUnix {
    pub(super) fn into_transport(self) -> io::Result<Box<dyn Transport>> {
        // Some platforms inherit nonblocking mode from the listener.
        self.stream.set_nonblocking(false)?;
        Ok(Box::new(Unix {
            stream: self.stream,
            path: self.path,
        }))
    }
    pub(super) fn close(self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
}
impl Drop for BoundUnix {
    fn drop(&mut self) {
        let cleanup = || -> io::Result<()> {
            let parent = fs::fstat(&self.parent)?;
            if !trusted_parent_stat(&parent) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Unix socket parent permissions changed",
                ));
            }
            match fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(current) if same_socket(&self.identity, &current) => {
                    fs::unlinkat(&self.parent, &self.name, AtFlags::empty())?;
                }
                Ok(_) | Err(rustix::io::Errno::NOENT) => (),
                Err(e) => return Err(e.into()),
            }
            Ok(())
        };
        if let Err(e) = cleanup() {
            eprintln!("event=bolt_socket_cleanup_failed error={:?}", e.kind());
        }
    }
}

struct Unix {
    stream: UnixStream,
    path: PathBuf,
}
impl ShutdownHandle for UnixStream {
    fn shutdown(&self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}
impl Transport for Unix {
    fn read_until(&mut self, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(remaining(deadline)?))?;
        self.stream.read(bytes)
    }
    fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(remaining(deadline)?))?;
        self.stream.write(bytes)
    }
    fn shutdown_handle(&self) -> io::Result<Box<dyn ShutdownHandle>> {
        Ok(Box::new(self.stream.try_clone()?))
    }
    fn peer(&self) -> io::Result<Peer> {
        Ok(Peer::Local)
    }
    fn local_addr(&self) -> io::Result<ListenAddr> {
        Ok(ListenAddr::Unix(self.path.clone()))
    }
}
