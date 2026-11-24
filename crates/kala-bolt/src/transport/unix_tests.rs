use super::trusted_parent;

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
