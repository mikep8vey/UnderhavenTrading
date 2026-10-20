use weather_bot::dashboard::{hash_password, load_admin, verify_pw, write_admin};

#[test]
fn password_hash_roundtrip() {
    let h = hash_password("correct horse battery").expect("hash");
    assert!(!h.contains("correct horse"));
    assert!(verify_pw(&h, "correct horse battery"));
    assert!(!verify_pw(&h, "wrong password here"));
    assert!(!verify_pw("not-a-hash", "anything"));
}

#[test]
fn admin_file_is_written_private_and_reloadable() {
    let path = std::env::temp_dir().join(format!("wb-admin-test-{}.json", std::process::id()));
    write_admin(&path, "boss", "a-long-test-password").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let adm = load_admin(&path).expect("load").expect("exists");
    assert_eq!(adm.user, "boss");
    assert!(verify_pw(&adm.hash, "a-long-test-password"));
    let _ = std::fs::remove_file(&path);
    assert!(load_admin(&path).expect("load").is_none());
}
