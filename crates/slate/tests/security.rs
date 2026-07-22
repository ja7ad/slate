use slate::{Db, KeySource, Options, Profile};
use slate_core::epoch::SecurityMode;

#[test]
fn test_security_mode() {
    let path = std::path::Path::new("./test_db_sec");
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();

    let opts = Options {
        capacity: 4096 * 1024,
        b_commit: 9,
        auto_b: false,
        n_keys: 100,
        profile: Profile::Pi,
    };

    let db = Db::open(path, KeySource::Bytes([0x42; 32]), opts).unwrap();

    // FileCounter must be BestEffortRollback
    assert!(matches!(
        db.security_mode(),
        SecurityMode::BestEffortRollback
    ));
}
