//! A session-local KWallet for the isolated session.
//!
//! Session apps (Chrome with --password-store=kwallet6) talk to
//! `org.kde.kwalletd6` on a private bus served by this module. The session
//! never reaches the host's wallet or Secret Service: at `session_start` one
//! guarded, read-only snapshot of the host wallet is taken, and from then on
//! every call is answered from memory. Writes land in the session's copy only
//! and vanish with the session.
//!
//! The snapshot is taken so that it cannot disturb the host:
//!
//! - it never starts a service: `org.kde.ksecretd` must already own its name,
//!   and every call is sent with NoAutoStart;
//! - it never unlocks or prompts: a locked wallet is refused, and the lock
//!   state comes from `org.kde.ksecretd` only, never from
//!   `org.freedesktop.secrets` (gnome-keyring aborts when a short-lived
//!   client disconnects mid-request);
//! - snapshots are serialized across every kwin-mcp server on the host and
//!   spaced apart, so many sessions starting together make no burst;
//! - it does not depend on kwalletd6 being healthy: when kwalletd6 is missing
//!   or does not answer, the same wallet is read from ksecretd directly with
//!   the standard Secret Service calls (a wedged kwalletd6 after a user-bus
//!   restart otherwise left every session without its Chrome Safe Storage key,
//!   and Chrome then drops every cookie it cannot decrypt).

use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::message::Type as MessageType;
use zbus::zvariant::Value;

pub const SERVICE: &str = "org.kde.kwalletd6";
pub const PATH: &str = "/modules/kwalletd6";
pub const INTERFACE: &str = "org.kde.KWallet";
/// The only Secret Service the lock state is read from.
const SECRET_BACKEND: &str = "org.kde.ksecretd";
/// App id the snapshot's own handle is opened under.
const SNAPSHOT_APP: &str = "kwin-mcp-snapshot";
/// Bound for each host call during the snapshot.
const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for the whole snapshot, lock wait excluded.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for another server's snapshot to finish.
const LOCK_WAIT: Duration = Duration::from_secs(40);
/// Minimum gap between two snapshots on this host.
const SNAPSHOT_SPACING: Duration = Duration::from_secs(5);
/// Limits that keep a runaway wallet from filling the server's memory.
const MAX_ENTRIES: usize = 20_000;
const MAX_BYTES: usize = 64 << 20;

/// KWallet entry types.
const TYPE_UNKNOWN: i32 = 0;
const TYPE_PASSWORD: i32 = 1;
const TYPE_STREAM: i32 = 2;
const TYPE_MAP: i32 = 3;

#[derive(Clone)]
struct Entry {
    kind: i32,
    data: Vec<u8>,
}

/// The wallet the session sees: the host snapshot plus the session's own writes.
#[derive(Clone, Default)]
pub struct Wallet {
    name: String,
    folders: BTreeMap<String, BTreeMap<String, Entry>>,
}

impl Wallet {
    pub fn entry_count(&self) -> usize {
        self.folders.values().map(BTreeMap::len).sum()
    }

    fn entry(&self, folder: &str, key: &str) -> Option<&Entry> {
        self.folders.get(folder)?.get(key)
    }

    fn write(&mut self, folder: &str, key: &str, kind: i32, data: Vec<u8>) -> bool {
        match self.folders.get_mut(folder) {
            Some(entries) => {
                entries.insert(key.to_owned(), Entry { kind, data });
                true
            }
            None => false,
        }
    }
}

/// Outcome of the host snapshot, reported by session_start.
pub struct Snapshot {
    pub wallet: Option<Wallet>,
    pub reason: String,
}

#[derive(Default)]
struct State {
    wallet: Option<Wallet>,
    /// Handles the session opened, with the app id each belongs to.
    handles: HashMap<i32, String>,
    next_handle: i32,
    next_transaction: i32,
}

