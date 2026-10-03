#![allow(clippy::expect_used)]

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct RpcClient {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
    stderr: Receiver<String>,
    pending: HashMap<u64, Value>,
    /// Private HOME for this run, removed on drop.
    home: PathBuf,
}

impl RpcClient {
    fn start() -> Self {
        Self::start_with_options(None)
    }

    fn start_with_options(delay_stage: Option<&str>) -> Self {
        Self::start_program(env!("CARGO_BIN_EXE_kwin-mcp"), delay_stage, false, false, &[])
    }

    #[cfg(debug_assertions)]
    fn start_with_options_and_stop(
        delay_stage: Option<&str>,
        stop_bwrap: bool,
    ) -> Self {
        Self::start_program(env!("CARGO_BIN_EXE_kwin-mcp"), delay_stage, stop_bwrap, false, &[])
    }

    #[cfg(debug_assertions)]
    fn start_with_first_proxy_failure() -> Self {
        Self::start_program(env!("CARGO_BIN_EXE_kwin-mcp"), None, false, true, &[])
    }

    fn start_with_env(extra_env: &[(&str, &str)]) -> Self {
        Self::start_program(env!("CARGO_BIN_EXE_kwin-mcp"), None, false, false, extra_env)
    }

    fn start_shim() -> Self {
        Self::start_program(
            env!("CARGO_BIN_EXE_kwin-mcp-shim"),
            None,
            false,
            false,
            &[("KWIN_MCP_REPO", "/nonexistent")],
        )
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
        let home =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kwin-mcp-e2e-home-{}-{nonce}", std::process::id()));
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
        } else { program.to_owned() };
        let mut command = Command::new(program);
        command
            .args(["--autoclean"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("KDEHOME", home.join(".kde"))
            .env(
                "XDG_RUNTIME_DIR",
                std::env::var_os("XDG_RUNTIME_DIR")
                    .unwrap_or_else(|| std::ffi::OsString::from("/run/user/1000")),
            )
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
                    .unwrap_or_else(|| std::ffi::OsString::from("unix:path=/run/user/1000/bus")),
            )
            .env(
                "WAYLAND_DISPLAY",
                std::env::var_os("WAYLAND_DISPLAY")
                    .unwrap_or_else(|| std::ffi::OsString::from("wayland-0")),
            );
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
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(message) = serde_json::from_str::<Value>(&line) {
                    let _ = response_tx.send(message);
                }
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = stderr_tx.send(line);
            }
        });
        Self {
            child,
            stdin,
            responses,
            stderr: stderr_rx,
            pending: HashMap::new(),
            home,
        }
    }

    fn send(&mut self, id: u64, method: &str, params: Value) {
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        writeln!(self.stdin, "{request}").expect("write JSON-RPC request");
        self.stdin.flush().expect("flush JSON-RPC request");
    }

    fn notify(&mut self, method: &str, params: Value) {
        let request = json!({"jsonrpc":"2.0", "method":method, "params":params});
        writeln!(self.stdin, "{request}").expect("write JSON-RPC notification");
        self.stdin.flush().expect("flush JSON-RPC notification");
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    #[cfg(debug_assertions)]
    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let request = json!({
                "jsonrpc":"2.0",
                "id":9999,
                "method":"tools/call",
                "params":{"name":"session_stop","arguments":{}}
            });
            let _ = writeln!(self.stdin, "{request}");
            let _ = self.stdin.flush();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
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
    assert!(response["result"].is_object(), "initialize failed: {response}");
    client.notify("notifications/initialized", json!({}));
}

fn call_tool(client: &mut RpcClient, id: u64, name: &str, arguments: Value) -> Value {
    client.send(
        id,
        "tools/call",
        json!({"name":name, "arguments":arguments}),
    );
    client.response(id, Duration::from_secs(45))
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
        .args(["search", "--all", "--pid", &pid.to_string(), "--title", "^kwin-viewer$"])
        .output()
        .expect("query host KWin windows");
    assert!(output.status.success(), "kdotool: {}", String::from_utf8_lossy(&output.stderr));
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

fn workdir(response: &Value) -> PathBuf {
    response["result"]["structuredContent"]["workdir"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("session_start did not return a workdir: {response}"))
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
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Python, input devices, and a live GPU session"]
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
            "konsole -e bash -lc 'python3 -m http.server {port} --bind 127.0.0.1 >/dev/null 2>&1 & listener=$!; sleep 1; if kill -0 \"$listener\" 2>/dev/null; then hostname > \"$XDG_RUNTIME_DIR/loopback-ready\"; fi; wait \"$listener\"'"
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
    drop(host_listener);
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

    for (stage, stop_bwrap) in [
        ("after-bwrap", false),
        ("after-bwrap", true),
    ] {
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
        let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}"));
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
    let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}"));
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
    assert!(elapsed < Duration::from_secs(22), "first start took {elapsed:?}: {first}");
    let message = first["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("exceeded 20s hard limit while scanning host mounts"), "{first}");
    assert_eq!(first["error"]["data"]["host_call_blocked"], json!(true), "{first}");

    // The blocked scan still owns the gate and the workdir, so the next
    // lifecycle call reports busy within the limit rather than hanging.
    let started = Instant::now();
    let second = call_tool(&mut client, 3, "session_start", json!({}));
    assert!(started.elapsed() < Duration::from_secs(22), "second start: {second}");
    assert_eq!(second["error"]["data"]["reason"], json!("lifecycle_busy"), "{second}");

    let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}"));
    client.wait_for_stderr("blocked host scan returned", Duration::from_secs(30));
    assert!(!workdir.exists(), "deferred cleanup left {}", workdir.display());
    assert!(
        process_children(server_pid).unwrap_or_else(|error| panic!("inspect children: {error}")).is_empty(),
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":1024,"height":768}));
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
            assert!(Instant::now() < deadline, "{combo}: page saw {title}, expected {expected}");
            thread::sleep(Duration::from_millis(200));
        }
    }
    for combo in ["ctrl+bogus", "foo+a", "ctrl+", ""] {
        id += 1;
        let rejected = call_tool(&mut client, id, "keyboard_key", json!({"key":combo}));
        assert_eq!(rejected["error"]["code"], json!(-32602), "{combo:?} was not rejected: {rejected}");
        thread::sleep(Duration::from_millis(500));
        id += 1;
        let title = window_title(&mut client, id, "K:").unwrap_or_default();
        assert!(title.starts_with("K:Enter "), "{combo:?} still sent input: {title}");
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":1024,"height":768}));
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
        let immediate = call_tool(&mut client, id, "screenshot", json!({"inline":true, "region":[0,120,1024,768]}));
        thread::sleep(Duration::from_millis(800));
        id += 1;
        let later = call_tool(&mut client, id, "screenshot", json!({"inline":true, "region":[0,120,1024,768]}));
        if inline_png(&immediate) != inline_png(&later) {
            use base64::Engine;
            for (name, shot) in [("immediate", &immediate), ("later", &later)] {
                let _ = std::fs::write(
                    std::env::temp_dir().join(format!("kwin-mcp-e2e-{name}.png")),
                    base64::engine::general_purpose::STANDARD.decode(inline_png(shot)).unwrap_or_default(),
                );
            }
            eprintln!("immediate: {}", immediate["result"]["content"][1]);
        }
        assert!(
            inline_png(&immediate) == inline_png(&later) && !inline_png(&later).is_empty(),
            "round {round}: screenshot right after keyboard_type predates the input"
        );
        let settle = immediate["result"]["content"][1]["text"].as_str().unwrap_or_default();
        assert!(settle.contains("\"settled\":true"), "round {round}: {settle}");
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
    let info = reader.next_frame(&mut pixels).expect("screenshot PNG frame");
    (info.width, pixels)
}

