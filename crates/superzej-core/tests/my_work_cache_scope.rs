//! Integration coverage for the per-scope `my_work_cache` (v28) — kept out of the
//! god-file `db.rs` per the file-size ratchet.

use superzej_core::db::Db;
use superzej_core::work::ALL_SCOPE;

#[test]
fn my_work_cache_roundtrips_per_scope() {
    let db = Db::open_memory().unwrap();
    assert!(db.get_my_work_cache("/repo/a").unwrap().is_none());
    db.put_my_work_cache("/repo/a", r#"[{"n":1}]"#).unwrap();
    let (json, fetched) = db.get_my_work_cache("/repo/a").unwrap().unwrap();
    assert_eq!(json, r#"[{"n":1}]"#);
    assert!(fetched > 0);
    // A second put to the same scope replaces (not appends).
    db.put_my_work_cache("/repo/a", r#"[{"n":2}]"#).unwrap();
    assert_eq!(
        db.get_my_work_cache("/repo/a").unwrap().unwrap().0,
        r#"[{"n":2}]"#
    );
    // A different scope is stored independently — no cross-repo bleed.
    db.put_my_work_cache(ALL_SCOPE, r#"[{"n":9}]"#).unwrap();
    assert_eq!(
        db.get_my_work_cache(ALL_SCOPE).unwrap().unwrap().0,
        r#"[{"n":9}]"#
    );
    assert_eq!(
        db.get_my_work_cache("/repo/a").unwrap().unwrap().0,
        r#"[{"n":2}]"#
    );
}
