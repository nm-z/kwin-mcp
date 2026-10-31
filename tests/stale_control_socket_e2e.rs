#![allow(clippy::expect_used)]

//! A server killed without unlinking its control socket leaves a file behind;
//! the next server removes it, and a session ID that names the dead server says
//! so. Everything happens before any session exists, so no KDE or display is
//! needed.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("send request");
        self.stdin.flush().expect("flush request");
    }

    fn reply(&mut self, id: u64) -> Value {
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
}

/// A pid with no process, so no live `kwin-mcp` can be it.
fn dead_pid(start: u32) -> u32 {
    (2..=start)
        .rev()
        .find(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        .expect("a free pid")
}

fn socket(runtime: &Path, name: &str) -> PathBuf {
    runtime.join(name)
}

fn start_server(home: &Path, runtime: &Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--no-viewer"])
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_RUNTIME_DIR", runtime)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let stdin = child.stdin.take().expect("server stdin");
    let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let mut server = Server {
        child,
        stdin,
        stdout,
    };
    server.send(&json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize",
        "params":{
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"stale-control-socket-e2e","version":"1"}
        }
    }));
    let reply = server.reply(1);
    assert!(reply["result"].is_object(), "initialize failed: {reply}");
    server.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    server
}

#[test]
fn stale_control_sockets_are_swept_and_the_dead_owner_is_named() {
    let root = std::env::temp_dir().join(format!("kwin-mcp-stale-socket-{}", std::process::id()));
    let fixture = Fixture(root);
    let home = fixture.0.join("home");
    let runtime = fixture.0.join("run");
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    std::fs::create_dir_all(&runtime).expect("create runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("restrict runtime directory");

    let dead = dead_pid(4_194_000);
    let stale = socket(&runtime, &format!("kwin-mcp-control-{dead}.sock"));
    // Dropping a bound listener leaves its socket file with nothing behind it,
    // exactly what a server killed without cleanup leaves.
    drop(UnixListener::bind(&stale).expect("bind stale socket"));
    // A listener that still answers must survive, whatever pid its name carries.
    let answering = socket(
        &runtime,
        &format!("kwin-mcp-control-{}.sock", dead_pid(4_193_000)),
    );
    let _listener = UnixListener::bind(&answering).expect("bind answering socket");
    // Names that are not control sockets are not ours to remove.
    let unrelated = socket(&runtime, "kwin-mcp-control-abc.sock");
    drop(UnixListener::bind(&unrelated).expect("bind unrelated socket"));

    let mut server = start_server(&home, &runtime);
    assert!(
        !stale.exists(),
        "stale control socket was not removed: {}",
        stale.display()
    );
    assert!(answering.exists(), "an answering control socket was removed");
    assert!(unrelated.exists(), "an unrelated socket name was removed");

    server.send(&json!({
        "jsonrpc":"2.0", "id":2, "method":"tools/call",
        "params":{"name":"screenshot", "arguments":{"session_id":format!("s{dead}-1")}}
    }));
    let reply = server.reply(2);
    let error = &reply["error"];
    let message = error["message"].as_str().expect("error message");
    assert!(
        message.contains(&format!("process {dead}, which is no longer running")),
        "the error did not say the owner exited: {message}"
    );
    assert_eq!(error["data"]["reason"], "different_server_process");
    assert_eq!(error["data"]["session_server_running"], false);
}
