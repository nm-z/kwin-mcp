//! A session-local Secret Service (`org.freedesktop.secrets`) for the isolated
//! session.
//!
//! Applications that keep credentials in the desktop keyring (libsecret and
//! keytar clients such as GitHub Copilot CLI, `gh`, Python `keyring`) look for
//! `org.freedesktop.secrets` on their session bus. A session's bus had no such
//! name, so they were signed out and offered to keep a new token in a plain text
//! file (#870). This module serves the name on the session's private bus, beside
//! the session wallet, under the wallet's rules:
//!
//! - the host is contacted once, at `session_start`, for a read-only copy: only
//!   `OpenSession("plain")`, property reads, `GetSecrets` and `Session.Close`
//!   are sent, one after the other on one connection;
//! - it never starts a service (`org.freedesktop.secrets` must already have an
//!   owner, and every call is sent with NoAutoStart), and never unlocks or
//!   prompts: a locked collection or item is left out, `Unlock` and `Prompt` are
//!   never called, and `GetSecrets` on unlocked items cannot prompt;
//! - a connection with a request that was not answered is kept open until the
//!   server exits instead of being dropped: gnome-keyring-daemon fails
//!   assertions and exits when a client disappears in the middle of a request
//!   (#156), and D-Bus activation then replaces it with a locked one;
//! - every later call is answered from memory. Items the session stores, changes
//!   or deletes live in the session's copy only and end with the session, so a
//!   sign-in made in a session has a keyring to go to and never reaches the
//!   host's.
//!
//! Only the "plain" session algorithm is offered. The transport is the session's
//! private Unix socket, and libsecret, secretstorage and oo7 fall back to it when
//! the Diffie-Hellman negotiation is answered with NotSupported.

use crate::wallet_mediator::{HostSlot, host_call, name_has_owner};
use futures::StreamExt;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::message::Type as MessageType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

pub const SERVICE: &str = "org.freedesktop.secrets";
const ROOT: &str = "/org/freedesktop/secrets";
const COLLECTIONS: &str = "/org/freedesktop/secrets/collection/";
const ALIASES: &str = "/org/freedesktop/secrets/aliases/";
const SESSIONS: &str = "/org/freedesktop/secrets/session/";
const SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const SESSION_INTERFACE: &str = "org.freedesktop.Secret.Session";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
/// The object path that means "no prompt is needed".
const NO_PROMPT: &str = "/";
/// The alias every client stores to unless it names a collection.
const DEFAULT_ALIAS: &str = "default";
/// The collection made for the session when the host's default one is not copied.
const LOCAL_COLLECTION: &str = "kwin_mcp_session";

/// Set to a D-Bus address to read the host Secret Service from that bus instead
/// of the host session bus (tests run a private keyring daemon there).
pub const HOST_ADDRESS_ENV: &str = "KWIN_MCP_SECRET_SERVICE_ADDRESS";

/// Bound for reading the host's collections, inside session_start's hard limit.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(6);
/// Limits that keep a runaway keyring, or a runaway session, from filling the
/// server's memory.
const MAX_ITEMS: usize = 20_000;
const MAX_BYTES: usize = 64 << 20;
/// Sessions a client opened and never closed are forgotten oldest first.
const MAX_SESSIONS: usize = 4096;

const NO_SUCH_OBJECT: &str = "org.freedesktop.Secret.Error.NoSuchObject";
const NO_SESSION: &str = "org.freedesktop.Secret.Error.NoSession";
const NOT_SUPPORTED: &str = "org.freedesktop.DBus.Error.NotSupported";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const LIMITS_EXCEEDED: &str = "org.freedesktop.DBus.Error.LimitsExceeded";

#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct Item {
    label: String,
    attributes: BTreeMap<String, String>,
    secret: Vec<u8>,
    content_type: String,
    created: u64,
    modified: u64,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Collection {
    label: String,
    created: u64,
    modified: u64,
    items: BTreeMap<u64, Item>,
}

/// The keyring the session sees: the host copy plus the session's own changes.
/// It has no `Debug`: secrets must not reach a log.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Store {
    collections: BTreeMap<String, Collection>,
    aliases: BTreeMap<String, String>,
    next_item: u64,
    next_session: u64,
    sessions: BTreeSet<u64>,
}

