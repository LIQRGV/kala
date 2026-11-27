mod common;

use common::{VARROW, code, feed, kala, stderr, stdout};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const SECRET: &str = "unique-cli-test-only-password-72819";

fn user(dir: &Path, action: &str, args: &[&str], input: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["user", action])
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        feed(&mut child, input.as_bytes());
    }
    let result = child.wait_with_output().unwrap();
    assert_no_secrets(&result);
    result
}

fn assert_no_secrets(output: &Output) {
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(SECRET), "password leaked in command output");
        assert!(
            !text.contains("$argon2"),
            "password hash leaked in command output"
        );
    }
}

fn success(output: &Output) {
    assert_eq!(code(output), 0, "{}", stderr(output));
}

fn bootstrap(dir: &Path) {
    let output = user(dir, "list", &[], None);
    success(&output);
    let text = stdout(&output);
    assert!(text.contains("root@localhost"));
    assert!(text.contains("trust"));
    assert!(text.contains("admin:*"));
    assert!(dir.join("system.kala").is_file());
}

fn invalid_account_precedes_password(action: &str) {
    let dir = tempfile::tempdir().unwrap();
    let output = user(
        dir.path(),
        action,
        &["bad account name", "--password-stdin"],
        None,
    );
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("account must be USER@HOST"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("password must contain"));
    assert!(!dir.path().join("system.kala").exists());
}

#[test]
fn user_add_invalid_account_precedes_empty_password() {
    invalid_account_precedes_password("add");
}

#[test]
fn user_set_invalid_account_precedes_empty_password() {
    invalid_account_precedes_password("set");
}

#[test]
fn valid_account_with_empty_stdin_keeps_password_error() {
    for action in ["add", "set"] {
        let dir = tempfile::tempdir().unwrap();
        let output = user(
            dir.path(),
            action,
            &["nosuch@localhost", "--password-stdin"],
            None,
        );
        assert_eq!(code(&output), 1, "{}", stderr(&output));
        assert!(
            stderr(&output).contains("password must contain 1 to 4096 bytes"),
            "{}",
            stderr(&output)
        );
        assert!(!dir.path().join("system.kala").exists());
    }
}

#[test]
fn invalid_account_with_trust_keeps_syntax_error() {
    for action in ["add", "set"] {
        let dir = tempfile::tempdir().unwrap();
        let output = user(dir.path(), action, &["bad account name", "--trust"], None);
        assert_eq!(code(&output), 1, "{}", stderr(&output));
        assert_eq!(
            stderr(&output).trim(),
            "kala: error: account must be USER@HOST with a nonempty ASCII user name of at most 64 bytes"
        );
    }
}

#[test]
fn user_set_unknown_account_is_checked_after_reading_password() {
    let dir = tempfile::tempdir().unwrap();
    let output = user(
        dir.path(),
        "set",
        &["nosuch@localhost", "--password-stdin"],
        Some(&format!("{SECRET}\n")),
    );
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("account was not found"),
        "{}",
        stderr(&output)
    );
    assert!(dir.path().join("system.kala").is_file());
}

#[test]
fn user_commands_manage_password_trust_grants_and_drop() {
    let dir = tempfile::tempdir().unwrap();
    bootstrap(dir.path());
    success(&user(
        dir.path(),
        "add",
        &["alice@127.0.0.1", "--password-stdin"],
        Some(&format!("{SECRET}\n")),
    ));
    success(&user(
        dir.path(),
        "grant",
        &["alice@127.0.0.1", "read", "varrow"],
        None,
    ));
    success(&user(
        dir.path(),
        "grant",
        &["alice@127.0.0.1", "write", "varrow"],
        None,
    ));
    let output = user(dir.path(), "list", &[], None);
    success(&output);
    let text = stdout(&output);
    for expected in [
        "alice@127.0.0.1",
        "kala_password",
        "read:varrow",
        "write:varrow",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    success(&user(
        dir.path(),
        "set",
        &["alice@127.0.0.1", "--trust"],
        None,
    ));
    success(&user(
        dir.path(),
        "revoke",
        &["alice@127.0.0.1", "read", "varrow"],
        None,
    ));
    let text = stdout(&user(dir.path(), "list", &[], None));
    assert!(!text.contains("read:varrow"));
    success(&user(dir.path(), "drop", &["alice@127.0.0.1"], None));
    assert!(!stdout(&user(dir.path(), "list", &[], None)).contains("alice"));
}

#[test]
fn last_admin_guard_and_invalid_accounts_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    bootstrap(dir.path());
    for (action, args) in [
        ("drop", vec!["root@localhost"]),
        ("revoke", vec!["root@localhost", "admin", "*"]),
        ("add", vec!["bob@%", "--trust"]),
        ("add", vec!["@localhost", "--trust"]),
        ("add", vec!["bad user@localhost", "--trust"]),
        ("add", vec!["bob@example.com", "--trust"]),
    ] {
        assert_ne!(code(&user(dir.path(), action, &args, None)), 0);
    }
    success(&user(
        dir.path(),
        "add",
        &["admin2@localhost", "--trust"],
        None,
    ));
    success(&user(
        dir.path(),
        "grant",
        &["admin2@localhost", "admin", "*"],
        None,
    ));
    success(&user(
        dir.path(),
        "revoke",
        &["root@localhost", "admin", "*"],
        None,
    ));
    assert_ne!(
        code(&user(dir.path(), "drop", &["admin2@localhost"], None)),
        0
    );
    success(&user(dir.path(), "drop", &["root@localhost"], None));
}

