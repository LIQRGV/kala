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
