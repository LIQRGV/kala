//! TLS listener validation through the real CLI.
#![cfg(unix)]

#[path = "common/serving.rs"]
mod serving;

use rcgen::{CertificateParams, KeyPair, date_time_ymd};
use serving::{Serving, short_dir};
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

const TLS: &str = "bolt+s://127.0.0.1:0";
const FILE_FLAGS: &[&str] = &["--tls.cert", "chain.pem", "--tls.key", "key.pem"];

// Occupying an early address makes an unexpected startup return promptly,
// and distinguishes validation before binding from a later error.
fn refused(dir: &Path, listeners: &[&str], options: &[&str]) -> String {
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = dir.join("first.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_kala"));
    command
        .current_dir(dir)
        .args(["serve", ".", "--listen"])
        .arg(format!("unix://{}", socket.display()))
        .arg("--listen")
        .arg(held.local_addr().unwrap().to_string());
    for listener in listeners {
        command.args(["--listen", listener]);
    }
    let output = command.args(options).output().unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "announced a listener on validation failure"
    );
    assert!(!socket.exists(), "left a socket after validation failed");
    let error = String::from_utf8(output.stderr).unwrap();
    assert_no_key_in_stderr(dir, &error);
    error
}

fn assert_no_key_in_stderr(dir: &Path, error: &str) {
    // Keep accidentally echoed private key material out of test diagnostics too.
    if let Ok(key) = fs::read_to_string(dir.join("key.pem")) {
        for line in key.lines().filter(|s| s.len() >= 12) {
            assert!(
                !error.contains(line),
                "private key material appeared on stderr"
            );
        }
    }
}

#[test]
fn tls_listener_without_cert_flags_is_refused() {
    let dir = short_dir();
    let error = refused(dir.path(), &[TLS], &[]);
    assert!(
        error.contains("--tls.cert") && error.contains("--tls.key"),
        "{error}"
    );
    for (flag, missing) in [("--tls.cert", "--tls.key"), ("--tls.key", "--tls.cert")] {
        let error = refused(dir.path(), &[TLS], &[flag, "unused.pem"]);
        assert!(error.contains(missing), "{error}");
    }
}

#[test]
fn tls_files_require_a_tls_listener_and_ssc_is_client_only() {
    let dir = short_dir();
    for options in [FILE_FLAGS, &FILE_FLAGS[..2], &FILE_FLAGS[2..]] {
        let error = refused(dir.path(), &[], options);
        assert!(error.contains("bolt+s://"), "{error}");
    }
    let error = refused(dir.path(), &["bolt+ssc://localhost:0"], &[]);
    assert!(
        error.contains("bolt+s://") && error.contains("bolt+ssc://"),
        "{error}"
    );
}

