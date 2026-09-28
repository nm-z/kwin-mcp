//! kwin-mcp-shim: the stable MCP front end for kwin-mcp.
//!
//! The MCP client launches this shim once and keeps it for the whole
//! connection. The shim runs every real kwin-mcp server as a child process and
//! relays newline-delimited JSON-RPC between the client and those children:
//!
//! - One child per session. Each `session_start` gets a fresh child, so each
//!   session has its own display, window stack, keyboard focus and mouse. The
//!   returned `session_id` routes every later tool call (issue #95).
//! - Hot reload. When the source tree changes the shim runs `cargo build`;
//!   when the kwin-mcp binary changes it swaps in a new idle child and sends
//!   `notifications/tools/list_changed`. Live sessions keep running on the
//!   child they started on until they stop; new sessions get the new build:
//!   a `session_start` made while a build is pending waits for it, and every
//!   start checks the binary on disk rather than trusting the warm child.
//!   No client reconnect is needed.
//! - Self-upgrade. When the shim binary itself is rebuilt, the shim re-executes
//!   it in place (same pid, same client pipes) as soon as no session is live
//!   and no call is in flight. The client's initialize params and any client
//!   input not yet handled carry over; the client sees tools/list_changed.
//! - Supervision. A child that exits or stops answering pings is replaced; its
//!   leftover processes are killed, orphans reparented to the shim (it is a
//!   child subreaper) are reaped, and leaked session workdirs are swept.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

// ── Tunables ─────────────────────────────────────────────────────────────

/// Source and binary poll period for hot reload.
const WATCH_POLL: Duration = Duration::from_secs(2);
/// A source change must be quiet this long before a build starts, so an
/// editor or `git pull` writing many files triggers one build.
const BUILD_DEBOUNCE: Duration = Duration::from_secs(2);
/// Longest a session_start waits for a pending build before it runs on the
/// binary that is there.
const BUILD_WAIT: Duration = Duration::from_secs(600);
/// Supervisor tick: pings, wedge checks, respawn and orphan reaping.
const TICK: Duration = Duration::from_secs(1);
/// How often each ready child is pinged.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// A child that has not answered a ping or finished initializing within this
/// long is wedged and gets replaced.
const WEDGE_TIMEOUT: Duration = Duration::from_secs(90);
/// SIGTERM to SIGKILL grace for wedged children and leftover processes.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Idle respawn backoff after repeated crashes inside CRASH_WINDOW.
const CRASH_WINDOW: Duration = Duration::from_secs(30);
const CRASH_LIMIT: usize = 3;
const CRASH_BACKOFF: Duration = Duration::from_secs(10);
/// Periodic leaked-workdir sweep.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);
/// How long shutdown waits for children to tear their sessions down.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(30);
/// Prefix of every JSON-RPC id the shim itself owns on a child connection.
const SHIM_ID: &str = "__kwin_shim:";
const DEFAULT_PROTOCOL: &str = "2025-06-18";
/// Carries the client connection state across a self-upgrade exec.
const RESUME_ENV: &str = "KWIN_MCP_SHIM_RESUME";

// ── Small helpers ────────────────────────────────────────────────────────

/// Identity of a file version: inode, size and mtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    ino: u64,
    len: u64,
    mtime: Option<SystemTime>,
}

fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp { ino: meta.ino(), len: meta.len(), mtime: meta.modified().ok() })
}

fn log(message: &str) {
    eprintln!("kwin-mcp-shim: {message}");
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn tool_error(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

fn signal(pid: u32, sig: nix::sys::signal::Signal) {
    if let Ok(raw) = i32::try_from(pid) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), sig);
    }
}

