#![allow(clippy::expect_used)]

//! browser_navigate reports a page only after reading the tab's address back from
//! Chrome, with CDP and without it (#908). A local HTTP server records what Chrome
//! asked for, so a result is checked against the request Chrome really made.
//!
//! The server binary is `KWIN_MCP_E2E_SERVER` when set, else the one cargo built.
//! `KWIN_MCP_E2E_EVIDENCE` names a directory that receives a screenshot of each
//! landed page.
//!
//! The HTTP fixture is adapted from the one Nathaniel Phelps wrote for #863.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// A page that keeps ctrl+l and alt+d for itself, as a web terminal does (#853).
const TRAP: &str = "<script>addEventListener('keydown',e=>{if((e.ctrlKey&&e.key=='l')||(e.altKey&&e.key=='d'))e.preventDefault()},true)</script>";
/// A page whose handler holds ctrl+l for 1.5 s before Chrome gets it back, as a
/// busy page does: the address bar takes focus only after typing has begun (#865).
const STALL: &str = "<script>addEventListener('keydown',e=>{if(e.ctrlKey&&e.key=='l'){const t=Date.now();while(Date.now()-t<1500);}},true)</script>";
const SLOW_PAGE: Duration = Duration::from_secs(3);
/// A listing card: its image and its title link to one item through tracking
/// queries, as on a shop's result page (#957).
const LINKS: &str = "<a href='/item?id=7&track=aaaa#photo'><img alt=''></a><a href='/item?id=7&track=bbbb'>Item seven</a><a href='/other?ref=x'>Other</a>";

