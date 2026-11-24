//! The `:concurrency` world setting (slice 15b): the world's concurrency
//! mode for a future pessimistic mode. Today it accepts only `optimistic`,
//! which is also what a world without the setting uses; weak mode is
//! chosen per transaction ([`Txn::set_concurrency`]), never here.

use kala_core::doctor::{DoctorOptions, doctor};
use kala_core::*;

#[test]
fn the_concurrency_setting_accepts_optimistic_only() {
    let mut w = World::in_memory(Schema::new()).unwrap();
    assert_eq!(w.world_setting("concurrency"), None);
    let mut t = w.begin();
    let err = t.world_setting("concurrency", "pessimistic").unwrap_err();
    assert!(
        matches!(&err, Error::Schema(m) if m.contains("not supported yet")),
        "{err:?}"
    );
    drop(t);
    for bad in ["weak", "serializable", "OPTIMISTIC"] {
        let mut t = w.begin();
        let err = t.world_setting("concurrency", bad).unwrap_err();
        assert!(
            matches!(&err, Error::Schema(m) if m.contains(":optimistic")),
            "{bad}: {err:?}"
        );
    }
    let mut t = w.begin();
    t.world_setting("concurrency", "optimistic").unwrap();
    // The setting never changes a transaction's own mode.
    assert_eq!(t.concurrency(), Concurrency::Optimistic);
    assert_eq!(t.commit().unwrap().declared, 1);
    assert_eq!(w.world_setting("concurrency"), Some("optimistic"));
    assert!(doctor(&w, &DoctorOptions::default()).is_clean());
    // A genesis setting is validated the same way.
    assert!(World::in_memory(Schema::new().setting("concurrency", "pessimistic")).is_err());
    assert!(World::in_memory(Schema::new().setting("concurrency", "optimistic")).is_ok());
}
