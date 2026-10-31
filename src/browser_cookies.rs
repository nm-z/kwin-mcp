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
const COOKIE_DATABASE_MAX_BYTES: u64 = 512 * 1024 * 1024;

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
    // Callers pass only session-owned or persisted private copies. Opening
    // read-write lets SQLite create the WAL shared-memory file when Chrome has
    // left the jar in WAL mode but removed its sidecars after shutdown.
    let database = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
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

fn apply_deleted(connection: &mut Connection, deleted: &DeletedCookies) -> Result<()> {
    let predicate = deleted
        .columns
        .iter()
        .map(|key| format!("{} IS ?", quote(key)))
        .collect::<Vec<_>>()
        .join(" AND ");
    // Applying a continuity snapshot is one operation. Autocommit here makes
    // every tombstone wait for a separate durable journal commit, which can
    // outlive the entire startup budget for an ordinary saved deletion list.
    let transaction = connection.transaction()?;
    {
        let mut remove = transaction.prepare(&format!(
            "DELETE FROM cookies WHERE {predicate} AND last_update_utc <= ?"
        ))?;
        for row in &deleted.rows {
            let mut identity = row.key.iter().map(Value::from).collect::<Vec<_>>();
            identity.push(Value::Integer(row.through));
            remove.execute(params_from_iter(identity))?;
        }
    }
    prune_expired(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// Tombstones remove the baseline version that a session deleted, including
/// old host rows and stale concurrent sessions. A newer last_update_utc wins
/// independently of session stop order; absence cannot date an unseen update.
pub fn apply_deletions(database: &Path, saved: &Path) -> Result<()> {
    let mut database = Connection::open(database)?;
    let deleted = read_deletions(saved, identity_columns(&database)?)?;
    apply_deleted(&mut database, &deleted)
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

fn cookie_path_metadata(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) => format!(
            "present(file={},dir={},mode={:04o},size={})",
            metadata.is_file(),
            metadata.is_dir(),
            metadata.permissions().mode() & 0o7777,
            metadata.len()
        ),
        Err(error) => format!("unavailable({error})"),
    }
}

/// Stage a private overlay-upper jar into a clean directory before opening it
/// with SQLite. Overlayfs can leave a character-device whiteout for a removed
/// lower-layer WAL; SQLite cannot open that sidecar by its pathname.
fn stage_private_cookie_jar(source: &Path, directory: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(source)?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "private cookie database is not a regular file"
    );
    let mut total = metadata.len();
    anyhow::ensure!(
        total <= COOKIE_DATABASE_MAX_BYTES,
        "cookie database exceeds the 512 MiB staging limit"
    );
    std::fs::create_dir(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let staged = directory.join("Cookies");
    std::fs::copy(source, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
    for suffix in ["-wal", "-journal"] {
        let mut source_sidecar = source.as_os_str().to_os_string();
        source_sidecar.push(suffix);
        let source_sidecar = PathBuf::from(source_sidecar);
        match std::fs::symlink_metadata(&source_sidecar) {
            Ok(metadata) if metadata.file_type().is_file() => {
                total = total.saturating_add(metadata.len());
                anyhow::ensure!(
                    total <= COOKIE_DATABASE_MAX_BYTES,
                    "cookie database and sidecars exceed the 512 MiB staging limit"
                );
                let mut staged_sidecar = staged.as_os_str().to_os_string();
                staged_sidecar.push(suffix);
                let staged_sidecar = PathBuf::from(staged_sidecar);
                std::fs::copy(source_sidecar, &staged_sidecar)?;
                std::fs::set_permissions(staged_sidecar, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(staged)
}

pub fn state_root() -> Result<PathBuf> {
    let state_home = match std::env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .context("HOME must be set when XDG_STATE_HOME is unset")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(state_home.join("kwin-mcp/browser-cookies"))
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Copy through SQLite so committed WAL changes are included.
pub fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        std::fs::metadata(source)?.len() <= COOKIE_DATABASE_MAX_BYTES,
        "cookie database exceeds the 512 MiB snapshot limit"
    );
    // `source` is a private copy, never a host database. SQLite needs a
    // read-write connection to recover a WAL-mode jar whose `-shm` was removed
    // when its last browser connection exited.
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
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

/// The database and its WAL are over the copy limit, so no snapshot is possible
/// and retrying cannot help.
#[derive(Debug)]
pub struct SnapshotTooLarge;

impl std::fmt::Display for SnapshotTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the database and its WAL exceed the 64 MiB copy limit")
    }
}

impl std::error::Error for SnapshotTooLarge {}

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
                    if total > HOST_COPY_MAX_BYTES {
                        return Err(SnapshotTooLarge.into());
                    }
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
            Err(error) if error.is::<SnapshotTooLarge>() => return Err(error),
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

fn cookie_schema_matches(left: &Path, right: &Path) -> Result<bool> {
    let left = Connection::open_with_flags(left, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let right = Connection::open_with_flags(right, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(
        columns(&left)? == columns(&right)?
            && identity_columns(&left)? == identity_columns(&right)?,
    )
}

fn deletion_schema_matches(saved: &Path, keys: &[String]) -> Result<bool> {
    match std::fs::read(saved.with_extension("deleted.json")) {
        Ok(bytes) => {
            let deleted: DeletedCookies = serde_json::from_slice(&bytes)?;
            Ok(deleted.columns == keys)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn session_schema_changed(session: &Path, baseline: Option<&Path>, saved: &Path) -> Result<bool> {
    for previous in baseline
        .into_iter()
        .chain([saved].into_iter().filter(|path| path.is_file()))
    {
        if previous.is_file() && !cookie_schema_matches(session, previous)? {
            return Ok(true);
        }
    }
    let session = Connection::open_with_flags(session, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    deletion_schema_matches(saved, &identity_columns(&session)?).map(|matches| !matches)
}

fn publish_session_jar(temporary: &Path, saved: &Path) -> Result<()> {
    let parent = saved.parent().context("cookie state has no parent")?;
    let deletions = saved.with_extension("deleted.json");
    match std::fs::remove_file(&deletions) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove stale cookie deletion tombstones"),
    }
    std::fs::rename(temporary, saved).context("publish session cookie jar after schema change")?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .context("sync cookie state directory")?;
    Ok(())
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
    let root = state_root()?;
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
            let parent = jar.saved.parent().context("cookie state has no parent")?;
            std::fs::create_dir_all(parent).context("create private cookie state directory")?;
            // Every directory beneath the private root is private as well.
            let mut directory = parent;
            while directory.starts_with(&root) {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .context("protect private cookie state directory")?;
                let Some(next) = directory.parent() else {
                    break;
                };
                directory = next;
            }
            let stage_directory = parent.join(".kwin-mcp-cookie-stage");
            let _ = std::fs::remove_dir_all(&stage_directory);
            let _ = std::fs::remove_file(&stage_directory);
            let result = (|| -> Result<()> {
                let staged = stage_private_cookie_jar(&jar.upper, &stage_directory)
                    .context("stage private session cookie database")?;
                let cookie_jar = match supports_cookie_jar(&staged) {
                    Ok(cookie_jar) => cookie_jar,
                    Err(error) => {
                        let mut wal = jar.upper.as_os_str().to_os_string();
                        wal.push("-wal");
                        let mut shm = jar.upper.as_os_str().to_os_string();
                        shm.push("-shm");
                        return Err(error).context(format!(
                            "inspect session cookie database {} (file {}; parent {}; WAL {}; SHM {})",
                            jar.upper.display(),
                            cookie_path_metadata(&jar.upper),
                            cookie_path_metadata(jar.upper.parent().unwrap_or(Path::new("."))),
                            cookie_path_metadata(Path::new(&wal)),
                            cookie_path_metadata(Path::new(&shm))
                        ));
                    }
                };
                if !cookie_jar {
                    eprintln!(
                        "teardown: skipping non-cookie SQLite database {}",
                        jar.upper.display()
                    );
                    return Ok(());
                }
                let _ = std::fs::remove_file(&temporary);
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)
                    .context("create temporary cookie database")?;
                snapshot(&staged, &temporary).context("snapshot private cookie jar")?;
                if session_schema_changed(&temporary, jar.baseline.as_deref(), &jar.saved)
                    .context("compare session cookie schema with persisted state")?
                {
                    eprintln!(
                        "teardown: cookie schema changed for {}; replacing persisted jar with the session jar",
                        jar.saved.display()
                    );
                    return publish_session_jar(&temporary, &jar.saved);
                }
                let deleted = record_deletions(&temporary, jar.baseline.as_deref(), &jar.saved)
                    .context("record cookie deletions")?;
                merge(&temporary, &jar.saved).context("merge persisted cookie jar")?;
                {
                    let mut temporary_database = Connection::open(&temporary)?;
                    apply_deleted(&mut temporary_database, &deleted)
                        .context("apply cookie deletion tombstones")?;
                }
                // Publish deletions before the jar: even a crash between renames
                // must not restore a removed cookie from the old saved jar.
                write_deletions(&jar.saved, &deleted)
                    .context("persist cookie deletion tombstones")?;
                std::fs::rename(&temporary, &jar.saved).context("publish persisted cookie jar")?;
                std::fs::File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .context("sync cookie state directory")?;
                Ok(())
            })();
            let _ = std::fs::remove_dir_all(&stage_directory);
            result
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error:#}", jar.saved.display()));
        }
        let _ = std::fs::remove_file(&temporary);
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}
