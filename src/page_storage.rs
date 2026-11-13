//! Page storage continuity for isolated Chromium profiles: Local Storage and
//! IndexedDB, where sites keep sign-ins that are not cookies (#955). Host
//! profiles stay read-only, as with `browser_cookies`.
//!
//! At session start each profile's stores are copied into the session's disk
//! (tables, which LevelDB never changes once written, as hard links) and the
//! copy is mounted over the profile's store as the read-only lower layer of an
//! overlay of its own, so the session's browser writes into that overlay's
//! upper layer and never into a file shared with the host. What the sessions
//! saved for the profile is applied to the copy first. When the session stops,
//! the copy and the upper layer show what the session changed, and that is
//! saved under the state root for the next session.
//!
//! Local Storage is one LevelDB per profile shared by every origin, so it is
//! carried key by key: a saved key holds the value the session started with
//! (`base`) and the value it left (`value`). A later session takes the saved
//! value where the host still holds the base value, and where the host changed
//! that key too, whichever change is later. The values are applied as one
//! write batch in a log file of their own, which the browser's LevelDB replays
//! when it opens the database. IndexedDB keeps a LevelDB per origin, so an
//! origin's databases are saved and restored whole: the saved copy is used
//! when the host has not written that origin since the session started from
//! it, or when the saved copy is newer.

use anyhow::{Context, Result};
use base64::Engine;
use rusty_leveldb::LdbIterator;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A profile's Local Storage database.
pub const LOCAL_STORAGE: &str = "Local Storage/leveldb";
/// A profile's IndexedDB directory, one LevelDB (and blob directory) per origin.
pub const INDEXED_DB: &str = "IndexedDB";
const ROOTS_FILE: &str = "roots.json";
const SAVED_LOCAL_STORAGE: &str = "local-storage.json";
const SNAPSHOT_ATTEMPTS: usize = 20;
const SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(25);
/// Bytes one start or stop may copy where a hard link is not possible (the
/// session disk on another file system). Past it a store stays as the HOME
/// overlay shows it, without continuity.
const COPY_BUDGET: u64 = 512 * 1024 * 1024;
const LOG_BLOCK_SIZE: usize = 32768;
const LOG_HEADER_SIZE: usize = 7;
/// Room above the last sequence number seen, for records a damaged log hides
/// from this reader but not from LevelDB's.
const SEQUENCE_MARGIN: u64 = 1 << 24;
/// base::Time counts microseconds from 1601; this is 1970 in it.
const CHROME_UNIX_EPOCH_MICROS: i64 = 11_644_473_600_000_000;
const META_PREFIX: &[u8] = b"META:";
const ACCESS_PREFIX: &[u8] = b"METAACCESS:";

/// An overlay the session mounts over a profile store.
pub struct Mount {
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub destination: PathBuf,
}

/// The overlays for a session, and why some stores keep no continuity.
pub struct Prepared {
    pub mounts: Vec<Mount>,
    pub errors: Vec<String>,
}

/// What `persist` needs about a profile, kept beside its layers.
#[derive(Serialize, Deserialize)]
struct ProfileRecord {
    profile: PathBuf,
    saved: PathBuf,
    dir: PathBuf,
    local_storage: bool,
    /// Each IndexedDB origin the session started with, and the host's last
    /// sequence number that the session's copy of it derives from.
    indexed_db: Option<BTreeMap<String, Option<u64>>>,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedLocalStorage {
    #[serde(default)]
    origins: BTreeMap<String, SavedOrigin>,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedOrigin {
    #[serde(default)]
    meta: Option<String>,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    keys: BTreeMap<String, SavedKey>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct SavedKey {
    base: Option<String>,
    value: Option<String>,
    /// base::Time of the change: the origin's last modification in the session.
    time: i64,
}

#[derive(Serialize, Deserialize)]
struct SavedDatabase {
    /// Unix nanoseconds when the session that saved it stopped.
    modified: i128,
    /// The host's last sequence number the saved copy derives from.
    #[serde(default)]
    base_sequence: Option<u64>,
    #[serde(default)]
    deleted: bool,
}

pub fn state_root() -> Result<PathBuf> {
    Ok(crate::browser_cookies::state_root()?
        .parent()
        .context("cookie state has no parent")?
        .join("page-storage"))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn unix_nanos(time: std::time::SystemTime) -> i128 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX))
        .unwrap_or_default()
}

fn chrome_now() -> i64 {
    i64::try_from(unix_nanos(std::time::SystemTime::now()) / 1000)
        .unwrap_or(i64::MAX)
        .saturating_add(CHROME_UNIX_EPOCH_MICROS)
}

// ── LevelDB files ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileKind {
    Current,
    Manifest(u64),
    Log(u64),
    Table(u64),
    /// LOCK, LOG, LOG.old and temporary files: not part of the database.
    Other,
}

fn file_kind(name: &str) -> FileKind {
    if name == "CURRENT" {
        return FileKind::Current;
    }
    if let Some(number) = name.strip_prefix("MANIFEST-").and_then(|n| n.parse().ok()) {
        return FileKind::Manifest(number);
    }
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return FileKind::Other;
    };
    match (stem.parse(), extension) {
        (Ok(number), "log") => FileKind::Log(number),
        (Ok(number), "ldb" | "sst") => FileKind::Table(number),
        _ => FileKind::Other,
    }
}

fn file_number(kind: FileKind) -> Option<u64> {
    match kind {
        FileKind::Manifest(number) | FileKind::Log(number) | FileKind::Table(number) => {
            Some(number)
        }
        FileKind::Current | FileKind::Other => None,
    }
}

/// A hard link of `source` at `destination`, or a copy charged to `budget`
/// when the file systems differ.
fn link_or_copy(source: &Path, destination: &Path, budget: &mut u64) -> std::io::Result<()> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(error),
        Err(_) => copy_charged(source, destination, budget),
    }
}

