#![allow(clippy::expect_used)]

//! Real production-stdio regression for #852. NoNewPrivs makes the bwrap
//! launched by pasta fail its UID map. Both retained and autocleaned workdirs
//! must report that stderr and the existing namespace guidance to the caller.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const UID_MAP_REFUSAL: &str = "bwrap: setting up uid map: Operation not permitted";

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
}

impl Server {
    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("open server stdin");
        writeln!(stdin, "{message}").expect("send request");
        stdin.flush().expect("flush request");
    }

    fn reply(&self, id: u64) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let response = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("server must answer within the deadline");
            if response["id"] == id {
                return response;
            }
        }
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(json!({
            "jsonrpc":"2.0", "id":id, "method":"tools/call",
            "params":{"name":name, "arguments":arguments}
        }));
        self.reply(id)
    }

    fn close(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("check server exit") {
                assert!(
                    status.success(),
                    "server failed after stdin closed: {status}"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "server did not exit on stdin EOF"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn refused_uid_map_is_reported(autoclean: bool) {
    let mode = if autoclean { "autoclean" } else { "retained" };
    let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let fixture = Fixture(root.join(format!("k852-{}-{mode}", std::process::id())));
    let home = fixture.0.join("home");
    let runtime = fixture.0.join("run");
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    std::fs::create_dir_all(&runtime).expect("create private runtime root");
    let server = std::env::var_os("KWIN_MCP_E2E_SERVER")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_kwin-mcp").into());
    let mut command = Command::new("setpriv");
    command.args(["--no-new-privs", "--"]).arg(server);
    command.arg("--no-viewer");
    if autoclean {
        command.arg("--autoclean");
    }
    let mut child = command
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".state"))
        .env("TMPDIR", &runtime)
        // Match the raw invocation in #852 and exercise validated host fallback.
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("KWIN_MCP_REPO", home.join("no-repository"))
        .env("CARGO", "/usr/bin/false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(fixture.0.join("server.stderr")).expect("create stderr log"),
        ))
        .spawn()
        .expect("spawn real kwin-mcp under NoNewPrivs");
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("server stdout");
    let (sender, responses) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && sender.send(message).is_err()
            {
                break;
            }
        }
    });
    let mut server = Server {
        child,
        stdin,
        responses,
    };
    server.send(json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize",
        "params":{
            "protocolVersion":"2025-03-26", "capabilities":{},
            "clientInfo":{"name":"startup-uid-map-e2e","version":"1"}
        }
    }));
    assert!(server.reply(1)["result"].is_object());
    server.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let response = server.call(
        2,
        "session_start",
        json!({"browser_profiles":"fresh", "startup_timeout_seconds":30}),
    );
    eprintln!("{mode}: {response}");
    let message = response["error"]["message"]
        .as_str()
        .expect("session_start must fail");
    let data = &response["error"]["data"];
    let session = data["session_id"].as_str().expect("failed session handle");
    let workdir = runtime.join(format!("kwin-mcp-{}-1", server.child.id()));
    let log = home
        .join(".cache/kwin-mcp")
        .join(workdir.file_name().expect("workdir basename"))
        .join("sandbox.log");
    // Check the real retained log independently of the tool result. The
    // autoclean mode must remove the same disk twin before replying.
    if autoclean {
        assert!(!workdir.exists(), "autoclean retained the workdir");
        assert!(!log.exists(), "autoclean retained sandbox stderr");
    } else {
        assert!(workdir.is_dir(), "default mode removed the workdir");
        let stderr = std::fs::read_to_string(&log).expect("read retained sandbox log");
        assert!(stderr.contains(UID_MAP_REFUSAL), "{stderr}");
        assert_eq!(data["sandbox_stderr_tail"], stderr, "{response}");
    }
    assert!(message.contains("before dbus-ready marker"), "{message}");
    assert!(message.contains(UID_MAP_REFUSAL), "{message}");
    assert!(
        message.contains("could not create the session's user namespace"),
        "no guidance in: {message}"
    );
    assert_eq!(data["sandbox_stderr_path"], json!(log), "{response}");
    assert_eq!(data["sandbox_stderr_truncated"], false, "{response}");
    assert!(
        data["sandbox_stderr_tail"]
            .as_str()
            .is_some_and(|tail| tail.contains(UID_MAP_REFUSAL)),
        "{response}"
    );
    let guidance = data["guidance"].as_str().expect("structured guidance");
    assert!(guidance.contains("NoNewPrivs: 1"), "{guidance}");
    assert!(guidance.contains("systemd-run --user"), "{guidance}");
    assert!(data["input_log"].is_string(), "{response}");

    let stopped = server.call(3, "session_stop", json!({"session_id":session}));
    assert!(stopped["error"].is_null(), "{stopped}");
    assert_eq!(stopped["result"]["isError"], false, "{stopped}");
    let listed = server.call(4, "session_list", json!({}));
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"],
        json!([]),
        "{listed}"
    );
    server.close();
    assert_eq!(workdir.exists(), !autoclean);
    assert_eq!(log.exists(), !autoclean);
}

#[test]
#[ignore = "requires pasta, non-setuid bwrap, uinput, setpriv and systemd user D-Bus"]
fn a_refused_uid_map_is_reported_without_autoclean() {
    refused_uid_map_is_reported(false);
}

#[test]
#[ignore = "requires pasta, non-setuid bwrap, uinput, setpriv and systemd user D-Bus"]
fn a_refused_uid_map_is_reported_with_autoclean() {
    refused_uid_map_is_reported(true);
}
