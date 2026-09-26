//! KWallet access for the isolated session.
//!
//! Session apps (Chrome with --password-store=kwallet6) reach the host wallet
//! through a filtered xdg-dbus-proxy. KWallet 6's kwalletd is a thin layer over
//! the Secret Service with two host-safety problems for such clients:
//!
//! - `open` on a wallet the provider does not list calls `createCollection`,
//!   so a session whose host provider is unhealthy or mismatched creates an
//!   empty wallet that host apps then see instead of their entries.
//! - Handles are never released on client disconnect, and `close(handle,
//!   false, appId)` returns 1 without releasing, so every session leaks one
//!   host handle per app per run.
//!
//! The proxy's upstream is therefore a private bus where this mediator owns
//! `org.kde.kwalletd6`. It forwards the read-only KWallet calls to the host,
//! lets `open` through only for a wallet the host already lists and has
//! unlocked, records each handle the session opens, turns the session's own
//! `close` into a real release, and force-closes whatever is left at teardown.

use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::message::Type as MessageType;
use zbus::zvariant::Structure;

pub const SERVICE: &str = "org.kde.kwalletd6";
pub const PATH: &str = "/modules/kwalletd6";
pub const INTERFACE: &str = "org.kde.KWallet";
/// Bound for each forwarded host call and for teardown releases.
const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// KWallet methods that only read host state; forwarded unchanged.
const READ_METHODS: &[&str] = &[
    "isEnabled", "networkWallet", "localWallet", "wallets", "isOpen", "folderList", "hasFolder",
    "entryList", "entriesList", "hasEntry", "entryType", "readEntry", "readMap", "readPassword",
    "mapList", "passwordList", "users", "readEntryList", "readMapList", "readPasswordList",
];
const OPEN_METHODS: &[&str] = &["open", "openPath"];
const OPEN_ASYNC_METHODS: &[&str] = &["openAsync", "openPathAsync"];

#[derive(Default)]
struct State {
    /// Host handles opened for the session, with the app id they belong to.
    handles: HashSet<(i32, String)>,
    /// openAsync transactions awaiting walletAsyncOpened, by transaction id.
    pending: HashMap<i32, String>,
}

/// Outcome of the host wallet checks, reported by session_start.
pub struct Preflight {
    pub wallet: Option<String>,
    pub reason: String,
}

