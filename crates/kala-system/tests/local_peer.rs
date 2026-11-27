//! Local transport identity is explicit and cannot alias an IP host selector.
use kala_system::auth::{AccountVerifier, AuthToken, Credentials, HostPattern, Peer};
use std::sync::Arc;

#[test]
fn local_peer_matches_only_the_localhost_host_pattern() {
    for (pattern, matches_local) in [
        ("localhost", true),
        ("%", false),
        ("127.0.0.1", false),
        ("::1", false),
        ("::ffff:127.0.0.1", false),
        ("127.0.0.0/8", false),
        ("0.0.0.0/0", false),
        ("::/0", false),
        ("127.%", false),
        ("192.168.1.%", false),
    ] {
        assert_eq!(
            HostPattern::parse(pattern)
                .unwrap()
                .matches_peer(Peer::Local),
            matches_local,
            "{pattern}"
        );
    }
    assert!(Peer::Local.is_loopback());
    assert_eq!(Peer::Local.to_string(), "localhost");
}

#[test]
fn explicit_ip_peers_preserve_host_matching_and_loopback_rules() {
    for (pattern, ip, expected) in [
        ("localhost", "127.0.0.1", true),
        ("localhost", "::1", true),
        ("localhost", "::ffff:127.0.0.1", true),
        ("localhost", "127.0.0.2", false),
        ("localhost", "192.168.1.1", false),
        ("%", "192.168.1.1", true),
        ("192.168.1.%", "192.168.1.9", true),
        ("192.168.1.%", "127.0.0.1", false),
        ("10.0.0.0/8", "10.1.2.3", true),
        ("fd00::/8", "fd12::7", true),
        ("127.0.0.1", "::ffff:127.0.0.1", true),
    ] {
        let host = HostPattern::parse(pattern).unwrap();
        let ip = ip.parse().unwrap();
        assert_eq!(
            host.matches_peer(Peer::Ip(ip)),
            expected,
            "{pattern} and {ip}"
        );
        assert_eq!(host.matches(ip), expected, "legacy IP matching changed");
    }
    for ip in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
        assert!(Peer::Ip(ip.parse().unwrap()).is_loopback());
    }
    for ip in ["192.168.1.9", "fd00::1", "::ffff:10.0.0.1"] {
        assert!(!Peer::Ip(ip.parse().unwrap()).is_loopback());
    }
}

#[test]
fn local_authentication_does_not_select_a_more_specific_ip_account() {
    let dir = tempfile::tempdir().unwrap();
    kala_system::system::add(dir.path(), "root@127.0.0.1", None).unwrap();
    let world =
        Arc::new(kala_core::World::open_read_only(&dir.path().join("system.kala")).unwrap());
    let verifier = AccountVerifier::new().unwrap();
    let token = AuthToken {
        scheme: "none".into(),
        principal: String::new(),
        credentials: Credentials::new(String::new()),
    };
    let local = verifier
        .authenticate_peer(&token, Peer::Local, || Ok(world.clone()))
        .unwrap();
    assert_eq!(local.principal, "root");
    assert_eq!(local.host, "localhost");
    for ip in ["127.0.0.1", "::ffff:127.0.0.1"] {
        let tcp = verifier
            .authenticate_peer(&token, Peer::Ip(ip.parse().unwrap()), || Ok(world.clone()))
            .unwrap();
        assert_eq!(tcp.host, "127.0.0.1");
    }
    assert!(
        verifier
            .authenticate_peer(&token, Peer::Ip("192.168.1.1".parse().unwrap()), || Ok(
                world.clone()
            ))
            .is_err()
    );
}