/// Wait for killed children to exit and reap them, giving up at `deadline`
/// (a child stuck in the kernel stays a zombie rather than blocking the shim).
fn reap(pids: &[u32], deadline: Instant) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    let mut left: Vec<nix::unistd::Pid> = pids.iter()
        .filter_map(|&pid| i32::try_from(pid).ok().map(nix::unistd::Pid::from_raw))
        .collect();
    while !left.is_empty() && Instant::now() < deadline {
        left.retain(|&pid| matches!(waitpid(pid, Some(WaitPidFlag::WNOHANG)), Ok(WaitStatus::StillAlive)));
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── Events ───────────────────────────────────────────────────────────────

enum Event {
    Client(Value),
    ClientClosed,
    Child(u64, Value),
    ChildExited(u64, String),
    BinaryChanged(Stamp),
    /// The watcher saw a source change (true) or finished building it (false).
    Building(bool),
    Tick,
}

// ── Children ─────────────────────────────────────────────────────────────

struct Child {
    pid: u32,
    /// Dropping the sender closes the child's stdin, which ends its MCP
    /// transport and runs its normal session cleanup.
    stdin: Option<mpsc::UnboundedSender<String>>,
    build: Stamp,
    spawned: Instant,
    ready: bool,
    backlog: Vec<String>,
    /// Session id once this child's session_start succeeded.
    session: Option<String>,
    tools: HashSet<String>,
    /// The full tool definitions this child serves.
    tool_defs: Vec<Value>,
    inflight: usize,
    retiring: bool,
    ping_sent: Option<Instant>,
    last_ping: Instant,
    last_used: Instant,
    killed_at: Option<Instant>,
}

impl Child {
    fn send_raw(&self, line: String) {
        if let Some(stdin) = &self.stdin {
            let _ = stdin.send(line);
        }
    }
    fn send(&mut self, line: String) {
        if self.ready { self.send_raw(line) } else { self.backlog.push(line) }
    }
}

#[derive(Clone)]
enum FlightKind {
    Plain,
    Start(String),
    Stop(String),
}

struct Flight {
    child: u64,
    id: Value,
    is_call: bool,
    kind: FlightKind,
}

// ── Shim state ───────────────────────────────────────────────────────────

struct Shim {
    out: mpsc::UnboundedSender<String>,
    events: mpsc::UnboundedSender<Event>,
    sweep: mpsc::UnboundedSender<()>,
    child_bin: PathBuf,
    child_args: Vec<String>,
    children: HashMap<u64, Child>,
    next_child: u64,
    /// The warm child with no session: answers tools/list and calls made
    /// before any session exists, and becomes the next session's child.
    idle: Option<u64>,
    sessions: BTreeMap<String, u64>,
    ended: HashMap<String, String>,
    inflight: HashMap<String, Flight>,
    reverse: HashMap<String, (u64, Value)>,
    next_reverse: u64,
    client_params: Option<Value>,
    pending_init: Option<Value>,
    pending_lists: Vec<Value>,
    init_result: Option<Value>,
    tools: Option<Vec<Value>>,
    /// Tool definitions of the newest idle child, before rewriting.
    base_tools: Vec<Value>,
    client_ready: bool,
    latest: Option<Stamp>,
    /// A source change is waiting for, or running, its cargo build.
    building: bool,
    /// session_start calls held until that build finishes.
    held_starts: Vec<(Value, Value, Instant)>,
    crashes: VecDeque<Instant>,
    respawn_after: Option<Instant>,
    /// Session ids (= pids) of children that exited; leftover processes in
    /// those process sessions are killed.
    dead_sids: HashSet<i32>,
    orphans: HashMap<i32, Instant>,
    closing: bool,
    /// This shim's own executable, and its identity when it started and at
    /// the last tick; a rebuilt shim re-executes itself once idle.
    self_path: PathBuf,
    self_stamp: Option<Stamp>,
    self_seen: Option<Stamp>,
    /// Set once the shim has decided to re-execute: the client reader is
    /// frozen and client messages are carried over instead of handled.
    upgrading: bool,
    freeze: std::sync::Arc<std::sync::atomic::AtomicBool>,
    carried: Vec<Value>,
}

impl Shim {
    fn write(&self, value: &Value) {
        let _ = self.out.send(value.to_string());
    }

    fn reply(&self, id: Value, result: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn reply_error(&self, id: Value, code: i64, message: String) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    fn spawn_child(&mut self) -> Option<u64> {
        let params = self.client_params.clone().unwrap_or_else(|| json!({
            "protocolVersion": DEFAULT_PROTOCOL,
            "capabilities": {},
            "clientInfo": { "name": "kwin-mcp-shim", "version": env!("CARGO_PKG_VERSION") },
        }));
        let build = stamp(&self.child_bin)?;
        let mut command = tokio::process::Command::new(&self.child_bin);
        command
            .args(&self.child_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        // Own process session, so everything a dead child leaves behind can be
        // found by session id; and die with the shim if it is killed outright.
        unsafe {
            command.pre_exec(|| {
                nix::libc::setsid();
                nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGTERM);
                Ok(())
            });
        }
        let mut process = match command.spawn() {
            Ok(process) => process,
            Err(error) => {
                log(&format!("spawn {} failed: {error}", self.child_bin.display()));
                return None;
            }
        };
        let (Some(pid), Some(stdin), Some(stdout)) = (process.id(), process.stdin.take(), process.stdout.take()) else {
            let _ = process.start_kill();
            return None;
        };
        let key = self.next_child;
        self.next_child += 1;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => { let _ = events.send(Event::Child(key, value)); }
                    Err(_) => log(&format!("child {pid}: non-JSON output: {line}")),
                }
            }
        });
        let events = self.events.clone();
        tokio::spawn(async move {
            let status = match process.wait().await {
                Ok(status) => status.to_string(),
                Err(error) => format!("wait failed: {error}"),
            };
            let _ = events.send(Event::ChildExited(key, status));
        });
        let now = Instant::now();
        let child = Child {
            pid, stdin: Some(tx), build, spawned: now, ready: false, backlog: Vec::new(),
            session: None, tools: HashSet::new(), tool_defs: Vec::new(), inflight: 0, retiring: false,
            ping_sent: None, last_ping: now, last_used: now, killed_at: None,
        };
        child.send_raw(json!({ "jsonrpc": "2.0", "id": format!("{SHIM_ID}init"), "method": "initialize", "params": params }).to_string());
        self.children.insert(key, child);
        log(&format!("started child {pid}"));
        Some(key)
    }

    /// Make sure a warm idle child exists, spawning one if needed.
    fn ensure_idle(&mut self) -> Option<u64> {
        if let Some(key) = self.idle && self.children.contains_key(&key) {
            return Some(key);
        }
        if self.closing || self.respawn_after.is_some_and(|at| Instant::now() < at) {
            return None;
        }
        self.idle = self.spawn_child();
        self.idle
    }

    /// Stop a child once its in-flight requests have answered.
    fn retire(&mut self, key: u64) {
        if self.idle == Some(key) {
            self.idle = None;
        }
        if let Some(child) = self.children.get_mut(&key) {
            child.retiring = true;
            if child.inflight == 0 {
                child.stdin = None;
            }
        }
    }

    fn forward(&mut self, key: u64, id: Value, is_call: bool, kind: FlightKind, message: &Value) {
        let Some(child) = self.children.get_mut(&key) else {
            self.reply_error(id, -32603, "kwin-mcp child vanished".to_owned());
            return;
        };
        child.inflight += 1;
        child.last_used = Instant::now();
        child.send(message.to_string());
        self.inflight.insert(id_key(&id), Flight { child: key, id, is_call, kind });
    }

    fn live_list(&self) -> String {
        if self.sessions.is_empty() {
            return "none".to_owned();
        }
        self.sessions.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    /// Which child serves a call, from its optional session_id.
    fn resolve(&mut self, session: Option<&str>) -> Result<u64, String> {
        if let Some(session) = session {
            if let Some(key) = self.sessions.get(session) {
                return Ok(*key);
            }
            if let Some(why) = self.ended.get(session) {
                return Err(format!("session {session} has ended: {why}. Call session_start for a new session."));
            }
            return Err(format!("unknown session_id '{session}'. Live sessions: {}.", self.live_list()));
        }
        let mut live = self.sessions.values();
        match (live.next(), live.next()) {
            (Some(key), None) => Ok(*key),
            (None, None) => self.ensure_idle().ok_or_else(|| "kwin-mcp is restarting; retry in a few seconds".to_owned()),
            (Some(_), Some(_)) | (None, Some(_)) => Err(format!(
                "several sessions are live ({}); pass session_id to choose one. Use the session_id your own session_start returned.",
                self.live_list()
            )),
        }
    }

    // ── Client side ──────────────────────────────────────────────────────

    fn on_client(&mut self, message: Value) {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => self.client_request(&method, id, message),
            (Some(method), None) => self.client_notification(&method, message),
            (None, Some(id)) => self.client_response(id, message),
            (None, None) => {}
        }
    }

    fn client_request(&mut self, method: &str, id: Value, message: Value) {
        match method {
            "initialize" => {
                self.client_params = message.get("params").cloned();
                if let Some(result) = self.client_init_result() {
                    self.reply(id, result);
                } else {
                    self.pending_init = Some(id);
                    self.ensure_idle();
                }
            }
            "ping" => self.reply(id, json!({})),
            "tools/list" => match &self.tools {
                Some(tools) => self.reply(id, json!({ "tools": tools })),
                None => {
                    self.pending_lists.push(id);
                    self.ensure_idle();
                }
            },
            "tools/call" => self.route_call(id, message),
            _ => match self.ensure_idle() {
                Some(key) => self.forward(key, id, false, FlightKind::Plain, &message),
                None => self.reply_error(id, -32603, "kwin-mcp is restarting".to_owned()),
            },
        }
    }

    fn route_call(&mut self, id: Value, mut message: Value) {
        let name = message.pointer("/params/name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let session = message
            .pointer_mut("/params/arguments")
            .and_then(Value::as_object_mut)
            .and_then(|arguments| arguments.remove("session_id"))
            .and_then(|value| match value {
                Value::String(text) => Some(text),
                Value::Number(number) => Some(number.to_string()),
                Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
            })
            .filter(|text| !text.is_empty());
        if name == "session_list" {
            let listing = self.session_listing();
            self.reply(id, json!({ "content": [{ "type": "text", "text": listing }] }));
            return;
        }
        if name == "session_start" && session.is_none() {
            self.start_session(id, message);
            return;
        }
        let key = match self.resolve(session.as_deref()) {
            Ok(key) => key,
            Err(text) => {
                self.reply(id, tool_error(text));
                return;
            }
        };
        if let Some(child) = self.children.get(&key)
            && child.ready && !child.tools.is_empty() && !child.tools.contains(&name)
            && self.tools.as_ref().is_some_and(|tools| tools.iter().any(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str())))
        {
            self.reply(id, tool_error(format!(
                "tool {name} is newer than the kwin-mcp build this session started on. \
                 session_stop and session_start to get the current build."
            )));
            return;
        }
        let kind = match (name.as_str(), self.children.get(&key).and_then(|child| child.session.clone())) {
            ("session_stop", Some(session)) => FlightKind::Stop(session),
            _ => FlightKind::Plain,
        };
        self.forward(key, id, true, kind, &message);
    }

    /// Start a new session on a fresh child running the binary on disk now.
    /// While a source change is still building, the call waits for it.
    fn start_session(&mut self, id: Value, message: Value) {
        if self.building {
            log("session_start waits for the kwin-mcp build in progress");
            self.held_starts.push((id, message, Instant::now()));
            return;
        }
        // The watcher reports a new binary a few seconds late; look now.
        let current = stamp(&self.child_bin);
        if let Some(build) = current {
            self.on_binary_changed(build);
        }
        if let Some(key) = self.idle
            && self.children.get(&key).is_some_and(|child| Some(child.build) != current)
        {
            self.retire(key);
        }
        // Every new session gets a fresh child of its own.
        let key = match self.idle.take().filter(|key| self.children.contains_key(key)) {
            Some(key) => Some(key),
            None => self.spawn_child(),
        };
        self.ensure_idle();
        let Some(key) = key else {
            self.reply(id, tool_error("kwin-mcp could not start a server process; see the MCP server log".to_owned()));
            return;
        };
        let session_id = self.children.get(&key).map(|child| format!("s{}", child.pid)).unwrap_or_default();
        self.forward(key, id, true, FlightKind::Start(session_id), &message);
    }

    fn on_building(&mut self, active: bool) {
        self.building = active;
        if !active {
            self.release_held_starts();
        }
    }

    fn release_held_starts(&mut self) {
        let held = std::mem::take(&mut self.held_starts);
        let waiting = self.building;
        self.building = false;
        for (id, message, _) in held {
            self.start_session(id, message);
        }
        self.building = waiting;
    }

    fn session_listing(&self) -> String {
        if self.sessions.is_empty() {
            return "No live sessions. session_start creates one and returns its session_id.".to_owned();
        }
        let now = Instant::now();
        let mut lines = Vec::new();
        for (session, key) in &self.sessions {
            let Some(child) = self.children.get(key) else { continue };
            let current = if Some(child.build) == self.latest { "current build" } else { "older build (restart the session to update)" };
            lines.push(format!(
                "{session}: server pid {}, workdir /tmp/kwin-mcp-{}, age {}s, idle {}s, {current}",
                child.pid, child.pid,
                now.duration_since(child.spawned).as_secs(),
                now.duration_since(child.last_used).as_secs(),
            ));
        }
        lines.join("\n")
    }

    fn client_notification(&mut self, method: &str, message: Value) {
        match method {
            "notifications/initialized" => self.client_ready = true,
            "notifications/cancelled" => {
                let target = message.pointer("/params/requestId").map(id_key)
                    .and_then(|key| self.inflight.get(&key)).map(|flight| flight.child);
                if let Some(child) = target.and_then(|key| self.children.get_mut(&key)) {
                    child.send(message.to_string());
                }
            }
            _ => {
                for child in self.children.values_mut() {
                    child.send(message.to_string());
                }
            }
        }
    }

    fn client_response(&mut self, id: Value, mut message: Value) {
        let Some(key) = id.as_str() else { return };
        let Some((child, original)) = self.reverse.remove(key) else { return };
        if let Some(object) = message.as_object_mut() {
            object.insert("id".to_owned(), original);
        }
        if let Some(child) = self.children.get(&child) {
            child.send_raw(message.to_string());
        }
    }

    // ── Child side ───────────────────────────────────────────────────────

    fn on_child(&mut self, key: u64, mut message: Value) {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                if let Some(own) = id.as_str().and_then(|text| text.strip_prefix(SHIM_ID)) {
                    let own = own.to_owned();
                    self.own_response(key, &own, message);
                } else {
                    self.child_response(key, id, message);
                }
            }
            (Some(_), Some(id)) => {
                // A request from the child to the client: give it an id that
                // cannot collide with another child's.
                let alias = format!("{SHIM_ID}rev:{}", self.next_reverse);
                self.next_reverse += 1;
                self.reverse.insert(alias.clone(), (key, id));
                if let Some(object) = message.as_object_mut() {
                    object.insert("id".to_owned(), Value::String(alias));
                }
                self.write(&message);
            }
            (Some(method), None) => {
                if method == "notifications/tools/list_changed" {
                    if let Some(child) = self.children.get(&key) {
                        child.send_raw(json!({ "jsonrpc": "2.0", "id": format!("{SHIM_ID}tools"), "method": "tools/list" }).to_string());
                    }
                } else {
                    self.write(&message);
                }
            }
            (None, None) => {}
        }
    }

    fn own_response(&mut self, key: u64, which: &str, message: Value) {
        let Some(child) = self.children.get_mut(&key) else { return };
        match which {
            "init" => {
                if let Some(result) = message.get("result") {
                    if self.idle == Some(key) || self.init_result.is_none() {
                        self.init_result = Some(result.clone());
                    }
                    child.send_raw(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string());
                    child.send_raw(json!({ "jsonrpc": "2.0", "id": format!("{SHIM_ID}tools"), "method": "tools/list" }).to_string());
                } else {
                    log(&format!("child {} rejected initialize: {message}", child.pid));
                    child.stdin = None;
                }
            }
            "tools" => {
                let tools = message.pointer("/result/tools").and_then(Value::as_array).cloned().unwrap_or_default();
                child.tools = tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned)).collect();
                child.tool_defs = tools.clone();
                if !child.ready {
                    child.ready = true;
                    for line in std::mem::take(&mut child.backlog) {
                        child.send_raw(line);
                    }
                }
                if self.idle == Some(key) || self.tools.is_none() {
                    self.base_tools = tools;
                }
                self.publish_tools();
                if let Some(id) = self.pending_init.take() {
                    match self.client_init_result() {
                        Some(result) => self.reply(id, result),
                        None => self.pending_init = Some(id),
                    }
                }
            }
            "ping" => child.ping_sent = None,
            _ => {}
        }
    }

    fn child_response(&mut self, key: u64, id: Value, mut message: Value) {
        let Some(flight) = self.inflight.remove(&id_key(&id)) else {
            self.write(&message);
            return;
        };
        if let Some(child) = self.children.get_mut(&key) {
            child.inflight = child.inflight.saturating_sub(1);
            child.last_used = Instant::now();
        }
        match flight.kind {
            FlightKind::Plain => {}
            FlightKind::Start(session) => {
                let ok = message.get("result").is_some_and(|result| result.get("isError").and_then(Value::as_bool) != Some(true));
                if ok {
                    self.sessions.insert(session.clone(), key);
                    if let Some(child) = self.children.get_mut(&key) {
                        child.session = Some(session.clone());
                    }
                    if let Some(result) = message.get_mut("result").and_then(Value::as_object_mut) {
                        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
                            content.push(json!({ "type": "text", "text": format!(
                                "session_id: {session}\nThis is your own isolated session. Pass session_id=\"{session}\" \
                                 to every other kwin-mcp tool call you make for it, and to session_stop when done."
                            ) }));
                        }
                        if let Some(structured) = result.get_mut("structuredContent").and_then(Value::as_object_mut) {
                            structured.insert("session_id".to_owned(), Value::String(session.clone()));
                        }
                    }
                    log(&format!("session {session} started"));
                } else {
                    self.retire(key);
                }
            }
            FlightKind::Stop(session) => {
                self.sessions.remove(&session);
                self.ended.insert(session.clone(), "it was stopped with session_stop".to_owned());
                log(&format!("session {session} stopped"));
                if let Some(child) = self.children.get_mut(&key) {
                    child.session = None;
                }
                self.retire(key);
                self.publish_tools();
            }
        }
        if let Some(child) = self.children.get_mut(&key)
            && child.retiring && child.inflight == 0
        {
            child.stdin = None;
        }
        self.write(&message);
        // Right after a session_stop, before the next session_start is read.
        self.maybe_upgrade(false);
    }

    fn client_init_result(&self) -> Option<Value> {
        let mut result = self.init_result.clone()?;
        let object = result.as_object_mut()?;
        let capabilities = object.entry("capabilities").or_insert_with(|| json!({}));
        if let Some(capabilities) = capabilities.as_object_mut() {
            capabilities.insert("tools".to_owned(), json!({ "listChanged": true }));
        }
        let note = "\n\nSessions (kwin-mcp-shim): every session_start call creates a new isolated \
                    session with its own display, windows, keyboard and mouse, and returns a session_id. \
                    Pass that session_id to every other tool. It may be omitted only while exactly one \
                    session is live. session_list shows the live sessions. Parallel agents and subagents \
                    must each call session_start and use their own session_id.";
        let instructions = object.get("instructions").and_then(Value::as_str).unwrap_or_default().to_owned();
        object.insert("instructions".to_owned(), Value::String(instructions + note));
        Some(result)
    }

    /// Publish the newest build's tools plus any tool a live session on an
    /// older build still serves, rewritten for routing; cache the list, answer
    /// waiting tools/list requests, and tell the client when it changed.
    fn publish_tools(&mut self) {
        let mut tools = self.base_tools.clone();
        let mut names: HashSet<String> = tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned)).collect();
        for child in self.children.values().filter(|child| child.session.is_some()) {
            for tool in &child.tool_defs {
                let Some(name) = tool.get("name").and_then(Value::as_str) else { continue };
                if names.insert(name.to_owned()) {
                    let mut tool = tool.clone();
                    if let Some(object) = tool.as_object_mut() {
                        let description = object.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
                        object.insert("description".to_owned(), Value::String(description + " (Served only by sessions started on an older kwin-mcp build.)"));
                    }
                    tools.push(tool);
                }
            }
        }
        let mut rewritten: Vec<Value> = tools.iter().map(|tool| {
            let mut tool = tool.clone();
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            if let Some(object) = tool.as_object_mut() {
                let extra = if name == "session_start" {
                    " Each call without session_id creates a NEW isolated session (its own display, \
                     windows, keyboard focus and mouse) and returns its session_id; pass an existing \
                     session_id to get that session's already_running status instead."
                } else {
                    " Pass the session_id from your session_start; it may be omitted only while exactly one session is live."
                };
                let description = object.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
                object.insert("description".to_owned(), Value::String(description + extra));
                let schema = object.entry("inputSchema").or_insert_with(|| json!({ "type": "object" }));
                if let Some(schema) = schema.as_object_mut() {
                    let properties = schema.entry("properties").or_insert_with(|| json!({}));
                    if let Some(properties) = properties.as_object_mut() {
                        properties.insert("session_id".to_owned(), json!({
                            "type": "string",
                            "description": "Session handle returned by session_start (for example \"s12345\").",
                        }));
                    }
                }
            }
            tool
        }).collect();
        if !rewritten.iter().any(|tool| tool.get("name").and_then(Value::as_str) == Some("session_list")) {
            rewritten.push(json!({
                "name": "session_list",
                "description": "List the live kwin-mcp sessions with their session_id, server pid, workdir, age, idle time and whether they run the current build.",
                "inputSchema": { "type": "object", "properties": {} },
            }));
        }
        let changed = self.tools.as_ref() != Some(&rewritten);
        self.tools = Some(rewritten.clone());
        for id in std::mem::take(&mut self.pending_lists) {
            self.reply(id, json!({ "tools": rewritten }));
        }
        if changed && self.client_ready {
            self.write(&json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
            log("tool list changed; notified the client");
        }
    }

    fn on_child_exit(&mut self, key: u64, status: &str) {
        let Some(child) = self.children.remove(&key) else { return };
        let expected = child.retiring || self.closing;
        log(&format!("child {} exited ({status}){}", child.pid, if expected { "" } else { " unexpectedly" }));
        if let Some(session) = &child.session
            && self.sessions.get(session) == Some(&key)
        {
            self.sessions.remove(session);
            self.ended.insert(session.clone(), format!(
                "its kwin-mcp server exited ({status}), so its desktop and apps are gone"
            ));
            self.publish_tools();
        }
        let failed: Vec<String> = self.inflight.iter().filter(|(_, flight)| flight.child == key).map(|(k, _)| k.clone()).collect();
        for flight_key in failed {
            if let Some(flight) = self.inflight.remove(&flight_key) {
                let text = format!("the kwin-mcp server handling this call exited ({status}). Call session_start for a new session.");
                if flight.is_call {
                    self.reply(flight.id, tool_error(text));
                } else {
                    self.reply_error(flight.id, -32603, text);
                }
            }
        }
        self.reverse.retain(|_, (owner, _)| *owner != key);
        if let Ok(sid) = i32::try_from(child.pid) {
            self.dead_sids.insert(sid);
        }
        if self.idle == Some(key) {
            self.idle = None;
            if !expected {
                let now = Instant::now();
                self.crashes.push_back(now);
                while self.crashes.front().is_some_and(|at| now.duration_since(*at) > CRASH_WINDOW) {
                    self.crashes.pop_front();
                }
                if self.crashes.len() >= CRASH_LIMIT {
                    log("idle child keeps crashing; backing off");
                    self.respawn_after = Some(now + CRASH_BACKOFF);
                }
            }
        }
        let _ = self.sweep.send(());
    }

    fn on_binary_changed(&mut self, build: Stamp) {
        if self.latest == Some(build) {
            return;
        }
        let first = self.latest.is_none();
        self.latest = Some(build);
        if first {
            return;
        }
        log("kwin-mcp binary changed; swapping in a new idle child (live sessions keep their child)");
        if let Some(old) = self.idle.take() {
            self.retire(old);
        }
        if self.client_params.is_some() {
            self.respawn_after = None;
            self.ensure_idle();
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if self.client_params.is_some() && !self.closing {
            if self.respawn_after.is_some_and(|at| now >= at) {
                self.respawn_after = None;
            }
            self.ensure_idle();
        }
        if self.held_starts.first().is_some_and(|(_, _, at)| now.duration_since(*at) > BUILD_WAIT) {
            log("kwin-mcp build is taking too long; starting held sessions on the current binary");
            self.release_held_starts();
        }
        let mut wedged = Vec::new();
        for (key, child) in &mut self.children {
            if let Some(killed) = child.killed_at {
                if now.duration_since(killed) > KILL_GRACE {
                    signal(child.pid, nix::sys::signal::Signal::SIGKILL);
                }
                continue;
            }
            let stuck = match (child.ready, child.ping_sent) {
                (false, _) => now.duration_since(child.spawned) > WEDGE_TIMEOUT,
                (true, Some(sent)) => now.duration_since(sent) > WEDGE_TIMEOUT,
                (true, None) => {
                    if now.duration_since(child.last_ping) >= PING_INTERVAL {
                        child.last_ping = now;
                        child.ping_sent = Some(now);
                        child.send_raw(json!({ "jsonrpc": "2.0", "id": format!("{SHIM_ID}ping"), "method": "ping" }).to_string());
                    }
                    false
                }
            };
            if stuck {
                wedged.push(*key);
            }
        }
        for key in wedged {
            if let Some(child) = self.children.get_mut(&key) {
                log(&format!("child {} is wedged (no answer for {}s); killing it", child.pid, WEDGE_TIMEOUT.as_secs()));
                child.killed_at = Some(now);
                signal(child.pid, nix::sys::signal::Signal::SIGTERM);
            }
        }
        self.reap_leftovers(now);
        self.maybe_upgrade(true);
    }

    /// Kill what dead children left behind and reap orphaned zombies. Only
    /// processes in a dead child's process session are touched, so daemons
    /// that live children spawned stay alone.
    fn reap_leftovers(&mut self, now: Instant) {
        if self.dead_sids.is_empty() {
            return;
        }
        let Ok(processes) = procfs::process::all_processes() else { return };
        let me = i32::try_from(std::process::id()).unwrap_or_default();
        let mut present = HashSet::new();
        for process in processes.flatten() {
            let Ok(stat) = process.stat() else { continue };
            if !self.dead_sids.contains(&stat.session) || stat.pid == me {
                continue;
            }
            present.insert(stat.pid);
            if stat.state == 'Z' {
                if stat.ppid == me {
                    let _ = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(stat.pid), Some(nix::sys::wait::WaitPidFlag::WNOHANG));
                }
                continue;
            }
            let pid = nix::unistd::Pid::from_raw(stat.pid);
            match self.orphans.get(&stat.pid) {
                None => {
                    log(&format!("killing leftover process {} ({}) from a dead child", stat.pid, stat.comm));
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
                    self.orphans.insert(stat.pid, now);
                }
                Some(first) if now.duration_since(*first) > KILL_GRACE => {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                }
                Some(_) => {}
            }
        }
        self.orphans.retain(|pid, _| present.contains(pid));
        // A dead child's session is finished once nothing in it remains, and
        // its sid cannot be reused while any member is alive.
        let live_sids: HashSet<i32> = present.iter().filter_map(|pid| procfs::process::Process::new(*pid).ok()?.stat().ok().map(|stat| stat.session)).collect();
        self.dead_sids.retain(|sid| live_sids.contains(sid));
    }

    /// Re-execute a rebuilt shim once nothing is live: no session, no call in
    /// flight, nothing waiting. The client connection carries over.
    /// Stability is judged across ticks; other callers only look.
    fn maybe_upgrade(&mut self, tick: bool) {
        let current = stamp(&self.self_path);
        let stable = current.is_some() && current == self.self_seen;
        if tick {
            self.self_seen = current;
        }
        let quiet = self.sessions.is_empty() && self.inflight.is_empty() && self.reverse.is_empty()
            && self.held_starts.is_empty() && self.pending_init.is_none() && self.pending_lists.is_empty();
        if !stable || current == self.self_stamp || self.building || self.closing || !self.client_ready || !quiet {
            return;
        }
        log("kwin-mcp-shim was rebuilt; re-executing it (no live sessions, the client stays connected)");
        self.freeze.store(true, std::sync::atomic::Ordering::SeqCst);
        self.upgrading = true;
        self.begin_shutdown();
    }

    fn begin_shutdown(&mut self) {
        self.closing = true;
        let keys: Vec<u64> = self.children.keys().copied().collect();
        for key in keys {
            if let Some(child) = self.children.get_mut(&key) {
                child.retiring = true;
                child.stdin = None;
            }
        }
        self.idle = None;
    }
}

