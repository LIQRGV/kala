//! Polish of slice 15b (PR #20): what the reviews found in the CLI's
//! handling of worlds that several processes write.

mod common;
use common::*;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

fn load(fixture: &Path, world: &Path, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&OsStr> = vec!["load".as_ref(), fixture.as_os_str(), world.as_os_str()];
    args.extend(extra.iter().map(|s| OsStr::new(*s)));
    kala(args)
}

fn fixture_file(dir: &Path, yaml: &str) -> std::path::PathBuf {
    let p = dir.join("f.yaml");
    fs::write(&p, yaml).unwrap();
    p
}

// ---- E: `load --force` never replaces a file whose lock it doesn't hold ----

/// An old file that doesn't open as a world (here: a Kala log whose
/// genesis is damaged, as a process may still have open and be committing
/// to) is replaced only under its lock, like one that does open.
#[test]
fn force_over_an_old_file_that_is_no_world_waits_for_its_lock() {
    for garbage in [&b"not a kala log at all"[..], &b""[..]] {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture_file(dir.path(), RESOLVED_FIXTURE);
        let path = dir.path().join("w.kala");
        fs::write(&path, garbage).unwrap();
        let lock = fs::File::open(&path).unwrap();
        lock.try_lock().expect("the lock is free");
        let o = load(&f, &path, &["--force", "--busy-timeout", "100ms"]);
        assert_eq!(code(&o), 1, "stderr: {}", stderr(&o));
        assert!(o.stdout.is_empty(), "stdout: {}", stdout(&o));
        assert!(
            stderr(&o).contains("its lock is held"),
            "stderr: {}",
            stderr(&o)
        );
        assert_eq!(fs::read(&path).unwrap(), garbage, "the file was replaced");
        assert_eq!(dir_names(dir.path()), vec!["f.yaml", "w.kala"]);
        // Free: replaced as asked.
        drop(lock);
        let o = load(&f, &path, &["--force", "--busy-timeout", "100ms"]);
        assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
        assert!(kala_core::World::open_read_only(&path).is_ok());
        assert_eq!(dir_names(dir.path()), vec!["f.yaml", "w.kala"]);
    }
}