pub struct WalletMediator {
    host: zbus::Connection,
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

/// Why the host wallet may not be forwarded, or None when it may be.
async fn wallet_refusal(host: &zbus::Connection, wallet: &str) -> Option<String> {
    let wallets: Vec<String> = match call(host, "wallets", &()).await {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(error) => return Some(format!("host kwalletd6 unavailable: {error}")),
    };
    if !wallets.iter().any(|name| name == wallet) {
        return Some(format!(
            "host wallet '{wallet}' is not listed by kwalletd6 ({:?}); refusing to let the session create it",
            wallets
        ));
    }
    match collection_locked(host, wallet).await {
        Some(false) => None,
        Some(true) => Some(format!("host wallet '{wallet}' is locked")),
        None => Some(format!("host Secret Service has no collection for '{wallet}'")),
    }
}

/// Locked state of the wallet's Secret Service collection: ksecretd first
/// (what kwalletd6 uses), then the org.freedesktop.secrets owner.
async fn collection_locked(host: &zbus::Connection, wallet: &str) -> Option<bool> {
    for service in ["org.kde.ksecretd", "org.freedesktop.secrets"] {
        let Ok(proxy) = zbus::Proxy::new(host, service, "/org/freedesktop/secrets", "org.freedesktop.Secret.Service").await else {
            continue;
        };
        let Ok(collections) = proxy.get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("Collections").await else {
            continue;
        };
        for path in collections {
            let Ok(collection) = zbus::Proxy::new(host, service, path.clone(), "org.freedesktop.Secret.Collection").await else {
                continue;
            };
            let label = collection.get_property::<String>("Label").await.unwrap_or_default();
            if label == wallet || path.as_str().rsplit('/').next() == Some(wallet) {
                return collection.get_property::<bool>("Locked").await.ok();
            }
        }
    }
    None
}

async fn call<B>(host: &zbus::Connection, method: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    tokio::time::timeout(
        HOST_CALL_TIMEOUT,
        host.call_method(Some(SERVICE), PATH, Some(INTERFACE), method, body),
    )
    .await
    .map_err(|_| zbus::Error::Failure(format!("{method}: host kwalletd6 did not answer")))?
}

/// Check the host wallet before exposing it: KWallet enabled, network wallet
/// listed by kwalletd6 (so opening it cannot create a collection), and its
/// Secret Service collection present and unlocked.
pub async fn preflight(host: &zbus::Connection) -> Preflight {
    let enabled = match call(host, "isEnabled", &()).await {
        Ok(reply) => reply.body().deserialize::<bool>().unwrap_or(false),
        Err(error) => return Preflight { wallet: None, reason: format!("host kwalletd6 unavailable: {error}") },
    };
    if !enabled {
        return Preflight { wallet: None, reason: "KWallet is disabled on the host".to_owned() };
    }
    let wallet: String = match call(host, "networkWallet", &()).await {
        Ok(reply) => reply.body().deserialize().unwrap_or_default(),
        Err(error) => return Preflight { wallet: None, reason: format!("networkWallet: {error}") },
    };
    match wallet_refusal(host, &wallet).await {
        None => Preflight { reason: format!("forwarding host wallet '{wallet}' read-only"), wallet: Some(wallet) },
        Some(reason) => Preflight { wallet: None, reason },
    }
}

impl WalletMediator {
    /// Serve org.kde.kwalletd6 on the private bus at `bus_address`,
    /// forwarding to `host`. `wallet` is None when preflight refused
    /// forwarding: the service then answers as a disabled KWallet.
    pub async fn start(bus_address: &str, host: zbus::Connection, wallet: Option<String>) -> zbus::Result<Self> {
        let session = zbus::connection::Builder::address(bus_address)?
            .name(SERVICE)?
            .build()
            .await?;
        let state = Arc::new(Mutex::new(State::default()));
        let mut tasks = Vec::new();

        // Re-emit the host's open signals for the session, recording handles
        // from openAsync transactions this session started.
        let rule = zbus::MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(SERVICE)?
            .interface(INTERFACE)?
            .path(PATH)?
            .build();
        let mut signals = zbus::MessageStream::for_match_rule(rule, &host, None).await?;
        {
            let (session, state) = (session.clone(), state.clone());
            tasks.push(tokio::spawn(async move {
                while let Some(Ok(message)) = signals.next().await {
                    let header = message.header();
                    let Some(member) = header.member().map(|member| member.to_string()) else { continue };
                    if member == "walletAsyncOpened"
                        && let Ok((transaction, handle)) = message.body().deserialize::<(i32, i32)>()
                    {
                        let Ok(mut state) = state.lock() else { continue };
                        match state.pending.remove(&transaction) {
                            Some(app) if handle >= 0 => {
                                state.handles.insert((handle, app));
                            }
                            // Another client's transaction: not the session's to see.
                            None => continue,
                            Some(_) => {}
                        }
                    }
                    if member != "walletAsyncOpened" && member != "walletOpened" {
                        continue;
                    }
                    if let Ok(body) = message.body().deserialize::<Structure<'_>>() {
                        let _ = session.emit_signal(None::<&str>, PATH, INTERFACE, member.as_str(), &body).await;
                    }
                }
            }));
        }

        let mut calls = zbus::MessageStream::from(&session);
        {
            let (session, host, state) = (session.clone(), host.clone(), state.clone());
            tasks.push(tokio::spawn(async move {
                while let Some(Ok(message)) = calls.next().await {
                    if message.message_type() != MessageType::MethodCall {
                        continue;
                    }
                    let (session, host, state, wallet) = (session.clone(), host.clone(), state.clone(), wallet.clone());
                    tokio::spawn(async move {
                        if let Err(error) = handle_call(&session, &host, &state, wallet.as_deref(), &message).await {
                            eprintln!("wallet mediator: {error}");
                            let _ = session
                                .reply_error(&message.header(), "org.freedesktop.DBus.Error.Failed", &error.to_string())
                                .await;
                        }
                    });
                }
            }));
        }
        Ok(Self { host, state, tasks })
    }

    /// Release every host handle the session opened and stop serving.
    pub async fn shutdown(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        let handles: Vec<(i32, String)> = self
            .state
            .lock()
            .map(|mut state| state.handles.drain().collect())
            .unwrap_or_default();
        for (handle, app) in handles {
            match call(&self.host, "close", &(handle, true, app.as_str())).await {
                Ok(_) => eprintln!("wallet mediator: released host handle for {app}"),
                Err(error) => eprintln!("wallet mediator: could not release host handle for {app}: {error}"),
            }
        }
    }
}

