use kala_system::auth::Identity;

#[test]
fn permits_write_truth_table_keeps_read_and_write_independent() {
    for (grants, db, write, read) in [
        (vec!["admin:varrow"], "varrow", true, true),
        (vec!["admin:*"], "varrow", true, true),
        (vec!["write:varrow"], "varrow", true, false),
        (vec!["write:*"], "varrow", true, false),
        (vec!["read:varrow"], "varrow", false, true),
        (vec!["read:*"], "varrow", false, true),
        (vec!["write:system"], "system", false, false),
        (vec!["write:*"], "system", false, false),
        (vec!["admin:system"], "system", true, true),
        (vec!["admin:*"], "system", true, true),
        (vec!["admin:varrow"], "system", false, false),
        (vec!["write:other"], "varrow", false, false),
        (
            vec!["write", "write:", "write:varrow:extra"],
            "varrow",
            false,
            false,
        ),
        (vec![], "varrow", false, false),
    ] {
        let identity = Identity {
            principal: "author".into(),
            host: "localhost".into(),
            grants: grants.into_iter().map(str::to_owned).collect(),
        };
        assert_eq!(
            identity.permits_write(db),
            write,
            "{:?} on {db}",
            identity.grants
        );
        assert_eq!(
            identity.permits_read(db),
            read,
            "{:?} on {db}",
            identity.grants
        );
    }
}