pub struct WalletMediator {
    state: Arc<Mutex<State>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for WalletMediator {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

type HostResult<T> = zbus::Result<T>;

async fn host_call<B, R>(proxy: &zbus::Proxy<'_>, method: &str, body: &B) -> HostResult<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    tokio::time::timeout(
        HOST_CALL_TIMEOUT,
        proxy.call_with_flags::<_, _, R>(method, zbus::proxy::MethodFlags::NoAutoStart.into(), body),
    )
    .await
    .map_err(|_| zbus::Error::Failure(format!("{method}: the host service did not answer in {HOST_CALL_TIMEOUT:?}")))??
    .ok_or_else(|| zbus::Error::Failure(format!("{method}: no reply")))
}

async fn name_has_owner(host: &zbus::Connection, name: &str) -> bool {
    let Ok(proxy) = zbus::fdo::DBusProxy::new(host).await else { return false };
    let Ok(name) = zbus::names::BusName::try_from(name) else { return false };
    proxy.name_has_owner(name).await.unwrap_or(false)
}

/// Serializes snapshots across processes with a lock file in the user runtime
/// directory, and spaces them SNAPSHOT_SPACING apart.
struct HostSlot {
    file: nix::fcntl::Flock<std::fs::File>,
}

impl HostSlot {
    async fn take() -> Result<Self, String> {
        let directory = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?
            .join("kwin-mcp");
        std::fs::create_dir_all(&directory).map_err(|error| format!("create {}: {error}", directory.display()))?;
        let path = directory.join("wallet-snapshot.lock");
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        loop {
            match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
                Ok(locked) => {
                    let slot = Self { file: locked };
                    let since = slot.file.metadata().and_then(|meta| meta.modified()).ok()
                        .and_then(|time| time.elapsed().ok());
                    if let Some(wait) = since.and_then(|elapsed| SNAPSHOT_SPACING.checked_sub(elapsed)) {
                        tokio::time::sleep(wait).await;
                    }
                    return Ok(slot);
                }
                Err((returned, nix::errno::Errno::EWOULDBLOCK)) => {
                    if std::time::Instant::now() >= deadline {
                        return Err("another kwin-mcp server held the wallet snapshot slot too long".to_owned());
                    }
                    file = returned;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err((_, error)) => return Err(format!("lock {}: {error}", path.display())),
            }
        }
    }
}

impl Drop for HostSlot {
    fn drop(&mut self) {
        // The modification time is what spaces the next snapshot.
        let _ = self.file.set_modified(std::time::SystemTime::now());
    }
}

fn refused(reason: impl Into<String>) -> Snapshot {
    Snapshot { wallet: None, reason: reason.into() }
}

/// Set to read the host wallet from ksecretd only (diagnostics).
const FROM_SECRET_SERVICE_ENV: &str = "KWIN_MCP_WALLET_FROM_SECRET_SERVICE";

/// Why one way of reading the host wallet did not work.
enum Failure {
    /// A decision, not a fault: the wallet is disabled, locked or not listed.
    /// No other way of reading it is tried.
    Refused(String),
    /// kwalletd6 is missing, wedged or erroring. ksecretd, the Secret Service
    /// behind it, may still answer, so the copy is tried from there.
    Unreachable(String),
}

fn unreachable_error(what: &str) -> impl FnOnce(zbus::Error) -> Failure + '_ {
    move |error| Failure::Unreachable(format!("{what}: {error}"))
}

/// Take the one read-only snapshot of the host wallet, or say why not.
pub async fn snapshot(host: &zbus::Connection) -> Snapshot {
    let slot = match HostSlot::take().await {
        Ok(slot) => slot,
        Err(reason) => return refused(reason),
    };
    let outcome = snapshot_locked(host).await;
    drop(slot);
    outcome
}