struct Fixture {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
        let port = listener.local_addr().expect("fixture address").port();
        listener.set_nonblocking(true).expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (worker_stop, worker_requests) = (stop.clone(), requests.clone());
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let requests = worker_requests.clone();
                        thread::spawn(move || Self::respond(stream, requests));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self {
            port,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn respond(mut stream: TcpStream, requests: Arc<Mutex<Vec<String>>>) {
        let _ = stream.set_nonblocking(false);
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("HTTP timeout");
        let mut reader = BufReader::new(stream.try_clone().expect("HTTP reader"));
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
        }
        requests.lock().expect("request log").push(path.clone());
        let page = |script: &str| {
            format!(
                "<!doctype html><title>fixture{path}</title><h1>fixture{path}</h1><input autofocus aria-label=page-field>{script}"
            )
        };
        let (status, extra, body) = match path.as_str() {
            "/download" => (
                "200 OK",
                "Content-Disposition: attachment; filename=issue908.txt\r\nContent-Type: application/octet-stream\r\n",
                "download fixture".to_owned(),
            ),
            "/redirect" => ("302 Found", "Location: /elsewhere\r\n", String::new()),
            "/favicon.ico" => ("404 Not Found", "", String::new()),
            "/trap" => ("200 OK", "Content-Type: text/html\r\n", page(TRAP)),
            "/stall" => ("200 OK", "Content-Type: text/html\r\n", page(STALL)),
            "/links" => ("200 OK", "Content-Type: text/html\r\n", page(LINKS)),
            _ => {
                if path == "/slow" {
                    thread::sleep(SLOW_PAGE);
                }
                ("200 OK", "Content-Type: text/html\r\n", page(""))
            }
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn requests_for(&self, path: &str) -> usize {
        self.requests
            .lock()
            .expect("request log")
            .iter()
            .filter(|request| *request == path)
            .count()
    }

    fn wait_for(&self, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.requests_for(path) == 0 {
            assert!(Instant::now() < deadline, "Chrome never asked for {path}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Wire {
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<Value>,
    early: HashMap<u64, Value>,
    next_id: u64,
    session: String,
    workdir: PathBuf,
    home: PathBuf,
}

impl Wire {
    fn start(name: &str) -> Self {
        Self::start_with(name, false)
    }

    /// With `reloadable`, the server runs a private copy of the binary, which
    /// it re-executes when the copy changes, as every install makes it do, and
    /// logs to `server.log` in its HOME.
    fn start_with(name: &str, reloadable: bool) -> Self {
        let home =
            std::env::temp_dir().join(format!("kwin-mcp-navigate-{name}-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
        let mut binary = PathBuf::from(
            std::env::var("KWIN_MCP_E2E_SERVER")
                .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned()),
        );
        let mut command = Command::new(&binary);
        command.stderr(Stdio::null());
        if reloadable {
            let copy = home.join("bin/kwin-mcp");
            std::fs::create_dir_all(home.join("bin")).expect("create the private bin directory");
            std::fs::copy(&binary, &copy).expect("copy kwin-mcp");
            binary = copy;
            command = Command::new(&binary);
            command
                .env("KWIN_MCP_BINARY", &binary)
                .stderr(std::fs::File::create(home.join("server.log")).expect("server log"));
        }
        let mut child = command
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let (send, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(message) = serde_json::from_str::<Value>(&line)
                    && send.send(message).is_err()
                {
                    break;
                }
            }
        });
        let mut wire = Self {
            child,
            stdin,
            replies,
            early: HashMap::new(),
            next_id: 0,
            session: String::new(),
            workdir: PathBuf::new(),
            home,
        };
        let id = wire.send(
            "initialize",
            json!({"protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"navigate-e2e","version":"2"}}),
        );
        wire.reply(id);
        writeln!(
            wire.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("initialized");
        let started = wire.tool("session_start", json!({"width":1280, "height":720}));
        let content = &started["result"]["structuredContent"];
        wire.session = content["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session_start: {started}"))
            .to_owned();
        wire.workdir = PathBuf::from(content["host_shared_dir"].as_str().expect("workdir"));
        wire
    }

    /// Send a request and return its id without waiting for the reply.
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .expect("send request");
        self.stdin.flush().expect("flush request");
        id
    }

    fn reply(&mut self, id: u64) -> Value {
        if let Some(reply) = self.early.remove(&id) {
            return reply;
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let reply = self
                .replies
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("no reply to request {id}: {error}"));
            match reply["id"].as_u64() {
                Some(got) if got == id => return reply,
                Some(got) => {
                    self.early.insert(got, reply);
                }
                None => {}
            }
        }
    }

    fn send_tool(&mut self, name: &str, mut arguments: Value) -> u64 {
        if !self.session.is_empty() {
            arguments["session_id"] = json!(self.session);
        }
        self.send("tools/call", json!({"name":name, "arguments":arguments}))
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.send_tool(name, arguments);
        let reply = self.reply(id);
        let shown: String = reply.to_string().chars().take(900).collect();
        eprintln!("{name}: {shown}");
        reply
    }

    fn navigate(&mut self, url: &str) -> Value {
        self.tool("browser_navigate", json!({"url":url, "speed":3}))
    }

    fn windows(&mut self) -> Vec<Value> {
        self.tool("window_list", json!({}))["result"]["structuredContent"]["windows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The id of the window whose title contains `title`, once there is one.
    fn window_titled(&mut self, title: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let windows = self.windows();
            for window in &windows {
                if window["title"].as_str().unwrap_or_default().contains(title) {
                    return window["id"].as_str().expect("window id").to_owned();
                }
            }
            assert!(Instant::now() < deadline, "no window titled {title}: {windows:?}");
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn launch(&mut self, command: String) -> Value {
        let launched = self.tool(
            "launch_app",
            json!({"command":command, "window_timeout_seconds":40}),
        );
        assert!(launched["error"].is_null(), "{launched}");
        launched
    }

    /// Gives the server's private binary a new modification time and waits
    /// until the server has re-executed itself with the session (#957).
    fn reexecute(&mut self) {
        let binary = self.home.join("bin/kwin-mcp");
        std::fs::File::open(&binary)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()))
            .expect("touch the server binary");
        let log = self.home.join("server.log");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("reload: reattached 1 sessions")
        {
            assert!(Instant::now() < deadline, "the server did not re-execute");
            thread::sleep(Duration::from_millis(250));
        }
    }

    /// browser_read of the active tab; the content of a success.
    fn read(&mut self, arguments: Value) -> String {
        let read = self.tool("browser_read", arguments);
        assert!(read["error"].is_null(), "browser_read failed: {read}");
        read["result"]["structuredContent"]["content"]
            .as_str()
            .unwrap_or_else(|| panic!("no content: {read}"))
            .to_owned()
    }

    /// Keep a screenshot of the active window as evidence, when asked to.
    fn keep(&mut self, name: &str) {
        let Some(directory) = std::env::var_os("KWIN_MCP_E2E_EVIDENCE").map(PathBuf::from) else {
            return;
        };
        thread::sleep(Duration::from_millis(500));
        let shot = self.tool("screenshot", json!({}));
        let path = shot["result"]["structuredContent"]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("screenshot: {shot}"));
        std::fs::create_dir_all(&directory).expect("create the evidence directory");
        std::fs::copy(path, directory.join(format!("{name}.png"))).expect("copy evidence");
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        if !self.session.is_empty() {
            let id = self.send_tool("session_stop", json!({}));
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                match self.replies.recv_timeout(Duration::from_millis(200)) {
                    Ok(reply) if reply["id"].as_u64() == Some(id) => break,
                    Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The structured content of a navigation that must have succeeded at `url`.
fn landed<'a>(reply: &'a Value, url: &str, method: &str) -> &'a Value {
    assert!(reply["error"].is_null(), "navigation failed: {reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let content = &reply["result"]["structuredContent"];
    assert_eq!(content["url"], url, "landed elsewhere: {reply}");
    assert_eq!(content["method"], method, "{reply}");
    content
}

/// The data of a navigation that must have failed for `reason`.
fn failed<'a>(reply: &'a Value, reason: &str, submitted: bool) -> &'a Value {
    let data = &reply["error"]["data"];
    assert_eq!(data["reason"], reason, "{reply}");
    assert_eq!(data["navigation_submitted"], submitted, "{reply}");
    data
}

fn long_data_url(characters: usize) -> String {
    let head = format!("data:text/html,<title>long-{characters}</title><p>");
    format!("{head}{}", "x".repeat(characters - head.len()))
}

/// Navigate to a page that answers after `SLOW_PAGE` and activate `other` while
/// Chrome waits for it, as a second agent on the same desktop does (#866).
fn navigate_while_another_window_takes_focus(
    wire: &mut Wire,
    fixture: &Fixture,
    other: &str,
) -> Value {
    let pending = wire.send_tool(
        "browser_navigate",
        json!({"url":fixture.url("/slow"), "speed":3}),
    );
    fixture.wait_for("/slow");
    let activated = wire.tool("window_activate", json!({"window_id":other}));
    assert!(activated["error"].is_null(), "{activated}");
    let reply = wire.reply(pending);
    eprintln!("browser_navigate (focus stolen): {reply}");
    reply
}

/// A session with Chrome on the fixture's `/alpha`: with a profile of its own,
/// which lets branded Chrome expose CDP, or with the default profile, which does
/// not. Returns the id of the browser window.
fn browser(name: &str, fixture: &Fixture, cdp: bool) -> (Wire, String) {
    let mut wire = Wire::start(name);
    let profile = if cdp {
        format!("--user-data-dir={} ", wire.workdir.join("profile").display())
    } else {
        String::new()
    };
    let launched = wire.launch(format!(
        "google-chrome-stable {profile}{}",
        fixture.url("/alpha")
    ));
    assert_eq!(
        launched["result"]["structuredContent"]["cdp"] == true,
        cdp,
        "{launched}"
    );
    let window = wire.window_titled("fixture/alpha");
    (wire, window)
}

const TYPED: &str = "address_bar_typed";

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_page_that_keeps_ctrl_l_does_not_keep_the_tab() {
    // #853: the address went into the page's field, the tab kept its page, and
    // the call still looked like a success.
    let fixture = Fixture::start();
    let (mut wire, _) = browser("853", &fixture, false);
    let trap = wire.navigate(&fixture.url("/trap"));
    assert_eq!(fixture.requests_for("/trap"), 1, "{trap}");
    let left = wire.navigate(&fixture.url("/beta"));
    assert_eq!(
        fixture.requests_for("/beta"),
        1,
        "the tab stayed on the page that consumes ctrl+l: {left}"
    );
    let content = landed(&left, &fixture.url("/beta"), TYPED);
    assert!(
        content["title"].as_str().unwrap_or_default().contains("fixture/beta"),
        "{left}"
    );
    wire.keep("853-left-a-page-that-keeps-ctrl-l");

    // An address that leaves the tab on its page is a failure that says so.
    let download = wire.navigate(&fixture.url("/download"));
    assert_eq!(fixture.requests_for("/download"), 1, "{download}");
    let data = failed(&download, "not_navigated", true);
    assert_eq!(data["url"], fixture.url("/beta"), "{download}");
    assert_eq!(data["retry_safe"], false, "{download}");

    // A redirect is where the address leads: reported, not failed.
    let redirect = wire.navigate(&fixture.url("/redirect"));
    let content = landed(&redirect, &fixture.url("/elsewhere"), TYPED);
    assert_eq!(content["redirected"], true, "{redirect}");
    assert_eq!(content["requested_url"], fixture.url("/redirect"), "{redirect}");

    // Another part of the page the tab shows opens no new document, and the
    // blank page has no address in accessibility: both are still reached.
    let part = wire.navigate(&fixture.url("/elsewhere#part"));
    landed(&part, &fixture.url("/elsewhere#part"), TYPED);
    assert_eq!(fixture.requests_for("/elsewhere"), 1, "{part}");
    landed(&wire.navigate("about:blank"), "about:blank", TYPED);

    // An address Chrome cannot load is its error page, twice in a row too.
    for _ in 0..2 {
        let refused = wire.navigate("http://127.0.0.1:1/");
        failed(&refused, "load_failed", true);
    }
    let invalid = wire.navigate("two words");
    failed(&invalid, "invalid_url", false);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_router_address_is_opened_and_never_searched_for() {
    // #865: the page holds ctrl+l until typing has begun, so the start of the
    // address went to the page and Chrome searched for the rest.
    let fixture = Fixture::start();
    let (mut wire, _) = browser("865", &fixture, false);
    let stall = wire.navigate(&fixture.url("/stall"));
    assert_eq!(fixture.requests_for("/stall"), 1, "{stall}");
    let router = wire.navigate(&fixture.url("/v1/blocks"));
    assert_eq!(
        fixture.requests_for("/v1/blocks"),
        1,
        "Chrome was not sent to the address: {router}"
    );
    let content = landed(&router, &fixture.url("/v1/blocks"), TYPED);
    assert!(
        content["title"].as_str().unwrap_or_default().contains("fixture/v1/blocks"),
        "{router}"
    );
    wire.keep("865-router-address-after-a-busy-page");

    // A bare host and port is an address too.
    let bare = wire.navigate(&format!("127.0.0.1:{}/bare", fixture.port));
    assert_eq!(fixture.requests_for("/bare"), 1, "{bare}");
    landed(&bare, &fixture.url("/bare"), TYPED);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn another_window_taking_focus_is_not_reported_as_the_page() {
    // #866: the title of the window that took focus was returned as the page.
    let fixture = Fixture::start();
    let (mut wire, first) = browser("866", &fixture, false);
    wire.launch(format!(
        "google-chrome-stable --new-window {}",
        fixture.url("/other")
    ));
    let other = wire.window_titled("fixture/other");
    let activated = wire.tool("window_activate", json!({"window_id":first}));
    assert!(activated["error"].is_null(), "{activated}");
    let stolen = navigate_while_another_window_takes_focus(&mut wire, &fixture, &other);
    let title = stolen["result"]["structuredContent"]["title"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !title.contains("fixture/other"),
        "the other window's title was reported as the page: {stolen}"
    );
    let content = landed(&stolen, &fixture.url("/slow"), TYPED);
    assert!(title.contains("fixture/slow"), "{stolen}");
    assert_eq!(content["window_active"], false, "{stolen}");
    wire.tool("window_activate", json!({"window_id":first}));
    wire.keep("866-page-opened-while-another-window-took-focus");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn an_address_longer_than_2000_characters_is_opened() {
    // #873: pasted into the address bar, and the agent's clipboard is kept.
    let fixture = Fixture::start();
    let (mut wire, _) = browser("873", &fixture, false);
    let kept = wire.tool(
        "clipboard_write",
        json!({"target":"session", "text":"kept-across-navigation"}),
    );
    assert!(kept["error"].is_null(), "{kept}");
    let pasted = wire.navigate(&long_data_url(3000));
    assert!(pasted["error"].is_null(), "the address was refused: {pasted}");
    let content = &pasted["result"]["structuredContent"];
    assert_eq!(content["method"], "address_bar_pasted", "{pasted}");
    assert!(
        content["title"].as_str().unwrap_or_default().contains("long-3000"),
        "{pasted}"
    );
    assert!(
        content["url"].as_str().unwrap_or_default().ends_with("[3000 characters]"),
        "the whole address did not arrive: {pasted}"
    );
    assert!(content.get("redirected").is_none(), "{pasted}");
    let clipboard = wire.tool("clipboard_read", json!({}));
    assert_eq!(
        clipboard["result"]["structuredContent"]["text"], "kept-across-navigation",
        "{clipboard}"
    );
    wire.keep("873-address-of-3000-characters-without-cdp");
    // The address bar keeps 512000 pasted characters; past that only CDP works.
    let too_long = wire.navigate(&long_data_url(512_001));
    failed(&too_long, "url_too_long_for_address_bar", false);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn cdp_reaches_the_browser_again_after_the_server_reexecutes_itself() {
    // #957: a re-execution, which every install causes, left the session
    // without its CDP connection: navigation fell back to typing and
    // browser_read refused to read.
    let fixture = Fixture::start();
    let mut wire = Wire::start_with("957", true);
    let profile = wire.workdir.join("profile");
    let launched = wire.launch(format!(
        "google-chrome-stable --user-data-dir={} {}",
        profile.display(),
        fixture.url("/alpha")
    ));
    let content = &launched["result"]["structuredContent"];
    assert_eq!(content["cdp"], true, "{launched}");
    wire.window_titled("fixture/alpha");
    let links_page = fixture.url("/links");
    landed(&wire.navigate(&links_page), &links_page, "cdp");

    // Each link with its whole target, then without queries, then one per target.
    let links = json!({"format":"links"});
    let whole = wire.read(links.clone());
    assert_eq!(whole.lines().count(), 3, "{whole}");
    assert!(whole.contains("/item?id=7&track=bbbb"), "{whole}");
    let bare = wire.read(json!({"format":"links", "strip_query":true}));
    assert!(!bare.contains('?') && !bare.contains('#'), "{bare}");
    let one = wire.read(json!({"format":"links", "strip_query":true, "dedupe":true}));
    assert_eq!(
        one.lines().collect::<Vec<_>>(),
        [
            format!("Item seven\t{}", fixture.url("/item")),
            format!("Other\t{}", fixture.url("/other")),
        ],
        "{one}"
    );
    let refused = wire.tool("browser_read", json!({"format":"text", "dedupe":true}));
    let reason = &refused["error"]["data"]["reason"];
    assert_eq!(reason, "invalid_option", "{refused}");

    wire.reexecute();
    let beta = fixture.url("/beta");
    landed(&wire.navigate(&beta), &beta, "cdp");
    let text = wire.read(json!({}));
    assert!(text.contains("fixture/beta"), "{text}");
    wire.keep("957-cdp-after-reexecution");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn method_address_bar_types_the_address_as_a_person_does_with_cdp_too() {
    // #958: a bot-protected site answered navigations through CDP with an
    // error page and typed ones with the page.
    let fixture = Fixture::start();
    let (mut wire, _) = browser("958", &fixture, true);
    let direct = fixture.url("/direct");
    landed(&wire.navigate(&direct), &direct, "cdp");
    let by_hand = fixture.url("/by-hand");
    let typed = wire.tool(
        "browser_navigate",
        json!({"url":by_hand, "method":"address_bar", "speed":3}),
    );
    landed(&typed, &by_hand, TYPED);
    assert_eq!(fixture.requests_for("/by-hand"), 1, "{typed}");
    wire.keep("958-typed-with-cdp");

    // Return in the address bar is where the key belongs: no focus warning.
    let shortcut = wire.tool("keyboard_key", json!({"key":"ctrl+l"}));
    assert!(shortcut["error"].is_null(), "{shortcut}");
    let address = fixture.url("/returned");
    let entered = wire.tool("keyboard_type", json!({"text":address, "speed":3}));
    assert!(entered["error"].is_null(), "{entered}");
    let returned = wire.tool("keyboard_key", json!({"key":"Return"}));
    let content = &returned["result"]["structuredContent"];
    assert!(content.get("warning").is_none(), "{returned}");
    assert_eq!(content["keyboard_focus"], "address_bar", "{returned}");
    fixture.wait_for("/returned");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn with_cdp_the_tab_is_navigated_directly_and_read_back() {
    let fixture = Fixture::start();
    let (mut wire, first) = browser("cdp", &fixture, true);

    // #853, #865: no key is sent, so a page that keeps ctrl+l changes nothing,
    // and a bare host is opened as an address.
    landed(&wire.navigate(&fixture.url("/trap")), &fixture.url("/trap"), "cdp");
    let bare = wire.navigate(&format!("127.0.0.1:{}/v1/blocks", fixture.port));
    assert_eq!(fixture.requests_for("/v1/blocks"), 1, "{bare}");
    let content = landed(&bare, &fixture.url("/v1/blocks"), "cdp");
    assert!(
        content["title"].as_str().unwrap_or_default().contains("fixture/v1/blocks"),
        "{bare}"
    );
    wire.keep("865-bare-router-address-with-cdp");

    let redirect = wire.navigate(&fixture.url("/redirect"));
    let content = landed(&redirect, &fixture.url("/elsewhere"), "cdp");
    assert_eq!(content["redirected"], true, "{redirect}");
    landed(&wire.navigate("about:blank"), "about:blank", "cdp");
    landed(&wire.navigate(&fixture.url("/elsewhere")), &fixture.url("/elsewhere"), "cdp");
    let part = wire.navigate(&fixture.url("/elsewhere#part"));
    landed(&part, &fixture.url("/elsewhere#part"), "cdp");
    assert_eq!(fixture.requests_for("/elsewhere"), 2, "{part}");

    // #853: the tab keeps its page, and the call says why.
    let download = wire.navigate(&fixture.url("/download"));
    let data = failed(&download, "download", true);
    assert_eq!(data["url"], fixture.url("/elsewhere#part"), "{download}");
    let refused = wire.navigate("http://127.0.0.1:1/");
    let data = failed(&refused, "load_failed", true);
    assert_eq!(data["error"], "net::ERR_UNSAFE_PORT", "{refused}");
    let script = wire.navigate("javascript:void(0)");
    failed(&script, "not_navigated", true);

    // #873: far past 2000 characters, with the clipboard untouched.
    let kept = wire.tool(
        "clipboard_write",
        json!({"target":"session", "text":"kept-across-navigation"}),
    );
    assert!(kept["error"].is_null(), "{kept}");
    let long = wire.navigate(&long_data_url(200_000));
    assert!(long["error"].is_null(), "{long}");
    let content = &long["result"]["structuredContent"];
    assert_eq!(content["method"], "cdp", "{long}");
    assert!(
        content["title"].as_str().unwrap_or_default().contains("long-200000"),
        "{long}"
    );
    assert!(
        content["url"].as_str().unwrap_or_default().ends_with("[200000 characters]"),
        "{long}"
    );
    let clipboard = wire.tool("clipboard_read", json!({}));
    assert_eq!(
        clipboard["result"]["structuredContent"]["text"], "kept-across-navigation",
        "{clipboard}"
    );
    wire.keep("873-address-of-200000-characters-with-cdp");

    // A new tab has its address bar focused; after the navigation the page has
    // the keyboard, as it has after Return in the address bar.
    let new_tab = wire.tool("keyboard_key", json!({"key":"ctrl+t"}));
    assert!(new_tab["error"].is_null(), "{new_tab}");
    wire.window_titled("New Tab");
    let opened = wire.navigate(&fixture.url("/typed"));
    landed(&opened, &fixture.url("/typed"), "cdp");
    let typed = wire.tool("keyboard_type", json!({"text":"in the page"}));
    assert_eq!(
        typed["result"]["structuredContent"]["delivery"]["status"], "verified",
        "the page's field did not get the keyboard: {typed}"
    );

    // #866: another window takes focus while the page is on its way.
    wire.launch(format!(
        "google-chrome-stable --user-data-dir={} --new-window {}",
        wire.workdir.join("profile").display(),
        fixture.url("/other")
    ));
    let other = wire.window_titled("fixture/other");
    let activated = wire.tool("window_activate", json!({"window_id":first}));
    assert!(activated["error"].is_null(), "{activated}");
    let stolen = navigate_while_another_window_takes_focus(&mut wire, &fixture, &other);
    let content = landed(&stolen, &fixture.url("/slow"), "cdp");
    assert!(
        content["title"].as_str().unwrap_or_default().contains("fixture/slow"),
        "the title is not the opened page's: {stolen}"
    );
    assert_eq!(content["window_active"], false, "{stolen}");
    wire.tool("window_activate", json!({"window_id":first}));
    wire.keep("866-page-opened-with-cdp-while-another-window-took-focus");

    let invalid = wire.navigate("two words");
    failed(&invalid, "invalid_url", false);
}