// ── Hot reload and sweeping ──────────────────────────────────────────────

fn newest_mtime(path: &Path, newest: &mut Option<SystemTime>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                newest_mtime(&entry.path(), newest);
            }
        }
    } else if let Ok(modified) = meta.modified()
        && newest.is_none_or(|current| modified > current)
    {
        *newest = Some(modified);
    }
}

fn source_mtime(repo: &Path) -> Option<SystemTime> {
    let mut newest = None;
    for part in ["src", "Cargo.toml", "Cargo.lock", "build.rs"] {
        newest_mtime(&repo.join(part), &mut newest);
    }
    newest
}

async fn cargo_build(repo: &Path, release: bool) {
    let log_dir = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(".cache/kwin-mcp-shim");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("build.log");
    let Ok(file) = std::fs::File::create(&log_path) else { return };
    let Ok(file_err) = file.try_clone() else { return };
    let mut command = tokio::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.arg("build").current_dir(repo).stdin(std::process::Stdio::null()).stdout(file).stderr(file_err);
    if release {
        command.arg("--release");
    }
    log(&format!("source changed; running cargo build in {} (log: {})", repo.display(), log_path.display()));
    match command.status().await {
        Ok(status) if status.success() => log("cargo build succeeded"),
        Ok(status) => log(&format!("cargo build failed ({status}); keeping the current build. See {}", log_path.display())),
        Err(error) => log(&format!("could not run cargo: {error}")),
    }
}

