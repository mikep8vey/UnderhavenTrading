use std::time::{SystemTime, UNIX_EPOCH};
use weather_bot::storage::sqlite::Storage;

#[test]
fn signal_exec_is_upserted_per_account_and_ignores_zero() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("wb-exec-{}-{unique}.db", std::process::id()));
    let s = Storage::open(&path).expect("open");

    // signal_id 0 means "not persisted" (e.g. the read-only signals CLI); it must be a no-op.
    s.set_signal_exec(0, "paper", "ORDERED", "ignored").expect("zero id");
    assert!(s.query_json("SELECT * FROM signal_exec").expect("query").is_empty());

    s.set_signal_exec(7, "paper", "ORDERED", "working").expect("paper ordered");
    s.set_signal_exec(7, "paper", "FILLED", "10 @ 0.4").expect("paper filled");
    s.set_signal_exec(7, "live", "SKIPPED_DEPTH", "thin book").expect("live skipped");

    let rows = s
        .query_json("SELECT account,status,detail FROM signal_exec WHERE signal_id=7 ORDER BY account")
        .expect("query");
    assert_eq!(rows.len(), 2, "one row per account, latest status wins");
    assert_eq!(rows[0]["account"].as_str(), Some("live"));
    assert_eq!(rows[0]["status"].as_str(), Some("SKIPPED_DEPTH"));
    assert_eq!(rows[1]["account"].as_str(), Some("paper"));
    assert_eq!(rows[1]["status"].as_str(), Some("FILLED"));
    assert_eq!(rows[1]["detail"].as_str(), Some("10 @ 0.4"));

    drop(s);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
