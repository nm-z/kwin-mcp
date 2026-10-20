#![allow(clippy::expect_used)]

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSION_LIFECYCLE_LOG_MAX_BYTES: usize = 1024 * 1024;

/// Keep fixture storage configurable independently of the checkout location.
fn fixture_temp_root() -> PathBuf {
    let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("TMPDIR").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    assert!(
        root.is_absolute(),
        "fixture root must be absolute: {}",
        root.display()
    );
    root
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to clean test directory {}: {error}",
                self.0.display()
            );
        }
    }
}

struct RpcClient {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    stderr: Receiver<String>,
    pending: HashMap<u64, Value>,
    sessions: Vec<String>,
    /// Private HOME for this run, removed on drop.
    home: PathBuf,
    notifications: Vec<Value>,
    binary_copy: Option<PathBuf>,
}

impl RpcClient {
    fn start() -> Self {
        Self::start_with_options(None)
    }

    fn start_with_options(delay_stage: Option<&str>) -> Self {
        Self::start_program(
            env!("CARGO_BIN_EXE_kwin-mcp"),
            delay_stage,
            false,
            false,
            &[],
        )
    }

    #[cfg(debug_assertions)]
    fn start_with_options_and_stop(delay_stage: Option<&str>, stop_bwrap: bool) -> Self {
        Self::start_program(
            env!("CARGO_BIN_EXE_kwin-mcp"),
            delay_stage,
            stop_bwrap,
            false,
            &[],
        )
    }

    #[cfg(debug_assertions)]
    fn start_with_first_proxy_failure() -> Self {
        Self::start_program(env!("CARGO_BIN_EXE_kwin-mcp"), None, false, true, &[])
    }

    fn start_with_env(extra_env: &[(&str, &str)]) -> Self {
        Self::start_program(
            env!("CARGO_BIN_EXE_kwin-mcp"),
            None,
            false,
            false,
            extra_env,
        )
    }

    fn start_with_cookie_test_bus() -> Self {
        let address = require_filtered_cookie_test_bus();
        Self::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", address.as_str())])
    }

    fn start_program(
        program: &str,
        delay_stage: Option<&str>,
        stop_bwrap: bool,
        fail_after_first_proxy: bool,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        // Short generated names leave room for nested Unix socket paths.
        let home = fixture_temp_root().join(format!("kmh-{}-{nonce:x}", std::process::id()));
        for directory in [
            home.join(".config"),
            home.join(".local/share"),
            home.join(".cache"),
            home.join(".local/state"),
            home.join(".kde"),
        ] {
            std::fs::create_dir_all(directory).expect("create private test HOME");
        }
        let program = if program == env!("CARGO_BIN_EXE_kwin-mcp") {
            std::env::var("KWIN_MCP_E2E_SERVER").unwrap_or_else(|_| program.to_owned())
        } else {
            program.to_owned()
        };
        let source_reload = extra_env
            .iter()
            .any(|(name, value)| *name == "KWIN_MCP_E2E_SOURCE_RELOAD" && *value == "1");
        let source_root = home.join("source");
        if source_reload {
            let original = std::env::var_os("KWIN_MCP_E2E_SOURCE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
            for name in [
                "src",
                "data",
                "Cargo.toml",
                "Cargo.lock",
                "build.rs",
                "cursor_v6_fixed.svg",
            ] {
                copy_source(&original.join(name), &source_root.join(name));
            }
        }
        let binary_copy = if extra_env
            .iter()
            .any(|(name, value)| *name == "KWIN_MCP_E2E_COPY_BINARY" && *value == "1")
        {
            let path = if source_reload {
                source_root.join("target/release/kwin-mcp")
            } else {
                home.join("kwin-mcp")
            };
            std::fs::create_dir_all(path.parent().expect("executable directory"))
                .expect("create executable directory");
            std::fs::copy(&program, &path).expect("copy private server executable");
            Some(path)
        } else {
            None
        };
        let mut command = Command::new(
            binary_copy
                .as_ref()
                .map_or_else(|| PathBuf::from(&program), Clone::clone),
        );
        command.env(
            "KWIN_MCP_REPO",
            if source_reload {
                source_root
            } else {
                home.join("no-repository")
            },
        );
        if source_reload {
            let original_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .expect("original HOME");
            command.env(
                "CARGO_HOME",
                std::env::var_os("CARGO_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| original_home.join(".cargo")),
            );
            command.env(
                "RUSTUP_HOME",
                std::env::var_os("RUSTUP_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| original_home.join(".rustup")),
            );
            command.env("CARGO_NET_OFFLINE", "true");
        }
        if source_reload && let Some(cargo) = std::env::var_os("KWIN_MCP_E2E_CARGO") {
            command.env("CARGO", cargo);
        }
        if let Some(path) = &binary_copy {
            command.env("KWIN_MCP_BINARY", path);
        }
        let runtime_dir = host_runtime_dir();
        let session_bus = host_session_bus_address(&runtime_dir);
        command
            .args(["--autoclean"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("KDEHOME", home.join(".kde"))
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("DBUS_SESSION_BUS_ADDRESS", session_bus)
            .env(
                "WAYLAND_DISPLAY",
                std::env::var_os("WAYLAND_DISPLAY")
                    .unwrap_or_else(|| std::ffi::OsString::from("wayland-0")),
            );
        if let Some((_, minutes)) = extra_env
            .iter()
            .find(|(name, _)| *name == "KWIN_MCP_E2E_TTL_MINUTES")
        {
            command.args(["--ttl", minutes]);
        }
        if let Some(stage) = delay_stage {
            command
                .env("KWIN_MCP_TEST_STARTUP_DELAY_STAGE", stage)
                .env("KWIN_MCP_TEST_STARTUP_DELAY_MS", "30000");
        }
        if stop_bwrap {
            command.env("KWIN_MCP_TEST_STOP_BWRAP", "1");
        }
        if fail_after_first_proxy {
            command.env("KWIN_MCP_TEST_FAIL_AFTER_FIRST_PROXY", "1");
        }
        command.envs(extra_env.iter().copied());
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("kwin-mcp stdin");
        let stdout = child.stdout.take().expect("kwin-mcp stdout");
        let stderr = child.stderr.take().expect("kwin-mcp stderr");
        let (response_tx, responses) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        let stderr_path = std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR")
            .map(PathBuf::from)
            .map(|root| {
                std::fs::create_dir_all(&root).expect("diagnostic directory");
                root.join(format!("server-{}.stderr.log", child.id()))
            });
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(message) = serde_json::from_str::<Value>(&line) {
                    let _ = response_tx.send(message);
                }
            }
        });
        thread::spawn(move || {
            let mut diagnostic = stderr_path.and_then(|path| std::fs::File::create(path).ok());
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(file) = &mut diagnostic {
                    let _ = writeln!(file, "{line}");
                }
                let _ = stderr_tx.send(line);
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            responses,
            stderr: stderr_rx,
            pending: HashMap::new(),
            sessions: Vec::new(),
            home,
            notifications: Vec::new(),
            binary_copy,
        }
    }

    fn send(&mut self, id: u64, method: &str, params: Value) {
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        writeln!(self.stdin.as_mut().expect("open client stdin"), "{request}")
            .expect("write JSON-RPC request");
        self.stdin
            .as_mut()
            .expect("open client stdin")
            .flush()
            .expect("flush JSON-RPC request");
    }

    fn notify(&mut self, method: &str, params: Value) {
        let request = json!({"jsonrpc":"2.0", "method":method, "params":params});
        writeln!(self.stdin.as_mut().expect("open client stdin"), "{request}")
            .expect("write JSON-RPC notification");
        self.stdin
            .as_mut()
            .expect("open client stdin")
            .flush()
            .expect("flush JSON-RPC notification");
    }

    fn response(&mut self, id: u64, timeout: Duration) -> Value {
        if let Some(message) = self.pending.remove(&id) {
            return message;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self
                .responses
                .recv_timeout(remaining)
                .expect("JSON-RPC response");
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
            if let Some(other_id) = message.get("id").and_then(Value::as_u64) {
                self.pending.insert(other_id, message);
            } else {
                self.notifications.push(message);
            }
        }
    }

    fn wait_for_notification(&mut self, method: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .notifications
                .iter()
                .any(|message| message["method"] == method)
            {
                return;
            }
            let message = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("notification");
            if let Some(id) = message["id"].as_u64() {
                self.pending.insert(id, message);
            } else {
                self.notifications.push(message);
            }
        }
    }

    fn wait_for_stderr(&self, text: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .stderr
                .recv_timeout(remaining)
                .expect("kwin-mcp startup log");
            if line.contains(text) {
                return line;
            }
        }
    }

    fn stop_process(mut self) {
        self.cleanup_sessions();
        drop(self.stdin.take());
    }

    fn cleanup_sessions(&mut self) {
        let sessions = std::mem::take(&mut self.sessions);
        let arguments = if sessions.is_empty() {
            vec![json!({})]
        } else {
            sessions
                .into_iter()
                .map(|session| json!({"session_id":session}))
                .collect()
        };
        for (index, arguments) in arguments.into_iter().enumerate() {
            let Some(stdin) = &mut self.stdin else { break };
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            let id = 9999 + index;
            let request = json!({"jsonrpc":"2.0", "id":id, "method":"tools/call",
                "params":{"name":"session_stop","arguments":arguments}});
            let _ = writeln!(stdin, "{request}");
            let _ = stdin.flush();
            let deadline = Instant::now() + Duration::from_secs(45);
            while let Ok(message) = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                if message["id"] == id {
                    break;
                }
            }
        }
    }

    #[cfg(debug_assertions)]
    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.cleanup_sessions();
        // Closing the transport lets the production server stop its scopes.
        // Killing it immediately after writing session_stop races that cleanup.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        let clean_exit = loop {
            if self.child.try_wait().ok().flatten().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(50));
        };
        if !clean_exit {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if clean_exit {
            let _ = std::fs::remove_dir_all(&self.home);
        } else {
            eprintln!(
                "retaining {} after uncertain server {} cleanup",
                self.home.display(),
                self.child.id()
            );
        }
    }
}

fn initialize(client: &mut RpcClient) {
    client.send(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18",
            "capabilities":{},
            "clientInfo":{"name":"kwin-mcp-e2e","version":"1"}
        }),
    );
    let response = client.response(1, Duration::from_secs(10));
    assert!(
        response["result"].is_object(),
        "initialize failed: {response}"
    );
    client.notify("notifications/initialized", json!({}));
}

fn call_tool(client: &mut RpcClient, id: u64, name: &str, arguments: Value) -> Value {
    let selected = arguments["session_id"].as_str().map(str::to_owned);
    client.send(
        id,
        "tools/call",
        json!({"name":name, "arguments":arguments}),
    );
    let response = client.response(id, Duration::from_secs(45));
    if response["error"].is_null() && !response["result"]["isError"].as_bool().unwrap_or(false) {
        if name == "session_start" {
            if let Some(session) = response["result"]["structuredContent"]["session_id"].as_str() {
                client.sessions.push(session.to_owned());
            }
        } else if name == "session_stop" {
            if let Some(selected) = selected {
                client.sessions.retain(|session| session != &selected);
            } else if client.sessions.len() == 1 {
                client.sessions.clear();
            }
        }
    }
    response
}

#[cfg(debug_assertions)]
fn process_children(pid: u32) -> std::io::Result<Vec<String>> {
    let output = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn process_alive(pid: u32) -> std::io::Result<bool> {
    match std::fs::metadata(format!("/proc/{pid}")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn viewer_windows(pid: u32) -> Vec<String> {
    let output = Command::new("kdotool")
        // kdotool refuses to run unless the session names Plasma 6.
        .env("KDE_SESSION_VERSION", "6")
        .args([
            "search",
            "--all",
            "--pid",
            &pid.to_string(),
            "--title",
            "^kwin-viewer.*$",
        ])
        .output()
        .expect("query host KWin windows");
    assert!(
        output.status.success(),
        "kdotool: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wait_for_viewer_windows(pid: u32, expected: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows = viewer_windows(pid);
        if windows.len() == expected || Instant::now() >= deadline {
            return windows;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_recent_viewer_input(path: &std::path::Path) -> SystemTime {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let modified = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .expect("viewer input activity timestamp");
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if modified > UNIX_EPOCH && age <= Duration::from_secs(1) {
            return modified;
        }
        assert!(
            Instant::now() < deadline,
            "viewer input did not update {} recently",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn workdir(response: &Value) -> PathBuf {
    response["result"]["structuredContent"]["workdir"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("session_start did not return a workdir: {response}"))
}

fn host_runtime_dir() -> std::ffi::OsString {
    use std::os::unix::fs::MetadataExt;
    std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| {
        let uid = std::fs::metadata("/proc/self")
            .expect("inspect current process owner")
            .uid();
        std::ffi::OsString::from(format!("/run/user/{uid}"))
    })
}

fn host_session_bus_address(runtime_dir: &std::ffi::OsStr) -> std::ffi::OsString {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|| {
        let mut address = std::ffi::OsString::from("unix:path=");
        address.push(runtime_dir);
        address.push("/bus");
        address
    })
}

fn require_filtered_cookie_test_bus() -> String {
    let address = std::env::var("KWIN_MCP_TEST_DBUS_SESSION_BUS_ADDRESS")
        .expect("cookie session checks require a filtered test D-Bus proxy");
    let host_bus = host_session_bus_address(&host_runtime_dir());
    assert!(
        std::ffi::OsStr::new(&address) != host_bus.as_os_str(),
        "cookie session checks must not connect directly to the host user bus"
    );
    address
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, input devices, and a live GPU session"]
fn autoclean_sweeps_only_a_crashed_owned_session() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let initialize = |client: &mut RpcClient| {
        client.send(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"orphan-sweep-e2e","version":"1"}
            }),
        );
        let response = client.response(1, Duration::from_secs(10));
        assert!(
            response["result"].is_object(),
            "initialize failed: {response}"
        );
        client.notify("notifications/initialized", json!({}));
    };

    let mut owner = RpcClient::start();
    initialize(&mut owner);
    let started = call_tool(
        &mut owner,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let dir = workdir(&started);
    let launched = call_tool(&mut owner, 3, "launch_app", json!({"command":"konsole"}));
    assert!(
        !launched["result"]["isError"].as_bool().unwrap_or(false),
        "{launched}"
    );
    let marker =
        std::fs::read_to_string(dir.join(".kwin-mcp-autoclean")).expect("read autoclean lease");
    let group: i32 = marker
        .split_whitespace()
        .nth(2)
        .expect("sandbox process group in lease")
        .parse()
        .expect("sandbox group number");

    let mut other = RpcClient::start();
    initialize(&mut other);
    assert!(dir.exists(), "another server swept a live session");
    other.stop_process();

    owner.child.kill().expect("crash test-owned server");
    owner.child.wait().expect("wait for crashed server");
    assert!(dir.exists(), "crash left no orphan to test");
    let deadline = Instant::now() + Duration::from_secs(10);
    while unsafe { nix::libc::kill(-group, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "sandbox group survived its owner"
        );
        thread::sleep(Duration::from_millis(100));
    }
    drop(owner);

    let mut reaper = RpcClient::start();
    initialize(&mut reaper);
    let deadline = Instant::now() + Duration::from_secs(10);
    while dir.exists() {
        assert!(
            Instant::now() < deadline,
            "orphan workdir was not swept: {}",
            dir.display()
        );
        thread::sleep(Duration::from_millis(100));
    }
    reaper.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, input devices, and a live GPU session"]
fn session_reaches_host_loopback_listener_on_ephemeral_port() {
    check_host_loopback_listener(
        std::net::TcpListener::bind("127.0.0.1:0").expect("host ephemeral listener"),
    );
}

#[test]
#[ignore = "requires private KWin, pasta, Konsole, and input devices"]
fn session_still_reaches_host_listener_below_the_ephemeral_range() {
    let range =
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").expect("host port range");
    let start: u16 = range
        .split_whitespace()
        .next()
        .expect("range start")
        .parse()
        .expect("port");
    let listener = (1024..start)
        .rev()
        .find_map(|port| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).ok())
        .expect("available non-ephemeral host listener");
    check_host_loopback_listener(listener);
}

fn check_host_loopback_listener(listener: std::net::TcpListener) {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    let port = listener.local_addr().expect("host listener address").port();
    eprintln!("host loopback proof port={port}");
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "session_start failed: {started}"
    );
    let session_id = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session_start returned a session ID")
        .to_owned();

    listener
        .set_nonblocking(true)
        .expect("make listener nonblocking");
    let host_request = thread::spawn(move || -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .map_err(|error| error.to_string())?;
                    let mut request = String::new();
                    BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
                        .read_line(&mut request)
                        .map_err(|error| error.to_string())?;
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
                        )
                        .map_err(|error| error.to_string())?;
                    return Ok(request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err("host loopback listener received no request".to_owned());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    });

    let command = format!(
        "konsole -e bash -lc 'curl --noproxy \"*\" --fail --silent --show-error --connect-timeout 2 --max-time 4 http://127.0.0.1:{port}/probe; sleep 3'"
    );
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"session_id":session_id,"command":command}),
    );
    assert!(
        !launched["result"]["isError"].as_bool().unwrap_or(false),
        "launch_app failed: {launched}"
    );
    let request = host_request
        .join()
        .expect("host listener thread")
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(request, "GET /probe HTTP/1.1\r\n");

    let stopped = call_tool(
        &mut client,
        4,
        "session_stop",
        json!({"session_id":session_id}),
    );
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "session_stop failed: {stopped}"
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Ruby, input devices, and a live GPU session"]
fn concurrent_sessions_bind_same_private_loopback_port() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    let host_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve host port");
    let port = host_listener
        .local_addr()
        .expect("host listener address")
        .port();
    // Keep this as a free port so pasta does not forward a host listener that
    // would occupy the same loopback port inside each session.
    drop(host_listener);
    let host_name = std::fs::read_to_string("/proc/sys/kernel/hostname").expect("read host name");
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let mut client = RpcClient::start();
        client.send(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"private-loopback-e2e","version":"1"}
            }),
        );
        let initialized = client.response(1, Duration::from_secs(10));
        assert!(
            initialized["result"].is_object(),
            "initialize failed: {initialized}"
        );
        client.notify("notifications/initialized", json!({}));
        let started = call_tool(
            &mut client,
            2,
            "session_start",
            json!({"width":800,"height":600}),
        );
        assert_eq!(
            started["result"]["structuredContent"]["status"], "started",
            "{started}"
        );
        let dir = workdir(&started);
        let command = format!(
            "konsole -e ruby -rsocket -e 'server = TCPServer.new(\"127.0.0.1\", {port}); File.write(File.join(ENV.fetch(\"XDG_RUNTIME_DIR\"), \"loopback-ready\"), Socket.gethostname); loop {{ server.accept.close }}'"
        );
        let launched = call_tool(&mut client, 3, "launch_app", json!({"command":command}));
        assert!(
            !launched["result"]["isError"].as_bool().unwrap_or(false),
            "{launched}"
        );
        sessions.push((client, dir));
    }
    for (_, dir) in &sessions {
        let marker = dir.join("loopback-ready");
        let deadline = Instant::now() + Duration::from_secs(10);
        let seen_host_name = loop {
            if let Ok(value) = std::fs::read_to_string(&marker) {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "listener did not bind in {}",
                dir.display()
            );
            thread::sleep(Duration::from_millis(200));
        };
        assert_eq!(
            seen_host_name.trim(),
            host_name.trim(),
            "sandbox hostname changed"
        );
    }
    for (mut client, dir) in sessions {
        let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
        assert!(
            !stopped["result"]["isError"].as_bool().unwrap_or(false),
            "{stopped}"
        );
        assert!(!dir.exists(), "session_stop left {}", dir.display());
        client.stop_process();
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn concurrent_stop_waits_for_start_and_restart_cleans_workdir() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    let mut client = RpcClient::start();
    client.send(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18",
            "capabilities":{},
            "clientInfo":{"name":"session-lifecycle-e2e","version":"1"}
        }),
    );
    let initialize = client.response(1, Duration::from_secs(10));
    assert!(
        initialize["result"].is_object(),
        "initialize failed: {initialize}"
    );
    client.notify("notifications/initialized", json!({}));

    client.send(
        2,
        "tools/call",
        json!({
            "name":"session_start",
            "arguments":{"width":800,"height":600}
        }),
    );
    client.wait_for_stderr("host_xdg_dir ready", Duration::from_secs(20));
    client.send(
        3,
        "tools/call",
        json!({"name":"session_stop","arguments":{}}),
    );

    let stop = client.response(3, Duration::from_secs(45));
    let start = client.response(2, Duration::from_secs(45));
    assert!(
        !stop["result"]["isError"].as_bool().unwrap_or(false),
        "concurrent stop failed: {stop}"
    );
    assert_ne!(
        stop["result"]["structuredContent"]["status"], "none",
        "stop raced ahead of startup: {stop}"
    );
    let first_workdir = workdir(&start);
    assert!(
        !first_workdir.exists(),
        "concurrent stop left {}",
        first_workdir.display()
    );

    let second_start = call_tool(
        &mut client,
        4,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let second_workdir = workdir(&second_start);
    let second_stop = call_tool(&mut client, 5, "session_stop", json!({}));
    assert!(
        !second_stop["result"]["isError"].as_bool().unwrap_or(false),
        "restart stop failed: {second_stop}"
    );
    assert!(
        !second_workdir.exists(),
        "restart stop left {}",
        second_workdir.display()
    );

    client.stop_process();
}

// Needs the debug-only KWIN_MCP_TEST_* hooks in kwin-mcp.
#[cfg(debug_assertions)]
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn startup_timeout_reclaims_children_and_workdir() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    for (stage, stop_bwrap) in [("after-bwrap", false), ("after-bwrap", true)] {
        let mut client = RpcClient::start_with_options_and_stop(Some(stage), stop_bwrap);
        let server_pid = client.pid();
        client.send(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"startup-timeout-e2e","version":"1"}
            }),
        );
        let initialize = client.response(1, Duration::from_secs(10));
        assert!(
            initialize["result"].is_object(),
            "initialize failed: {initialize}"
        );
        client.notify("notifications/initialized", json!({}));
        client.send(
            2,
            "tools/call",
            json!({
                "name":"session_start",
                "arguments":{"width":800,"height":600}
            }),
        );
        let bwrap_line = client.wait_for_stderr("pasta spawned pid=", Duration::from_secs(20));
        let bwrap_pid = bwrap_line
            .split_once("pid=")
            .and_then(|(_, value)| value.trim().parse::<u32>().ok())
            .unwrap_or_else(|| panic!("could not parse bwrap PID: {bwrap_line}"));
        assert!(
            process_alive(bwrap_pid).unwrap_or_else(|error| panic!("inspect bwrap: {error}")),
            "bwrap was not alive before delayed stage {stage}"
        );
        client.wait_for_stderr(&format!("test delay at {stage}"), Duration::from_secs(20));
        let start = client.response(2, Duration::from_secs(45));
        assert!(
            start["error"].is_object() || start["result"]["isError"].as_bool() == Some(true),
            "delayed startup unexpectedly succeeded: {start}"
        );
        let error_message = start["error"]["message"].as_str().unwrap_or_default();
        assert!(
            error_message.contains("exceeded 20s hard limit"),
            "delayed startup did not hit the hard timeout: {start}"
        );
        let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}-1"));
        assert!(!workdir.exists(), "timeout left {}", workdir.display());
        let children = process_children(server_pid)
            .unwrap_or_else(|error| panic!("inspect child processes: {error}"));
        assert!(
            children.is_empty(),
            "timeout left child processes for {stage}: {:?}",
            children
        );
        client.stop_process();
    }
}

// Needs the debug-only KWIN_MCP_TEST_* hooks in kwin-mcp.
#[cfg(debug_assertions)]
#[test]
#[ignore = "requires a live D-Bus session and bubblewrap environment"]
fn first_proxy_failure_reaps_registered_proxy() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    let mut client = RpcClient::start_with_first_proxy_failure();
    let server_pid = client.pid();
    client.send(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18",
            "capabilities":{},
            "clientInfo":{"name":"proxy-rollback-e2e","version":"1"}
        }),
    );
    let initialize = client.response(1, Duration::from_secs(10));
    assert!(
        initialize["result"].is_object(),
        "initialize failed: {initialize}"
    );
    client.notify("notifications/initialized", json!({}));
    client.send(
        2,
        "tools/call",
        json!({
            "name":"session_start",
            "arguments":{"width":800,"height":600}
        }),
    );
    let proxy_line =
        client.wait_for_stderr("test first proxy registered pid=", Duration::from_secs(20));
    let proxy_pid = proxy_line
        .split_once("pid=")
        .and_then(|(_, value)| value.trim().parse::<u32>().ok())
        .unwrap_or_else(|| panic!("could not parse proxy PID: {proxy_line}"));
    assert!(
        process_alive(proxy_pid).unwrap_or_else(|error| panic!("inspect proxy: {error}")),
        "first proxy was not alive before forced failure"
    );
    let start = client.response(2, Duration::from_secs(20));
    let error_message = start["error"]["message"].as_str().unwrap_or_default();
    assert!(
        error_message.contains("test failure after first proxy"),
        "startup did not report the forced proxy failure: {start}"
    );
    let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}-1"));
    assert!(
        !workdir.exists(),
        "proxy failure left {}",
        workdir.display()
    );
    assert!(
        !process_alive(proxy_pid).unwrap_or_else(|error| panic!("inspect reaped proxy: {error}")),
        "first proxy survived forced failure"
    );
    let children = process_children(server_pid)
        .unwrap_or_else(|error| panic!("inspect proxy-failure children: {error}"));
    assert!(
        children.is_empty(),
        "proxy failure left children: {children:?}"
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, browsers, input devices, and a live GPU session"]
fn compound_chrome_gets_browser_switches_and_accessibility_tree() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    let mut client = RpcClient::start();
    client.send(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18",
            "capabilities":{},
            "clientInfo":{"name":"launch-app-e2e","version":"1"}
        }),
    );
    let initialize = client.response(1, Duration::from_secs(10));
    assert!(
        initialize["result"].is_object(),
        "initialize failed: {initialize}"
    );
    client.notify("notifications/initialized", json!({}));

    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1024,"height":768}),
    );
    let workdir = workdir(&started);
    let env_path = workdir.join("browser-env.txt");
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "echo preparing; env | grep -E '^(DBUS_SESSION_BUS_ADDRESS|AT_SPI_BUS_ADDRESS)=' > '{}'; google-chrome-stable --no-first-run https://example.com",
            env_path.display()
        )}),
    );
    let window = launched["result"]["structuredContent"]["window"]
        .as_str()
        .unwrap_or("error");
    assert!(
        launched["error"].is_null()
            && launched["result"]["isError"].as_bool() != Some(true)
            && window.starts_with('{')
            && window.ends_with('}'),
        "Chrome did not create a managed window: {launched}"
    );
    let launch_env = std::fs::read_to_string(&env_path).expect("compound command environment");
    assert!(
        launch_env.contains(&format!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path={}/service_bus_socket",
            workdir.display()
        )) && launch_env.contains("AT_SPI_BUS_ADDRESS="),
        "Chrome launch shell did not inherit the filtered buses: {launch_env}"
    );

    let tree_deadline = Instant::now() + Duration::from_secs(20);
    let mut next_id = 4;
    let _tree = loop {
        let tree = call_tool(
            &mut client,
            next_id,
            "accessibility_tree",
            json!({"max_depth":16}),
        );
        next_id += 1;
        let tree_text = serde_json::to_string(&tree)
            .expect("serialize accessibility tree")
            .to_lowercase();
        if tree_text.contains("document web") && tree_text.contains("example domain") {
            break tree;
        }
        assert!(
            Instant::now() < tree_deadline,
            "Chrome accessibility tree did not expose loaded page content: {tree}"
        );
        thread::sleep(Duration::from_millis(500));
    };

    let argv_path = workdir.join("browser-argv.txt");
    let probe = format!(
        "needle=--force-renderer-$(printf accessibility); for d in /proc/[0-9]*; do p=\"$d/cmdline\"; exe=$(readlink \"$d/exe\" 2>/dev/null) || continue; case \"$exe\" in */chrome|*/google-chrome|*/google-chrome-stable) cmd=$(tr '\\0' ' ' <\"$p\" 2>/dev/null) || continue; case \"$cmd\" in *\"$needle\"*) printf 'pid=%s\\ncmd=%s\\n' \"${{d##*/}}\" \"$cmd\" > '{}'; break;; esac;; esac; done; sleep 2",
        argv_path.display()
    );
    let escaped_probe = probe.replace('\'', "'\\''");
    let probe_result = call_tool(
        &mut client,
        next_id,
        "launch_app",
        json!({"command":format!("konsole --hold -e bash -lc '{}'", escaped_probe)}),
    );
    next_id += 1;
    assert!(
        !probe_result["result"]["isError"].as_bool().unwrap_or(false),
        "argv probe failed: {probe_result}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let argv = loop {
        if let Ok(value) = std::fs::read_to_string(&argv_path) {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "browser argv probe did not produce {}",
            argv_path.display()
        );
        thread::sleep(Duration::from_millis(200));
    };
    assert!(
        argv.starts_with("pid="),
        "probe did not identify a browser PID: {argv}"
    );
    assert!(
        argv.contains("--ozone-platform=wayland"),
        "Chrome argv lacks Wayland switch: {argv}"
    );
    assert!(
        argv.contains("--password-store=kwallet6"),
        "Chrome argv lacks KWallet switch: {argv}"
    );
    assert!(
        argv.contains("--force-renderer-accessibility"),
        "Chrome argv lacks renderer accessibility switch: {argv}"
    );
    let stopped = call_tool(&mut client, next_id, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "session_stop failed: {stopped}"
    );
    assert!(!workdir.exists(), "session_stop left {}", workdir.display());
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, browsers, input devices, and a live GPU session"]
fn wrapped_chrome_gets_browser_switches_in_actual_argv() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    for command in [
        "nohup google-chrome-stable --no-first-run https://example.com >/tmp/chromium.log 2>&1",
        "timeout 30s google-chrome-stable --no-first-run https://example.com",
        "nohup env LANG=C google-chrome-stable --no-first-run https://example.com",
        "timeout 30s env LANG=C google-chrome-stable --no-first-run https://example.com",
    ] {
        let mut client = RpcClient::start();
        client.send(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-06-18",
                "capabilities":{},
                "clientInfo":{"name":"wrapped-launch-e2e","version":"1"}
            }),
        );
        let initialize = client.response(1, Duration::from_secs(10));
        assert!(
            initialize["result"].is_object(),
            "initialize failed for {command}: {initialize}"
        );
        client.notify("notifications/initialized", json!({}));
        let started = call_tool(
            &mut client,
            2,
            "session_start",
            json!({"width":1024,"height":768}),
        );
        let workdir = workdir(&started);
        let launched = call_tool(&mut client, 3, "launch_app", json!({"command":command}));
        let window = launched["result"]["structuredContent"]["window"]
            .as_str()
            .unwrap_or("error");
        assert!(
            launched["error"].is_null()
                && launched["result"]["isError"].as_bool() != Some(true)
                && window.starts_with('{')
                && window.ends_with('}'),
            "wrapped Chrome did not create a managed window for {command}: {launched}"
        );

        let argv_path = workdir.join("browser-argv.txt");
        let probe = format!(
            "needle=--force-renderer-$(printf accessibility); for d in /proc/[0-9]*; do p=\"$d/cmdline\"; exe=$(readlink \"$d/exe\" 2>/dev/null) || continue; case \"$exe\" in */chrome|*/google-chrome|*/google-chrome-stable) cmd=$(tr '\\0' ' ' <\"$p\" 2>/dev/null) || continue; case \"$cmd\" in *\"$needle\"*) printf 'pid=%s\\ncmd=%s\\n' \"${{d##*/}}\" \"$cmd\" > '{}'; break;; esac;; esac; done; sleep 2",
            argv_path.display()
        );
        let escaped_probe = probe.replace('\'', "'\\''");
        let probe_result = call_tool(
            &mut client,
            4,
            "launch_app",
            json!({"command":format!("konsole --hold -e bash -lc '{}'", escaped_probe)}),
        );
        assert!(
            !probe_result["result"]["isError"].as_bool().unwrap_or(false),
            "argv probe failed for {command}: {probe_result}"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let argv = loop {
            if let Ok(value) = std::fs::read_to_string(&argv_path) {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "browser argv probe did not produce {} for {command}",
                argv_path.display()
            );
            thread::sleep(Duration::from_millis(200));
        };
        assert!(
            argv.starts_with("pid=")
                && argv.contains("--ozone-platform=wayland")
                && argv.contains("--password-store=kwallet6")
                && argv.contains("--force-renderer-accessibility"),
            "wrapped Chrome argv missed injected switches for {command}: {argv}"
        );
        let stopped = call_tool(&mut client, 5, "session_stop", json!({}));
        assert!(
            !stopped["result"]["isError"].as_bool().unwrap_or(false),
            "session_stop failed for {command}: {stopped}"
        );
        assert!(!workdir.exists(), "session_stop left {}", workdir.display());
        client.stop_process();
    }
}

// Needs the debug-only KWIN_MCP_TEST_* hooks in kwin-mcp.
#[cfg(debug_assertions)]
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn blocked_host_scan_answers_within_hard_limit_and_cleans_later() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    // A thread sleep stands in for a stat blocked on a hung FUSE mount: no
    // async timeout can preempt it, so only the blocking-thread handoff keeps
    // session_start inside its hard limit.
    let mut client = RpcClient::start_with_env(&[("KWIN_MCP_TEST_BLOCK_HOST_SCAN_MS", "50000")]);
    let server_pid = client.pid();
    initialize(&mut client);
    let started = Instant::now();
    let first = call_tool(&mut client, 2, "session_start", json!({}));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(22),
        "first start took {elapsed:?}: {first}"
    );
    let message = first["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("exceeded 20s hard limit while scanning host mounts"),
        "{first}"
    );
    assert_eq!(
        first["error"]["data"]["host_call_blocked"],
        json!(true),
        "{first}"
    );

    // Each session_start creates a new desktop. Address the timed-out handle
    // to exercise its still-owned lifecycle gate instead of starting another.
    let failed_id = first["error"]["data"]["session_id"]
        .as_str()
        .expect("timed-out startup handle");
    let started = Instant::now();
    let second = call_tool(
        &mut client,
        3,
        "session_stop",
        json!({"session_id":failed_id}),
    );
    assert!(
        started.elapsed() < Duration::from_secs(22),
        "second start: {second}"
    );
    assert_eq!(
        second["error"]["data"]["reason"],
        json!("lifecycle_busy"),
        "{second}"
    );

    let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}-1"));
    client.wait_for_stderr("blocked host scan returned", Duration::from_secs(30));
    assert!(
        !workdir.exists(),
        "deferred cleanup left {}",
        workdir.display()
    );
    assert!(
        process_children(server_pid)
            .unwrap_or_else(|error| panic!("inspect children: {error}"))
            .is_empty(),
        "deferred cleanup left child processes"
    );
    client.stop_process();
}

/// Title of the first window whose title starts with `prefix`, from window_list.
fn window_title(client: &mut RpcClient, id: u64, prefix: &str) -> Option<String> {
    let listed = call_tool(client, id, "window_list", json!({}));
    let text = listed["result"]["content"][0]["text"].as_str()?.to_owned();
    let marker = format!("title=\"{prefix}");
    let start = text.find(&marker)? + "title=\"".len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_owned())
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Chrome, input devices, and a live GPU session"]
fn keyboard_key_resolves_punctuation_combos_and_rejects_unparseable_ones() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1024,"height":768}),
    );
    let workdir = workdir(&started);
    // The page titles itself with the last non-modifier keydown it saw.
    let page = workdir.join("keys.html");
    std::fs::write(
        &page,
        "<html><head><title>K:ready</title></head><body><script>\
         addEventListener('keydown',e=>{if(['Control','Shift','Alt','Meta'].includes(e.key))return;\
         document.title='K:'+e.code+(e.ctrlKey?'+C':'')+(e.shiftKey?'+S':'');e.preventDefault();},true);\
         </script></body></html>",
    )
    .expect("write key page");
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!("google-chrome-stable --no-first-run --new-window 'file://{}'", page.display())}),
    );
    assert!(launched["error"].is_null(), "launch failed: {launched}");
    let mut id = 4;
    let deadline = Instant::now() + Duration::from_secs(15);
    while window_title(&mut client, id, "K:ready").is_none() {
        id += 1;
        assert!(Instant::now() < deadline, "key page did not load");
        thread::sleep(Duration::from_millis(300));
    }
    for (combo, expected) in [
        ("ctrl+minus", "K:Minus+C"),
        ("ctrl+-", "K:Minus+C"),
        ("ctrl+equal", "K:Equal+C"),
        ("ctrl+plus", "K:Equal+C+S"),
        ("ctrl++", "K:Equal+C+S"),
        ("ctrl+Home", "K:Home+C"),
        ("ctrl+L", "K:KeyL+C"),
        ("Return", "K:Enter"),
    ] {
        id += 1;
        let pressed = call_tool(&mut client, id, "keyboard_key", json!({"key":combo}));
        assert!(pressed["error"].is_null(), "{combo} failed: {pressed}");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            id += 1;
            let title = window_title(&mut client, id, "K:").unwrap_or_default();
            if title.starts_with(expected) && title[expected.len()..].starts_with(' ') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{combo}: page saw {title}, expected {expected}"
            );
            thread::sleep(Duration::from_millis(200));
        }
    }
    for combo in ["ctrl+bogus", "foo+a", "ctrl+", ""] {
        id += 1;
        let rejected = call_tool(&mut client, id, "keyboard_key", json!({"key":combo}));
        assert_eq!(
            rejected["error"]["code"],
            json!(-32602),
            "{combo:?} was not rejected: {rejected}"
        );
        thread::sleep(Duration::from_millis(500));
        id += 1;
        let title = window_title(&mut client, id, "K:").unwrap_or_default();
        assert!(
            title.starts_with("K:Enter "),
            "{combo:?} still sent input: {title}"
        );
    }
    id += 1;
    call_tool(&mut client, id, "session_stop", json!({}));
    client.stop_process();
}

