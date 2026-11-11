#![allow(clippy::expect_used)]

//! A server that inherits `no_new_privs` must start sessions itself (#906).
//! Every bwrap-based sandbox sets it, so a server started by a sandboxed
//! worker, a mux shim that worker runs, or this test runner inside such a
//! checkout inherits it. pasta then cannot start bwrap: the command it runs
//! loses `CAP_SETFCAP` and bwrap cannot map ids.
//!
//! Both cases start the server over stdio the way such a parent would and
//! verify the desktop from outside: a window that maps, a screenshot of it,
//! a file read through the sandbox root, a connection from the session to a
//! host listener, and that nothing of the session is left after `session_stop`.
//!
//! Run with `KWIN_MCP_E2E=1 cargo test --test no_new_privs_e2e -- --ignored
//! --test-threads=1`. `KWIN_MCP_E2E_SERVER` names another server binary and
//! `KWIN_MCP_E2E_PROOF_DIR` keeps each case's screenshot.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const CALL_TIMEOUT: Duration = Duration::from_secs(90);
const GONE_TIMEOUT: Duration = Duration::from_secs(15);
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const WINDOW_TITLE: &str = "kwin-mcp-nnp-proof";

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    home: PathBuf,
    next_id: u64,
}

impl Server {
    /// Start `launcher... kwin-mcp --no-viewer --autoclean` over stdio.
    fn start(launcher: &[&str]) -> Self {
        let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
            .or_else(|| std::env::var_os("TMPDIR"))
            .filter(|value| !value.is_empty())
            .map_or_else(|| PathBuf::from("/var/tmp"), PathBuf::from);
        let home = root.join(format!("kmn-{}-{}", std::process::id(), launcher[0]));
        for directory in [".config", ".local/share", ".cache", ".local/state"] {
            std::fs::create_dir_all(home.join(directory)).expect("create private HOME");
        }
        let server = std::env::var("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned());
        let mut child = Command::new(launcher[0])
            .args(&launcher[1..])
            .arg(server)
            .args(["--no-viewer", "--autoclean"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the server through its launcher");
        let stdout = child.stdout.take().expect("server stdout");
        let (sender, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut server = Self {
            child,
            stdin,
            responses,
            home,
            next_id: 0,
        };
        let initialized = server.rpc(
            "initialize",
            json!({"protocolVersion":"2025-06-18", "capabilities":{},
                   "clientInfo":{"name":"no-new-privs-e2e","version":"1"}}),
        );
        assert!(
            initialized["result"].is_object(),
            "initialize: {initialized}"
        );
        server.write(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        server
    }

    fn write(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("open server stdin");
        writeln!(stdin, "{message}").expect("write request");
        stdin.flush().expect("flush request");
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let response = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("{method} #{id}: {error}"));
            if response["id"] == json!(id) {
                return response;
            }
        }
    }

    /// A tool call that must succeed; returns its structured content.
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.rpc("tools/call", json!({"name":name, "arguments":arguments}));
        assert!(
            response["error"].is_null() && response["result"]["isError"] != json!(true),
            "{name} failed: {response}"
        );
        response["result"]["structuredContent"].clone()
    }

    /// Close the transport and wait for the server to exit on its own.
    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + GONE_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                assert!(status.success(), "server exited with {status}");
                break;
            }
            assert!(Instant::now() < deadline, "server did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_dir_all(&self.home).expect("remove private HOME");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Only reached with the child still running when an assertion failed.
        // Closing the transport makes the server stop its own sessions.
        drop(self.stdin.take());
        let deadline = Instant::now() + GONE_TIMEOUT;
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn status_field(pid: u32, name: &str) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .expect("read process status")
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| panic!("{name} missing for process {pid}"))
}

/// (PID, command name, start time) of every process in the session's scope.
fn scope_members(unit: &str) -> Vec<(u32, String, String)> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "-p", "ControlGroup", "--value", unit])
        .output()
        .expect("ask the user manager for the session scope");
    let group = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(!group.is_empty(), "{unit} has no control group");
    std::fs::read_to_string(format!("/sys/fs/cgroup{group}/cgroup.procs"))
        .expect("read the scope's processes")
        .lines()
        .filter_map(|line| line.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let name = stat.split_once('(')?.1.rsplit_once(')')?.0.to_owned();
            // Field 22 of stat, 20 fields after the command name.
            let started = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .to_owned();
            Some((pid, name, started))
        })
        .collect()
}

fn still_running((pid, _, started): &(u32, String, String)) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let now = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .to_owned();
            Some(&now == started)
        })
        .unwrap_or(false)
}