fn copy_charged(source: &Path, destination: &Path, budget: &mut u64) -> std::io::Result<()> {
    let size = std::fs::metadata(source)?.len();
    if size > *budget {
        return Err(std::io::Error::other(format!(
            "copying {} would pass the {} MiB copy budget",
            source.display(),
            COPY_BUDGET >> 20
        )));
    }
    let copied = std::fs::copy(source, destination)?;
    *budget = budget.saturating_sub(copied);
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// A copy of a LevelDB directory nothing writes: tables linked, the files
/// LevelDB appends to or replaces copied.
fn copy_leveldb(source: &Path, destination: &Path, budget: &mut u64) -> Result<()> {
    private_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let target = destination.join(&name);
        match file_kind(&name.to_string_lossy()) {
            FileKind::Table(_) => link_or_copy(&entry.path(), &target, budget)?,
            FileKind::Current | FileKind::Manifest(_) | FileKind::Log(_) => {
                copy_charged(&entry.path(), &target, budget)?
            }
            FileKind::Other => {}
        }
    }
    Ok(())
}

/// Copy a LevelDB directory that another process may be writing (the host
/// browser keeps it open) into `destination` as a crash-consistent image:
/// CURRENT, then the manifest it names, then the logs, then the tables. A log
/// or manifest that grows during the copy ends in a torn record, which
/// LevelDB's recovery drops as after a crash. A flush or compaction during the
/// copy can delete a table the copied manifest still names, or the log it
/// recovers from, so the image is checked and the copy repeated.
fn snapshot_leveldb(source: &Path, destination: &Path, budget: &mut u64) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..SNAPSHOT_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SNAPSHOT_RETRY_DELAY);
        }
        let _ = std::fs::remove_dir_all(destination);
        let before = *budget;
        let result = (|| -> Result<()> {
            private_dir(destination)?;
            let current = std::fs::read(source.join("CURRENT")).context("read CURRENT")?;
            let manifest = String::from_utf8_lossy(&current).trim().to_owned();
            anyhow::ensure!(
                matches!(file_kind(&manifest), FileKind::Manifest(_)),
                "CURRENT names no manifest"
            );
            copy_charged(
                &source.join(&manifest),
                &destination.join(&manifest),
                budget,
            )?;
            std::fs::write(destination.join("CURRENT"), &current)?;
            let mut tables = Vec::new();
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                let name = entry.file_name();
                match file_kind(&name.to_string_lossy()) {
                    FileKind::Log(_) => {
                        // A log retired since the listing is skipped; the
                        // check below tells whether the image needed it.
                        if let Err(error) =
                            copy_charged(&entry.path(), &destination.join(&name), budget)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            return Err(error.into());
                        }
                    }
                    FileKind::Table(_) => tables.push(name),
                    FileKind::Current | FileKind::Manifest(_) | FileKind::Other => {}
                }
            }
            for name in tables {
                if let Err(error) =
                    link_or_copy(&source.join(&name), &destination.join(&name), budget)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(error.into());
                }
            }
            let state = leveldb_state(destination)?;
            let present = present_numbers(destination)?;
            if let Some(missing) = state.live_tables.difference(&present).next() {
                anyhow::bail!("table {missing} was removed during the copy");
            }
            anyhow::ensure!(
                state.log_number == 0 || present.contains(&state.log_number),
                "log {} was retired during the copy",
                state.log_number
            );
            Ok(())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                *budget = before;
                last_error = Some(error);
            }
        }
    }
    let _ = std::fs::remove_dir_all(destination);
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no attempt made")))
        .with_context(|| format!("copy {} while it is in use", source.display()))
}

fn present_numbers(directory: &Path) -> Result<BTreeSet<u64>> {
    let mut numbers = BTreeSet::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if let Some(number) = file_number(file_kind(&entry.file_name().to_string_lossy())) {
            numbers.insert(number);
        }
    }
    Ok(numbers)
}

/// The records of a LevelDB log, read as LevelDB's own reader does: a record
/// with a bad checksum or length drops the rest of its block, and a torn
/// record at the end of the file ends it.
fn log_records(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut records = Vec::new();
    let mut pending: Option<Vec<u8>> = None;
    let mut block = 0;
    while block < bytes.len() {
        let end = (block + LOG_BLOCK_SIZE).min(bytes.len());
        let mut offset = block;
        while end - offset >= LOG_HEADER_SIZE {
            let header = &bytes[offset..offset + LOG_HEADER_SIZE];
            let checksum = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let length = usize::from(u16::from_le_bytes([header[4], header[5]]));
            let kind = header[6];
            let start = offset + LOG_HEADER_SIZE;
            if kind == 0 && length == 0 {
                // Preallocated space.
                break;
            }
            if start + length > end {
                // Torn at the end of the file, or a bad length.
                pending = None;
                break;
            }
            let data = &bytes[start..start + length];
            if unmask(checksum) != crc32c::crc32c_append(crc32c::crc32c(&[kind]), data) {
                pending = None;
                break;
            }
            offset = start + length;
            match kind {
                1 => {
                    pending = None;
                    records.push(data.to_vec());
                }
                2 => pending = Some(data.to_vec()),
                3 => {
                    if let Some(record) = pending.as_mut() {
                        record.extend_from_slice(data);
                    }
                }
                4 => {
                    if let Some(mut record) = pending.take() {
                        record.extend_from_slice(data);
                        records.push(record);
                    }
                }
                _ => pending = None,
            }
        }
        block += LOG_BLOCK_SIZE;
    }
    records
}

fn mask(checksum: u32) -> u32 {
    checksum.rotate_right(15).wrapping_add(0xa282_ead8)
}

fn unmask(masked: u32) -> u32 {
    masked.wrapping_sub(0xa282_ead8).rotate_left(15)
}