fn inline_png(response: &Value) -> String {
    response["result"]["content"]
        .as_array()
        .and_then(|content| content.iter().find(|item| item["type"] == "image"))
        .and_then(|image| image["data"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Chrome, input devices, and a live GPU session"]
fn screenshot_right_after_input_shows_the_input() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1024,"height":768}),
    );
    let workdir = workdir(&started);
    let page = workdir.join("typed.html");
    std::fs::write(
        &page,
        "<html><title>T:ready</title><body style='margin:0'>\
         <input id=i autofocus style='font-size:100px;width:100%;caret-color:transparent'>\
         <div id=d style='font-size:100px'></div>\
         <script>i.oninput=()=>d.textContent=i.value.length</script></body></html>",
    )
    .expect("write typing page");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!("google-chrome-stable --no-first-run --new-window 'file://{}'", page.display())}),
    );
    let mut id = 4;
    let deadline = Instant::now() + Duration::from_secs(15);
    while window_title(&mut client, id, "T:ready").is_none() {
        id += 1;
        assert!(Instant::now() < deadline, "typing page did not load");
        thread::sleep(Duration::from_millis(300));
    }
    id += 1;
    call_tool(&mut client, id, "mouse_click", json!({"x":300,"y":150}));
    thread::sleep(Duration::from_millis(500));
    for round in 0..8 {
        id += 1;
        call_tool(&mut client, id, "keyboard_type", json!({"text":"x"}));
        id += 1;
        // Only the page body: Chrome's toolbar changes on its own as
        // extension icons load, which is not input staleness.
        let immediate = call_tool(
            &mut client,
            id,
            "screenshot",
            json!({"inline":true, "region":[0,120,1024,768]}),
        );
        thread::sleep(Duration::from_millis(800));
        id += 1;
        let later = call_tool(
            &mut client,
            id,
            "screenshot",
            json!({"inline":true, "region":[0,120,1024,768]}),
        );
        if inline_png(&immediate) != inline_png(&later) {
            use base64::Engine;
            for (name, shot) in [("immediate", &immediate), ("later", &later)] {
                let _ = std::fs::write(
                    std::env::temp_dir().join(format!("kwin-mcp-e2e-{name}.png")),
                    base64::engine::general_purpose::STANDARD
                        .decode(inline_png(shot))
                        .unwrap_or_default(),
                );
            }
            eprintln!("immediate: {}", immediate["result"]["content"][1]);
        }
        assert!(
            inline_png(&immediate) == inline_png(&later) && !inline_png(&later).is_empty(),
            "round {round}: screenshot right after keyboard_type predates the input"
        );
        let settle = immediate["result"]["content"][1]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            settle.contains("\"settled\":true"),
            "round {round}: {settle}"
        );
    }
    id += 1;
    call_tool(&mut client, id, "session_stop", json!({}));
    client.stop_process();
}

fn decode_rgba(response: &Value) -> (u32, Vec<u8>) {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inline_png(response))
        .expect("inline screenshot base64");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("screenshot PNG header");
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut pixels)
        .expect("screenshot PNG frame");
    (info.width, pixels)
}

fn pixel(image: &(u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let index = usize::try_from((y * image.0 + x) * 4).expect("pixel index");
    [
        image.1[index],
        image.1[index + 1],
        image.1[index + 2],
        image.1[index + 3],
    ]
}

fn screenshot_meta(response: &Value) -> Value {
    let text = response["result"]["content"][1]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no screenshot metadata in {response}"));
    serde_json::from_str(text).expect("screenshot metadata JSON")
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, kdialog, konsole, input devices, and a live GPU session"]
fn screenshot_pixels_are_mouse_coordinates_for_dialogs_crops_and_maximized_windows() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1920,"height":1080}),
    );
    let workdir = workdir(&started);
    let result_path = workdir.join("kdialog.rc");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!("kdialog --yesno 'Unlock profile?' --yes-label Relaunch --no-label Cancel; echo $? > '{}'", result_path.display())}),
    );
    thread::sleep(Duration::from_millis(1000));

    // Non-maximized dialog at a nonzero origin: the default image is the dialog.
    let full = call_tool(&mut client, 4, "screenshot", json!({"inline":true}));
    let meta = screenshot_meta(&full);
    let (win_w, win_h) = (
        meta["window"]["width"].as_i64().expect("w"),
        meta["window"]["height"].as_i64().expect("h"),
    );
    assert!(
        meta["window"]["x"].as_i64() > Some(0) && meta["window"]["y"].as_i64() > Some(0),
        "{meta}"
    );
    assert_eq!(meta["region"], json!([0, 0, win_w, win_h]), "{meta}");
    assert_eq!(
        (meta["width"].as_i64(), meta["height"].as_i64()),
        (Some(win_w), Some(win_h)),
        "{meta}"
    );

    // A crop past the window edges keeps window-relative coordinates: the
    // dialog's own pixel (5,5) sits at image (45,45) under region origin -40.
    let wide = call_tool(
        &mut client,
        5,
        "screenshot",
        json!({"inline":true, "region":[-40,-40,win_w + 40,win_h + 40]}),
    );
    assert_eq!(
        screenshot_meta(&wide)["region"],
        json!([-40, -40, win_w + 40, win_h + 40])
    );
    let (full_image, wide_image) = (decode_rgba(&full), decode_rgba(&wide));
    assert_eq!(pixel(&full_image, 5, 5), pixel(&wide_image, 45, 45));
    assert_ne!(
        pixel(&wide_image, 45, 45),
        pixel(&wide_image, 5, 5),
        "crop did not include the surroundings"
    );

    // Click the button where it appears in the crop, translated by the
    // documented contract: input = image pixel + region origin.
    let found = call_tool(
        &mut client,
        6,
        "find_ui_elements",
        json!({"query":"Relaunch"}),
    );
    let listing = found["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let numbers: Vec<i64> = listing
        .rsplit_once('(')
        .map(|(_, rest)| rest.trim_end_matches([')', '\n']).replace('x', ","))
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    assert_eq!(numbers.len(), 4, "button geometry: {listing}");
    let (image_x, image_y) = (
        numbers[0] + numbers[2] / 2 + 40,
        numbers[1] + numbers[3] / 2 + 40,
    );
    call_tool(
        &mut client,
        7,
        "mouse_click",
        json!({"x": image_x - 40, "y": image_y - 40}),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result_path.exists() {
        assert!(
            Instant::now() < deadline,
            "click at screenshot coordinates missed the dialog button"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        std::fs::read_to_string(&result_path)
            .unwrap_or_default()
            .trim(),
        "0"
    );

    // Maximized window at the origin: image equals the display.
    call_tool(&mut client, 8, "launch_app", json!({"command":"konsole"}));
    let maximized = call_tool(&mut client, 9, "screenshot", json!({}));
    let meta = &maximized["result"]["structuredContent"];
    assert_eq!(
        (meta["window"]["x"].as_i64(), meta["window"]["y"].as_i64()),
        (Some(0), Some(0)),
        "{meta}"
    );
    assert_eq!(
        (meta["width"].as_i64(), meta["height"].as_i64()),
        (Some(1920), Some(1080)),
        "{meta}"
    );
    call_tool(&mut client, 10, "session_stop", json!({}));
    client.stop_process();
}

fn save_inline_screenshot(result: &Value, label: &str) -> PathBuf {
    use base64::Engine;

    assert!(result["error"].is_null(), "{result}");
    let image = result["result"]["content"]
        .as_array()
        .expect("screenshot content")
        .iter()
        .find(|item| item["type"] == "image")
        .expect("inline screenshot image");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().expect("screenshot data"))
        .expect("decode screenshot");
    let path =
        std::env::temp_dir().join(format!("kwin-mcp-300-{label}-{}.png", std::process::id()));
    std::fs::write(&path, bytes).expect("write live screenshot");
    eprintln!("live screenshot: {}", path.display());
    path
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, viewer GPU, and a host Wayland session"]
fn viewer_input_only_pauses_agent_input_for_the_recent_quiet_period() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut desktop = RpcClient::start();
    initialize(&mut desktop);
    let desktop_started = call_tool(
        &mut desktop,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert!(desktop_started["error"].is_null(), "{desktop_started}");
    let desktop_id = desktop_started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("desktop session id");
    let display = workdir(&desktop_started).join("wayland-0");
    let display = display.to_str().expect("desktop Wayland socket").to_owned();

    // Run the viewer as a real window on the outer KWin desktop. Input sent
    // through the outer MCP session reaches that window as viewer input.
    let mut client = RpcClient::start_with_env(&[("WAYLAND_DISPLAY", &display)]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert!(started["error"].is_null(), "{started}");
    let session_id = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session id");
    let session_dir = workdir(&started);
    let opened = call_tool(
        &mut client,
        3,
        "viewer_open",
        json!({"session_id":session_id}),
    );
    assert_eq!(
        opened["result"]["structuredContent"]["status"],
        json!("opened"),
        "{opened}"
    );
    assert_eq!(
        opened["result"]["structuredContent"]["viewer"]["state"],
        json!("ready"),
        "{opened}"
    );

    let viewer_window = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let listed = call_tool(
                &mut desktop,
                3,
                "window_list",
                json!({"session_id":desktop_id}),
            );
            assert!(listed["error"].is_null(), "{listed}");
            let rows = listed["result"]["structuredContent"]["windows"]
                .as_array()
                .expect("desktop windows");
            if let Some(viewer) = rows.iter().find(|row| {
                row["title"]
                    .as_str()
                    .is_some_and(|title| title.starts_with("kwin-viewer"))
                    || row["resourceClass"]
                        .as_str()
                        .is_some_and(|class| class.contains("kwin-viewer"))
            }) {
                break viewer.clone();
            }
            assert!(
                Instant::now() < deadline,
                "viewer window did not appear: {rows:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    };
    let activated = call_tool(
        &mut desktop,
        4,
        "window_activate",
        json!({"session_id":desktop_id,"window_id":viewer_window["id"]}),
    );
    assert!(activated["error"].is_null(), "{activated}");

    let page = "data:text/html,%3C!doctype%20html%3E%3Chtml%3E%3Cbody%3E%3Ch1%3EViewer%20input%20probe%3C/h1%3E%3Clabel%3EBirth%20date:%20%3Cinput%20type%3Ddate%20aria-label%3D%22Birth%20date%22%20autofocus%3E%3C/label%3E%3Cbr%3E%3Clabel%3ERepeat%20probe:%20%3Cinput%20type%3Dtext%20aria-label%3D%22Repeat%20probe%22%3E%3C/label%3E%3C/body%3E%3C/html%3E";
    let launched = call_tool(
        &mut client,
        4,
        "launch_app",
        json!({"command":format!("google-chrome-stable --no-first-run --app='{page}'")}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let mut next_client_id = 5;
    let tree_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let tree = call_tool(
            &mut client,
            next_client_id,
            "accessibility_tree",
            json!({"session_id":session_id,"max_depth":16}),
        );
        next_client_id += 1;
        let tree_text = serde_json::to_string(&tree)
            .expect("serialize date field accessibility tree")
            .to_lowercase();
        if tree_text.contains("birth date") && tree_text.contains("repeat probe") {
            break;
        }
        assert!(
            Instant::now() < tree_deadline,
            "date page did not load: {tree}"
        );
        thread::sleep(Duration::from_millis(250));
    }
    let date_before = call_tool(
        &mut client,
        next_client_id,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    next_client_id += 1;
    let _date_before_image = save_inline_screenshot(&date_before, "date-field-before");

    // A minimized Wayland window may stop submitting frames while its event
    // loop remains live. Its status heartbeat must not depend on rendering.
    let viewer_before = call_tool(
        &mut desktop,
        9000,
        "screenshot",
        json!({"session_id":desktop_id}),
    );
    if let Some(proof) = std::env::var_os("KWIN_MCP_E2E_PROOF_DIR").map(PathBuf::from) {
        std::fs::create_dir_all(&proof).expect("create before idle evidence");
        let path = viewer_before["result"]["structuredContent"]["path"]
            .as_str()
            .expect("before idle screenshot");
        std::fs::copy(path, proof.join("before-idle-viewer.png")).expect("copy before idle viewer");
    }
    let status_path = session_dir.join("viewer-status.json");
    let viewer_pid = opened["result"]["structuredContent"]["viewer"]["pid"]
        .as_u64()
        .expect("owned viewer pid");
    let covered = call_tool(
        &mut desktop,
        9001,
        "launch_app",
        json!({"session_id":desktop_id,"command":"konsole --separate"}),
    );
    assert!(covered["error"].is_null(), "{covered}");
    let activate_for_minimize = call_tool(
        &mut desktop,
        9010,
        "window_activate",
        json!({"session_id":desktop_id,"window_id":viewer_window["id"]}),
    );
    assert!(
        activate_for_minimize["error"].is_null(),
        "{activate_for_minimize}"
    );
    call_tool(
        &mut desktop,
        9011,
        "keyboard_key",
        json!({"session_id":desktop_id,"key":"alt+F3"}),
    );
    call_tool(
        &mut desktop,
        9012,
        "mouse_click",
        json!({"session_id":desktop_id,"x":90,"y":37}),
    );
    let minimize_deadline = Instant::now() + Duration::from_secs(5);
    let mut check_id = 9020;
    loop {
        let windows = call_tool(
            &mut desktop,
            check_id,
            "window_list",
            json!({"session_id":desktop_id}),
        );
        check_id += 1;
        let minimized = windows["result"]["structuredContent"]["windows"]
            .as_array()
            .is_some_and(|windows| {
                windows.iter().any(|window| {
                    window["id"] == viewer_window["id"] && window["minimized"] == true
                })
            });
        if minimized {
            break;
        }
        assert!(
            Instant::now() < minimize_deadline,
            "viewer did not minimize: {windows}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_millis(1200));
    let activity_before = std::fs::metadata(session_dir.join("viewer-input-activity"))
        .expect("viewer activity file")
        .modified()
        .expect("activity timestamp");
    thread::sleep(Duration::from_secs(35));
    let status: Value = serde_json::from_str(
        &std::fs::read_to_string(&status_path).expect("viewer heartbeat report"),
    )
    .expect("viewer heartbeat JSON");
    assert_eq!(
        status["pid"], viewer_pid,
        "idle refresh replaced the viewer"
    );
    let status_age = std::fs::metadata(&status_path)
        .expect("viewer status metadata")
        .modified()
        .expect("viewer status timestamp")
        .elapsed()
        .expect("viewer status age");
    assert!(
        status_age < Duration::from_secs(3),
        "idle viewer status became stale: {status_age:?}"
    );
    assert_eq!(
        std::fs::metadata(session_dir.join("viewer-input-activity"))
            .expect("idle activity metadata")
            .modified()
            .expect("idle activity timestamp"),
        activity_before,
        "idle heartbeat falsely recorded human input"
    );
    let idle_input = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert!(idle_input["error"].is_null(), "{idle_input}");

    // An actually frozen event loop must still fail closed. A background
    // heartbeat thread would incorrectly hide this condition.
    let pid = nix::unistd::Pid::from_raw(i32::try_from(viewer_pid).expect("viewer pid range"));
    let resume_viewer = ResumeProcess(pid);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).expect("pause owned viewer");
    thread::sleep(Duration::from_secs(4));
    let frozen = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert!(
        !frozen["error"].is_null() && frozen.to_string().contains("stale"),
        "{frozen}"
    );
    drop(resume_viewer);
    let heartbeat_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let age = std::fs::metadata(&status_path)
            .expect("resumed status metadata")
            .modified()
            .expect("resumed status timestamp")
            .elapsed()
            .expect("resumed status age");
        if age < Duration::from_secs(2) {
            break;
        }
        assert!(
            Instant::now() < heartbeat_deadline,
            "resumed viewer did not refresh status"
        );
        thread::sleep(Duration::from_millis(100));
    }
    call_tool(
        &mut desktop,
        9002,
        "keyboard_key",
        json!({"session_id":desktop_id,"key":"alt+F4"}),
    );
    let reactivated = call_tool(
        &mut desktop,
        9003,
        "window_activate",
        json!({"session_id":desktop_id,"window_id":viewer_window["id"]}),
    );
    assert!(reactivated["error"].is_null(), "{reactivated}");
    let viewer_after = call_tool(
        &mut desktop,
        9004,
        "screenshot",
        json!({"session_id":desktop_id}),
    );
    if let Some(proof) = std::env::var_os("KWIN_MCP_E2E_PROOF_DIR").map(PathBuf::from) {
        std::fs::create_dir_all(&proof).expect("create idle viewer evidence");
        let path = viewer_after["result"]["structuredContent"]["path"]
            .as_str()
            .expect("viewer recovery screenshot");
        std::fs::copy(path, proof.join("after-idle-viewer.png")).expect("copy idle viewer image");
        std::fs::write(
            proof.join("idle-viewer.json"),
            serde_json::to_vec_pretty(&json!({
                "idle_seconds":35,"minimized":true,"status_age_ms":status_age.as_millis(),"viewer_pid":viewer_pid,
                "idle_input":idle_input,"frozen_input":frozen,
                "before_window":viewer_before["result"]["structuredContent"]["window"],
                "after_window":viewer_after["result"]["structuredContent"]["window"]
            }))
            .expect("encode viewer evidence"),
        )
        .expect("write viewer evidence");
    }

    let (typed, during_typing) = thread::scope(|scope| {
        let (started_tx, started_rx) = mpsc::channel();
        let screenshot_id = next_client_id;
        next_client_id += 1;
        let client = &mut client;
        let screenshot = scope.spawn(move || {
            started_rx.recv().expect("typing started");
            thread::sleep(Duration::from_millis(150));
            call_tool(
                client,
                screenshot_id,
                "screenshot",
                json!({"session_id":session_id,"inline":true}),
            )
        });
        started_tx.send(()).expect("start concurrent screenshot");
        let typed = call_tool(
            &mut desktop,
            5,
            "keyboard_type",
            json!({"session_id":desktop_id,"text":"10122026"}),
        );
        let screenshot = screenshot.join().expect("agent screenshot while typing");
        (typed, screenshot)
    });
    assert!(typed["error"].is_null(), "{typed}");
    let _during_typing_image = save_inline_screenshot(&during_typing, "date-during-typing");
    let activity_path = session_dir.join("viewer-input-activity");
    wait_for_recent_viewer_input(&activity_path);
    let paused = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert_eq!(
        paused["error"]["data"]["reason"],
        json!("recent_viewer_input"),
        "{paused}"
    );
    assert_eq!(
        paused["error"]["data"]["retry_safe"],
        json!(true),
        "{paused}"
    );

    thread::sleep(Duration::from_millis(1100));
    let resumed_input = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert!(resumed_input["error"].is_null(), "{resumed_input}");
    let date_screenshot = call_tool(
        &mut client,
        next_client_id,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    next_client_id += 1;
    let _date_image = save_inline_screenshot(&date_screenshot, "date-field");

    let clicked = call_tool(
        &mut desktop,
        6,
        "mouse_click",
        json!({"session_id":desktop_id,"x":150,"y":114}),
    );
    assert!(clicked["error"].is_null(), "{clicked}");
    wait_for_recent_viewer_input(&activity_path);
    let repeat_field = call_tool(
        &mut client,
        next_client_id,
        "find_ui_elements",
        json!({"session_id":session_id,"query":"Repeat probe"}),
    );
    next_client_id += 1;
    let repeat_field_text = serde_json::to_string(&repeat_field)
        .expect("serialize repeat field")
        .to_lowercase();
    assert!(
        repeat_field_text.contains("repeat probe") && repeat_field_text.contains("focused"),
        "repeat field is missing or not focused: {repeat_field}"
    );

    thread::sleep(Duration::from_millis(1100));
    // Viewer input must interrupt an already admitted paced operation, too.
    let input_log = started["result"]["structuredContent"]["input_log"]
        .as_str()
        .expect("input log path");
    let interrupted_id = next_client_id;
    next_client_id += 1;
    let interrupted = thread::scope(|scope| {
        let (begin_tx, begin_rx) = mpsc::channel();
        let client = &mut client;
        let typing = scope.spawn(move || {
            begin_tx.send(()).expect("start agent typing");
            call_tool(
                client,
                interrupted_id,
                "keyboard_type",
                json!({"session_id":session_id,"text":"x".repeat(300)}),
            )
        });
        begin_rx.recv().expect("agent typing submitted");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let sent = std::fs::read_to_string(input_log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|record| {
                    record["tool"]["request_id"] == interrupted_id
                        && record["character"] == "x"
                        && record["pressed"] == true
                });
            if sent {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "agent did not begin key submission"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let human = call_tool(
            &mut desktop,
            60,
            "keyboard_key",
            json!({"session_id":desktop_id,"key":"b"}),
        );
        assert!(human["error"].is_null(), "{human}");
        let began = Instant::now();
        let interrupted = typing.join().expect("agent typing completion");
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "agent typing did not yield promptly"
        );
        interrupted
    });
    assert_eq!(
        interrupted["error"]["data"]["reason"], "viewer_input_interrupted",
        "{interrupted}"
    );
    assert_eq!(
        interrupted["error"]["data"]["retry_safe"], false,
        "{interrupted}"
    );
    thread::sleep(Duration::from_millis(1100));
    for key in ["ctrl+a", "ctrl+c"] {
        let response = call_tool(
            &mut client,
            next_client_id,
            "keyboard_key",
            json!({"session_id":session_id,"key":key}),
        );
        next_client_id += 1;
        assert!(response["error"].is_null(), "{response}");
    }
    let receipt = call_tool(
        &mut client,
        next_client_id,
        "clipboard_read",
        json!({"session_id":session_id}),
    );
    next_client_id += 1;
    let text = receipt["result"]["structuredContent"]["text"]
        .as_str()
        .expect("interrupted field text");
    assert!(
        text.contains('b') && text.contains('x') && text.matches('x').count() < 300,
        "human input lost or agent typing continued: {text}"
    );
    let interrupted_screen = call_tool(
        &mut client,
        next_client_id,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    next_client_id += 1;
    save_inline_screenshot(&interrupted_screen, "viewer-interrupted-agent");
    let cleared = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Backspace"}),
    );
    next_client_id += 1;
    assert!(cleared["error"].is_null(), "{cleared}");
    let baseline_press = call_tool(
        &mut client,
        next_client_id,
        "keyboard_press",
        json!({"session_id":session_id,"key":"a"}),
    );
    next_client_id += 1;
    assert!(baseline_press["error"].is_null(), "{baseline_press}");
    thread::sleep(Duration::from_millis(1100));
    let baseline_held = call_tool(
        &mut client,
        next_client_id,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    next_client_id += 1;
    let _baseline_held_image = save_inline_screenshot(&baseline_held, "direct-key-held");
    let baseline_release = call_tool(
        &mut client,
        next_client_id,
        "keyboard_release",
        json!({"session_id":session_id,"key":"a"}),
    );
    next_client_id += 1;
    assert!(baseline_release["error"].is_null(), "{baseline_release}");
    let baseline_select = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"ctrl+a"}),
    );
    next_client_id += 1;
    assert!(baseline_select["error"].is_null(), "{baseline_select}");
    let baseline_copy = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"ctrl+c"}),
    );
    next_client_id += 1;
    assert!(baseline_copy["error"].is_null(), "{baseline_copy}");
    let baseline_clipboard = call_tool(
        &mut client,
        next_client_id,
        "clipboard_read",
        json!({"session_id":session_id}),
    );
    next_client_id += 1;
    let baseline_text = baseline_clipboard["result"]["structuredContent"]["text"]
        .as_str()
        .expect("baseline repeat text");
    let baseline_repeats = baseline_text
        .chars()
        .filter(|character| *character == 'a')
        .count();
    assert!(
        baseline_repeats > 1,
        "held key did not repeat: {baseline_text}"
    );
    let clear = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Backspace"}),
    );
    next_client_id += 1;
    assert!(clear["error"].is_null(), "{clear}");
    let clear_clipboard = call_tool(
        &mut client,
        next_client_id,
        "clipboard_write",
        json!({"session_id":session_id,"target":"session","text":""}),
    );
    next_client_id += 1;
    assert!(clear_clipboard["error"].is_null(), "{clear_clipboard}");

    let pressed = call_tool(
        &mut desktop,
        7,
        "keyboard_press",
        json!({"session_id":desktop_id,"key":"a"}),
    );
    assert!(pressed["error"].is_null(), "{pressed}");
    thread::sleep(Duration::from_millis(1100));
    let repeat_held = call_tool(
        &mut client,
        next_client_id,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    next_client_id += 1;
    let _repeat_held_image = save_inline_screenshot(&repeat_held, "held-key-repeat");
    let released = call_tool(
        &mut desktop,
        8,
        "keyboard_release",
        json!({"session_id":desktop_id,"key":"a"}),
    );
    assert!(released["error"].is_null(), "{released}");
    let repeat_result = call_tool(
        &mut client,
        next_client_id,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    next_client_id += 1;
    let _repeat_result_image = save_inline_screenshot(&repeat_result, "held-key-result");
    thread::sleep(Duration::from_millis(1100));
    let viewer_select = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"ctrl+a"}),
    );
    next_client_id += 1;
    assert!(viewer_select["error"].is_null(), "{viewer_select}");
    let viewer_copy = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"ctrl+c"}),
    );
    next_client_id += 1;
    assert!(viewer_copy["error"].is_null(), "{viewer_copy}");
    let viewer_clipboard = call_tool(
        &mut client,
        next_client_id,
        "clipboard_read",
        json!({"session_id":session_id}),
    );
    next_client_id += 1;
    let viewer_text = viewer_clipboard["result"]["structuredContent"]["text"]
        .as_str()
        .expect("viewer repeat text");
    let viewer_repeats = viewer_text
        .chars()
        .filter(|character| *character == 'a')
        .count();
    let repeat_margin = baseline_repeats / 4 + 2;
    eprintln!("held-key repeat count: session={baseline_repeats}, viewer={viewer_repeats}");
    assert!(
        viewer_repeats > 1 && viewer_repeats <= baseline_repeats + repeat_margin,
        "viewer hold repeated at a different rate: session={baseline_repeats}, viewer={viewer_repeats}"
    );

    let still_open = call_tool(
        &mut client,
        next_client_id,
        "viewer_open",
        json!({"session_id":session_id}),
    );
    next_client_id += 1;
    assert_eq!(
        still_open["result"]["structuredContent"]["status"],
        json!("already_open"),
        "{still_open}"
    );

    let closed = call_tool(
        &mut client,
        next_client_id,
        "viewer_close",
        json!({"session_id":session_id}),
    );
    next_client_id += 1;
    assert!(closed["error"].is_null(), "{closed}");

    // A remote --serve relay has no local viewer_child. Send the same key
    // records --remote writes so the server sees activity through its actual
    // relay input path rather than a test-written marker timestamp.
    thread::sleep(Duration::from_millis(1100));
    let quiet_input = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert!(quiet_input["error"].is_null(), "{quiet_input}");

    let mut relay = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_kwin-viewer"))
            .args(["--serve"])
            .arg(&session_dir)
            .args(["800", "600"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start remote viewer relay"),
    );
    let relay_pid = u64::from(relay.0.id());
    let relay_status = session_dir.join("viewer-status.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(&relay_status)
            && let Ok(status) = serde_json::from_str::<Value>(&contents)
            && status["pid"].as_u64() == Some(relay_pid)
            && (status["state"] == "starting" || status["state"] == "ready")
        {
            break;
        }
        if let Some(status) = relay.0.try_wait().expect("check remote relay") {
            panic!("remote relay exited before input: {status}");
        }
        assert!(Instant::now() < deadline, "remote relay did not start");
        thread::sleep(Duration::from_millis(20));
    }

    let quiet_remote_input = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert!(
        quiet_remote_input["error"].is_null(),
        "{quiet_remote_input}"
    );
    let activity_before_remote_input = std::fs::metadata(&activity_path)
        .and_then(|metadata| metadata.modified())
        .expect("activity timestamp before remote input");
    let relay_stdin = relay.0.stdin.as_mut().expect("remote relay input");
    for pressed in [1_u64, 0] {
        let mut record = [0_u8; 17];
        record[0] = 4; // Op::Key
        record[1..9].copy_from_slice(&1_u64.to_le_bytes()); // Escape
        record[9..17].copy_from_slice(&pressed.to_le_bytes());
        relay_stdin
            .write_all(&record)
            .expect("send key through remote relay");
    }
    relay_stdin.flush().expect("flush remote relay input");
    let remote_deadline = Instant::now() + Duration::from_secs(3);
    let remote_activity = loop {
        let modified = std::fs::metadata(&activity_path)
            .and_then(|metadata| metadata.modified())
            .expect("remote viewer activity timestamp");
        if modified > activity_before_remote_input {
            break modified;
        }
        assert!(
            Instant::now() < remote_deadline,
            "remote relay did not update input activity"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        SystemTime::now()
            .duration_since(remote_activity)
            .unwrap_or_default()
            <= Duration::from_secs(1),
        "remote viewer activity was not recent"
    );
    let remote_paused = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert_eq!(
        remote_paused["error"]["data"]["reason"],
        json!("recent_viewer_input"),
        "{remote_paused}"
    );
    assert_eq!(
        remote_paused["error"]["data"]["retry_safe"],
        json!(true),
        "{remote_paused}"
    );
    thread::sleep(Duration::from_millis(1100));
    let remote_resumed = call_tool(
        &mut client,
        next_client_id,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Escape"}),
    );
    next_client_id += 1;
    assert!(remote_resumed["error"].is_null(), "{remote_resumed}");

    drop(relay);
    call_tool(
        &mut client,
        next_client_id,
        "session_stop",
        json!({"session_id":session_id}),
    );
    client.stop_process();
    call_tool(
        &mut desktop,
        9,
        "session_stop",
        json!({"session_id":desktop_id}),
    );
    desktop.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, kdotool, a live GPU session, and a host Wayland session"]
fn viewer_opens_on_demand_and_closes_without_stopping_session() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let host_display = std::env::var_os("XDG_RUNTIME_DIR")
        .zip(std::env::var_os("WAYLAND_DISPLAY"))
        .map(|(runtime, display)| PathBuf::from(runtime).join(display));
    if !host_display.as_deref().is_some_and(std::path::Path::exists) {
        eprintln!("no host Wayland display; skipping");
        return;
    }
    // The default starts without a host viewer.
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let viewer = &started["result"]["structuredContent"]["viewer"];
    assert_eq!(viewer["state"], json!("closed"), "{started}");
    assert!(
        !workdir(&started).join("viewer.log").exists(),
        "session_start launched a viewer"
    );
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":"kdialog --title viewer-first-frame --msgbox 'First frame content'"}),
    );
    assert!(
        !launched["result"]["isError"].as_bool().unwrap_or(false),
        "viewer frame fixture did not launch: {launched}"
    );
    let captured = call_tool(&mut client, 4, "screenshot", json!({}));
    assert!(
        !captured["result"]["isError"].as_bool().unwrap_or(false),
        "session screenshot before viewer_open failed: {captured}"
    );
    let opened = call_tool(&mut client, 5, "viewer_open", json!({}));
    let viewer = &opened["result"]["structuredContent"]["viewer"];
    let log_path = opened["result"]["structuredContent"]["viewer"]["log"]
        .as_str()
        .unwrap_or("");
    let viewer_log = std::fs::read_to_string(log_path)
        .unwrap_or_else(|error| format!("could not read viewer log: {error}"));
    assert_eq!(
        opened["result"]["structuredContent"]["status"],
        json!("opened"),
        "{opened}\n{viewer_log}"
    );
    assert_eq!(viewer["state"], json!("ready"), "{opened}\n{viewer_log}");
    assert!(
        viewer_log.contains("first frame submitted for presentation"),
        "viewer did not submit its first frame: {opened}\n{viewer_log}"
    );
    let configured = viewer_log.find("initial nonzero resize received");
    let first_redraw = viewer_log.find("first host window redraw");
    assert!(
        configured
            .zip(first_redraw)
            .is_some_and(|(configured, redraw)| configured < redraw),
        "viewer drew before initial Wayland configure: {opened}\n{viewer_log}"
    );
    assert!(
        viewer_log.contains("first PipeWire frame consumed by host window"),
        "viewer did not consume its first PipeWire frame: {opened}\n{viewer_log}"
    );
    let consumed = viewer_log.find("first PipeWire frame consumed by host window");
    let submitted = viewer_log.find("first frame submitted for presentation");
    assert!(
        consumed
            .zip(submitted)
            .is_some_and(|(consumed, submitted)| consumed < submitted),
        "viewer reported submission before consuming its first PipeWire frame: {opened}\n{viewer_log}"
    );
    let pid = u32::try_from(viewer["pid"].as_u64().expect("viewer pid")).expect("pid fits");
    assert!(
        process_alive(pid).unwrap_or(false),
        "reported viewer is not running"
    );
    assert_eq!(
        wait_for_viewer_windows(pid, 1).len(),
        1,
        "viewer_open did not create exactly one host window: pid={pid} opened={opened}\n{viewer_log}"
    );
    thread::sleep(Duration::from_secs(4));
    let again = call_tool(&mut client, 6, "viewer_open", json!({}));
    assert_eq!(
        again["result"]["structuredContent"]["status"],
        json!("already_open"),
        "{again}"
    );
    let windows = viewer_windows(pid);
    assert_eq!(windows.len(), 1, "viewer window disappeared before close");
    let close_window = Command::new("kdotool")
        .env("KDE_SESSION_VERSION", "6")
        .args(["windowclose", windows[0].as_str()])
        .output()
        .expect("close host viewer window");
    assert!(
        close_window.status.success(),
        "kdotool windowclose failed: {}",
        String::from_utf8_lossy(&close_window.stderr)
    );
    assert!(
        wait_for_viewer_windows(pid, 0).is_empty(),
        "host window close left the viewer window visible"
    );

    let reopened = call_tool(&mut client, 7, "viewer_open", json!({}));
    assert_eq!(
        reopened["result"]["structuredContent"]["status"],
        json!("opened"),
        "{reopened}"
    );
    let reopened_viewer = &reopened["result"]["structuredContent"]["viewer"];
    let reopened_pid = u32::try_from(
        reopened_viewer["pid"]
            .as_u64()
            .expect("reopened viewer pid"),
    )
    .expect("reopened pid fits");
    assert_ne!(
        reopened_pid, pid,
        "viewer_open reused a closed viewer process"
    );
    assert!(
        process_alive(reopened_pid).unwrap_or(false),
        "reopened viewer is not running"
    );
    assert_eq!(
        wait_for_viewer_windows(reopened_pid, 1).len(),
        1,
        "viewer_open did not recreate the host window: pid={reopened_pid} opened={reopened}"
    );

    let closed = call_tool(&mut client, 8, "viewer_close", json!({}));
    assert_eq!(
        closed["result"]["structuredContent"]["status"],
        json!("closed"),
        "{closed}"
    );
    assert_eq!(
        closed["result"]["structuredContent"]["viewer"]["state"],
        json!("closed"),
        "{closed}"
    );
    assert!(
        !process_alive(reopened_pid).unwrap_or(true),
        "viewer remained alive after viewer_close"
    );
    assert!(
        wait_for_viewer_windows(reopened_pid, 0).is_empty(),
        "viewer_close left a host window"
    );
    let running = call_tool(&mut client, 9, "session_list", json!({}));
    assert_eq!(
        running["result"]["structuredContent"]["sessions"]
            .as_array()
            .expect("live sessions")
            .len(),
        1
    );
    call_tool(&mut client, 10, "session_stop", json!({}));
    client.stop_process();

    // A missing host Wayland display affects viewer_open, not session_start.
    let mut client = RpcClient::start_program(
        env!("CARGO_BIN_EXE_kwin-mcp"),
        None,
        false,
        false,
        &[("WAYLAND_DISPLAY", "kwin-mcp-no-such-display")],
    );
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let content = &started["result"]["structuredContent"];
    assert_eq!(content["status"], json!("started"), "{started}");
    assert_eq!(content["viewer"]["state"], json!("closed"), "{started}");
    let opened = call_tool(&mut client, 3, "viewer_open", json!({}));
    let content = &opened["result"]["structuredContent"];
    assert_eq!(content["status"], json!("unavailable"), "{opened}");
    assert_eq!(content["viewer"]["state"], json!("unavailable"), "{opened}");
    assert!(
        content["viewer"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("host Wayland resolution failed"),
        "{opened}"
    );
    call_tool(&mut client, 4, "session_stop", json!({}));
    client.stop_process();
}

/// Remaining bytes of this user's disk quota on the filesystem holding `dir`,
/// or None when it has no user quota.
fn user_quota_headroom(dir: &std::path::Path) -> Option<u64> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let handle = std::fs::File::open(dir).ok()?;
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let mut quota = [0u64; 9];
    // SAFETY: Q_GETQUOTA writes one 72-byte struct if_dqblk into `quota`.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_quotactl_fd,
            handle.as_raw_fd(),
            0x0080_0007 << 8, // QCMD(Q_GETQUOTA, USRQUOTA)
            uid,
            quota.as_mut_ptr(),
        )
    };
    (result == 0 && quota[0] > 0).then(|| (quota[0] * 1024).saturating_sub(quota[2]))
}

struct FillFile(PathBuf);
impl Drop for FillFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, a /tmp tmpfs with usrquota, and enough RAM to fill that quota"]
fn screenshot_at_user_quota_reports_the_limit_and_leaves_no_partial_file() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let Some(_) = user_quota_headroom(std::path::Path::new("/tmp")) else {
        eprintln!("/tmp has no user quota; skipping");
        return;
    };
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let workdir = workdir(&started);
    call_tool(&mut client, 3, "launch_app", json!({"command":"konsole"}));
    let first = call_tool(&mut client, 4, "screenshot", json!({}));
    assert!(first["error"].is_null(), "{first}");
    let screenshot = workdir.join("screenshot.png");
    assert!(screenshot.exists());

    // Use up the quota, leaving less than one screenshot of headroom, while
    // the filesystem itself keeps free space: the situation behind EDQUOT.
    let fill = FillFile(
        std::env::temp_dir().join(format!("kwin-mcp-e2e-quota-fill-{}", std::process::id())),
    );
    std::fs::remove_file(&screenshot).expect("remove first screenshot");
    let headroom = user_quota_headroom(std::path::Path::new("/tmp")).expect("quota headroom");
    let file = std::fs::File::create(&fill.0).expect("create fill file");
    nix::fcntl::posix_fallocate(
        &file,
        0,
        i64::try_from(headroom - 16 * 1024).expect("fill size"),
    )
    .expect("fill quota");

    let failed = call_tool(&mut client, 5, "screenshot", json!({}));
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Disk quota exceeded") && message.contains("Per-user disk quota"),
        "{failed}"
    );
    assert!(
        !screenshot.exists(),
        "failed write left a partial screenshot"
    );

    let inline = call_tool(&mut client, 6, "screenshot", json!({"inline":true}));
    assert!(
        !inline_png(&inline).is_empty(),
        "inline screenshot missing at quota: {inline}"
    );
    assert!(
        inline["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("file not saved"),
        "{inline}"
    );
    assert!(
        !screenshot.exists(),
        "inline screenshot at quota left a partial file"
    );

    // An open descriptor keeps the space charged, so close it before removing.
    drop(file);
    drop(fill);
    let recovered = call_tool(&mut client, 7, "screenshot", json!({}));
    assert!(
        recovered["error"].is_null() && screenshot.exists(),
        "{recovered}"
    );
    call_tool(&mut client, 8, "session_stop", json!({}));
    client.stop_process();
}

