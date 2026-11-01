#![allow(clippy::expect_used)]

//! With `--keep-sessions`, a server whose MCP client closed keeps its running
//! session, and a second server (a respawned host) reaches it by session ID
//! through the control socket. Stopping the last session lets the first exit.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(home: &Path, extra_args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
            .args(["--autoclean", "--no-viewer"])
            .args(extra_args)
            .env("HOME", home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let mut server = Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        };
        let reply = server.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"keep-sessions-e2e","version":"1"}
            }),
        );
        assert!(reply["result"].is_object(), "{reply}");
        let stdin = server.stdin.as_mut().expect("open stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("send initialized");
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let stdin = self.stdin.as_mut().expect("open stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .expect("send request");
        stdin.flush().expect("flush request");
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read reply");
            assert!(read > 0, "server closed stdout before reply {id}");
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message["id"] == id
            {
                return message;
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name, "arguments":arguments}))
    }
}

/// Whether the server process is still running. `/proc/<pid>` is not enough:
/// an exited child stays there as a zombie until its parent waits for it.
fn running(server: &mut Server) -> bool {
    server.child.try_wait().expect("poll server").is_none()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn a_second_server_reaches_a_session_whose_client_closed() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-keep-sessions-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    let cleanup = PathBuf::from(&home);
    let mut first = Server::start(&home, &["--keep-sessions", "5"]);
    let first_pid = first.child.id();

    let started = first.tool(
        "session_start",
        json!({"browser_profiles":"fresh", "width":1280, "height":720}),
    );
    let session = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(
        session.starts_with(&format!("s{first_pid}-")),
        "session ID does not name its server: {session}"
    );
    let launched = first.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");

    // The MCP client closes: the server stays up because it owns a session.
    drop(first.stdin.take());
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        running(&mut first),
        "the server exited with its client although it owns a running session"
    );

    let mut second = Server::start(&home, &[]);
    let windows = second.tool("window_list", json!({"session_id":session}));
    assert!(windows["error"].is_null(), "not forwarded: {windows}");
    let listed = windows["result"]["structuredContent"]["windows"]
        .as_array()
        .expect("windows");
    assert!(!listed.is_empty(), "the forwarded session lists no window");

    let stopped = second.tool("session_stop", json!({"session_id":session}));
    assert!(stopped["error"].is_null(), "{stopped}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while running(&mut first) {
        assert!(
            Instant::now() < deadline,
            "the first server stayed after its last session stopped"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_dir_all(cleanup);
}
