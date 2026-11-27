use kala_system::auth::{AccountName, HostPattern, Identity};
use proptest::prelude::*;
use std::net::{IpAddr, Ipv4Addr};

#[test]
fn host_patterns_parse_and_match_without_dns() {
    for (pattern, yes, no) in [
        ("10.0.0.0/8", "10.1.2.3", "11.0.0.1"),
        ("192.168.1.%", "192.168.1.9", "192.168.2.9"),
        ("10.%", "10.2.3.4", "11.2.3.4"),
        ("10.%.%.%", "10.2.3.4", "11.2.3.4"),
        ("fd00::/8", "fd12::7", "fe80::1"),
        ("::1", "::1", "::2"),
        ("::/0", "fd12::7", "10.0.0.1"),
        ("10.1.2.3/32", "10.1.2.3", "10.1.2.4"),
        ("127.0.0.1", "::ffff:127.0.0.1", "127.0.0.2"),
        ("localhost", "::1", "127.0.0.2"),
    ] {
        let host = HostPattern::parse(pattern).unwrap();
        assert!(
            host.matches(yes.parse().unwrap()),
            "{pattern} must match {yes}"
        );
        assert!(
            !host.matches(no.parse().unwrap()),
            "{pattern} must not match {no}"
        );
    }
    let any = HostPattern::parse("%").unwrap();
    for peer in ["1.2.3.4", "2001:db8::7", "::ffff:192.168.1.9"] {
        assert!(any.matches(peer.parse().unwrap()));
    }
    for invalid in [
        "",
        "example.com",
        "192.%.1.%",
        "192.16%.%",
        "10.0.0.0/33",
        "fd00::/129",
        "256.%",
        "10.1.2.3.%",
        "10..%",
        "localhost.",
    ] {
        assert!(HostPattern::parse(invalid).is_err(), "accepted {invalid:?}");
    }
    for invalid in [
        "@localhost",
        "bad name@localhost",
        "root",
        "root@",
        "a@@localhost",
    ] {
        assert!(AccountName::parse(invalid).is_err());
    }
    assert!(AccountName::parse(&format!("{}@localhost", "x".repeat(65))).is_err());
    assert!(AccountName::parse(&format!("{}@localhost", "x".repeat(64))).is_ok());
}

#[test]
fn specificity_orders_every_pattern_kind() {
    let ordered = [
        "%",
        "10.%",
        "10.1.%",
        "0.0.0.0/0",
        "10.0.0.0/8",
        "10.1.0.0/16",
        "localhost",
        "127.0.0.1",
    ];
    for pair in ordered.windows(2) {
        assert!(
            HostPattern::parse(pair[0]).unwrap().specificity()
                < HostPattern::parse(pair[1]).unwrap().specificity(),
            "{pair:?}"
        );
    }
}

#[test]
fn write_only_is_not_read_and_system_always_needs_admin() {
    for (grants, db, allowed) in [
        (vec!["write:varrow"], "varrow", false),
        (vec!["read:varrow"], "varrow", true),
        (vec!["read:*"], "varrow", true),
        (vec!["admin:varrow"], "varrow", true),
        (vec!["read:*"], "system", false),
        (vec!["read:system"], "system", false),
        (vec!["admin:system"], "system", true),
        (vec!["admin:*"], "system", true),
        (vec!["admin:varrow"], "system", false),
    ] {
        let identity = Identity {
            principal: "alice".into(),
            host: "localhost".into(),
            grants: grants.into_iter().map(str::to_owned).collect(),
            generation: 0,
        };
        assert_eq!(
            identity.permits_read(db),
            allowed,
            "{:?} on {db}",
            identity.grants
        );
    }
}

proptest! {
    #[test]
    fn cidr_matches_naive_bit_check(network in any::<u32>(), peer in any::<u32>(), prefix in 0u8..=32) {
        let pattern = HostPattern::parse(&format!("{}/{prefix}",Ipv4Addr::from(network))).unwrap();
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        prop_assert_eq!(pattern.matches(IpAddr::V4(Ipv4Addr::from(peer))),network & mask == peer & mask);
    }
}

#[test]
fn a_dropped_and_recreated_account_is_not_the_account_an_identity_was_issued_for() {
    use kala_system::{auth::current_identity, system};
    let dir = tempfile::tempdir().unwrap();
    system::ensure(dir.path()).unwrap();
    let open = || kala_core::World::open_read_only(&dir.path().join("system.kala")).unwrap();
    system::add(dir.path(), "alice@%", Some("old password")).unwrap();
    system::grant(dir.path(), "alice@%", "read", "varrow").unwrap();
    let issued = current_identity(
        &open(),
        &Identity {
            principal: "alice".into(),
            host: "%".into(),
            grants: Vec::new(),
            generation: account_generation(&open(), "alice@%"),
        },
    )
    .unwrap()
    .expect("the account exists");
    assert_eq!(issued.grants, ["read:varrow"]);
    assert_ne!(issued.generation, 0);
    // A grant or password change on the SAME account keeps the identity
    // current (its grants are re-read).
    system::grant(dir.path(), "alice@%", "write", "varrow").unwrap();
    system::set(dir.path(), "alice@%", Some("changed password")).unwrap();
    let current = current_identity(&open(), &issued).unwrap().unwrap();
    assert_eq!(current.generation, issued.generation);
    assert_eq!(current.grants, ["read:varrow", "write:varrow"]);
    // Dropped: gone.
    system::drop_account(dir.path(), "alice@%").unwrap();
    assert_eq!(current_identity(&open(), &issued).unwrap(), None);
    // Re-created under the same name: another account. The old identity
    // gets none of its grants.
    system::add(dir.path(), "alice@%", Some("new password")).unwrap();
    system::grant(dir.path(), "alice@%", "admin", "*").unwrap();
    assert_eq!(current_identity(&open(), &issued).unwrap(), None);
    assert_eq!(current_identity(&open(), &current).unwrap(), None);
    let recreated = account_generation(&open(), "alice@%");
    assert!(recreated > issued.generation);
    // Other accounts are untouched by all of this.
    let root = account_generation(&open(), "root@localhost");
    assert_ne!(root, recreated);
}

/// The generation a login as `account` gets now.
fn account_generation(world: &kala_core::World, account: &str) -> i64 {
    let (user, host) = account.split_once('@').unwrap();
    // An identity with the wrong generation is "not this account"; probe
    // for the right one through the public API only.
    let probe = |generation| Identity {
        principal: user.into(),
        host: host.into(),
        grants: Vec::new(),
        generation,
    };
    (0..=world.latest_tick().0)
        .find(|g| {
            kala_system::auth::current_identity(world, &probe(*g))
                .unwrap()
                .is_some()
        })
        .expect("the account exists")
}
