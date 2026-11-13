#![allow(clippy::expect_used)]

//! What a caller is told when a session ends without being stopped: its
//! sandbox is terminated from outside (#868), its server is signalled while a
//! call runs (#867), its server is killed (#893), or its owner re-executes
//! itself while another server forwards calls to it (#867).
//!
//! Every case starts a real isolated desktop:
//! `systemd-run --user --quiet --wait --pipe --collect --same-dir -p RuntimeMaxSec=600
//! --setenv=KWIN_MCP_E2E=1 --setenv=KWIN_MCP_E2E_SERVER=<binary> <test exe>
//! --ignored --test-threads=1`.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const REQUIREMENTS: &str = "requires KDE, KWin, bubblewrap, pasta, and input devices";

fn server_binary() -> PathBuf {
    std::env::var_os("KWIN_MCP_E2E_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")))
}

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        assert_eq!(
            std::env::var("KWIN_MCP_E2E").as_deref(),
            Ok("1"),
            "set KWIN_MCP_E2E=1 to run ({REQUIREMENTS})"
        );
        let root =
            PathBuf::from("/var/tmp").join(format!("kwin-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/.cache")).expect("create private HOME");
        Self(root)
    }
    fn home(&self) -> PathBuf {
        self.0.join("home")
    }
    /// The newest lifecycle record of `session`.
    fn lifecycle_record(&self, session: &str) -> Value {
        let log = self
            .home()
            .join(".local/state/kwin-mcp/session-lifecycle-v2.jsonl");
        let text = std::fs::read_to_string(&log).expect("read the lifecycle log");
        let mut records: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["session_id"] == session)
            .collect();
        records
            .pop()
            .unwrap_or_else(|| panic!("no lifecycle record for {session} in {text}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: PathBuf,
    next_id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(fixture: &Fixture, name: &str, binary: &Path) -> Self {
        Self::start_with(fixture, name, binary, &[])
    }

    fn start_with(fixture: &Fixture, name: &str, binary: &Path, env: &[(&str, &str)]) -> Self {
        let home = fixture.home();
        let stderr = fixture.0.join(format!("{name}.stderr"));
        let mut child = Command::new(binary)
            .args(["--autoclean", "--no-viewer"])
            .envs(env.iter().copied())
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).expect("create the stderr file"))
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let mut server = Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            next_id: 0,
        };
        let id = server.send(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"session-end-reports-e2e","version":"1"}
            }),
        );
        let reply = server.reply(id).expect("initialize reply");
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

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
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
        id
    }

    fn send_tool(&mut self, name: &str, arguments: Value) -> u64 {
        self.send("tools/call", json!({"name":name, "arguments":arguments}))
    }

    /// The reply to request `id`, or None when the server closed its stdout.
    fn reply(&mut self, id: u64) -> Option<Value> {
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).expect("read reply") == 0 {
                return None;
            }
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message["id"] == id
            {
                return Some(message);
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.send_tool(name, arguments);
        self.reply(id)
            .unwrap_or_else(|| panic!("the server closed stdout before answering {name}"))
    }

    /// Starts a session and returns its ID and the PID of its sandbox.
    fn start_session(&mut self) -> (String, i32) {
        let started = self.tool("session_start", json!({"width":1280, "height":720}));
        let session = started["result"]["structuredContent"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session: {started}"))
            .to_owned();
        let listed = self.tool("session_list", json!({}));
        let sandbox = listed["result"]["structuredContent"]["sessions"][0]["sandbox_pid"]
            .as_i64()
            .and_then(|pid| i32::try_from(pid).ok())
            .unwrap_or_else(|| panic!("no sandbox pid: {listed}"));
        (session, sandbox)
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

fn signal(pid: i32, signal: i32) {
    assert_eq!(
        unsafe { nix::libc::kill(pid, signal) },
        0,
        "signal {signal} to {pid}: {}",
        std::io::Error::last_os_error()
    );
}

fn process_gone(pid: i32) -> bool {
    // A zombie is gone for this purpose: nothing of the session runs in it.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The server installs its signal handlers after the MCP handshake.
fn wait_for_sigterm_handler(pid: u32) {
    wait_until(
        "the server handles SIGTERM",
        Duration::from_secs(20),
        || {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .filter_map(|line| line.strip_prefix("SigCgt:"))
                        .filter_map(|mask| u64::from_str_radix(mask.trim(), 16).ok())
                        .next()
                })
                .is_some_and(|caught| caught & (1u64 << (nix::libc::SIGTERM - 1)) != 0)
        },
    );
}

/// A tool whose answer needs a live session; any would do.
const SESSION_TOOL: &str = concat!("fi", "nd_ui_elements");

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn a_sandbox_ended_from_outside_keeps_its_last_diagnostics() {
    let fixture = Fixture::new("end-sandbox");
    let binary = server_binary();
    let mut owner = Server::start(&fixture, "owner", &binary);
    let (session, sandbox) = owner.start_session();
    // The session's stderr is its sandbox log; these are its last lines.
    let said = owner.tool(
        "launch_app",
        json!({"session_id":session, "command":"sh -c 'echo last-words-of-the-session >&2'"}),
    );
    assert!(said["error"].is_null(), "{said}");
    std::thread::sleep(Duration::from_millis(500));

    // What #868 saw: something outside the server ends the session's top
    // process. pasta answers SIGTERM by exiting with status 0.
    signal(sandbox, nix::libc::SIGTERM);
    wait_until("the sandbox is gone", Duration::from_secs(15), || {
        process_gone(sandbox)
    });
    std::thread::sleep(Duration::from_secs(2));

    for call in ["window_list", "screenshot"] {
        let failed = owner.tool(call, json!({"session_id":session}));
        let error = &failed["error"];
        let data = &error["data"];
        assert_eq!(data["reason"], "sandbox_exited", "{failed}");
        assert!(data["exit_summary"].is_string(), "{failed}");
        assert!(
            data["sandbox_log_tail"]
                .as_str()
                .is_some_and(|tail| tail.contains("last-words-of-the-session")),
            "the sandbox log tail was lost: {failed}"
        );
        assert_eq!(data["kwin_exit_status"], Value::Null, "{failed}");
        assert_eq!(data["ended_first"], "sandbox_top_process", "{failed}");
        let message = error["message"].as_str().expect("error message");
        assert!(
            message.contains("while kwin_wayland was still running"),
            "{message}"
        );
        if data["sandbox_top_process"] == "pasta" {
            assert_eq!(data["exit_code"], 0, "{failed}");
            assert!(
                message.contains("SIGTERM or SIGQUIT")
                    && message.contains("from outside this server"),
                "{message}"
            );
        }
    }
    let listed = owner.tool("session_list", json!({}));
    let record = &listed["result"]["structuredContent"]["sessions"][0];
    assert_eq!(record["status"], "dead", "{listed}");
    assert!(record["diagnostic"]["exit_summary"].is_string(), "{listed}");

    let cleared = owner.tool("session_stop", json!({"session_id":session}));
    assert_eq!(
        cleared["result"]["structuredContent"]["status"], "cleaned",
        "{cleared}"
    );
    let recorded = fixture.lifecycle_record(&session);
    assert_eq!(recorded["state"], "session_dead", "{recorded}");
    let detail = recorded["detail"].as_str().expect("lifecycle detail");
    assert!(
        detail.contains("while kwin_wayland was still running")
            && detail.contains("Sandbox log ended:"),
        "{detail}"
    );
    drop(owner);

    // A later server answers for the session from that record.
    let mut later = Server::start(&fixture, "later", &binary);
    let unknown = later.tool(SESSION_TOOL, json!({"session_id":session}));
    let message = unknown["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("What ended it:")
            && message.contains("while kwin_wayland was still running")
            && message.contains("Sandbox log ended:"),
        "{message}"
    );
    assert_eq!(
        unknown["error"]["data"]["session_status"], "dead",
        "{unknown}"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn a_call_running_when_the_server_is_signalled_is_answered_with_the_cause() {
    let fixture = Fixture::new("end-signal");
    let binary = server_binary();
    let mut owner = Server::start(&fixture, "owner", &binary);
    let owner_pid = owner.pid();
    let (session, sandbox) = owner.start_session();
    wait_for_sigterm_handler(owner_pid);

    let never = fixture.0.join("never-exported");
    let waiting = owner.send_tool(
        "export_file",
        json!({"session_id":session, "session_path":"/tmp/never-written",
            "host_path":never, "timeout_seconds":120}),
    );
    std::thread::sleep(Duration::from_secs(1));
    let signalled = Instant::now();
    signal(
        i32::try_from(owner_pid).expect("pid fits"),
        nix::libc::SIGTERM,
    );

    // #867: the call is answered, with the reason, before the transport closes.
    let answer = owner
        .reply(waiting)
        .expect("the transport closed without an answer to the running call");
    let answered_after = signalled.elapsed();
    let data = &answer["error"]["data"];
    assert_eq!(data["reason"], "server_shutting_down", "{answer}");
    assert_eq!(data["session_id"], session.as_str(), "{answer}");
    assert_eq!(data["retry_safe"], false, "{answer}");
    let sender = format!("SIGTERM from PID {} (", std::process::id());
    assert!(
        data["shutdown_cause"]
            .as_str()
            .is_some_and(|cause| cause.starts_with(&sender)),
        "the cause does not name this test as the sender: {answer}"
    );
    assert!(
        answered_after < Duration::from_secs(3),
        "the answer took {answered_after:?}"
    );

    // The server then stops its session and exits.
    wait_until(
        "the signalled server exits",
        Duration::from_secs(60),
        || !matches!(owner.child.try_wait(), Ok(None)),
    );
    assert!(process_gone(sandbox), "the session outlived its server");
    assert!(
        owner.stderr().contains(&format!("shutdown: {sender}")),
        "{}",
        owner.stderr()
    );
    let recorded = fixture.lifecycle_record(&session);
    assert_eq!(recorded["state"], "shutdown_cleanup_finished", "{recorded}");
    assert!(
        recorded["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with(&sender)),
        "{recorded}"
    );

    // #893: a later server names the exited owner, what ended it, and the way on.
    let mut later = Server::start(&fixture, "later", &binary);
    let unknown = later.tool(SESSION_TOOL, json!({"session_id":session}));
    let error = &unknown["error"];
    let message = error["message"].as_str().expect("error message");
    assert_eq!(
        error["data"]["reason"], "different_server_process",
        "{unknown}"
    );
    assert_eq!(error["data"]["session_server_running"], false, "{unknown}");
    assert_eq!(error["data"]["session_status"], "stopped", "{unknown}");
    assert!(
        message.contains(&format!("process {owner_pid}, which is no longer running"))
            && message.contains(&format!("What ended it: {sender}"))
            && message.contains("Call session_start without session_id"),
        "{message}"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn a_session_ends_with_its_killed_owner_and_a_later_call_names_the_owner() {
    let fixture = Fixture::new("end-killed");
    let binary = server_binary();
    let mut owner = Server::start(&fixture, "owner", &binary);
    let owner_pid = owner.pid();
    let (session, sandbox) = owner.start_session();

    // A killed server records nothing; its session must not outlive it.
    owner.child.kill().expect("kill the owner");
    owner.child.wait().expect("reap the owner");
    wait_until(
        "the session ends with its owner",
        Duration::from_secs(20),
        || process_gone(sandbox),
    );

    let mut later = Server::start(&fixture, "later", &binary);
    let unknown = later.tool(SESSION_TOOL, json!({"session_id":session}));
    let error = &unknown["error"];
    let data = &error["data"];
    assert_eq!(data["reason"], "different_server_process", "{unknown}");
    assert_eq!(data["session_server_pid"], owner_pid, "{unknown}");
    assert_eq!(data["session_server_running"], false, "{unknown}");
    assert_eq!(data["session_available_here"], false, "{unknown}");
    assert_eq!(
        data["previous_server_cleanup"], "not_confirmed_from_current_process",
        "{unknown}"
    );
    let message = error["message"].as_str().expect("error message");
    assert!(
        message.contains(&format!("process {owner_pid}, which is no longer running"))
            && message.contains("its sessions ended with it")
            && message
                .contains("Call session_start without session_id and relaunch your applications"),
        "{message}"
    );
    // The recovery works: the later server starts a desktop of its own.
    let (fresh, _) = later.start_session();
    assert_ne!(fresh, session);
    let stopped = later.tool("session_stop", json!({"session_id":fresh}));
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn forwarded_calls_survive_the_owner_re_executing() {
    let fixture = Fixture::new("end-reexec");
    let binary = server_binary();
    // The owner runs from a copy, so the test can replace its binary, which
    // is how an installed update reaches a running server.
    let copy = fixture.0.join("bin/kwin-mcp");
    std::fs::create_dir_all(copy.parent().expect("bin directory")).expect("create bin");
    std::fs::copy(&binary, &copy).expect("copy the server binary");
    let mut owner = Server::start(&fixture, "owner", &copy);
    let (session, _) = owner.start_session();
    let mut other = Server::start(&fixture, "other", &binary);

    // A forwarded call that is still running when the owner decides to
    // re-execute: the owner finishes it first.
    let never = fixture.0.join("never-exported");
    let waiting = other.send_tool(
        "export_file",
        json!({"session_id":session, "session_path":"/tmp/never-written",
            "host_path":never, "timeout_seconds":12}),
    );
    std::thread::sleep(Duration::from_secs(1));
    let update = fixture.0.join("bin/kwin-mcp.new");
    std::fs::copy(&binary, &update).expect("write the updated binary");
    std::fs::rename(&update, &copy).expect("install the updated binary");

    let finished = other.reply(waiting).expect("answer to the forwarded call");
    assert_ne!(
        finished["error"]["data"]["reason"], "session_owner_connection_failed",
        "the owner cut the forwarded call off: {finished}"
    );
    // The next call arrives while the owner's new image is starting.
    let windows = other.tool("window_list", json!({"session_id":session}));
    assert!(windows["error"].is_null(), "not retried: {windows}");
    wait_until("the owner re-executed", Duration::from_secs(30), || {
        owner.stderr().contains("reload: reattached 1 sessions")
    });
    let windows = other.tool("window_list", json!({"session_id":session}));
    assert!(windows["error"].is_null(), "{windows}");
    let listed = owner.tool("session_list", json!({}));
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"][0]["status"], "running",
        "{listed}"
    );
    let stopped = owner.tool("session_stop", json!({"session_id":session}));
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
}

// ── session_stop answers within its limit (#899) ────────────────────────

/// The PIDs in the session's scope whose process name is `name`.
fn scope_processes(sandbox: i32, name: &str) -> Vec<i32> {
    let cgroup =
        std::fs::read_to_string(format!("/proc/{sandbox}/cgroup")).expect("sandbox cgroup");
    let path = cgroup
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .next()
        .expect("cgroup v2 path");
    let members = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/cgroup.procs"))
        .expect("scope members");
    members
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|comm| comm.trim() == name)
        })
        .collect()
}

fn stage_named<'a>(stopped: &'a Value, stage: &str) -> Option<&'a Value> {
    let stages = stopped["result"]["structuredContent"]["stages"].as_array()?;
    let index = stages.iter().position(|entry| entry["stage"] == stage)?;
    stages.get(index)
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn session_stop_does_not_wait_for_a_stopped_compositor() {
    let fixture = Fixture::new("stop-kwin");
    let mut server = Server::start(&fixture, "server", &server_binary());
    let (session, sandbox) = server.start_session();
    let compositors = scope_processes(sandbox, "kwin_wayland");
    assert_eq!(compositors.len(), 1, "{compositors:?}");
    signal(compositors[0], nix::libc::SIGSTOP);

    let began = Instant::now();
    let stopped = server.tool("session_stop", json!({"session_id":session}));
    let took = began.elapsed();
    let result = &stopped["result"]["structuredContent"];
    assert_eq!(result["status"], "stopped", "{stopped}");
    // KWin gets 3 s to close browser windows; it took 15 s before.
    assert!(
        took < Duration::from_secs(9),
        "session_stop took {took:?}: {stopped}"
    );
    let asked = stage_named(&stopped, "asking KWin to close browser windows")
        .unwrap_or_else(|| panic!("a slow stop lists its stages: {stopped}"));
    assert!(
        asked["note"]
            .as_str()
            .is_some_and(|note| note.contains("KWin did not answer within 3 s")),
        "{stopped}"
    );
    assert!(
        result["elapsed_ms"].as_u64().is_some_and(|ms| ms >= 3000),
        "{stopped}"
    );
    let text = stopped["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        text.contains("the slowest stage of the stop being asking KWin to close browser windows"),
        "{text}"
    );
    assert!(process_gone(sandbox), "the sandbox outlived session_stop");
    assert!(
        process_gone(compositors[0]),
        "the stopped compositor outlived session_stop"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn session_stop_ends_processes_that_ignore_sigterm() {
    let fixture = Fixture::new("stop-term");
    let mut server = Server::start(&fixture, "server", &server_binary());
    let (session, sandbox) = server.start_session();
    let launched = server.tool(
        "launch_app",
        json!({
            "session_id":session,
            "command":"bash -c 'trap \"\" TERM HUP INT; exec sleep 600'",
        }),
    );
    assert!(launched["error"].is_null(), "{launched}");
    wait_until(
        "the TERM-proof process runs",
        Duration::from_secs(10),
        || !scope_processes(sandbox, "sleep").is_empty(),
    );
    let stubborn = scope_processes(sandbox, "sleep");

    let began = Instant::now();
    let stopped = server.tool("session_stop", json!({"session_id":session}));
    let took = began.elapsed();
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
    assert!(
        took < Duration::from_secs(15),
        "session_stop took {took:?}: {stopped}"
    );
    // The answer is honest: nothing of the session is left running.
    for pid in stubborn {
        assert!(
            process_gone(pid),
            "process {pid} outlived a stop that said stopped"
        );
    }
    assert!(process_gone(sandbox));
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, input devices, and Google Chrome"]
fn session_stop_gives_up_on_a_browser_that_does_not_exit() {
    let fixture = Fixture::new("stop-chrome");
    let mut server = Server::start(&fixture, "server", &server_binary());
    let (session, sandbox) = server.start_session();
    let launched = server.tool(
        "launch_app",
        json!({
            "session_id":session,
            "command":"google-chrome-stable --user-data-dir=/tmp/profile about:blank",
            "window_timeout_seconds":40,
        }),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let browsers = scope_processes(sandbox, "chrome");
    assert!(!browsers.is_empty(), "no browser process in the session");
    for pid in &browsers {
        signal(*pid, nix::libc::SIGSTOP);
    }

    let began = Instant::now();
    let stopped = server.tool("session_stop", json!({"session_id":session}));
    let took = began.elapsed();
    let result = &stopped["result"]["structuredContent"];
    assert_eq!(result["status"], "stopped", "{stopped}");
    assert!(
        took < Duration::from_secs(20),
        "session_stop took {took:?}: {stopped}"
    );
    assert!(
        result["cookie_errors"][0]
            .as_str()
            .is_some_and(|error| error.contains("did not finish cookie flush within 6s")),
        "{stopped}"
    );
    assert!(
        stage_named(&stopped, "waiting for browsers to exit and flush cookies").is_some(),
        "{stopped}"
    );
    for pid in browsers {
        assert!(process_gone(pid), "browser {pid} outlived session_stop");
    }
}

/// Needs a debug build of the server (`KWIN_MCP_E2E_SERVER=target/debug/kwin-mcp`):
/// `KWIN_MCP_TEST_STOP_STORAGE_DELAY_MS` stands in for a disk too busy to
/// delete the session's files in time, and a release build ignores it.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, input devices, and a debug server build"]
fn session_stop_reports_a_cleanup_that_outlasts_its_limit() {
    let fixture = Fixture::new("stop-limit");
    let mut server = Server::start_with(
        &fixture,
        "server",
        &server_binary(),
        &[("KWIN_MCP_TEST_STOP_STORAGE_DELAY_MS", "40000")],
    );
    let (session, sandbox) = server.start_session();
    let workdir = PathBuf::from(format!(
        "/tmp/kwin-mcp-{}",
        session.strip_prefix('s').expect("session id")
    ));
    assert!(workdir.is_dir(), "{}", workdir.display());

    let began = Instant::now();
    let pending = server.tool("session_stop", json!({"session_id":session}));
    let took = began.elapsed();
    let result = &pending["result"]["structuredContent"];
    assert!(
        took >= Duration::from_secs(29) && took < Duration::from_secs(33),
        "session_stop answered after {took:?}: {pending}"
    );
    assert_eq!(result["status"], "cleanup_pending", "{pending}");
    assert_eq!(result["session_status"], "stopped", "{pending}");
    assert_eq!(result["limit_seconds"], 30, "{pending}");
    assert_eq!(
        result["unfinished"]["stage"], "deleting the session's overlay and runtime files",
        "{pending}"
    );
    let text = pending["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(
        text.contains("Every process of the session has ended")
            && text.contains("call session_stop again"),
        "{text}"
    );
    // That is true: the processes are gone, the files are not yet.
    assert!(process_gone(sandbox), "the sandbox was still running");
    assert!(workdir.is_dir(), "the workdir was reported as still there");
    // The handle is kept, and says the session is stopped.
    let listed = server.tool("session_list", json!({}));
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"][0]["status"], "stopped",
        "{listed}"
    );

    // The next stop waits for the same cleanup and reports its end.
    let finished = server.tool("session_stop", json!({"session_id":session}));
    let result = &finished["result"]["structuredContent"];
    assert_eq!(result["status"], "stopped", "{finished}");
    assert_eq!(
        result["workdir_removed"],
        workdir.display().to_string(),
        "{finished}"
    );
    assert!(!workdir.exists(), "the workdir was reported removed");
    let recorded = fixture.lifecycle_record(&session);
    assert_eq!(recorded["state"], "session_stopped", "{recorded}");
    assert_eq!(recorded["workdir_cleanup"], "removed", "{recorded}");
    let listed = server.tool("session_list", json!({}));
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"],
        json!([]),
        "{listed}"
    );
}
