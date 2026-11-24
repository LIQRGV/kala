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