/// `records` framed as a LevelDB log.
fn log_bytes(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        let mut rest = record.as_slice();
        let mut first = true;
        loop {
            let left = LOG_BLOCK_SIZE - out.len() % LOG_BLOCK_SIZE;
            if left < LOG_HEADER_SIZE {
                out.resize(out.len() + left, 0);
                continue;
            }
            let take = rest.len().min(left - LOG_HEADER_SIZE);
            let last = take == rest.len();
            let kind: u8 = match (first, last) {
                (true, true) => 1,
                (true, false) => 2,
                (false, false) => 3,
                (false, true) => 4,
            };
            let data = &rest[..take];
            let checksum = mask(crc32c::crc32c_append(crc32c::crc32c(&[kind]), data));
            out.extend_from_slice(&checksum.to_le_bytes());
            // At most a block, so it fits.
            out.extend_from_slice(&u16::try_from(take).unwrap_or(u16::MAX).to_le_bytes());
            out.push(kind);
            out.extend_from_slice(data);
            rest = &rest[take..];
            first = false;
            if last {
                break;
            }
        }
    }
    out
}

fn varint(bytes: &[u8], position: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*position)?;
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    let [low, ..] = value.to_le_bytes();
    let mut low = low;
    while value >= 0x80 {
        out.push(low | 0x80);
        value >>= 7;
        [low, ..] = value.to_le_bytes();
    }
    out.push(low);
}

fn length_prefixed<'a>(bytes: &'a [u8], position: &mut usize) -> Option<&'a [u8]> {
    let length = usize::try_from(varint(bytes, position)?).ok()?;
    let slice = bytes.get(*position..*position + length)?;
    *position += length;
    Some(slice)
}

#[derive(Default, Debug)]
struct LevelDbState {
    last_sequence: u64,
    log_number: u64,
    next_file: u64,
    live_tables: BTreeSet<u64>,
}

/// What LevelDB's recovery would see in `directory`: the manifest's version
/// edits, then the batches in the logs it still replays.
fn leveldb_state(directory: &Path) -> Result<LevelDbState> {
    let current = std::fs::read(directory.join("CURRENT")).context("read CURRENT")?;
    let manifest = String::from_utf8_lossy(&current).trim().to_owned();
    let bytes = std::fs::read(directory.join(&manifest)).context("read the manifest")?;
    let mut state = LevelDbState::default();
    let mut prev_log = 0;
    let mut live = BTreeSet::new();
    for edit in log_records(&bytes) {
        let mut position = 0;
        let parsed = (|| -> Option<()> {
            while position < edit.len() {
                match varint(&edit, &mut position)? {
                    1 => {
                        length_prefixed(&edit, &mut position)?;
                    }
                    2 => state.log_number = varint(&edit, &mut position)?,
                    3 => state.next_file = varint(&edit, &mut position)?,
                    4 => state.last_sequence = varint(&edit, &mut position)?,
                    5 => {
                        varint(&edit, &mut position)?;
                        length_prefixed(&edit, &mut position)?;
                    }
                    6 => {
                        let level = varint(&edit, &mut position)?;
                        let number = varint(&edit, &mut position)?;
                        live.remove(&(level, number));
                    }
                    7 => {
                        let level = varint(&edit, &mut position)?;
                        let number = varint(&edit, &mut position)?;
                        varint(&edit, &mut position)?;
                        length_prefixed(&edit, &mut position)?;
                        length_prefixed(&edit, &mut position)?;
                        live.insert((level, number));
                    }
                    9 => prev_log = varint(&edit, &mut position)?,
                    _ => return None,
                }
            }
            Some(())
        })();
        anyhow::ensure!(
            parsed.is_some(),
            "the manifest holds an edit this reader does not know"
        );
    }
    state.live_tables = live.into_iter().map(|(_, number)| number).collect();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let FileKind::Log(number) = file_kind(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if number < state.log_number && number != prev_log {
            continue;
        }
        for batch in log_records(&std::fs::read(entry.path())?) {
            if batch.len() < 12 {
                continue;
            }
            let sequence = u64::from_le_bytes(batch[..8].try_into()?);
            let count = u32::from_le_bytes(batch[8..12].try_into()?);
            let last = sequence + u64::from(count.max(1)) - 1;
            state.last_sequence = state.last_sequence.max(last);
        }
    }
    Ok(state)
}

/// Write `operations` (a value to put, or None to delete) as one batch in a
/// new log in `directory`, numbered and sequenced above everything there, so
/// LevelDB replays it last and its values win.
fn append_batch(
    directory: &Path,
    operations: &[(Vec<u8>, Option<Vec<u8>>)],
    least_sequence: u64,
) -> Result<()> {
    if operations.is_empty() {
        return Ok(());
    }
    let state = leveldb_state(directory)?;
    let present = present_numbers(directory)?;
    let number = state
        .next_file
        .max(present.last().copied().unwrap_or(0) + 1);
    let sequence = state.last_sequence.max(least_sequence) + 1 + SEQUENCE_MARGIN;
    let mut batch = Vec::new();
    batch.extend_from_slice(&sequence.to_le_bytes());
    batch.extend_from_slice(&u32::try_from(operations.len())?.to_le_bytes());
    for (key, value) in operations {
        batch.push(u8::from(value.is_some()));
        put_varint(&mut batch, u64::try_from(key.len())?);
        batch.extend_from_slice(key);
        if let Some(value) = value {
            put_varint(&mut batch, u64::try_from(value.len())?);
            batch.extend_from_slice(value);
        }
    }
    let path = directory.join(format!("{number:06}.log"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    std::io::Write::write_all(&mut file, &log_bytes(&[batch]))?;
    Ok(())
}

// ── Local Storage ────────────────────────────────────────────────────────

/// A Local Storage database opened for reading. LevelDB writes on open, so it
/// is always a throwaway copy.
struct Reader {
    db: rusty_leveldb::DB,
}

impl Reader {
    fn open(copy: &Path) -> Result<Self> {
        let options = rusty_leveldb::Options {
            create_if_missing: false,
            paranoid_checks: false,
            ..Default::default()
        };
        let db = rusty_leveldb::DB::open(copy, options)
            .map_err(|error| anyhow::anyhow!("open {}: {error}", copy.display()))?;
        Ok(Self { db })
    }

    fn scan(&mut self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut iterator = self
            .db
            .new_iter()
            .map_err(|error| anyhow::anyhow!("iterate: {error}"))?;
        iterator.seek(prefix);
        let mut found = Vec::new();
        while iterator.valid() {
            let Some((key, value)) = iterator.current() else {
                break;
            };
            if !key.starts_with(prefix) {
                break;
            }
            found.push((key[prefix.len()..].to_vec(), value.to_vec()));
            if !iterator.advance() {
                break;
            }
        }
        Ok(found)
    }

    /// Each origin's metadata (`META:` entries), which the browser rewrites
    /// whenever it commits a change to the origin.
    fn metas(&mut self) -> Result<BTreeMap<String, Vec<u8>>> {
        Ok(self
            .scan(META_PREFIX)?
            .into_iter()
            .filter_map(|(origin, meta)| Some((String::from_utf8(origin).ok()?, meta)))
            .collect())
    }

    fn entries(&mut self, origin: &str) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        Ok(self.scan(&data_prefix(origin))?.into_iter().collect())
    }

    fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.db.get(key).map(|value| value.to_vec())
    }

    fn sequence(&mut self) -> u64 {
        self.db.get_snapshot().sequence()
    }
}

