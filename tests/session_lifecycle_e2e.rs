use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

struct RpcClient {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
    stderr: Receiver<String>,
    pending: HashMap<u64, Value>,
}

impl RpcClient {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
            .args(["--no-viewer", "--autoclean"])
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
        drop(self.stdin);
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
