#![allow(clippy::expect_used)]

//! Real Chrome navigation through the production stdio entrypoint. The HTTP
//! fixture and native omnibox copies independently distinguish the requested URL
//! from an old page, a history completion and a redirect with the same title.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let worker_stop = stop.clone();
        let worker_requests = requests.clone();
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
        let (status, extra, body) = match path.as_str() {
            "/download" => (
                "200 OK",
                "Content-Disposition: attachment; filename=issue853.txt\r\nContent-Type: application/octet-stream\r\n",
                "download fixture",
            ),
            "/redirect" => ("302 Found", "Location: /elsewhere\r\n", ""),
            "/query?value=New" => ("302 Found", "Location: /query?value=old\r\n", ""),
            _ => {
                if path == "/delayed" {
                    thread::sleep(Duration::from_secs(1));
                }
                (
                    "200 OK",
                    "Content-Type: text/html\r\n",
                    "<!doctype html><title>navigation-fixture</title><h1>navigation-fixture</h1><input autofocus aria-label=page-field value=untouched>",
                )
            }
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }

    fn saw(&self, path: &str) -> bool {
        self.requests
            .lock()
            .expect("request log")
            .iter()
            .any(|request| request == path)
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
    next_id: u64,
    session: Option<String>,
    home: PathBuf,
}

impl Wire {
    fn start() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let home = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("TMPDIR").filter(|value| !value.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"))
            .join(format!("kmnav-{}-{nonce:x}", std::process::id()));
        for directory in [".cache", ".config", ".local/share", ".local/state"] {
            std::fs::create_dir_all(home.join(directory)).expect("private HOME");
        }
        let binary = std::env::var("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned());
        let mut child = Command::new(binary)
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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
            next_id: 0,
            session: None,
            home,
        };
        wire.request(
            "initialize",
            json!({"protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"navigate-e2e","version":"2"}}),
        )
        .expect("initialize");
        writeln!(
            wire.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("initialized");
        wire.stdin.flush().expect("flush initialized");
        let started = wire.tool(
            "session_start",
            json!({"browser_profiles":"fresh", "width":1280, "height":720}),
        );
        wire.session = Some(
            started["result"]["structuredContent"]["session_id"]
                .as_str()
                .unwrap_or_else(|| panic!("session_start: {started}"))
                .to_owned(),
        );
        wire
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let reply = self
                .replies
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|e| e.to_string())?;
            if reply["id"] == id {
                return Ok(reply);
            }
        }
    }

    fn tool(&mut self, name: &str, mut arguments: Value) -> Value {
        if let Some(session) = &self.session {
            arguments["session_id"] = json!(session);
        }
        let reply = self
            .request("tools/call", json!({"name":name, "arguments":arguments}))
            .expect("tool reply");
        eprintln!("{name}: {reply}");
        reply
    }

    fn omnibox(&mut self) -> String {
        for key in ["ctrl+l", "ctrl+c"] {
            let reply = self.tool("keyboard_key", json!({"key":key}));
            assert!(reply["error"].is_null(), "{reply}");
        }
        let copied = self.tool("clipboard_read", json!({}));
        self.tool("keyboard_key", json!({"key":"Escape"}));
        copied["result"]["structuredContent"]["text"]
            .as_str()
            .expect("copied address")
            .to_owned()
    }

    fn navigate(&mut self, url: &str) -> Value {
        self.tool("browser_navigate", json!({"url":url, "speed":4}))
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.request(
                "tools/call",
                json!({"name":"session_stop", "arguments":{"session_id":session}}),
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn navigate_opens_the_typed_address_and_reports_where_it_landed() {
    let fixture = Fixture::start();
    let mut wire = Wire::start();
    let initial = fixture.url("localhost", "/alpha");
    let profile = wire.home.join("profile");
    let launched = wire.tool(
        "launch_app",
        json!({
        "command":format!("google-chrome-stable --host-resolver-rules='MAP localhost 127.0.0.1' --user-data-dir={} {initial}", profile.display()),
        "window_timeout_seconds":40}),
    );
    assert_eq!(
        launched["result"]["structuredContent"]["cdp"], true,
        "{launched}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let windows = wire.tool("window_list", json!({}));
        if windows.to_string().contains("navigation-fixture") {
            break;
        }
        assert!(Instant::now() < deadline, "fixture did not load: {windows}");
        thread::sleep(Duration::from_millis(250));
    }
    // Different origins, populated history, case-sensitive queries, and a delayed
    // document all retain the same title. No Ctrl+T workaround is used.
    for path in [
        "/beta",
        "/prefix-long",
        "/prefix",
        "/prefix?value=One",
        "/prefix?value=Two",
        "/delayed",
    ] {
        let target = fixture.url("127.0.0.1", path);
        let reply = wire.navigate(&target);
        let copied = wire.omnibox();
        assert_eq!(copied, target, "native address: {reply}");
        assert!(fixture.saw(path), "fixture never received {path}: {reply}");
        assert!(reply["error"].is_null(), "{reply}");
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        let content = &reply["result"]["structuredContent"];
        assert_eq!(content["url"], target, "{reply}");
        assert_ne!(content["matched"], false, "{reply}");
    }
    // A download never commits a new document, and redirects land elsewhere.
    // Both used to return a normal-looking success, including matched:false.
    for (path, landed) in [
        ("/download", "/delayed"),
        ("/redirect", "/elsewhere"),
        ("/query?value=New", "/query?value=old"),
    ] {
        let reply = wire.navigate(&fixture.url("127.0.0.1", path));
        let actual = fixture.url("127.0.0.1", landed);
        let copied = wire.omnibox();
        assert!(fixture.saw(path), "fixture never received {path}: {reply}");
        assert_eq!(copied, actual, "native address: {reply}");
        assert!(reply["error"].is_null(), "{reply}");
        assert_eq!(
            reply["result"]["isError"], true,
            "false navigation success: {reply}"
        );
        let content = &reply["result"]["structuredContent"];
        assert_eq!(content["url"], actual, "{reply}");
        assert_eq!(content["matched"], false, "{reply}");
        assert_eq!(content["status"], "not_reached", "{reply}");
        assert_eq!(content["retry_safe"], false, "{reply}");
    }
    let invalid = wire.navigate("two words");
    assert_eq!(
        invalid["error"]["data"]["reason"], "invalid_url",
        "{invalid}"
    );
    assert_eq!(wire.omnibox(), fixture.url("127.0.0.1", "/query?value=old"));
    // Keep the issue's New Tab workaround working as well as populated tabs.
    wire.tool("keyboard_key", json!({"key":"ctrl+t"}));
    let target = fixture.url("127.0.0.1", "/after-new-tab");
    let reply = wire.navigate(&target);
    assert_eq!(wire.omnibox(), target, "{reply}");
    assert!(fixture.saw("/after-new-tab"), "{reply}");
    assert!(reply["error"].is_null(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    assert_eq!(
        reply["result"]["structuredContent"]["url"], target,
        "{reply}"
    );
}
