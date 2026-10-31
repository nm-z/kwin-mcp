#![allow(clippy::expect_used)]

//! A server that exits must say why on stderr, so an MCP host that respawns it
//! can be told apart from a crash. Shutdown happens before any session exists,
//! so the test needs neither KDE nor a display.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const EXIT_DEADLINE: Duration = Duration::from_secs(20);

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: std::process::ChildStderr,
}

fn start_server() -> Server {
    let home = std::env::temp_dir().join(format!("kwin-mcp-shutdown-cause-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--no-viewer"])
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kwin-mcp");
    let mut stdin = child.stdin.take().expect("server stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let stderr = child.stderr.take().expect("server stderr");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"shutdown-cause-e2e","version":"1"}
            }
        })
    )
    .expect("send initialize");
    stdin.flush().expect("flush initialize");
    let mut reply = String::new();
    stdout.read_line(&mut reply).expect("read initialize reply");
    assert!(reply.contains("\"result\""), "initialize failed: {reply}");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
    )
    .expect("send initialized");
    stdin.flush().expect("flush initialized");
    Server {
        child,
        stdin: Some(stdin),
        stdout,
        stderr,
    }
}

fn wait_for_exit_and_stderr(mut server: Server) -> String {
    let deadline = Instant::now() + EXIT_DEADLINE;
    while server.child.try_wait().expect("poll server").is_none() {
        assert!(Instant::now() < deadline, "server did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut text = String::new();
    server
        .stderr
        .read_to_string(&mut text)
        .expect("read server stderr");
    // Keep stdout open until here so a broken pipe cannot end the server early.
    drop(server.stdout);
    text
}

#[test]
fn closing_stdin_is_reported_as_the_client_closing_the_connection() {
    let mut server = start_server();
    drop(server.stdin.take());
    let stderr = wait_for_exit_and_stderr(server);
    assert!(
        stderr.contains("shutdown: the MCP client closed the connection"),
        "stderr did not name the cause:\n{stderr}"
    );
}

/// The server installs its signal handlers after the MCP handshake. Before
/// that, SIGTERM kills it with the default action and nothing is logged.
fn wait_for_sigterm_handler(pid: u32) {
    let deadline = Instant::now() + EXIT_DEADLINE;
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read status");
        let caught = status
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
            .expect("SigCgt mask");
        let sigterm_bit = 1u64 << (nix::libc::SIGTERM - 1);
        if caught & sigterm_bit != 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "server never installed a SIGTERM handler"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sigterm_is_reported_as_sigterm() {
    let server = start_server();
    wait_for_sigterm_handler(server.child.id());
    let pid = i32::try_from(server.child.id()).expect("pid fits");
    assert_eq!(unsafe { nix::libc::kill(pid, nix::libc::SIGTERM) }, 0);
    let stderr = wait_for_exit_and_stderr(server);
    assert!(
        stderr.contains("shutdown: SIGTERM"),
        "stderr did not name the cause:\n{stderr}"
    );
}