#[test]
fn passwords_are_stdin_only_and_empty_password_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    bootstrap(dir.path());
    for input in ["", "\n", "\r\n"] {
        assert_ne!(
            code(&user(
                dir.path(),
                "add",
                &["alice@127.0.0.1", "--password-stdin"],
                Some(input)
            )),
            0
        );
    }
    let output = user(
        dir.path(),
        "add",
        &["alice@127.0.0.1", "--password", "example"],
        None,
    );
    assert_eq!(code(&output), 2);
    for password_flag in [
        format!("--password={SECRET}"),
        format!("--credentials={SECRET}"),
    ] {
        let output = user(
            dir.path(),
            "add",
            &["alice@127.0.0.1", &password_flag],
            None,
        );
        assert_eq!(code(&output), 2);
    }
    assert_ne!(
        code(&user(dir.path(), "add", &["alice@127.0.0.1"], None)),
        0
    );
}

#[test]
fn system_marker_blocks_general_writes_even_after_rename() {
    let dir = tempfile::tempdir().unwrap();
    bootstrap(dir.path());
    let original = dir.path().join("system.kala");
    let renamed = dir.path().join("renamed.kala");
    std::fs::copy(&original, &renamed).unwrap();
    for path in [&original, &renamed] {
        let before = std::fs::read(path).unwrap();
        for args in [
            vec!["transact", path.to_str().unwrap(), "-"],
            vec!["repl", path.to_str().unwrap(), "--write"],
            vec!["load", VARROW, path.to_str().unwrap(), "--force"],
        ] {
            let output = kala(args);
            assert_ne!(code(&output), 0);
            assert!(stderr(&output).contains("kala user"), "{}", stderr(&output));
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
    let reserved = dir.path().join("new");
    std::fs::create_dir(&reserved).unwrap();
    let output = kala([
        "load",
        VARROW,
        reserved.join("system.kala").to_str().unwrap(),
    ]);
    assert_ne!(code(&output), 0);
}

#[test]
fn system_history_and_every_read_surface_redact_password_hashes() {
    let dir = tempfile::tempdir().unwrap();
    bootstrap(dir.path());
    success(&user(
        dir.path(),
        "set",
        &["root@localhost", "--password-stdin"],
        Some(&format!("{SECRET}\n")),
    ));
    success(&user(
        dir.path(),
        "set",
        &["root@localhost", "--password-stdin"],
        Some("another-test-only-password\n"),
    ));
    let world = dir.path().join("system.kala");
    let w = kala_core::World::open_read_only(&world).unwrap();
    let root = w.entities_of_type("ACCOUNT").unwrap().next().unwrap();
    let root = w.format_id(root);
    let checks = dir.path().join("checks.edn");
    std::fs::write(
        &checks,
        "[[:account-hashes [:find ?p :where [?e :password_hash ?p]]]]",
    )
    .unwrap();
    let queries = [
        "[:find ?p :where [?e :password_hash ?p]]",
        "[:find ?e :where [?e :user \"root\"]]",
    ];
    for json in [false, true] {
        for (command, mut rest) in [
            ("history", vec![root.as_str(), "password_hash"]),
            ("asof", vec![root.as_str()]),
            ("log", vec![]),
            ("info", vec![]),
            ("doctor", vec!["--checks", checks.to_str().unwrap()]),
            ("explain", vec![queries[0]]),
            ("list", vec!["ACCOUNT"]),
            ("query", vec![queries[0]]),
            ("query", vec![queries[1]]),
        ] {
            if json && command != "explain" {
                rest.push("--json");
            }
            let mut args = vec![command, world.to_str().unwrap()];
            args.extend(rest);
            let output = kala(args);
            assert_no_secrets(&output);
            success(&output);
            if command == "history" {
                assert!(stdout(&output).contains("retract"));
            }
        }
    }
}

#[test]
fn system_repl_redacts_hash_values() {
    let dir = tempfile::tempdir().unwrap();
    bootstrap(dir.path());
    success(&user(
        dir.path(),
        "set",
        &["root@localhost", "--password-stdin"],
        Some(&format!("{SECRET}\n")),
    ));
    let path = dir.path().join("system.kala");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"[:find ?p :where [?e :password_hash ?p]]\n.quit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_no_secrets(&output);
    success(&output);
    assert!(stdout(&output).contains("redacted"));
}