#[test]
#[ignore = "run through tests/screenshot_storage_e2e.sh with KDE, KWin, bubblewrap and kdialog"]
fn screenshot_at_full_workdir_keeps_desktop_inspection_available() {
    use base64::Engine;
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let temporary = std::env::temp_dir();
    assert_eq!(
        std::env::var_os("KWIN_MCP_E2E_PRIVATE_TMPFS"),
        Some(temporary.clone().into_os_string()),
        "use the wrapper: never fill the host's shared /tmp"
    );
    let filesystem = nix::sys::statfs::statfs(&temporary).expect("private tmpfs");
    assert_eq!(filesystem.filesystem_type(), nix::sys::statfs::TMPFS_MAGIC);
    let stat = nix::sys::statvfs::statvfs(&temporary).expect("private tmpfs capacity");
    assert!(stat.blocks() * stat.fragment_size() <= 1024 * 1024 * 1024);
    let proof =
        PathBuf::from(std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR").expect("proof directory"));
    std::fs::create_dir_all(&proof).expect("proof directory");

    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1440,"height":900}),
    );
    let workdir = workdir(&started);
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":"test ! -w \"$XDG_RUNTIME_DIR/kwin-scripts\" && kdialog --title kwin319 --msgbox 'Screenshot storage regression #319'"}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    for id in 4..7 {
        let shot = call_tool(&mut client, id, "screenshot", json!({"inline":true}));
        assert!(!inline_png(&shot).is_empty(), "{shot}");
    }
    let screenshot = workdir.join("screenshot.png");
    std::fs::copy(&screenshot, proof.join("before.png")).expect("before screenshot");
    std::fs::remove_file(&screenshot).expect("remove previous screenshot");

    // Fill only the wrapper's small private mount. Unlike the user-quota case,
    // leave no page for the active-window script that precedes PNG capture.
    let fill = FillFile(temporary.join("fill"));
    let file = std::fs::File::create(&fill.0).expect("private fill file");
    let stat = nix::sys::statvfs::statvfs(&temporary).expect("private headroom");
    let remaining = stat.blocks_available() * stat.fragment_size();
    nix::fcntl::posix_fallocate(
        &file,
        0,
        i64::try_from(remaining).expect("bounded fill size"),
    )
    .expect("fill private tmpfs");
    assert_eq!(
        nix::sys::statvfs::statvfs(&temporary)
            .expect("full tmpfs")
            .blocks_available(),
        0
    );

    let failed = call_tool(&mut client, 7, "screenshot", json!({}));
    std::fs::write(proof.join("full-workdir.json"), failed.to_string()).expect("failure evidence");
    assert_eq!(
        failed["error"]["data"]["reason"], "write_failed",
        "{failed}"
    );
    let message = failed["error"]["message"]
        .as_str()
        .expect("storage diagnostic");
    assert!(message.contains("No space left on device"), "{message}");
    assert!(message.contains("inline=true"), "{message}");
    assert!(!screenshot.exists());
    assert!(!workdir.join("screenshot.partial").exists());

    let windows = call_tool(&mut client, 8, "window_list", json!({}));
    std::fs::write(proof.join("windows.json"), windows.to_string()).expect("window evidence");
    assert!(windows["error"].is_null(), "{windows}");
    assert!(
        windows["result"]["structuredContent"]["windows"]
            .as_array()
            .expect("window list")
            .iter()
            .any(|window| window["title"]
                .as_str()
                .is_some_and(|title| title.contains("kwin319"))),
        "{windows}"
    );
    for id in 9..11 {
        let inline = call_tool(&mut client, id, "screenshot", json!({"inline":true}));
        let metadata = screenshot_meta(&inline);
        assert!(metadata["path"].is_null(), "{metadata}");
        assert!(
            metadata["write_error"]
                .as_str()
                .is_some_and(|error| error.contains("No space left on device")),
            "{metadata}"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(inline_png(&inline))
            .expect("fallback PNG");
        let mut decoder = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .expect("fallback PNG header");
        let mut pixels = vec![0; decoder.output_buffer_size()];
        let info = decoder
            .next_frame(&mut pixels)
            .expect("complete fallback PNG");
        assert!(info.width > 0 && info.height > 0);
        std::fs::write(proof.join("at-capacity.png"), bytes).expect("fallback screenshot");
        std::fs::write(proof.join("inline.json"), metadata.to_string()).expect("fallback metadata");
        assert!(!screenshot.exists());
        assert!(!workdir.join("screenshot.partial").exists());
    }
    let disk = client
        .home
        .join(".cache/kwin-mcp")
        .join(workdir.file_name().expect("workdir name"));
    assert_eq!(
        std::fs::read_dir(disk.join("kwin-scripts"))
            .expect("disk-backed scripts")
            .count(),
        0,
        "KWin scripts leaked after inspection"
    );
    drop(file);
    drop(fill);
    let recovered = call_tool(&mut client, 11, "screenshot", json!({}));
    assert!(
        recovered["error"].is_null() && screenshot.exists(),
        "{recovered}"
    );
    std::fs::copy(&screenshot, proof.join("recovered.png")).expect("recovered screenshot");
    let stopped = call_tool(&mut client, 12, "session_stop", json!({}));
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(!workdir.exists(), "workdir was not cleaned");
    assert!(!disk.exists(), "disk twin was not cleaned");
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, konsole, sqlite3, input devices, and a live GPU session"]
fn browser_cookies_continue_across_sessions_without_host_writes() {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start_with_cookie_test_bus();
    let profile = client.home.join(".config/google-chrome/Profile 2");
    std::fs::create_dir_all(&profile).expect("profile directory");
    let database = profile.join("Cookies");
    let host = rusqlite::Connection::open(&database).expect("host cookies");
    host.execute_batch(
        "PRAGMA locking_mode=EXCLUSIVE; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE cookies(host_key TEXT, name TEXT, path TEXT, last_update_utc INTEGER,
         encrypted_value BLOB, UNIQUE(host_key,name,path));
         INSERT INTO cookies VALUES('.example.test','session','/',10,x'0010');
         INSERT INTO cookies VALUES('.example.test','host','/',10,x'0010');",
    )
    .expect("seed cookies");
    let wal = profile.join("Cookies-wal");
    let source_database = std::fs::read(&database).expect("host database bytes");
    let source_wal = std::fs::read(&wal).expect("host WAL bytes");
    assert!(
        !profile.join("Cookies-shm").exists(),
        "EXCLUSIVE fixture unexpectedly uses shared memory"
    );
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let first_workdir = workdir(&started);
    let created = client.home.join(".config/google-chrome/Default/Cookies");
    let sql = "PRAGMA journal_mode=WAL; UPDATE cookies SET last_update_utc=20, encrypted_value=x'0020'; INSERT INTO cookies VALUES('.example.test','new','/',40,x'0040');";
    let new_sql = "CREATE TABLE cookies(host_key TEXT, name TEXT, path TEXT, last_update_utc INTEGER, encrypted_value BLOB, UNIQUE(host_key,name,path)); INSERT INTO cookies VALUES('.example.test','session','/',99,x'0099');";
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "sqlite3 {} {} && mkdir -p {} && sqlite3 {} {}; konsole",
            shlex::try_quote(&database.display().to_string()).expect("database quote"),
            shlex::try_quote(sql).expect("SQL quote"),
            shlex::try_quote(&created.parent().expect("profile parent").display().to_string()).expect("profile quote"),
            shlex::try_quote(&created.display().to_string()).expect("new database quote"),
            shlex::try_quote(new_sql).expect("new SQL quote")
        )}),
    );
    assert!(
        !launched["result"]["isError"].as_bool().unwrap_or(false),
        "{launched}"
    );
    save_session_image(
        &mut client,
        100,
        started["result"]["structuredContent"]["session_id"]
            .as_str()
            .expect("session ID"),
        "cookies-first-session.png",
    );
    let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(!first_workdir.exists(), "first session was not cleaned");
    let saved = client
        .home
        .join(".local/state/kwin-mcp/browser-cookies/.config/google-chrome/Profile 2/Cookies");
    assert_eq!(
        std::fs::metadata(&saved)
            .expect("saved jar")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let persisted = rusqlite::Connection::open(&saved).expect("saved cookies");
    let count: i64 = persisted
        .query_row("SELECT count(*) FROM cookies", [], |row| row.get(0))
        .expect("persisted cookie count");
    assert_eq!(count, 3);
    drop(persisted);
    let original: i64 = host
        .query_row(
            "SELECT last_update_utc FROM cookies WHERE name='session'",
            [],
            |row| row.get(0),
        )
        .expect("host timestamp");
    assert_eq!(original, 10, "session changed the host profile");
    assert_eq!(
        std::fs::read(&database).expect("host database after session"),
        source_database
    );
    assert_eq!(
        std::fs::read(&wal).expect("host WAL after session"),
        source_wal
    );
    assert!(
        !profile.join("Cookies-shm").exists(),
        "session opened host shared memory"
    );
    assert!(!created.exists(), "session created a host profile");
    host.execute(
        "UPDATE cookies SET last_update_utc=30,encrypted_value=x'0030' WHERE name='host'",
        [],
    )
    .expect("real browser refresh");
    let started = call_tool(
        &mut client,
        5,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let second_workdir = workdir(&started);
    let exported = client.home.join("merged-cookies.sqlite");
    let result = call_tool(
        &mut client,
        6,
        "export_file",
        json!({
            "session_path":database,"host_path":exported
        }),
    );
    assert!(
        !result["result"]["isError"].as_bool().unwrap_or(false),
        "{result}"
    );
    let merged = rusqlite::Connection::open(exported).expect("merged jar");
    let rows = merged
        .prepare("SELECT name,last_update_utc,hex(encrypted_value) FROM cookies ORDER BY name")
        .expect("merged query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("merged rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("merged values");
    assert_eq!(
        rows,
        vec![
            ("host".to_owned(), 30, "0030".to_owned()),
            ("new".to_owned(), 40, "0040".to_owned()),
            ("session".to_owned(), 20, "0020".to_owned())
        ]
    );
    let new_export = client.home.join("new-profile-cookies.sqlite");
    let result = call_tool(
        &mut client,
        101,
        "export_file",
        json!({
            "session_path":created,"host_path":new_export
        }),
    );
    assert!(
        !result["result"]["isError"].as_bool().unwrap_or(false),
        "{result}"
    );
    let restored = rusqlite::Connection::open(new_export).expect("new profile jar");
    let updated: i64 = restored
        .query_row(
            "SELECT last_update_utc FROM cookies WHERE name='session'",
            [],
            |row| row.get(0),
        )
        .expect("new profile cookie");
    assert_eq!(updated, 99, "profiles shared a cookie jar");
    let stopped = call_tool(&mut client, 7, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(!second_workdir.exists(), "second session was not cleaned");

    let started = call_tool(
        &mut client,
        8,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let third_workdir = workdir(&started);
    let migrate_schema = "ALTER TABLE cookies ADD COLUMN source_scheme INTEGER NOT NULL DEFAULT 0; UPDATE cookies SET source_scheme=1;";
    let migrated = call_tool(
        &mut client,
        9,
        "launch_app",
        json!({"command":format!(
            "sqlite3 {} {}; konsole",
            shlex::try_quote(&database.display().to_string()).expect("database quote"),
            shlex::try_quote(migrate_schema).expect("migration SQL quote")
        )}),
    );
    assert!(
        !migrated["result"]["isError"].as_bool().unwrap_or(false),
        "{migrated}"
    );
    let stopped = call_tool(&mut client, 10, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(
        !third_workdir.exists(),
        "schema migration session was not cleaned"
    );
    let persisted = rusqlite::Connection::open(&saved).expect("schema-updated saved jar");
    let columns = persisted
        .prepare("PRAGMA table_info(cookies)")
        .expect("schema query")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("schema columns")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("schema names");
    assert!(columns.iter().any(|column| column == "source_scheme"));
    let migrated_value: i64 = persisted
        .query_row(
            "SELECT source_scheme FROM cookies WHERE name='session'",
            [],
            |row| row.get(0),
        )
        .expect("migrated session cookie");
    assert_eq!(migrated_value, 1);
    drop(persisted);

    let started = call_tool(
        &mut client,
        11,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let startup_cookie_errors = started["result"]["structuredContent"]["cookie_errors"]
        .as_array()
        .expect("session_start cookie errors");
    assert!(
        startup_cookie_errors.iter().any(|error| error
            .as_str()
            .is_some_and(|message| message.contains("cookie schemas differ"))),
        "schema mismatch was not returned from session_start: {started}"
    );
    let fourth_workdir = workdir(&started);
    let stopped = call_tool(&mut client, 12, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(
        !fourth_workdir.exists(),
        "schema fallback session was not cleaned"
    );

    host.execute(
        "UPDATE cookies SET last_update_utc=50,encrypted_value=x'0050' WHERE name='host'",
        [],
    )
    .expect("host cookie update after schema fallback");
    let started = call_tool(
        &mut client,
        13,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let fifth_workdir = workdir(&started);
    let refreshed = client.home.join("refreshed-host-cookie.sqlite");
    let result = call_tool(
        &mut client,
        14,
        "export_file",
        json!({"session_path":database,"host_path":refreshed}),
    );
    assert!(
        !result["result"]["isError"].as_bool().unwrap_or(false),
        "{result}"
    );
    let refreshed_jar = rusqlite::Connection::open(refreshed).expect("refreshed session jar");
    let host_timestamp: i64 = refreshed_jar
        .query_row(
            "SELECT last_update_utc FROM cookies WHERE name='host'",
            [],
            |row| row.get(0),
        )
        .expect("refreshed host cookie");
    assert_eq!(
        host_timestamp, 50,
        "schema mismatch froze the old host cookie"
    );
    drop(refreshed_jar);
    let stopped = call_tool(&mut client, 15, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(!fifth_workdir.exists(), "refreshed session was not cleaned");

    let started = call_tool(
        &mut client,
        16,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let sixth_workdir = workdir(&started);
    let cookie_state = client.home.join(".local/state/kwin-mcp/browser-cookies");
    let displaced_state = client
        .home
        .join(".local/state/kwin-mcp/browser-cookies.saved");
    std::fs::rename(&cookie_state, &displaced_state).expect("displace private cookie state");
    std::fs::write(&cookie_state, b"occupied").expect("block cookie state directory");
    let stopped = call_tool(&mut client, 17, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(
        stopped["result"]["structuredContent"]["cookie_errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "cookie save failure was not returned from session_stop: {stopped}"
    );
    assert!(
        !sixth_workdir.exists(),
        "save-failure session was not cleaned"
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, konsole, input devices, and a live GPU session"]
fn browser_cookies_snapshot_concurrent_wal_commits_and_checkpoints() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    struct Writer {
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Writer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("WAL writer finished");
            }
        }
    }
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start_with_cookie_test_bus();
    let relative = ".config/google-chrome/Profile 2/Cookies";
    let database = client.home.join(relative);
    let saved = client
        .home
        .join(".local/state/kwin-mcp/browser-cookies")
        .join(relative);
    let schema = "CREATE TABLE cookies(host_key TEXT, name TEXT, path TEXT, last_update_utc INTEGER,
        encrypted_value BLOB, UNIQUE(host_key,name,path));
        WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2048)
        INSERT INTO cookies SELECT '.example.test',printf('cookie%d',x),'/',0,zeroblob(1024) FROM n;";
    for path in [&database, &saved] {
        std::fs::create_dir_all(path.parent().expect("jar parent")).expect("jar directory");
        rusqlite::Connection::open(path)
            .expect("jar")
            .execute_batch(schema)
            .expect("jar schema");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicI64::new(0));
    let writer_stop = stop.clone();
    let writer_generation = generation.clone();
    let writer_database = database.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let writer = Writer {
        stop,
        thread: Some(thread::spawn(move || {
            let host = rusqlite::Connection::open(writer_database).expect("writer connection");
            host.execute_batch("PRAGMA locking_mode=EXCLUSIVE; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").expect("exclusive WAL");
            ready_tx.send(()).expect("writer ready");
            let mut generation = 0i64;
            while !writer_stop.load(Ordering::SeqCst) {
                generation += 1;
                host.execute(
                    "UPDATE cookies SET last_update_utc=?1,encrypted_value=CAST(?1 AS BLOB)",
                    [generation],
                )
                .expect("atomic cookie generation");
                writer_generation.store(generation, Ordering::SeqCst);
                host.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                    .expect("concurrent checkpoint");
                thread::sleep(Duration::from_millis(5));
            }
        })),
    };
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("WAL fixture started");
    initialize(&mut client);
    let mut observed_live_generation = false;
    for cycle in 0..4u64 {
        let started = call_tool(
            &mut client,
            10 + cycle * 10,
            "session_start",
            json!({"width":800,"height":600}),
        );
        let session_workdir = workdir(&started);
        let exported = client.home.join(format!("wal-snapshot-{cycle}.sqlite"));
        let result = call_tool(
            &mut client,
            11 + cycle * 10,
            "export_file",
            json!({"session_path":database,"host_path":exported}),
        );
        assert!(
            !result["result"]["isError"].as_bool().unwrap_or(false),
            "{result}"
        );
        let snapshot = rusqlite::Connection::open(&exported).expect("snapshot");
        let check: String = snapshot
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("integrity check");
        assert_eq!(check, "ok");
        let (rows, versions, minimum, maximum): (i64, i64, i64, i64) = snapshot.query_row(
            "SELECT count(*),count(DISTINCT last_update_utc),min(last_update_utc),max(last_update_utc) FROM cookies",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).expect("snapshot generation");
        assert_eq!(rows, 2048);
        assert_eq!(versions, 1, "snapshot mixed concurrent transactions");
        assert_eq!(minimum, maximum);
        assert!(maximum <= generation.load(Ordering::SeqCst));
        observed_live_generation |= maximum > 0;
        if cycle == 0 {
            let launched = call_tool(&mut client, 12, "launch_app", json!({"command":"konsole"}));
            assert!(
                !launched["result"]["isError"].as_bool().unwrap_or(false),
                "{launched}"
            );
            save_session_image(
                &mut client,
                13,
                &session_handle(&started),
                "cookies-concurrent-wal.png",
            );
        }
        let stopped = call_tool(&mut client, 14 + cycle * 10, "session_stop", json!({}));
        assert!(
            !stopped["result"]["isError"].as_bool().unwrap_or(false),
            "{stopped}"
        );
        assert!(!session_workdir.exists());
    }
    drop(writer);
    assert!(
        observed_live_generation,
        "all snapshots fell back to saved state"
    );
    assert!(
        !database.with_file_name("Cookies-shm").exists(),
        "opened host shared memory"
    );
    let host_bytes = std::fs::read(&database).expect("host snapshot after writer stopped");
    let started = call_tool(
        &mut client,
        60,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let session_workdir = workdir(&started);
    call_tool(&mut client, 61, "session_stop", json!({}));
    assert!(!session_workdir.exists());
    assert_eq!(
        std::fs::read(&database).expect("unchanged host jar"),
        host_bytes
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, konsole, sqlite3, input devices, and a live GPU session"]
fn browser_cookie_deletion_survives_stale_sessions_and_expiry_is_pruned() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start_with_cookie_test_bus();
    let database = client.home.join(".config/google-chrome/Profile 2/Cookies");
    std::fs::create_dir_all(database.parent().expect("profile parent")).expect("profile");
    let host = rusqlite::Connection::open(&database).expect("host jar");
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_micros(),
    )
    .expect("cookie time")
        + 11_644_473_600_000_000;
    host.execute_batch(&format!("CREATE TABLE cookies(host_key TEXT,name TEXT,path TEXT,
        top_frame_site_key TEXT,source_scheme INTEGER,source_port INTEGER,last_update_utc INTEGER,
        expires_utc INTEGER,has_expires INTEGER,encrypted_value BLOB,
        UNIQUE(host_key,name,path,top_frame_site_key,source_scheme,source_port));
        INSERT INTO cookies VALUES('.example.test','removed','/','',2,443,10,0,0,x'0010');
        INSERT INTO cookies VALUES('.example.test','kept','/','',2,443,10,0,0,x'0010');
        INSERT INTO cookies VALUES('.example.test','kept','/','https://partition.test',2,443,10,0,0,x'0010');
        INSERT INTO cookies VALUES('.example.test','expired','/','',2,443,10,1,1,x'0010');
        INSERT INTO cookies VALUES('.example.test','zero-expired','/','',2,443,10,0,1,x'0010');
        INSERT INTO cookies VALUES('.example.test','future','/','',2,443,10,{},1,x'0010');
        INSERT INTO cookies VALUES('.example.test','session','/','',2,443,10,0,0,x'0010');", now + 3_600_000_000)).expect("seed jar");
    let unsupported = client.home.join(".config/google-chrome/Other/Cookies");
    std::fs::create_dir_all(unsupported.parent().expect("other parent")).expect("other directory");
    rusqlite::Connection::open(&unsupported)
        .expect("other SQLite")
        .execute_batch(
            "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('preserve');",
        )
        .expect("other schema");
    let original = std::fs::read(&database).expect("host bytes");
    let unsupported_original = std::fs::read(&unsupported).expect("other bytes");
    initialize(&mut client);
    let a = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let a_id = session_handle(&a);
    let b = call_tool(
        &mut client,
        3,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let b_id = session_handle(&b);
    let quoted_database = shlex::try_quote(&database.display().to_string())
        .expect("jar quote")
        .into_owned();
    let deleted = call_tool(
        &mut client,
        4,
        "launch_app",
        json!({"session_id":a_id,"command":format!("sqlite3 {quoted_database} \"DELETE FROM cookies WHERE name='removed';\" && konsole")}),
    );
    assert!(
        !deleted["result"]["isError"].as_bool().unwrap_or(false),
        "{deleted}"
    );
    let updated = call_tool(
        &mut client,
        5,
        "launch_app",
        json!({"session_id":b_id,"command":format!("sqlite3 {quoted_database} \"UPDATE cookies SET last_update_utc={now} WHERE name='kept' AND top_frame_site_key=''; INSERT INTO cookies VALUES('.example.test','new','/','',2,443,{now},0,0,x'0020');\" && konsole")}),
    );
    assert!(
        !updated["result"]["isError"].as_bool().unwrap_or(false),
        "{updated}"
    );
    save_session_image(&mut client, 6, &a_id, "cookies-deletion-session-a.png");
    // Both stops run at once: either lock acquisition order must preserve the
    // deletion and the other session's newer row/profile partition identities.
    for (id, session) in [(7, &a_id), (8, &b_id)] {
        client.send(
            id,
            "tools/call",
            json!({"name":"session_stop","arguments":{"session_id":session}}),
        );
    }
    for id in [7, 8] {
        let stopped = client.response(id, Duration::from_secs(45));
        assert!(
            stopped["error"].is_null() && !stopped["result"]["isError"].as_bool().unwrap_or(false),
            "{stopped}"
        );
    }
    client
        .sessions
        .retain(|session| session != &a_id && session != &b_id);
    assert!(!workdir(&a).exists() && !workdir(&b).exists());
    assert_eq!(std::fs::read(&database).expect("unchanged host"), original);
    assert_eq!(
        std::fs::read(&unsupported).expect("unchanged other"),
        unsupported_original
    );
    let saved = client
        .home
        .join(".local/state/kwin-mcp/browser-cookies/.config/google-chrome/Profile 2/Cookies");
    let private = rusqlite::Connection::open(&saved).expect("saved jar");
    let counts: (i64,i64,i64,i64,i64,i64) = private.query_row("SELECT count(*),sum(name='removed'),sum(name='expired'),sum(name='session'),sum(name='future'),sum(name='kept') FROM cookies", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).expect("persisted identities");
    assert_eq!(counts, (5, 0, 0, 1, 1, 2));
    let kept: i64 = private
        .query_row(
            "SELECT last_update_utc FROM cookies WHERE name='kept' AND top_frame_site_key=''",
            [],
            |row| row.get(0),
        )
        .expect("newer concurrent row");
    assert_eq!(kept, now);
    assert!(
        !client
            .home
            .join(".local/state/kwin-mcp/browser-cookies/.config/google-chrome/Other/Cookies")
            .exists()
    );
    drop(private);
    let c = call_tool(
        &mut client,
        9,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let c_id = session_handle(&c);
    let exported = client.home.join("after-deletion.sqlite");
    let result = call_tool(
        &mut client,
        10,
        "export_file",
        json!({"session_id":c_id,"session_path":database,"host_path":exported}),
    );
    assert!(
        !result["result"]["isError"].as_bool().unwrap_or(false),
        "{result}"
    );
    let restored = rusqlite::Connection::open(exported).expect("restored jar");
    let removed: i64 = restored
        .query_row(
            "SELECT count(*) FROM cookies WHERE name IN ('removed','expired')",
            [],
            |row| row.get(0),
        )
        .expect("deleted count");
    assert_eq!(
        removed, 0,
        "old host jar resurrected removed or expired cookie"
    );
    let hidden = call_tool(
        &mut client,
        11,
        "launch_app",
        json!({"session_id":c_id,"command":"test ! -e \"$HOME/.local/state/kwin-mcp/browser-cookies/.config/google-chrome/Profile 2/Cookies\" && konsole"}),
    );
    assert!(
        !hidden["result"]["isError"].as_bool().unwrap_or(false),
        "private store visible: {hidden}"
    );
    call_tool(&mut client, 12, "session_stop", json!({"session_id":c_id}));
    assert!(!workdir(&c).exists());
    let recreated = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_micros(),
    )
    .expect("cookie time")
        + 11_644_473_601_000_000;
    host.execute(
        "UPDATE cookies SET last_update_utc=? WHERE name='removed'",
        [recreated],
    )
    .expect("genuine newer host cookie");
    let d = call_tool(
        &mut client,
        13,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let d_id = session_handle(&d);
    let recreated_copy = client.home.join("recreated-cookie.sqlite");
    let result = call_tool(
        &mut client,
        14,
        "export_file",
        json!({"session_id":d_id,"session_path":database,"host_path":recreated_copy}),
    );
    assert!(
        !result["result"]["isError"].as_bool().unwrap_or(false),
        "{result}"
    );
    let recreated_jar = rusqlite::Connection::open(recreated_copy).expect("recreated jar");
    let version: i64 = recreated_jar
        .query_row(
            "SELECT last_update_utc FROM cookies WHERE name='removed'",
            [],
            |row| row.get(0),
        )
        .expect("new cookie survived tombstone");
    assert_eq!(version, recreated);
    call_tool(&mut client, 15, "session_stop", json!({"session_id":d_id}));
    assert!(!workdir(&d).exists());
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, konsole, python3, input devices, and a live GPU session"]
fn session_reads_host_live_sqlite_through_read_only_mounts() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    // The test HOME lives outside /tmp, which the sandbox replaces with tmpfs.
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(format!(".cache/kwin-mcp-e2e-sqlite-{}", std::process::id()));
    struct RemoveDir(PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveDir(home.clone());
    for directory in [
        ".codex",
        ".config",
        ".local/share",
        ".cache",
        ".local/state",
        ".kde",
    ] {
        std::fs::create_dir_all(home.join(directory)).expect("create test HOME");
    }
    let database = home.join(".codex/state.sqlite");
    let setup = rusqlite::Connection::open(&database).expect("create database");
    setup
        .execute_batch(
            "pragma journal_mode=wal; create table threads(id integer primary key, body blob);
             with recursive n(i) as (select 1 union all select i+1 from n where i < 2000)
             insert into threads(body) select randomblob(2048) from n;",
        )
        .expect("seed database");
    drop(setup);

    // Cookie jars must remain writable only in the session overlay.
    let cookie_database = home.join(".config/google-chrome/Default/Network/Cookies");
    std::fs::create_dir_all(cookie_database.parent().expect("cookie directory"))
        .expect("create cookie directory");
    let cookie_host =
        rusqlite::Connection::open(&cookie_database).expect("open host cookie fixture");
    cookie_host
        .execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE retained_cookie (value TEXT);
         INSERT INTO retained_cookie VALUES ('host-cookie');",
        )
        .expect("seed host cookie fixture");

    // A host app keeps writing its thread store for the whole session.
    struct LiveWriter {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Drop for LiveWriter {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(writer) = self.thread.take() {
                let _ = writer.join();
            }
        }
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (stop, database) = (stop.clone(), database.clone());
        thread::spawn(move || {
            let host = rusqlite::Connection::open(&database).expect("host connection");
            host.execute_batch("pragma journal_mode=wal; pragma wal_autocheckpoint=100;")
                .expect("host pragmas");
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                host.execute("insert into threads(body) values (randomblob(2048))", [])
                    .expect("host insert");
                host.execute(
                    "delete from threads where id = (select min(id) from threads)",
                    [],
                )
                .expect("host delete");
                thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let mut writer = LiveWriter {
        stop,
        thread: Some(writer),
    };
    thread::sleep(Duration::from_millis(300));

    let home_str = home.display().to_string();
    let mut client = RpcClient::start_with_env(&[
        ("HOME", home_str.as_str()),
        ("XDG_CONFIG_HOME", &format!("{home_str}/.config")),
        ("XDG_DATA_HOME", &format!("{home_str}/.local/share")),
        ("XDG_CACHE_HOME", &format!("{home_str}/.cache")),
        ("XDG_STATE_HOME", &format!("{home_str}/.local/state")),
        ("KDEHOME", &format!("{home_str}/.kde")),
    ]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let workdir = workdir(&started);
    let report = workdir.join("sqlite-check.txt");
    // Run this test executable inside the real session as a Rust SQLite probe.
    let probe = workdir.join("sqlite-probe");
    std::fs::copy(std::env::current_exe().expect("test executable"), &probe)
        .expect("copy session probe");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "KWIN_SQLITE_PROBE_DB={} KWIN_SQLITE_PROBE_COOKIES={} KWIN_SQLITE_PROBE_REPORT={} {} --ignored --exact session_sqlite_probe --nocapture; konsole",
            shlex::try_quote(&database.display().to_string()).expect("quote database"),
            shlex::try_quote(&cookie_database.display().to_string()).expect("quote cookies"),
            shlex::try_quote(&report.display().to_string()).expect("quote report"),
            shlex::try_quote(&probe.display().to_string()).expect("quote probe"),
        )}),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        if let Ok(text) = std::fs::read_to_string(&report)
            && text.ends_with('\n')
        {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "session SQLite check did not finish"
        );
        thread::sleep(Duration::from_millis(200));
    };
    writer
        .stop
        .store(true, std::sync::atomic::Ordering::Relaxed);
    writer
        .thread
        .take()
        .expect("host writer thread")
        .join()
        .expect("host writer");
    assert_eq!(
        result.trim(),
        "bad=0 readonly=1 private=1",
        "live database reads or read-only protection failed"
    );

    let host = rusqlite::Connection::open(&database).expect("reopen host database");
    let check: String = host
        .query_row("pragma integrity_check", [], |row| row.get(0))
        .expect("host integrity");
    assert_eq!(check, "ok");
    let leaked: i64 = host
        .query_row(
            "select count(*) from threads where body = x'73657373696f6e'",
            [],
            |row| row.get(0),
        )
        .expect("host query");
    assert_eq!(leaked, 0, "session write reached the host database");
    let host_cookies: i64 = cookie_host
        .query_row("SELECT count(*) FROM retained_cookie", [], |row| row.get(0))
        .expect("read host cookie fixture");
    assert_eq!(host_cookies, 1, "session cookie write reached the host jar");
    drop(cookie_host);
    call_tool(&mut client, 4, "session_stop", json!({}));
    client.stop_process();
}

#[test]
#[ignore = "helper launched by session_reads_host_live_sqlite_through_read_only_mounts"]
fn session_sqlite_probe() {
    let database = std::env::var("KWIN_SQLITE_PROBE_DB").expect("probe database");
    let cookies = std::env::var("KWIN_SQLITE_PROBE_COOKIES").expect("probe cookie jar");
    let report = std::env::var("KWIN_SQLITE_PROBE_REPORT").expect("probe report");
    for flags in [
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ] {
        let reader =
            rusqlite::Connection::open_with_flags(&database, flags).expect("session reader");
        for _ in 0..20 {
            let check: String = reader
                .query_row("pragma quick_check", [], |row| row.get(0))
                .expect("session quick check");
            assert_eq!(check, "ok");
            thread::sleep(Duration::from_millis(50));
        }
    }
    let writer = rusqlite::Connection::open(&database).expect("session writer");
    let error = writer
        .execute("insert into threads(body) values (x'73657373696f6e')", [])
        .expect_err("host database must reject session writes");
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly)
    );
    let jar = rusqlite::Connection::open(cookies).expect("session cookie jar");
    jar.execute("insert into retained_cookie values ('session-cookie')", [])
        .expect("private cookie write");
    let count: i64 = jar
        .query_row("select count(*) from retained_cookie", [], |row| row.get(0))
        .expect("private cookie count");
    assert_eq!(count, 2);
    std::fs::write(report, "bad=0 readonly=1 private=1\n").expect("probe report");
}

#[test]
#[ignore = "helper launched by session_follows_host_sqlite_wal_rotation"]
fn session_sqlite_rotation_probe() {
    let database = std::env::var("KWIN_SQLITE_PROBE_DB").expect("probe database");
    let report = std::env::var("KWIN_SQLITE_PROBE_REPORT").expect("probe report");
    let alias = std::fs::canonicalize(&database).expect("live database alias");
    let parent = alias.parent().expect("alias directory");
    assert!(
        alias.starts_with("/run/kwin-mcp-host-sqlite"),
        "missing read-only directory alias"
    );
    assert!(
        std::fs::read_dir(parent.join(".cache/kwin-mcp"))
            .expect("hidden disk root")
            .next()
            .is_none(),
        "alias exposed session storage"
    );
    assert!(
        std::fs::read_dir(parent.join(".cache/google-chrome"))
            .expect("hidden browser cache")
            .next()
            .is_none(),
        "data alias exposed the hidden browser cache"
    );
    for entry in std::fs::read_dir("/run/kwin-mcp-host-sqlite").expect("data aliases") {
        assert!(
            !entry
                .expect("data alias")
                .path()
                .join("hidden-cache.sqlite")
                .exists(),
            "a data alias below a hidden ancestor exposed its database"
        );
    }
    if let Ok(aliases) = std::fs::read_dir("/run/kwin-mcp-host-sockets") {
        for entry in aliases {
            assert!(
                !entry
                    .expect("socket alias")
                    .path()
                    .join("hidden-helper.sock")
                    .exists(),
                "a helper alias below a hidden ancestor exposed its socket"
            );
        }
    }
    use std::os::unix::fs::FileTypeExt;
    assert!(
        !std::fs::metadata(parent.join("host-graphical.sock"))
            .expect("masked host socket")
            .file_type()
            .is_socket(),
        "alias exposed a graphical host socket"
    );
    assert!(
        !std::fs::metadata(parent.join("host-helper.sock"))
            .expect("masked helper socket")
            .file_type()
            .is_socket(),
        "data alias bypassed helper socket policy"
    );
    for secret in ["bus", "keyring", "p11-kit"] {
        let path = parent.join("host-runtime-link").join(secret);
        if let Ok(metadata) = std::fs::metadata(&path) {
            assert!(
                !metadata.file_type().is_socket(),
                "alias exposes host runtime secret {secret}"
            );
            if metadata.is_dir() {
                assert!(
                    std::fs::read_dir(path)
                        .expect("runtime secret directory")
                        .next()
                        .is_none(),
                    "alias exposes host runtime secret directory {secret}"
                );
            }
        }
    }
    let reader =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("session reader after WAL rotation");
    for _ in 0..20 {
        let check: String = reader
            .query_row("pragma quick_check", [], |row| row.get(0))
            .expect("rotated database quick check under concurrent writes");
        assert_eq!(check, "ok");
        thread::sleep(Duration::from_millis(10));
    }
    let marker: i64 = reader
        .query_row("select max(marker) from generation", [], |row| row.get(0))
        .expect("latest host generation");
    drop(reader);
    std::fs::write(report, format!("{marker}\n")).expect("rotation report");
}

#[test]
#[ignore = "host graphical socket fixture for the live SQLite alias regression"]
fn host_graphical_socket_fixture() {
    use std::io::Read;
    let path = std::env::var_os("KWIN_GRAPHICAL_SOCKET_FIXTURE").expect("socket fixture path");
    let _listener =
        std::os::unix::net::UnixListener::bind(path).expect("graphical fixture listener");
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .expect("fixture lifetime");
}

#[test]
#[ignore = "helper launched by session_start_bounds_large_ancestor_copies_with_a_nested_mount"]
fn session_graphical_home_socket_probe() {
    let socket = std::env::var("KWIN_GRAPHICAL_HOME_SOCKET").expect("graphical socket path");
    let report = std::env::var("KWIN_GRAPHICAL_HOME_REPORT").expect("graphical socket report");
    assert!(
        std::os::unix::net::UnixStream::connect(&socket).is_err(),
        "split HOME exposed a graphical host socket through its ordinary path"
    );
    std::fs::write(report, "GRAPHICAL_SOCKET_BLOCKED\n").expect("graphical socket report");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, and a live GPU session"]
fn session_follows_host_sqlite_wal_rotation() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    let directory = client.home.clone();
    let database = directory.join("rotation.sqlite");
    let hidden_cache = directory.join(".cache/google-chrome/nested");
    let cookie_state = directory.join(".local/state/kwin-mcp/browser-cookies");
    assert!(
        !cookie_state.exists(),
        "cookie state must be absent beneath the HOME alias fixture"
    );
    std::fs::create_dir_all(&hidden_cache).expect("nested browser cache");
    let cache_database = rusqlite::Connection::open(hidden_cache.join("hidden-cache.sqlite"))
        .expect("hidden cache fixture database");
    cache_database.execute_batch("pragma journal_mode=wal; create table private_cache(value); insert into private_cache values('synthetic');")
        .expect("live nested cache fixture");
    let _hidden_helper =
        std::os::unix::net::UnixListener::bind(hidden_cache.join("hidden-helper.sock"))
            .expect("hidden helper socket fixture");
    let _helper_socket = std::os::unix::net::UnixListener::bind(directory.join("host-helper.sock"))
        .expect("host helper socket fixture");
    struct GraphicalFixture(Child);
    impl Drop for GraphicalFixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let socket = directory.join("host-graphical.sock");
    let _graphical = GraphicalFixture(
        Command::new(std::env::current_exe().expect("fixture executable"))
            .args(["--ignored", "--exact", "host_graphical_socket_fixture"])
            .env("KWIN_GRAPHICAL_SOCKET_FIXTURE", &socket)
            // Only this owned helper is marked graphical; no host connection is made.
            .env("WAYLAND_DISPLAY", "wayland-0")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("start graphical fixture"),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "graphical fixture did not start");
        thread::sleep(Duration::from_millis(20));
    }
    std::os::unix::fs::symlink(host_runtime_dir(), directory.join("host-runtime-link"))
        .expect("runtime symlink fixture");
    let mut host = rusqlite::Connection::open(&database).expect("host database");
    host.execute_batch("pragma journal_mode=wal; create table generation(marker integer); insert into generation values(0); create table churn(body blob);")
        .expect("seed generation");
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let runtime = workdir(&started);
    let probe = runtime.join("rotation-probe");
    std::fs::copy(std::env::current_exe().expect("test executable"), &probe).expect("copy probe");
    let mut previous_wal = 0;
    use std::os::unix::fs::MetadataExt;
    for generation in 1..=3 {
        host.execute_batch("pragma wal_checkpoint(TRUNCATE)")
            .expect("checkpoint");
        drop(host);
        assert!(
            !database.with_file_name("rotation.sqlite-wal").exists(),
            "host WAL was not unlinked"
        );
        host = rusqlite::Connection::open(&database).expect("reopen host database");
        host.execute(
            "insert into generation values(?1)",
            [i64::try_from(generation).expect("SQLite generation")],
        )
        .expect("new WAL generation");
        let wal_inode = std::fs::metadata(database.with_file_name("rotation.sqlite-wal"))
            .expect("new WAL")
            .ino();
        eprintln!(
            "WAL generation {generation}: previous inode {previous_wal}, current {wal_inode}"
        );
        previous_wal = wal_inode;
        struct Writer {
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
            thread: Option<thread::JoinHandle<()>>,
        }
        impl Drop for Writer {
            fn drop(&mut self) {
                self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(writer) = self.thread.take() {
                    let _ = writer.join();
                }
            }
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = {
            let (stop, writes, database) = (stop.clone(), writes.clone(), database.clone());
            thread::spawn(move || {
                let connection =
                    rusqlite::Connection::open(database).expect("concurrent host writer");
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    connection.execute_batch("insert into churn values(randomblob(128)); delete from churn where rowid < (select max(rowid)-32 from churn);")
                        .expect("concurrent host mutation");
                    writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let mut writer = Writer {
            stop,
            thread: Some(writer),
        };
        let report = runtime.join(format!("generation-{generation}.txt"));
        call_tool(
            &mut client,
            3 + generation,
            "launch_app",
            json!({"command":format!(
                "KWIN_SQLITE_PROBE_DB={} KWIN_SQLITE_PROBE_REPORT={} {} --ignored --exact session_sqlite_rotation_probe --nocapture; konsole",
                shlex::try_quote(&database.display().to_string()).expect("quote database"),
                shlex::try_quote(&report.display().to_string()).expect("quote report"),
                shlex::try_quote(&probe.display().to_string()).expect("quote probe"),
            )}),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !report.exists() {
            assert!(
                Instant::now() < deadline,
                "session failed after WAL rotation {generation}"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            std::fs::read_to_string(report).expect("generation report"),
            format!("{generation}\n")
        );
        writer
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        writer
            .thread
            .take()
            .expect("host writer thread")
            .join()
            .expect("concurrent host writer completed");
        assert!(
            writes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "host writer never ran during generation {generation}"
        );
    }
    drop(cache_database);
    call_tool(&mut client, 10, "session_stop", json!({}));
    assert!(!runtime.exists(), "session workdir remains");
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, konsole, input devices, and a live GPU session"]
fn export_file_hands_session_files_to_the_host_and_verifies_them() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(format!(".cache/kwin-mcp-e2e-export-{}", std::process::id()));
    struct RemoveDir(PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveDir(home.clone());
    for directory in [
        "Downloads",
        "out",
        ".config",
        ".local/share",
        ".cache",
        ".local/state",
        ".kde",
    ] {
        std::fs::create_dir_all(home.join(directory)).expect("create test HOME");
    }
    let home_str = home.display().to_string();
    let mut client = RpcClient::start_with_env(&[
        ("HOME", home_str.as_str()),
        ("XDG_CONFIG_HOME", &format!("{home_str}/.config")),
        ("XDG_DATA_HOME", &format!("{home_str}/.local/share")),
        ("XDG_CACHE_HOME", &format!("{home_str}/.cache")),
        ("XDG_STATE_HOME", &format!("{home_str}/.local/state")),
        ("KDEHOME", &format!("{home_str}/.kde")),
    ]);
    initialize(&mut client);
    call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    // A "download" in the session HOME, a file in the session's private /tmp,
    // and a download still in progress.
    let download = home.join("Downloads/report.bin");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "head -c 300000 /dev/urandom > '{d}'; printf scratch > /tmp/session-note.txt; : > '{d}.crdownload.part'; : > '{home}/Downloads/partial.zip.crdownload'; konsole",
            d = download.display(), home = home_str
        )}),
    );
    thread::sleep(Duration::from_millis(500));
    assert!(
        !download.exists(),
        "session write leaked to the host without export"
    );

    let not_ready = call_tool(
        &mut client,
        4,
        "wait_for_file",
        json!({
            "session_path": home.join("Downloads/not-yet-created.txt"),
            "timeout_seconds": 1,
            "stable_for_ms": 100,
        }),
    );
    assert_eq!(not_ready["error"]["data"]["status"], json!("not_ready"));
    assert_eq!(not_ready["error"]["data"]["reason"], json!("missing"));

    let delayed = home.join("Downloads/capture.txt");
    let producer = format!(
        "sleep 3; printf 'capture complete\\n' > {}; exec bash",
        shlex::try_quote(&delayed.display().to_string()).expect("quote delayed session path")
    );
    let command = format!(
        "konsole --separate -e bash -c {}",
        shlex::try_quote(&producer).expect("quote delayed producer command")
    );
    let launched = call_tool(&mut client, 5, "launch_app", json!({"command": command}));
    assert_eq!(
        launched["result"]["structuredContent"]["window_outcome"],
        json!("new"),
        "{launched}"
    );
    let ready = call_tool(
        &mut client,
        6,
        "wait_for_file",
        json!({
            "session_path": delayed,
            "timeout_seconds": 10,
            "stable_for_ms": 500,
        }),
    );
    let readiness = &ready["result"]["structuredContent"];
    assert_eq!(readiness["status"], json!("ready"), "{ready}");
    assert_eq!(readiness["size_bytes"], json!(17), "{ready}");
    let delayed_export = call_tool(
        &mut client,
        7,
        "export_file",
        json!({"session_path": delayed, "host_path": home.join("out")}),
    );
    assert_eq!(
        delayed_export["result"]["structuredContent"]["status"],
        json!("exported"),
        "{delayed_export}"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("out/capture.txt")).expect("exported capture"),
        "capture complete\n"
    );

    let exported = call_tool(
        &mut client,
        8,
        "export_file",
        json!({"session_path": download, "host_path": home.join("out")}),
    );
    let content = &exported["result"]["structuredContent"];
    assert_eq!(content["status"], json!("exported"), "{exported}");
    assert_eq!(content["bytes"], json!(300_000), "{exported}");
    let host_copy = home.join("out/report.bin");
    assert_eq!(
        std::fs::metadata(&host_copy)
            .map(|meta| meta.len())
            .unwrap_or(0),
        300_000
    );

    let refused = call_tool(
        &mut client,
        9,
        "export_file",
        json!({"session_path": download, "host_path": home.join("out")}),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already exists"),
        "{refused}"
    );
    let replaced = call_tool(
        &mut client,
        10,
        "export_file",
        json!({"session_path": download, "host_path": host_copy, "overwrite": true}),
    );
    assert_eq!(
        replaced["result"]["structuredContent"]["status"],
        json!("exported"),
        "{replaced}"
    );

    // Default destination: the same path on the host (the directory the user named).
    let default = call_tool(
        &mut client,
        11,
        "export_file",
        json!({"session_path": download}),
    );
    assert_eq!(
        default["result"]["structuredContent"]["host_path"],
        json!(download.display().to_string()),
        "{default}"
    );
    assert!(download.exists());

    let note = call_tool(
        &mut client,
        12,
        "export_file",
        json!({"session_path": "/tmp/session-note.txt", "host_path": home.join("out")}),
    );
    assert_eq!(
        note["result"]["structuredContent"]["status"],
        json!("exported"),
        "{note}"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("out/session-note.txt")).unwrap_or_default(),
        "scratch"
    );

    let partial = call_tool(
        &mut client,
        13,
        "export_file",
        json!({"session_path": home.join("Downloads/partial.zip"), "host_path": home.join("out")}),
    );
    assert!(
        partial["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("still downloading"),
        "{partial}"
    );
    assert!(!home.join("out/partial.zip").exists());

    // A directory must be rejected before touching even an explicitly
    // replaceable host file, with recovery guidance using the caller's path.
    let destination = home.join("out/directory-refusal.txt");
    std::fs::write(&destination, "preserve host contents").expect("host sentinel");
    let directory = home.join("Downloads");
    let refused_directory = call_tool(
        &mut client,
        14,
        "export_file",
        json!({"session_path": directory, "host_path": destination, "overwrite": true}),
    );
    let error = &refused_directory["error"];
    assert_eq!(error["data"]["status"], json!("not_exported"));
    assert_eq!(error["data"]["session_path"], json!(directory));
    let message = error["message"].as_str().expect("directory error message");
    assert!(
        message.contains("is a directory") && message.contains("archive"),
        "{message}"
    );
    assert!(
        message.contains("export each file individually"),
        "{message}"
    );
    assert!(
        !message.contains("/proc/"),
        "internal session path leaked: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(&destination).expect("untouched host sentinel"),
        "preserve host contents"
    );
    call_tool(&mut client, 15, "session_stop", json!({}));
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, kdialog, and input devices"]
fn import_file_refreshes_host_notes_without_restarting_the_session() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    let vault = client.home.join("vault");
    std::fs::create_dir_all(&vault).expect("private vault");
    let note = vault.join("event.md");
    let edited = vault.join("edited.md");
    let deleted = vault.join("deleted.md");
    for path in [&edited, &deleted] {
        std::fs::write(path, "original host note\n").expect("initial note");
    }
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let session = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session started")
        .to_owned();
    let setup = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({
            "session_id":session,
            "command":": > \"$HOME/vault/event.md\"; printf 'private edit\\n' > \"$HOME/vault/edited.md\"; rm \"$HOME/vault/deleted.md\"; ln -s /tmp \"$HOME/vault/tmp-link\""
        }),
    );
    assert!(setup["error"].is_null(), "{setup}");

    // Host additions and atomic replacements after session_start cannot replace
    // a blank upper file, a private edit, or a session whiteout on their own.
    let contents = "---\ntitle: Import refresh proof\ndate: 2026-10-15\nallDay: true\n---\nHost calendar note is visible.\n";
    for path in [&note, &edited, &deleted] {
        let pending = path.with_extension("pending");
        std::fs::write(&pending, contents).expect("host write");
        std::fs::rename(pending, path).expect("atomic host replacement");
    }
    let mut next_id = 4;
    let mut call = |name: &str, mut arguments: Value| {
        arguments["session_id"] = json!(session);
        next_id += 1;
        call_tool(&mut client, next_id, name, arguments)
    };
    let baseline = vault.join("baseline.txt");
    let exported = call(
        "export_file",
        json!({"session_path":note,"host_path":baseline}),
    );
    assert_eq!(
        exported["result"]["structuredContent"]["verified"], true,
        "{exported}"
    );
    assert_eq!(std::fs::read(&baseline).expect("blank baseline"), b"");
    let private = vault.join("private.txt");
    call(
        "export_file",
        json!({"session_path":edited,"host_path":private}),
    );
    assert_eq!(
        std::fs::read_to_string(&private).expect("private baseline"),
        "private edit\n"
    );
    let missing = call(
        "export_file",
        json!({"session_path":deleted,"host_path":vault.join("absent.txt")}),
    );
    assert!(
        !missing["error"].is_null(),
        "whiteout disappeared: {missing}"
    );

    for path in [&note, &edited] {
        let refused = call("import_file", json!({"host_path":path}));
        assert_eq!(
            refused["error"]["data"]["status"], "not_imported",
            "{refused}"
        );
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("overwrite=true"),
            "{refused}"
        );
    }
    for (index, path) in [&note, &edited, &deleted].into_iter().enumerate() {
        let imported = call("import_file", json!({"host_path":path,"overwrite":true}));
        let result = &imported["result"]["structuredContent"];
        assert_eq!(result["status"], "imported", "{imported}");
        assert_eq!(result["session_path"], json!(path), "{imported}");
        assert_eq!(result["bytes"], contents.len(), "{imported}");
        assert_eq!(result["verified"], true, "{imported}");
        assert_eq!(result["application_reload_required"], true, "{imported}");
        // Read back through a separate MCP operation, not the import receipt.
        let actual = vault.join(format!("actual-{index}.md"));
        let exported = call(
            "export_file",
            json!({"session_path":path,"host_path":actual}),
        );
        assert_eq!(
            exported["result"]["structuredContent"]["verified"], true,
            "{exported}"
        );
        assert_eq!(
            std::fs::read_to_string(actual).expect("session bytes"),
            contents
        );
        assert_eq!(
            std::fs::read_to_string(path).expect("host preserved"),
            contents
        );
    }
    // Absolute parent symlinks must resolve into the session's private /tmp.
    let host_sentinel =
        std::env::temp_dir().join(format!("kwin-mcp-import-{}", std::process::id()));
    std::fs::write(&host_sentinel, "host sentinel").expect("host sentinel");
    let link = vault.join("tmp-link");
    let linked = call(
        "import_file",
        json!({"host_path":note,"session_path":link.join(host_sentinel.file_name().expect("sentinel name"))}),
    );
    assert_eq!(
        linked["result"]["structuredContent"]["status"], "imported",
        "{linked}"
    );
    assert_eq!(
        std::fs::read_to_string(&host_sentinel).expect("sentinel unchanged"),
        "host sentinel"
    );
    std::fs::remove_file(&host_sentinel).expect("remove host sentinel");
    let linked_copy = vault.join("linked-copy.md");
    let exported = call(
        "export_file",
        json!({"session_path":format!("/tmp/{}",host_sentinel.file_name().expect("sentinel name").to_string_lossy()),"host_path":linked_copy}),
    );
    assert_eq!(
        exported["result"]["structuredContent"]["verified"], true,
        "{exported}"
    );
    assert_eq!(
        std::fs::read_to_string(linked_copy).expect("symlink destination"),
        contents
    );

    let fifo = vault.join("host.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).expect("host FIFO fixture");
    for arguments in [
        json!({"host_path":note,"session_path":"relative.md"}),
        json!({"host_path":vault}),
        json!({"host_path":fifo,"session_path":note,"overwrite":true}),
        json!({"host_path":vault.join("missing.md"),"session_path":note,"overwrite":true}),
        json!({"host_path":note,"session_path":"/tmp/missing-import-parent/note.md"}),
        json!({"host_path":note,"session_path":format!("/etc/kwin-mcp-import-{}.md",std::process::id())}),
        json!({"host_path":note,"session_path":"/proc/self/fd/1/note.md"}),
    ] {
        let refused = call("import_file", arguments);
        assert!(
            !refused["error"].is_null(),
            "unsafe/invalid import accepted: {refused}"
        );
    }
    let shown = call(
        "launch_app",
        json!({"command":"kdialog --title 'Refreshed host note' --textbox \"$HOME/vault/event.md\" 700 400"}),
    );
    assert!(shown["error"].is_null(), "{shown}");
    let capture = call("screenshot", json!({}));
    assert!(
        capture["result"]["structuredContent"]["window"]["title"]
            .as_str()
            .unwrap_or_default()
            .contains("Refreshed host note"),
        "{capture}"
    );
    if let Some(directory) = std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR") {
        std::fs::copy(
            capture["result"]["structuredContent"]["path"]
                .as_str()
                .expect("capture path"),
            PathBuf::from(directory).join("import-refreshed-note.png"),
        )
        .expect("save capture evidence");
    }
    let stopped = call("session_stop", json!({}));
    assert!(stopped["error"].is_null(), "{stopped}");
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Chrome, input devices, and a live GPU session"]
fn chrome_file_chooser_attaches_host_and_session_files_with_the_documented_keys() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    // The session HOME must hold the files: outside it, the host root is
    // read-only in the sandbox. It lives outside /tmp, which the sandbox
    // replaces with its own tmpfs.
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(format!(".cache/kwin-mcp-e2e-upload-{}", std::process::id()));
    struct RemoveDir(PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveDir(home.clone());
    for sub in [
        "Uploads",
        ".config",
        ".local/share",
        ".cache",
        ".local/state",
        ".kde",
    ] {
        std::fs::create_dir_all(home.join(sub)).expect("create test HOME");
    }
    let directory = home.join("Uploads");
    std::fs::write(directory.join("host-created.pdf"), b"%PDF-1.4 host\n").expect("host file");
    let home_str = home.display().to_string();
    let mut client = RpcClient::start_with_env(&[
        ("HOME", home_str.as_str()),
        ("XDG_CONFIG_HOME", &format!("{home_str}/.config")),
        ("XDG_DATA_HOME", &format!("{home_str}/.local/share")),
        ("XDG_CACHE_HOME", &format!("{home_str}/.cache")),
        ("XDG_STATE_HOME", &format!("{home_str}/.local/state")),
        ("KDEHOME", &format!("{home_str}/.kde")),
    ]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1280,"height":800}),
    );
    let workdir = workdir(&started);
    let page = workdir.join("upload.html");
    std::fs::write(
        &page,
        "<html><title>F:none</title><body><input type=file id=f style='font-size:40px'><script>\
         f.onchange=()=>document.title='F:'+(f.files[0]?f.files[0].name+':'+f.files[0].size:'none');\
         f.oncancel=()=>document.title='F:cancelled'</script></body></html>",
    )
    .expect("write upload page");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "printf session-created-data > '{}'; google-chrome-stable --no-first-run --new-window 'file://{}'",
            directory.join("session-created.pdf").display(),
            page.display()
        )}),
    );
    let mut id = 4;
    let deadline = Instant::now() + Duration::from_secs(15);
    while window_title(&mut client, id, "F:none").is_none() {
        id += 1;
        assert!(Instant::now() < deadline, "upload page did not load");
        thread::sleep(Duration::from_millis(300));
    }
    assert!(
        !directory.join("session-created.pdf").exists(),
        "session file leaked to the host"
    );
    for (file, bytes) in [("host-created.pdf", 14), ("session-created.pdf", 20)] {
        id += 1;
        call_tool(&mut client, id, "keyboard_key", json!({"key":"F5"}));
        thread::sleep(Duration::from_millis(1500));
        let found = call_tool(
            &mut client,
            id + 1,
            "find_ui_elements",
            json!({"query":"Choose File"}),
        );
        id += 1;
        assert!(
            found["result"]["structuredContent"]["matches"].as_u64() >= Some(1),
            "{found}"
        );
        id += 1;
        call_tool(&mut client, id, "mouse_click", json!({"x":100,"y":120}));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            id += 1;
            if window_title(&mut client, id, "Open File").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "file chooser did not open");
            thread::sleep(Duration::from_millis(200));
        }
        for (tool, arguments) in [
            ("keyboard_key", json!({"key":"ctrl+l"})),
            (
                "keyboard_type",
                json!({"text": directory.join(file).display().to_string()}),
            ),
            ("keyboard_key", json!({"key":"alt+o"})),
        ] {
            id += 1;
            call_tool(&mut client, id, tool, arguments);
            thread::sleep(Duration::from_millis(300));
        }
        let expected = format!("F:{file}:{bytes}");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            id += 1;
            let title = window_title(&mut client, id, "F:").unwrap_or_default();
            if title.starts_with(&expected) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{file}: page shows {title}, expected {expected}"
            );
            thread::sleep(Duration::from_millis(200));
        }
    }
    id += 1;
    call_tool(&mut client, id, "session_stop", json!({}));
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, /dev/fuse, sshfs, sftp-server, konsole, and a live GPU session"]
fn fuse_mounts_work_in_the_session_without_giving_apps_capabilities() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let sftp_server = [
        "/usr/lib/ssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/lib/openssh/sftp-server",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).exists());
    let (Some(sftp_server), true) = (sftp_server, std::path::Path::new("/usr/bin/sshfs").exists())
    else {
        eprintln!("sshfs or sftp-server missing; skipping");
        return;
    };
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let workdir = workdir(&started);
    let bridge_exe = client
        .home
        .join(".cache/kwin-mcp")
        .join(workdir.file_name().expect("session workdir name"))
        .join("kwin-mcp-exe");
    assert!(
        bridge_exe.is_file(),
        "FUSE bridge image was not staged in the disk-backed session twin: {}",
        bridge_exe.display()
    );
    assert!(
        !workdir.join("kwin-mcp-exe").exists(),
        "FUSE bridge image consumed space in the /tmp workdir"
    );
    std::fs::create_dir_all(workdir.join("src")).expect("source dir");
    std::fs::write(workdir.join("src/hello.txt"), "over fuse\n").expect("source file");
    // sshfs over a local sftp-server: a real libfuse3 client going through
    // fusermount3, with no network involved.
    std::fs::write(
        workdir.join("fake-ssh"),
        format!("#!/bin/sh\nexec {sftp_server}\n"),
    )
    .expect("fake ssh");
    let report = workdir.join("fuse-report.txt");
    let script = format!(
        "set -u; w='{w}'; m=\"$HOME/fuse-mnt\"; mkdir -p \"$m\"; chmod +x \"$w/fake-ssh\"\n\
         exec > \"$w/fuse-report.part\" 2>&1\n\
         sshfs -o ssh_command=\"$w/fake-ssh\" \"x:$w/src\" \"$m\" && echo \"read=$(cat \"$m/hello.txt\")\"\n\
         fusermount3 -u \"$m\" && echo unmounted\n\
         fusermount3 -u \"$HOME\" 2>/dev/null || echo refused-unmount-home\n\
         sshfs -o ssh_command=\"$w/fake-ssh\" \"x:$w/src\" /usr/share 2>/dev/null || echo refused-system-mountpoint\n\
         echo \"caps=$(awk '/^CapEff/{{print $2}}' /proc/self/status)\"\n\
         touch /usr/kwin-mcp-probe 2>/dev/null || echo root-read-only\n\
         mv \"$w/fuse-report.part\" \"$w/fuse-report.txt\"\n",
        w = workdir.display()
    );
    std::fs::write(workdir.join("fuse-check.sh"), script).expect("write fuse script");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!("bash '{}'; konsole", workdir.join("fuse-check.sh").display())}),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        if let Ok(text) = std::fs::read_to_string(&report) {
            break text;
        }
        assert!(Instant::now() < deadline, "FUSE check did not finish");
        thread::sleep(Duration::from_millis(200));
    };
    for expected in [
        "read=over fuse",
        "unmounted",
        "refused-unmount-home",
        "refused-system-mountpoint",
        "caps=0000000000000000",
        "root-read-only",
    ] {
        assert!(
            result.lines().any(|line| line == expected),
            "missing {expected:?} in:\n{result}"
        );
    }
    call_tool(&mut client, 4, "session_stop", json!({}));
    assert!(
        !bridge_exe.exists(),
        "session_stop retained its disk-backed FUSE bridge image"
    );
    client.stop_process();
}