async fn snapshot_locked(host: &zbus::Connection) -> Snapshot {
    // Diagnostic switch: read from ksecretd even while kwalletd6 answers, to
    // exercise the fallback.
    let why = if std::env::var_os(FROM_SECRET_SERVICE_ENV).is_some() {
        format!("{FROM_SECRET_SERVICE_ENV} is set")
    } else {
        match tokio::time::timeout(SNAPSHOT_TIMEOUT, read_via_kwalletd(host)).await {
            Ok(Ok(snapshot)) => return snapshot,
            Ok(Err(Failure::Refused(reason))) => return refused(reason),
            Ok(Err(Failure::Unreachable(reason))) => reason,
            Err(_) => "host kwalletd6 snapshot timed out".to_owned(),
        }
    };
    match tokio::time::timeout(SNAPSHOT_TIMEOUT, read_via_secret_service(host, &why)).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(Failure::Refused(reason) | Failure::Unreachable(reason))) => refused(format!("{why}; {reason}")),
        Err(_) => refused(format!("{why}; reading ksecretd timed out")),
    }
}

async fn read_via_kwalletd(host: &zbus::Connection) -> Result<Snapshot, Failure> {
    if !name_has_owner(host, SECRET_BACKEND).await {
        return Err(Failure::Refused(format!("{SECRET_BACKEND} is not running on the host; kwin-mcp does not start it")));
    }
    if !name_has_owner(host, SERVICE).await {
        return Err(Failure::Unreachable(format!("{SERVICE} is not running on the host; kwin-mcp does not start it")));
    }
    let proxy = zbus::Proxy::new(host, SERVICE, PATH, INTERFACE).await.map_err(unreachable_error("host kwalletd6 proxy"))?;
    match host_call::<_, bool>(&proxy, "isEnabled", &()).await {
        Ok(true) => {}
        Ok(false) => return Err(Failure::Refused("KWallet is disabled on the host".to_owned())),
        Err(error) => return Err(Failure::Unreachable(format!("host kwalletd6 unavailable: {error}"))),
    }
    let name: String = host_call(&proxy, "networkWallet", &()).await.map_err(unreachable_error("networkWallet"))?;
    let listed: Vec<String> = host_call(&proxy, "wallets", &()).await.map_err(unreachable_error("wallets"))?;
    if !listed.contains(&name) {
        return Err(Failure::Refused(format!("host wallet '{name}' is not listed by kwalletd6; kwin-mcp does not create it")));
    }
    match find_collection(host, &name).await {
        Some((_, false)) => {}
        Some((_, true)) => return Err(Failure::Refused(format!("host wallet '{name}' is locked; kwin-mcp does not unlock it"))),
        None => return Err(Failure::Refused(format!("host Secret Service has no readable collection for '{name}'"))),
    }
    let handle: i32 = match host_call(&proxy, "open", &(name.as_str(), 0i64, SNAPSHOT_APP)).await {
        Ok(handle) if handle >= 0 => handle,
        Ok(_) => return Err(Failure::Unreachable("host kwalletd6 refused to open the wallet".to_owned())),
        Err(error) => return Err(Failure::Unreachable(format!("open: {error}"))),
    };
    let copied = copy_entries(&proxy, handle, &name).await;
    // The snapshot's own handle is always released, whatever the copy did.
    let _ = host_call::<_, i32>(&proxy, "close", &(handle, false, SNAPSHOT_APP)).await;
    let wallet = copied.map_err(Failure::Unreachable)?;
    let reason = format!("session-local copy of host wallet '{name}': {} entries", wallet.entry_count());
    Ok(Snapshot { wallet: Some(wallet), reason })
}

/// The collection holding `wallet`, and whether it is locked.
async fn find_collection(host: &zbus::Connection, wallet: &str) -> Option<(zbus::zvariant::OwnedObjectPath, bool)> {
    let service = zbus::Proxy::new(host, SECRET_BACKEND, "/org/freedesktop/secrets", "org.freedesktop.Secret.Service").await.ok()?;
    let collections = service.get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("Collections").await.ok()?;
    for path in collections {
        let Ok(collection) = zbus::Proxy::new(host, SECRET_BACKEND, path.clone(), "org.freedesktop.Secret.Collection").await else {
            continue;
        };
        let label = collection.get_property::<String>("Label").await.unwrap_or_default();
        if label == wallet || path.as_str().rsplit('/').next() == Some(wallet) {
            let locked = collection.get_property::<bool>("Locked").await.ok()?;
            return Some((path, locked));
        }
    }
    None
}

