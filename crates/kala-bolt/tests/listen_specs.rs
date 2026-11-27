//! Listen spellings are DNS-free and never silently select another transport.
use kala_bolt::transport::ListenAddr;
use proptest::prelude::*;

#[test]
fn accepted_tcp_listen_specs_are_canonical() {
    for (input, expected) in [
        ("127.0.0.1:0", "127.0.0.1:0"),
        ("localhost:7687", "127.0.0.1:7687"),
        ("[::1]:7687", "[::1]:7687"),
        ("bolt://127.0.0.1:7687", "127.0.0.1:7687"),
        ("bolt://localhost:0", "127.0.0.1:0"),
        ("bolt://[::1]:0", "[::1]:0"),
        ("bolt+s://localhost:0", "bolt+s://127.0.0.1:0"),
        ("bolt+s://[::1]:7687", "bolt+s://[::1]:7687"),
    ] {
        let parsed: ListenAddr = input.parse().unwrap();
        assert_eq!(parsed.to_string(), expected, "{input}");
    }
}

#[test]
fn rejected_listen_schemes_and_hostnames_explain_the_problem() {
    for scheme in ["ws", "wss", "invented"] {
        let error = format!("{scheme}://127.0.0.1:7687")
            .parse::<ListenAddr>()
            .unwrap_err()
            .to_string();
        assert!(error.contains(scheme), "{error}");
        assert!(error.contains("not supported yet"), "{error}");
    }
    let error = "bolt+ssc://localhost:0"
        .parse::<ListenAddr>()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("bolt+ssc://") && error.contains("bolt+s://"),
        "{error}"
    );
    let error = "neo4j://localhost:7687"
        .parse::<ListenAddr>()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("neo4j") && error.contains("not a listen address"),
        "{error}"
    );
    for input in [
        "example.org:7687",
        "bolt://example.org:7687",
        "bolt+s://example.org:7687",
        "localhost",
        "127.0.0.1",
        "localhost:65536",
    ] {
        assert!(input.parse::<ListenAddr>().is_err(), "{input}");
    }
}

#[test]
#[cfg(unix)]
fn unix_listen_specs_require_absolute_bounded_paths() {
    let parsed: ListenAddr = "unix:///tmp/kala-test.sock".parse().unwrap();
    assert_eq!(parsed.to_string(), "unix:///tmp/kala-test.sock");
    for input in ["unix://", "unix://relative.sock"] {
        let error = input.parse::<ListenAddr>().unwrap_err().to_string();
        assert!(error.contains("absolute"), "{error}");
    }
    let input = format!("unix:///{}", "x".repeat(200));
    let error = input.parse::<ListenAddr>().unwrap_err().to_string();
    assert!(error.contains("limit"), "{error}");
    assert!(
        error.chars().any(|c| c.is_ascii_digit()),
        "the path limit must be named: {error}"
    );
    assert!("unix:///tmp/a\0b".parse::<ListenAddr>().is_err());
}

#[test]
#[cfg(not(unix))]
fn unix_listen_specs_explain_the_platform_limit() {
    let error = "unix:///tmp/kala.sock"
        .parse::<ListenAddr>()
        .unwrap_err()
        .to_string();
    assert!(error.contains("not supported on this platform"), "{error}");
}

proptest! {
    #[test]
    fn listen_spec_parser_never_panics(input in any::<String>()) {
        let _ = input.parse::<ListenAddr>();
    }
}