fn session_wallet<B, R>(connection: &zbus::blocking::Connection, method: &str, body: &B) -> R
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    connection
        .call_method(
            Some("org.kde.kwalletd6"),
            "/modules/kwalletd6",
            Some("org.kde.KWallet"),
            method,
            body,
        )
        .unwrap_or_else(|error| panic!("{method}: {error}"))
        .body()
        .deserialize()
        .unwrap_or_else(|error| panic!("{method} reply: {error}"))
}

fn host_kwallet(
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
) -> Option<zbus::Message> {
    let connection = zbus::blocking::Connection::session().ok()?;
    connection
        .call_method(
            Some("org.kde.kwalletd6"),
            "/modules/kwalletd6",
            Some("org.kde.KWallet"),
            method,
            body,
        )
        .ok()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, an unlocked host KWallet, and a live GPU session"]
fn session_kwallet_is_session_local_and_never_opens_host_handles() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let users = || -> Vec<String> {
        host_kwallet("users", &("kdewallet",))
            .and_then(|reply| reply.body().deserialize().ok())
            .unwrap_or_default()
    };
    let wallets = || -> Vec<String> {
        host_kwallet("wallets", &())
            .and_then(|reply| reply.body().deserialize().ok())
            .unwrap_or_default()
    };
    if !wallets().iter().any(|wallet| wallet == "kdewallet") {
        eprintln!("no host kdewallet; skipping");
        return;
    }
    let (users_before, wallets_before) = (users(), wallets());
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["wallet"]["mode"],
        json!("session-local copy"),
        "{started}"
    );
    let workdir = workdir(&started);
    // One connection on the bus session apps use, as a browser holds; the
    // service ties handles to the connection that opened them.
    let address = format!("unix:path={}", workdir.join("service_bus_socket").display());
    let wallet = zbus::blocking::connection::Builder::address(address.as_str())
        .expect("session wallet bus address")
        .build()
        .expect("connect to the session wallet bus");
    let kept: i32 = session_wallet(&wallet, "open", &("kdewallet", 0i64, "kwin-mcp-e2e-kept"));
    assert!(kept >= 0, "open kdewallet returned {kept}");
    let closed: i32 = session_wallet(&wallet, "open", &("kdewallet", 0i64, "kwin-mcp-e2e-closed"));
    assert_eq!(
        session_wallet::<_, i32>(&wallet, "close", &(closed, false, "kwin-mcp-e2e-closed")),
        0
    );
    assert_eq!(
        session_wallet::<_, i32>(
            &wallet,
            "open",
            &("kwin-mcp-e2e-missing-wallet", 0i64, "kwin-mcp-e2e-kept")
        ),
        -1
    );
    assert!(session_wallet::<_, bool>(
        &wallet,
        "createFolder",
        &(kept, "kwin-mcp-e2e", "kwin-mcp-e2e-kept")
    ));
    assert_eq!(
        session_wallet::<_, i32>(
            &wallet,
            "writePassword",
            &(kept, "kwin-mcp-e2e", "k", "v", "kwin-mcp-e2e-kept")
        ),
        0
    );
    assert_eq!(
        session_wallet::<_, String>(
            &wallet,
            "readPassword",
            &(kept, "kwin-mcp-e2e", "k", "kwin-mcp-e2e-kept")
        ),
        "v"
    );
    assert_eq!(
        session_wallet::<_, i32>(&wallet, "close", &("kdewallet", true)),
        -1
    );
    assert_eq!(
        users(),
        users_before,
        "the session opened host KWallet handles"
    );
    drop(wallet);
    call_tool(&mut client, 4, "session_stop", json!({}));
    client.stop_process();
    assert_eq!(users(), users_before, "session left host KWallet handles");
    assert_eq!(
        wallets(),
        wallets_before,
        "session changed the host wallet list"
    );
}