fn data_prefix(origin: &str) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(origin.len() + 2);
    prefix.push(b'_');
    prefix.extend_from_slice(origin.as_bytes());
    prefix.push(0);
    prefix
}

fn prefixed(prefix: &[u8], origin: &str) -> Vec<u8> {
    [prefix, origin.as_bytes()].concat()
}

/// The last modification of an origin, from its `META:` entry (a protobuf
/// whose field 1 is a base::Time).
fn meta_time(meta: &[u8]) -> Option<i64> {
    let mut position = 0;
    while position < meta.len() {
        let tag = varint(meta, &mut position)?;
        match tag & 7 {
            0 => {
                let value = varint(meta, &mut position)?;
                if tag >> 3 == 1 {
                    return i64::try_from(value).ok();
                }
            }
            1 => position += 8,
            2 => {
                length_prefixed(meta, &mut position)?;
            }
            5 => position += 4,
            _ => return None,
        }
    }
    None
}

type Operations = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// The writes that bring the saved keys into a session's copy of the host's
/// Local Storage (see the module documentation for the rule).
fn plan_local_storage(
    saved: &SavedLocalStorage,
    metas: &BTreeMap<String, Vec<u8>>,
    mut host_entries: impl FnMut(&str) -> Result<BTreeMap<Vec<u8>, Vec<u8>>>,
) -> Result<Operations> {
    let decode = |value: &Option<String>| -> Option<Option<Vec<u8>>> {
        match value {
            Some(value) => unb64(value).map(Some),
            None => Some(None),
        }
    };
    let mut operations = Vec::new();
    for (origin, saved_origin) in &saved.origins {
        let host_time = metas
            .get(origin)
            .and_then(|meta| meta_time(meta))
            .unwrap_or(0);
        let host = host_entries(origin)?;
        let mut changed = false;
        for (key, entry) in &saved_origin.keys {
            let (Some(key), Some(base), Some(value)) =
                (unb64(key), decode(&entry.base), decode(&entry.value))
            else {
                continue;
            };
            let current = host.get(&key).cloned();
            if current == value {
                continue;
            }
            if current == base || entry.time > host_time {
                operations.push(([data_prefix(origin), key].concat(), value));
                changed = true;
            }
        }
        if changed && !metas.contains_key(origin) {
            for (prefix, value) in [
                (META_PREFIX, &saved_origin.meta),
                (ACCESS_PREFIX, &saved_origin.access),
            ] {
                if let Some(value) = value.as_deref().and_then(unb64) {
                    operations.push((prefixed(prefix, origin), Some(value)));
                }
            }
        }
    }
    Ok(operations)
}

/// A changed key: its name, value at start, value at stop.
type KeyChange = (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);

/// One origin's changes in a session.
struct OriginChange {
    meta: Option<Vec<u8>>,
    access: Option<Vec<u8>>,
    time: i64,
    keys: Vec<KeyChange>,
}

/// Fold a session's changes into the saved state.
fn merge_local_storage(saved: &mut SavedLocalStorage, changes: BTreeMap<String, OriginChange>) {
    for (origin, change) in changes {
        let entry = saved.origins.entry(origin).or_default();
        if let Some(meta) = &change.meta {
            entry.meta = Some(b64(meta));
        }
        if let Some(access) = &change.access {
            entry.access = Some(b64(access));
        }
        for (key, base, value) in change.keys {
            let key = b64(&key);
            let base = base.as_deref().map(b64);
            let value = value.as_deref().map(b64);
            let base = match entry.keys.get(&key) {
                // The session started from the saved value, so the change is
                // still relative to what the host held.
                Some(previous) if previous.value == base => previous.base.clone(),
                // Another session changed the key later than this one did.
                Some(previous) if previous.time > change.time => continue,
                _ => base,
            };
            if base == value {
                entry.keys.remove(&key);
            } else {
                entry.keys.insert(
                    key,
                    SavedKey {
                        base,
                        value,
                        time: change.time,
                    },
                );
            }
        }
    }
    saved.origins.retain(|_, origin| !origin.keys.is_empty());
}