fn session_works_under(launcher: &[&str], single_uid_namespace: bool) {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a host listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking host listener");
    let port = listener.local_addr().expect("listener address").port();

    let mut server = Server::start(launcher);
    let started = server.tool(
        "session_start",
        json!({"width":1280, "height":720, "startup_timeout_seconds":60}),
    );
    let session = started["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let workdir = PathBuf::from(started["host_shared_dir"].as_str().expect("workdir"));

    // The session ID names the server process: s<pid>-<n>.
    let server_pid: u32 = session
        .strip_prefix('s')
        .and_then(|rest| rest.split('-').next())
        .and_then(|pid| pid.parse().ok())
        .expect("server PID in the session ID");
    assert_eq!(status_field(server_pid, "NoNewPrivs:"), "1");
    let uid_map = std::fs::read_to_string(format!("/proc/{server_pid}/uid_map"))
        .expect("server uid_map")
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        uid_map[2] == "1",
        single_uid_namespace,
        "server uid_map {uid_map:?}"
    );

    let unit = format!(
        "{}.scope",
        workdir.file_name().expect("workdir name").to_string_lossy()
    );
    let members = scope_members(&unit);
    // pasta is a link to passt, which may run as an architecture variant
    // such as passt.avx2.
    for programs in [&["pasta", "passt"][..], &["bwrap"][..]] {
        assert!(
            members
                .iter()
                .any(|(_, name, _)| programs.iter().any(|program| name.starts_with(program))),
            "none of {programs:?} is in {unit}: {members:?}"
        );
    }

    let launched = server.tool(
        "launch_app",
        json!({"session_id":session, "window_timeout_seconds":40,
               "command":format!("kdialog --title {WINDOW_TITLE} --msgbox 'started under no_new_privs'")}),
    );
    let windows = server.tool("window_list", json!({"session_id":session}));
    assert!(
        windows.to_string().contains(WINDOW_TITLE),
        "no {WINDOW_TITLE} window after {launched}: {windows}"
    );
    let shot = server.tool("screenshot", json!({"session_id":session}));
    let png = PathBuf::from(shot["path"].as_str().expect("screenshot path"));
    let mut magic = [0u8; 8];
    std::fs::File::open(&png)
        .and_then(|mut file| file.read_exact(&mut magic))
        .expect("read the screenshot");
    assert_eq!(magic, PNG_MAGIC, "{} is not a PNG", png.display());
    if let Some(proof) = std::env::var_os("KWIN_MCP_E2E_PROOF_DIR").map(PathBuf::from) {
        std::fs::create_dir_all(&proof).expect("proof directory");
        std::fs::copy(
            &png,
            proof.join(format!("no-new-privs-{}.png", launcher[0])),
        )
        .expect("keep the screenshot");
    }

    // A file in the session's private /tmp is reachable only through the
    // sandbox root, and the connection only through the network pasta serves.
    let _ = server.tool(
        "launch_app",
        json!({"session_id":session, "expect_window":false,
               "command":format!("printf file-proof > /tmp/nnp-proof.txt; for attempt in $(seq 50); do (printf net-proof > /dev/tcp/127.0.0.1/{port}) 2>/dev/null && break; sleep 0.2; done")}),
    );
    let deadline = Instant::now() + GONE_TIMEOUT;
    let mut received = String::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                stream
                    .set_read_timeout(Some(GONE_TIMEOUT))
                    .expect("stream timeout");
                stream
                    .read_to_string(&mut received)
                    .expect("read from the session");
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the session never reached host port {port}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
    assert_eq!(received, "net-proof");
    let exported = server.home.join("exported-proof.txt");
    let _ = server.tool(
        "export_file",
        json!({"session_id":session, "session_path":"/tmp/nnp-proof.txt",
               "host_path":exported}),
    );
    assert_eq!(
        std::fs::read_to_string(&exported).expect("exported file"),
        "file-proof"
    );

    let members = scope_members(&unit);
    let stopped = server.tool("session_stop", json!({"session_id":session}));
    assert_eq!(stopped["status"], json!("stopped"), "{stopped}");
    let deadline = Instant::now() + GONE_TIMEOUT;
    loop {
        let left: Vec<_> = members
            .iter()
            .filter(|member| still_running(member))
            .collect();
        if left.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session processes outlived session_stop: {left:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let active = Command::new("systemctl")
        .args(["--user", "is-active", &unit])
        .output()
        .expect("query the session scope");
    assert_ne!(
        String::from_utf8_lossy(&active.stdout).trim(),
        "active",
        "{unit} is still active"
    );
    assert!(!workdir.exists(), "{} was not removed", workdir.display());
    let twins = server.home.join(".cache/kwin-mcp");
    let leftover: Vec<_> = std::fs::read_dir(&twins)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.file_name() == workdir.file_name())
                .collect()
        })
        .unwrap_or_default();
    assert!(leftover.is_empty(), "disk twin left behind: {leftover:?}");
    server.finish();
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| Path::new(&dir).join(program).is_file())
    })
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, kdialog, setpriv, and input devices"]
fn a_server_started_with_no_new_privs_runs_a_session() {
    assert!(on_path("setpriv"), "setpriv (util-linux) is required");
    session_works_under(&["setpriv", "--no-new-privs"], false);
}

/// What the reports show: `uid_map: 1000 1000 1` beside `NoNewPrivs: 1` is a
/// server inside another bwrap sandbox's user namespace.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, kdialog, and input devices"]
fn a_server_inside_a_bwrap_sandbox_runs_a_session() {
    session_works_under(&["bwrap", "--unshare-user", "--dev-bind", "/", "/"], true);
}