fn host_bus_peer_pids() -> Vec<u32> {
    let output = Command::new("busctl")
        .args([
            "--user",
            "--json=short",
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.Debug.Stats",
            "GetStats",
        ])
        .output()
        .expect("run busctl GetStats");
    assert!(
        output.status.success(),
        "GetStats failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stats: Value = serde_json::from_slice(&output.stdout).expect("GetStats JSON");
    stats
        .pointer("/data/0/org.bus1.DBus.Debug.Stats.PeerAccounting/data")
        .and_then(Value::as_array)
        .expect("dbus-broker peer accounting")
        .iter()
        .filter_map(|peer| peer.pointer("/1/ProcessID/data")?.as_u64())
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

fn host_bus_broker_fds() -> usize {
    let output = Command::new("busctl")
        .args([
            "--user",
            "--json=short",
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            "s",
            "org.freedesktop.DBus",
        ])
        .output()
        .expect("run busctl GetConnectionUnixProcessID");
    let reply: Value = serde_json::from_slice(&output.stdout).expect("broker pid JSON");
    let launcher = reply["data"][0].as_u64().expect("broker pid");
    let children = std::fs::read_to_string(format!("/proc/{launcher}/task/{launcher}/children"))
        .unwrap_or_default();
    let pid = children
        .split_whitespace()
        .find(|child| {
            std::fs::read_to_string(format!("/proc/{child}/comm"))
                .is_ok_and(|name| name.trim() == "dbus-broker")
        })
        .map_or_else(|| launcher.to_string(), str::to_owned);
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("read broker fds")
        .count()
}

fn ancestor_in(mut pid: u32, roots: &[u32]) -> Option<u32> {
    for _ in 0..64 {
        if roots.contains(&pid) {
            return Some(pid);
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        pid = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        if pid <= 1 {
            return None;
        }
    }
    None
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Chrome, dbus-broker, input devices, and a live GPU session"]
fn parallel_sessions_hold_no_host_user_bus_connections() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let count: usize = std::env::var("KWIN_MCP_E2E_SESSIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(6);
    let fds_before = host_bus_broker_fds();

    let starts: Vec<_> = (0..count)
        .map(|index| {
            thread::spawn(move || {
                let mut client = RpcClient::start();
                client.send(
                    1,
                    "initialize",
                    json!({
                        "protocolVersion":"2025-06-18",
                        "capabilities":{},
                        "clientInfo":{"name":"host-bus-e2e","version":"1"}
                    }),
                );
                let initialized = client.response(1, Duration::from_secs(10));
                assert!(initialized["result"].is_object(), "initialize failed: {initialized}");
                client.notify("notifications/initialized", json!({}));
                let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
                assert_eq!(started["result"]["structuredContent"]["status"], "started", "{started}");
                let dir = workdir(&started);
                let launched = call_tool(
                    &mut client,
                    3,
                    "launch_app",
                    json!({"command":format!(
                        "google-chrome-stable --no-first-run --user-data-dir=$XDG_RUNTIME_DIR/chrome-{index} about:blank"
                    )}),
                );
                assert!(
                    launched["result"]["structuredContent"]["window"]
                        .as_str()
                        .is_some_and(|window| window.starts_with('{')),
                    "Chrome did not open a window: {launched}"
                );
                (client, dir)
            })
        })
        .collect();
    let sessions: Vec<_> = starts
        .into_iter()
        .map(|start| start.join().expect("session thread"))
        .collect();
    let roots: Vec<u32> = sessions
        .iter()
        .map(|(client, _)| client.child.id())
        .collect();

    let held: Vec<(u32, u32)> = host_bus_peer_pids()
        .into_iter()
        .filter_map(|peer| ancestor_in(peer, &roots).map(|root| (root, peer)))
        .collect();
    assert!(
        held.is_empty(),
        "(server, peer) pairs on the host user bus: {held:?}"
    );
    let fds_running = host_bus_broker_fds();

    for (mut client, dir) in sessions {
        let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
        assert!(
            !stopped["result"]["isError"].as_bool().unwrap_or(false),
            "{stopped}"
        );
        assert!(!dir.exists(), "session_stop left {}", dir.display());
        client.stop_process();
    }
    let fds_after = host_bus_broker_fds();
    eprintln!(
        "host bus broker fds: before {fds_before}, {count} sessions running {fds_running}, after {fds_after}"
    );
    assert!(
        fds_running <= fds_before + count,
        "broker fds grew from {fds_before} to {fds_running} with {count} sessions"
    );
    assert!(
        fds_after <= fds_before + count,
        "broker fds {fds_after} after stop, {fds_before} before"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn session_start_binds_large_live_codex_state_without_copying_it() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    let codex_home = client.home.join(".codex");
    std::fs::create_dir(&codex_home).expect("create fixture directory");
    let database = codex_home.join("state_5.sqlite");
    let source = rusqlite::Connection::open(&database).expect("open host state fixture");
    source
        .execute_batch(
            "PRAGMA journal_mode=WAL;
         CREATE TABLE retained_state (value BLOB NOT NULL);
         INSERT INTO retained_state VALUES (zeroblob(67108864));",
        )
        .expect("write large live WAL fixture");
    initialize(&mut client);
    let timer = Instant::now();
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );
    let elapsed = timer.elapsed();
    let directory = workdir(&started);
    let upper = client
        .home
        .join(".cache/kwin-mcp")
        .join(directory.file_name().expect("workdir name"))
        .join("overlay-upper/.codex");
    assert!(
        !upper.join("state_5.sqlite").exists(),
        "startup copied the database"
    );
    let private_database = upper.join("state_5.sqlite");
    assert!(
        std::fs::symlink_metadata(&private_database)
            .expect("private database link")
            .file_type()
            .is_symlink(),
        "startup did not create the private live-database link"
    );
    assert!(
        std::fs::read_link(private_database)
            .expect("live database destination")
            .starts_with("/run/kwin-mcp-host-sqlite"),
        "database link did not resolve through the live read-only alias"
    );
    assert!(
        !upper.join("state_5.sqlite-wal").exists(),
        "startup copied the WAL"
    );
    let report = directory.join("database-mount.txt");
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "findmnt -n -o OPTIONS -T '{}' > '{}'; stat -Lc '%i' '{}' >> '{}'; konsole",
            database.display(), report.display(), database.display(), report.display()
        )}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let evidence = std::fs::read_to_string(&report).expect("database mount report");
    let mut lines = evidence.lines();
    assert!(
        lines
            .next()
            .expect("mount options")
            .split(',')
            .any(|option| option == "ro"),
        "{evidence}"
    );
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        lines
            .next()
            .expect("inode")
            .parse::<u64>()
            .expect("inode number"),
        std::fs::metadata(&database)
            .expect("host database metadata")
            .ino()
    );
    drop(source);
    let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
    assert!(!directory.exists(), "workdir remains after stop");
    client.stop_process();
    println!(
        "PASS: 64 MiB live WAL database shares the host inode read-only, no database/WAL copy, startup {:?}, stopped session",
        elapsed
    );
}

#[test]
#[ignore = "requires KDE, bubblewrap, Konsole, squashfuse, mksquashfs, input devices and a filtered test D-Bus proxy; set KWIN_MCP_TEST_TMPDIR to a short path outside the real HOME and /tmp"]
fn session_can_explicitly_share_live_codex_state() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let address = require_filtered_cookie_test_bus();
    let mut client = RpcClient::start_with_env(&[
        ("DBUS_SESSION_BUS_ADDRESS", address.as_str()),
        ("KWIN_MCP_E2E_COPY_BINARY", "1"),
    ]);
    assert!(
        !client
            .home
            .starts_with(PathBuf::from(std::env::var_os("HOME").expect("real HOME"))),
        "mount fixture must stay outside the real HOME"
    );
    let codex = client.home.join(".codex");
    std::fs::create_dir(&codex).expect("create Codex state directory");
    // Force the split overlay path responsible for oversized ancestor files.
    let portal = codex.join("portal");
    let source = client.home.join("mount-source");
    std::fs::create_dir(&portal).expect("mountpoint");
    std::fs::create_dir(&source).expect("mount source");
    std::fs::write(source.join("fixture"), "mounted").expect("mount fixture");
    let image = client.home.join("mount.squashfs");
    assert!(
        Command::new("mksquashfs")
            .arg(&source)
            .arg(&image)
            .args(["-noappend", "-processors", "1", "-quiet"])
            .stdout(Stdio::null())
            .status()
            .expect("create fixture image")
            .success()
    );
    struct FixtureMount(Child, PathBuf);
    impl Drop for FixtureMount {
        fn drop(&mut self) {
            let _ = Command::new("fusermount3").arg("-u").arg(&self.1).status();
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mount = FixtureMount(
        Command::new("squashfuse")
            .arg("-f")
            .arg(&image)
            .arg(&portal)
            .stdout(Stdio::null())
            .spawn()
            .expect("mount owned fixture"),
        portal.clone(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !procfs::process::Process::myself()
        .expect("self")
        .mountinfo()
        .expect("mount inventory")
        .0
        .iter()
        .any(|mount| mount.mount_point == portal)
    {
        assert!(Instant::now() < deadline, "fixture mount did not appear");
        thread::sleep(Duration::from_millis(20));
    }
    let socket_path = codex.join("host-app.sock");
    let _socket =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("host socket fixture");
    let oversized = codex.join("large-state.bin");
    std::fs::File::create(&oversized)
        .expect("inactive state fixture")
        .set_len(128 * 1024 * 1024)
        .expect("oversized sparse state");
    let database = codex.join("state_5.sqlite");
    let host = rusqlite::Connection::open(&database).expect("open host fixture");
    host.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE state (value TEXT);
         INSERT INTO state VALUES ('host');
         CREATE TABLE large (body BLOB);
         INSERT INTO large VALUES (zeroblob(67108865));",
    )
    .expect("seed oversized live state");
    std::fs::write(client.home.join("private.txt"), "host").expect("private fixture");
    initialize(&mut client);

    let alias = client.home.join("state-alias");
    std::os::unix::fs::symlink(&codex, &alias).expect("fixture symlink");
    for private in [
        ".cache/google-chrome",
        ".local/state/kwin-mcp/browser-cookies",
    ] {
        std::fs::create_dir_all(client.home.join(private)).expect("private state fixture");
    }
    for (index, paths) in [
        json!([client.home]),
        json!([client.home.join("missing")]),
        json!([database]),
        json!([alias]),
        json!([codex.join("..")]),
        json!([client.home.join(".config")]),
        json!([client.home.join(".cache")]),
        json!([client.home.join(".cache/google-chrome")]),
        json!([client.home.join(".local")]),
        json!([client.home.join(".local/state")]),
        json!([client.home.join(".local/state/kwin-mcp/browser-cookies")]),
        json!(["/var/tmp"]),
    ]
    .into_iter()
    .enumerate()
    {
        let rejected = call_tool(
            &mut client,
            100 + u64::try_from(index).expect("request ID"),
            "session_start",
            json!({"host_write_paths":paths}),
        );
        assert!(
            rejected["result"]["isError"] == true || !rejected["error"].is_null(),
            "{rejected}"
        );
        assert!(
            rejected.to_string().contains("host_write_paths"),
            "{rejected}"
        );
    }
    let conflicting = call_tool(
        &mut client,
        20,
        "session_start",
        json!({"host_write_paths":[codex], "writable_paths":[database]}),
    );
    assert!(
        conflicting.to_string().contains("conflicts"),
        "{conflicting}"
    );

    for (index, shared) in [false, true].into_iter().enumerate() {
        let request = 30 + 10 * u64::try_from(index).expect("request ID");
        let mut params = json!({"width":640,"height":480});
        if shared {
            params["host_write_paths"] = json!([codex]);
        }
        let started = call_tool(&mut client, request, "session_start", params);
        let directory = workdir(&started);
        if shared {
            let binary = client.binary_copy.as_ref().expect("private executable");
            let replacement = binary.with_extension("next");
            std::fs::copy(binary, &replacement).expect("stage replacement executable");
            std::fs::rename(&replacement, binary).expect("publish replacement executable");
            client.wait_for_stderr("reload: reattached 1 sessions", Duration::from_secs(30));
            client
                .wait_for_notification("notifications/tools/list_changed", Duration::from_secs(10));
        }
        let layers = client
            .home
            .join(".cache/kwin-mcp")
            .join(directory.file_name().expect("workdir name"));
        if shared {
            for name in ["state_5.sqlite", "state_5.sqlite-wal", "large-state.bin"] {
                assert!(
                    !layers.join("overlay-root/.codex").join(name).exists(),
                    "copied shared state: {name}"
                );
            }
        }
        let expected_paths = if shared { json!([codex]) } else { json!([]) };
        assert_eq!(
            started["result"]["structuredContent"]["host_write_paths"], expected_paths,
            "{started}"
        );
        let listed = call_tool(&mut client, request + 1, "session_list", json!({}));
        assert_eq!(
            listed["result"]["structuredContent"]["sessions"][0]["host_write_paths"],
            expected_paths,
            "{listed}"
        );
        if shared {
            assert!(
                !started["result"]["structuredContent"]["oversized_read_only_files"]
                    .as_array()
                    .expect("oversized report")
                    .contains(&json!(oversized)),
                "shared state reported read-only: {started}"
            );
        }
        let report = directory.join("live-state-report");
        let probe = directory.join("live-state-probe");
        std::fs::copy(std::env::current_exe().expect("test executable"), &probe)
            .expect("copy Rust probe");
        let quote = |path: &std::path::Path| {
            shlex::try_quote(path.to_str().expect("UTF-8 path"))
                .expect("quote path")
                .into_owned()
        };
        let private_file = client.home.join("private.txt");
        let launched = call_tool(
            &mut client,
            request + 2,
            "launch_app",
            json!({"command":format!(
            "KWIN_LIVE_STATE_DB={} KWIN_LIVE_STATE_PRIVATE={} KWIN_LIVE_STATE_REPORT={} KWIN_LIVE_STATE_SOCKET={} KWIN_LIVE_STATE_SHARED={} {} --ignored --exact session_live_state_probe --nocapture; konsole",
            quote(&database), quote(&private_file), quote(&report), quote(&socket_path), shared, quote(&probe),
            )}),
        );
        assert!(launched["error"].is_null(), "{launched}");
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::read_to_string(&report).ok().as_deref() != Some("PASS\n") {
            assert!(
                Instant::now() < deadline,
                "live state probe failed: {launched}"
            );
            thread::sleep(Duration::from_millis(100));
        }
        let count: i64 = host
            .query_row(
                "SELECT count(*) FROM state WHERE value='session'",
                [],
                |row| row.get(0),
            )
            .expect("host observes session writes");
        assert_eq!(count, i64::from(shared));
        if shared {
            use std::io::Read;
            let mut header = [0; 13];
            std::fs::File::open(&oversized)
                .expect("oversized host state")
                .read_exact(&mut header)
                .expect("host write evidence");
            assert_eq!(&header, b"session-state");
        }
        assert_eq!(
            std::fs::read_to_string(client.home.join("private.txt")).expect("host private file"),
            "host"
        );
        let stopped = call_tool(&mut client, request + 3, "session_stop", json!({}));
        assert!(
            !stopped["result"]["isError"].as_bool().unwrap_or(false),
            "{stopped}"
        );
        assert!(!directory.exists(), "session cleanup");
    }
    drop(host);
    let reopened = rusqlite::Connection::open(&database).expect("persistent state");
    assert_eq!(
        reopened
            .query_row(
                "SELECT count(*) FROM state WHERE value='session'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("persistent row"),
        1
    );
    assert_eq!(
        reopened
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("integrity"),
        "ok"
    );
    drop(reopened);
    drop(mount);
    client.stop_process();
}

#[test]
#[ignore = "helper launched by session_can_explicitly_share_live_codex_state"]
fn session_live_state_probe() {
    let database = std::env::var("KWIN_LIVE_STATE_DB").expect("database path");
    let shared = std::env::var("KWIN_LIVE_STATE_SHARED").expect("sharing mode") == "true";
    if shared {
        assert!(
            std::os::unix::net::UnixStream::connect(
                std::env::var("KWIN_LIVE_STATE_SOCKET").expect("socket path")
            )
            .is_err(),
            "shared state exposed a host app socket"
        );
    }
    let connection = rusqlite::Connection::open(&database).expect("open session state");
    let inserted = connection.execute("INSERT INTO state VALUES ('session')", []);
    assert_eq!(inserted.is_ok(), shared, "{inserted:?}");
    if shared {
        std::fs::OpenOptions::new()
            .write(true)
            .open(PathBuf::from(&database).with_file_name("large-state.bin"))
            .expect("open oversized live state writable")
            .write_all(b"session-state")
            .expect("write oversized live state");
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint live WAL");
        drop(connection);
        let connection = rusqlite::Connection::open(&database).expect("reopen live database");
        connection.execute_batch("CREATE TABLE recreated_sidecar (value TEXT); INSERT INTO recreated_sidecar VALUES ('live');").expect("write after checkpoint");
    }
    std::fs::write(
        std::env::var("KWIN_LIVE_STATE_PRIVATE").expect("private file"),
        "session",
    )
    .expect("private overlay write");
    std::fs::write(
        std::env::var("KWIN_LIVE_STATE_REPORT").expect("report"),
        "PASS\n",
    )
    .expect("probe report");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn application_binary_stderr_keeps_screenshots_and_shutdown_working() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );
    let directory = workdir(&started);
    let log = client
        .home
        .join(".cache/kwin-mcp")
        .join(directory.file_name().expect("workdir name"))
        .join("sandbox.log");
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({
            "command":"printf '\\377\\376\\n' >&2; kdialog --title binary-stderr-proof --msgbox binary-stderr-proof"
        }),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let captured = call_tool(&mut client, 4, "screenshot", json!({"inline":false}));
    assert!(captured["error"].is_null(), "{captured}");
    let capture = captured["result"]["structuredContent"]["path"]
        .as_str()
        .expect("screenshot path");
    assert!(std::fs::metadata(capture).expect("captured PNG").len() > 0);
    let diagnostics = std::fs::read(&log).expect("raw sandbox diagnostics");
    assert!(diagnostics.windows(3).any(|bytes| bytes == [255, 254, 10]));
    let stopped = call_tool(&mut client, 5, "session_stop", json!({}));
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
    assert!(!directory.exists(), "session workdir remains");
    assert!(!log.exists(), "sandbox log remains after stop");
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn launch_app_reports_commands_that_exit_without_a_window() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );

    let begun = Instant::now();
    let missing = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":"kwin-mcp-e2e-no-such-command"}),
    );
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert_eq!(
        missing["result"]["structuredContent"]["exit_status"], 127,
        "{missing}"
    );
    assert_eq!(
        missing["result"]["structuredContent"]["command_resolution"],
        "kwin-mcp-e2e-no-such-command -> not found in session PATH",
        "{missing}"
    );
    assert_eq!(
        missing["result"]["structuredContent"]["window_timeout_seconds"], 15,
        "{missing}"
    );
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "missing command took {:?}",
        begun.elapsed()
    );

    let failed = call_tool(&mut client, 4, "launch_app", json!({"command":"exit 3"}));
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(
        failed["result"]["structuredContent"]["exit_status"], 3,
        "{failed}"
    );
    assert_eq!(
        failed["result"]["structuredContent"]["command_resolution"], "exit -> shell builtin",
        "{failed}"
    );
    // The command's stderr comes back with its exit status.
    let noisy = call_tool(
        &mut client,
        7,
        "launch_app",
        json!({"command":"sh -c 'echo boom >&2; exit 3'"}),
    );
    assert_eq!(noisy["result"]["isError"], true, "{noisy}");
    assert_eq!(
        noisy["result"]["structuredContent"]["exit_status"], 3,
        "{noisy}"
    );
    assert!(
        noisy["result"]["structuredContent"]["command_resolution"]
            .as_str()
            .is_some_and(
                |resolution| resolution.starts_with("sh -> /") && resolution.ends_with("/sh")
            ),
        "{noisy}"
    );
    assert!(
        noisy["result"]["structuredContent"]["stderr_tail"]
            .as_array()
            .is_some_and(|lines| lines.contains(&json!("boom"))),
        "{noisy}"
    );
    assert!(
        noisy["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("stderr (last") && text.contains("boom")),
        "{noisy}"
    );

    let finished = call_tool(
        &mut client,
        5,
        "launch_app",
        json!({"command":"true","window_timeout_seconds":1}),
    );
    assert_ne!(finished["result"]["isError"], true, "{finished}");
    assert_eq!(
        finished["result"]["structuredContent"]["exit_status"], 0,
        "{finished}"
    );
    assert_eq!(
        finished["result"]["structuredContent"]["command_resolution"], "true -> shell builtin",
        "{finished}"
    );
    let assigned = call_tool(
        &mut client,
        8,
        "launch_app",
        json!({"command":"KWIN_MCP_DIAGNOSTIC_FIXTURE=1 sh -c 'exit 3'"}),
    );
    assert_eq!(
        assigned["result"]["structuredContent"]["exit_status"], 3,
        "{assigned}"
    );
    assert!(
        assigned["result"]["structuredContent"]["command_resolution"].is_null(),
        "assignment prefixes must not be misreported as missing executables: {assigned}"
    );
    assert!(
        std::fs::read_dir(workdir(&started))
            .expect("workdir")
            .flatten()
            .all(|entry| entry
                .path()
                .extension()
                .is_none_or(|ext| ext != "resolution")),
        "command-resolution sidecars remain after completed launches"
    );
    let retained = std::fs::read_dir(workdir(&started))
        .expect("workdir")
        .flatten()
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("launch-")
                && entry.path().extension().is_some_and(|ext| ext == "sh")
        })
        .count();
    assert_eq!(retained, 0, "launch command files were retained");

    let stopped = call_tool(&mut client, 6, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn launch_app_names_a_socket_path_in_a_host_directory_the_session_cannot_see() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    // The socket does not exist yet, so only its host parent directory is probed.
    let directory =
        std::env::temp_dir().join(format!("kwin-mcp-e2e-socket-dir-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("host directory");
    let socket = directory.join("sock");
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );
    let refused = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command": format!("true {}", socket.display())}),
    );
    assert_eq!(
        refused["error"]["data"]["reason"], "path_not_visible",
        "{refused}"
    );
    assert_eq!(
        refused["error"]["data"]["path"],
        json!(socket.display().to_string()),
        "{refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("is not visible in the session")),
        "{refused}"
    );
    let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
    assert!(stopped["error"].is_null(), "{stopped}");
    client.stop_process();
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, ChatGPT, input devices, and a live GPU session"]
fn launch_chatgpt_with_disk_backed_host_tmpdir() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let temp_root = fixture_temp_root().join(format!(
        "kwin-mcp-e2e-tmpdir-{}-{nonce}",
        std::process::id()
    ));
    use std::os::unix::fs::DirBuilderExt;
    let mut temp_dir = std::fs::DirBuilder::new();
    temp_dir
        .mode(0o700)
        .create(&temp_root)
        .expect("create disk-backed host TMPDIR");
    let _temp_root_cleanup = RemoveDirOnDrop(temp_root.clone());
    let host_tmpdir = temp_root.display().to_string();
    let mut client = RpcClient::start_with_env(&[("TMPDIR", &host_tmpdir)]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );
    let directory = workdir(&started);
    assert!(
        directory.starts_with(&temp_root),
        "session workdir {} did not use host TMPDIR {}",
        directory.display(),
        temp_root.display()
    );

    let debug_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve a free ChatGPT debug port")
        .local_addr()
        .expect("read reserved ChatGPT debug port")
        .port();
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "printf '%s\\n' \"$TMPDIR\" > \"$XDG_RUNTIME_DIR/tmpdir-proof\" && touch \"$TMPDIR/kwin-mcp-tmpdir-probe\" && /usr/lib/chatgpt/ChatGPT --user-data-dir=\"$HOME/.km-input-e2e-private\" --force-renderer-accessibility --remote-debugging-port={debug_port}"
        )}),
    );
    assert_ne!(launched["result"]["isError"], true, "{launched}");
    assert_eq!(
        std::fs::read_to_string(directory.join("tmpdir-proof"))
            .expect("read TMPDIR proof")
            .trim(),
        "/tmp",
        "launched processes should use the sandbox's writable private /tmp"
    );
    let window_id = launched["result"]["structuredContent"]["window"]
        .as_str()
        .expect("ChatGPT window ID");
    let windows = call_tool(&mut client, 4, "window_list", json!({}));
    assert!(
        windows["result"]["structuredContent"]["windows"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|window| window["id"] == window_id)),
        "ChatGPT window missing from window_list: {windows}"
    );

    let stopped = call_tool(&mut client, 5, "session_stop", json!({}));
    assert_ne!(stopped["result"]["isError"], true, "{stopped}");
    assert!(!directory.exists(), "session workdir remains");
    client.stop_process();
    std::fs::remove_dir_all(&temp_root).expect("remove disk-backed host TMPDIR");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Google Chrome, input devices, and a live GPU session"]
fn launch_app_reports_existing_chrome_window_after_url_handoff() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );

    let first = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({
            "command":"google-chrome-stable --profile-directory=Default --user-data-dir=\"$HOME/.local/share/kwin-mcp-e2e-profile-handoff\" --no-first-run --new-window 'data:text/html,<title>kwin-mcp-handoff-before</title>Before handoff'"
        }),
    );
    assert_ne!(first["result"]["isError"], true, "{first}");
    let first_window = first["result"]["structuredContent"]["window"]
        .as_str()
        .expect("initial Chrome window ID")
        .to_owned();

    let school = call_tool(
        &mut client,
        4,
        "launch_app",
        json!({
            "command":"google-chrome-stable --profile-directory=School --user-data-dir=\"$HOME/.local/share/kwin-mcp-e2e-profile-handoff\" --no-first-run --new-window 'data:text/html,<title>kwin-mcp-school-window</title>School profile'"
        }),
    );
    assert_ne!(school["result"]["isError"], true, "{school}");
    let school_window = school["result"]["structuredContent"]["window"]
        .as_str()
        .expect("School Chrome window ID")
        .to_owned();
    assert_ne!(school_window, first_window, "{school}");

    // A window can appear before Chrome loads its initial document. Establish
    // both titles so School's initial load cannot masquerade as a second handoff.
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows = call_tool(&mut client, 40, "window_list", json!({}));
        let ready = windows["result"]["structuredContent"]["windows"]
            .as_array()
            .is_some_and(|entries| {
                [
                    (&first_window, "kwin-mcp-handoff-before"),
                    (&school_window, "kwin-mcp-school-window"),
                ]
                .iter()
                .all(|(id, title)| {
                    entries.iter().any(|window| {
                        window["id"] == id.as_str()
                            && window["title"]
                                .as_str()
                                .is_some_and(|value| value.contains(title))
                    })
                })
            });
        if ready {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "initial pages not ready: {windows}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let handoff_command = "google-chrome-stable --profile-directory=Default --user-data-dir=\"$HOME/.local/share/kwin-mcp-e2e-profile-handoff\" --no-first-run 'data:text/html,<title>kwin-mcp-handoff-after</title>After handoff'";
    let handed_off = call_tool(
        &mut client,
        5,
        "launch_app",
        json!({
            "command":handoff_command
        }),
    );
    assert_ne!(handed_off["result"]["isError"], true, "{handed_off}");
    let result = &handed_off["result"]["structuredContent"];
    assert_eq!(result["window"], first_window, "{handed_off}");
    assert_eq!(result["window_outcome"], "reused_existing", "{handed_off}");
    assert_eq!(result["window_reused"], true, "{handed_off}");
    assert!(
        result["window_title"]
            .as_str()
            .is_some_and(|title| title.contains("kwin-mcp-handoff-after")),
        "handoff title was not reported: {handed_off}"
    );

    let focused_school = call_tool(
        &mut client,
        6,
        "window_activate",
        json!({"window_id":school_window}),
    );
    assert_ne!(
        focused_school["result"]["isError"], true,
        "{focused_school}"
    );

    // Multiple URLs can be forwarded without a new window or a changed title.
    let same_title_command = "google-chrome-stable --profile-directory=Default --user-data-dir=\"$HOME/.local/share/kwin-mcp-e2e-profile-handoff\" --no-first-run 'data:text/html,<title>kwin-mcp-handoff-after</title>Same title, another tab' 'data:text/html,<title>kwin-mcp-handoff-after</title>Same title, second tab'";
    let ambiguous = call_tool(
        &mut client,
        7,
        "launch_app",
        json!({"command":same_title_command,"window_timeout_seconds":1}),
    );
    assert_eq!(
        ambiguous["result"]["structuredContent"]["window_outcome"], "unattributed",
        "{ambiguous}"
    );
    assert_eq!(
        ambiguous["result"]["structuredContent"]["exit_status"], 0,
        "{ambiguous}"
    );
    assert!(
        ambiguous["result"]["structuredContent"]["handoff"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("activation or minimized state")),
        "missing attribution reason: {ambiguous}"
    );
    assert!(
        ambiguous["result"]["structuredContent"]["handoff"]["candidate_windows"]
            .as_array()
            .is_some_and(|candidates| {
                candidates
                    .iter()
                    .any(|candidate| candidate["window"] == first_window)
            }),
        "the focused existing window was not reported as a candidate: {ambiguous}"
    );

    let windows = call_tool(&mut client, 8, "window_list", json!({}));
    assert!(
        windows["result"]["structuredContent"]["windows"]
            .as_array()
            .is_some_and(|entries| {
                entries.iter().any(|window| {
                    window["id"] == first_window
                        && window["title"]
                            .as_str()
                            .is_some_and(|title| title.contains("kwin-mcp-handoff-after"))
                })
            }),
        "the reused Chrome window did not show the handed-off tab: {windows}"
    );

    let stopped = call_tool(&mut client, 9, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, input devices, and a live GPU session"]
fn launch_app_waits_for_a_delayed_window() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );

    let begun = Instant::now();
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":"sleep 18; konsole --separate --hold -e bash -c 'printf delayed-window-proof'","window_timeout_seconds":30}),
    );
    assert_ne!(launched["result"]["isError"], true, "{launched}");
    let elapsed = begun.elapsed();
    assert!(
        elapsed >= Duration::from_secs(17),
        "launch_app returned before the delayed window appeared: {elapsed:?}"
    );
    let window_id = launched["result"]["structuredContent"]["window"]
        .as_str()
        .expect("window ID");
    assert!(window_id.starts_with('{'), "{launched}");
    assert_eq!(
        launched["result"]["structuredContent"]["window_timeout_seconds"], 30,
        "{launched}"
    );
    let windows = call_tool(&mut client, 4, "window_list", json!({}));
    assert!(
        windows["result"]["structuredContent"]["windows"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|window| window["id"] == window_id)),
        "delayed window missing from window_list: {windows}"
    );

    let stopped = call_tool(&mut client, 5, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn session_overlay_lives_on_disk_and_is_removed_at_stop() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );
    let dir = workdir(&started);
    let disk = client
        .home
        .join(".cache/kwin-mcp")
        .join(dir.file_name().expect("workdir name"));
    assert!(
        disk.join("overlay-upper").is_dir(),
        "no overlay upper in {}",
        disk.display()
    );
    assert!(
        !dir.join("tmp/overlay-upper").exists(),
        "overlay upper still in {}",
        dir.display()
    );

    let seen = dir.join("disk-root-listing");
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!(
            "echo on-disk > \"$HOME/kwin-mcp-disk-probe\" && test -d \"$HOME/.cache/kwin-mcp\" && ls -A \"$HOME/.cache/kwin-mcp\" > '{}'; exit 3",
            seen.display()
        )}),
    );
    assert_eq!(
        launched["result"]["structuredContent"]["exit_status"], 3,
        "{launched}"
    );
    let listing = std::fs::read_to_string(&seen).expect("listing from the session");
    assert!(
        listing.trim().is_empty(),
        "the session sees overlay layers: {listing}"
    );
    let probe = ["overlay-upper", "overlay-root"]
        .iter()
        .map(|layer| disk.join(layer).join("kwin-mcp-disk-probe"))
        .find(|path| path.is_file())
        .expect("session write in a disk layer");
    assert_eq!(
        std::fs::read_to_string(probe).expect("probe").trim(),
        "on-disk"
    );

    let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
    assert!(
        !stopped["result"]["isError"].as_bool().unwrap_or(false),
        "{stopped}"
    );
    assert!(!disk.exists(), "session_stop left {}", disk.display());
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Chrome, input devices, and a live GPU session"]
fn keyboard_key_warns_when_the_browser_page_lacks_focus() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":1024,"height":768}),
    );
    let workdir = workdir(&started);
    let page = workdir.join("focus.html");
    std::fs::write(
        &page,
        "<html><head><title>K:ready</title></head><body><script>\
         addEventListener('keydown',e=>{if(['Control','Shift','Alt','Meta'].includes(e.key))return;\
         document.title='K:'+e.code;},true);\
         </script></body></html>",
    )
    .expect("write focus page");
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!("google-chrome-stable --no-first-run --new-window 'file://{}'", page.display())}),
    );
    assert!(launched["error"].is_null(), "launch failed: {launched}");
    let mut id = 4;
    let deadline = Instant::now() + Duration::from_secs(15);
    while window_title(&mut client, id, "K:ready").is_none() {
        id += 1;
        assert!(Instant::now() < deadline, "focus page did not load");
        thread::sleep(Duration::from_millis(300));
    }
    let key = |client: &mut RpcClient, id: &mut u64, combo: &str| -> Value {
        *id += 1;
        let pressed = call_tool(client, *id, "keyboard_key", json!({"key":combo}));
        assert!(pressed["error"].is_null(), "{combo} failed: {pressed}");
        pressed["result"]["structuredContent"]["warning"].clone()
    };
    // The page has focus: no warning, and the page sees the key.
    assert!(key(&mut client, &mut id, "a").is_null());
    // ctrl+L is sent while the page has focus and moves it to the address bar.
    assert!(key(&mut client, &mut id, "ctrl+L").is_null());
    let warned = key(&mut client, &mut id, "b");
    assert!(
        warned
            .as_str()
            .is_some_and(|text| text.contains("did not have keyboard focus")),
        "no warning with focus in the address bar: {warned}"
    );
    // A click inside the page gives it focus back.
    id += 1;
    let clicked = call_tool(&mut client, id, "mouse_click", json!({"x":500,"y":400}));
    assert!(clicked["error"].is_null(), "click failed: {clicked}");
    assert!(key(&mut client, &mut id, "c").is_null());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        id += 1;
        if window_title(&mut client, id, "K:KeyC").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "page did not see c after the click"
        );
        thread::sleep(Duration::from_millis(200));
    }
    id += 1;
    call_tool(&mut client, id, "session_stop", json!({}));
    client.stop_process();
}

fn direct_children(pid: u32) -> Vec<i32> {
    let parent = i32::try_from(pid).expect("parent pid");
    procfs::process::all_processes()
        .expect("read procfs")
        .flatten()
        .filter_map(|process| process.stat().ok())
        .filter(|stat| stat.ppid == parent)
        .map(|stat| stat.pid)
        .collect()
}

#[test]
fn discovery_creates_no_session_processes() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state_home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kwin-mcp-discovery-state-{nonce}"));
    std::fs::create_dir_all(&state_home).expect("create private lifecycle state");
    let state_home = state_home.to_string_lossy().into_owned();
    let extra_env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut client = RpcClient::start_with_env(&extra_env);
    thread::sleep(Duration::from_secs(2));
    assert!(direct_children(client.child.id()).is_empty());
    initialize(&mut client);
    client.send(2, "tools/list", json!({}));
    let listed = client.response(2, Duration::from_secs(10));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|tool| tool["name"] == "session_start"));
    assert!(tools.iter().any(|tool| tool["name"] == "session_list"));
    for tool in tools
        .iter()
        .filter(|tool| tool["name"] != "session_list" && tool["name"] != "session_start")
    {
        assert_eq!(
            tool["inputSchema"]["properties"]["session_id"]["type"],
            "string"
        );
    }
    let session_server_pid = u32::MAX;
    let session_id = format!("s{session_server_pid}-1");
    let unavailable = call_tool(
        &mut client,
        3,
        "window_list",
        json!({"session_id":session_id}),
    );
    assert_eq!(
        unavailable["error"]["data"]["reason"], "different_server_process",
        "{unavailable}"
    );
    assert_eq!(
        unavailable["error"]["data"]["session_status"], "owned_by_another_process",
        "{unavailable}"
    );
    assert_eq!(
        unavailable["error"]["data"]["session_available_here"], false,
        "{unavailable}"
    );
    assert_eq!(
        unavailable["error"]["data"]["previous_server_cleanup"],
        "not_confirmed_from_current_process",
        "{unavailable}"
    );
    assert!(
        unavailable["error"]["message"]
            .as_str()
            .is_some_and(
                |message| message.contains("session_start without session_id")
                    && message.contains("relaunch")
            ),
        "{unavailable}"
    );
    let no_live = call_tool(&mut client, 4, "window_list", json!({}));
    assert_eq!(
        no_live["error"]["data"]["reason"], "session_handle_unavailable",
        "{no_live}"
    );
    assert_eq!(
        no_live["error"]["data"]["session_status"], "none_running",
        "{no_live}"
    );
    let sessions = call_tool(&mut client, 5, "session_list", json!({}));
    assert_eq!(
        sessions["result"]["structuredContent"]["scope"], "current_process",
        "{sessions}"
    );
    client.send(6, "ping", json!({}));
    assert_eq!(
        client.response(6, Duration::from_secs(2))["result"],
        json!({})
    );
    for (index, method) in [
        "resources/list",
        "resources/templates/list",
        "prompts/list",
        "completion/complete",
    ]
    .iter()
    .enumerate()
    {
        let id = 7 + u64::try_from(index).expect("index");
        client.send(id, method, json!({"ref":{"type":"ref/prompt","name":"unused"},"argument":{"name":"unused","value":""}}));
        assert!(
            client.response(id, Duration::from_secs(2))["result"].is_object(),
            "{method} failed"
        );
    }
    thread::sleep(Duration::from_secs(4));
    assert!(
        direct_children(client.child.id()).is_empty(),
        "discovery retained a child"
    );
    assert!(
        !client
            .stderr
            .try_iter()
            .any(|line| line.contains("started child"))
    );
    client.stop_process();
    std::fs::remove_dir_all(state_home).expect("remove private lifecycle state");
}

#[test]
fn describe_matches_the_stdio_server() {
    let described = Command::new(
        std::env::var("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned()),
    )
    .args(["--describe", "--autoclean"])
    .output()
    .expect("describe");
    assert!(
        described.status.success(),
        "{}",
        String::from_utf8_lossy(&described.stderr)
    );
    let metadata: Value = serde_json::from_slice(&described.stdout).expect("metadata JSON");
    let mut client = RpcClient::start();
    client.send(1, "initialize", json!({"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"test", "version":"1"}}));
    let initialized = client.response(1, Duration::from_secs(10));
    assert_eq!(metadata["initialize"], initialized["result"]);
    client.notify("notifications/initialized", json!({}));
    client.send(2, "tools/list", json!({}));
    let listed = client.response(2, Duration::from_secs(10));
    let sorted = |value: &Value| {
        let mut tools = value.as_array().expect("tools").clone();
        tools.sort_by_key(|tool| tool["name"].as_str().unwrap_or_default().to_owned());
        tools
    };
    assert_eq!(
        sorted(&metadata["tools"]),
        sorted(&listed["result"]["tools"])
    );
    client.stop_process();
}

#[test]
fn non_pollable_stdio_error_identifies_the_descriptor() {
    let output = Command::new(
        std::env::var("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned()),
    )
    .args(["--width", "1232", "--height", "924", "--autoclean"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .expect("start server with null stdin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("MCP stdin (fd 0) is not pollable by epoll"),
        "startup error did not identify the unpollable input descriptor: {stderr}"
    );
}

#[test]
fn call_without_session_creates_no_processes() {
    let mut client = RpcClient::start();
    initialize(&mut client);
    let result = call_tool(&mut client, 2, "window_list", json!({}));
    assert!(result.to_string().contains("no live session"), "{result}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !direct_children(client.child.id()).is_empty() {
        assert!(Instant::now() < deadline, "unbound server did not exit");
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(4));
    assert!(direct_children(client.child.id()).is_empty());
    client.send(3, "ping", json!({}));
    assert_eq!(
        client.response(3, Duration::from_secs(2))["result"],
        json!({})
    );
    client.stop_process();
}

#[test]
fn unknown_session_from_another_server_reports_recovery_guidance() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state_home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kwin-mcp-other-server-state-{nonce}"));
    std::fs::create_dir_all(&state_home).expect("create private lifecycle state");
    let state_home = state_home.to_string_lossy().into_owned();
    let extra_env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut client = RpcClient::start_with_env(&extra_env);
    initialize(&mut client);
    let server_pid = client.child.id();
    let session_server_pid = u32::MAX;
    let session_id = format!("s{session_server_pid}-1");

    let response = call_tool(
        &mut client,
        2,
        "screenshot",
        json!({"session_id":session_id,"inline":true}),
    );
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(
        response["error"]["data"]["reason"], "different_server_process",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["session_status"], "owned_by_another_process",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["session_available_here"], false,
        "{response}"
    );
    assert_eq!(response["error"]["data"]["session_id"], session_id);
    assert_eq!(response["error"]["data"]["server_pid"], server_pid);
    assert_eq!(
        response["error"]["data"]["session_server_pid"],
        session_server_pid
    );
    let message = response["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("server restart"), "{message}");
    assert!(
        message.contains("session_start without session_id"),
        "{message}"
    );

    let local_missing_id = format!("s{server_pid}-999999");
    let local_missing = call_tool(
        &mut client,
        3,
        "window_list",
        json!({"session_id":local_missing_id}),
    );
    assert_eq!(
        local_missing["error"]["data"]["reason"], "session_not_found",
        "{local_missing}"
    );
    assert_eq!(
        local_missing["error"]["data"]["session_status"], "not_found",
        "{local_missing}"
    );
    assert_eq!(
        local_missing["error"]["data"]["session_available_here"], false,
        "{local_missing}"
    );
    assert!(direct_children(server_pid).is_empty());
    client.stop_process();
    std::fs::remove_dir_all(state_home).expect("remove private lifecycle state");
}

#[test]
fn silent_session_owner_handshake_has_a_deadline() {
    use std::os::unix::fs::PermissionsExt;
    let path = PathBuf::from(host_runtime_dir())
        .join(format!("kwin-mcp-control-{}.sock", std::process::id()));
    let listener =
        std::os::unix::net::UnixListener::bind(&path).expect("private silent owner socket");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("socket permissions");
    struct RemoveSocket(PathBuf);
    impl Drop for RemoveSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _socket = RemoveSocket(path);
    let (done_tx, done_rx) = mpsc::channel();
    let fixture = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("owner connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .expect("read deadline");
        let mut reader = BufReader::new(stream);
        let mut initialize = String::new();
        reader
            .read_line(&mut initialize)
            .expect("initialization request");
        assert!(initialize.contains("initialize"), "{initialize}");
        let _ = done_rx.recv_timeout(Duration::from_secs(8));
    });
    let mut caller = RpcClient::start();
    initialize(&mut caller);
    let began = Instant::now();
    let response = call_tool(
        &mut caller,
        2,
        "window_list",
        json!({"session_id":format!("s{}-1", std::process::id())}),
    );
    let elapsed = began.elapsed();
    let _ = done_tx.send(());
    fixture.join().expect("silent owner fixture");
    assert!(
        elapsed < Duration::from_secs(6),
        "owner handshake did not time out: {elapsed:?}"
    );
    assert_eq!(
        response["error"]["data"]["reason"], "different_server_process",
        "{response}"
    );
    caller.stop_process();
}

#[test]
fn unknown_session_from_a_live_server_is_checked_by_its_owner() {
    let mut owner = RpcClient::start();
    initialize(&mut owner);
    let owner_pid = owner.child.id();
    let mut caller = RpcClient::start();
    initialize(&mut caller);
    let session_id = format!("s{owner_pid}-999999");

    let response = call_tool(
        &mut caller,
        2,
        "viewer_close",
        json!({"session_id":session_id}),
    );
    assert_eq!(
        response["error"]["data"]["reason"], "session_not_found",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["server_pid"], owner_pid,
        "{response}"
    );
    assert_ne!(response["error"]["data"]["server_pid"], caller.child.id());

    drop(caller);
    drop(owner);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, input devices, and a live GPU session"]
fn live_session_tools_are_forwarded_to_the_owner_process() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    // Both desktops belong to this test; the caller's desktop hosts the viewer.
    let mut caller = RpcClient::start();
    initialize(&mut caller);
    let outer = call_tool(
        &mut caller,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let caller_id = session_handle(&outer);
    let display = workdir(&outer).join("wayland-0");
    let mut owner =
        RpcClient::start_with_env(&[("WAYLAND_DISPLAY", display.to_str().expect("display"))]);
    initialize(&mut owner);
    let started = call_tool(
        &mut owner,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let session_id = session_handle(&started);
    let launched = call_tool(
        &mut owner,
        3,
        "launch_app",
        json!({"session_id":session_id,"command":"konsole --separate --nofork -e bash --noprofile --norc"}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let window_id = launched["result"]["structuredContent"]["window"].clone();
    let windows = call_tool(
        &mut caller,
        3,
        "window_list",
        json!({"session_id":session_id}),
    );
    assert!(
        windows["result"]["structuredContent"]["windows"]
            .as_array()
            .is_some_and(|windows| windows.iter().any(|window| window["id"] == window_id)),
        "{windows}"
    );
    let opened = call_tool(
        &mut caller,
        4,
        "viewer_open",
        json!({"session_id":session_id}),
    );
    assert_eq!(
        opened["result"]["structuredContent"]["viewer"]["state"], "ready",
        "{opened}"
    );
    let viewer_pid = u32::try_from(
        opened["result"]["structuredContent"]["viewer"]["pid"]
            .as_u64()
            .expect("viewer PID"),
    )
    .expect("PID fits");
    let command = "printf routed-owner-proof > \"$HOME/routed-proof\"";
    let typed = call_tool(
        &mut caller,
        5,
        "keyboard_type",
        json!({"session_id":session_id,"text":command}),
    );
    assert!(typed["error"].is_null(), "{typed}");
    assert_eq!(
        typed["result"]["structuredContent"]["retry_safe"], false,
        "{typed}"
    );
    let returned = call_tool(
        &mut caller,
        6,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Return"}),
    );
    assert!(returned["error"].is_null(), "{returned}");
    let proof = caller.home.join("routed-proof.txt");
    let exported = call_tool(
        &mut caller,
        7,
        "export_file",
        json!({"session_id":session_id,"session_path":owner.home.join("routed-proof"),"host_path":proof}),
    );
    assert!(exported["error"].is_null(), "{exported}");
    assert_eq!(
        std::fs::read_to_string(&proof).expect("exported proof"),
        "routed-owner-proof"
    );
    let prefix = call_tool(
        &mut caller,
        20,
        "keyboard_type",
        json!({"session_id":session_id,"text":"printf %s '"}),
    );
    assert!(prefix["error"].is_null(), "{prefix}");
    for index in 0..12 {
        caller.send(100 + index, "tools/call", json!({"name":"keyboard_type","arguments":{"session_id":session_id,"text":format!("{index},")}}));
    }
    for index in 0..12 {
        let response = caller.response(100 + index, Duration::from_secs(30));
        assert!(response["error"].is_null(), "{response}");
    }
    let suffix = call_tool(
        &mut caller,
        21,
        "keyboard_type",
        json!({"session_id":session_id,"text":"' > \"$HOME/routed-order\""}),
    );
    assert!(suffix["error"].is_null(), "{suffix}");
    let submitted = call_tool(
        &mut caller,
        22,
        "keyboard_key",
        json!({"session_id":session_id,"key":"Return"}),
    );
    assert!(submitted["error"].is_null(), "{submitted}");
    let ordered_proof = caller.home.join("routed-order.txt");
    let exported_order = call_tool(
        &mut caller,
        23,
        "export_file",
        json!({"session_id":session_id,"session_path":owner.home.join("routed-order"),"host_path":ordered_proof}),
    );
    assert!(exported_order["error"].is_null(), "{exported_order}");
    assert_eq!(
        std::fs::read_to_string(ordered_proof).expect("ordered output"),
        (0..12).map(|index| format!("{index},")).collect::<String>(),
        "forwarded input lost wire order"
    );
    let log = std::fs::read_to_string(
        started["result"]["structuredContent"]["input_log"]
            .as_str()
            .expect("input log"),
    )
    .expect("owner audit");
    assert!(
        log.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|record| record["typing"]["characters"] == command),
        "owner did not audit routed text"
    );
    let closed = call_tool(
        &mut caller,
        8,
        "viewer_close",
        json!({"session_id":session_id}),
    );
    assert!(closed["error"].is_null(), "{closed}");
    assert!(
        !process_alive(viewer_pid).unwrap_or(true),
        "forwarded close did not reap viewer"
    );
    let stopped = call_tool(
        &mut caller,
        9,
        "session_stop",
        json!({"session_id":session_id}),
    );
    assert!(stopped["error"].is_null(), "{stopped}");
    owner.sessions.retain(|owned| owned != &session_id);
    assert!(!workdir(&started).exists(), "owner workdir remains");
    let listed = call_tool(&mut owner, 4, "session_list", json!({}));
    assert!(
        listed["result"]["structuredContent"]["sessions"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{listed}"
    );
    let caller_sessions = call_tool(&mut caller, 10, "session_list", json!({}));
    assert!(
        caller_sessions["result"]["structuredContent"]["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions.len() == 1 && sessions[0]["session_id"] == caller_id),
        "forwarding affected caller's desktop: {caller_sessions}"
    );
    caller.stop_process();
    owner.stop_process();
}

#[test]
#[ignore = "requires private KWin sessions, input devices, and a full one-minute idle TTL"]
fn live_idle_expiry_records_the_terminal_state_and_cleanup() {
    check_live_idle_expiry(false);
}

#[test]
#[ignore = "requires private KWin, input devices, one-minute idle TTL, and sudo for a synthetic cleanup blocker"]
fn live_expiry_cleanup_retry_preserves_the_reason_and_end_time() {
    check_live_idle_expiry(true);
}

struct CleanupBlocker(PathBuf);
impl CleanupBlocker {
    fn create(path: PathBuf) -> Self {
        assert!(
            Command::new("sudo")
                .args([
                    "-n", "install", "-d", "-m", "700", "-o", "root", "-g", "root"
                ])
                .arg(&path)
                .status()
                .expect("create synthetic cleanup blocker")
                .success()
        );
        Self(path)
    }
}
impl Drop for CleanupBlocker {
    fn drop(&mut self) {
        let _ = Command::new("sudo")
            .args(["-n", "rmdir"])
            .arg(&self.0)
            .status();
    }
}

fn check_live_idle_expiry(block_cleanup: bool) {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start_with_env(&[("KWIN_MCP_E2E_TTL_MINUTES", "1")]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600,"browser_profiles":"fresh"}),
    );
    let id = session_handle(&started);
    let runtime = workdir(&started);
    let disk = client
        .home
        .join(".cache/kwin-mcp")
        .join(runtime.file_name().expect("workdir name"));
    let log = client
        .home
        .join(".local/state/kwin-mcp/session-lifecycle.jsonl");
    let blocker = block_cleanup.then(|| CleanupBlocker::create(disk.join("synthetic-root-owned")));
    let deadline = Instant::now() + Duration::from_secs(85);
    let mut request = 3;
    loop {
        let list = call_tool(&mut client, request, "session_list", json!({}));
        request += 1;
        let sessions = list["result"]["structuredContent"]["sessions"]
            .as_array()
            .expect("sessions");
        if sessions
            .iter()
            .any(|entry| entry["session_id"] == id && entry["status"] == "expired")
            && (block_cleanup || (!runtime.exists() && !disk.exists()))
            && log.exists()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle session did not expire and clean up: {list}"
        );
        thread::sleep(Duration::from_secs(1));
    }
    let records = std::fs::read_to_string(&log).expect("real lifecycle log");
    let record: Value =
        serde_json::from_str(records.lines().last().expect("expiry record")).expect("expiry JSON");
    assert_eq!(record["session_id"], id);
    assert_eq!(record["state"], "session_expired");
    assert_eq!(record["reason"], "idle_timeout");
    assert_eq!(
        record["workdir_cleanup"],
        if block_cleanup { "retained" } else { "removed" }
    );
    let unavailable = call_tool(
        &mut client,
        request,
        "window_list",
        json!({"session_id":id}),
    );
    assert_eq!(
        unavailable["error"]["data"]["session_status"], "expired",
        "{unavailable}"
    );
    drop(blocker);
    thread::sleep(Duration::from_secs(1));
    let stopped = call_tool(
        &mut client,
        request + 1,
        "session_stop",
        json!({"session_id":id}),
    );
    assert!(stopped["error"].is_null(), "{stopped}");
    let stale = call_tool(
        &mut client,
        request + 2,
        "window_list",
        json!({"session_id":id}),
    );
    assert_eq!(
        stale["error"]["data"]["session_status"], "expired",
        "{stale}"
    );
    assert_eq!(
        stale["error"]["data"]["session_end_reason"], "idle_timeout",
        "{stale}"
    );
    assert!(!runtime.exists() && !disk.exists());
    assert_eq!(
        stale["error"]["data"]["previous_session_lifecycle_record"]["workdir_cleanup"], "removed",
        "{stale}"
    );
    assert_eq!(
        stale["error"]["data"]["session_ended_at_unix_seconds"], record["finished_at_unix_seconds"],
        "cleanup changed the terminal timestamp: {stale}"
    );
    eprintln!("expiry proof: {record}; unavailable={stale}");
    client.stop_process();
}

#[test]
#[ignore = "requires private KWin sessions, input devices, and sudo for one synthetic root-owned directory"]
fn cleanup_retry_updates_the_record_to_removed() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600,"browser_profiles":"fresh"}),
    );
    let id = session_handle(&started);
    let runtime = workdir(&started);
    let disk = client
        .home
        .join(".cache/kwin-mcp")
        .join(runtime.file_name().expect("workdir name"));
    let blocked = disk.join("synthetic-root-owned");
    let blocker = CleanupBlocker::create(blocked);
    let first = call_tool(&mut client, 3, "session_stop", json!({"session_id":id}));
    let kept = runtime.exists() && disk.exists();
    let still_blocked = call_tool(&mut client, 4, "session_stop", json!({"session_id":id}));

    drop(blocker);
    assert!(
        first["error"].is_object() && kept,
        "cleanup unexpectedly discarded ownership: {first}"
    );
    assert_eq!(
        still_blocked["error"]["data"]["session_status"], "stopped",
        "{still_blocked}"
    );
    assert_eq!(
        still_blocked["error"]["data"]["session_end_reason"], "session_stop",
        "{still_blocked}"
    );
    let second = call_tool(&mut client, 5, "session_stop", json!({"session_id":id}));

    assert!(second["error"].is_null(), "{second}");
    assert!(!runtime.exists() && !disk.exists());
    let stale = call_tool(&mut client, 6, "window_list", json!({"session_id":id}));
    eprintln!("cleanup retry proof: first={first}; second={second}; stale={stale}");
    assert_eq!(
        stale["error"]["data"]["previous_session_lifecycle_record"]["workdir_cleanup"], "removed",
        "successful cleanup left a stale retained record: {stale}"
    );
    client.stop_process();
}

#[test]
fn prior_manual_stop_and_ttl_expiry_report_terminal_session_status() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state_home =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kwin-mcp-terminal-state-{nonce}"));
    let log_dir = state_home.join("kwin-mcp");
    std::fs::create_dir_all(&log_dir).expect("create private lifecycle state");
    let state_home = state_home.to_string_lossy().into_owned();
    let extra_env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut client = RpcClient::start_with_env(&extra_env);
    initialize(&mut client);

    // Adjacent OS PIDs can belong to concurrent real test servers. Their
    // control sockets would forward these synthetic IDs outside this fixture.
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .expect("kernel PID limit")
        .trim()
        .parse()
        .expect("PID limit");
    let stopped_pid = pid_max.checked_add(1).expect("synthetic stopped PID");
    let expired_pid = pid_max.checked_add(2).expect("synthetic expired PID");
    let stopped_id = format!("s{stopped_pid}-1");
    let expired_id = format!("s{expired_pid}-1");
    let records = [
        json!({
            "session_id": stopped_id,
            "server_pid": stopped_pid,
            "state": "session_stopped",
            "reason": "session_stop",
            "finished_at_unix_seconds": 100,
            "workdir_cleanup": "removed",
        }),
        json!({
            "session_id": expired_id,
            "server_pid": expired_pid,
            "state": "session_expired",
            "reason": "idle_timeout",
            "finished_at_unix_seconds": 200,
            "workdir_cleanup": "removed",
        }),
    ];
    let contents = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        log_dir.join("session-lifecycle.jsonl"),
        format!("{contents}\n"),
    )
    .expect("write terminal lifecycle records");

    let stopped = call_tool(
        &mut client,
        2,
        "window_list",
        json!({"session_id":stopped_id}),
    );
    assert_eq!(
        stopped["error"]["data"]["session_status"], "stopped",
        "{stopped}"
    );
    assert_eq!(
        stopped["error"]["data"]["session_stopped_at_unix_seconds"], 100,
        "{stopped}"
    );
    assert_eq!(
        stopped["error"]["data"]["session_end_reason"], "session_stop",
        "{stopped}"
    );

    let expired = call_tool(
        &mut client,
        3,
        "window_list",
        json!({"session_id":expired_id}),
    );
    assert_eq!(
        expired["error"]["data"]["session_status"], "expired",
        "{expired}"
    );
    assert_eq!(
        expired["error"]["data"]["session_expired_at_unix_seconds"], 200,
        "{expired}"
    );
    assert_eq!(
        expired["error"]["data"]["session_end_reason"], "idle_timeout",
        "{expired}"
    );
    assert!(
        expired["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Session status is expired")),
        "{expired}"
    );

    client.stop_process();
    std::fs::remove_dir_all(state_home).expect("remove private lifecycle state");
}

#[test]
fn malformed_session_lifecycle_record_is_an_error() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state_home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kwin-mcp-malformed-state-{nonce}"));
    let log_dir = state_home.join("kwin-mcp");
    std::fs::create_dir_all(&log_dir).expect("create private lifecycle state");
    let state_home = state_home.to_string_lossy().into_owned();
    let extra_env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut client = RpcClient::start_with_env(&extra_env);
    initialize(&mut client);

    let server_pid = client.child.id();
    let session_server_pid = u32::MAX;
    let session_id = format!("s{session_server_pid}-1");
    let malformed = format!(
        "{}\n",
        json!({
            "session_id": session_id,
            "state": "shutdown_cleanup_finished",
            "reason": "server_shutdown",
            "finished_at_unix_seconds": 1,
            "workdir_cleanup": "removed",
        })
    );
    std::fs::write(log_dir.join("session-lifecycle.jsonl"), malformed)
        .expect("write malformed lifecycle record");

    let response = call_tool(
        &mut client,
        2,
        "window_list",
        json!({"session_id":session_id}),
    );
    assert_eq!(response["error"]["code"], -32603, "{response}");
    assert_eq!(
        response["error"]["data"]["reason"], "malformed_session_lifecycle_record",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["unknown_session_error"]["reason"], "different_server_process",
        "{response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("malformed session-lifecycle record")),
        "{response}"
    );
    assert!(direct_children(server_pid).is_empty());
    client.stop_process();
    std::fs::remove_dir_all(state_home).expect("remove private lifecycle state");
}