fn load_json<T: for<'a> Deserialize<'a> + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn store_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    private_dir(parent)?;
    let temporary = path.with_extension("json.new");
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(value)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// The session's private Local Storage: the host's, with the saved keys
/// applied. Ok(false) when the profile has none.
fn prepare_local_storage(record: &ProfileRecord, budget: &mut u64) -> Result<bool> {
    let host = record.profile.join(LOCAL_STORAGE);
    if !host.join("CURRENT").is_file() {
        return Ok(false);
    }
    let lower = record.dir.join("ls-lower");
    snapshot_leveldb(&host, &lower, budget)?;
    let saved: SavedLocalStorage = load_json(&record.saved.join(SAVED_LOCAL_STORAGE))?;
    if !saved.origins.is_empty() {
        let copy = record.dir.join("ls-read");
        copy_leveldb(&lower, &copy, budget)?;
        let planned = (|| {
            let mut reader = Reader::open(&copy)?;
            let metas = reader.metas()?;
            let operations = plan_local_storage(&saved, &metas, |origin| reader.entries(origin))?;
            Ok::<_, anyhow::Error>((operations, reader.sequence()))
        })();
        let _ = std::fs::remove_dir_all(&copy);
        let (operations, sequence) = planned?;
        append_batch(&lower, &operations, sequence)?;
    }
    Ok(true)
}

/// What the session changed in its Local Storage, per origin.
fn local_storage_changes(start: &Path, end: &Path) -> Result<BTreeMap<String, OriginChange>> {
    let mut before = Reader::open(start)?;
    let mut after = Reader::open(end)?;
    let old = before.metas()?;
    let new = after.metas()?;
    let mut changes = BTreeMap::new();
    for origin in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        if old.get(origin) == new.get(origin) {
            continue;
        }
        let was = before.entries(origin)?;
        let now = after.entries(origin)?;
        let keys: Vec<KeyChange> = was
            .keys()
            .chain(now.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|key| was.get(*key) != now.get(*key))
            .map(|key| (key.clone(), was.get(key).cloned(), now.get(key).cloned()))
            .collect();
        if keys.is_empty() {
            continue;
        }
        let meta = new.get(origin).cloned();
        changes.insert(
            origin.clone(),
            OriginChange {
                time: meta
                    .as_deref()
                    .and_then(meta_time)
                    .unwrap_or_else(chrome_now),
                meta,
                access: after.get(&prefixed(ACCESS_PREFIX, origin)),
                keys,
            },
        );
    }
    Ok(changes)
}

/// Save what the session changed in its Local Storage.
fn persist_local_storage(record: &ProfileRecord, budget: &mut u64) -> Result<()> {
    let lower = record.dir.join("ls-lower");
    let start = record.dir.join("ls-start");
    let end = record.dir.join("ls-end");
    let _ = std::fs::remove_dir_all(&start);
    let _ = std::fs::remove_dir_all(&end);
    let changes = copy_leveldb(&lower, &start, budget)
        .and_then(|()| merge_layers(&lower, &record.dir.join("ls-upper"), &end, budget, true))
        .and_then(|()| local_storage_changes(&start, &end));
    let _ = std::fs::remove_dir_all(&start);
    let _ = std::fs::remove_dir_all(&end);
    let changes = changes?;
    if changes.is_empty() {
        return Ok(());
    }
    let path = record.saved.join(SAVED_LOCAL_STORAGE);
    let mut saved: SavedLocalStorage = load_json(&path)?;
    merge_local_storage(&mut saved, changes);
    store_json(&path, &saved)
}

// ── IndexedDB ────────────────────────────────────────────────────────────

/// The origin an IndexedDB entry belongs to (`https_example.com_0` for
/// `https_example.com_0.indexeddb.leveldb`).
fn origin_stem(name: &str) -> Option<&str> {
    name.split_once(".indexeddb.").map(|(stem, _)| stem)
}

fn origins(directory: &Path) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut found: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(found);
    };
    for entry in entries {
        let entry = entry?;
        if let Some(stem) = origin_stem(&entry.file_name().to_string_lossy()) {
            found.entry(stem.to_owned()).or_default().push(entry.path());
        }
    }
    Ok(found)
}

fn is_leveldb(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "leveldb")
}

/// The last sequence number of an origin's database, None without one.
fn origin_sequence(parts: &[PathBuf]) -> Result<Option<u64>> {
    for part in parts {
        if is_leveldb(part) && part.join("CURRENT").is_file() {
            return Ok(Some(leveldb_state(part)?.last_sequence));
        }
    }
    Ok(None)
}

/// Newest modification among an origin's files, LevelDB's own text log and
/// lock aside.
fn newest_change(path: &Path) -> i128 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_dir() {
        return std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| newest_change(&entry.path()))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if matches!(&*name, "LOG" | "LOG.old" | "LOCK") {
        return 0;
    }
    metadata.modified().map(unix_nanos).unwrap_or(0)
}

/// Hard links of a tree's files, copies where linking fails.
fn link_tree(source: &Path, destination: &Path, budget: &mut u64) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        private_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            match link_tree(&entry.path(), &destination.join(entry.file_name()), budget) {
                Ok(()) => {}
                // Deleted by the host browser while this ran.
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
    } else if metadata.is_file() {
        link_or_copy(source, destination, budget)?;
    }
    Ok(())
}

/// One origin's parts copied: databases as LevelDB copies (`live` when the
/// host may be writing them), anything else linked.
fn copy_origin(parts: &[PathBuf], destination: &Path, live: bool, budget: &mut u64) -> Result<()> {
    for part in parts {
        let target = destination.join(part.file_name().context("part has no name")?);
        if is_leveldb(part) && part.join("CURRENT").is_file() {
            if live {
                snapshot_leveldb(part, &target, budget)?;
            } else {
                copy_leveldb(part, &target, budget)?;
            }
        } else {
            link_tree(part, &target, budget)?;
        }
    }
    Ok(())
}

