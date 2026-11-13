#![allow(clippy::expect_used)]

//! `session_start` with `host` runs the desktop under another kwin-mcp started
//! with `--listen`, reached over WebSocket with a shared secret. Calls with the
//! returned ID are routed there, and the listener stops the session when it is
//! stopped or when the server that started it goes away.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// A private HOME holding the shared secret, under `KWIN_MCP_TEST_TMPDIR` or
/// `/var/tmp`: the session's disk overlay lives in it, and `/tmp` has a quota.
fn home_with_secret(label: &str, secret: Option<&str>) -> PathBuf {
    let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    let home = root.join(format!("kwin-mcp-remote-{label}-{}", std::process::id()));
    let config = home.join(".config/kwin-mcp");
    std::fs::create_dir_all(&config).expect("create config directory");
    std::fs::create_dir_all(home.join(".cache")).expect("create cache directory");
    if let Some(secret) = secret {
        let path = config.join("remote-secret");
        std::fs::write(&path, format!("{secret}\n")).expect("write secret");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict secret");
    }
    home
}

fn kwin_mcp(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"));
    command
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("XDG_CONFIG_HOME");
    command
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|socket| socket.local_addr())
        .expect("reserve a port")
        .port()
}

struct Listener {
    child: Child,
    address: String,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Listener {
    fn start(home: &Path) -> Self {
        Self::start_at(home, format!("127.0.0.1:{}", free_port()))
    }

    fn start_at(home: &Path, address: String) -> Self {
        let mut child = kwin_mcp(home)
            .args(["--listen", &address, "--autoclean"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn listener");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(&address).is_err() {
            assert!(
                child.try_wait().expect("poll listener").is_none(),
                "the listener exited before accepting connections"
            );
            assert!(Instant::now() < deadline, "the listener never accepted");
            std::thread::sleep(Duration::from_millis(100));
        }
        Self { child, address }
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
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
    fn start(home: &Path) -> Self {
        let mut child = kwin_mcp(home)
            .arg("--autoclean")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let reply = server.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"remote-session-e2e","version":"1"}
            }),
        );
        assert!(reply["result"].is_object(), "{reply}");
        writeln!(
            server.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("send initialized");
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .expect("send request");
        self.stdin.flush().expect("flush request");
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

    /// Starts a session on `host` and returns its ID and sandbox PID.
    fn start_remote(&mut self, host: &str) -> (String, u64) {
        let started = self.tool(
            "session_start",
            json!({"host":host, "width":1280, "height":720}),
        );
        let session = started["result"]["structuredContent"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {started}"))
            .to_owned();
        let listed = self.tool("session_list", json!({}));
        let mut record = Value::Null;
        for listed in listed["result"]["structuredContent"]["sessions"]
            .as_array()
            .expect("session records")
        {
            if listed["session_id"] == session.as_str() {
                record = listed.clone();
            }
        }
        assert_eq!(record["host"], host, "remote session not listed: {listed}");
        assert_eq!(record["status"], "running", "{record}");
        let sandbox = record["sandbox_pid"].as_u64().expect("sandbox pid");
        (session, sandbox)
    }
}

/// Waits until process `pid` is gone.
fn exited(pid: u64, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Path::new(&format!("/proc/{pid}")).exists() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    true
}

#[test]
fn the_listener_needs_the_secret_and_refuses_a_peer_without_it() {
    // No secret file: nothing listens.
    let bare = home_with_secret("bare", None);
    let refused = kwin_mcp(&bare)
        .args(["--listen", &format!("127.0.0.1:{}", free_port())])
        .stdin(Stdio::null())
        .output()
        .expect("run listener");
    assert!(!refused.status.success(), "listened without a secret file");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("remote-secret"),
        "{refused:?}"
    );

    // A peer holding another secret is turned away before any session starts.
    let listener_home = home_with_secret("listener", Some("listener-secret-0123456789"));
    let listener = Listener::start(&listener_home);
    let stranger_home = home_with_secret("stranger", Some("another-secret-0123456789"));
    let mut stranger = Server::start(&stranger_home);
    let started = stranger.tool("session_start", json!({"host":listener.address}));
    assert_eq!(
        started["error"]["data"]["reason"], "remote_host_unreachable",
        "{started}"
    );
    assert!(
        !started.to_string().contains("secret-0123456789"),
        "a secret leaked into the reply: {started}"
    );
    drop(stranger);
    drop(listener);
    for home in [bare, listener_home, stranger_home] {
        let _ = std::fs::remove_dir_all(home);
    }
}

/// The listener's own answer to a session_start it refuses at once.
fn refused_start(server: &mut Server, host: &str) -> Value {
    server.tool(
        "session_start",
        json!({"host":host, "startup_timeout_seconds":0}),
    )
}

#[test]
fn a_restarted_listener_is_dialed_again_under_the_same_name() {
    let home = home_with_secret("redial", Some("redial-secret-0123456789"));
    let listener = Listener::start(&home);
    let host = listener.address.clone();
    let mut server = Server::start(&home);
    let answered = refused_start(&mut server, &host);
    assert!(
        answered["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("startup_timeout_seconds")),
        "the listener did not answer: {answered}"
    );
    // The listener stops and starts again on the same address (#960).
    drop(listener);
    let listener = Listener::start_at(&home, host.clone());
    for attempt in ["at once", "after the old connection ended"] {
        let answered = refused_start(&mut server, &host);
        assert!(
            answered["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("startup_timeout_seconds")),
            "{attempt}: the restarted listener was not dialed again: {answered}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    drop(server);
    drop(listener);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn a_session_of_an_ended_connection_reports_it_and_a_new_one_starts() {
    let home = home_with_secret("lost", Some("lost-secret-0123456789"));
    let listener = Listener::start(&home);
    let host = listener.address.clone();
    let mut server = Server::start(&home);
    let (old, sandbox) = server.start_remote(&host);
    drop(listener);
    assert!(
        exited(sandbox, Duration::from_secs(30)),
        "the stopped listener left its session running"
    );
    let listener = Listener::start_at(&home, host.clone());
    let (new, _) = server.start_remote(&host);
    assert_ne!(old, new);
    let routed = server.tool("window_list", json!({"session_id":old}));
    assert_eq!(
        routed["error"]["data"]["reason"], "remote_connection_lost",
        "a session of the ended connection went to the new one: {routed}"
    );
    let listed = server.tool("session_list", json!({}));
    let sessions = listed["result"]["structuredContent"]["sessions"]
        .as_array()
        .expect("session records");
    let status = |id: &str| {
        sessions
            .iter()
            .filter(|record| record["session_id"] == id)
            .map(|record| record["status"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(status(&old), [json!("connection_lost")], "{listed}");
    assert_eq!(status(&new), [json!("running")], "{listed}");
    let cleared = server.tool("session_stop", json!({"session_id":old}));
    assert_eq!(
        cleared["result"]["structuredContent"]["session_status"], "connection_lost",
        "{cleared}"
    );
    let stopped = server.tool("session_stop", json!({"session_id":new}));
    assert!(stopped["error"].is_null(), "{stopped}");
    drop(server);
    drop(listener);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn a_session_runs_on_the_listener_and_ends_with_its_owner() {
    let home = home_with_secret("session", Some("shared-secret-0123456789"));
    let mut listener = Listener::start(&home);
    let listener_pid = listener.child.id();
    let host = listener.address.clone();
    let mut server = Server::start(&home);

    let (session, sandbox) = server.start_remote(&host);
    assert!(
        session.starts_with(&format!("s{listener_pid}-")),
        "the session does not belong to the listener: {session}"
    );

    let launched = server.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "not routed: {launched}");
    let windows = server.tool("window_list", json!({"session_id":session}));
    assert!(windows["error"].is_null(), "not routed: {windows}");
    let listed = windows["result"]["structuredContent"]["windows"]
        .as_array()
        .expect("windows");
    assert!(!listed.is_empty(), "the remote session lists no window");
    let shot = server.tool("screenshot", json!({"session_id":session, "inline":true}));
    assert!(shot["error"].is_null(), "not routed: {shot}");
    let image = shot["result"]["content"]
        .as_array()
        .is_some_and(|content| content.iter().any(|part| part["type"] == "image"));
    assert!(image, "the remote screenshot carries no image: {shot}");

    // The viewer of a remote session is covered by remote_viewer_e2e.rs.
    let stopped = server.tool("session_stop", json!({"session_id":session}));
    assert!(stopped["error"].is_null(), "{stopped}");
    assert!(
        exited(sandbox, Duration::from_secs(20)),
        "session_stop left the remote sandbox running"
    );

    // The owning server dies without stopping its second session.
    let (_, orphan) = server.start_remote(&host);
    server.child.kill().expect("kill the owning server");
    server.child.wait().expect("reap the owning server");
    assert!(
        exited(orphan, Duration::from_secs(30)),
        "the listener kept a session whose owner is gone"
    );
    assert!(
        listener.child.try_wait().expect("poll listener").is_none(),
        "the listener exited with its peer"
    );
    drop(server);
    drop(listener);
    let _ = std::fs::remove_dir_all(home);
}