/// What a refused call is answered with: a D-Bus error name and its text.
type Refusal = (&'static str, String);

fn refuse<T>(name: &'static str, text: impl Into<String>) -> Result<T, Refusal> {
    Err((name, text.into()))
}

/// The object a call was sent to.
enum Object {
    Service,
    Collection(String),
    Item(String, u64),
    Session(u64),
}

/// A secret as the protocol carries it: session, parameters, value, content type.
type WireSecret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

fn collection_path(id: &str) -> String {
    format!("{COLLECTIONS}{id}")
}

fn item_path(id: &str, item: u64) -> String {
    format!("{COLLECTIONS}{id}/{item}")
}

/// A collection id is one object path element: letters, digits and underscores.
fn path_element(text: &str) -> String {
    let element: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if element.is_empty() {
        "collection".to_owned()
    } else {
        element
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Store {
    pub fn item_count(&self) -> usize {
        self.collections
            .values()
            .map(|collection| collection.items.len())
            .sum()
    }

    fn bytes(&self) -> usize {
        self.collections
            .values()
            .flat_map(|collection| collection.items.values())
            .map(|item| item.secret.len())
            .sum()
    }

    fn object(&self, path: &str) -> Option<Object> {
        if path == ROOT {
            return Some(Object::Service);
        }
        if let Some(session) = path.strip_prefix(SESSIONS) {
            let id = session.strip_prefix('s')?.parse().ok()?;
            return self.sessions.contains(&id).then_some(Object::Session(id));
        }
        let (id, item) = if let Some(rest) = path.strip_prefix(ALIASES) {
            let (alias, item) = rest
                .split_once('/')
                .map_or((rest, None), |(a, i)| (a, Some(i)));
            (self.aliases.get(alias)?.clone(), item)
        } else {
            let rest = path.strip_prefix(COLLECTIONS)?;
            let (id, item) = rest
                .split_once('/')
                .map_or((rest, None), |(c, i)| (c, Some(i)));
            (id.to_owned(), item)
        };
        let collection = self.collections.get(&id)?;
        match item {
            None => Some(Object::Collection(id)),
            Some(item) => {
                let item = item.parse().ok()?;
                collection
                    .items
                    .contains_key(&item)
                    .then_some(Object::Item(id, item))
            }
        }
    }

    /// Paths of the items carrying every wanted attribute, in one collection or all.
    fn search(&self, only: Option<&str>, wanted: &HashMap<String, String>) -> Vec<String> {
        self.collections
            .iter()
            .filter(|(id, _)| only.is_none_or(|only| only == id.as_str()))
            .flat_map(|(id, collection)| {
                collection
                    .items
                    .iter()
                    .filter(|(_, item)| {
                        wanted
                            .iter()
                            .all(|(key, value)| item.attributes.get(key) == Some(value))
                    })
                    .map(move |(number, _)| item_path(id, *number))
            })
            .collect()
    }

    /// Give the `default` alias a collection when the copy brought none, so a
    /// client's first store has somewhere to go without creating or unlocking.
    fn ensure_default(&mut self, now: u64) {
        let resolved = self
            .aliases
            .get(DEFAULT_ALIAS)
            .is_some_and(|id| self.collections.contains_key(id));
        if resolved {
            return;
        }
        self.collections
            .entry(LOCAL_COLLECTION.to_owned())
            .or_insert_with(|| Collection {
                label: "Session keyring".to_owned(),
                created: now,
                modified: now,
                items: BTreeMap::new(),
            });
        self.aliases
            .insert(DEFAULT_ALIAS.to_owned(), LOCAL_COLLECTION.to_owned());
    }

    /// The id of a new empty collection, or of the one `alias` already names.
    fn create_collection(&mut self, label: &str, alias: &str, now: u64) -> (String, bool) {
        if let Some(existing) = self
            .aliases
            .get(alias)
            .filter(|id| self.collections.contains_key(*id))
        {
            return (existing.clone(), false);
        }
        let base = path_element(if label.is_empty() { alias } else { label });
        let mut id = base.clone();
        let mut suffix = 1u32;
        while self.collections.contains_key(&id) {
            suffix += 1;
            id = format!("{base}_{suffix}");
        }
        self.collections.insert(
            id.clone(),
            Collection {
                label: label.to_owned(),
                created: now,
                modified: now,
                items: BTreeMap::new(),
            },
        );
        if !alias.is_empty() {
            self.aliases.insert(alias.to_owned(), id.clone());
        }
        (id, true)
    }

    /// Store `item` in collection `id`; with `replace`, over the item that has
    /// exactly its attributes. Returns the item's number and whether it is new.
    fn create_item(
        &mut self,
        id: &str,
        mut item: Item,
        replace: bool,
    ) -> Result<(u64, bool), Refusal> {
        let replaced = replace
            .then(|| {
                self.collections.get(id).and_then(|collection| {
                    collection
                        .items
                        .iter()
                        .filter(|(_, existing)| existing.attributes == item.attributes)
                        .map(|(number, existing)| {
                            (*number, existing.created, existing.secret.len())
                        })
                        .next()
                })
            })
            .flatten();
        let freed = replaced.map_or(0, |(_, _, bytes)| bytes);
        if (replaced.is_none() && self.item_count() >= MAX_ITEMS)
            || self
                .bytes()
                .saturating_sub(freed)
                .saturating_add(item.secret.len())
                > MAX_BYTES
        {
            return refuse(LIMITS_EXCEEDED, "the session keyring is full");
        }
        let number = match replaced {
            Some((number, created, _)) => {
                item.created = created;
                number
            }
            None => {
                self.next_item += 1;
                self.next_item
            }
        };
        let Some(collection) = self.collections.get_mut(id) else {
            return refuse(NO_SUCH_OBJECT, "no such collection");
        };
        collection.modified = item.modified;
        collection.items.insert(number, item);
        Ok((number, replaced.is_none()))
    }
}

/// A signal to emit once a call's reply is on its way.
struct Signal {
    path: String,
    interface: &'static str,
    name: &'static str,
    object: String,
}

impl Signal {
    fn service(name: &'static str, id: &str) -> Self {
        Self {
            path: ROOT.to_owned(),
            interface: SERVICE_INTERFACE,
            name,
            object: collection_path(id),
        }
    }

    fn collection(name: &'static str, id: &str, item: u64) -> Self {
        Self {
            path: collection_path(id),
            interface: COLLECTION_INTERFACE,
            name,
            object: item_path(id, item),
        }
    }
}

/// A secret on its way out: the session it is for, its value, its content type.
type PlainSecret = (String, Vec<u8>, String);

/// A reply body, built under the store's lock and sent after it is released.
enum Out {
    Unit,
    Path(String),
    Paths(Vec<String>),
    TwoPaths(String, String),
    TwoLists(Vec<String>, Vec<String>),
    ListAndPath(Vec<String>, String),
    Session(String),
    Secret(String, Vec<u8>, String),
    Secrets(Vec<(String, PlainSecret)>),
    Property(Value<'static>),
    Properties(HashMap<String, Value<'static>>),
}

fn object_path(path: &str) -> zbus::Result<ObjectPath<'static>> {
    ObjectPath::try_from(path.to_owned()).map_err(zbus::Error::from)
}

fn object_paths(paths: &[String]) -> Vec<ObjectPath<'static>> {
    paths
        .iter()
        .filter_map(|path| object_path(path).ok())
        .collect()
}

fn properties(store: &Store, object: &Object) -> HashMap<String, Value<'static>> {
    let mut map = HashMap::new();
    match object {
        Object::Service => {
            let paths: Vec<String> = store
                .collections
                .keys()
                .map(|id| collection_path(id))
                .collect();
            map.insert("Collections".to_owned(), Value::from(object_paths(&paths)));
        }
        Object::Collection(id) => {
            if let Some(collection) = store.collections.get(id) {
                let paths: Vec<String> = collection
                    .items
                    .keys()
                    .map(|number| item_path(id, *number))
                    .collect();
                map.insert("Items".to_owned(), Value::from(object_paths(&paths)));
                map.insert("Label".to_owned(), Value::from(collection.label.clone()));
                map.insert("Locked".to_owned(), Value::from(false));
                map.insert("Created".to_owned(), Value::from(collection.created));
                map.insert("Modified".to_owned(), Value::from(collection.modified));
            }
        }
        Object::Item(id, number) => {
            if let Some(item) = store.collections.get(id).and_then(|c| c.items.get(number)) {
                let attributes: HashMap<String, String> = item
                    .attributes
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                map.insert(
                    "Type".to_owned(),
                    Value::from(
                        item.attributes
                            .get("xdg:schema")
                            .cloned()
                            .unwrap_or_default(),
                    ),
                );
                map.insert("Attributes".to_owned(), Value::from(attributes));
                map.insert("Label".to_owned(), Value::from(item.label.clone()));
                map.insert("Locked".to_owned(), Value::from(false));
                map.insert("Created".to_owned(), Value::from(item.created));
                map.insert("Modified".to_owned(), Value::from(item.modified));
            }
        }
        Object::Session(_) => {}
    }
    map
}

fn interface_of(object: &Object) -> &'static str {
    match object {
        Object::Service => SERVICE_INTERFACE,
        Object::Collection(_) => COLLECTION_INTERFACE,
        Object::Item(..) => ITEM_INTERFACE,
        Object::Session(_) => SESSION_INTERFACE,
    }
}

fn text_property(properties: &mut HashMap<String, OwnedValue>, key: &str) -> String {
    properties
        .remove(key)
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default()
}

fn attribute_property(
    properties: &mut HashMap<String, OwnedValue>,
    key: &str,
) -> BTreeMap<String, String> {
    properties
        .remove(key)
        .and_then(|value| HashMap::<String, String>::try_from(value).ok())
        .map(|attributes| attributes.into_iter().collect())
        .unwrap_or_default()
}

/// Answer one call from the store. Everything a session changes stays here.
fn dispatch(
    store: &mut Store,
    path: &str,
    interface: &str,
    member: &str,
    body: &zbus::message::Body,
    now: u64,
) -> Result<(Out, Vec<Signal>), Refusal> {
    let Some(object) = store.object(path) else {
        return refuse(
            NO_SUCH_OBJECT,
            format!("the session keyring has no object {path}"),
        );
    };
    let bad = |error: zbus::Error| (INVALID_ARGS, format!("{member}: {error}"));
    let unknown = || {
        refuse(
            UNKNOWN_METHOD,
            format!("{interface}.{member} is not available at {path}"),
        )
    };
    let session_exists = |store: &Store, session: &OwnedObjectPath| {
        if matches!(store.object(session.as_str()), Some(Object::Session(_))) {
            Ok(())
        } else {
            refuse(NO_SESSION, "open a session with OpenSession first")
        }
    };
    if interface == PROPERTIES_INTERFACE {
        return match member {
            "GetAll" => Ok((Out::Properties(properties(store, &object)), Vec::new())),
            "Get" => {
                let (_, name): (String, String) = body.deserialize().map_err(bad)?;
                match properties(store, &object).remove(&name) {
                    Some(value) => Ok((Out::Property(value), Vec::new())),
                    None => refuse(INVALID_ARGS, format!("no property {name}")),
                }
            }
            "Set" => {
                let (_, name, value): (String, String, OwnedValue) =
                    body.deserialize().map_err(bad)?;
                let mut given = HashMap::from([(name.clone(), value)]);
                match (&object, name.as_str()) {
                    (Object::Collection(id), "Label") => {
                        if let Some(collection) = store.collections.get_mut(id) {
                            collection.label = text_property(&mut given, "Label");
                            collection.modified = now;
                        }
                        Ok((Out::Unit, vec![Signal::service("CollectionChanged", id)]))
                    }
                    (Object::Item(id, number), "Label" | "Attributes") => {
                        if let Some(item) = store
                            .collections
                            .get_mut(id)
                            .and_then(|c| c.items.get_mut(number))
                        {
                            if name == "Label" {
                                item.label = text_property(&mut given, "Label");
                            } else {
                                item.attributes = attribute_property(&mut given, "Attributes");
                            }
                            item.modified = now;
                        }
                        Ok((
                            Out::Unit,
                            vec![Signal::collection("ItemChanged", id, *number)],
                        ))
                    }
                    _ => refuse(INVALID_ARGS, format!("{name} cannot be set")),
                }
            }
            _ => unknown(),
        };
    }
    if !interface.is_empty() && interface != interface_of(&object) {
        return unknown();
    }
    match object {
        Object::Service => match member {
            "OpenSession" => {
                let (algorithm, _input): (String, OwnedValue) = body.deserialize().map_err(bad)?;
                if algorithm != "plain" {
                    return refuse(
                        NOT_SUPPORTED,
                        format!(
                            "the session keyring offers the plain algorithm only, not {algorithm}"
                        ),
                    );
                }
                store.next_session += 1;
                let id = store.next_session;
                store.sessions.insert(id);
                while store.sessions.len() > MAX_SESSIONS {
                    store.sessions.pop_first();
                }
                Ok((Out::Session(format!("{SESSIONS}s{id}")), Vec::new()))
            }
            "CreateCollection" => {
                let (mut given, alias): (HashMap<String, OwnedValue>, String) =
                    body.deserialize().map_err(bad)?;
                let label = text_property(&mut given, "org.freedesktop.Secret.Collection.Label");
                let (id, created) = store.create_collection(&label, &alias, now);
                let signals = if created {
                    vec![Signal::service("CollectionCreated", &id)]
                } else {
                    Vec::new()
                };
                Ok((
                    Out::TwoPaths(collection_path(&id), NO_PROMPT.to_owned()),
                    signals,
                ))
            }
            "SearchItems" => {
                let wanted: HashMap<String, String> = body.deserialize().map_err(bad)?;
                Ok((
                    Out::TwoLists(store.search(None, &wanted), Vec::new()),
                    Vec::new(),
                ))
            }
            // Nothing in the session's copy is locked, and nothing in it can be.
            "Unlock" => {
                let objects: Vec<OwnedObjectPath> = body.deserialize().map_err(bad)?;
                let known = objects
                    .iter()
                    .filter(|path| store.object(path.as_str()).is_some())
                    .map(|path| path.as_str().to_owned())
                    .collect();
                Ok((Out::ListAndPath(known, NO_PROMPT.to_owned()), Vec::new()))
            }
            "Lock" => Ok((
                Out::ListAndPath(Vec::new(), NO_PROMPT.to_owned()),
                Vec::new(),
            )),
            "GetSecrets" => {
                let (items, session): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                    body.deserialize().map_err(bad)?;
                session_exists(store, &session)?;
                let secrets = items
                    .iter()
                    .filter_map(|path| match store.object(path.as_str()) {
                        Some(Object::Item(id, number)) => {
                            let item = store.collections.get(&id)?.items.get(&number)?;
                            Some((
                                path.as_str().to_owned(),
                                (
                                    session.as_str().to_owned(),
                                    item.secret.clone(),
                                    item.content_type.clone(),
                                ),
                            ))
                        }
                        Some(Object::Service | Object::Collection(_) | Object::Session(_))
                        | None => None,
                    })
                    .collect();
                Ok((Out::Secrets(secrets), Vec::new()))
            }
            "ReadAlias" => {
                let alias: String = body.deserialize().map_err(bad)?;
                let path = store
                    .aliases
                    .get(&alias)
                    .filter(|id| store.collections.contains_key(*id))
                    .map_or_else(|| NO_PROMPT.to_owned(), |id| collection_path(id));
                Ok((Out::Path(path), Vec::new()))
            }
            "SetAlias" => {
                let (alias, collection): (String, OwnedObjectPath) =
                    body.deserialize().map_err(bad)?;
                if collection.as_str() == NO_PROMPT {
                    store.aliases.remove(&alias);
                    return Ok((Out::Unit, Vec::new()));
                }
                match store.object(collection.as_str()) {
                    Some(Object::Collection(id)) => {
                        store.aliases.insert(alias, id);
                        Ok((Out::Unit, Vec::new()))
                    }
                    Some(Object::Service | Object::Item(..) | Object::Session(_)) | None => {
                        refuse(NO_SUCH_OBJECT, "no such collection")
                    }
                }
            }
            _ => unknown(),
        },
        Object::Collection(id) => match member {
            "Delete" => {
                store.collections.remove(&id);
                store.aliases.retain(|_, target| *target != id);
                Ok((
                    Out::Path(NO_PROMPT.to_owned()),
                    vec![Signal::service("CollectionDeleted", &id)],
                ))
            }
            "SearchItems" => {
                let wanted: HashMap<String, String> = body.deserialize().map_err(bad)?;
                Ok((Out::Paths(store.search(Some(&id), &wanted)), Vec::new()))
            }
            "CreateItem" => {
                let (mut given, (session, _parameters, secret, content_type), replace): (
                    HashMap<String, OwnedValue>,
                    WireSecret,
                    bool,
                ) = body.deserialize().map_err(bad)?;
                session_exists(store, &session)?;
                let item = Item {
                    label: text_property(&mut given, "org.freedesktop.Secret.Item.Label"),
                    attributes: attribute_property(
                        &mut given,
                        "org.freedesktop.Secret.Item.Attributes",
                    ),
                    secret,
                    content_type,
                    created: now,
                    modified: now,
                };
                let (number, created) = store.create_item(&id, item, replace)?;
                let signal = if created {
                    "ItemCreated"
                } else {
                    "ItemChanged"
                };
                Ok((
                    Out::TwoPaths(item_path(&id, number), NO_PROMPT.to_owned()),
                    vec![Signal::collection(signal, &id, number)],
                ))
            }
            _ => unknown(),
        },
        Object::Item(id, number) => match member {
            "Delete" => {
                if let Some(collection) = store.collections.get_mut(&id) {
                    collection.items.remove(&number);
                    collection.modified = now;
                }
                Ok((
                    Out::Path(NO_PROMPT.to_owned()),
                    vec![Signal::collection("ItemDeleted", &id, number)],
                ))
            }
            "GetSecret" => {
                let session: OwnedObjectPath = body.deserialize().map_err(bad)?;
                session_exists(store, &session)?;
                match store
                    .collections
                    .get(&id)
                    .and_then(|c| c.items.get(&number))
                {
                    Some(item) => Ok((
                        Out::Secret(
                            session.as_str().to_owned(),
                            item.secret.clone(),
                            item.content_type.clone(),
                        ),
                        Vec::new(),
                    )),
                    None => refuse(NO_SUCH_OBJECT, "no such item"),
                }
            }
            "SetSecret" => {
                let ((session, _parameters, secret, content_type),): (WireSecret,) =
                    body.deserialize().map_err(bad)?;
                session_exists(store, &session)?;
                let freed = store
                    .collections
                    .get(&id)
                    .and_then(|c| c.items.get(&number))
                    .map_or(0, |item| item.secret.len());
                if store
                    .bytes()
                    .saturating_sub(freed)
                    .saturating_add(secret.len())
                    > MAX_BYTES
                {
                    return refuse(LIMITS_EXCEEDED, "the session keyring is full");
                }
                if let Some(item) = store
                    .collections
                    .get_mut(&id)
                    .and_then(|c| c.items.get_mut(&number))
                {
                    item.secret = secret;
                    item.content_type = content_type;
                    item.modified = now;
                }
                Ok((
                    Out::Unit,
                    vec![Signal::collection("ItemChanged", &id, number)],
                ))
            }
            _ => unknown(),
        },
        Object::Session(id) => match member {
            "Close" => {
                store.sessions.remove(&id);
                Ok((Out::Unit, Vec::new()))
            }
            _ => unknown(),
        },
    }
}

/// Outcome of the host copy, reported by session_start.
pub struct Snapshot {
    pub store: Store,
    /// Items copied from the host.
    pub copied: usize,
    pub reason: String,
}

pub fn empty(reason: impl Into<String>) -> Snapshot {
    let mut store = Store::default();
    store.ensure_default(now());
    Snapshot {
        store,
        copied: 0,
        reason: format!("{}; the session keyring starts empty", reason.into()),
    }
}

/// Connections whose last request was not answered. They stay open until the
/// server exits, so the host service never sees a client vanish mid-request.
static UNANSWERED: Mutex<Vec<zbus::Connection>> = Mutex::new(Vec::new());

fn keep_open(host: &zbus::Connection) {
    if let Ok(mut kept) = UNANSWERED.lock() {
        kept.push(host.clone());
    }
}

/// Take the one read-only copy of the host's unlocked collections, or say why
/// the session keyring starts empty.
pub async fn snapshot(host: &zbus::Connection, runtime: &std::path::Path) -> Snapshot {
    // The wallet's lock: ksecretd can be behind both names, and reads of the
    // host's secrets from every kwin-mcp server stay one at a time.
    let slot = match tokio::time::timeout(SNAPSHOT_TIMEOUT, HostSlot::take(runtime)).await {
        Ok(Ok(slot)) => slot,
        Ok(Err(reason)) => return empty(reason),
        Err(_) => return empty("other sessions were still reading the host's secrets"),
    };
    let outcome = match tokio::time::timeout(SNAPSHOT_TIMEOUT, read_host(host)).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(reason)) => {
            keep_open(host);
            empty(reason)
        }
        Err(_) => {
            keep_open(host);
            empty(format!(
                "the host Secret Service did not answer within {} s",
                SNAPSHOT_TIMEOUT.as_secs()
            ))
        }
    };
    drop(slot);
    outcome
}