/// The session's private IndexedDB: per origin the host's, or the saved copy
/// where that wins. Returns the host sequence each origin derives from.
fn prepare_indexed_db(
    record: &ProfileRecord,
    budget: &mut u64,
) -> Result<BTreeMap<String, Option<u64>>> {
    let host = record.profile.join(INDEXED_DB);
    let saved_root = record.saved.join(INDEXED_DB);
    let lower = record.dir.join("idb-lower");
    private_dir(&lower)?;
    let host_origins = origins(&host)?;
    let mut saved = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(&saved_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".json") {
                let database: Option<SavedDatabase> = load_json(&entry.path())?;
                if let Some(database) = database {
                    saved.insert(stem.to_owned(), database);
                }
            }
        }
    }
    let mut bases = BTreeMap::new();
    for stem in host_origins
        .keys()
        .chain(saved.keys())
        .collect::<BTreeSet<_>>()
    {
        let parts = host_origins
            .get(stem)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let host_sequence = origin_sequence(parts).ok().flatten();
        let saved_entry = saved.get(stem).filter(|entry| {
            host_sequence == entry.base_sequence
                || entry.modified
                    > parts
                        .iter()
                        .map(|part| newest_change(part))
                        .max()
                        .unwrap_or(0)
        });
        if let Some(entry) = saved_entry {
            if !entry.deleted {
                let copy = saved_root.join(stem);
                let parts: Vec<PathBuf> = std::fs::read_dir(&copy)
                    .with_context(|| format!("read {}", copy.display()))?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<_>>()?;
                copy_origin(&parts, &lower, false, budget)?;
                bases.insert(stem.clone(), entry.base_sequence);
            }
        } else if !parts.is_empty() {
            copy_origin(parts, &lower, true, budget)?;
            bases.insert(stem.clone(), host_sequence);
        }
    }
    // Entries of no origin are kept as the host has them.
    if let Ok(entries) = std::fs::read_dir(&host) {
        for entry in entries.flatten() {
            if origin_stem(&entry.file_name().to_string_lossy()).is_none() {
                link_tree(&entry.path(), &lower.join(entry.file_name()), budget)?;
            }
        }
    }
    Ok(bases)
}

/// Save each IndexedDB origin the session wrote, or a tombstone for one it removed.
fn persist_indexed_db(
    record: &ProfileRecord,
    bases: &BTreeMap<String, Option<u64>>,
    budget: &mut u64,
) -> Result<()> {
    let lower = record.dir.join("idb-lower");
    let end = record.dir.join("idb-end");
    let _ = std::fs::remove_dir_all(&end);
    let result = merge_layers(&lower, &record.dir.join("idb-upper"), &end, budget, false)
        .and_then(|()| save_indexed_db(record, bases, &lower, &end, budget));
    let _ = std::fs::remove_dir_all(&end);
    result
}

fn save_indexed_db(
    record: &ProfileRecord,
    bases: &BTreeMap<String, Option<u64>>,
    lower: &Path,
    end: &Path,
    budget: &mut u64,
) -> Result<()> {
    let before = origins(lower)?;
    let after = origins(end)?;
    let saved_root = record.saved.join(INDEXED_DB);
    let now = unix_nanos(std::time::SystemTime::now());
    let mut failures = Vec::new();
    for stem in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let result = (|| -> Result<()> {
            let base_sequence = bases.get(stem).copied().flatten();
            let Some(parts) = after.get(stem) else {
                // The session removed the origin's databases.
                let _ = std::fs::remove_dir_all(saved_root.join(stem));
                return store_json(
                    &saved_root.join(format!("{stem}.json")),
                    &SavedDatabase {
                        modified: now,
                        base_sequence,
                        deleted: true,
                    },
                );
            };
            let end_sequence = origin_sequence(parts)?;
            let changed = match before.get(stem) {
                Some(start) => end_sequence > origin_sequence(start)?,
                None => end_sequence.is_some_and(|sequence| sequence > 0),
            };
            if !changed {
                return Ok(());
            }
            private_dir(&saved_root)?;
            let staged = saved_root.join(format!(".{stem}.new"));
            let _ = std::fs::remove_dir_all(&staged);
            private_dir(&staged)?;
            copy_origin(parts, &staged, false, budget)?;
            let target = saved_root.join(stem);
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&staged, &target)?;
            store_json(
                &saved_root.join(format!("{stem}.json")),
                &SavedDatabase {
                    modified: now,
                    base_sequence,
                    deleted: false,
                },
            )
        })();
        if let Err(error) = result {
            failures.push(format!("{stem}: {error:#}"));
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

// ── Overlay layers ───────────────────────────────────────────────────────

fn overlay_opaque(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut value = [0u8; 4];
    for name in [c"user.overlay.opaque", c"trusted.overlay.opaque"] {
        // SAFETY: both strings are NUL-terminated and the buffer's length is
        // the one passed.
        let read = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if read > 0 && value[0] == b'y' {
            return true;
        }
    }
    false
}

/// The directory a session saw at `upper` over `lower`, as links in `out`:
/// upper entries over lower ones, overlay whiteouts (character devices)
/// removing them, opaque directories hiding the lower directory. With
/// `leveldb`, the files LevelDB appends to or replaces are copied and its
/// lock and text log left out.
fn merge_layers(
    lower: &Path,
    upper: &Path,
    out: &Path,
    budget: &mut u64,
    leveldb: bool,
) -> Result<()> {
    private_dir(out)?;
    let mut names: BTreeMap<std::ffi::OsString, (Option<PathBuf>, Option<PathBuf>)> =
        BTreeMap::new();
    if !overlay_opaque(upper)
        && let Ok(entries) = std::fs::read_dir(lower)
    {
        for entry in entries.flatten() {
            names.insert(entry.file_name(), (Some(entry.path()), None));
        }
    }
    if let Ok(entries) = std::fs::read_dir(upper) {
        for entry in entries.flatten() {
            names.entry(entry.file_name()).or_default().1 = Some(entry.path());
        }
    }
    for (name, (from_lower, from_upper)) in names {
        let target = out.join(&name);
        let source = match (from_lower, from_upper) {
            (from_lower, Some(upper_path)) => {
                let metadata = std::fs::symlink_metadata(&upper_path)?;
                if metadata.file_type().is_char_device() {
                    continue;
                }
                if metadata.is_dir() {
                    let lower_dir = from_lower
                        .filter(|path| path.is_dir())
                        .unwrap_or_else(|| upper_path.join(".kwin-mcp-none"));
                    merge_layers(&lower_dir, &upper_path, &target, budget, leveldb)?;
                    continue;
                }
                upper_path
            }
            (Some(lower_path), None) => lower_path,
            (None, None) => continue,
        };
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            link_tree(&source, &target, budget)?;
        } else if metadata.is_file() {
            let kind = file_kind(&name.to_string_lossy());
            if leveldb && kind == FileKind::Other {
                continue;
            }
            if leveldb && !matches!(kind, FileKind::Table(_)) {
                copy_charged(&source, &target, budget)?;
            } else {
                link_or_copy(&source, &target, budget)?;
            }
        }
    }
    Ok(())
}