#[test]
fn oversized_session_lifecycle_log_is_trimmed_during_lookup() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state_home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kwin-mcp-oversized-state-{nonce}"));
    let log_dir = state_home.join("kwin-mcp");
    std::fs::create_dir_all(&log_dir).expect("create private lifecycle state");
    let lifecycle_log = log_dir.join("session-lifecycle.jsonl");
    let state_home = state_home.to_string_lossy().into_owned();
    let extra_env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut client = RpcClient::start_with_env(&extra_env);
    initialize(&mut client);

    let session_server_pid = u32::MAX;
    let session_id = format!("s{session_server_pid}-1");
    let seed_record = format!(
        "{}\n",
        json!({
            "session_id": session_id,
            "server_pid": session_server_pid,
            "state": "shutdown_cleanup_finished",
            "reason": "server_shutdown",
            "finished_at_unix_seconds": 1,
            "workdir_cleanup": "removed",
        })
    );
    let mut seed_log = Vec::new();
    while seed_log.len() <= SESSION_LIFECYCLE_LOG_MAX_BYTES {
        seed_log.extend_from_slice(seed_record.as_bytes());
    }
    std::fs::write(&lifecycle_log, seed_log).expect("seed oversized lifecycle log");

    let response = call_tool(
        &mut client,
        2,
        "window_list",
        json!({"session_id":session_id}),
    );
    assert_eq!(
        response["error"]["data"]["previous_server_cleanup"], "shutdown_cleanup_finished",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["session_status"], "stopped",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["session_stopped_at_unix_seconds"], 1,
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["session_end_reason"], "server_shutdown",
        "{response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Session status is stopped")),
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["previous_server_cleanup_record"]["session_id"], session_id,
        "{response}"
    );
    assert!(
        std::fs::metadata(&lifecycle_log)
            .expect("read capped lifecycle log metadata")
            .len()
            <= u64::try_from(SESSION_LIFECYCLE_LOG_MAX_BYTES).expect("lifecycle cap fits u64"),
        "session-lifecycle.jsonl exceeded its cap"
    );
    client.stop_process();
    std::fs::remove_dir_all(state_home).expect("remove private lifecycle state");
}

fn child_compositor(server: u32) -> i32 {
    child_named_process(server, "kwin_wayland")
}

fn child_named_process(server: u32, name: &str) -> i32 {
    let server = i32::try_from(server).expect("server pid");
    let stats: Vec<_> = procfs::process::all_processes()
        .expect("process inventory")
        .flatten()
        .filter_map(|process| process.stat().ok())
        .collect();
    let owned: Vec<_> = stats
        .iter()
        .filter(|stat| stat.comm == name)
        .filter(|stat| {
            let mut parent = stat.ppid;
            for _ in 0..64 {
                if parent == server {
                    return true;
                }
                let Some(next) = stats.iter().find(|stat| stat.pid == parent) else {
                    return false;
                };
                if next.ppid == parent {
                    return false;
                }
                parent = next.ppid;
            }
            false
        })
        .map(|stat| stat.pid)
        .collect();
    assert_eq!(
        owned.len(),
        1,
        "expected one {name} below server {server}: {owned:?}"
    );
    owned[0]
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, input devices, and a live GPU session"]
fn screenshot_timeout_is_retryable_and_preserves_the_desktop() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600,"browser_profiles":"fresh"}),
    );
    let session_id = session_handle(&started);
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"session_id":session_id,"command":"konsole --separate --hold -e bash -c 'printf screenshot-timeout-proof'"}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let before = call_tool(
        &mut client,
        4,
        "screenshot",
        json!({"session_id":session_id}),
    );
    assert!(before["error"].is_null(), "{before}");
    let proof = std::env::var_os("KWIN_MCP_E2E_PROOF_DIR").map(PathBuf::from);
    if let Some(path) = &proof {
        std::fs::create_dir_all(path).expect("evidence directory");
        std::fs::copy(
            before["result"]["structuredContent"]["path"]
                .as_str()
                .expect("before image"),
            path.join("before.png"),
        )
        .expect("copy before image");
    }
    let resumed = ResumeProcess(nix::unistd::Pid::from_raw(child_compositor(
        client.child.id(),
    )));
    nix::sys::signal::kill(resumed.0, nix::sys::signal::Signal::SIGSTOP)
        .expect("pause owned compositor");
    let begun = Instant::now();
    client.send(
        5,
        "tools/call",
        json!({"name":"screenshot","arguments":{"session_id":session_id}}),
    );
    thread::sleep(Duration::from_millis(300));
    client.send(6, "ping", json!({}));
    assert_eq!(
        client.response(6, Duration::from_secs(2))["result"],
        json!({})
    );
    let fault = client.response(5, Duration::from_secs(25));
    let elapsed_ms = begun.elapsed().as_millis();
    let data = &fault["error"]["data"];
    assert_eq!(data["reason"], "frame_unavailable", "{fault}");
    assert_eq!(data["retry_safe"], true, "{fault}");
    assert_eq!(data["retry_after_ms"], 1000, "{fault}");
    assert_eq!(data["stage"], "screenshot", "{fault}");
    assert!(
        (19_000..25_000).contains(&elapsed_ms),
        "{elapsed_ms}: {fault}"
    );
    drop(resumed);
    let windows = call_tool(
        &mut client,
        7,
        "window_list",
        json!({"session_id":session_id}),
    );
    assert!(
        windows.to_string().to_lowercase().contains("konsole"),
        "{windows}"
    );
    let after = call_tool(
        &mut client,
        8,
        "screenshot",
        json!({"session_id":session_id}),
    );
    assert!(after["error"].is_null(), "{after}");
    assert_eq!(
        after["result"]["structuredContent"]["window"]["id"],
        before["result"]["structuredContent"]["window"]["id"]
    );
    if let Some(path) = &proof {
        std::fs::copy(
            after["result"]["structuredContent"]["path"]
                .as_str()
                .expect("after image"),
            path.join("after.png"),
        )
        .expect("copy after image");
        std::fs::write(path.join("timeout.json"), serde_json::to_vec_pretty(&json!({"elapsed_ms":elapsed_ms,"fault":fault,"before":before,"after":after,"windows":windows})).expect("evidence JSON")).expect("save timeout evidence");
    }
    let stopped = call_tool(
        &mut client,
        9,
        "session_stop",
        json!({"session_id":session_id}),
    );
    assert!(stopped["error"].is_null(), "{stopped}");
    client.stop_process();
}

