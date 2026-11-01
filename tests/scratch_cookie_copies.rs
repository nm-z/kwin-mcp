#![allow(clippy::expect_used)]

//! The session's private cookie copies are rebuilt at every start, so they are
//! written without fsync: the scratch variants must give the same rows as the
//! durable ones, and must leave the database in a state SQLite accepts.

use kwin_mcp::browser_cookies::{merge, merge_scratch, snapshot, snapshot_scratch};
use rusqlite::Connection;
use std::path::Path;

const SCHEMA: &str = "CREATE TABLE cookies (host_key TEXT NOT NULL, name TEXT NOT NULL, \
    path TEXT NOT NULL, value TEXT, last_update_utc INTEGER NOT NULL, \
    expires_utc INTEGER NOT NULL DEFAULT 0, has_expires INTEGER NOT NULL DEFAULT 0, \
    UNIQUE (host_key, name, path));";

fn jar(path: &Path, rows: &[(&str, &str, i64)]) {
    let connection = Connection::open(path).expect("open jar");
    connection.execute_batch(SCHEMA).expect("create schema");
    for (name, value, updated) in rows {
        connection
            .execute(
                "INSERT INTO cookies (host_key, name, path, value, last_update_utc) \
                 VALUES ('example.test', ?1, '/', ?2, ?3)",
                (name, value, updated),
            )
            .expect("insert cookie");
    }
}

fn rows(path: &Path) -> Vec<(String, String)> {
    let connection = Connection::open(path).expect("open copy");
    let check: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(check, "ok", "{}", path.display());
    let mut statement = connection
        .prepare("SELECT name, value FROM cookies ORDER BY name")
        .expect("prepare");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

#[test]
fn scratch_copies_hold_the_same_rows_as_durable_ones() {
    let directory = std::env::temp_dir().join(format!("kwin-mcp-scratch-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create fixture directory");
    let host = directory.join("host.db");
    let saved = directory.join("saved.db");
    jar(&host, &[("a", "host-a", 10), ("b", "host-b", 30)]);
    jar(&saved, &[("b", "saved-b", 20), ("c", "saved-c", 40)]);

    let durable = directory.join("durable.db");
    let scratch = directory.join("scratch.db");
    snapshot(&host, &durable).expect("durable snapshot");
    snapshot_scratch(&host, &scratch).expect("scratch snapshot");
    merge(&durable, &saved).expect("durable merge");
    merge_scratch(&scratch, &saved).expect("scratch merge");

    let expected = vec![
        ("a".to_owned(), "host-a".to_owned()),
        ("b".to_owned(), "host-b".to_owned()),
        ("c".to_owned(), "saved-c".to_owned()),
    ];
    assert_eq!(rows(&durable), expected);
    assert_eq!(rows(&scratch), expected);
    let _ = std::fs::remove_dir_all(&directory);
}
