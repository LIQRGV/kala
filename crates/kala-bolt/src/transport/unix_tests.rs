use super::trusted_parent;
use std::cell::RefCell;

thread_local! {
    /// Runs once, in `SocketLock::take`, after the lock file is opened and
    /// before it is flock-ed: the window in which another server can finish
    /// shutting down and remove the file.
    static BETWEEN_OPEN_AND_LOCK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
pub(super) fn between_open_and_lock() {
    if let Some(hook) = BETWEEN_OPEN_AND_LOCK.with(|h| h.borrow_mut().take()) {
        hook();
    }
}

#[test]
fn socket_parent_owner_and_write_permissions_are_both_trusted() {
    for (owner, mode, expected) in [
        (1001, 0o755, false),
        (1001, 0o1777, false),
        (1000, 0o755, true),
        (0, 0o1777, true),
        (1000, 0o777, false),
        (0, 0o777, false),
        (1000, 0o770, false),
        (1000, 0o1777, true),
    ] {
        assert_eq!(
            trusted_parent(owner, mode, 1000),
            expected,
            "owner={owner} mode={mode:o} effective_uid=1000"
        );
    }
}

// The staging directory holds the socket before its final mode is set, so it
// must never exist with wider permissions than 0700, whatever the umask.
// umask is process-wide: the check runs in a child copy of this test binary.
#[test]
fn staging_directory_is_created_private_under_umask_000() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "KALA_STAGING_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transport::unix::tests::staging_directory_is_created_private_under_umask_000",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    rustix::process::umask(rustix::fs::Mode::empty());
    let parent = tempfile::tempdir().unwrap();
    let parent_stat = rustix::fs::stat(parent.path()).unwrap();
    let stage = super::staging_directory(parent.path(), &parent_stat).unwrap();
    let mode = std::fs::metadata(stage.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o700, "staging directory was created {mode:o}");
}

// A server shutting down removes its lock file while it still holds the lock.
// A start that had already opened that file must not treat a lock on the
// removed inode as the lock: a third server would create and lock a new file
// and both would "hold" the path.
#[test]
fn a_lock_on_a_removed_lock_file_is_not_taken_for_the_lock() {
    use rustix::fs::{AtFlags, statat};
    let dir = tempfile::tempdir().unwrap();
    let parent = std::fs::File::open(dir.path()).unwrap();
    let socket = dir.path().join("bolt.sock");
    let lock_path = dir.path().join(".bolt.sock.lock");
    std::fs::write(&lock_path, b"").unwrap();
    let removed = lock_path.clone();
    BETWEEN_OPEN_AND_LOCK.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || std::fs::remove_file(removed).unwrap()));
    });
    let lock =
        super::SocketLock::take(&parent, std::ffi::OsStr::new("bolt.sock"), &socket).unwrap();
    let named = statat(&parent, ".bolt.sock.lock", AtFlags::SYMLINK_NOFOLLOW)
        .expect("the held lock file must exist at its name");
    // (The inode number may equal the removed file's: it is free again once
    // the first descriptor is closed. What matters is that the lock is on the
    // file now at the name.)
    assert!(super::same_inode(&named, &lock.identity));
    // A second start is refused while this one holds it.
    let error = super::SocketLock::take(&parent, std::ffi::OsStr::new("bolt.sock"), &socket)
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("is already in use by another kala serve"),
        "{error}"
    );
    drop(lock);
    assert!(!lock_path.exists());
}