struct ResumeProcess(nix::unistd::Pid);
impl Drop for ResumeProcess {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGCONT);
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, input devices, and a live GPU session"]
fn launch_app_submits_despite_its_own_frozen_compositor_and_recovers() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    assert_eq!(
        started["result"]["structuredContent"]["status"], "started",
        "{started}"
    );
    let workdir = workdir(&started);
    let compositor = child_compositor(client.child.id());
    let resumed = ResumeProcess(nix::unistd::Pid::from_raw(compositor));
    nix::sys::signal::kill(resumed.0, nix::sys::signal::Signal::SIGSTOP)
        .expect("stop owned compositor");
    let begun = Instant::now();
    client.send(3, "tools/call", json!({"name":"launch_app","arguments":{"command":"konsole --separate --hold -e bash -c 'printf launch-deadline-proof'"}}));
    thread::sleep(Duration::from_millis(300));
    let ping_started = Instant::now();
    client.send(4, "ping", json!({}));
    assert_eq!(
        client.response(4, Duration::from_secs(2))["result"],
        json!({})
    );
    let ping_ms = ping_started.elapsed().as_millis();
    // The window baselines are bounded at 5 seconds each and never hold the
    // command back, so it is submitted; the window wait then ends at its own 15.
    let fault_response = client.response(3, Duration::from_secs(45));
    let elapsed_ms = begun.elapsed().as_millis();
    let payload = &fault_response["result"]["structuredContent"];
    assert!(fault_response["error"].is_null(), "{fault_response}");
    assert_eq!(payload["command_submitted"], true, "{fault_response}");
    assert_eq!(payload["window"], "timeout", "{fault_response}");
    assert_eq!(
        payload["window_baseline"], "unavailable",
        "{fault_response}"
    );
    let failures = payload["window_baseline_errors"]
        .as_array()
        .expect("baseline diagnostics");
    assert_eq!(failures.len(), 2, "{fault_response}");
    assert_eq!(failures[0]["stage"], "pre_launch");
    assert_eq!(failures[1]["stage"], "after_submit");
    for failure in failures {
        let reason = failure["reason"]
            .as_str()
            .expect("inventory failure reason");
        assert!(
            reason.contains("KWin") && reason.contains("within 5 seconds"),
            "{reason}"
        );
    }
    assert!(
        (24_000..29_000).contains(&elapsed_ms),
        "launch duration {elapsed_ms}ms"
    );
    eprintln!(
        "launch fault proof: server={} compositor={compositor} ping_ms={ping_ms} launch_ms={elapsed_ms} response={fault_response}",
        client.child.id()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let scripts = std::fs::read_dir(
            client
                .home
                .join(".cache/kwin-mcp")
                .join(workdir.file_name().expect("workdir name"))
                .join("kwin-scripts"),
        )
        .expect("workdir")
        .flatten()
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("kwin-mcp-")
                && entry.path().extension().is_some_and(|ext| ext == "js")
        })
        .count();
        if scripts == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled script file was retained"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let bus = zbus::blocking::connection::Builder::address(
        format!("unix:path={}", workdir.join("bus").display()).as_str(),
    )
    .expect("session bus address")
    .build()
    .expect("session bus connection");
    let server_name = started["result"]["structuredContent"]["bus"]
        .as_str()
        .expect("server bus name");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let reply = bus.call_method(
            Some(server_name),
            "/KWinMCP",
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        );
        let empty = match reply {
            Ok(reply) => !reply
                .body()
                .deserialize::<String>()
                .expect("introspection XML")
                .contains("<node name="),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" =>
            {
                true
            }
            Err(error) => panic!("callback inspection failed: {error}"),
        };
        if empty {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled callback registration was retained"
        );
        thread::sleep(Duration::from_millis(100));
    }
    drop(resumed);
    let launched = call_tool(
        &mut client,
        5,
        "launch_app",
        json!({"command":"konsole --separate --hold -e bash -c 'printf launch-deadline-recovery'"}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    assert_ne!(launched["result"]["isError"], true, "{launched}");
    let windows = call_tool(&mut client, 6, "window_list", json!({}));
    assert!(
        windows.to_string().to_lowercase().contains("konsole"),
        "{windows}"
    );
    let screenshot = call_tool(&mut client, 7, "screenshot", json!({"inline":false}));
    assert!(screenshot["error"].is_null(), "{screenshot}");
    if let Some(directory) = std::env::var_os("KWIN_MCP_PROOF_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("proof directory");
        let capture = screenshot["result"]["structuredContent"]["path"]
            .as_str()
            .expect("screenshot path");
        std::fs::copy(capture, directory.join("launch-recovery.png")).expect("copy screenshot");
        std::fs::write(directory.join("launch-fault.json"), json!({"server":client.child.id(),"compositor":compositor,"ping_ms":ping_ms,"launch_ms":elapsed_ms,"response":fault_response,
            "commit":started["result"]["structuredContent"]["commit"],"version":started["result"]["structuredContent"]["version"]}).to_string()).expect("write fault proof");
    }
    let stopped = call_tool(&mut client, 8, "session_stop", json!({}));
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(!workdir.exists());
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, squashfuse, mksquashfs, input devices, and a live GPU session"]
fn session_start_bounds_large_ancestor_copies_with_a_nested_mount() {
    use base64::Engine;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    // Keep the test mount outside the real HOME so other agents' overlay plans
    // do not change while this fixture is mounted. Use a short fixture root.
    let fixture = fixture_temp_root().join(format!("kmn-{}-{nonce:x}", std::process::id()));
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _fixture = Fixture(fixture.clone());
    let home = fixture.join("home");
    for directory in [
        ".config",
        ".local/share",
        ".cache",
        ".local/state",
        ".kde",
        "project/portal",
    ] {
        std::fs::create_dir_all(home.join(directory)).expect("fixture directory");
    }
    let mount_source = fixture.join("mount-source");
    std::fs::create_dir(&mount_source).expect("mount source");
    std::fs::write(mount_source.join("visible.txt"), "mounted-data").expect("mounted fixture");
    let mount_image = fixture.join("mount.squashfs");
    assert!(
        Command::new("mksquashfs")
            .arg(&mount_source)
            .arg(&mount_image)
            .args(["-noappend", "-processors", "1", "-quiet"])
            .stdout(Stdio::null())
            .status()
            .expect("create mount image")
            .success()
    );
    let mount_point = home.join("project/portal");
    struct Mount(Child, PathBuf);
    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = Command::new("fusermount3").arg("-u").arg(&self.1).status();
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _mount = Mount(
        Command::new("squashfuse")
            .arg("-f")
            .arg(&mount_image)
            .arg(&mount_point)
            .stdout(Stdio::null())
            .spawn()
            .expect("mount owned fixture"),
        mount_point.clone(),
    );
    let mount_deadline = Instant::now() + Duration::from_secs(5);
    while !procfs::process::Process::myself()
        .expect("self")
        .mountinfo()
        .expect("mounts")
        .0
        .iter()
        .any(|mount| mount.mount_point == mount_point)
    {
        assert!(
            Instant::now() < mount_deadline,
            "fixture mount did not appear"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let graphical_socket = home.join("project/host-graphical.sock");
    assert!(
        graphical_socket.as_os_str().as_encoded_bytes().len() <= 107,
        "fixture socket path exceeds 107 bytes: {}; choose a shorter KWIN_MCP_TEST_TMPDIR or TMPDIR",
        graphical_socket.display()
    );
    struct GraphicalFixture(Child);
    impl Drop for GraphicalFixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _graphical = GraphicalFixture(
        Command::new(std::env::current_exe().expect("fixture executable"))
            .args(["--ignored", "--exact", "host_graphical_socket_fixture"])
            .env("KWIN_GRAPHICAL_SOCKET_FIXTURE", &graphical_socket)
            // Only this owned helper is marked graphical; no host connection is made.
            .env("WAYLAND_DISPLAY", "wayland-0")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("start graphical fixture"),
    );
    let socket_deadline = Instant::now() + Duration::from_secs(5);
    while !graphical_socket.exists() {
        assert!(
            Instant::now() < socket_deadline,
            "graphical fixture did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let ordinary = home.join("project/ordinary.bin");
    let large = home.join("project/large.bin");
    let explicit = home.join("project/explicit.bin");
    let threshold = 64 * 1024 * 1024;
    for (path, length) in [
        (&ordinary, threshold),
        (&large, threshold + 1),
        (&explicit, threshold + 1),
    ] {
        let mut file = std::fs::File::create(path).expect("fixture file");
        file.set_len(length).expect("sparse fixture size");
        file.write_all(b"host-data").expect("fixture header");
        file.seek(SeekFrom::End(-9)).expect("fixture tail position");
        file.write_all(b"host-tail").expect("fixture tail");
    }
    let clone_probe = fixture.join("clone-probe");
    let reflink = Command::new("cp")
        .args(["--reflink=always", "--"])
        .arg(&large)
        .arg(&clone_probe)
        .stderr(Stdio::null())
        .status()
        .expect("reflink probe")
        .success();
    let _ = std::fs::remove_file(&clone_probe);
    let quote =
        |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
    let home_text = home.display().to_string();
    let mut client = RpcClient::start_with_env(&[
        ("HOME", &home_text),
        ("XDG_CONFIG_HOME", &format!("{home_text}/.config")),
        ("XDG_DATA_HOME", &format!("{home_text}/.local/share")),
        ("XDG_CACHE_HOME", &format!("{home_text}/.cache")),
        ("XDG_STATE_HOME", &format!("{home_text}/.local/state")),
        ("KDEHOME", &format!("{home_text}/.kde")),
    ]);
    initialize(&mut client);
    let started_at = Instant::now();
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({
            "width":800, "height":600, "writable_paths":[explicit]
        }),
    );
    if !started["error"].is_null() {
        for line in client.stderr.try_iter() {
            eprintln!("{line}");
        }
    }
    assert!(
        started_at.elapsed() < Duration::from_secs(20),
        "startup exceeded deadline: {started}"
    );
    let dir = workdir(&started);
    let socket_probe = dir.join("graphical-socket-probe");
    std::fs::copy(
        std::env::current_exe().expect("probe executable"),
        &socket_probe,
    )
    .expect("copy graphical socket probe");
    let socket_report = dir.join("graphical-socket-report.txt");
    let disk = home
        .join(".cache/kwin-mcp")
        .join(dir.file_name().expect("workdir name"));
    if !reflink {
        // bwrap creates an empty mountpoint file beneath the read-only bind.
        let staged = std::fs::metadata(disk.join("overlay-root/project/large.bin"))
            .expect("bind mountpoint");
        assert_eq!(staged.len(), 0, "eager oversized copy");
        assert_eq!(
            staged.blocks(),
            0,
            "oversized file data allocated in staging"
        );
    }
    let fallback = started["result"]["structuredContent"]["oversized_read_only_files"]
        .as_array()
        .expect("fallback report");
    assert_eq!(fallback.contains(&json!(large)), !reflink, "{started}");
    assert!(
        !fallback.contains(&json!(ordinary)) && !fallback.contains(&json!(explicit)),
        "{started}"
    );
    assert!(
        disk.join("overlay-root/project/explicit.bin").is_file(),
        "explicit writable copy missing"
    );
    // session_start always creates a new desktop. Keep the original handle
    // throughout this fixture so all checks exercise its writable_paths.
    let session_id = session_handle(&started);
    let report = dir.join("ancestor-check.txt");
    let script = dir.join("ancestor-check.sh");
    let oversized_write = if reflink {
        format!(
            "printf sessionxx | dd of={} bs=9 count=1 conv=notrunc status=none\n",
            quote(&large)
        )
    } else {
        format!(
            "if printf sessionxx | dd of={} bs=9 count=1 conv=notrunc status=none 2>/dev/null; then exit 1; fi\n",
            quote(&large)
        )
    };
    std::fs::write(
        &script,
        format!(
            "set -eu\n\
         test \"$(cat {})\" = mounted-data\n\
         for path in {} {} {}; do\n\
           test \"$(head -c 9 \"$path\")\" = host-data\n\
           test \"$(tail -c 9 \"$path\")\" = host-tail\n\
         done\n\
         for path in {} {}; do\n\
           printf sessionxx | dd of=\"$path\" bs=9 count=1 conv=notrunc status=none\n\
           test \"$(head -c 9 \"$path\")\" = sessionxx\n\
         done\n\
         {oversized_write}\
         printf 'ANCESTOR_COPY_PASS\\n' | tee {}\n",
            quote(&home.join("project/portal/visible.txt")),
            quote(&ordinary),
            quote(&large),
            quote(&explicit),
            quote(&ordinary),
            quote(&explicit),
            quote(&report)
        ),
    )
    .expect("session check script");
    let script_contents = std::fs::read_to_string(&script).expect("read session check script");
    let socket_check = format!(
        "KWIN_GRAPHICAL_HOME_SOCKET={} KWIN_GRAPHICAL_HOME_REPORT={} {} --ignored --exact session_graphical_home_socket_probe --nocapture\n",
        quote(&graphical_socket),
        quote(&socket_report),
        quote(&socket_probe)
    );
    std::fs::write(&script, format!("{script_contents}{socket_check}"))
        .expect("add graphical socket check");
    let launched = call_tool(
        &mut client,
        4,
        "launch_app",
        json!({
            "session_id": session_id, "command": format!("konsole --hold -e bash {}", quote(&script))
        }),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::fs::read_to_string(&report).ok().as_deref() != Some("ANCESTOR_COPY_PASS\n")
        || std::fs::read_to_string(&socket_report).ok().as_deref()
            != Some("GRAPHICAL_SOCKET_BLOCKED\n")
    {
        assert!(
            Instant::now() < deadline,
            "session file or graphical socket checks did not pass: {launched}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let screenshot = call_tool(
        &mut client,
        5,
        "screenshot",
        json!({"session_id": session_id, "inline":true}),
    );
    assert!(
        !inline_png(&screenshot).is_empty(),
        "missing rendered output"
    );
    if let Ok(proof) = std::env::var("KWIN_MCP_E2E_PROOF_DIR") {
        std::fs::create_dir_all(&proof).expect("proof directory");
        let image = base64::engine::general_purpose::STANDARD
            .decode(inline_png(&screenshot))
            .expect("screenshot base64");
        std::fs::write(PathBuf::from(&proof).join("nested-ancestor.png"), image)
            .expect("retain screenshot");
        std::fs::write(
            PathBuf::from(&proof).join("nested-start.json"),
            started.to_string(),
        )
        .expect("retain start response");
    }
    for path in [&ordinary, &large, &explicit] {
        let mut file = std::fs::File::open(path).expect("host fixture");
        let mut header = [0; 9];
        file.read_exact(&mut header).expect("host header");
        assert_eq!(
            &header,
            b"host-data",
            "host file changed: {}",
            path.display()
        );
        assert_eq!(
            file.metadata().expect("host metadata").nlink(),
            1,
            "host hardlink created"
        );
    }
    let stopped = call_tool(
        &mut client,
        6,
        "session_stop",
        json!({"session_id": session_id}),
    );
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(!dir.exists() && !disk.exists(), "owned workdirs remain");
    client.stop_process();
    println!(
        "PASS: nested mount, 64 MiB writable boundary, oversized readable/private, explicit writable copy, rendered output, host files unchanged, cleanup; reflink={reflink}"
    );
}

fn copy_source(source: &std::path::Path, target: &std::path::Path) {
    if source.is_dir() {
        std::fs::create_dir_all(target).expect("source directory");
        for entry in std::fs::read_dir(source).expect("source entries") {
            let entry = entry.expect("source entry");
            copy_source(&entry.path(), &target.join(entry.file_name()));
        }
    } else {
        std::fs::create_dir_all(target.parent().expect("source parent"))
            .expect("source parent directory");
        std::fs::copy(source, target).expect("source file");
    }
}

fn session_handle(response: &Value) -> String {
    let Some(session_id) = response["result"]["structuredContent"]["session_id"].as_str() else {
        panic!("session_start did not return a session ID: {response}");
    };
    session_id.to_owned()
}
fn save_session_image(client: &mut RpcClient, request: u64, session: &str, name: &str) {
    use base64::Engine;
    let result = call_tool(
        client,
        request,
        "screenshot",
        json!({"session_id":session,"inline":true}),
    );
    let image = result["result"]["content"]
        .as_array()
        .expect("screenshot content")
        .iter()
        .find(|item| item["type"] == "image")
        .expect("session screenshot");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().expect("PNG data"))
        .expect("decode PNG");
    if let Some(directory) = std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("image directory");
        std::fs::write(directory.join(name), bytes).expect("save session image");
    }
}
fn submitted_text(path: &std::path::Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "private dialog did not submit {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "requires private KWin sessions, bubblewrap, input devices, and kdialog"]
fn one_process_routes_parallel_sessions_and_orders_input_on_stdio() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let starting = Instant::now();
    for id in [2, 3] {
        client.send(
            id,
            "tools/call",
            json!({"name":"session_start","arguments":{"width":800,"height":600}}),
        );
    }
    let a = client.response(2, Duration::from_secs(30));
    let b = client.response(3, Duration::from_secs(30));
    assert_eq!(a["result"]["structuredContent"]["status"], "started", "{a}");
    assert_eq!(b["result"]["structuredContent"]["status"], "started", "{b}");
    let a_id = session_handle(&a);
    let b_id = session_handle(&b);
    assert_ne!(a_id, b_id);
    assert_ne!(workdir(&a), workdir(&b));
    assert!(
        starting.elapsed() < Duration::from_secs(25),
        "session starts serialized or exceeded their deadline"
    );
    let records = call_tool(&mut client, 4, "session_list", json!({}));
    let records = records["result"]["structuredContent"]["sessions"]
        .as_array()
        .expect("sessions");
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|session| session["pid"] == client.child.id())
    );
    let server_children: Vec<_> = direct_children(client.child.id())
        .into_iter()
        .filter(|pid| {
            std::fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .is_some_and(|path| {
                    path.file_name()
                        .is_some_and(|name| name == "kwin-mcp" || name == "kwin-mcp-shim")
                })
        })
        .collect();
    assert!(
        server_children.is_empty(),
        "per-session servers remain: {server_children:?}"
    );
    let ambiguous = call_tool(&mut client, 5, "window_list", json!({}));
    assert!(
        ambiguous.to_string().contains("pass the session_id"),
        "{ambiguous}"
    );
    for (request, id, response, title) in [(6, &a_id, &a, "private-A"), (7, &b_id, &b, "private-B")]
    {
        let command = format!(
            "kdialog --title {title} --inputbox 'Ordered input' > {}/submitted.txt",
            workdir(response).display()
        );
        let launched = call_tool(
            &mut client,
            request,
            "launch_app",
            json!({"session_id":id,"command":command}),
        );
        assert!(launched["error"].is_null(), "{launched}");
        let windows = call_tool(
            &mut client,
            request + 20,
            "window_list",
            json!({"session_id":id}),
        );
        assert!(windows.to_string().contains(title), "{windows}");
        let other = if title == "private-A" {
            "private-B"
        } else {
            "private-A"
        };
        assert!(
            !windows.to_string().contains(other),
            "windows crossed desktops"
        );
    }
    let a_sandbox = records
        .iter()
        .find(|record| record["session_id"] == a_id)
        .expect("A record")["sandbox_pid"]
        .as_u64()
        .expect("sandbox PID");
    let compositor = child_compositor(u32::try_from(a_sandbox).expect("PID range"));
    let resumed = ResumeProcess(nix::unistd::Pid::from_raw(compositor));
    nix::sys::signal::kill(resumed.0, nix::sys::signal::Signal::SIGSTOP)
        .expect("stop private A compositor");
    for request in [400, 401] {
        client.send(
            request,
            "tools/call",
            json!({"name":"window_list","arguments":{"session_id":a_id}}),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let concurrent = std::fs::read_dir(
            client
                .home
                .join(".cache/kwin-mcp")
                .join(workdir(&a).file_name().expect("workdir name"))
                .join("kwin-scripts"),
        )
        .expect("A workdir")
        .flatten()
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("kwin-mcp-")
                && entry.path().extension().is_some_and(|ext| ext == "js")
        })
        .count();
        if concurrent >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "same-session reads serialized");
        thread::sleep(Duration::from_millis(10));
    }
    let begun = Instant::now();
    let b_windows = call_tool(&mut client, 402, "window_list", json!({"session_id":b_id}));
    assert!(b_windows.to_string().contains("private-B"), "{b_windows}");
    assert!(
        begun.elapsed() < Duration::from_secs(2),
        "B waited for the frozen A compositor"
    );
    eprintln!(
        "two A reads concurrently pending; B window_list completed in {}ms",
        begun.elapsed().as_millis()
    );
    drop(resumed);
    for request in [400, 401] {
        let response = client.response(request, Duration::from_secs(5));
        assert!(response.to_string().contains("private-A"), "{response}");
    }
    for offset in 0..12 {
        for (base, id, letter) in [(100, &a_id, "a"), (200, &b_id, "b")] {
            client.send(base + offset, "tools/call", json!({"name":"keyboard_type","arguments":{"session_id":id,"text":format!("{letter}{offset},")}}));
        }
    }
    for offset in 0..12 {
        for base in [100, 200] {
            let response = client.response(base + offset, Duration::from_secs(30));
            assert!(response["error"].is_null(), "{response}");
        }
    }
    save_session_image(&mut client, 300, &a_id, "session-a-ordered.png");
    save_session_image(&mut client, 301, &b_id, "session-b-ordered.png");
    for (request, id) in [(302, &a_id), (303, &b_id)] {
        let pressed = call_tool(
            &mut client,
            request,
            "keyboard_key",
            json!({"session_id":id,"key":"Return"}),
        );
        assert!(pressed["error"].is_null(), "{pressed}");
    }
    for (response, letter) in [(&a, "a"), (&b, "b")] {
        let expected = format!(
            "{}\n",
            (0..12)
                .map(|offset| format!("{letter}{offset},"))
                .collect::<String>()
        );
        assert_eq!(
            submitted_text(&workdir(response).join("submitted.txt")),
            expected
        );
    }
    for (request, id, response) in [(304, &a_id, &a), (305, &b_id, &b)] {
        let stopped = call_tool(
            &mut client,
            request,
            "session_stop",
            json!({"session_id":id}),
        );
        assert_eq!(
            stopped["result"]["structuredContent"]["status"], "stopped",
            "{stopped}"
        );
        assert!(!workdir(response).exists());
    }
    let listed = call_tool(&mut client, 306, "session_list", json!({}));
    assert_eq!(listed["result"]["structuredContent"]["sessions"], json!([]));
    client.send(307, "ping", json!({}));
    assert_eq!(
        client.response(307, Duration::from_secs(5))["result"],
        json!({})
    );
    client.stop_process();
}

#[test]
#[ignore = "requires private KWin sessions, bubblewrap, input devices, and kdialog"]
fn reexecution_preserves_pid_pipes_live_windows_and_partial_requests() {
    check_reexecution(false);
}

#[test]
#[ignore = "requires private KWin sessions, kdialog, and a Rust build toolchain"]
fn source_rebuild_preserves_sessions_and_changes_published_tools() {
    check_reexecution(true);
}

fn check_reexecution(source_reload: bool) {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start_with_env(&[
        ("KWIN_MCP_E2E_COPY_BINARY", "1"),
        (
            "KWIN_MCP_E2E_SOURCE_RELOAD",
            if source_reload { "1" } else { "0" },
        ),
    ]);
    initialize(&mut client);
    let a = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let b = call_tool(
        &mut client,
        3,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let a_log = PathBuf::from(
        a["result"]["structuredContent"]["input_log"]
            .as_str()
            .expect("input log"),
    );
    let a_id = session_handle(&a);
    let b_id = session_handle(&b);
    for (request, id, response, title) in [(4, &a_id, &a, "reload-A"), (5, &b_id, &b, "reload-B")] {
        let command = format!(
            "kdialog --title {title} --inputbox 'Survive reload' > {}/submitted.txt",
            workdir(response).display()
        );
        let launched = call_tool(
            &mut client,
            request,
            "launch_app",
            json!({"session_id":id,"command":command}),
        );
        assert!(launched["error"].is_null(), "{launched}");
        call_tool(
            &mut client,
            request + 10,
            "keyboard_type",
            json!({"session_id":id,"text":title}),
        );
    }
    save_session_image(&mut client, 20, &a_id, "session-a-before-reload.png");
    let log_before = std::fs::read_to_string(&a_log).expect("input before reload");
    let pid = client.child.id();
    let pipes: Vec<_> = [0, 1]
        .map(|fd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).expect("client pipe"))
        .into_iter()
        .collect();
    let partial = json!({"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"window_list","arguments":{"session_id":b_id}}}).to_string();
    let split = partial.len() / 2;
    client
        .stdin
        .as_mut()
        .expect("open client stdin")
        .write_all(&partial.as_bytes()[..split])
        .expect("partial JSON request");
    client
        .stdin
        .as_mut()
        .expect("open client stdin")
        .flush()
        .expect("partial flush");
    let binary = client
        .binary_copy
        .as_ref()
        .expect("private executable")
        .clone();
    if source_reload {
        let source = client.home.join("source/src/main.rs");
        let text = std::fs::read_to_string(&source).expect("private server source");
        assert!(text.contains("List live isolated sessions in this server process."));
        std::fs::write(
            source,
            text.replace(
                "List live isolated sessions in this server process.",
                "List live isolated sessions after a private source rebuild.",
            ),
        )
        .expect("edit private tool description");
    } else {
        let replacement = binary.with_extension("next");
        let replacement_source = std::env::var_os("KWIN_MCP_E2E_REEXEC_REPLACEMENT")
            .map(PathBuf::from)
            .unwrap_or_else(|| binary.clone());
        std::fs::copy(&replacement_source, &replacement).expect("stage rebuilt executable");

        std::fs::rename(&replacement, &binary).expect("publish rebuilt executable");
    }
    client.wait_for_stderr(
        "reload: reattached 2 sessions",
        Duration::from_secs(if source_reload { 300 } else { 30 }),
    );

    client.wait_for_notification("notifications/tools/list_changed", Duration::from_secs(10));
    assert_eq!(client.child.id(), pid);
    for (index, fd) in [0, 1].iter().enumerate() {
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).expect("preserved pipe"),
            pipes[index]
        );
    }
    client
        .stdin
        .as_mut()
        .expect("open client stdin")
        .write_all(&partial.as_bytes()[split..])
        .expect("finish buffered request");
    client
        .stdin
        .as_mut()
        .expect("open client stdin")
        .write_all(b"\n")
        .expect("finish request line");
    client
        .stdin
        .as_mut()
        .expect("open client stdin")
        .flush()
        .expect("flush request");
    let retained = client.response(30, Duration::from_secs(10));
    let logged_sessions = call_tool(&mut client, 29, "session_list", json!({}));
    let rows = logged_sessions["result"]["structuredContent"]["sessions"]
        .as_array()
        .expect("sessions");
    assert_eq!(
        rows.iter()
            .find(|row| row["session_id"] == a_id)
            .expect("session A")["input_log"],
        a_log.to_str().expect("path")
    );
    assert_eq!(
        std::fs::read_to_string(&a_log).expect("input after reload"),
        log_before
    );

    if source_reload {
        client.send(60, "tools/list", json!({}));
        let tools = client.response(60, Duration::from_secs(10));
        assert!(
            tools.to_string().contains("after a private source rebuild"),
            "new tool definition was not published"
        );
    }
    assert!(retained.to_string().contains("reload-B"), "{retained}");
    save_session_image(&mut client, 31, &a_id, "session-a-after-reload.png");
    save_session_image(&mut client, 32, &b_id, "session-b-after-reload.png");
    for (request, id, response, title) in [(33, &a_id, &a, "reload-A"), (34, &b_id, &b, "reload-B")]
    {
        let input = call_tool(
            &mut client,
            request,
            "keyboard_type",
            json!({"session_id":id,"text":"-reattached"}),
        );
        assert!(input["error"].is_null(), "{input}");
        call_tool(
            &mut client,
            request + 10,
            "keyboard_key",
            json!({"session_id":id,"key":"Return"}),
        );
        assert_eq!(
            submitted_text(&workdir(response).join("submitted.txt")),
            format!("{title}-reattached\n")
        );
        call_tool(
            &mut client,
            request + 20,
            "session_stop",
            json!({"session_id":id}),
        );
        assert!(!workdir(response).exists());
    }
    client.stop_process();
}

#[test]
#[ignore = "requires private KWin sessions, bubblewrap, and input devices"]
fn transport_eof_cleans_every_owned_session() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let state_home =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kwin-mcp-eof-state-{nonce}"));
    let lifecycle_dir = state_home.join("kwin-mcp");
    std::fs::create_dir_all(&lifecycle_dir).expect("create shared lifecycle state");
    let lifecycle_log = lifecycle_dir.join("session-lifecycle.jsonl");
    let seed_record = format!(
        "{}\n",
        json!({
            "session_id": "s1-1",
            "server_pid": 1,
            "state": "shutdown_cleanup_finished",
            "reason": "server_shutdown",
            "finished_at_unix_seconds": 1,
            "workdir_cleanup": "removed",
        })
    );
    let mut seed_log = Vec::new();
    while seed_log.len() <= SESSION_LIFECYCLE_LOG_MAX_BYTES {
        seed_log.extend_from_slice(seed_record.as_bytes());
    }
    std::fs::write(&lifecycle_log, seed_log).expect("seed bounded lifecycle log");
    let state_home = state_home.to_string_lossy().into_owned();
    let extra_env = [("XDG_STATE_HOME", state_home.as_str())];
    let mut client = RpcClient::start_with_env(&extra_env);
    initialize(&mut client);
    let mut directories = Vec::new();
    let mut sessions = Vec::new();
    for request in [2, 3] {
        let response = call_tool(
            &mut client,
            request,
            "session_start",
            json!({"width":800,"height":600}),
        );
        assert_eq!(
            response["result"]["structuredContent"]["status"], "started",
            "{response}"
        );
        sessions.push(
            response["result"]["structuredContent"]["session_id"]
                .as_str()
                .expect("session ID")
                .to_owned(),
        );
        directories.push(workdir(&response));
    }
    let manually_stopped = call_tool(
        &mut client,
        4,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let manually_stopped_id = session_handle(&manually_stopped);
    let manually_stopped_workdir = workdir(&manually_stopped);
    sessions.push(manually_stopped_id.clone());
    directories.push(manually_stopped_workdir.clone());
    let stopped = call_tool(
        &mut client,
        5,
        "session_stop",
        json!({"session_id":manually_stopped_id}),
    );
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
    assert!(!manually_stopped_workdir.exists());
    drop(client.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    while client.child.try_wait().expect("server status").is_none() {
        assert!(
            Instant::now() < deadline,
            "server did not finish shutdown after EOF"
        );
        thread::sleep(Duration::from_millis(20));
    }
    for directory in directories {
        assert!(
            !directory.exists(),
            "session retained after EOF: {}",
            directory.display()
        );
    }
    assert!(
        std::fs::metadata(&lifecycle_log)
            .expect("read bounded lifecycle log metadata")
            .len()
            <= u64::try_from(SESSION_LIFECYCLE_LOG_MAX_BYTES).expect("lifecycle cap fits u64"),
        "session-lifecycle.jsonl exceeded its cap"
    );
    let mut replacement = RpcClient::start_with_env(&extra_env);
    initialize(&mut replacement);
    for (index, session_id) in sessions.iter().enumerate() {
        let request = 2 + u64::try_from(index).expect("index");
        let unavailable = call_tool(
            &mut replacement,
            request,
            "window_list",
            json!({"session_id":session_id}),
        );
        if index < 2 {
            assert_eq!(
                unavailable["error"]["data"]["previous_server_cleanup"],
                "shutdown_cleanup_finished",
                "{unavailable}"
            );
            let record = &unavailable["error"]["data"]["previous_server_cleanup_record"];
            assert_eq!(record["state"], "shutdown_cleanup_finished", "{record}");
            assert_eq!(record["reason"], "server_shutdown", "{record}");
            assert_eq!(
                record["session_id"].as_str(),
                Some(session_id.as_str()),
                "{record}"
            );
            assert_eq!(record["workdir_cleanup"], "removed", "{record}");
            assert!(
                unavailable["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("shutdown cleanup finished")),
                "{unavailable}"
            );
        } else {
            assert_eq!(
                unavailable["error"]["data"]["session_status"], "stopped",
                "{unavailable}"
            );
            assert_eq!(
                unavailable["error"]["data"]["session_end_reason"], "session_stop",
                "{unavailable}"
            );
            let record = &unavailable["error"]["data"]["previous_session_lifecycle_record"];
            assert_eq!(record["state"], "session_stopped", "{record}");
            assert_eq!(record["reason"], "session_stop", "{record}");
            assert_eq!(
                record["session_id"].as_str(),
                Some(session_id.as_str()),
                "{record}"
            );
        }
    }
    replacement.stop_process();
    std::fs::remove_dir_all(state_home).expect("remove shared lifecycle state");
}

#[test]
#[ignore = "requires private KWin sessions, input devices, and kdialog"]
fn starts_cannot_adopt_listed_sessions_and_each_caller_gets_a_new_desktop() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    client.send(2, "tools/list", json!({}));
    let metadata = client.response(2, Duration::from_secs(5));
    let tools = metadata["result"]["tools"].as_array().expect("tools");
    let start = tools
        .iter()
        .find(|tool| tool["name"] == "session_start")
        .expect("start tool");
    assert!(
        start["inputSchema"]["properties"]
            .get("session_id")
            .is_none()
    );
    let list = tools
        .iter()
        .find(|tool| tool["name"] == "session_list")
        .expect("list tool");
    assert!(
        list["description"]
            .as_str()
            .expect("description")
            .contains("other agents' sessions")
    );
    let a = call_tool(
        &mut client,
        3,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let a_id = session_handle(&a);
    let launched = call_tool(
        &mut client,
        4,
        "launch_app",
        json!({"session_id":a_id,"command":format!("kdialog --title caller-A --inputbox 'Caller A owns this desktop' > {}/caller.txt",workdir(&a).display())}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let listed = call_tool(&mut client, 5, "session_list", json!({}));
    let contents = &listed["result"]["structuredContent"];
    assert!(
        contents["ownership_notice"]
            .as_str()
            .expect("ownership notice")
            .contains("your own session_start calls")
    );
    let foreign = contents["sessions"][0]["session_id"]
        .as_str()
        .expect("listed session");
    assert!(contents["sessions"][0].get("owner").is_none());
    let refused = call_tool(
        &mut client,
        6,
        "session_start",
        json!({"session_id":foreign}),
    );
    assert!(
        refused["error"].is_object(),
        "listed session was adopted: {refused}"
    );
    assert!(refused.to_string().contains("session_id"), "{refused}");
    let b = call_tool(
        &mut client,
        7,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let b_id = session_handle(&b);
    assert_ne!(a_id, b_id);
    assert_ne!(workdir(&a), workdir(&b));
    let b_windows = call_tool(&mut client, 8, "window_list", json!({"session_id":b_id}));
    assert!(!b_windows.to_string().contains("caller-A"), "{b_windows}");
    call_tool(
        &mut client,
        9,
        "launch_app",
        json!({"session_id":b_id,"command":format!("kdialog --title caller-B --inputbox 'Caller B owns this desktop' > {}/caller.txt",workdir(&b).display())}),
    );
    for (request, id, text) in [(10, &a_id, "only-A"), (11, &b_id, "only-B")] {
        let typed = call_tool(
            &mut client,
            request,
            "keyboard_type",
            json!({"session_id":id,"text":text}),
        );
        assert!(typed["error"].is_null(), "{typed}");
    }
    save_session_image(&mut client, 12, &a_id, "caller-a.png");
    save_session_image(&mut client, 13, &b_id, "caller-b.png");
    for (request, id, response, expected) in
        [(14, &a_id, &a, "only-A\n"), (15, &b_id, &b, "only-B\n")]
    {
        call_tool(
            &mut client,
            request,
            "keyboard_key",
            json!({"session_id":id,"key":"Return"}),
        );
        assert_eq!(
            submitted_text(&workdir(response).join("caller.txt")),
            expected
        );
    }
    let listed = call_tool(&mut client, 16, "session_list", json!({}));
    let sessions = listed["result"]["structuredContent"]["sessions"]
        .as_array()
        .expect("sessions");
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .all(|session| session.get("owner").is_none())
    );
    assert!(
        listed["result"]["content"]
            .to_string()
            .contains("other agents' sessions")
    );
    for (request, id) in [(17, &a_id), (18, &b_id)] {
        call_tool(
            &mut client,
            request,
            "session_stop",
            json!({"session_id":id}),
        );
    }
    client.stop_process();
}

#[test]
#[ignore = "requires private KWin sessions, bubblewrap, input devices, and kdialog"]
fn input_events_are_logged_immediately_and_survive_session_stop() {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let state = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "input-log-state-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut client =
        RpcClient::start_with_env(&[("XDG_STATE_HOME", state.to_str().expect("state path"))]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let session = session_handle(&started);
    let log = PathBuf::from(
        started["result"]["structuredContent"]["input_log"]
            .as_str()
            .expect("input log path"),
    );
    assert!(log.starts_with(&state));
    assert_eq!(
        std::fs::metadata(&log)
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let listed = call_tool(&mut client, 3, "session_list", json!({}));
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"][0]["input_log"],
        log.to_str().expect("path")
    );
    let launched = call_tool(
        &mut client,
        4,
        "launch_app",
        json!({"session_id":session,"command":format!("kdialog --title input-log --inputbox 'Persistent input log' > {}/submitted.txt", workdir(&started).display())}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let operations = [
        (5, "mouse_move", json!({"x":30,"y":40})),
        (6, "mouse_click", json!({"x":30,"y":40})),
        (
            7,
            "mouse_drag",
            json!({"from_x":30,"from_y":40,"to_x":80,"to_y":40}),
        ),
        (
            8,
            "mouse_scroll",
            json!({"x":30,"y":40,"delta":2,"discrete":true}),
        ),
        (
            9,
            "mouse_scroll",
            json!({"x":30,"y":40,"delta":24,"horizontal":true}),
        ),
        (10, "keyboard_type", json!({"text":"Ab"})),
        (11, "keyboard_press", json!({"key":"Shift"})),
    ];
    for (id, tool, mut args) in operations {
        args["session_id"] = json!(session);
        let result = call_tool(&mut client, id, tool, args);
        assert!(
            result["error"].is_null() && result["result"]["isError"] != true,
            "{tool}: {result}"
        );
        let text = std::fs::read_to_string(&log).expect("live log");
        let latest: Value =
            serde_json::from_str(text.lines().last().expect("record")).expect("JSONL");
        assert_eq!(latest["type"], "call_summary");
        assert_eq!(latest["tool"]["request_id"], id);
    }
    thread::sleep(Duration::from_millis(50));
    let released = call_tool(
        &mut client,
        12,
        "keyboard_release",
        json!({"session_id":session,"key":"Shift"}),
    );
    assert!(released["error"].is_null(), "{released}");
    save_session_image(&mut client, 13, &session, "input-log.png");
    call_tool(
        &mut client,
        14,
        "keyboard_key",
        json!({"session_id":session,"key":"Return"}),
    );
    assert_eq!(
        submitted_text(&workdir(&started).join("submitted.txt")),
        "Ab\n"
    );
    let invalid = call_tool(
        &mut client,
        16,
        "keyboard_type",
        json!({"session_id":session,"text":"é"}),
    );
    assert!(
        invalid["error"].is_object() || invalid["result"]["isError"] == true,
        "{invalid}"
    );
    let records: Vec<Value> = std::fs::read_to_string(&log)
        .expect("log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL record"))
        .collect();
    let mut previous = 0;
    for record in &records {
        let mono = record["monotonic_ns"]
            .as_u64()
            .expect("monotonic timestamp");
        assert!(mono >= previous);
        previous = mono;
        assert!(
            record["wall_time_unix_ns"]
                .as_u64()
                .expect("wall timestamp")
                > 1_700_000_000_000_000_000
        );
        assert!(record["tool"]["name"].as_str().is_some());
        assert!(record["tool"]["request_id"].as_u64().is_some());
        if record["type"] == "event" {
            assert_eq!(record["delivery"], "flushed");
        }
        if record["event"] == "motion" {
            for (screen, local, axis) in [("screen_x", "window_x", 0), ("screen_y", "window_y", 1)]
            {
                assert_eq!(
                    record[screen].as_f64().expect("screen"),
                    record[local].as_f64().expect("local")
                        + record["window_origin"][axis].as_f64().expect("origin")
                );
            }
        }
    }
    let summary = |id: u64| {
        records
            .iter()
            .find(|r| r["type"] == "call_summary" && r["tool"]["request_id"] == id)
            .expect("summary")
    };
    assert_eq!(summary(16)["outcome"], "error");
    assert!(
        !records
            .iter()
            .any(|r| r["tool"]["request_id"] == 16 && r["type"] == "event")
    );
    let typing = &summary(10)["typing"];
    assert_eq!(typing["characters"], "Ab");
    assert_eq!(typing["character_count"], 2);
    assert_eq!(typing["keys"].as_array().expect("keys").len(), 3);
    for key in typing["keys"].as_array().expect("keys") {
        assert_eq!(
            key["hold_ns"].as_u64(),
            Some(key["up_ns"].as_u64().expect("up") - key["down_ns"].as_u64().expect("down"))
        );
    }
    assert!(summary(11)["typing"]["keys"][0]["up_ns"].is_null());
    assert!(
        summary(12)["typing"]["keys"][0]["hold_ns"]
            .as_u64()
            .expect("held duration")
            >= 50_000_000
    );
    for id in [5, 7] {
        let pointer = &summary(id)["pointer"];
        assert!(pointer["path_length_px"].as_f64().expect("path length") > 0.0);
        assert!(pointer["peak_speed_px_s"].as_f64().expect("speed") > 0.0);
        assert_eq!(
            pointer["duration_ns"].as_u64(),
            Some(
                pointer["end_ns"].as_u64().expect("end")
                    - pointer["start_ns"].as_u64().expect("start")
            )
        );
    }
    assert!(
        records
            .iter()
            .any(|r| r["event"] == "button" && r["pressed"] == true)
    );
    assert!(
        records
            .iter()
            .any(|r| r["event"] == "button" && r["pressed"] == false)
    );
    for (id, mode, unit) in [(8, "discrete", "value120"), (9, "smooth", "pixels")] {
        let scroll = &summary(id)["scroll"];
        let events: Vec<_> = records
            .iter()
            .filter(|r| r["tool"]["request_id"] == id && r["event"] == "scroll")
            .collect();
        assert_eq!(scroll["mode"], mode);
        assert_eq!(scroll["unit"], unit);
        assert_eq!(
            scroll["events"].as_u64(),
            Some(u64::try_from(events.len()).expect("event count"))
        );
        for (raw, total) in [("dx", "total_dx"), ("dy", "total_dy")] {
            assert_eq!(
                scroll[total].as_f64(),
                Some(
                    events
                        .iter()
                        .map(|r| r[raw].as_f64().expect("delta"))
                        .sum::<f64>()
                )
            );
        }
        assert!(scroll["events_per_second"].as_f64().expect("event rate") > 0.0);
    }
    call_tool(
        &mut client,
        15,
        "session_stop",
        json!({"session_id":session}),
    );
    assert!(!workdir(&started).exists(), "session workdir remains");
    client.stop_process();
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("persistent log")
            .lines()
            .count(),
        records.len()
    );
    println!(
        "PASS: {} timestamped records; pointer, buttons, typing holds/gaps, discrete/smooth scroll; log survives session stop and server exit",
        records.len()
    );
    std::fs::remove_dir_all(state).expect("remove private test state");
}

#[test]
#[ignore = "requires private KWin session and approximately two minutes of real input"]
fn human_input_matches_published_timing_profile_on_stdio() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let session = session_handle(&started);
    let log = PathBuf::from(
        started["result"]["structuredContent"]["input_log"]
            .as_str()
            .expect("log path"),
    );
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"session_id":session,"command":format!("kdialog --title human-input --inputbox 'Human input timing' > {}/submitted.txt", workdir(&started).display())}),
    );
    let read_records = || -> Vec<Value> {
        std::fs::read_to_string(&log)
            .expect("input log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSONL"))
            .collect()
    };
    let mut request = 10;
    let mut measured = Vec::new();
    for (distance, width, speed) in [
        (80.0_f64, 20.0_f64, 1.0_f64),
        (600.0, 20.0, 1.0),
        (600.0, 80.0, 1.0),
        (600.0, 20.0, 2.0),
    ] {
        let mut durations = Vec::new();
        for _ in 0..4 {
            call_tool(
                &mut client,
                request,
                "mouse_move",
                json!({"session_id":session,"x":50,"y":100,"speed":4}),
            );
            request += 1;
            let response = call_tool(
                &mut client,
                request,
                "mouse_move",
                json!({"session_id":session,"x":format!("{:.0}",50.0+distance),"y":100,"target_width":width,"speed":speed}),
            );
            assert!(response["error"].is_null(), "{response}");
            let records = read_records();
            let summary = records
                .iter()
                .find(|r| r["type"] == "call_summary" && r["tool"]["request_id"] == request)
                .expect("move summary");
            let pointer = &summary["pointer"];
            let duration = pointer["duration_ns"].as_f64().expect("duration") / 1e6;
            durations.push(duration);
            let events: Vec<_> = records
                .iter()
                .filter(|r| r["event"] == "motion" && r["tool"]["request_id"] == request)
                .collect();
            assert!(events.len() > 10, "pointer still jumps");
            assert_eq!(
                events.last().expect("last motion")["window_x"].as_f64(),
                Some(50.0 + distance)
            );
            assert_eq!(
                events.last().expect("last motion")["window_y"].as_f64(),
                Some(100.0)
            );
            assert!(
                events
                    .iter()
                    .any(|r| (r["window_y"].as_f64().expect("y") - 100.0).abs() > 0.01),
                "straight path"
            );
            let mut intervals: Vec<_> = events
                .windows(2)
                .map(|pair| {
                    (pair[1]["monotonic_ns"].as_f64().expect("time")
                        - pair[0]["monotonic_ns"].as_f64().expect("time"))
                        / 1e6
                })
                .collect();
            intervals.sort_by(f64::total_cmp);
            assert!(
                (6.0..30.0).contains(&intervals[intervals.len() / 2]),
                "report interval {intervals:?}"
            );
            let start_ns = pointer["start_ns"].as_f64().expect("start");
            let surge = if distance > 200.0 { 0.85 } else { 1.0 };
            let near_distance = if distance > 200.0 {
                distance - width.min(30.0) * 0.2
            } else {
                distance
            };
            let mut deviation = Vec::new();
            for event in &events {
                let t = (event["monotonic_ns"].as_f64().expect("time") - start_ns)
                    / (duration * 1e6 * surge);
                if (0.1..0.8).contains(&t) {
                    let expected = 10.0 * t.powi(3) - 15.0 * t.powi(4) + 6.0 * t.powi(5);
                    deviation.push(
                        ((event["window_x"].as_f64().expect("x") - 50.0) / near_distance
                            - expected)
                            .abs(),
                    );
                }
            }
            assert!(!deviation.is_empty());
            assert!(
                deviation.iter().sum::<f64>()
                    / f64::from(u32::try_from(deviation.len()).expect("samples"))
                    < 0.08,
                "minimum-jerk shape differs"
            );
            request += 1;
        }
        let mean = durations.iter().sum::<f64>() / 4.0;
        let expected = (230.0 + 166.0 * (distance / width + 1.0).log2()) / speed;
        assert!(
            (mean - expected).abs() < 160.0 / speed + 25.0,
            "Fitts mean {mean} vs {expected}"
        );
        println!(
            "pointer D={distance} W={width} speed={speed}: mean={mean:.1} ms, published fit={expected:.1} ms"
        );
        measured.push(mean);
    }
    assert!(
        measured[1] > measured[0] && measured[1] > measured[2] && measured[1] > measured[3] * 1.5
    );
    for (id, discrete, delta, reports, total) in [
        (60, true, 4, 4, 480.0),
        // Smooth totals are axis units, 12 pixels each.
        (61, false, 600, 5, 50.0),
        (62, false, -9000, 32, -750.0),
    ] {
        let started = std::time::Instant::now();
        let result = call_tool(
            &mut client,
            id,
            "mouse_scroll",
            json!({"session_id":session,"x":100,"y":100,"delta":delta,"discrete":discrete}),
        );
        assert!(result["error"].is_null(), "{result}");
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        let records = read_records();
        let events: Vec<_> = records
            .iter()
            .filter(|r| r["event"] == "scroll" && r["tool"]["request_id"] == id)
            .collect();
        assert_eq!(events.len(), reports);
        assert!(
            (events
                .iter()
                .map(|r| r["dy"].as_f64().expect("delta"))
                .sum::<f64>()
                - total)
                .abs()
                < 1e-3
        );
        assert!(
            events.last().expect("last")["monotonic_ns"]
                .as_u64()
                .expect("time")
                - events[0]["monotonic_ns"].as_u64().expect("time")
                >= (u64::try_from(reports).expect("reports") - 1) * 16_000_000
        );
    }
    let text = "the little red fox sees the green tree and the blue sky. ".repeat(5);
    let profile: Value = serde_json::from_str(include_str!("../data/input-profile.json"))
        .expect("published sample fit");
    let mut typing_durations = Vec::new();
    for (id, speed) in [(70, 1.0), (72, 2.0)] {
        call_tool(
            &mut client,
            id - 1,
            "keyboard_key",
            json!({"session_id":session,"key":"ctrl+a","speed":speed}),
        );
        client.send(id,"tools/call",json!({"name":"keyboard_type","arguments":{"session_id":session,"text":text,"speed":speed}}));
        let response = client.response(id, Duration::from_secs(90));
        assert!(response["error"].is_null(), "{response}");
        let records = read_records();
        let summary = records
            .iter()
            .find(|r| r["type"] == "call_summary" && r["tool"]["request_id"] == id)
            .expect("typing summary");
        assert_eq!(summary["typing"]["characters"], text);
        let keys = summary["typing"]["keys"].as_array().expect("key timings");
        assert_eq!(keys.len(), text.len());
        let mut hold_z = Vec::new();
        let mut iki_z = Vec::new();
        let mut overlap = 0_u32;
        for pair in keys.windows(2) {
            let bigram = format!(
                "{}{}",
                pair[0]["character"].as_str().expect("char"),
                pair[1]["character"].as_str().expect("char")
            );
            let stats = profile["bigrams"]
                .get(&bigram)
                .unwrap_or(&profile["bigrams"]["*"]);
            let hold = pair[0]["hold_ns"].as_f64().expect("hold") / 1e6 * speed;
            let iki = (pair[1]["down_ns"].as_f64().expect("down")
                - pair[0]["down_ns"].as_f64().expect("down"))
                / 1e6
                * speed;
            hold_z.push(
                (hold.ln() - stats[1].as_f64().expect("mean")) / stats[2].as_f64().expect("sd"),
            );
            iki_z.push(
                (iki.ln() - stats[3].as_f64().expect("mean")) / stats[4].as_f64().expect("sd"),
            );
            if pair[1]["down_ns"].as_u64() < pair[0]["up_ns"].as_u64() {
                overlap += 1;
                assert!(pair[1]["gap_before_ns"].as_i64().expect("signed gap") < 0);
            }
        }
        for (name, z) in [("hold", hold_z), ("interval", iki_z)] {
            let n = f64::from(u32::try_from(z.len()).expect("sample count"));
            let mean = z.iter().sum::<f64>() / n;
            let second = z.iter().map(|v| v * v).sum::<f64>() / n;
            assert!(
                mean.abs() < 0.35 && (0.5..1.8).contains(&second),
                "{name} fitted log-z mean={mean} second moment={second}"
            );
            println!(
                "typing speed={speed} {name}: log-z mean={mean:.3}, second moment={second:.3}"
            );
        }
        assert!(overlap > 20, "rollover missing");
        let duration = summary["typing"]["duration_ns"].as_f64().expect("duration") / 1e9;
        let wpm = f64::from(u32::try_from(text.len()).expect("characters")) / 5.0 / duration * 60.0;
        println!("typing speed={speed}: {wpm:.1} WPM, {overlap} overlapping pairs");
        assert!((70.0 * speed..140.0 * speed).contains(&wpm));
        typing_durations.push(duration);
    }
    assert!(typing_durations[0] > typing_durations[1] * 1.5);
    save_session_image(&mut client, 80, &session, "human-input.png");
    call_tool(
        &mut client,
        81,
        "keyboard_key",
        json!({"session_id":session,"key":"Return"}),
    );
    assert_eq!(
        submitted_text(&workdir(&started).join("submitted.txt")),
        format!("{text}\n")
    );
    let invalid = call_tool(
        &mut client,
        82,
        "mouse_move",
        json!({"session_id":session,"x":5,"y":5,"speed":0}),
    );
    assert!(invalid["error"].is_object());
    assert!(
        !read_records()
            .iter()
            .any(|r| r["type"] == "event" && r["tool"]["request_id"] == 82)
    );
    call_tool(
        &mut client,
        83,
        "session_stop",
        json!({"session_id":session}),
    );
    client.stop_process();
}

#[test]
#[ignore = "requires private KWin desktops, Chrome, PipeWire, and Vulkan viewer rendering"]
fn viewer_displays_current_pixels_after_a_second_fresh_session() {
    use base64::Engine;

    const WIDTH: u32 = 1440;
    const HEIGHT: u32 = 900;
    const SOURCE_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
    const VIEWER_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
    const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(100);
    const COLOR_TOLERANCE: u8 = 16;

    fn tool(client: &mut RpcClient, request: &mut u64, name: &str, args: Value) -> Value {
        let response = call_tool(client, *request, name, args);
        *request += 1;
        response
    }

    fn matches_palette(response: &Value, colors: &[[u8; 3]; 4]) -> bool {
        if inline_png(response).is_empty() {
            return false;
        }
        let image = decode_rgba(response);
        let height = u32::try_from(image.1.len() / 4 / usize::try_from(image.0).expect("width"))
            .expect("height");
        [(1, 1), (3, 1), (1, 3), (3, 3)]
            .into_iter()
            .zip(colors)
            .all(|((x, y), expected)| {
                let actual = pixel(&image, image.0 * x / 4, height * y / 4);
                actual[..3]
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.abs_diff(*expected) <= COLOR_TOLERANCE)
            })
    }

    let save = |response: &Value, name: &str| {
        if let Some(root) = std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR") {
            let root = PathBuf::from(root);
            std::fs::create_dir_all(&root).expect("screenshot artifact directory");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(inline_png(response))
                .expect("screenshot artifact base64");
            std::fs::write(root.join(name), bytes).expect("save screenshot artifact");
        }
    };

    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    // The viewer's host is another isolated desktop, so its actual composited
    // pixels can be checked through MCP without capturing the user's desktop.
    let mut desktop = RpcClient::start();
    initialize(&mut desktop);
    let host = call_tool(
        &mut desktop,
        2,
        "session_start",
        json!({"width":WIDTH,"height":HEIGHT}),
    );
    let host_id = session_handle(&host);
    let display = workdir(&host).join("wayland-0");
    assert!(display.exists(), "private viewer display missing");
    // Force PipeWire to become ready after WirePlumber would otherwise start.
    // Both are still real daemons; only their startup ordering is controlled.
    let wrappers = fixture_temp_root().join(format!("km-viewer-media-{}", std::process::id()));
    std::fs::create_dir(&wrappers).expect("media wrapper directory");
    let _wrappers = RemoveDirOnDrop(wrappers.clone());
    use std::os::unix::fs::PermissionsExt;
    for (name, script) in [
        (
            "pipewire",
            "#!/bin/bash\nsleep 1\nexec /usr/bin/pipewire \"$@\"\n",
        ),
        (
            "wireplumber",
            "#!/bin/bash\ntest -S \"$XDG_RUNTIME_DIR/pipewire-0\" || { echo 'policy started before PipeWire socket' >&2; exit 98; }\nexec /usr/bin/wireplumber \"$@\"\n",
        ),
    ] {
        let path = wrappers.join(name);
        std::fs::write(&path, script).expect("media startup wrapper");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("executable wrapper");
    }
    let path = format!(
        "{}:{}",
        wrappers.display(),
        std::env::var("PATH").expect("PATH")
    );
    let mut client = RpcClient::start_with_env(&[
        (
            "WAYLAND_DISPLAY",
            display.to_str().expect("private display"),
        ),
        ("PATH", &path),
    ]);
    initialize(&mut client);
    let server_pid = client.child.id();
    let mut request = 2;
    let mut host_request = 3;
    let mut previous_session = None;
    let palettes = [
        [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]],
        [[0, 255, 255], [255, 0, 255], [255, 255, 0], [0, 0, 255]],
    ];
    for (round, colors) in palettes.iter().enumerate() {
        let fixture = client.home.join("viewer-frame.html");
        let swatches =
            colors.map(|[r, g, b]| format!("<div style='background:rgb({r},{g},{b})'></div>"));
        std::fs::write(
            &fixture,
            format!(
                "<!doctype html><style>html,body{{margin:0;width:100%;height:100%;overflow:hidden}}\
                 body{{display:grid;grid-template:1fr 1fr/1fr 1fr}}\
                 p{{position:absolute;top:45%;left:30%;background:white;padding:12px;font:28px sans-serif}}</style>\
                 {}<p>Fresh session {}</p>",
                swatches.join(""),
                round + 1
            ),
        )
        .expect("write private browser fixture");
        let started = tool(
            &mut client,
            &mut request,
            "session_start",
            json!({"width":WIDTH,"height":HEIGHT}),
        );
        let session = session_handle(&started);
        assert_ne!(previous_session.as_ref(), Some(&session));
        assert_eq!(client.child.id(), server_pid, "MCP server was restarted");
        assert_eq!(
            started["result"]["structuredContent"]["viewer"]["state"], "closed",
            "{started}"
        );
        let launched = tool(
            &mut client,
            &mut request,
            "launch_app",
            json!({"session_id":session,"command":format!(
                "google-chrome-stable --user-data-dir=/tmp/viewer-fresh-{round} --no-first-run --no-default-browser-check --app='file://{}'",
                fixture.display()
            )}),
        );
        assert!(
            !launched["result"]["isError"].as_bool().unwrap_or(false),
            "{launched}"
        );
        let deadline = Instant::now() + SOURCE_FRAME_TIMEOUT;
        loop {
            let source = tool(
                &mut client,
                &mut request,
                "screenshot",
                json!({"session_id":session,"inline":true}),
            );
            if matches_palette(&source, colors) {
                let meta = screenshot_meta(&source);
                assert_eq!(meta["width"], WIDTH, "{meta}");
                assert_eq!(meta["height"], HEIGHT, "{meta}");
                save(&source, &format!("viewer-fresh-{}-source.png", round + 1));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "browser fixture pixels did not appear"
            );
            thread::sleep(FRAME_POLL_INTERVAL);
        }

        let open_request = request;
        request += 1;
        client.send(
            open_request,
            "tools/call",
            json!({"name":"viewer_open","arguments":{"session_id":session}}),
        );
        let expected_title = format!("kwin-viewer {session}");
        let deadline = Instant::now() + VIEWER_FRAME_TIMEOUT;
        let mut displayed = false;
        let mut saved_first = false;
        while Instant::now() < deadline {
            let windows = tool(
                &mut desktop,
                &mut host_request,
                "window_list",
                json!({"session_id":host_id}),
            );
            let viewers: Vec<_> = windows["result"]["structuredContent"]["windows"]
                .as_array()
                .expect("private desktop windows")
                .iter()
                .filter(|window| window["title"] == expected_title)
                .collect();
            assert!(viewers.len() <= 1, "duplicate viewer windows: {viewers:?}");
            if let Some(viewer) = viewers.first() {
                tool(
                    &mut desktop,
                    &mut host_request,
                    "window_activate",
                    json!({"session_id":host_id,"window_id":viewer["id"]}),
                );
                let captured = tool(
                    &mut desktop,
                    &mut host_request,
                    "screenshot",
                    json!({"session_id":host_id,"inline":true}),
                );
                if !inline_png(&captured).is_empty() && !saved_first {
                    save(
                        &captured,
                        &format!("viewer-fresh-{}-initial.png", round + 1),
                    );
                    saved_first = true;
                }
                if matches_palette(&captured, colors) {
                    save(
                        &captured,
                        &format!("viewer-fresh-{}-displayed.png", round + 1),
                    );
                    displayed = true;
                    break;
                }
            }
            thread::sleep(FRAME_POLL_INTERVAL);
        }
        let opened = client.response(open_request, Duration::from_secs(10));
        let log = std::fs::read_to_string(workdir(&started).join("viewer.log")).unwrap_or_default();
        if opened["result"]["structuredContent"]["viewer"]["state"] != "ready"
            && let Some(root) = std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR")
        {
            let root = PathBuf::from(root);
            let runtime = workdir(&started);
            let sandbox = client
                .home
                .join(".cache/kwin-mcp")
                .join(runtime.file_name().expect("workdir name"))
                .join("sandbox.log");
            let _ = std::fs::copy(sandbox, root.join("failed-sandbox.log"));
            let _ = std::fs::write(root.join("failed-viewer.log"), &log);
            if let Ok(graph) = Command::new("timeout")
                .args(["3", "pw-dump", "--remote"])
                .arg(runtime.join("pipewire-0"))
                .output()
            {
                let _ = std::fs::write(root.join("failed-pipewire.json"), graph.stdout);
                let _ = std::fs::write(root.join("failed-pipewire.stderr"), graph.stderr);
            }
        }
        assert_eq!(
            opened["result"]["structuredContent"]["viewer"]["state"],
            "ready",
            "fresh session {}: {opened}\n{log}",
            round + 1
        );
        assert!(
            displayed,
            "fresh session {}: viewer did not display current session pixels\n{log}",
            round + 1
        );
        let pid = u32::try_from(
            opened["result"]["structuredContent"]["viewer"]["pid"]
                .as_u64()
                .expect("viewer PID"),
        )
        .expect("PID fits");
        let stopped = tool(
            &mut client,
            &mut request,
            "session_stop",
            json!({"session_id":session}),
        );
        assert_eq!(
            stopped["result"]["structuredContent"]["status"], "stopped",
            "{stopped}"
        );
        assert!(
            !process_alive(pid).unwrap_or(true),
            "stopped session left a viewer process"
        );
        let windows = tool(
            &mut desktop,
            &mut host_request,
            "window_list",
            json!({"session_id":host_id}),
        );
        assert!(
            !windows["result"]["structuredContent"]["windows"]
                .as_array()
                .expect("private desktop windows")
                .iter()
                .any(|window| window["title"] == expected_title),
            "stopped session left its viewer window: {windows}"
        );
        previous_session = Some(session);
        println!(
            "PASS: fresh session {} displayed its current palette and cleaned up",
            round + 1
        );
    }
    client.stop_process();
    tool(
        &mut desktop,
        &mut host_request,
        "session_stop",
        json!({"session_id":host_id}),
    );
    desktop.stop_process();
}