// ── Session start and stop ───────────────────────────────────────────────

fn profiles(config: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(config) else {
        return Vec::new();
    };
    let mut profiles: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("Preferences").is_file())
        .collect();
    profiles.sort();
    profiles
}

/// Make each profile's private stores under `root` (on the session's disk,
/// removed with it) and return the overlays that put them in the session.
/// A store that cannot be copied stays as the HOME overlay shows it, and why
/// is among the errors.
pub fn prepare(home: &Path, config_dirs: &[&str], root: &Path) -> Result<Prepared> {
    let state = state_root()?;
    private_dir(root)?;
    let mut budget = COPY_BUDGET;
    let mut records = Vec::new();
    let mut mounts = Vec::new();
    let mut errors = Vec::new();
    for config in config_dirs {
        for profile in profiles(&home.join(config)) {
            let Ok(relative) = profile.strip_prefix(home) else {
                continue;
            };
            let dir = root.join(records.len().to_string());
            private_dir(&dir)?;
            let mut record = ProfileRecord {
                saved: state.join(relative),
                profile,
                dir,
                local_storage: false,
                indexed_db: None,
            };
            let layers = |dir: &Path, name: &str, destination: PathBuf| -> Result<Mount> {
                let upper = dir.join(format!("{name}-upper"));
                let work = dir.join(format!("{name}-work"));
                private_dir(&upper)?;
                private_dir(&work)?;
                Ok(Mount {
                    lower: dir.join(format!("{name}-lower")),
                    upper,
                    work,
                    destination,
                })
            };
            match prepare_local_storage(&record, &mut budget) {
                Ok(true) => {
                    mounts.push(layers(
                        &record.dir,
                        "ls",
                        record.profile.join(LOCAL_STORAGE),
                    )?);
                    record.local_storage = true;
                }
                Ok(false) => {}
                Err(error) => {
                    let _ = std::fs::remove_dir_all(record.dir.join("ls-lower"));
                    errors.push(format!(
                        "{}: {error:#}",
                        record.profile.join(LOCAL_STORAGE).display()
                    ));
                }
            }
            match prepare_indexed_db(&record, &mut budget) {
                Ok(bases) => {
                    mounts.push(layers(&record.dir, "idb", record.profile.join(INDEXED_DB))?);
                    record.indexed_db = Some(bases);
                }
                Err(error) => {
                    let _ = std::fs::remove_dir_all(record.dir.join("idb-lower"));
                    errors.push(format!(
                        "{}: {error:#}",
                        record.profile.join(INDEXED_DB).display()
                    ));
                }
            }
            records.push(record);
        }
    }
    store_json(&root.join(ROOTS_FILE), &records)?;
    Ok(Prepared { mounts, errors })
}