/// Wallet the host's own default points at when kwalletd6 cannot be asked.
const DEFAULT_WALLET: &str = "kdewallet";

/// Read the wallet straight from ksecretd with the standard Secret Service
/// calls (OpenSession "plain", GetSecrets). It never unlocks anything: a
/// locked collection is refused, and GetSecrets on an unlocked one cannot
/// prompt. kwalletd6 is a thin layer over these items: the KWallet folder is
/// the item's `server` attribute, the entry name its `user`, and `type` is
/// plaintext, binary or map.
async fn read_via_secret_service(host: &zbus::Connection, why: &str) -> Result<Snapshot, Failure> {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedObjectPath, Value};
    if !name_has_owner(host, SECRET_BACKEND).await {
        return Err(Failure::Refused(format!("{SECRET_BACKEND} is not running on the host; kwin-mcp does not start it")));
    }
    let (collection_path, locked) = find_collection(host, DEFAULT_WALLET)
        .await
        .ok_or_else(|| Failure::Refused(format!("{SECRET_BACKEND} has no readable collection '{DEFAULT_WALLET}'")))?;
    if locked {
        return Err(Failure::Refused(format!("host wallet '{DEFAULT_WALLET}' is locked; kwin-mcp does not unlock it")));
    }
    let service = zbus::Proxy::new(host, SECRET_BACKEND, "/org/freedesktop/secrets", "org.freedesktop.Secret.Service")
        .await
        .map_err(unreachable_error("ksecretd proxy"))?;
    let collection = zbus::Proxy::new(host, SECRET_BACKEND, collection_path.clone(), "org.freedesktop.Secret.Collection")
        .await
        .map_err(unreachable_error("ksecretd collection proxy"))?;
    let items: Vec<OwnedObjectPath> = collection.get_property("Items").await.map_err(unreachable_error("Items"))?;
    if items.len() > MAX_ENTRIES {
        return Err(Failure::Refused("host wallet is too large to copy into a session".to_owned()));
    }
    let (_, session): (zbus::zvariant::OwnedValue, OwnedObjectPath) =
        host_call(&service, "OpenSession", &("plain", Value::new(String::new()))).await.map_err(unreachable_error("OpenSession"))?;
    let secrets = host_call::<_, HashMap<OwnedObjectPath, (OwnedObjectPath, Vec<u8>, Vec<u8>, String)>>(&service, "GetSecrets", &(&items, &session)).await;
    let mut found: Vec<(u64, String, String, Entry)> = Vec::new();
    let mut result: Result<(), Failure> = Ok(());
    match secrets {
        Err(error) => result = Err(Failure::Unreachable(format!("GetSecrets: {error}"))),
        Ok(secrets) => {
            for (path, (_, _, value, _)) in secrets {
                let Ok(item) = zbus::Proxy::new(host, SECRET_BACKEND, path.clone(), "org.freedesktop.Secret.Item").await else { continue };
                let attributes: HashMap<String, String> = match item.get_property("Attributes").await {
                    Ok(attributes) => attributes,
                    Err(_) => continue,
                };
                let (Some(folder), Some(key)) = (attributes.get("server"), attributes.get("user")) else { continue };
                let modified: u64 = item.get_property("Modified").await.unwrap_or(0);
                let kind = match attributes.get("type").map(String::as_str) {
                    Some("plaintext") => TYPE_PASSWORD,
                    Some("map") => TYPE_MAP,
                    Some("binary") => TYPE_STREAM,
                    _ if std::str::from_utf8(&value).is_ok() => TYPE_PASSWORD,
                    _ => TYPE_STREAM,
                };
                found.push((modified, folder.clone(), key.clone(), Entry { kind, data: value }));
            }
        }
    }
    // The session is closed whatever the read did.
    if let Ok(proxy) = zbus::Proxy::new(host, SECRET_BACKEND, session, "org.freedesktop.Secret.Session").await {
        let _ = host_call::<_, ()>(&proxy, "Close", &()).await;
    }
    result?;
    // Two items can share a folder and name (the host wallet has two "Chrome
    // Safe Storage" items, written by different clients): the most recently
    // modified one is used.
    found.sort_by_key(|(modified, ..)| *modified);
    let mut wallet = Wallet { name: DEFAULT_WALLET.to_owned(), folders: BTreeMap::new() };
    let (mut bytes, mut conflicts) = (0usize, 0usize);
    for (_, folder, key, entry) in found {
        bytes += entry.data.len();
        if bytes > MAX_BYTES {
            return Err(Failure::Refused("host wallet is too large to copy into a session".to_owned()));
        }
        let previous = wallet.folders.entry(folder).or_default().insert(key, entry.clone());
        conflicts += usize::from(previous.is_some_and(|previous| previous.data != entry.data));
    }
    let reason = format!(
        "session-local copy of host wallet '{DEFAULT_WALLET}' read from ksecretd because {why}: {} entries{}",
        wallet.entry_count(),
        if conflicts > 0 { format!(" ({conflicts} duplicate name(s) held different values; the newest was used)") } else { String::new() }
    );
    Ok(Snapshot { wallet: Some(wallet), reason })
}