async fn watch(repo: Option<PathBuf>, release: bool, binary: PathBuf, events: mpsc::UnboundedSender<Event>) {
    let bin_mtime = stamp(&binary).and_then(|found| found.mtime);
    let mut seen_source = repo.as_deref().and_then(source_mtime);
    // Build at startup when the tree is newer than the binary.
    let mut built_source = match (seen_source, bin_mtime) {
        (Some(source), Some(bin)) if source > bin => None,
        (source, _) => source,
    };
    let mut source_changed = Instant::now();
    let mut last_binary = None;
    let mut building = false;
    loop {
        if let Some(repo) = &repo {
            let current = source_mtime(repo);
            // Hold new sessions from the first sight of a change until its
            // build is done, so none start on the binary about to be replaced.
            let pending = current.is_some() && current != built_source;
            if pending != building {
                building = pending;
                let _ = events.send(Event::Building(pending));
            }
            if current != seen_source {
                seen_source = current;
                source_changed = Instant::now();
            } else if pending && source_changed.elapsed() >= BUILD_DEBOUNCE {
                built_source = current;
                cargo_build(repo, release).await;
                if let Some(found) = stamp(&binary) {
                    let _ = events.send(Event::BinaryChanged(found));
                }
                if source_mtime(repo) == built_source {
                    building = false;
                    let _ = events.send(Event::Building(false));
                }
            }
        }
        // Only report a binary that has been stable for a full poll.
        let current = stamp(&binary);
        if let Some(found) = current
            && current == last_binary
        {
            let _ = events.send(Event::BinaryChanged(found));
        }
        last_binary = current;
        tokio::time::sleep(WATCH_POLL).await;
    }
}