#[test]
#[ignore = "requires three private KWin sessions and Vulkan viewer rendering"]
fn viewer_titles_identify_sessions_and_update_reasons_in_private_desktop() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut desktop = RpcClient::start();
    initialize(&mut desktop);
    let host = call_tool(
        &mut desktop,
        2,
        "session_start",
        json!({"width":1024,"height":768}),
    );
    let host_id = session_handle(&host);
    let display = workdir(&host).join("wayland-0");
    assert!(display.exists(), "private viewer display missing");
    let mut client = RpcClient::start_with_env(&[(
        "WAYLAND_DISPLAY",
        display.to_str().expect("private display"),
    )]);
    initialize(&mut client);
    let a = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let b = call_tool(
        &mut client,
        3,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let a_id = session_handle(&a);
    let b_id = session_handle(&b);
    for (request, id, title) in [(4, &a_id, "Viewer A"), (5, &b_id, "Viewer B")] {
        call_tool(
            &mut client,
            request,
            "launch_app",
            json!({"session_id":id,"command":format!("kdialog --title '{title}' --inputbox '{title} content'")}),
        );
    }
    let reason = "tick \"I am human\", then Continue shopping";
    let first = call_tool(
        &mut client,
        6,
        "viewer_open",
        json!({"session_id":a_id,"reason":reason}),
    );
    assert_ne!(
        first["result"]["structuredContent"]["viewer"]["state"], "unavailable",
        "{first}"
    );
    let second = call_tool(&mut client, 7, "viewer_open", json!({"session_id":b_id}));
    assert_ne!(
        second["result"]["structuredContent"]["viewer"]["state"], "unavailable",
        "{second}"
    );
    let windows = call_tool(
        &mut desktop,
        3,
        "window_list",
        json!({"session_id":host_id}),
    );
    println!(
        "private viewer windows: {}",
        windows["result"]["structuredContent"]
    );
    call_tool(
        &mut desktop,
        5,
        "keyboard_press",
        json!({"session_id":host_id,"key":"alt+Tab"}),
    );
    save_session_image(&mut desktop, 4, &host_id, "viewer-titles.png");
    call_tool(
        &mut desktop,
        6,
        "keyboard_release",
        json!({"session_id":host_id,"key":"alt+Tab"}),
    );
    let expected_a = format!("kwin-viewer {a_id}: {reason}");
    let expected_b = format!("kwin-viewer {b_id}");
    let rows = windows["result"]["structuredContent"]["windows"]
        .as_array()
        .expect("windows");
    assert!(
        rows.iter().any(|row| row["title"] == expected_a),
        "missing title {expected_a}: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row["title"] == expected_b),
        "missing title {expected_b}: {rows:?}"
    );
    let first_pid = first["result"]["structuredContent"]["viewer"]["pid"].clone();
    let updated = call_tool(
        &mut client,
        8,
        "viewer_open",
        json!({"session_id":a_id,"reason":"Enter the verification code"}),
    );
    assert_eq!(
        updated["result"]["structuredContent"]["viewer"]["pid"],
        first_pid
    );
    assert_eq!(
        updated["result"]["structuredContent"]["status"],
        "already_open"
    );
    let expected_updated = format!("kwin-viewer {a_id}: Enter the verification code");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = 10;
    loop {
        let windows = call_tool(
            &mut desktop,
            request,
            "window_list",
            json!({"session_id":host_id}),
        );
        request += 1;
        if windows["result"]["structuredContent"]["windows"]
            .as_array()
            .expect("windows")
            .iter()
            .any(|row| row["title"] == expected_updated)
        {
            break;
        }
        assert!(Instant::now() < deadline, "title did not update: {windows}");
        thread::sleep(Duration::from_millis(100));
    }
    println!(
        "PASS: distinct session titles; quoted reason; existing viewer PID retained while title changes"
    );
    for (request, id) in [(20, a_id), (21, b_id)] {
        call_tool(
            &mut client,
            request,
            "session_stop",
            json!({"session_id":id}),
        );
    }
    client.stop_process();
    call_tool(
        &mut desktop,
        request + 1,
        "session_stop",
        json!({"session_id":host_id}),
    );
    desktop.stop_process();
}

#[test]
#[ignore = "requires private KWin sessions, PipeWire, WirePlumber, and Vulkan"]
fn viewer_reports_stalled_pipewire_as_failed_and_recovers() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut desktop = RpcClient::start();
    initialize(&mut desktop);
    let host = call_tool(
        &mut desktop,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let host_id = session_handle(&host);
    let display = workdir(&host).join("wayland-0");
    assert!(display.exists());
    let mut client = RpcClient::start_with_env(&[(
        "WAYLAND_DISPLAY",
        display.to_str().expect("private display"),
    )]);
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let session = session_handle(&started);
    let path = workdir(&started);
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"session_id":session,"command":"kdialog --title pipewire-recovery --inputbox 'Recovered private viewer'"}),
    );
    struct GraphMonitor(Child);
    impl Drop for GraphMonitor {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let graph_path = path.join("private-pipewire-graph.json");
    let graph_errors = path.join("private-pipewire-graph.stderr");
    let _monitor = GraphMonitor(
        Command::new("pw-dump")
            .env("HOME", &client.home)
            .env("XDG_CONFIG_HOME", client.home.join(".config"))
            .env("XDG_RUNTIME_DIR", &path)
            .args(["--monitor", "--remote"])
            .arg(path.join("pipewire-0"))
            .stdout(Stdio::from(
                std::fs::File::create(&graph_path).expect("graph file"),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(&graph_errors).expect("graph errors"),
            ))
            .spawn()
            .expect("monitor private graph"),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while std::fs::metadata(&graph_path).expect("graph file").len() == 0 {
        assert!(
            Instant::now() < deadline,
            "private graph monitor did not initialize: {}",
            std::fs::read_to_string(&graph_errors).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let policy = child_named_process(client.child.id(), "wireplumber");
    let resume = ResumeProcess(nix::unistd::Pid::from_raw(policy));
    nix::sys::signal::kill(resume.0, nix::sys::signal::Signal::SIGSTOP)
        .expect("stop owned private policy manager");
    let began = Instant::now();
    client.send(4,"tools/call",json!({"name":"viewer_open","arguments":{"session_id":session,"reason":"Verify the picture is visible"}}));
    let deadline = Instant::now() + Duration::from_secs(3);
    let node = loop {
        let log = std::fs::read_to_string(path.join("viewer.log")).unwrap_or_default();
        if log.contains("pw stream Unconnected -> Connecting") {
            if let Some(id) = log.lines().find_map(|line| {
                line.strip_prefix("kwin-viewer: connected to pipewire node ")
                    .and_then(|id| id.parse::<u64>().ok())
            }) {
                break id;
            }
        }
        assert!(
            Instant::now() < deadline,
            "private stream did not reach Connecting: {log}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let objects = loop {
        let text = std::fs::read_to_string(&graph_path).expect("graph snapshot");
        let values: Vec<Value> = serde_json::Deserializer::from_str(&text)
            .into_iter::<Value>()
            .map_while(Result::ok)
            .collect();
        let objects: Vec<Value> = values
            .into_iter()
            .filter_map(|value| value.as_array().cloned())
            .flatten()
            .collect();
        if objects
            .iter()
            .any(|object| object["id"] == node && object["type"] == "PipeWire:Interface:Node")
        {
            break objects;
        }
        assert!(
            Instant::now() < deadline,
            "current screencast node missing in private graph"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let target = objects
        .iter()
        .find(|object| object["id"] == node)
        .expect("live screencast node");
    let props = &target["info"]["props"];
    let owner = objects
        .iter()
        .find(|object| object["id"] == props["client.id"])
        .expect("node client");
    assert_eq!(
        owner["info"]["props"]["application.process.binary"],
        "kwin_wayland"
    );
    println!(
        "Connecting target: live node {node}, serial {}, owner {}",
        props["object.serial"], owner["info"]["props"]["application.process.binary"]
    );
    let connecting: Value = serde_json::from_str(
        &std::fs::read_to_string(path.join("viewer-status.json")).expect("connecting status"),
    )
    .expect("JSON status");
    assert_eq!(
        connecting["state"], "starting",
        "premature streaming status: {connecting}"
    );
    thread::sleep(Duration::from_millis(150));
    save_session_image(&mut desktop, 9, &host_id, "viewer-stalled.png");
    let result = client.response(4, Duration::from_secs(10));
    println!("viewer result: {}", result["result"]["structuredContent"]);
    assert_eq!(
        result["result"]["structuredContent"]["viewer"]["state"], "unavailable",
        "{result}"
    );
    assert!(result.to_string().contains("Connecting"), "{result}");
    assert!(began.elapsed() < Duration::from_secs(8));
    let status: Value = serde_json::from_str(
        &std::fs::read_to_string(path.join("viewer-status.json")).expect("status file"),
    )
    .expect("status JSON");
    assert_eq!(status["state"], "failed");
    assert!(
        status["detail"]
            .as_str()
            .expect("failure detail")
            .contains("Connecting")
    );
    drop(resume);
    let ready = call_tool(
        &mut client,
        5,
        "viewer_open",
        json!({"session_id":session,"reason":"Confirm recovery"}),
    );
    assert_eq!(
        ready["result"]["structuredContent"]["viewer"]["state"], "ready",
        "{ready}"
    );
    let log = std::fs::read_to_string(path.join("viewer.log")).expect("viewer log");
    assert!(
        log.contains("first frame submitted for presentation"),
        "{log}"
    );
    thread::sleep(Duration::from_millis(400));
    save_session_image(&mut desktop, 3, &host_id, "viewer-recovered.png");
    let reused = call_tool(&mut client, 7, "viewer_open", json!({"session_id":session}));
    assert_eq!(
        reused["result"]["structuredContent"]["status"],
        "already_open"
    );
    assert_eq!(
        reused["result"]["structuredContent"]["viewer"]["pid"],
        ready["result"]["structuredContent"]["viewer"]["pid"]
    );
    call_tool(
        &mut client,
        6,
        "session_stop",
        json!({"session_id":session}),
    );
    client.stop_process();
    call_tool(
        &mut desktop,
        4,
        "session_stop",
        json!({"session_id":host_id}),
    );
    desktop.stop_process();
    println!(
        "PASS: live KWin node stalled while its policy manager was suspended; bounded unavailable/failed result with Connecting; retry presents a frame"
    );
}

#[test]
#[ignore = "requires private KWin desktops, kdialog, wl-paste, and xterm"]
fn clipboard_tools_separate_desktops_and_keep_generated_values_out_of_responses() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut desktop = RpcClient::start();
    initialize(&mut desktop);
    let host = call_tool(
        &mut desktop,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let host_id = session_handle(&host);
    let display = workdir(&host).join("wayland-0");
    let mut client = RpcClient::start_with_env(&[
        (
            "WAYLAND_DISPLAY",
            display.to_str().expect("private display"),
        ),
        ("KWIN_MCP_E2E_COPY_BINARY", "1"),
    ]);
    std::fs::copy(
        env!("CARGO_BIN_EXE_kwin-viewer"),
        client.home.join("kwin-viewer"),
    )
    .expect("private viewer executable");
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let id = session_handle(&started);
    let dir = workdir(&started);
    let mut request = 10;
    let mut run = |client: &mut RpcClient, name: &str, arguments: Value| {
        request += 1;
        let response = call_tool(client, request, name, arguments);
        assert!(response["error"].is_null(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        response["result"]["structuredContent"].clone()
    };
    let agent_text = "session café ✓";
    assert_eq!(
        run(
            &mut client,
            "clipboard_write",
            json!({"session_id":id,"target":"session","text":agent_text})
        ),
        json!({"target":"session","length":14})
    );
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id}))["text"],
        agent_text
    );
    let long_command = format!("{}printf 'paste probe';", "echo x;".repeat(82));
    assert_eq!(long_command.chars().count(), 595);
    assert_eq!(
        run(
            &mut client,
            "clipboard_write",
            json!({"session_id":id,"target":"session","text":&long_command})
        ),
        json!({"target":"session","length":595})
    );
    let xterm_command = format!(
        "xterm -e sh -c 'IFS= read -r pasted; printf \"%s\\n\" \"$pasted\" > \"{}/xterm.txt\"'",
        dir.display()
    );
    run(
        &mut client,
        "launch_app",
        json!({"session_id":id,"command":xterm_command}),
    );
    let paste_started = Instant::now();
    run(
        &mut client,
        "keyboard_key",
        json!({"session_id":id,"key":"shift+Insert"}),
    );
    run(
        &mut client,
        "keyboard_key",
        json!({"session_id":id,"key":"Return"}),
    );
    assert_eq!(
        submitted_text(&dir.join("xterm.txt")),
        format!("{long_command}\n")
    );
    assert!(
        paste_started.elapsed() < Duration::from_secs(5),
        "595-character XTerm paste exceeded five seconds"
    );
    run(
        &mut client,
        "clipboard_write",
        json!({"session_id":id,"target":"session","text":agent_text}),
    );
    run(
        &mut client,
        "launch_app",
        json!({"session_id":id,"command":format!("kdialog --title 'Session clipboard' --inputbox 'Paste session text' > {}/agent.txt",dir.display())}),
    );
    run(
        &mut client,
        "keyboard_key",
        json!({"session_id":id,"key":"ctrl+v"}),
    );
    save_session_image(&mut client, 1000, &id, "clipboard-session.png");
    run(
        &mut client,
        "keyboard_key",
        json!({"session_id":id,"key":"Return"}),
    );
    assert_eq!(
        submitted_text(&dir.join("agent.txt")),
        format!("{agent_text}\n")
    );

    run(
        &mut client,
        "clipboard_write",
        json!({"session_id":id,"target":"host","text":"host paste"}),
    );
    assert_eq!(
        run(
            &mut desktop,
            "clipboard_read",
            json!({"session_id":host_id})
        )["text"],
        "host paste"
    );
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id}))["text"],
        agent_text
    );
    run(
        &mut client,
        "launch_app",
        json!({"session_id":id,"command":format!("kdialog --title 'Viewer clipboard' --inputbox 'Paste through viewer' > {}/viewer.txt",dir.display())}),
    );
    let viewer = run(
        &mut client,
        "viewer_open",
        json!({"session_id":id,"reason":"Private clipboard test"}),
    );
    assert_eq!(viewer["viewer"]["state"], "ready", "{viewer}");
    run(
        &mut desktop,
        "keyboard_key",
        json!({"session_id":host_id,"key":"ctrl+v"}),
    );
    thread::sleep(Duration::from_millis(250));
    save_session_image(&mut client, 1001, &id, "clipboard-viewer.png");
    run(
        &mut client,
        "clipboard_write",
        json!({"session_id":id,"target":"host","text":"replace on copy"}),
    );
    // Let the viewer's existing tool-input idle gate open before user input.
    thread::sleep(Duration::from_millis(2100));
    run(
        &mut desktop,
        "keyboard_key",
        json!({"session_id":host_id,"key":"ctrl+a"}),
    );
    run(
        &mut desktop,
        "keyboard_key",
        json!({"session_id":host_id,"key":"ctrl+c"}),
    );
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        run(
            &mut desktop,
            "clipboard_read",
            json!({"session_id":host_id})
        )["text"],
        "host paste"
    );
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id}))["text"],
        agent_text
    );
    run(
        &mut desktop,
        "keyboard_key",
        json!({"session_id":host_id,"key":"Return"}),
    );
    assert_eq!(submitted_text(&dir.join("viewer.txt")), "host paste\n");

    let secret =
        json!({"length":32,"character_classes":["lowercase","uppercase","digits","symbols"]});
    let result = run(
        &mut client,
        "clipboard_write",
        json!({"session_id":id,"target":"host","generate_secret":secret}),
    );
    assert_eq!(result, json!({"target":"host","length":32}));
    assert_eq!(
        run(
            &mut desktop,
            "clipboard_read",
            json!({"session_id":host_id})
        ),
        json!({"target":"session","length":32,"text":null,"protected":true})
    );
    run(
        &mut client,
        "launch_app",
        json!({"session_id":id,"command":format!("kdialog --title 'Secret paste' --password 'Paste generated secret' > {}/secret.txt",dir.display())}),
    );
    thread::sleep(Duration::from_millis(2100));
    run(
        &mut desktop,
        "keyboard_key",
        json!({"session_id":host_id,"key":"shift+Insert"}),
    );
    thread::sleep(Duration::from_millis(250));
    save_session_image(&mut client, 1002, &id, "clipboard-secret.png");
    run(
        &mut desktop,
        "keyboard_key",
        json!({"session_id":host_id,"key":"Return"}),
    );
    let generated = submitted_text(&dir.join("secret.txt"));
    let generated = generated.trim_end_matches('\n');
    assert!(
        generated.len() == 32
            && generated.bytes().any(|b| b.is_ascii_lowercase())
            && generated.bytes().any(|b| b.is_ascii_uppercase())
            && generated.bytes().any(|b| b.is_ascii_digit())
            && generated.bytes().any(|b| b.is_ascii_punctuation()),
        "generated value must satisfy requested length and classes"
    );
    let result = run(
        &mut client,
        "clipboard_write",
        json!({"session_id":id,"target":"session","generate_secret":secret}),
    );
    assert_eq!(result, json!({"target":"session","length":32}));
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id})),
        json!({"target":"session","length":32,"text":null,"protected":true})
    );

    // The server's anonymous reload state must preserve both clipboard owners.
    let binary = client.binary_copy.as_ref().expect("private executable");
    let replacement = binary.with_extension("next");
    std::fs::copy(binary, &replacement).expect("stage executable");
    std::fs::rename(replacement, binary).expect("replace executable");
    client.wait_for_stderr("reload: reattached 1 sessions", Duration::from_secs(30));
    client.wait_for_notification("notifications/tools/list_changed", Duration::from_secs(10));
    for (connection, session) in [(&mut client, &id), (&mut desktop, &host_id)] {
        assert_eq!(
            run(connection, "clipboard_read", json!({"session_id":session})),
            json!({"target":"session","length":32,"text":null,"protected":true})
        );
    }
    let xterm_secret_command = format!(
        "xterm -e sh -c 'IFS= read -r pasted; printf \"%s\\n\" \"$pasted\" > \"{}/xterm-secret.txt\"'",
        dir.display()
    );
    run(
        &mut client,
        "launch_app",
        json!({"session_id":id,"command":xterm_secret_command}),
    );
    run(
        &mut client,
        "keyboard_key",
        json!({"session_id":id,"key":"shift+Insert"}),
    );
    run(
        &mut client,
        "keyboard_key",
        json!({"session_id":id,"key":"Return"}),
    );
    let restored_secret = submitted_text(&dir.join("xterm-secret.txt"));
    let restored_secret = restored_secret.trim_end_matches('\n');
    assert!(
        restored_secret.len() == 32
            && restored_secret.bytes().any(|b| b.is_ascii_lowercase())
            && restored_secret.bytes().any(|b| b.is_ascii_uppercase())
            && restored_secret.bytes().any(|b| b.is_ascii_digit())
            && restored_secret.bytes().any(|b| b.is_ascii_punctuation()),
        "reloaded PRIMARY selection must retain the protected generated value"
    );
    // Replacing a selection from an app must prevent reload from resurrecting it.
    run(
        &mut client,
        "launch_app",
        json!({"session_id":id,"command":"printf app-selection | wl-copy"}),
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id}))["text"],
        "app-selection"
    );
    let binary = client.binary_copy.as_ref().expect("private executable");
    let replacement = binary.with_extension("next");
    std::fs::copy(binary, &replacement).expect("stage executable");
    std::fs::rename(replacement, binary).expect("replace executable");
    client.wait_for_stderr("reload: reattached 1 sessions", Duration::from_secs(30));
    client.wait_for_notification("notifications/tools/list_changed", Duration::from_secs(10));
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id}))["text"],
        "app-selection"
    );
    for arguments in [
        json!({"session_id":id,"target":"session","text":"bad","generate_secret":secret}),
        json!({"session_id":id,"target":"session","generate_secret":{"length":20,"character_classes":[]}}),
        json!({"session_id":id,"target":"session","generate_secret":{"length":1,"character_classes":["lowercase","digits"]}}),
    ] {
        let response = call_tool(&mut client, 2000, "clipboard_write", arguments);
        assert!(
            response["error"].is_object() || response["result"]["isError"] == true,
            "invalid write accepted"
        );
    }
    assert_eq!(
        run(&mut client, "clipboard_read", json!({"session_id":id}))["text"],
        "app-selection"
    );
    run(&mut client, "session_stop", json!({"session_id":id}));
    client.stop_process();
    run(&mut desktop, "session_stop", json!({"session_id":host_id}));
    desktop.stop_process();
    println!(
        "PASS: separate clipboards; 595-character XTerm Shift+Insert paste; Unicode paste; viewer copy and paste; generated secret classes and length without readback; reload preserves owners and newer app selections; invalid writes preserve selection"
    );
}

#[test]
#[ignore = "requires a private KWin compositor and input devices"]
fn clipboard_read_times_out_on_a_private_frozen_compositor_and_recovers() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let id = session_handle(&started);
    let compositor = nix::unistd::Pid::from_raw(child_compositor(client.child.id()));
    // Exercise initial connection and an existing clipboard connection separately.
    for request in [3, 5] {
        nix::sys::signal::kill(compositor, nix::sys::signal::Signal::SIGSTOP)
            .expect("pause owned compositor");
        let resume = ResumeProcess(compositor);
        let start = Instant::now();
        let response = call_tool(
            &mut client,
            request,
            "clipboard_read",
            json!({"session_id":id}),
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "clipboard deadline exceeded"
        );
        assert!(
            response["error"].is_object() || response["result"]["isError"] == true,
            "{response}"
        );
        drop(resume);
        let recovered = call_tool(
            &mut client,
            request + 1,
            "clipboard_read",
            json!({"session_id":id}),
        );
        assert_eq!(
            recovered["result"]["structuredContent"]["protected"], false,
            "{recovered}"
        );
    }
    call_tool(&mut client, 7, "session_stop", json!({"session_id":id}));
    client.stop_process();
    println!(
        "PASS: initial and established clipboard connections time out within 3 seconds and recover after resuming the owned compositor"
    );
}

#[test]
#[ignore = "requires a private KWin session, Bambu Studio, and kdialog"]
fn launch_rejects_host_tmp_file_before_opening_app() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let path = std::env::temp_dir().join(format!(
        "kwin-mcp-host-path-{}-{}.stl",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::write(
        &path,
        b"solid triangle\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid triangle\n",
    )
    .expect("write host STL");
    let mut client = RpcClient::start();
    let visible = client.home.join("visible.stl");
    std::fs::write(&visible, b"solid visible\nendsolid visible\n").expect("write visible host STL");
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":640,"height":480}),
    );
    let id = session_handle(&started);
    for (request, argument) in [
        (3, format!("bambustudio {}", path.display())),
        (
            4,
            format!(
                "kdialog --title HiddenHostPath --msgbox --file={}",
                path.display()
            ),
        ),
    ] {
        let rejected = call_tool(
            &mut client,
            request,
            "launch_app",
            json!({"session_id":id,"command":argument}),
        );
        let error = rejected["error"].to_string();
        assert!(
            error.contains(path.to_str().expect("path text")),
            "{rejected}"
        );
        assert!(
            error.contains("/tmp is private to the session"),
            "{rejected}"
        );
    }
    let windows = call_tool(&mut client, 5, "window_list", json!({"session_id":id}));
    assert!(
        !windows["result"]["structuredContent"]["windows"]
            .as_array()
            .expect("windows")
            .iter()
            .any(|row| {
                let title = row["title"].as_str().unwrap_or_default();
                title.contains("HiddenHostPath") || title.contains("Bambu")
            }),
        "path preflight started the app: {windows}"
    );
    let ordinary = call_tool(
        &mut client,
        6,
        "launch_app",
        json!({"session_id":id,"command":format!("kdialog --title OrdinaryLaunch --msgbox {}",visible.display())}),
    );
    assert!(ordinary["error"].is_null(), "{ordinary}");
    let window = &ordinary["result"]["structuredContent"]["window"];
    assert!(
        window.is_string() && window != "exited" && window != "timeout",
        "{ordinary}"
    );
    call_tool(&mut client, 7, "session_stop", json!({"session_id":id}));
    client.stop_process();
    std::fs::remove_file(&path).expect("remove host test STL");
    println!(
        "PASS: host /tmp path and --file path rejected before app launch; ordinary launch still opens a private window"
    );
}

#[test]
#[ignore = "requires private KWin desktops, Chrome, PipeWire, and Vulkan viewer rendering"]
fn viewer_reuses_and_reopens_after_presenting_a_frame() {
    use base64::Engine;

    const WIDTH: u32 = 1400;
    const HEIGHT: u32 = 1000;
    const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    const COLOR_TOLERANCE: u8 = 16;

    fn tool(client: &mut RpcClient, request: &mut u64, name: &str, args: Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(3);
        let response = loop {
            let response = call_tool(client, *request, name, args.clone());
            *request += 1;
            if response["error"]["data"]["reason"] == "recent_viewer_input"
                && response["error"]["data"]["retry_safe"] == true
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            break response;
        };

        assert!(response["error"].is_null(), "{response}");
        assert!(
            !response["result"]["isError"].as_bool().unwrap_or(false),
            "{response}"
        );
        response
    }

    fn matches_palette(response: &Value, colors: &[[u8; 3]; 4]) -> bool {
        if inline_png(response).is_empty() {
            return false;
        }
        let image = decode_rgba(response);
        let height = u32::try_from(image.1.len() / 4 / usize::try_from(image.0).expect("width"))
            .expect("height");
        [(1, 1), (3, 1), (1, 3), (3, 3)]
            .into_iter()
            .zip(colors)
            .all(|((x, y), expected)| {
                let actual = pixel(&image, image.0 * x / 4, height * y / 4);
                actual[..3]
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.abs_diff(*expected) <= COLOR_TOLERANCE)
            })
    }

    fn save(response: &Value, name: &str) {
        if let Some(root) = std::env::var_os("KWIN_MCP_TEST_ARTIFACT_DIR") {
            let root = PathBuf::from(root);
            std::fs::create_dir_all(&root).expect("artifact directory");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(inline_png(response))
                .expect("screenshot base64");
            std::fs::write(root.join(name), bytes).expect("save screenshot");
        }
    }

    fn write_fixture(path: &std::path::Path, colors: &[[u8; 3]; 4], label: &str) {
        let swatches =
            colors.map(|[r, g, b]| format!("<div style='background:rgb({r},{g},{b})'></div>"));
        std::fs::write(path, format!(
            "<!doctype html><style>html,body{{margin:0;width:100%;height:100%;overflow:hidden}}\
             body{{display:grid;grid-template:1fr 1fr/1fr 1fr}}\
             p{{position:absolute;top:45%;left:30%;background:white;padding:12px;font:28px sans-serif}}</style>\
             {}<p>Same session, frame {}</p>", swatches.join(""), label
        )).expect("write private HTML fixture");
    }

    fn navigate(
        client: &mut RpcClient,
        request: &mut u64,
        session: &str,
        fixture: &std::path::Path,
    ) {
        tool(
            client,
            request,
            "keyboard_key",
            json!({"session_id":session,"key":"ctrl+l"}),
        );
        tool(
            client,
            request,
            "keyboard_type",
            json!({"session_id":session,"text":format!("file://{}",fixture.display())}),
        );
        tool(
            client,
            request,
            "keyboard_key",
            json!({"session_id":session,"key":"Return"}),
        );
    }

    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    // The viewer renders into an owned outer desktop, so the proof captures
    // compositor pixels without reading or operating the user's desktop.
    let bus = require_filtered_cookie_test_bus();
    let mut desktop = RpcClient::start_with_env(&[("DBUS_SESSION_BUS_ADDRESS", &bus)]);
    initialize(&mut desktop);
    let host = call_tool(
        &mut desktop,
        2,
        "session_start",
        json!({"width":WIDTH,"height":HEIGHT}),
    );
    let host_id = session_handle(&host);
    let display = workdir(&host).join("wayland-0");
    let mut client = RpcClient::start_with_env(&[
        ("WAYLAND_DISPLAY", display.to_str().expect("display")),
        ("DBUS_SESSION_BUS_ADDRESS", &bus),
    ]);
    initialize(&mut client);
    let server_pid = client.child.id();
    let mut request = 2;
    let mut host_request = 3;
    let started = tool(
        &mut client,
        &mut request,
        "session_start",
        json!({"width":WIDTH,"height":HEIGHT}),
    );
    let session = session_handle(&started);
    let palettes = [
        [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]],
        [[0, 255, 255], [255, 0, 255], [255, 255, 0], [0, 0, 255]],
    ];
    let mut previous_pid = None;
    let mut browser_id = None;
    for (round, colors) in palettes.iter().enumerate() {
        let fixture = client.home.join(format!("viewer-reuse-{round}.html"));
        write_fixture(&fixture, colors, &format!("{} opened", round + 1));
        if round == 0 {
            tool(
                &mut client,
                &mut request,
                "launch_app",
                json!({"session_id":session,"command":format!(
                    "google-chrome-stable --user-data-dir=/tmp/viewer-reuse --no-first-run --no-default-browser-check 'file://{}'", fixture.display()
                )}),
            );
            let windows = tool(
                &mut client,
                &mut request,
                "window_list",
                json!({"session_id":session}),
            );
            browser_id = Some(
                windows["result"]["structuredContent"]["windows"]
                    .as_array()
                    .expect("browser windows")
                    .iter()
                    .find(|window| {
                        window["resourceClass"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("chrome")
                    })
                    .expect("Chrome window")["id"]
                    .clone(),
            );
        } else {
            navigate(&mut client, &mut request, &session, &fixture);
        }
        let deadline = Instant::now() + FRAME_TIMEOUT;
        loop {
            let source = tool(
                &mut client,
                &mut request,
                "screenshot",
                json!({"session_id":session,"inline":true}),
            );
            if matches_palette(&source, colors) {
                break;
            }
            assert!(Instant::now() < deadline, "source palette did not appear");
            thread::sleep(POLL_INTERVAL);
        }

        let opened = tool(
            &mut client,
            &mut request,
            "viewer_open",
            json!({"session_id":session,"reason":"Inspect the current frame"}),
        );
        let viewer = &opened["result"]["structuredContent"]["viewer"];
        let log = std::fs::read_to_string(workdir(&started).join("viewer.log")).unwrap_or_default();
        assert_eq!(viewer["state"], "ready", "{opened}\n{log}");
        let pid = viewer["pid"].as_u64().expect("viewer PID");
        assert_ne!(
            previous_pid,
            Some(pid),
            "reopen did not create a new viewer"
        );
        if let Some(previous) = previous_pid {
            assert!(
                !process_alive(u32::try_from(previous).expect("old PID fits")).unwrap_or(true),
                "reopen did not reap the old viewer"
            );
        }
        let mut window_id = None;

        for (phase, reason) in [
            ("opened", "Inspect the current frame"),
            ("reused", "Enter the verification code"),
        ] {
            let current_colors = if phase == "reused" {
                colors.map(|[r, g, b]| [b, r, g])
            } else {
                *colors
            };
            if phase == "reused" {
                // Change the live browser before updating the viewer's reason:
                // stale pixels and a stale ready file must not satisfy reuse.
                let live = client.home.join(format!("viewer-reuse-{round}-live.html"));
                write_fixture(&live, &current_colors, &format!("{} reused", round + 1));
                navigate(&mut client, &mut request, &session, &live);
                let reused = tool(
                    &mut client,
                    &mut request,
                    "viewer_open",
                    json!({"session_id":session,"reason":reason}),
                );
                let data = &reused["result"]["structuredContent"];
                assert_eq!(data["status"], "already_open", "{reused}");
                assert_eq!(data["viewer"]["state"], "ready", "{reused}");
                assert_eq!(
                    data["viewer"]["pid"], pid,
                    "reuse replaced a working viewer"
                );
            }
            let title = format!("kwin-viewer {session}: {reason}");
            let deadline = Instant::now() + FRAME_TIMEOUT;
            loop {
                let windows = tool(
                    &mut desktop,
                    &mut host_request,
                    "window_list",
                    json!({"session_id":host_id}),
                );
                let viewers: Vec<_> = windows["result"]["structuredContent"]["windows"]
                    .as_array()
                    .expect("outer windows")
                    .iter()
                    .filter(|window| {
                        window["title"]
                            .as_str()
                            .unwrap_or_default()
                            .starts_with(&format!("kwin-viewer {session}:"))
                    })
                    .collect();
                assert!(viewers.len() <= 1, "duplicate viewers: {viewers:?}");
                if let Some(window) = viewers.first().filter(|window| window["title"] == title) {
                    if let Some(id) = &window_id {
                        assert_eq!(&window["id"], id, "reuse replaced the window");
                    }
                    window_id = Some(window["id"].clone());
                    tool(
                        &mut desktop,
                        &mut host_request,
                        "window_activate",
                        json!({"session_id":host_id,"window_id":window["id"]}),
                    );
                    let displayed = tool(
                        &mut desktop,
                        &mut host_request,
                        "screenshot",
                        json!({"session_id":host_id,"inline":true}),
                    );
                    if matches_palette(&displayed, &current_colors) {
                        save(&displayed, &format!("viewer-reuse-{round}-{phase}.png"));
                        break;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "viewer did not show current pixels/title: {windows}"
                );
                thread::sleep(POLL_INTERVAL);
            }
        }
        assert_eq!(client.child.id(), server_pid, "server restarted");
        let windows = tool(
            &mut client,
            &mut request,
            "window_list",
            json!({"session_id":session}),
        );
        assert!(
            windows["result"]["structuredContent"]["windows"]
                .as_array()
                .expect("source windows")
                .iter()
                .any(|window| Some(&window["id"]) == browser_id.as_ref()),
            "browser was replaced: {windows}"
        );
        if round == 0 {
            // Exercise an actual window-manager close through the outer MCP
            // desktop, rather than relying only on viewer_close.
            tool(
                &mut desktop,
                &mut host_request,
                "window_activate",
                json!({"session_id":host_id,"window_id":window_id}),
            );
            tool(
                &mut desktop,
                &mut host_request,
                "keyboard_key",
                json!({"session_id":host_id,"key":"alt+F4"}),
            );
            let deadline = Instant::now() + FRAME_TIMEOUT;
            while process_alive(u32::try_from(pid).expect("PID fits")).unwrap_or(false) {
                assert!(
                    Instant::now() < deadline,
                    "viewer process survived window close"
                );
                thread::sleep(POLL_INTERVAL);
            }
        } else {
            let closed = tool(
                &mut client,
                &mut request,
                "viewer_close",
                json!({"session_id":session}),
            );
            assert_eq!(closed["result"]["structuredContent"]["status"], "closed");
        }
        assert!(
            round == 0 || !process_alive(u32::try_from(pid).expect("PID fits")).unwrap_or(true),
            "viewer process survived close"
        );
        let windows = tool(
            &mut desktop,
            &mut host_request,
            "window_list",
            json!({"session_id":host_id}),
        );
        assert!(
            !windows["result"]["structuredContent"]["windows"]
                .as_array()
                .expect("outer windows")
                .iter()
                .any(|window| Some(&window["id"]) == window_id.as_ref()),
            "viewer window survived close"
        );
        previous_pid = Some(pid);
        println!(
            "PASS: round {} opened, reused with updated title/current pixels, and closed; browser/session unchanged",
            round + 1
        );
    }
    tool(
        &mut client,
        &mut request,
        "session_stop",
        json!({"session_id":session}),
    );
    client.stop_process();
    tool(
        &mut desktop,
        &mut host_request,
        "session_stop",
        json!({"session_id":host_id}),
    );
    desktop.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, input devices, and a live GPU session"]
fn launch_app_reads_the_initial_empty_desktop_without_a_second_active_window_probe() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(
        &mut client,
        2,
        "session_start",
        json!({"width":800,"height":600}),
    );
    let session_id = session_handle(&started);
    let workdir = workdir(&started);
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({
            "session_id": session_id,
            "command": "printf submitted-once > \"$XDG_RUNTIME_DIR/first-launch-proof\"; konsole --separate --hold -e bash -c 'printf first-launch-window'"
        }),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let payload = &launched["result"]["structuredContent"];
    assert_eq!(payload["command_submitted"], true, "{launched}");
    assert_eq!(payload["window_baseline"], "pre_launch", "{launched}");
    assert_eq!(payload["window_baseline_errors"], json!([]), "{launched}");
    assert_eq!(payload["window_outcome"], "new", "{launched}");
    assert_eq!(
        std::fs::read_to_string(workdir.join("first-launch-proof"))
            .expect("independent launch marker"),
        "submitted-once"
    );
    let windows = call_tool(
        &mut client,
        4,
        "window_list",
        json!({"session_id": session_id}),
    );
    assert!(
        windows.to_string().to_lowercase().contains("konsole"),
        "{windows}"
    );
    call_tool(
        &mut client,
        5,
        "session_stop",
        json!({"session_id": session_id}),
    );
    client.stop_process();
}