async fn copy_entries(proxy: &zbus::Proxy<'_>, handle: i32, name: &str) -> Result<Wallet, String> {
    let describe = |what: &str, error: zbus::Error| format!("{what}: {error}");
    let mut wallet = Wallet { name: name.to_owned(), folders: BTreeMap::new() };
    let (mut entries, mut bytes) = (0usize, 0usize);
    // kwalletd6 over a Secret Service backend lists a folder once per item it
    // holds (2848 names for 17 folders on the dev machine), so names are
    // deduplicated before each is read.
    let folders: std::collections::BTreeSet<String> = host_call::<_, Vec<String>>(proxy, "folderList", &(handle, SNAPSHOT_APP)).await.map_err(|e| describe("folderList", e))?.into_iter().collect();
    for folder in folders {
        let keys: std::collections::BTreeSet<String> = host_call::<_, Vec<String>>(proxy, "entryList", &(handle, folder.as_str(), SNAPSHOT_APP)).await.map_err(|e| describe("entryList", e))?.into_iter().collect();
        let mut copy = BTreeMap::new();
        for key in keys {
            let args = (handle, folder.as_str(), key.as_str(), SNAPSHOT_APP);
            let kind: i32 = host_call(proxy, "entryType", &args).await.map_err(|e| describe("entryType", e))?;
            let data: Vec<u8> = match kind {
                TYPE_PASSWORD => host_call::<_, String>(proxy, "readPassword", &args).await.map_err(|e| describe("readPassword", e))?.into_bytes(),
                TYPE_MAP => host_call(proxy, "readMap", &args).await.map_err(|e| describe("readMap", e))?,
                _ => host_call(proxy, "readEntry", &args).await.map_err(|e| describe("readEntry", e))?,
            };
            entries += 1;
            bytes += data.len();
            if entries > MAX_ENTRIES || bytes > MAX_BYTES {
                return Err("host wallet is too large to copy into a session".to_owned());
            }
            copy.insert(key, Entry { kind, data });
        }
        wallet.folders.insert(folder, copy);
    }
    Ok(wallet)
}

impl WalletMediator {
    /// Serve org.kde.kwalletd6 on the private bus at `bus_address` from
    /// `wallet`. None answers as a disabled KWallet.
    pub async fn start(bus_address: &str, wallet: Option<Wallet>) -> zbus::Result<Self> {
        let session = zbus::connection::Builder::address(bus_address)?
            .name(SERVICE)?
            .build()
            .await?;
        let state = Arc::new(Mutex::new(State { wallet, next_handle: 1, next_transaction: 1, ..State::default() }));
        let mut calls = zbus::MessageStream::from(&session);
        let task = {
            let (session, state) = (session.clone(), state.clone());
            tokio::spawn(async move {
                while let Some(Ok(message)) = calls.next().await {
                    if message.message_type() != MessageType::MethodCall {
                        continue;
                    }
                    let (session, state) = (session.clone(), state.clone());
                    tokio::spawn(async move {
                        if let Err(error) = handle_call(&session, &state, &message).await {
                            eprintln!("session wallet: {error}");
                            let _ = session
                                .reply_error(&message.header(), "org.freedesktop.DBus.Error.Failed", &error.to_string())
                                .await;
                        }
                    });
                }
            })
        };
        Ok(Self { state, tasks: vec![task] })
    }