/// Save what the session changed in each profile's page storage. `root` is
/// the directory `prepare` was given; the session's browsers have exited.
pub fn persist(root: &Path) -> Result<()> {
    let records: Vec<ProfileRecord> = load_json(&root.join(ROOTS_FILE))?;
    if records.is_empty() {
        return Ok(());
    }
    let state = state_root()?;
    private_dir(&state)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state.join("lock"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("another session is saving page storage"),
        }
    }
    let mut budget = COPY_BUDGET;
    let mut failures = Vec::new();
    for record in &records {
        if record.local_storage
            && let Err(error) = persist_local_storage(record, &mut budget)
        {
            failures.push(format!(
                "{}: {error:#}",
                record.profile.join(LOCAL_STORAGE).display()
            ));
        }
        if let Some(bases) = &record.indexed_db
            && let Err(error) = persist_indexed_db(record, bases, &mut budget)
        {
            failures.push(format!(
                "{}: {error:#}",
                record.profile.join(INDEXED_DB).display()
            ));
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!(
            "kwin-mcp-page-storage-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn database(path: &Path, entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        let mut db = rusty_leveldb::DB::open(path, rusty_leveldb::Options::default())?;
        for (key, value) in entries {
            db.put(key, value)?;
        }
        db.flush()?;
        Ok(())
    }

    fn key(origin: &str, name: &str) -> Vec<u8> {
        [data_prefix(origin), [&[1u8][..], name.as_bytes()].concat()].concat()
    }

    /// Origin metadata with base::Time `time`, which the tests keep positive.
    fn meta(time: i64) -> Vec<u8> {
        let mut out = vec![8];
        put_varint(&mut out, u64::try_from(time).unwrap_or(0));
        out
    }

    #[test]
    fn a_log_written_here_reads_back_including_records_across_blocks() {
        let records = vec![b"short".to_vec(), vec![7u8; 70_000], b"after".to_vec()];
        assert_eq!(log_records(&log_bytes(&records)), records);
        let mut torn = log_bytes(&records);
        torn.truncate(torn.len() - 2);
        assert_eq!(log_records(&torn), records[..2].to_vec());
    }

    #[test]
    fn the_origin_time_is_field_one_of_its_metadata() {
        assert_eq!(
            meta_time(&meta(13_438_986_614_028_930)),
            Some(13_438_986_614_028_930)
        );
        assert_eq!(meta_time(&[0x10, 0x05]), None);
    }

    #[test]
    fn a_batch_appended_as_a_log_wins_when_leveldb_opens_the_database() -> Result<()> {
        let root = scratch("append")?;
        let db = root.join("db");
        database(
            &db,
            &[
                (key("https://a.example", "token"), b"\x01old".to_vec()),
                (key("https://a.example", "gone"), b"\x01x".to_vec()),
                (b"META:https://a.example".to_vec(), meta(5)),
            ],
        )?;
        append_batch(
            &db,
            &[
                (key("https://a.example", "token"), Some(b"\x01new".to_vec())),
                (key("https://a.example", "gone"), None),
            ],
            0,
        )?;
        let copy = root.join("copy");
        copy_leveldb(&db, &copy, &mut COPY_BUDGET.clone())?;
        let mut reader = Reader::open(&copy)?;
        let entries = reader.entries("https://a.example")?;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(
            entries.get(b"\x01token".as_slice()),
            Some(&b"\x01new".to_vec())
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn the_snapshot_holds_every_table_the_manifest_names() -> Result<()> {
        let root = scratch("snapshot")?;
        let db = root.join("db");
        let entries: Vec<_> = (0..2000)
            .map(|index| {
                (
                    key("https://b.example", &format!("k{index}")),
                    vec![b'v'; 512],
                )
            })
            .collect();
        database(&db, &entries)?;
        let copy = root.join("copy");
        snapshot_leveldb(&db, &copy, &mut COPY_BUDGET.clone())?;
        let state = leveldb_state(&copy)?;
        let present = present_numbers(&copy)?;
        assert!(state.live_tables.is_subset(&present));
        let read = root.join("read");
        copy_leveldb(&copy, &read, &mut COPY_BUDGET.clone())?;
        let entries = Reader::open(&read)?.entries("https://b.example")?;
        assert_eq!(entries.len(), 2000);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    fn saved_key(base: Option<&[u8]>, value: Option<&[u8]>, time: i64) -> SavedKey {
        SavedKey {
            base: base.map(b64),
            value: value.map(b64),
            time,
        }
    }

    #[test]
    fn a_saved_key_applies_where_the_host_kept_the_base_or_changed_it_earlier() -> Result<()> {
        let origin = "https://c.example";
        let mut saved = SavedLocalStorage::default();
        let keys = &mut saved.origins.entry(origin.to_owned()).or_default().keys;
        keys.insert(b64(b"token"), saved_key(Some(b"old"), Some(b"new"), 100));
        keys.insert(b64(b"theme"), saved_key(Some(b"dark"), Some(b"light"), 100));
        keys.insert(b64(b"added"), saved_key(None, Some(b"yes"), 100));
        keys.insert(b64(b"removed"), saved_key(Some(b"v"), None, 100));
        let metas = BTreeMap::from([(origin.to_owned(), meta(200))]);
        let host = BTreeMap::from([
            (b"token".to_vec(), b"old".to_vec()),
            // Changed on the host after the session: the host keeps it.
            (b"theme".to_vec(), b"blue".to_vec()),
            (b"removed".to_vec(), b"v".to_vec()),
        ]);
        let mut operations = plan_local_storage(&saved, &metas, |_| Ok(host.clone()))?;
        operations.sort();
        let prefix = data_prefix(origin);
        assert_eq!(
            operations,
            vec![
                (
                    [prefix.clone(), b"added".to_vec()].concat(),
                    Some(b"yes".to_vec())
                ),
                ([prefix.clone(), b"removed".to_vec()].concat(), None),
                (
                    [prefix.clone(), b"token".to_vec()].concat(),
                    Some(b"new".to_vec())
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn merging_keeps_the_original_base_and_drops_changes_undone() {
        let origin = "https://d.example".to_owned();
        let mut saved = SavedLocalStorage::default();
        let change = |key: &[u8], base: &[u8], value: &[u8], time| {
            BTreeMap::from([(
                origin.clone(),
                OriginChange {
                    meta: Some(meta(time)),
                    access: None,
                    time,
                    keys: vec![(key.to_vec(), Some(base.to_vec()), Some(value.to_vec()))],
                },
            )])
        };
        merge_local_storage(&mut saved, change(b"token", b"v0", b"v1", 10));
        // The next session started from v1 and moved on to v2.
        merge_local_storage(&mut saved, change(b"token", b"v1", b"v2", 20));
        assert_eq!(
            saved.origins[&origin].keys[&b64(b"token")],
            saved_key(Some(b"v0"), Some(b"v2"), 20)
        );
        // A session that started earlier, from the host's v0, stops later with
        // an older change: the newer one stays.
        merge_local_storage(&mut saved, change(b"token", b"v0", b"vx", 15));
        assert_eq!(
            saved.origins[&origin].keys[&b64(b"token")].value,
            Some(b64(b"v2"))
        );
        // Back to the host's value: nothing left to carry.
        merge_local_storage(&mut saved, change(b"token", b"v2", b"v0", 30));
        assert!(saved.origins.is_empty());
    }

    #[test]
    fn merged_layers_show_upper_entries_over_lower_ones() -> Result<()> {
        let root = scratch("layers")?;
        let lower = root.join("lower");
        let upper = root.join("upper");
        std::fs::create_dir_all(lower.join("dir"))?;
        std::fs::create_dir_all(upper.join("dir"))?;
        for (layer, file) in [
            (&lower, "kept"),
            (&lower, "replaced"),
            (&lower, "dir/inner"),
            (&upper, "replaced"),
            (&upper, "dir/new"),
        ] {
            let name = if *layer == lower { "lower" } else { "upper" };
            std::fs::write(layer.join(file), name)?;
        }
        let out = root.join("out");
        merge_layers(&lower, &upper, &out, &mut COPY_BUDGET.clone(), false)?;
        for (file, expected) in [
            ("kept", "lower"),
            ("replaced", "upper"),
            ("dir/inner", "lower"),
            ("dir/new", "upper"),
        ] {
            assert_eq!(std::fs::read_to_string(out.join(file))?, expected, "{file}");
        }
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
