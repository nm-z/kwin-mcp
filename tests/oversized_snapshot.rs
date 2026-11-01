#![allow(clippy::expect_used)]

//! A database over the copy limit is reported as exactly that, immediately,
//! so `session_start` can keep it live read-only instead of failing.

use kwin_mcp::browser_cookies::{SnapshotTooLarge, snapshot_host};
use std::time::{Duration, Instant};

#[test]
fn an_oversized_database_is_reported_as_too_large_without_retries() {
    let directory = std::env::temp_dir().join(format!("kwin-mcp-oversized-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create fixture directory");
    let database = directory.join("big.db");
    // A sparse file: 65 MiB of length costs no disk, and is one MiB over the limit.
    std::fs::File::create(&database)
        .and_then(|file| file.set_len(65 * 1024 * 1024))
        .expect("create sparse database");

    let began = Instant::now();
    let error = snapshot_host(&database, &directory.join("copy.db"))
        .expect_err("a 65 MiB database cannot be snapshotted");
    let elapsed = began.elapsed();
    let _ = std::fs::remove_dir_all(&directory);

    assert!(
        error.is::<SnapshotTooLarge>(),
        "the failure was not reported as too large: {error:#}"
    );
    let text = format!("{error:#}");
    assert!(text.contains("64 MiB"), "{text}");
    assert!(
        !text.contains("cookie"),
        "the message wrongly blames a cookie database: {text}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "an oversized database was retried: {elapsed:?}"
    );
}