async fn sweeper(binary: PathBuf, mut trigger: mpsc::UnboundedReceiver<()>) {
    loop {
        let _ = tokio::time::timeout(SWEEP_INTERVAL, trigger.recv()).await;
        // Coalesce bursts, and give a dead child's container a moment to go.
        tokio::time::sleep(Duration::from_secs(3)).await;
        while trigger.try_recv().is_ok() {}
        let status = tokio::process::Command::new(&binary)
            .arg("--sweep-workdirs")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .await;
        if let Err(error) = status {
            log(&format!("workdir sweep failed: {error}"));
        }
    }
}

// ── Client input ─────────────────────────────────────────────────────────

/// Read client lines from stdin until EOF or until `freeze` is set. Returns
/// the bytes of an unfinished line, so a re-executed shim can pick them up.
fn read_client(events: &mpsc::UnboundedSender<Event>, freeze: &std::sync::atomic::AtomicBool, mut pending: Vec<u8>) -> Vec<u8> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::AsFd;
    let stdin = std::io::stdin();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let text = line.get(..end).unwrap_or_default();
            if text.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(text) {
                Ok(value) => { let _ = events.send(Event::Client(value)); }
                Err(error) => log(&format!("ignoring malformed client line: {error}")),
            }
        }
        if freeze.load(std::sync::atomic::Ordering::SeqCst) {
            return pending;
        }
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(100u8)) {
            Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
            Ok(_) => {}
            Err(_) => break,
        }
        match nix::unistd::read(stdin.as_fd(), &mut buf) {
            Ok(0) => break,
            Ok(read) => pending.extend_from_slice(buf.get(..read).unwrap_or_default()),
            Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => {}
            Err(_) => break,
        }
    }
    let _ = events.send(Event::ClientClosed);
    Vec::new()
}