    /// Stop serving and drop the session's wallet. Nothing on the host is
    /// touched.
    pub async fn shutdown(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        if let Ok(mut state) = self.state.lock() {
            *state = State::default();
        }
    }
}

const INTROSPECTION: &str = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\"><node><interface name=\"org.kde.KWallet\"/></node>";

/// The wallet behind `handle` when `app` owns it.
fn owned<'a>(state: &'a mut State, handle: i32, app: &str) -> Option<&'a mut Wallet> {
    if state.handles.get(&handle).is_some_and(|owner| owner == app) {
        state.wallet.as_mut()
    } else {
        None
    }
}

async fn handle_call(session: &zbus::Connection, state: &Arc<Mutex<State>>, message: &zbus::Message) -> zbus::Result<()> {
    let header = message.header();
    let member = header.member().map(|member| member.to_string()).unwrap_or_default();
    let interface = header.interface().map(|interface| interface.to_string()).unwrap_or_default();
    match (interface.as_str(), member.as_str()) {
        ("org.freedesktop.DBus.Introspectable", "Introspect") => return session.reply(&header, &INTROSPECTION).await,
        ("org.freedesktop.DBus.Peer", "Ping") => return session.reply(&header, &()).await,
        ("org.freedesktop.DBus.Peer", "GetMachineId") => {
            let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
            return session.reply(&header, &id.trim()).await;
        }
        _ => {}
    }
    if header.path().map(|path| path.as_str()) != Some(PATH) || (interface != INTERFACE && !interface.is_empty()) {
        return session
            .reply_error(&header, "org.freedesktop.DBus.Error.UnknownObject", &"kwin-mcp exposes only org.kde.KWallet at /modules/kwalletd6")
            .await;
    }
    let body = message.body();
    let signature = body.signature().to_string();
    let deny = |reason: &str| format!("kwin-mcp: {member} {reason}");
    let bad = |error: zbus::Error| zbus::Error::Failure(format!("{member}: {error}"));

    // Replies are built under the lock and sent after it is released.
    enum Reply {
        Bool(bool),
        Int(i32),
        Text(String),
        Bytes(Vec<u8>),
        List(Vec<String>),
        Dict(HashMap<String, Value<'static>>),
        Denied,
        /// An asynchronous open: the transaction id, then the result signal.
        Async(i32, i32),
    }
    let wallet_name = state
        .lock()
        .map_err(|_| zbus::Error::Failure("wallet state poisoned".to_owned()))?
        .wallet
        .as_ref()
        .map(|wallet| wallet.name.clone());
    let Some(name) = wallet_name else {
        return match member.as_str() {
            "isEnabled" => session.reply(&header, &false).await,
            "wallets" => session.reply(&header, &Vec::<String>::new()).await,
            "open" | "openPath" => session.reply(&header, &-1i32).await,
            _ => session.reply_error(&header, "org.freedesktop.DBus.Error.AccessDenied", &deny("is unavailable: the host wallet is not copied into this session")).await,
        };
    };
    let reply = {
        let mut state = state.lock().map_err(|_| zbus::Error::Failure("wallet state poisoned".to_owned()))?;
        match member.as_str() {
            "isEnabled" => Reply::Bool(true),
            "networkWallet" | "localWallet" => Reply::Text(name),
            "wallets" => Reply::List(vec![name]),
            "users" => Reply::List(Vec::new()),
            "isOpen" if signature == "s" => {
                let requested: String = body.deserialize().map_err(bad)?;
                Reply::Bool(requested == name && !state.handles.is_empty())
            }
            "isOpen" => {
                let handle: i32 = body.deserialize().map_err(bad)?;
                Reply::Bool(state.handles.contains_key(&handle))
            }
            "open" | "openAsync" => {
                let (requested, app, asynchronous) = if member == "open" {
                    let (requested, _window, app): (String, i64, String) = body.deserialize().map_err(bad)?;
                    (requested, app, false)
                } else {
                    let (requested, _window, app, _session): (String, i64, String, bool) = body.deserialize().map_err(bad)?;
                    (requested, app, true)
                };
                if requested != name {
                    Reply::Int(-1)
                } else {
                    let handle = state.next_handle;
                    state.next_handle += 1;
                    state.handles.insert(handle, app);
                    if asynchronous {
                        let transaction = state.next_transaction;
                        state.next_transaction += 1;
                        Reply::Async(transaction, handle)
                    } else {
                        Reply::Int(handle)
                    }
                }
            }
            "openPath" | "openPathAsync" => Reply::Int(-1),
            "close" if signature == "ibs" => {
                let (handle, _force, app): (i32, bool, String) = body.deserialize().map_err(bad)?;
                if state.handles.get(&handle).is_some_and(|owner| *owner == app) {
                    state.handles.remove(&handle);
                    Reply::Int(0)
                } else {
                    Reply::Int(-1)
                }
            }
            "close" => Reply::Int(-1),
            "folderList" => {
                let (handle, app): (i32, String) = body.deserialize().map_err(bad)?;
                Reply::List(owned(&mut state, handle, &app).map(|wallet| wallet.folders.keys().cloned().collect()).unwrap_or_default())
            }
            "hasFolder" => {
                let (handle, folder, app): (i32, String, String) = body.deserialize().map_err(bad)?;
                Reply::Bool(owned(&mut state, handle, &app).is_some_and(|wallet| wallet.folders.contains_key(&folder)))
            }
            "entryList" => {
                let (handle, folder, app): (i32, String, String) = body.deserialize().map_err(bad)?;
                Reply::List(owned(&mut state, handle, &app)
                    .and_then(|wallet| wallet.folders.get(&folder))
                    .map(|entries| entries.keys().cloned().collect())
                    .unwrap_or_default())
            }
            "hasEntry" | "entryType" | "readPassword" | "readEntry" | "readMap" => {
                let (handle, folder, key, app): (i32, String, String, String) = body.deserialize().map_err(bad)?;
                let entry = owned(&mut state, handle, &app).and_then(|wallet| wallet.entry(&folder, &key)).cloned();
                match member.as_str() {
                    "hasEntry" => Reply::Bool(entry.is_some()),
                    "entryType" => Reply::Int(entry.map_or(TYPE_UNKNOWN, |entry| entry.kind)),
                    "readPassword" => Reply::Text(entry.map(|entry| String::from_utf8_lossy(&entry.data).into_owned()).unwrap_or_default()),
                    _ => Reply::Bytes(entry.map(|entry| entry.data).unwrap_or_default()),
                }
            }
            "entriesList" | "mapList" | "passwordList" => {
                let (handle, folder, app): (i32, String, String) = body.deserialize().map_err(bad)?;
                let wanted = match member.as_str() {
                    "mapList" => Some(TYPE_MAP),
                    "passwordList" => Some(TYPE_PASSWORD),
                    _ => None,
                };
                let mut dict = HashMap::new();
                if let Some(entries) = owned(&mut state, handle, &app).and_then(|wallet| wallet.folders.get(&folder)) {
                    for (key, entry) in entries.iter().filter(|(_, entry)| wanted.is_none_or(|kind| entry.kind == kind)) {
                        let value = if entry.kind == TYPE_PASSWORD && wanted == Some(TYPE_PASSWORD) {
                            Value::from(String::from_utf8_lossy(&entry.data).into_owned())
                        } else {
                            Value::from(entry.data.clone())
                        };
                        dict.insert(key.clone(), value);
                    }
                }
                Reply::Dict(dict)
            }
            // Writes change the session's copy only.
            "createFolder" => {
                let (handle, folder, app): (i32, String, String) = body.deserialize().map_err(bad)?;
                Reply::Bool(owned(&mut state, handle, &app).is_some_and(|wallet| {
                    wallet.folders.entry(folder).or_default();
                    true
                }))
            }
            "removeFolder" => {
                let (handle, folder, app): (i32, String, String) = body.deserialize().map_err(bad)?;
                Reply::Bool(owned(&mut state, handle, &app).is_some_and(|wallet| wallet.folders.remove(&folder).is_some()))
            }
            "writePassword" => {
                let (handle, folder, key, value, app): (i32, String, String, String, String) = body.deserialize().map_err(bad)?;
                let done = owned(&mut state, handle, &app).is_some_and(|wallet| wallet.write(&folder, &key, TYPE_PASSWORD, value.into_bytes()));
                Reply::Int(if done { 0 } else { -1 })
            }
            "writeEntry" | "writeMap" => {
                let (handle, folder, key, value, kind, app): (i32, String, String, Vec<u8>, i32, String) = if signature == "issayis" {
                    body.deserialize().map_err(bad)?
                } else {
                    let (handle, folder, key, value, app): (i32, String, String, Vec<u8>, String) = body.deserialize().map_err(bad)?;
                    (handle, folder, key, value, if member == "writeMap" { TYPE_MAP } else { TYPE_STREAM }, app)
                };
                let done = owned(&mut state, handle, &app).is_some_and(|wallet| wallet.write(&folder, &key, kind, value));
                Reply::Int(if done { 0 } else { -1 })
            }
            "removeEntry" => {
                let (handle, folder, key, app): (i32, String, String, String) = body.deserialize().map_err(bad)?;
                let done = owned(&mut state, handle, &app).is_some_and(|wallet| {
                    wallet.folders.get_mut(&folder).is_some_and(|entries| entries.remove(&key).is_some())
                });
                Reply::Int(if done { 0 } else { -1 })
            }
            _ => Reply::Denied,
        }
    };
    match reply {
        Reply::Bool(value) => session.reply(&header, &value).await,
        Reply::Int(value) => session.reply(&header, &value).await,
        Reply::Text(value) => session.reply(&header, &value).await,
        Reply::Bytes(value) => session.reply(&header, &value).await,
        Reply::List(value) => session.reply(&header, &value).await,
        Reply::Dict(value) => session.reply(&header, &value).await,
        Reply::Denied => {
            session
                .reply_error(&header, "org.freedesktop.DBus.Error.AccessDenied", &deny("is not available in the session-local wallet"))
                .await
        }
        Reply::Async(transaction, handle) => {
            // The app learns its transaction id first, then the result.
            session.reply(&header, &transaction).await?;
            session.emit_signal(None::<&str>, PATH, INTERFACE, "walletAsyncOpened", &(transaction, handle)).await?;
            session.emit_signal(None::<&str>, PATH, INTERFACE, "walletOpened", &(handle,)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet() -> Wallet {
        let mut wallet = Wallet { name: "kdewallet".to_owned(), folders: BTreeMap::new() };
        wallet.folders.entry("Chrome Keys".to_owned()).or_default();
        assert!(wallet.write("Chrome Keys", "Chrome Safe Storage", TYPE_PASSWORD, b"secret".to_vec()));
        wallet
    }

    #[test]
    fn writes_need_an_existing_folder_and_stay_in_the_copy() {
        let mut wallet = wallet();
        assert!(!wallet.write("Other", "key", TYPE_STREAM, vec![1]));
        assert_eq!(wallet.entry_count(), 1);
        assert!(wallet.write("Chrome Keys", "extra", TYPE_MAP, vec![2, 3]));
        assert_eq!(wallet.entry_count(), 2);
        assert_eq!(wallet.entry("Chrome Keys", "extra").map(|entry| entry.kind), Some(TYPE_MAP));
    }

    #[test]
    fn handles_belong_to_the_app_that_opened_them() {
        let mut state = State { wallet: Some(wallet()), next_handle: 1, ..State::default() };
        state.handles.insert(7, "chrome".to_owned());
        assert!(owned(&mut state, 7, "chrome").is_some());
        assert!(owned(&mut state, 7, "other").is_none());
        assert!(owned(&mut state, 8, "chrome").is_none());
    }
}