async fn host_properties(
    host: &zbus::Connection,
    path: &str,
    interface: &str,
) -> Result<HashMap<String, OwnedValue>, String> {
    let proxy = zbus::Proxy::new(host, SERVICE, path.to_owned(), PROPERTIES_INTERFACE)
        .await
        .map_err(|error| format!("host Secret Service proxy: {error}"))?;
    host_call(&proxy, "GetAll", &(interface,))
        .await
        .map_err(|error| format!("reading {path}: {error}"))
}

fn take<T: TryFrom<OwnedValue>>(
    properties: &mut HashMap<String, OwnedValue>,
    key: &str,
) -> Option<T> {
    properties
        .remove(key)
        .and_then(|value| T::try_from(value).ok())
}

async fn read_host(host: &zbus::Connection) -> Result<Snapshot, String> {
    if !name_has_owner(host, SERVICE).await {
        return Ok(empty(format!(
            "{SERVICE} is not running on the host, and kwin-mcp does not start it"
        )));
    }
    let service = zbus::Proxy::new(host, SERVICE, ROOT, SERVICE_INTERFACE)
        .await
        .map_err(|error| format!("host Secret Service proxy: {error}"))?;
    let listed: Vec<OwnedObjectPath> = take(
        &mut host_properties(host, ROOT, SERVICE_INTERFACE).await?,
        "Collections",
    )
    .unwrap_or_default();
    let mut store = Store::default();
    // Host path of each copied item, with where it goes in the session's copy.
    let mut wanted: Vec<(OwnedObjectPath, String, Item)> = Vec::new();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut locked: Vec<String> = Vec::new();
    for path in &listed {
        let mut collection = host_properties(host, path.as_str(), COLLECTION_INTERFACE).await?;
        let label: String = take(&mut collection, "Label").unwrap_or_default();
        let name = path.as_str().rsplit('/').next().unwrap_or_default();
        // A lock state that cannot be read is treated as locked.
        if take::<bool>(&mut collection, "Locked") != Some(false) {
            locked.push(if label.is_empty() {
                name.to_owned()
            } else {
                label
            });
            continue;
        }
        let id = path_element(name);
        ids.insert(path.as_str().to_owned(), id.clone());
        store.collections.insert(
            id.clone(),
            Collection {
                label,
                created: take(&mut collection, "Created").unwrap_or(0),
                modified: take(&mut collection, "Modified").unwrap_or(0),
                items: BTreeMap::new(),
            },
        );
        let items: Vec<OwnedObjectPath> = take(&mut collection, "Items").unwrap_or_default();
        if wanted.len() + items.len() > MAX_ITEMS {
            return Ok(empty(
                "the host keyring is too large to copy into a session",
            ));
        }
        for item_path in items {
            let mut item = host_properties(host, item_path.as_str(), ITEM_INTERFACE).await?;
            if take::<bool>(&mut item, "Locked") != Some(false) {
                continue;
            }
            let attributes: HashMap<String, String> =
                take(&mut item, "Attributes").unwrap_or_default();
            wanted.push((
                item_path,
                id.clone(),
                Item {
                    label: take(&mut item, "Label").unwrap_or_default(),
                    attributes: attributes.into_iter().collect(),
                    secret: Vec::new(),
                    content_type: String::new(),
                    created: take(&mut item, "Created").unwrap_or(0),
                    modified: take(&mut item, "Modified").unwrap_or(0),
                },
            ));
        }
    }
    let mut copied = 0usize;
    if !wanted.is_empty() {
        let (_, session): (OwnedValue, OwnedObjectPath) = host_call(
            &service,
            "OpenSession",
            &("plain", Value::new(String::new())),
        )
        .await
        .map_err(|error| format!("OpenSession: {error}"))?;
        let paths: Vec<&OwnedObjectPath> = wanted.iter().map(|(path, ..)| path).collect();
        let secrets = host_call::<_, HashMap<OwnedObjectPath, WireSecret>>(
            &service,
            "GetSecrets",
            &(&paths, &session),
        )
        .await;
        // The session is closed whatever the read did.
        if let Ok(proxy) = zbus::Proxy::new(host, SERVICE, session, SESSION_INTERFACE).await {
            host_call::<_, ()>(&proxy, "Close", &())
                .await
                .map_err(|error| format!("closing the host session: {error}"))?;
        }
        let mut secrets = secrets.map_err(|error| format!("GetSecrets: {error}"))?;
        let mut bytes = 0usize;
        for (path, id, mut item) in wanted {
            let Some((_, _, secret, content_type)) = secrets.remove(&path) else {
                continue;
            };
            bytes += secret.len();
            if bytes > MAX_BYTES {
                return Ok(empty(
                    "the host keyring is too large to copy into a session",
                ));
            }
            item.secret = secret;
            item.content_type = content_type;
            store.next_item += 1;
            if let Some(collection) = store.collections.get_mut(&id) {
                collection.items.insert(store.next_item, item);
                copied += 1;
            }
        }
    }
    let default: OwnedObjectPath = host_call(&service, "ReadAlias", &(DEFAULT_ALIAS,))
        .await
        .map_err(|error| format!("ReadAlias: {error}"))?;
    if let Some(id) = ids.get(default.as_str()) {
        store.aliases.insert(DEFAULT_ALIAS.to_owned(), id.clone());
    }
    store.ensure_default(now());
    let mut reason = format!(
        "session-local copy of the host Secret Service: {copied} item(s) from {} unlocked collection(s)",
        ids.len()
    );
    if !locked.is_empty() {
        reason.push_str(&format!(
            "; {} locked on the host and not copied, because kwin-mcp does not unlock it: {}",
            locked.len(),
            locked.join(", ")
        ));
    }
    Ok(Snapshot {
        store,
        copied,
        reason,
    })
}