/// Replace this process with the rebuilt shim, handing it the client's
/// initialize params and every client byte not yet handled. Only returns on
/// failure.
fn reexec(path: &Path, args: &[String], client_params: Option<Value>, ended: &HashMap<String, String>, input: Vec<u8>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let state = json!({ "client_params": client_params, "ended": ended, "input": input });
    std::process::Command::new(path).args(args).env(RESUME_ENV, state.to_string()).exec()
}

// ── Main ─────────────────────────────────────────────────────────────────

fn locate() -> Result<(PathBuf, PathBuf, Option<PathBuf>, bool), String> {
    let exe = std::env::current_exe().map_err(|error| format!("current_exe: {error}"))?;
    let dir = exe.parent().ok_or("shim executable has no directory")?.to_path_buf();
    let binary = std::env::var_os("KWIN_MCP_BINARY").map(PathBuf::from).unwrap_or_else(|| dir.join("kwin-mcp"));
    let release = dir.file_name().is_some_and(|name| name == "release");
    let repo = std::env::var_os("KWIN_MCP_REPO").map(PathBuf::from)
        .or_else(|| dir.parent()?.parent().map(Path::to_path_buf))
        .filter(|repo| repo.join("Cargo.toml").is_file() && repo.join("src").is_dir());
    Ok((exe, binary, repo, release))
}

