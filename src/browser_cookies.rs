//! Cookie continuity for isolated Chromium profiles. Host profiles stay read-only.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const HOST_COPY_MAX_BYTES: u64 = 64 * 1024 * 1024;
const HOST_COPY_ATTEMPTS: usize = 5;

#[derive(Clone, Serialize, Deserialize)]
pub struct CookieRoot {
    pub upper: PathBuf,
    pub saved: PathBuf,
}

struct CookieJar {
    upper: PathBuf,
    saved: PathBuf,
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
    anyhow::ensure!(std::fs::metadata(source)?.len() <= 512 * 1024 * 1024,
        "cookie database exceeds the 512 MiB snapshot limit");
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::ZERO)?;
    let mut copy = Connection::open(destination)?;
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o600))?;
    let step = rusqlite::backup::Backup::new(&source, &mut copy)?.step(-1)?;
    anyhow::ensure!(step == rusqlite::backup::StepResult::Done,
        "cookie database is locked ({step:?})");
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
/// files, verify two identical observations, then let SQLite read the frozen
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
                    anyhow::ensure!(total <= HOST_COPY_MAX_BYTES,
                        "host cookie database and WAL exceed the 64 MiB copy limit");
                    contents.push(Some(bytes));
                }
                Err(error) if suffix == "-wal" && error.kind() == std::io::ErrorKind::NotFound => {
                    contents.push(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let wal = contents.pop().flatten();
        let database = contents.pop().flatten().context("host cookie database is missing")?;
        Ok((database, wal))
    };
    for _ in 0..HOST_COPY_ATTEMPTS {
        // A rollback journal may contain the only committed version of a page.
        // Wait for that transaction to finish instead of copying an active one.
        if active_journal()? {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let first = read()?;
        let second = read()?;
        if first != second || active_journal()? {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
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
            let check: String = database.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
            anyhow::ensure!(check == "ok", "host cookie snapshot failed integrity checking");
            drop(database);
            std::fs::rename(&candidate, destination)?;
            clear_sidecars(destination)?;
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(directory);
        return result;
    }
    anyhow::bail!("host cookie database kept changing during all snapshot attempts")
}

/// Keep complete rows, including encrypted values and partition metadata.
/// A schema mismatch preserves the destination rather than guessing a migration.
pub fn merge(destination: &Path, source: &Path) -> Result<()> {
    if !source.is_file() {
        return Ok(());
    }
    let source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::ZERO)?;
    let mut destination = Connection::open(destination)?;
    destination.busy_timeout(Duration::ZERO)?;
    let columns = |connection: &Connection| -> Result<Vec<String>> {
        Ok(connection.prepare("PRAGMA table_info(cookies)")?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let names = columns(&destination)?;
    anyhow::ensure!(names == columns(&source)?, "cookie schemas differ");
    let indexes = destination.prepare("PRAGMA index_list(cookies)")?
        .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, bool>(2)?, row.get::<_, bool>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut keys = None;
    for (index, unique, partial) in indexes {
        if !unique || partial {
            continue;
        }
        let columns = destination.prepare(&format!("PRAGMA index_info({})", quote(&index)))?
            .query_map([], |row| row.get::<_, Option<String>>(2))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter().collect::<Option<Vec<_>>>();
        if let Some(columns) = columns
            && ["host_key", "name", "path"].iter().all(|key| columns.iter().any(|name| name == key))
        {
            keys = Some(columns);
            break;
        }
    }
    let keys = keys.context("cookie identity unique index is missing")?;
    let indices = keys.iter().map(|key| names.iter().position(|name| name == key)
        .context("cookie identity column is missing")).collect::<Result<Vec<_>>>()?;
    let updated = names.iter().position(|name| name == "last_update_utc")
        .context("cookie update timestamp is missing")?;
    let fields = names.iter().map(|name| quote(name)).collect::<Vec<_>>().join(",");
    let predicate = keys.iter().map(|key| format!("{} = ?", quote(key)))
        .collect::<Vec<_>>().join(" AND ");
    let placeholders = vec!["?"; names.len()].join(",");
    let transaction = destination.transaction()?;
    let mut rows = source.prepare(&format!("SELECT {fields} FROM cookies"))?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..names.len()).map(|index| row.get::<_, Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut identity = indices.iter().map(|index| values[*index].clone()).collect::<Vec<_>>();
        identity.push(values[updated].clone());
        let newer: bool = transaction.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM cookies WHERE {predicate} AND last_update_utc >= ?)"),
            params_from_iter(&identity), |row| row.get(0))?;
        if !newer {
            transaction.execute(
                &format!("DELETE FROM cookies WHERE {predicate}"),
                params_from_iter(identity.iter().take(indices.len())))?;
            transaction.execute(
                &format!("INSERT INTO cookies ({fields}) VALUES ({placeholders})"),
                params_from_iter(&values))?;
        }
    }
    transaction.commit()?;
    Ok(())
}

pub fn persist(roots: &[CookieRoot]) -> Result<()> {
    let mut jars = Vec::new();
    for root in roots {
        let mut directories = vec![(root.upper.clone(), 0)];
        while let Some((directory, depth)) = directories.pop() {
            let Ok(entries) = std::fs::read_dir(directory) else { continue };
            for entry in entries.flatten() {
                let kind = entry.file_type()?;
                if kind.is_dir() && depth < 2 {
                    directories.push((entry.path(), depth + 1));
                } else if kind.is_file() && entry.file_name() == "Cookies" {
                    let upper = entry.path();
                    let saved = root.saved.join(upper.strip_prefix(&root.upper)?);
                    jars.push(CookieJar {
                        upper,
                        saved,
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
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true)
        .truncate(false).mode(0o600).open(root.join("lock"))?;
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
            std::fs::create_dir_all(parent)?;
            // Every directory beneath the private root is private as well.
            let mut directory = parent;
            while directory.starts_with(&root) {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
                let Some(next) = directory.parent() else { break };
                directory = next;
            }
            let _ = std::fs::remove_file(&temporary);
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
                .open(&temporary)?;
            snapshot(&jar.upper, &temporary)?;
            merge(&temporary, &jar.saved)?;
            std::fs::rename(&temporary, &jar.saved)?;
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