pub struct SecretService {
    state: Arc<Mutex<Store>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for SecretService {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl SecretService {
    /// Serve org.freedesktop.secrets on the private bus at `bus_address` from `store`.
    pub async fn start(bus_address: &str, store: Store) -> zbus::Result<Self> {
        let session = zbus::connection::Builder::address(bus_address)?
            .name(SERVICE)?
            .build()
            .await?;
        let state = Arc::new(Mutex::new(store));
        let mut calls = zbus::MessageStream::from(&session);
        let task = {
            let (session, state) = (session.clone(), state.clone());
            tokio::spawn(async move {
                while let Some(Ok(message)) = calls.next().await {
                    if message.message_type() != MessageType::MethodCall {
                        continue;
                    }
                    // Calls are answered in the order they arrive: a client's
                    // CreateItem must be in the store before its next search.
                    if let Err(error) = handle_call(&session, &state, &message).await {
                        eprintln!("session keyring: {error}");
                    }
                }
            })
        };
        Ok(Self {
            state,
            tasks: vec![task],
        })
    }

    pub fn saved_state(&self) -> std::io::Result<Store> {
        self.state
            .lock()
            .map(|store| store.clone())
            .map_err(|_| std::io::Error::other("session keyring lock poisoned"))
    }

    /// Stop serving and drop the session's keyring. Nothing on the host is touched.
    pub async fn shutdown(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        if let Ok(mut store) = self.state.lock() {
            *store = Store::default();
        }
    }
}

const INTROSPECTION_HEAD: &str = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\"><node>\
<interface name=\"org.freedesktop.DBus.Properties\">\
<method name=\"Get\"><arg type=\"s\" direction=\"in\"/><arg type=\"s\" direction=\"in\"/><arg type=\"v\" direction=\"out\"/></method>\
<method name=\"GetAll\"><arg type=\"s\" direction=\"in\"/><arg type=\"a{sv}\" direction=\"out\"/></method>\
<method name=\"Set\"><arg type=\"s\" direction=\"in\"/><arg type=\"s\" direction=\"in\"/><arg type=\"v\" direction=\"in\"/></method>\
</interface>";

fn introspection(object: Option<&Object>) -> String {
    let interface = match object {
        Some(Object::Service) => {
            "<interface name=\"org.freedesktop.Secret.Service\">\
<method name=\"OpenSession\"><arg type=\"s\" direction=\"in\"/><arg type=\"v\" direction=\"in\"/><arg type=\"v\" direction=\"out\"/><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"CreateCollection\"><arg type=\"a{sv}\" direction=\"in\"/><arg type=\"s\" direction=\"in\"/><arg type=\"o\" direction=\"out\"/><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"SearchItems\"><arg type=\"a{ss}\" direction=\"in\"/><arg type=\"ao\" direction=\"out\"/><arg type=\"ao\" direction=\"out\"/></method>\
<method name=\"Unlock\"><arg type=\"ao\" direction=\"in\"/><arg type=\"ao\" direction=\"out\"/><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"Lock\"><arg type=\"ao\" direction=\"in\"/><arg type=\"ao\" direction=\"out\"/><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"GetSecrets\"><arg type=\"ao\" direction=\"in\"/><arg type=\"o\" direction=\"in\"/><arg type=\"a{o(oayays)}\" direction=\"out\"/></method>\
<method name=\"ReadAlias\"><arg type=\"s\" direction=\"in\"/><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"SetAlias\"><arg type=\"s\" direction=\"in\"/><arg type=\"o\" direction=\"in\"/></method>\
<signal name=\"CollectionCreated\"><arg type=\"o\"/></signal><signal name=\"CollectionDeleted\"><arg type=\"o\"/></signal><signal name=\"CollectionChanged\"><arg type=\"o\"/></signal>\
<property name=\"Collections\" type=\"ao\" access=\"read\"/></interface>"
        }
        Some(Object::Collection(_)) => {
            "<interface name=\"org.freedesktop.Secret.Collection\">\
<method name=\"Delete\"><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"SearchItems\"><arg type=\"a{ss}\" direction=\"in\"/><arg type=\"ao\" direction=\"out\"/></method>\
<method name=\"CreateItem\"><arg type=\"a{sv}\" direction=\"in\"/><arg type=\"(oayays)\" direction=\"in\"/><arg type=\"b\" direction=\"in\"/><arg type=\"o\" direction=\"out\"/><arg type=\"o\" direction=\"out\"/></method>\
<signal name=\"ItemCreated\"><arg type=\"o\"/></signal><signal name=\"ItemDeleted\"><arg type=\"o\"/></signal><signal name=\"ItemChanged\"><arg type=\"o\"/></signal>\
<property name=\"Items\" type=\"ao\" access=\"read\"/><property name=\"Label\" type=\"s\" access=\"readwrite\"/><property name=\"Locked\" type=\"b\" access=\"read\"/>\
<property name=\"Created\" type=\"t\" access=\"read\"/><property name=\"Modified\" type=\"t\" access=\"read\"/></interface>"
        }
        Some(Object::Item(..)) => {
            "<interface name=\"org.freedesktop.Secret.Item\">\
<method name=\"Delete\"><arg type=\"o\" direction=\"out\"/></method>\
<method name=\"GetSecret\"><arg type=\"o\" direction=\"in\"/><arg type=\"(oayays)\" direction=\"out\"/></method>\
<method name=\"SetSecret\"><arg type=\"(oayays)\" direction=\"in\"/></method>\
<property name=\"Locked\" type=\"b\" access=\"read\"/><property name=\"Attributes\" type=\"a{ss}\" access=\"readwrite\"/><property name=\"Label\" type=\"s\" access=\"readwrite\"/>\
<property name=\"Type\" type=\"s\" access=\"read\"/><property name=\"Created\" type=\"t\" access=\"read\"/><property name=\"Modified\" type=\"t\" access=\"read\"/></interface>"
        }
        Some(Object::Session(_)) => {
            "<interface name=\"org.freedesktop.Secret.Session\"><method name=\"Close\"/></interface>"
        }
        None => "",
    };
    format!("{INTROSPECTION_HEAD}{interface}</node>")
}

async fn handle_call(
    session: &zbus::Connection,
    state: &Arc<Mutex<Store>>,
    message: &zbus::Message,
) -> zbus::Result<()> {
    let header = message.header();
    let text = |name: Option<String>| name.unwrap_or_default();
    let member = text(header.member().map(ToString::to_string));
    let interface = text(header.interface().map(ToString::to_string));
    let path = text(header.path().map(ToString::to_string));
    let poisoned = || zbus::Error::Failure("session keyring lock poisoned".to_owned());
    match (interface.as_str(), member.as_str()) {
        ("org.freedesktop.DBus.Introspectable", "Introspect") => {
            let object = state.lock().map_err(|_| poisoned())?.object(&path);
            return session
                .reply(&header, &introspection(object.as_ref()))
                .await;
        }
        ("org.freedesktop.DBus.Peer", "Ping") => return session.reply(&header, &()).await,
        ("org.freedesktop.DBus.Peer", "GetMachineId") => {
            let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
            return session.reply(&header, &id.trim()).await;
        }
        _ => {}
    }
    let outcome = {
        let mut store = state.lock().map_err(|_| poisoned())?;
        dispatch(
            &mut store,
            &path,
            &interface,
            &member,
            &message.body(),
            now(),
        )
    };
    let (out, signals) = match outcome {
        Ok(answer) => answer,
        Err((name, text)) => return session.reply_error(&header, name, &text).await,
    };
    match out {
        Out::Unit => session.reply(&header, &()).await?,
        Out::Path(path) => session.reply(&header, &object_path(&path)?).await?,
        Out::Paths(paths) => session.reply(&header, &object_paths(&paths)).await?,
        Out::TwoPaths(first, second) => {
            session
                .reply(&header, &(object_path(&first)?, object_path(&second)?))
                .await?
        }
        Out::TwoLists(first, second) => {
            session
                .reply(&header, &(object_paths(&first), object_paths(&second)))
                .await?
        }
        Out::ListAndPath(list, path) => {
            session
                .reply(&header, &(object_paths(&list), object_path(&path)?))
                .await?
        }
        Out::Session(path) => {
            session
                .reply(&header, &(Value::new(String::new()), object_path(&path)?))
                .await?
        }
        Out::Secret(secret_session, value, content_type) => {
            let secret = (
                object_path(&secret_session)?,
                Vec::<u8>::new(),
                value,
                content_type,
            );
            session.reply(&header, &(secret,)).await?
        }
        Out::Secrets(secrets) => {
            let mut map = HashMap::new();
            for (path, (secret_session, value, content_type)) in secrets {
                map.insert(
                    object_path(&path)?,
                    (
                        object_path(&secret_session)?,
                        Vec::<u8>::new(),
                        value,
                        content_type,
                    ),
                );
            }
            session.reply(&header, &map).await?
        }
        Out::Property(value) => session.reply(&header, &value).await?,
        Out::Properties(map) => session.reply(&header, &map).await?,
    }
    for signal in signals {
        session
            .emit_signal(
                None::<&str>,
                signal.path.as_str(),
                signal.interface,
                signal.name,
                &(object_path(&signal.object)?,),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call<B>(
        store: &mut Store,
        path: &str,
        interface: &str,
        member: &str,
        body: &B,
    ) -> Result<Out, Refusal>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let message = zbus::Message::method_call(path, member)
            .and_then(|builder| builder.interface(interface))
            .and_then(|builder| builder.build(body))
            .map_err(|error| (INVALID_ARGS, error.to_string()))?;
        dispatch(store, path, interface, member, &message.body(), 100).map(|(out, _)| out)
    }

    fn session(store: &mut Store) -> Result<OwnedObjectPath, Refusal> {
        match call(
            store,
            ROOT,
            SERVICE_INTERFACE,
            "OpenSession",
            &("plain", Value::new("")),
        )? {
            Out::Session(path) => {
                OwnedObjectPath::try_from(path).map_err(|error| (INVALID_ARGS, error.to_string()))
            }
            _ => refuse(INVALID_ARGS, "OpenSession did not return a session"),
        }
    }

    fn store_item(
        store: &mut Store,
        session: &OwnedObjectPath,
        user: &str,
        secret: &[u8],
        replace: bool,
    ) -> Result<String, Refusal> {
        let mut given: HashMap<&str, Value> = HashMap::new();
        given.insert("org.freedesktop.Secret.Item.Label", Value::from("token"));
        given.insert(
            "org.freedesktop.Secret.Item.Attributes",
            Value::from(HashMap::from([("service", "copilot-cli"), ("user", user)])),
        );
        let secret = (session, Vec::<u8>::new(), secret.to_vec(), "text/plain");
        let default = format!("{ALIASES}{DEFAULT_ALIAS}");
        match call(
            store,
            &default,
            COLLECTION_INTERFACE,
            "CreateItem",
            &(given, secret, replace),
        )? {
            Out::TwoPaths(item, prompt) => {
                assert_eq!(prompt, NO_PROMPT);
                Ok(item)
            }
            _ => refuse(INVALID_ARGS, "CreateItem did not return an item"),
        }
    }

    fn fresh() -> Store {
        let mut store = Store::default();
        store.ensure_default(1);
        store
    }

    #[test]
    fn an_item_stored_through_the_default_alias_is_searchable_and_read_back() -> Result<(), Refusal>
    {
        let mut store = fresh();
        let session = session(&mut store)?;
        let item = store_item(&mut store, &session, "octocat", b"gho_secret", false)?;
        assert_eq!(item, item_path(LOCAL_COLLECTION, 1));
        let wanted = HashMap::from([("service", "copilot-cli"), ("user", "octocat")]);
        match call(&mut store, ROOT, SERVICE_INTERFACE, "SearchItems", &wanted)? {
            Out::TwoLists(unlocked, locked) => {
                assert_eq!(unlocked, vec![item.clone()]);
                assert!(locked.is_empty());
            }
            _ => return refuse(INVALID_ARGS, "unexpected search reply"),
        }
        let other = HashMap::from([("service", "copilot-cli"), ("user", "someone-else")]);
        assert!(matches!(
            call(&mut store, ROOT, SERVICE_INTERFACE, "SearchItems", &other)?,
            Out::TwoLists(unlocked, _) if unlocked.is_empty()
        ));
        match call(&mut store, &item, ITEM_INTERFACE, "GetSecret", &(&session,))? {
            Out::Secret(_, value, content_type) => {
                assert_eq!(value, b"gho_secret");
                assert_eq!(content_type, "text/plain");
            }
            _ => return refuse(INVALID_ARGS, "unexpected secret reply"),
        }
        Ok(())
    }

    #[test]
    fn replace_overwrites_the_item_with_the_same_attributes_only() -> Result<(), Refusal> {
        let mut store = fresh();
        let session = session(&mut store)?;
        let first = store_item(&mut store, &session, "octocat", b"old", false)?;
        let replaced = store_item(&mut store, &session, "octocat", b"new", true)?;
        let other = store_item(&mut store, &session, "hubot", b"other", true)?;
        let added = store_item(&mut store, &session, "octocat", b"second", false)?;
        assert_eq!(first, replaced);
        assert_ne!(first, other);
        assert_ne!(first, added);
        assert_eq!(store.item_count(), 3);
        assert!(matches!(
            call(&mut store, &first, ITEM_INTERFACE, "GetSecret", &(&session,))?,
            Out::Secret(_, value, _) if value == b"new"
        ));
        Ok(())
    }

    #[test]
    fn only_the_plain_algorithm_is_offered_and_secrets_need_a_session() -> Result<(), Refusal> {
        let mut store = fresh();
        let refused = call(
            &mut store,
            ROOT,
            SERVICE_INTERFACE,
            "OpenSession",
            &(
                "dh-ietf1024-sha256-aes128-cbc-pkcs7",
                Value::new(vec![1u8, 2]),
            ),
        );
        // libsecret asks again for a plain session only on this error.
        assert!(matches!(refused, Err((NOT_SUPPORTED, _))));
        let opened = session(&mut store)?;
        let item = store_item(&mut store, &opened, "octocat", b"secret", false)?;
        assert!(matches!(
            call(&mut store, opened.as_str(), SESSION_INTERFACE, "Close", &()),
            Ok(Out::Unit)
        ));
        assert!(matches!(
            call(&mut store, &item, ITEM_INTERFACE, "GetSecret", &(&opened,)),
            Err((NO_SESSION, _))
        ));
        Ok(())
    }

    #[test]
    fn unlock_answers_without_a_prompt_and_deleted_items_are_gone() -> Result<(), Refusal> {
        let mut store = fresh();
        let session = session(&mut store)?;
        let item = store_item(&mut store, &session, "octocat", b"secret", false)?;
        let objects = vec![object_path(&item).map_err(|error| (INVALID_ARGS, error.to_string()))?];
        assert!(matches!(
            call(&mut store, ROOT, SERVICE_INTERFACE, "Unlock", &(objects,))?,
            Out::ListAndPath(unlocked, prompt) if unlocked == vec![item.clone()] && prompt == NO_PROMPT
        ));
        assert!(matches!(
            call(&mut store, &item, ITEM_INTERFACE, "Delete", &())?,
            Out::Path(_)
        ));
        assert!(matches!(
            call(&mut store, &item, ITEM_INTERFACE, "GetSecret", &(&session,)),
            Err((NO_SUCH_OBJECT, _))
        ));
        assert_eq!(store.item_count(), 0);
        Ok(())
    }

    #[test]
    fn a_new_collection_takes_its_alias_and_the_default_one_always_exists() -> Result<(), Refusal> {
        let mut store = fresh();
        assert!(matches!(
            call(&mut store, ROOT, SERVICE_INTERFACE, "ReadAlias", &(DEFAULT_ALIAS,))?,
            Out::Path(path) if path == collection_path(LOCAL_COLLECTION)
        ));
        let given = HashMap::from([(
            "org.freedesktop.Secret.Collection.Label",
            Value::from("Work Keys"),
        )]);
        let created = call(
            &mut store,
            ROOT,
            SERVICE_INTERFACE,
            "CreateCollection",
            &(given, "work"),
        )?;
        assert!(
            matches!(&created, Out::TwoPaths(path, _) if *path == collection_path("work_keys"))
        );
        assert!(matches!(
            call(&mut store, &format!("{ALIASES}work"), PROPERTIES_INTERFACE, "Get", &(COLLECTION_INTERFACE, "Label"))?,
            Out::Property(Value::Str(label)) if label.as_str() == "Work Keys"
        ));
        assert!(matches!(
            call(
                &mut store,
                "/org/freedesktop/secrets/collection/missing",
                COLLECTION_INTERFACE,
                "Delete",
                &()
            ),
            Err((NO_SUCH_OBJECT, _))
        ));
        Ok(())
    }
}
