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
}

impl RpcClient {
    fn start() -> Self {
        Self::start_with_options(None, false)
    }

    fn start_with_options(delay_stage: Option<&str>, viewer: bool) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let home =
            std::env::temp_dir().join(format!("kwin-mcp-e2e-home-{}-{nonce}", std::process::id()));
        for directory in [
            home.join(".config"),
            home.join(".local/share"),
            home.join(".cache"),
            home.join(".local/state"),
            home.join(".kde"),
        ] {
            std::fs::create_dir_all(directory).expect("create private test HOME");
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"));
        let arguments = if viewer {
            vec!["--autoclean"]
        } else {
            vec!["--no-viewer", "--autoclean"]
        };
        command
            .args(arguments)
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

    fn wait_for_stderr(&self, text: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .stderr
                .recv_timeout(remaining)
                .expect("kwin-mcp startup log");
            if line.contains(text) {
                return;
            }
        }
    }

    fn stop_process(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

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
    }
}

fn call_tool(client: &mut RpcClient, id: u64, name: &str, arguments: Value) -> Value {
    client.send(
        id,
        "tools/call",
        json!({"name":name, "arguments":arguments}),
    );
    client.response(id, Duration::from_secs(45))
}

fn process_children(pid: u32) -> Vec<String> {
    std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn process_threads(pid: u32) -> Vec<String> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
        .map(|name| name.trim().to_owned())
        .collect()
}

fn workdir(response: &Value) -> PathBuf {
    response["result"]["structuredContent"]["workdir"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("session_start did not return a workdir: {response}"))
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

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, and a live GPU session"]
fn startup_timeout_reclaims_children_endpoint_and_workdir() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );

    for (stage, viewer) in [("after-bwrap", false), ("after-viewer-endpoint", true)] {
        let mut client = RpcClient::start_with_options(Some(stage), viewer);
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
        let start = call_tool(
            &mut client,
            2,
            "session_start",
            json!({"width":800,"height":600}),
        );
        assert!(
            start["error"].is_object() || start["result"]["isError"].as_bool() == Some(true),
            "delayed startup unexpectedly succeeded: {start}"
        );
        let workdir = PathBuf::from(format!("/tmp/kwin-mcp-{server_pid}"));
        assert!(!workdir.exists(), "timeout left {}", workdir.display());
        assert!(
            process_children(server_pid).is_empty(),
            "timeout left child processes for {stage}: {:?}",
            process_children(server_pid)
        );
        assert!(
            !process_threads(server_pid)
                .iter()
                .any(|name| name == "viewer-endpoint"),
            "timeout left endpoint thread for {stage}"
        );
        client.stop_process();
    }
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
    let launched = call_tool(
        &mut client,
        3,
        "launch_app",
        json!({"command":"google-chrome-stable --no-first-run https://example.com && echo compound-done"}),
    );
    let window = launched["result"]["structuredContent"]["window"]
        .as_str()
        .unwrap_or("error");
    assert_ne!(
        window, "timeout",
        "Chrome did not create a managed window: {launched}"
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
        "needle=--force-renderer-$(printf accessibility); for d in /proc/[0-9]*; do p=\"$d/cmdline\"; cmd=$(tr '\\0' ' ' <\"$p\" 2>/dev/null) || continue; case \"$cmd\" in *\"$needle\"*) printf '%s\\n' \"$cmd\" > '{}'; break;; esac; done; sleep 2",
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