fn pixel(image: &(u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let index = usize::try_from((y * image.0 + x) * 4).expect("pixel index");
    [image.1[index], image.1[index + 1], image.1[index + 2], image.1[index + 3]]
}

fn screenshot_meta(response: &Value) -> Value {
    let text = response["result"]["content"][1]["text"].as_str().unwrap_or_else(|| panic!("no screenshot metadata in {response}"));
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":1920,"height":1080}));
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
    let (win_w, win_h) = (meta["window"]["width"].as_i64().expect("w"), meta["window"]["height"].as_i64().expect("h"));
    assert!(meta["window"]["x"].as_i64() > Some(0) && meta["window"]["y"].as_i64() > Some(0), "{meta}");
    assert_eq!(meta["region"], json!([0, 0, win_w, win_h]), "{meta}");
    assert_eq!((meta["width"].as_i64(), meta["height"].as_i64()), (Some(win_w), Some(win_h)), "{meta}");

    // A crop past the window edges keeps window-relative coordinates: the
    // dialog's own pixel (5,5) sits at image (45,45) under region origin -40.
    let wide = call_tool(&mut client, 5, "screenshot", json!({"inline":true, "region":[-40,-40,win_w + 40,win_h + 40]}));
    assert_eq!(screenshot_meta(&wide)["region"], json!([-40, -40, win_w + 40, win_h + 40]));
    let (full_image, wide_image) = (decode_rgba(&full), decode_rgba(&wide));
    assert_eq!(pixel(&full_image, 5, 5), pixel(&wide_image, 45, 45));
    assert_ne!(pixel(&wide_image, 45, 45), pixel(&wide_image, 5, 5), "crop did not include the surroundings");

    // Click the button where it appears in the crop, translated by the
    // documented contract: input = image pixel + region origin.
    let found = call_tool(&mut client, 6, "find_ui_elements", json!({"query":"Relaunch"}));
    let listing = found["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
    let numbers: Vec<i64> = listing
        .rsplit_once('(')
        .map(|(_, rest)| rest.trim_end_matches([')', '\n']).replace('x', ","))
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    assert_eq!(numbers.len(), 4, "button geometry: {listing}");
    let (image_x, image_y) = (numbers[0] + numbers[2] / 2 + 40, numbers[1] + numbers[3] / 2 + 40);
    call_tool(&mut client, 7, "mouse_click", json!({"x": image_x - 40, "y": image_y - 40}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result_path.exists() {
        assert!(Instant::now() < deadline, "click at screenshot coordinates missed the dialog button");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(std::fs::read_to_string(&result_path).unwrap_or_default().trim(), "0");

    // Maximized window at the origin: image equals the display.
    call_tool(&mut client, 8, "launch_app", json!({"command":"konsole"}));
    let maximized = call_tool(&mut client, 9, "screenshot", json!({}));
    let meta = &maximized["result"]["structuredContent"];
    assert_eq!((meta["window"]["x"].as_i64(), meta["window"]["y"].as_i64()), (Some(0), Some(0)), "{meta}");
    assert_eq!((meta["width"].as_i64(), meta["height"].as_i64()), (Some(1920), Some(1080)), "{meta}");
    call_tool(&mut client, 10, "session_stop", json!({}));
    client.stop_process();
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    let viewer = &started["result"]["structuredContent"]["viewer"];
    assert_eq!(viewer["state"], json!("closed"), "{started}");
    assert!(!workdir(&started).join("viewer.log").exists(), "session_start launched a viewer");
    let opened = call_tool(&mut client, 3, "viewer_open", json!({}));
    assert_eq!(opened["result"]["structuredContent"]["status"], json!("opened"), "{opened}");
    let viewer = &opened["result"]["structuredContent"]["viewer"];
    assert!(viewer["state"] == "ready" || viewer["state"] == "starting", "{opened}");
    let pid = u32::try_from(viewer["pid"].as_u64().expect("viewer pid")).expect("pid fits");
    assert!(process_alive(pid).unwrap_or(false), "reported viewer is not running");
    assert_eq!(wait_for_viewer_windows(pid, 1).len(), 1, "viewer_open did not create exactly one host window: pid={pid} opened={opened}");
    let again = call_tool(&mut client, 4, "viewer_open", json!({}));
    assert_eq!(again["result"]["structuredContent"]["status"], json!("already_open"), "{again}");
    let closed = call_tool(&mut client, 5, "viewer_close", json!({}));
    assert_eq!(closed["result"]["structuredContent"]["status"], json!("closed"), "{closed}");
    assert_eq!(closed["result"]["structuredContent"]["viewer"]["state"], json!("closed"), "{closed}");
    assert!(!process_alive(pid).unwrap_or(true), "viewer remained alive after viewer_close");
    assert!(wait_for_viewer_windows(pid, 0).is_empty(), "viewer_close left a host window");
    let running = call_tool(&mut client, 6, "session_start", json!({}));
    assert_eq!(running["result"]["structuredContent"]["status"], json!("already_running"), "{running}");
    call_tool(&mut client, 7, "session_stop", json!({}));
    client.stop_process();

    // A missing host Wayland display affects viewer_open, not session_start.
    let mut client = RpcClient::start_program(env!("CARGO_BIN_EXE_kwin-mcp"), None, false, false, &[("WAYLAND_DISPLAY", "kwin-mcp-no-such-display")]);
    initialize(&mut client);
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    let content = &started["result"]["structuredContent"];
    assert_eq!(content["status"], json!("started"), "{started}");
    assert_eq!(content["viewer"]["state"], json!("closed"), "{started}");
    let opened = call_tool(&mut client, 3, "viewer_open", json!({}));
    let content = &opened["result"]["structuredContent"];
    assert_eq!(content["status"], json!("unavailable"), "{opened}");
    assert_eq!(content["viewer"]["state"], json!("unavailable"), "{opened}");
    assert!(
        content["viewer"]["reason"].as_str().unwrap_or_default().contains("host Wayland resolution failed"),
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    let workdir = workdir(&started);
    call_tool(&mut client, 3, "launch_app", json!({"command":"konsole"}));
    let first = call_tool(&mut client, 4, "screenshot", json!({}));
    assert!(first["error"].is_null(), "{first}");
    let screenshot = workdir.join("screenshot.png");
    assert!(screenshot.exists());

    // Use up the quota, leaving less than one screenshot of headroom, while
    // the filesystem itself keeps free space: the situation behind EDQUOT.
    let fill = FillFile(std::env::temp_dir().join(format!("kwin-mcp-e2e-quota-fill-{}", std::process::id())));
    std::fs::remove_file(&screenshot).expect("remove first screenshot");
    let headroom = user_quota_headroom(std::path::Path::new("/tmp")).expect("quota headroom");
    let file = std::fs::File::create(&fill.0).expect("create fill file");
    nix::fcntl::posix_fallocate(&file, 0, i64::try_from(headroom - 16 * 1024).expect("fill size")).expect("fill quota");

    let failed = call_tool(&mut client, 5, "screenshot", json!({}));
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("Disk quota exceeded") && message.contains("Per-user disk quota"), "{failed}");
    assert!(!screenshot.exists(), "failed write left a partial screenshot");

    let inline = call_tool(&mut client, 6, "screenshot", json!({"inline":true}));
    assert!(!inline_png(&inline).is_empty(), "inline screenshot missing at quota: {inline}");
    assert!(inline["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("file not saved"), "{inline}");
    assert!(!screenshot.exists(), "inline screenshot at quota left a partial file");

    // An open descriptor keeps the space charged, so close it before removing.
    drop(file);
    drop(fill);
    let recovered = call_tool(&mut client, 7, "screenshot", json!({}));
    assert!(recovered["error"].is_null() && screenshot.exists(), "{recovered}");
    call_tool(&mut client, 8, "session_stop", json!({}));
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, konsole, python3, input devices, and a live GPU session"]
fn session_reads_a_consistent_copy_of_a_host_live_sqlite_database() {
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
    for directory in [".codex", ".config", ".local/share", ".cache", ".local/state", ".kde"] {
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

    // A host app keeps writing its thread store for the whole session.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (stop, database) = (stop.clone(), database.clone());
        thread::spawn(move || {
            let host = rusqlite::Connection::open(&database).expect("host connection");
            host.execute_batch("pragma journal_mode=wal; pragma wal_autocheckpoint=100;").expect("host pragmas");
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                host.execute("insert into threads(body) values (randomblob(2048))", []).expect("host insert");
                host.execute("delete from threads where id = (select min(id) from threads)", []).expect("host delete");
            }
        })
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    let workdir = workdir(&started);
    let report = workdir.join("sqlite-check.txt");
    // Read-only and long-lived read-write connections, then a session write.
    let script = format!(
        "import sqlite3,time\n\
         db='{db}'; out=open('{out}','w'); bad=0\n\
         for i in range(20):\n\
         \x20   c=sqlite3.connect('file:'+db+'?mode=ro',uri=True)\n\
         \x20   c.execute('select count(*),sum(length(body)) from threads').fetchone()\n\
         \x20   bad+=c.execute('pragma quick_check').fetchone()[0]!='ok'; c.close(); time.sleep(0.05)\n\
         w=sqlite3.connect(db)\n\
         for i in range(20):\n\
         \x20   bad+=w.execute('pragma quick_check').fetchone()[0]!='ok'; time.sleep(0.05)\n\
         w.execute(\"insert into threads(body) values (x'73657373696f6e')\"); w.commit()\n\
         out.write('bad=%d\\n' % bad); out.close()\n",
        db = database.display(),
        out = report.display()
    );
    std::fs::write(workdir.join("sqlite-check.py"), script).expect("write check script");
    call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":format!("python3 '{}'; konsole", workdir.join("sqlite-check.py").display())}),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        if let Ok(text) = std::fs::read_to_string(&report)
            && text.ends_with('\n')
        {
            break text;
        }
        assert!(Instant::now() < deadline, "session SQLite check did not finish");
        thread::sleep(Duration::from_millis(200));
    };
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().expect("host writer");
    assert_eq!(result.trim(), "bad=0", "session saw a malformed database");

    let host = rusqlite::Connection::open(&database).expect("reopen host database");
    let check: String = host.query_row("pragma integrity_check", [], |row| row.get(0)).expect("host integrity");
    assert_eq!(check, "ok");
    let leaked: i64 = host
        .query_row("select count(*) from threads where body = x'73657373696f6e'", [], |row| row.get(0))
        .expect("host query");
    assert_eq!(leaked, 0, "session write reached the host database");
    call_tool(&mut client, 4, "session_stop", json!({}));
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
    for directory in ["Downloads", "out", ".config", ".local/share", ".cache", ".local/state", ".kde"] {
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
    call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
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
    assert!(!download.exists(), "session write leaked to the host without export");

    let exported = call_tool(&mut client, 4, "export_file", json!({"session_path": download, "host_path": home.join("out")}));
    let content = &exported["result"]["structuredContent"];
    assert_eq!(content["status"], json!("exported"), "{exported}");
    assert_eq!(content["bytes"], json!(300_000), "{exported}");
    let host_copy = home.join("out/report.bin");
    assert_eq!(std::fs::metadata(&host_copy).map(|meta| meta.len()).unwrap_or(0), 300_000);

    let refused = call_tool(&mut client, 5, "export_file", json!({"session_path": download, "host_path": home.join("out")}));
    assert!(refused["error"]["message"].as_str().unwrap_or_default().contains("already exists"), "{refused}");
    let replaced = call_tool(&mut client, 6, "export_file", json!({"session_path": download, "host_path": host_copy, "overwrite": true}));
    assert_eq!(replaced["result"]["structuredContent"]["status"], json!("exported"), "{replaced}");

    // Default destination: the same path on the host (the directory the user named).
    let default = call_tool(&mut client, 7, "export_file", json!({"session_path": download}));
    assert_eq!(default["result"]["structuredContent"]["host_path"], json!(download.display().to_string()), "{default}");
    assert!(download.exists());

    let note = call_tool(&mut client, 8, "export_file", json!({"session_path": "/tmp/session-note.txt", "host_path": home.join("out")}));
    assert_eq!(note["result"]["structuredContent"]["status"], json!("exported"), "{note}");
    assert_eq!(std::fs::read_to_string(home.join("out/session-note.txt")).unwrap_or_default(), "scratch");

    let partial = call_tool(&mut client, 9, "export_file", json!({"session_path": home.join("Downloads/partial.zip"), "host_path": home.join("out")}));
    assert!(partial["error"]["message"].as_str().unwrap_or_default().contains("still downloading"), "{partial}");
    assert!(!home.join("out/partial.zip").exists());
    call_tool(&mut client, 10, "session_stop", json!({}));
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
    for sub in ["Uploads", ".config", ".local/share", ".cache", ".local/state", ".kde"] {
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":1280,"height":800}));
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
    assert!(!directory.join("session-created.pdf").exists(), "session file leaked to the host");
    for (file, bytes) in [("host-created.pdf", 14), ("session-created.pdf", 20)] {
        id += 1;
        call_tool(&mut client, id, "keyboard_key", json!({"key":"F5"}));
        thread::sleep(Duration::from_millis(1500));
        let found = call_tool(&mut client, id + 1, "find_ui_elements", json!({"query":"Choose File"}));
        id += 1;
        assert!(found["result"]["structuredContent"]["matches"].as_u64() >= Some(1), "{found}");
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
            ("keyboard_type", json!({"text": directory.join(file).display().to_string()})),
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
            assert!(Instant::now() < deadline, "{file}: page shows {title}, expected {expected}");
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
    let sftp_server = ["/usr/lib/ssh/sftp-server", "/usr/libexec/openssh/sftp-server", "/usr/lib/openssh/sftp-server"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists());
    let (Some(sftp_server), true) = (sftp_server, std::path::Path::new("/usr/bin/sshfs").exists()) else {
        eprintln!("sshfs or sftp-server missing; skipping");
        return;
    };
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    let workdir = workdir(&started);
    std::fs::create_dir_all(workdir.join("src")).expect("source dir");
    std::fs::write(workdir.join("src/hello.txt"), "over fuse\n").expect("source file");
    // sshfs over a local sftp-server: a real libfuse3 client going through
    // fusermount3, with no network involved.
    std::fs::write(workdir.join("fake-ssh"), format!("#!/bin/sh\nexec {sftp_server}\n")).expect("fake ssh");
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
        assert!(result.lines().any(|line| line == expected), "missing {expected:?} in:\n{result}");
    }
    call_tool(&mut client, 4, "session_stop", json!({}));
    client.stop_process();
}

fn session_wallet<B, R>(connection: &zbus::blocking::Connection, method: &str, body: &B) -> R
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    connection
        .call_method(Some("org.kde.kwalletd6"), "/modules/kwalletd6", Some("org.kde.KWallet"), method, body)
        .unwrap_or_else(|error| panic!("{method}: {error}"))
        .body()
        .deserialize()
        .unwrap_or_else(|error| panic!("{method} reply: {error}"))
}

fn host_kwallet(method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> Option<zbus::Message> {
    let connection = zbus::blocking::Connection::session().ok()?;
    connection
        .call_method(Some("org.kde.kwalletd6"), "/modules/kwalletd6", Some("org.kde.KWallet"), method, body)
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
        host_kwallet("users", &("kdewallet",)).and_then(|reply| reply.body().deserialize().ok()).unwrap_or_default()
    };
    let wallets = || -> Vec<String> {
        host_kwallet("wallets", &()).and_then(|reply| reply.body().deserialize().ok()).unwrap_or_default()
    };
    if !wallets().iter().any(|wallet| wallet == "kdewallet") {
        eprintln!("no host kdewallet; skipping");
        return;
    }
    let (users_before, wallets_before) = (users(), wallets());
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(started["result"]["structuredContent"]["wallet"]["mode"], json!("session-local copy"), "{started}");
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
    assert_eq!(session_wallet::<_, i32>(&wallet, "close", &(closed, false, "kwin-mcp-e2e-closed")), 0);
    assert_eq!(session_wallet::<_, i32>(&wallet, "open", &("kwin-mcp-e2e-missing-wallet", 0i64, "kwin-mcp-e2e-kept")), -1);
    assert!(session_wallet::<_, bool>(&wallet, "createFolder", &(kept, "kwin-mcp-e2e", "kwin-mcp-e2e-kept")));
    assert_eq!(session_wallet::<_, i32>(&wallet, "writePassword", &(kept, "kwin-mcp-e2e", "k", "v", "kwin-mcp-e2e-kept")), 0);
    assert_eq!(session_wallet::<_, String>(&wallet, "readPassword", &(kept, "kwin-mcp-e2e", "k", "kwin-mcp-e2e-kept")), "v");
    assert_eq!(session_wallet::<_, i32>(&wallet, "close", &("kdewallet", true)), -1);
    assert_eq!(users(), users_before, "the session opened host KWallet handles");
    drop(wallet);
    call_tool(&mut client, 4, "session_stop", json!({}));
    client.stop_process();
    assert_eq!(users(), users_before, "session left host KWallet handles");
    assert_eq!(wallets(), wallets_before, "session changed the host wallet list");
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
        pid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
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
    let roots: Vec<u32> = sessions.iter().map(|(client, _)| client.child.id()).collect();

    let held: Vec<(u32, u32)> = host_bus_peer_pids()
        .into_iter()
        .filter_map(|peer| ancestor_in(peer, &roots).map(|root| (root, peer)))
        .collect();
    assert!(held.is_empty(), "(server, peer) pairs on the host user bus: {held:?}");
    let fds_running = host_bus_broker_fds();

    for (mut client, dir) in sessions {
        let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
        assert!(!stopped["result"]["isError"].as_bool().unwrap_or(false), "{stopped}");
        assert!(!dir.exists(), "session_stop left {}", dir.display());
        client.stop_process();
    }
    let fds_after = host_bus_broker_fds();
    eprintln!("host bus broker fds: before {fds_before}, {count} sessions running {fds_running}, after {fds_after}");
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
fn session_start_preserves_live_codex_state_database() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    let codex_home = client.home.join(".codex");
    std::fs::create_dir(&codex_home).expect("create fixture directory");
    let source = rusqlite::Connection::open(codex_home.join("state_5.sqlite"))
        .expect("open host state fixture");
    source
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE retained_state (value TEXT NOT NULL);
             INSERT INTO retained_state VALUES ('host-state');",
        )
        .expect("write live WAL fixture");
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
    let snapshot_path = client
        .home
        .join(".cache/kwin-mcp")
        .join(directory.file_name().expect("workdir name"))
        .join("overlay-upper/.codex/state_5.sqlite");
    let snapshot = rusqlite::Connection::open_with_flags(
        &snapshot_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open isolated state snapshot");
    source
        .execute("UPDATE retained_state SET value = 'host-changed'", [])
        .expect("change host fixture after session start");
    let value: String = snapshot
        .query_row("SELECT value FROM retained_state", [], |row| row.get(0))
        .expect("read preserved state");
    assert_eq!(value, "host-state");
    drop(snapshot);
    drop(source);
    let stopped = call_tool(&mut client, 3, "session_stop", json!({}));
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
    assert!(!snapshot_path.exists(), "state snapshot remains after stop");
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn application_binary_stderr_keeps_screenshots_and_shutdown_working() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(started["result"]["structuredContent"]["status"], "started", "{started}");
    let directory = workdir(&started);
    let log = client.home.join(".cache/kwin-mcp")
        .join(directory.file_name().expect("workdir name")).join("sandbox.log");
    let launched = call_tool(&mut client, 3, "launch_app", json!({
        "command":"printf '\\377\\376\\n' >&2; kdialog --title binary-stderr-proof --msgbox binary-stderr-proof"
    }));
    assert!(launched["error"].is_null(), "{launched}");
    let captured = call_tool(&mut client, 4, "screenshot", json!({"inline":false}));
    assert!(captured["error"].is_null(), "{captured}");
    let capture = captured["result"]["structuredContent"]["path"].as_str().expect("screenshot path");
    assert!(std::fs::metadata(capture).expect("captured PNG").len() > 0);
    let diagnostics = std::fs::read(&log).expect("raw sandbox diagnostics");
    assert!(diagnostics.windows(3).any(|bytes| bytes == [255, 254, 10]));
    let stopped = call_tool(&mut client, 5, "session_stop", json!({}));
    assert_eq!(stopped["result"]["structuredContent"]["status"], "stopped", "{stopped}");
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(started["result"]["structuredContent"]["status"], "started", "{started}");

    let begun = Instant::now();
    let missing = call_tool(&mut client, 3, "launch_app", json!({"command":"kwin-mcp-e2e-no-such-command"}));
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert_eq!(missing["result"]["structuredContent"]["exit_status"], 127, "{missing}");
    assert!(begun.elapsed() < Duration::from_secs(5), "missing command took {:?}", begun.elapsed());

    let failed = call_tool(&mut client, 4, "launch_app", json!({"command":"exit 3"}));
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(failed["result"]["structuredContent"]["exit_status"], 3, "{failed}");

    let finished = call_tool(&mut client, 5, "launch_app", json!({"command":"true"}));
    assert_ne!(finished["result"]["isError"], true, "{finished}");
    assert_eq!(finished["result"]["structuredContent"]["exit_status"], 0, "{finished}");
    let retained = std::fs::read_dir(workdir(&started)).expect("workdir").flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("launch-") && entry.path().extension().is_some_and(|ext| ext == "sh")).count();
    assert_eq!(retained, 0, "launch command files were retained");

    let stopped = call_tool(&mut client, 6, "session_stop", json!({}));
    assert!(!stopped["result"]["isError"].as_bool().unwrap_or(false), "{stopped}");
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn shim_keeps_the_client_connected_when_another_process_sends_sigterm() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let mut client = RpcClient::start_shim();
    initialize(&mut client);
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(started["result"]["structuredContent"]["status"], "started", "{started}");
    let first = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    let shim = client.child.id();
    let sent = Command::new("kill")
        .args(["-TERM", &shim.to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success(), "kill -TERM {shim} failed");
    thread::sleep(Duration::from_secs(3));
    assert!(
        client.child.try_wait().expect("shim status").is_none(),
        "the shim exited after SIGTERM from another process"
    );

    let old = call_tool(&mut client, 3, "screenshot", json!({"session_id": first}));
    let old_text = old["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert_eq!(old["result"]["isError"], true, "{old}");
    assert!(old_text.contains("sent SIGTERM"), "{old}");

    let restarted = call_tool(&mut client, 4, "session_start", json!({"width":800,"height":600}));
    assert_eq!(restarted["result"]["structuredContent"]["status"], "started", "{restarted}");
    let second = restarted["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    assert_ne!(first, second);
    let stopped = call_tool(&mut client, 5, "session_stop", json!({"session_id": second}));
    assert!(!stopped["result"]["isError"].as_bool().unwrap_or(false), "{stopped}");

    let pid = nix::unistd::Pid::from_raw(i32::try_from(shim).expect("shim pid"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("SIGTERM from the client");
    let deadline = Instant::now() + Duration::from_secs(45);
    while client.child.try_wait().expect("shim status").is_none() {
        assert!(Instant::now() < deadline, "the shim ignored SIGTERM from its client");
        thread::sleep(Duration::from_millis(200));
    }
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(started["result"]["structuredContent"]["status"], "started", "{started}");
    let dir = workdir(&started);
    let disk = client
        .home
        .join(".cache/kwin-mcp")
        .join(dir.file_name().expect("workdir name"));
    assert!(disk.join("overlay-upper").is_dir(), "no overlay upper in {}", disk.display());
    assert!(!dir.join("tmp/overlay-upper").exists(), "overlay upper still in {}", dir.display());

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
    assert_eq!(launched["result"]["structuredContent"]["exit_status"], 3, "{launched}");
    let listing = std::fs::read_to_string(&seen).expect("listing from the session");
    assert!(listing.trim().is_empty(), "the session sees overlay layers: {listing}");
    let probe = ["overlay-upper", "overlay-root"]
        .iter()
        .map(|layer| disk.join(layer).join("kwin-mcp-disk-probe"))
        .find(|path| path.is_file())
        .expect("session write in a disk layer");
    assert_eq!(std::fs::read_to_string(probe).expect("probe").trim(), "on-disk");

    let stopped = call_tool(&mut client, 4, "session_stop", json!({}));
    assert!(!stopped["result"]["isError"].as_bool().unwrap_or(false), "{stopped}");
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
    let started = call_tool(&mut client, 2, "session_start", json!({"width":1024,"height":768}));
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
        warned.as_str().is_some_and(|text| text.contains("did not have keyboard focus")),
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
        assert!(Instant::now() < deadline, "page did not see c after the click");
        thread::sleep(Duration::from_millis(200));
    }
    id += 1;
    call_tool(&mut client, id, "session_stop", json!({}));
    client.stop_process();
}

fn direct_children(pid: u32) -> Vec<i32> {
    let parent = i32::try_from(pid).expect("parent pid");
    procfs::process::all_processes().expect("read procfs").flatten()
        .filter_map(|process| process.stat().ok())
        .filter(|stat| stat.ppid == parent)
        .map(|stat| stat.pid)
        .collect()
}

#[test]
fn shim_discovery_has_no_persistent_server() {
    let mut client = RpcClient::start_shim();
    thread::sleep(Duration::from_secs(2));
    assert!(direct_children(client.child.id()).is_empty());
    initialize(&mut client);
    client.send(2, "tools/list", json!({}));
    let listed = client.response(2, Duration::from_secs(10));
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|tool| tool["name"] == "session_start"));
    assert!(tools.iter().any(|tool| tool["name"] == "session_list"));
    for tool in tools.iter().filter(|tool| tool["name"] != "session_list") {
        assert_eq!(tool["inputSchema"]["properties"]["session_id"]["type"], "string");
    }
    call_tool(&mut client, 3, "session_list", json!({}));
    client.send(4, "ping", json!({}));
    assert_eq!(client.response(4, Duration::from_secs(2))["result"], json!({}));
    for (index, method) in ["resources/list", "resources/templates/list", "prompts/list", "completion/complete"].iter().enumerate() {
        let id = 5 + u64::try_from(index).expect("index");
        client.send(id, method, json!({"ref":{"type":"ref/prompt","name":"unused"},"argument":{"name":"unused","value":""}}));
        assert!(client.response(id, Duration::from_secs(2))["result"].is_object(), "{method} failed");
    }
    thread::sleep(Duration::from_secs(4));
    assert!(direct_children(client.child.id()).is_empty(), "discovery retained a child");
    assert!(!client.stderr.try_iter().any(|line| line.contains("started child")));
    client.stop_process();
}

#[test]
fn describe_matches_the_stdio_server() {
    let described = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--describe", "--autoclean"])
        .output().expect("describe");
    assert!(described.status.success(), "{}", String::from_utf8_lossy(&described.stderr));
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
    assert_eq!(sorted(&metadata["tools"]), sorted(&listed["result"]["tools"]));
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, kdialog, input devices, and a live GPU session"]
fn shim_starts_only_requested_sessions_and_does_not_replace_stopped_servers() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start_shim();
    initialize(&mut client);
    assert!(direct_children(client.child.id()).is_empty());
    let first = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(first["result"]["structuredContent"]["status"], "started", "{first}");
    let first_id = first["result"]["structuredContent"]["session_id"].as_str().expect("session id").to_owned();
    eprintln!("lazy proof: shim={} first={first_id}", client.child.id());
    assert_eq!(direct_children(client.child.id()).len(), 1, "unexpected spare server");
    let launched = call_tool(&mut client, 3, "launch_app", json!({"session_id":first_id, "command":"kdialog --title lazy-server-proof --msgbox lazy-server-proof"}));
    assert!(!launched["result"]["isError"].as_bool().unwrap_or(false), "{launched}");
    let windows = call_tool(&mut client, 4, "window_list", json!({"session_id":first_id}));
    assert!(windows.to_string().contains("lazy-server-proof"), "{windows}");
    let screenshot = call_tool(&mut client, 5, "screenshot", json!({"session_id":first_id,"inline":true}));
    assert!(screenshot["result"]["content"].as_array().expect("screenshot content")
        .iter().any(|item| item["type"] == "image"), "screenshot did not return an image");
    if let Some(directory) = std::env::var_os("KWIN_MCP_PROOF_DIR") {
        use base64::Engine;
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("proof directory");
        let image = screenshot["result"]["content"].as_array().expect("content").iter()
            .find(|item| item["type"] == "image").expect("image");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(image["data"].as_str().expect("image data")).expect("decode image");
        std::fs::write(directory.join("lazy-first-session.png"), bytes).expect("write proof");
    }
    let second = call_tool(&mut client, 6, "session_start", json!({"width":800,"height":600}));
    assert_eq!(second["result"]["structuredContent"]["status"], "started", "{second}");
    let second_id = second["result"]["structuredContent"]["session_id"].as_str().expect("session id").to_owned();
    eprintln!("lazy proof: second={second_id}");
    assert_ne!(first_id, second_id);
    assert_eq!(direct_children(client.child.id()).len(), 2, "unexpected spare server");
    let second_windows = call_tool(&mut client, 7, "window_list", json!({"session_id":second_id}));
    assert!(!second_windows.to_string().contains("lazy-server-proof"), "window leaked between sessions");
    for (index, session) in [first_id, second_id].iter().enumerate() {
        let id = 8 + u64::try_from(index).expect("index");
        let stopped = call_tool(&mut client, id, "session_stop", json!({"session_id":session}));
        assert!(!stopped["result"]["isError"].as_bool().unwrap_or(false), "{stopped}");
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !direct_children(client.child.id()).is_empty() {
        assert!(Instant::now() < deadline, "server did not exit after session_stop");
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(4));
    assert!(direct_children(client.child.id()).is_empty(), "server respawned without a call");
    client.send(10, "ping", json!({}));
    assert_eq!(client.response(10, Duration::from_secs(2))["result"], json!({}));
    client.stop_process();
}

#[test]
fn shim_releases_server_after_call_without_session() {
    let mut client = RpcClient::start_shim();
    initialize(&mut client);
    let result = call_tool(&mut client, 2, "window_list", json!({}));
    assert!(result.to_string().contains("no session"), "{result}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !direct_children(client.child.id()).is_empty() {
        assert!(Instant::now() < deadline, "unbound server did not exit");
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(4));
    assert!(direct_children(client.child.id()).is_empty());
    client.send(3, "ping", json!({}));
    assert_eq!(client.response(3, Duration::from_secs(2))["result"], json!({}));
    client.stop_process();
}

fn child_compositor(server: u32) -> i32 {
    let server = i32::try_from(server).expect("server pid");
    let stats: Vec<_> = procfs::process::all_processes().expect("process inventory").flatten()
        .filter_map(|process| process.stat().ok()).collect();
    let owned: Vec<_> = stats.iter().filter(|stat| stat.comm == "kwin_wayland").filter(|stat| {
        let mut parent = stat.ppid;
        for _ in 0..64 {
            if parent == server { return true }
            let Some(next) = stats.iter().find(|stat| stat.pid == parent) else { return false };
            if next.ppid == parent { return false }
            parent = next.ppid;
        }
        false
    }).map(|stat| stat.pid).collect();
    assert_eq!(owned.len(), 1, "expected one compositor below server {server}: {owned:?}");
    owned[0]
}

struct ResumeProcess(nix::unistd::Pid);
impl Drop for ResumeProcess {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGCONT);
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, input devices, and a live GPU session"]
fn launch_app_times_out_on_its_own_frozen_compositor_and_recovers() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut client = RpcClient::start();
    initialize(&mut client);
    let started = call_tool(&mut client, 2, "session_start", json!({"width":800,"height":600}));
    assert_eq!(started["result"]["structuredContent"]["status"], "started", "{started}");
    let workdir = workdir(&started);
    let compositor = child_compositor(client.child.id());
    let resumed = ResumeProcess(nix::unistd::Pid::from_raw(compositor));
    nix::sys::signal::kill(resumed.0, nix::sys::signal::Signal::SIGSTOP).expect("stop owned compositor");
    let begun = Instant::now();
    client.send(3, "tools/call", json!({"name":"launch_app","arguments":{"command":"konsole --separate --hold -e bash -c 'printf launch-deadline-proof'"}}));
    thread::sleep(Duration::from_millis(300));
    let ping_started = Instant::now();
    client.send(4, "ping", json!({}));
    assert_eq!(client.response(4, Duration::from_secs(2))["result"], json!({}));
    let ping_ms = ping_started.elapsed().as_millis();
    let timed_out = client.response(3, Duration::from_secs(25));
    let elapsed_ms = begun.elapsed().as_millis();
    assert_eq!(timed_out["error"]["data"]["reason"], "launch_timeout", "{timed_out}");
    assert_eq!(timed_out["error"]["data"]["command_submitted"], false, "{timed_out}");
    assert!(timed_out["error"]["data"]["stage"].as_str().expect("stage").contains("active window"));
    assert!((19_000..23_000).contains(&elapsed_ms), "timeout duration {elapsed_ms}ms");
    eprintln!("launch fault proof: server={} compositor={compositor} ping_ms={ping_ms} timeout_ms={elapsed_ms} response={timed_out}", client.child.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let scripts = std::fs::read_dir(&workdir).expect("workdir").flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("kwin-mcp-") && entry.path().extension().is_some_and(|ext| ext == "js")).count();
        if scripts == 0 { break }
        assert!(Instant::now() < deadline, "cancelled script file was retained");
        thread::sleep(Duration::from_millis(100));
    }
    let bus = zbus::blocking::connection::Builder::address(format!("unix:path={}", workdir.join("bus").display()).as_str())
        .expect("session bus address").build().expect("session bus connection");
    let server_name = started["result"]["structuredContent"]["bus"].as_str().expect("server bus name");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let reply = bus.call_method(Some(server_name), "/KWinMCP", Some("org.freedesktop.DBus.Introspectable"), "Introspect", &());
        let empty = match reply {
            Ok(reply) => !reply.body().deserialize::<String>().expect("introspection XML").contains("<node name="),
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" => true,
            Err(error) => panic!("callback inspection failed: {error}"),
        };
        if empty { break }
        assert!(Instant::now() < deadline, "cancelled callback registration was retained");
        thread::sleep(Duration::from_millis(100));
    }
    drop(resumed);
    let launched = call_tool(&mut client, 5, "launch_app", json!({"command":"konsole --separate --hold -e bash -c 'printf launch-deadline-recovery'"}));
    assert!(launched["error"].is_null(), "{launched}");
    assert_ne!(launched["result"]["isError"], true, "{launched}");
    let windows = call_tool(&mut client, 6, "window_list", json!({}));
    assert!(windows.to_string().to_lowercase().contains("konsole"), "{windows}");
    let screenshot = call_tool(&mut client, 7, "screenshot", json!({"inline":false}));
    assert!(screenshot["error"].is_null(), "{screenshot}");
    if let Some(directory) = std::env::var_os("KWIN_MCP_PROOF_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("proof directory");
        let capture = screenshot["result"]["structuredContent"]["path"].as_str().expect("screenshot path");
        std::fs::copy(capture, directory.join("launch-recovery.png")).expect("copy screenshot");
        std::fs::write(directory.join("launch-fault.json"), json!({"server":client.child.id(),"compositor":compositor,"ping_ms":ping_ms,"timeout_ms":elapsed_ms,"response":timed_out,
            "commit":started["result"]["structuredContent"]["commit"],"version":started["result"]["structuredContent"]["version"]}).to_string()).expect("write fault proof");
    }
    let stopped = call_tool(&mut client, 8, "session_stop", json!({}));
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(!workdir.exists());
    client.stop_process();
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, Konsole, input devices, and a live GPU session"]
fn session_start_bounds_large_ancestor_copies_with_a_nested_mount() {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
    let fixture = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(format!(".cache/kwin-mcp-e2e-nested-{}-{nonce}", std::process::id()));
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }
    let _fixture = Fixture(fixture.clone());
    let home = fixture.join("home");
    for directory in [".config", ".local/share", ".cache", ".local/state", ".kde", "project/portal"] {
        std::fs::create_dir_all(home.join(directory)).expect("fixture directory");
    }
    let mount_source = fixture.join("mount-source");
    std::fs::create_dir(&mount_source).expect("mount source");
    std::fs::write(mount_source.join("visible.txt"), "mounted-data").expect("mounted fixture");
    let ordinary = home.join("project/ordinary.bin");
    let large = home.join("project/large.bin");
    let explicit = home.join("project/explicit.bin");
    let threshold = 64 * 1024 * 1024;
    for (path, length) in [(&ordinary, threshold), (&large, threshold + 1), (&explicit, threshold + 1)] {
        let mut file = std::fs::File::create(path).expect("fixture file");
        file.set_len(length).expect("sparse fixture size");
        file.write_all(b"host-data").expect("fixture header");
        file.seek(SeekFrom::End(-9)).expect("fixture tail position");
        file.write_all(b"host-tail").expect("fixture tail");
    }
    let clone_probe = fixture.join("clone-probe");
    let reflink = Command::new("cp").args(["--reflink=always", "--"])
        .arg(&large).arg(&clone_probe).stderr(Stdio::null()).status().expect("reflink probe").success();
    let _ = std::fs::remove_file(&clone_probe);
    let quote = |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
    let server = PathBuf::from(std::env::var("KWIN_MCP_E2E_SERVER")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned()));
    let wrapper = fixture.join("server.sh");
    std::fs::write(&wrapper, format!(
        "#!/bin/sh\nexec bwrap --die-with-parent --bind / / --dev-bind /dev /dev --proc /proc --ro-bind {} {} -- {} \"$@\"\n",
        quote(&mount_source), quote(&home.join("project/portal")), quote(&server)
    )).expect("write namespace wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).expect("wrapper mode");
    let home_text = home.display().to_string();
    let mut client = RpcClient::start_program(wrapper.to_str().expect("wrapper path"), None, false, false, &[
        ("HOME", &home_text),
        ("XDG_CONFIG_HOME", &format!("{home_text}/.config")),
        ("XDG_DATA_HOME", &format!("{home_text}/.local/share")),
        ("XDG_CACHE_HOME", &format!("{home_text}/.cache")),
        ("XDG_STATE_HOME", &format!("{home_text}/.local/state")),
        ("KDEHOME", &format!("{home_text}/.kde")),
    ]);
    initialize(&mut client);
    let started_at = Instant::now();
    let started = call_tool(&mut client, 2, "session_start", json!({
        "width":800, "height":600, "writable_paths":[explicit]
    }));
    assert!(started_at.elapsed() < Duration::from_secs(20), "startup exceeded deadline: {started}");
    let dir = workdir(&started);
    let disk = home.join(".cache/kwin-mcp").join(dir.file_name().expect("workdir name"));
    let fallback = started["result"]["structuredContent"]["oversized_read_only_files"]
        .as_array().expect("fallback report");
    assert_eq!(fallback.contains(&json!(large)), !reflink, "{started}");
    assert!(!fallback.contains(&json!(ordinary)) && !fallback.contains(&json!(explicit)), "{started}");
    if !reflink {
        assert!(!disk.join("overlay-root/project/large.bin").exists(), "eager oversized copy");
    }
    assert!(disk.join("overlay-root/project/explicit.bin").is_file(), "explicit writable copy missing");
    let repeat = call_tool(&mut client, 3, "session_start", json!({}));
    assert_eq!(repeat["result"]["structuredContent"]["oversized_read_only_files"], json!(fallback));
    let report = dir.join("ancestor-check.txt");
    let script = dir.join("ancestor-check.sh");
    let oversized_write = if reflink {
        format!("printf sessionxx | dd of={} bs=9 count=1 conv=notrunc status=none\n", quote(&large))
    } else {
        format!("if printf sessionxx | dd of={} bs=9 count=1 conv=notrunc status=none 2>/dev/null; then exit 1; fi\n", quote(&large))
    };
    std::fs::write(&script, format!(
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
        quote(&home.join("project/portal/visible.txt")), quote(&ordinary), quote(&large), quote(&explicit),
        quote(&ordinary), quote(&explicit), quote(&report)
    )).expect("session check script");
    let launched = call_tool(&mut client, 4, "launch_app", json!({
        "command": format!("konsole --hold -e bash {}", quote(&script))
    }));
    assert!(launched["error"].is_null(), "{launched}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::fs::read_to_string(&report).ok().as_deref() != Some("ANCESTOR_COPY_PASS\n") {
        assert!(Instant::now() < deadline, "session file checks did not pass: {launched}");
        thread::sleep(Duration::from_millis(100));
    }
    let screenshot = call_tool(&mut client, 5, "screenshot", json!({"inline":true}));
    assert!(!inline_png(&screenshot).is_empty(), "missing rendered output");
    if let Ok(proof) = std::env::var("KWIN_MCP_E2E_PROOF_DIR") {
        std::fs::create_dir_all(&proof).expect("proof directory");
        std::fs::write(PathBuf::from(&proof).join("nested-ancestor.png"), inline_png(&screenshot)).expect("retain screenshot");
        std::fs::write(PathBuf::from(&proof).join("nested-start.json"), started.to_string()).expect("retain start response");
    }
    for path in [&ordinary, &large, &explicit] {
        let mut file = std::fs::File::open(path).expect("host fixture");
        let mut header = [0; 9];
        file.read_exact(&mut header).expect("host header");
        assert_eq!(&header, b"host-data", "host file changed: {}", path.display());
        assert_eq!(file.metadata().expect("host metadata").nlink(), 1, "host hardlink created");
    }
    let stopped = call_tool(&mut client, 6, "session_stop", json!({}));
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(!dir.exists() && !disk.exists(), "owned workdirs remain");
    client.stop_process();
    println!("PASS: nested mount, 64 MiB writable boundary, oversized readable/private, explicit writable copy, rendered output, host files unchanged, cleanup; reflink={reflink}");
}