async fn handle_call(
    session: &zbus::Connection,
    host: &zbus::Connection,
    state: &Arc<Mutex<State>>,
    wallet: Option<&str>,
    message: &zbus::Message,
) -> zbus::Result<()> {
    let header = message.header();
    let member = header.member().map(|member| member.to_string()).unwrap_or_default();
    let interface = header.interface().map(|interface| interface.to_string()).unwrap_or_default();
    if header.path().map(|path| path.as_str()) == Some(PATH) {
        match (interface.as_str(), member.as_str()) {
            ("org.freedesktop.DBus.Introspectable", "Introspect") => {
                let reply = tokio::time::timeout(
                    HOST_CALL_TIMEOUT,
                    host.call_method(Some(SERVICE), PATH, Some("org.freedesktop.DBus.Introspectable"), "Introspect", &()),
                )
                .await
                .map_err(|_| zbus::Error::Failure("Introspect: host kwalletd6 did not answer".to_owned()))??;
                return session.reply(&header, &reply.body().deserialize::<String>()?).await;
            }
            ("org.freedesktop.DBus.Peer", "Ping") => return session.reply(&header, &()).await,
            ("org.freedesktop.DBus.Peer", "GetMachineId") => {
                let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
                return session.reply(&header, &id.trim()).await;
            }
            _ => {}
        }
    }
    if header.path().map(|path| path.as_str()) != Some(PATH) || (interface != INTERFACE && !interface.is_empty()) {
        return session
            .reply_error(&header, "org.freedesktop.DBus.Error.UnknownObject", &"kwin-mcp exposes only org.kde.KWallet at /modules/kwalletd6")
            .await;
    }
    let body = message.body();
    let deny = |reason: &str| format!("kwin-mcp: {member} {reason}");

    // Without a healthy host wallet, answer as a disabled KWallet.
    let Some(wallet) = wallet else {
        return match member.as_str() {
            "isEnabled" => session.reply(&header, &false).await,
            "wallets" => session.reply(&header, &Vec::<String>::new()).await,
            "open" | "openPath" => session.reply(&header, &-1i32).await,
            _ => session.reply_error(&header, "org.freedesktop.DBus.Error.AccessDenied", &deny("is unavailable: host wallet forwarding is disabled")).await,
        };
    };

    if READ_METHODS.contains(&member.as_str()) {
        let reply = if body.signature().to_string().is_empty() {
            call(host, &member, &()).await?
        } else {
            call(host, &member, &body.deserialize::<Structure<'_>>()?).await?
        };
        return session.reply(&header, &reply.body().deserialize::<Structure<'_>>()?).await;
    }

    if OPEN_METHODS.contains(&member.as_str()) || OPEN_ASYNC_METHODS.contains(&member.as_str()) {
        let (requested, app): (String, String) = if OPEN_METHODS.contains(&member.as_str()) {
            let (requested, _window, app): (String, i64, String) = body.deserialize()?;
            (requested, app)
        } else {
            let (requested, _window, app, _session): (String, i64, String, bool) = body.deserialize()?;
            (requested, app)
        };
        // Fail closed: never let a session open (and so create or unlock)
        // anything but the checked host wallet, re-checked at each open.
        let refusal = if member.starts_with("openPath") || requested != wallet {
            Some(format!("session may only open host wallet '{wallet}'"))
        } else {
            wallet_refusal(host, wallet).await
        };
        if let Some(reason) = refusal {
            eprintln!("wallet mediator: refused {member}({requested}) for {app}: {reason}");
            return session.reply(&header, &-1i32).await;
        }
        let reply = call(host, &member, &body.deserialize::<Structure<'_>>()?).await?;
        let value: i32 = reply.body().deserialize()?;
        eprintln!("wallet mediator: {member}({requested}) for {app} -> {}", if value >= 0 { "ok" } else { "refused by host" });
        if value >= 0
            && let Ok(mut state) = state.lock()
        {
            if OPEN_METHODS.contains(&member.as_str()) {
                state.handles.insert((value, app));
            } else {
                state.pending.insert(value, app);
            }
        }
        return session.reply(&header, &value).await;
    }

    if member == "close"
        && let Ok((handle, _force, app)) = body.deserialize::<(i32, bool, String)>()
    {
        // A session close releases the host handle for real (force=true),
        // but only for a handle this session opened.
        let owned = state.lock().map(|state| state.handles.contains(&(handle, app.clone()))).unwrap_or(false);
        if !owned {
            return session.reply(&header, &-1i32).await;
        }
        let reply = call(host, "close", &(handle, true, app.as_str())).await?;
        eprintln!("wallet mediator: released host handle for {app} on its close");
        if let Ok(mut state) = state.lock() {
            state.handles.remove(&(handle, app));
        }
        return session.reply(&header, &reply.body().deserialize::<i32>()?).await;
    }

    session
        .reply_error(&header, "org.freedesktop.DBus.Error.AccessDenied", &deny("is not permitted: the session gets read-only access to the host wallet"))
        .await
}
