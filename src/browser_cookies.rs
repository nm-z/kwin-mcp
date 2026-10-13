//! Cookie continuity for isolated Chromium profiles. Host profiles stay read-only.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const HOST_COPY_MAX_BYTES: u64 = 64 * 1024 * 1024;
const HOST_COPY_ATTEMPTS: usize = 5;
const HOST_COPY_RETRY_DELAY: Duration = Duration::from_millis(20);

#[derive(Debug, PartialEq, Eq)]
struct FileVersion {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn file_version(path: &Path) -> Result<Option<FileVersion>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(FileVersion {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CookieRoot {
    pub upper: PathBuf,
    pub saved: PathBuf,
    #[serde(default)]
    pub baseline: Option<PathBuf>,
}

struct CookieJar {
    upper: PathBuf,
    saved: PathBuf,
    baseline: Option<PathBuf>,
}

// Chromium stores microseconds since the Windows epoch (1601-01-01).
const WINDOWS_EPOCH_MICROSECONDS: i64 = 11_644_473_600_000_000;

fn cookie_time() -> Result<i64> {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok(i64::try_from(elapsed.as_micros())? + WINDOWS_EPOCH_MICROSECONDS)
}

fn columns(connection: &Connection) -> Result<Vec<String>> {
    Ok(connection
        .prepare("PRAGMA table_info(cookies)")?
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?)
}

fn identity_columns(connection: &Connection) -> Result<Vec<String>> {
    let indexes = connection
        .prepare("PRAGMA index_list(cookies)")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (index, unique, partial) in indexes {
        if !unique || partial {
            continue;
        }
        let keys = connection
            .prepare(&format!("PRAGMA index_info({})", quote(&index)))?
            .query_map([], |row| row.get::<_, Option<String>>(2))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Option<Vec<_>>>();
        if let Some(keys) = keys
            && ["host_key", "name", "path"]
                .iter()
                .all(|key| keys.iter().any(|name| name == key))
        {
            return Ok(keys);
        }
    }
    anyhow::bail!("cookie identity unique index is missing")
}

/// Other SQLite files named Cookies inside a browser config are kept private,
/// but are not cookie jars and must not enter the continuity store.
pub fn supports_cookie_jar(database: &Path) -> Result<bool> {
    let database = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let names = columns(&database)?;
    Ok(["host_key", "name", "path", "last_update_utc"]
        .iter()
        .all(|name| names.iter().any(|column| column == name))
        && identity_columns(&database).is_ok())
}

fn prune_expired(connection: &Connection) -> Result<()> {
    let names = columns(connection)?;
    if names.iter().any(|name| name == "expires_utc") {
        let predicate = if names.iter().any(|name| name == "has_expires") {
            "has_expires != 0 AND expires_utc <= ?"
        } else {
            "expires_utc > 0 AND expires_utc <= ?"
        };
        connection.execute(
            &format!("DELETE FROM cookies WHERE {predicate}"),
            [cookie_time()?],
        )?;
    }
    Ok(())
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
enum CookieKey {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<Value> for CookieKey {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer(value),
            Value::Real(value) => Self::Real(value),
            Value::Text(value) => Self::Text(value),
            Value::Blob(value) => Self::Blob(value),
        }
    }
}

impl From<&CookieKey> for Value {
    fn from(value: &CookieKey) -> Self {
        match value {
            CookieKey::Null => Self::Null,
            CookieKey::Integer(value) => Self::Integer(*value),
            CookieKey::Real(value) => Self::Real(*value),
            CookieKey::Text(value) => Self::Text(value.clone()),
            CookieKey::Blob(value) => Self::Blob(value.clone()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct DeletedCookie {
    key: Vec<CookieKey>,
    through: i64,
}

#[derive(Serialize, Deserialize)]
struct DeletedCookies {
    columns: Vec<String>,
    rows: Vec<DeletedCookie>,
}

fn read_deletions(saved: &Path, keys: Vec<String>) -> Result<DeletedCookies> {
    match std::fs::read(saved.with_extension("deleted.json")) {
        Ok(bytes) => {
            let deleted: DeletedCookies = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                deleted.columns == keys,
                "deleted cookie identity schema differs"
            );
            Ok(deleted)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DeletedCookies {
            columns: keys,
            rows: Vec::new(),
        }),
        Err(error) => Err(error.into()),
    }
}

fn apply_deleted(connection: &Connection, deleted: &DeletedCookies) -> Result<()> {
    let predicate = deleted
        .columns
        .iter()
        .map(|key| format!("{} IS ?", quote(key)))
        .collect::<Vec<_>>()
        .join(" AND ");
    for row in &deleted.rows {
        let mut identity = row.key.iter().map(Value::from).collect::<Vec<_>>();
        identity.push(Value::Integer(row.through));
        connection.execute(
            &format!("DELETE FROM cookies WHERE {predicate} AND last_update_utc <= ?"),
            params_from_iter(identity),
        )?;
    }
    prune_expired(connection)
}

/// Tombstones remove the baseline version that a session deleted, including
/// old host rows and stale concurrent sessions. A newer last_update_utc wins
/// independently of session stop order; absence cannot date an unseen update.
pub fn apply_deletions(database: &Path, saved: &Path) -> Result<()> {
    let database = Connection::open(database)?;
    let deleted = read_deletions(saved, identity_columns(&database)?)?;
    apply_deleted(&database, &deleted)
}

fn record_deletions(
    database: &Path,
    baseline: Option<&Path>,
    saved: &Path,
) -> Result<DeletedCookies> {
    let current = Connection::open(database)?;
    let keys = identity_columns(&current)?;
    let mut deleted = read_deletions(saved, keys.clone())?;
    if let Some(baseline) = baseline.filter(|path| path.is_file()) {
        let baseline = Connection::open_with_flags(baseline, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        anyhow::ensure!(
            identity_columns(&baseline)? == keys,
            "baseline cookie identity schema differs"
        );
        let fields = keys
            .iter()
            .map(|key| quote(key))
            .collect::<Vec<_>>()
            .join(",");
        let predicate = keys
            .iter()
            .map(|key| format!("{} IS ?", quote(key)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut query =
            baseline.prepare(&format!("SELECT {fields},last_update_utc FROM cookies"))?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let values = (0..keys.len())
                .map(|index| row.get::<_, Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let exists: bool = current.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM cookies WHERE {predicate})"),
                params_from_iter(&values),
                |row| row.get(0),
            )?;
            if !exists {
                let key = values.into_iter().map(CookieKey::from).collect::<Vec<_>>();
                let through = row.get(keys.len())?;
                if let Some(existing) = deleted.rows.iter_mut().find(|row| row.key == key) {
                    existing.through = existing.through.max(through);
                } else {
                    deleted.rows.push(DeletedCookie { key, through });
                }
            }
        }
    }
    Ok(deleted)
}

fn write_deletions(saved: &Path, deleted: &DeletedCookies) -> Result<()> {
    use std::io::Write;
    let destination = saved.with_extension("deleted.json");
    let temporary = saved.with_extension("deleted.pending");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(deleted)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, destination)?;
    std::fs::File::open(saved.parent().context("cookie state has no parent")?)?.sync_all()?;
    Ok(())
}

pub fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("kwin-mcp/browser-cookies")
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Copy through SQLite so committed WAL changes are included.
pub fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        std::fs::metadata(source)?.len() <= 512 * 1024 * 1024,
        "cookie database exceeds the 512 MiB snapshot limit"
    );
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::ZERO)?;
    let mut copy = Connection::open(destination)?;
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o600))?;
    let step = rusqlite::backup::Backup::new(&source, &mut copy)?.step(-1)?;
    anyhow::ensure!(
        step == rusqlite::backup::StepResult::Done,
        "cookie database is locked ({step:?})"
    );
    copy.execute_batch("PRAGMA journal_mode=DELETE;")?;
    drop(copy);
    // The session's lower layer may still have live host sidecars. Shadow them
    // so opening the merged jar cannot replay a different host WAL over it.
    clear_sidecars(destination)?;
    Ok(())
}

fn clear_sidecars(database: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let mut path = database.as_os_str().to_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        std::fs::write(&path, [])?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Chrome keeps an EXCLUSIVE SQLite lock even between commits. Read only its
/// files, verify unchanged file identities/timestamps around two identical
/// observations, then let SQLite read the frozen
/// private copy, including committed WAL frames. Never open host shared memory.
pub fn snapshot_host(source: &Path, destination: &Path) -> Result<()> {
    use std::io::Read;
    let sidecar = |suffix: &str| {
        let mut path = source.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    };
    let active_journal = || -> Result<bool> {
        match std::fs::File::open(sidecar("-journal")) {
            Ok(file) => {
                let mut header = Vec::new();
                file.take(8).read_to_end(&mut header)?;
                Ok(header.iter().any(|byte| *byte != 0))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    };
    let versions = || -> Result<_> {
        ["", "-wal", "-journal"]
            .map(|suffix| file_version(&sidecar(suffix)))
            .into_iter()
            .collect::<Result<Vec<_>>>()
    };
    let read = || -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let mut total = 0;
        let mut contents = Vec::new();
        for suffix in ["", "-wal"] {
            let path = sidecar(suffix);
            match std::fs::File::open(path) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(HOST_COPY_MAX_BYTES + 1).read_to_end(&mut bytes)?;
                    total += u64::try_from(bytes.len())?;
                    anyhow::ensure!(
                        total <= HOST_COPY_MAX_BYTES,
                        "host cookie database and WAL exceed the 64 MiB copy limit"
                    );
                    contents.push(Some(bytes));
                }
                Err(error) if suffix == "-wal" && error.kind() == std::io::ErrorKind::NotFound => {
                    contents.push(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let wal = contents.pop().flatten();
        let database = contents
            .pop()
            .flatten()
            .context("host cookie database is missing")?;
        Ok((database, wal))
    };
    let mut last_error = None;
    for _ in 0..HOST_COPY_ATTEMPTS {
        let attempt = (|| -> Result<()> {
            let before = versions()?;
            // A rollback journal may contain the only committed version of a page.
            // Wait for that transaction to finish instead of copying an active one.
            anyhow::ensure!(
                !active_journal()?,
                "host cookie rollback transaction is active"
            );
            let first = read()?;
            let second = read()?;
            anyhow::ensure!(
                first == second && before == versions()? && !active_journal()?,
                "host cookie database or WAL changed during capture"
            );
            drop(second);
            let parent = destination.parent().context("cookie copy has no parent")?;
            let directory = parent.join(".kwin-mcp-cookie-copy");
            std::fs::create_dir(&directory)?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            let copy = directory.join("Cookies");
            let candidate = directory.join("candidate");
            let result = (|| -> Result<()> {
                std::fs::write(&copy, &first.0)?;
                if let Some(wal) = &first.1 {
                    std::fs::write(directory.join("Cookies-wal"), wal)?;
                }
                snapshot(&copy, &candidate)?;
                let database = Connection::open(&candidate)?;
                let check: String =
                    database.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
                anyhow::ensure!(
                    check == "ok",
                    "host cookie snapshot failed integrity checking"
                );
                drop(database);
                std::fs::rename(&candidate, destination)?;
                clear_sidecars(destination)?;
                Ok(())
            })();
            let _ = std::fs::remove_dir_all(directory);
            result
        })();
        match attempt {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(HOST_COPY_RETRY_DELAY);
    }
    Err(last_error.context("no host cookie snapshot attempts")?)
        .context("host cookie snapshot failed during all attempts")
}

/// Keep complete rows, including encrypted values and partition metadata.
/// A schema mismatch preserves the destination rather than guessing a migration.
pub fn merge(destination: &Path, source: &Path) -> Result<()> {
    if !source.is_file() {
        return prune_expired(&Connection::open(destination)?);
    }
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::ZERO)?;
    let mut destination = Connection::open(destination)?;
    destination.busy_timeout(Duration::ZERO)?;
    let names = columns(&destination)?;
    anyhow::ensure!(names == columns(&source)?, "cookie schemas differ");
    let keys = identity_columns(&destination)?;
    let indices = keys
        .iter()
        .map(|key| {
            names
                .iter()
                .position(|name| name == key)
                .context("cookie identity column is missing")
        })
        .collect::<Result<Vec<_>>>()?;
    let updated = names
        .iter()
        .position(|name| name == "last_update_utc")
        .context("cookie update timestamp is missing")?;
    let fields = names
        .iter()
        .map(|name| quote(name))
        .collect::<Vec<_>>()
        .join(",");
    let predicate = keys
        .iter()
        .map(|key| format!("{} IS ?", quote(key)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let placeholders = vec!["?"; names.len()].join(",");
    let transaction = destination.transaction()?;
    let mut rows = source.prepare(&format!("SELECT {fields} FROM cookies"))?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..names.len())
            .map(|index| row.get::<_, Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut identity = indices
            .iter()
            .map(|index| values[*index].clone())
            .collect::<Vec<_>>();
        identity.push(values[updated].clone());
        let newer: bool = transaction.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM cookies WHERE {predicate} AND last_update_utc >= ?)"
            ),
            params_from_iter(&identity),
            |row| row.get(0),
        )?;
        if !newer {
            transaction.execute(
                &format!("DELETE FROM cookies WHERE {predicate}"),
                params_from_iter(identity.iter().take(indices.len())),
            )?;
            transaction.execute(
                &format!("INSERT INTO cookies ({fields}) VALUES ({placeholders})"),
                params_from_iter(&values),
            )?;
        }
    }
    transaction.commit()?;
    prune_expired(&destination)
}

pub fn persist(roots: &[CookieRoot]) -> Result<()> {
    let mut jars = Vec::new();
    for root in roots {
        let mut directories = vec![(root.upper.clone(), 0)];
        while let Some((directory, depth)) = directories.pop() {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let kind = entry.file_type()?;
                if kind.is_dir() && depth < 2 {
                    directories.push((entry.path(), depth + 1));
                } else if kind.is_file() && entry.file_name() == "Cookies" {
                    let upper = entry.path();
                    let saved = root.saved.join(upper.strip_prefix(&root.upper)?);
                    let relative = upper.strip_prefix(&root.upper)?;
                    let baseline = root
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.join(relative));
                    jars.push(CookieJar {
                        upper,
                        saved,
                        baseline,
                    });
                }
            }
        }
    }
    if jars.is_empty() {
        return Ok(());
    }
    let root = state_root();
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("lock"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("another session is saving browser cookies"),
        }
    }
    let mut failures = Vec::new();
    for jar in jars.iter().filter(|jar| jar.upper.is_file()) {
        let temporary = jar.saved.with_extension("snapshot");
        let result = (|| -> Result<()> {
            if !supports_cookie_jar(&jar.upper)? {
                eprintln!(
                    "teardown: skipping non-cookie SQLite database {}",
                    jar.upper.display()
                );
                return Ok(());
            }
            let parent = jar.saved.parent().context("cookie state has no parent")?;
            std::fs::create_dir_all(parent)?;
            // Every directory beneath the private root is private as well.
            let mut directory = parent;
            while directory.starts_with(&root) {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
                let Some(next) = directory.parent() else {
                    break;
                };
                directory = next;
            }
            let _ = std::fs::remove_file(&temporary);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            snapshot(&jar.upper, &temporary).context("snapshot private cookie jar")?;
            let deleted = record_deletions(&temporary, jar.baseline.as_deref(), &jar.saved)
                .context("record cookie deletions")?;
            merge(&temporary, &jar.saved).context("merge persisted cookie jar")?;
            apply_deleted(&Connection::open(&temporary)?, &deleted)
                .context("apply cookie deletion tombstones")?;
            // Publish deletions before the jar: even a crash between renames
            // must not restore a removed cookie from the old saved jar.
            write_deletions(&jar.saved, &deleted).context("persist cookie deletion tombstones")?;
            std::fs::rename(&temporary, &jar.saved).context("publish persisted cookie jar")?;
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .context("sync cookie state directory")?;
            Ok(())
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error:#}", jar.saved.display()));
        }
        let _ = std::fs::remove_file(&temporary);
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}