fn pair(dir: &Path, params: &CertificateParams) {
    let key = KeyPair::generate().unwrap();
    fs::write(
        dir.join("chain.pem"),
        params.self_signed(&key).unwrap().pem(),
    )
    .unwrap();
    let path = dir.join("key.pem");
    fs::write(&path, key.serialize_pem()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn valid_params() -> CertificateParams {
    CertificateParams::new(vec!["localhost".into()]).unwrap()
}

fn assert_rejected(dir: &Path, expected: &[&str]) {
    let error = refused(dir, &[TLS], FILE_FLAGS).to_lowercase();
    for text in expected {
        assert!(error.contains(text), "missing {text:?} in {error:?}");
    }
}

#[test]
fn unreadable_or_missing_tls_files_name_the_file() {
    let dir = short_dir();
    for path in ["chain.pem", "key.pem"] {
        for unreadable in [false, true] {
            pair(dir.path(), &valid_params());
            let file = dir.path().join(path);
            fs::remove_file(&file).unwrap();
            // A directory cannot be read as PEM even when CI runs as root.
            if unreadable {
                fs::create_dir(&file).unwrap();
                fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
            }
            assert_rejected(dir.path(), &[path]);
            if unreadable {
                fs::remove_dir(file).unwrap();
            }
        }
    }
}

#[test]
fn certificate_chain_and_key_must_parse_and_match() {
    let dir = short_dir();
    let path = dir.path();
    pair(path, &valid_params());
    fs::write(path.join("chain.pem"), "").unwrap();
    assert_rejected(path, &["chain.pem", "certificate"]);
    pair(path, &valid_params());
    fs::write(
        path.join("chain.pem"),
        "-----BEGIN CERTIFICATE-----\n???\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    assert_rejected(path, &["chain.pem"]);
    pair(path, &valid_params());
    fs::write(path.join("key.pem"), "PRIVATE-TEST-MARKER-DO-NOT-ECHO").unwrap();
    assert_rejected(path, &["key.pem"]);
    pair(path, &valid_params());
    fs::write(
        path.join("key.pem"),
        KeyPair::generate().unwrap().serialize_pem(),
    )
    .unwrap();
    assert_rejected(path, &["match"]);
}

#[test]
fn a_group_or_world_readable_private_key_is_refused() {
    let dir = short_dir();
    pair(dir.path(), &valid_params());
    for mode in [0o640, 0o604, 0o644] {
        fs::set_permissions(dir.path().join("key.pem"), fs::Permissions::from_mode(mode)).unwrap();
        assert_rejected(dir.path(), &["key.pem", "permission"]);
    }
}

#[test]
fn validity_errors_name_the_offending_date() {
    let dir = short_dir();
    let mut params = valid_params();
    for (start, end, reason, date) in [
        (1998, 1999, "expired", "1999"),
        (2100, 2101, "not yet valid", "2100"),
    ] {
        params.not_before = date_time_ymd(start, 1, 1);
        params.not_after = date_time_ymd(end, 1, 1);
        pair(dir.path(), &params);
        assert_rejected(dir.path(), &[reason, date]);
    }
}

fn start(dir: &Path, listeners: &[&str]) -> Serving {
    let cert = dir.join("chain.pem");
    let key = dir.join("key.pem");
    Serving::start(
        dir,
        listeners,
        &[
            "--tls.cert",
            cert.to_str().unwrap(),
            "--tls.key",
            key.to_str().unwrap(),
        ],
    )
}

#[test]
fn non_loopback_tls_announces_the_scheme_and_warns_about_routing_and_expiry() {
    let dir = short_dir();
    let mut params = valid_params();
    params.not_after = (SystemTime::now() + Duration::from_secs(7 * 86400)).into();
    let expiry = params.not_after.date().to_string();
    pair(dir.path(), &params);
    fs::set_permissions(
        dir.path().join("key.pem"),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    let mut server = start(dir.path(), &["bolt+s://0.0.0.0:0", "127.0.0.1:0"]);
    assert!(server.addresses[0].starts_with("bolt+s://0.0.0.0:"));
    assert!(server.addresses[1].starts_with("127.0.0.1:"));
    assert!(server.signal("-INT").success());
    let errors = server.stderr();
    assert_no_key_in_stderr(dir.path(), &errors);
    assert!(
        errors.contains(&expiry) && errors.contains("expir"),
        "{errors}"
    );
    assert!(
        errors.contains("no --advertised-address on a non-loopback listener"),
        "{errors}"
    );
}

#[test]
fn tls_never_exempts_another_listener_from_authentication_or_encryption() {
    let dir = short_dir();
    pair(dir.path(), &valid_params());
    for listeners in [
        ["bolt+s://0.0.0.0:0", "0.0.0.0:0"],
        ["0.0.0.0:0", "bolt+s://0.0.0.0:0"],
    ] {
        let error = refused(dir.path(), &listeners, FILE_FLAGS);
        assert!(error.contains("--insecure-allow-plaintext"), "{error}");
    }
    let mut flags = FILE_FLAGS.to_vec();
    flags.extend(["--auth", "none", "--insecure-allow-plaintext"]);
    let error = refused(dir.path(), &["bolt+s://0.0.0.0:0"], &flags);
    assert!(error.contains("loopback"), "{error}");
}

#[test]
fn invalid_tls_files_do_not_bootstrap_the_accounts_database() {
    let dir = short_dir();
    pair(dir.path(), &valid_params());
    fs::write(dir.path().join("chain.pem"), "").unwrap();
    assert_rejected(dir.path(), &["chain.pem"]);
    assert!(
        !dir.path().join("system.kala").exists(),
        "invalid TLS configuration created system.kala"
    );
}

#[test]
fn tls_files_each_allow_one_mib_and_refuse_one_byte_more() {
    const LIMIT: usize = 1024 * 1024;
    let dir = short_dir();
    pair(dir.path(), &valid_params());
    for name in ["chain.pem", "key.pem"] {
        let path = dir.path().join(name);
        let mut pem = fs::read(&path).unwrap();
        // Trailing whitespace keeps the PEM itself valid at both sizes.
        pem.resize(LIMIT, b' ');
        fs::write(path, pem).unwrap();
    }
    let mut server = start(dir.path(), &[TLS]);
    assert!(server.signal("-TERM").success());
    for name in ["chain.pem", "key.pem"] {
        let path = dir.path().join(name);
        let mut pem = fs::read(&path).unwrap();
        pem.push(b' ');
        fs::write(&path, &pem).unwrap();
        let error = refused(dir.path(), &[TLS], FILE_FLAGS);
        assert!(
            error.contains(name) && (error.contains("1 MiB") || error.contains("1048576")),
            "{name} over 1 MiB must report the file size limit"
        );
        fs::write(path, &pem[..LIMIT]).unwrap();
    }
}