/// Connection state handed over by the shim that re-executed into this one.
struct Resume {
    client_params: Value,
    ended: HashMap<String, String>,
    input: Vec<u8>,
}

fn take_resume() -> Option<Resume> {
    let text = std::env::var(RESUME_ENV).ok()?;
    // Single-threaded here: the runtime starts after this.
    unsafe { std::env::remove_var(RESUME_ENV) };
    // Children the old image never reaped (older shims killed slow children
    // and re-executed without waiting) are zombies of this pid; no process is
    // spawned yet, so every exited child here is one of them.
    while let Ok(status) = nix::sys::wait::waitpid(None, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
        if status == nix::sys::wait::WaitStatus::StillAlive {
            break;
        }
    }
    let mut state: Value = serde_json::from_str(&text).ok()?;
    let client_params = state.get_mut("client_params").map(Value::take).filter(|params| !params.is_null())?;
    let ended = state.get_mut("ended").map(Value::take).and_then(|ended| serde_json::from_value(ended).ok()).unwrap_or_default();
    let input = state.get_mut("input").map(Value::take).and_then(|input| serde_json::from_value(input).ok()).unwrap_or_default();
    Some(Resume { client_params, ended, input })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let resume = take_resume();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(run(resume));
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn run(resume: Option<Resume>) -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
        // Orphans of dead children reparent to the shim so it can reap them.
        nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1);
    }
    let (self_path, binary, repo, release) = locate()?;
    log(&format!(
        "v{} {}relaying to {}; hot reload {}",
        env!("CARGO_PKG_VERSION"),
        if resume.is_some() { "(re-executed after a rebuild) " } else { "" },
        binary.display(),
        match &repo { Some(repo) => format!("watching {}", repo.display()), None => "watching the binary only".to_owned() },
    ));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let (sweep_tx, sweep_rx) = mpsc::unbounded_channel::<()>();

    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });
    let freeze = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let events = event_tx.clone();
        let freeze = freeze.clone();
        let input = resume.as_ref().map(|resume| resume.input.clone()).unwrap_or_default();
        std::thread::spawn(move || read_client(&events, &freeze, input))
    };
    let tick_events = event_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if tick_events.send(Event::Tick).is_err() { break }
        }
    });
    tokio::spawn(watch(repo, release, binary.clone(), event_tx.clone()));
    tokio::spawn(sweeper(binary.clone(), sweep_rx));
    let _ = sweep_tx.send(());

    let self_stamp = stamp(&self_path);
    let mut shim = Shim {
        out: out_tx, events: event_tx, sweep: sweep_tx,
        child_bin: binary, child_args: std::env::args().skip(1).collect(),
        children: HashMap::new(), next_child: 0, idle: None,
        sessions: BTreeMap::new(), ended: HashMap::new(), inflight: HashMap::new(),
        reverse: HashMap::new(), next_reverse: 0, client_params: None, pending_init: None,
        pending_lists: Vec::new(), init_result: None, tools: None, base_tools: Vec::new(), client_ready: false,
        latest: None, building: false, held_starts: Vec::new(), crashes: VecDeque::new(), respawn_after: None,
        dead_sids: HashSet::new(), orphans: HashMap::new(), closing: false,
        self_path, self_stamp, self_seen: self_stamp, upgrading: false, freeze: freeze.clone(), carried: Vec::new(),
    };
    shim.latest = stamp(&shim.child_bin);
    if let Some(resume) = resume {
        // The client already initialized; the first tool list the new idle
        // child reports goes out as tools/list_changed.
        shim.client_params = Some(resume.client_params);
        shim.client_ready = true;
        shim.ended = resume.ended;
        shim.ensure_idle();
    }

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut deadline: Option<Instant> = None;
    let mut client_closed = false;
    loop {
        let event = tokio::select! {
            event = event_rx.recv() => event,
            _ = sigterm.recv() => Some(Event::ClientClosed),
            _ = sigint.recv() => Some(Event::ClientClosed),
            _ = sighup.recv() => Some(Event::ClientClosed),
        };
        let Some(event) = event else { break };
        match event {
            Event::Client(message) if shim.upgrading => shim.carried.push(message),
            Event::Client(message) => shim.on_client(message),
            Event::ClientClosed => {
                client_closed = true;
                if deadline.is_none() || shim.upgrading {
                    log("client closed; stopping children");
                    shim.upgrading = false;
                    shim.begin_shutdown();
                    deadline = Some(Instant::now() + SHUTDOWN_WAIT);
                }
            }
            Event::Child(key, message) => shim.on_child(key, message),
            Event::ChildExited(key, status) => shim.on_child_exit(key, &status),
            Event::BinaryChanged(build) => shim.on_binary_changed(build),
            Event::Building(active) => shim.on_building(active),
            Event::Tick => shim.on_tick(),
        }
        if shim.upgrading && deadline.is_none() {
            deadline = Some(Instant::now() + SHUTDOWN_WAIT);
        }
        if let Some(at) = deadline {
            if shim.children.is_empty() {
                break;
            }
            if Instant::now() >= at {
                for child in shim.children.values() {
                    signal(child.pid, nix::sys::signal::Signal::SIGKILL);
                }
                // A re-exec drops the tasks that would wait on these children,
                // so reap them here or they stay zombies under the new shim.
                let pids: Vec<u32> = shim.children.values().map(|child| child.pid).collect();
                reap(&pids, Instant::now() + SHUTDOWN_WAIT);
                break;
            }
        }
    }
    if shim.upgrading && !client_closed {
        // Everything the client sent but this shim did not handle goes to the
        // new one, in order: queued messages, then unread bytes.
        let partial = tokio::task::spawn_blocking(move || reader.join().unwrap_or_default()).await.unwrap_or_default();
        while let Ok(event) = event_rx.try_recv() {
            if let Event::Client(message) = event {
                shim.carried.push(message);
            }
        }
        let mut input = Vec::new();
        for message in &shim.carried {
            input.extend_from_slice(message.to_string().as_bytes());
            input.push(b'\n');
        }
        input.extend_from_slice(&partial);
        let (path, args, params, ended) = (shim.self_path.clone(), shim.child_args.clone(), shim.client_params.clone(), shim.ended.clone());
        drop(shim);
        let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;
        let error = reexec(&path, &args, params, &ended, input);
        log(&format!("re-executing {} failed: {error}", path.display()));
        return Err(error.into());
    }
    drop(shim);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    Ok(())
}
