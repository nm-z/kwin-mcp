#![allow(clippy::expect_used)]

//! A screenshot that gets no frame must say what the session's compositor is
//! doing. Stopping kwin_wayland gives the reported `frame_unavailable` timeout,
//! and the error then shows the compositor stopped (state T), which tells a
//! frozen compositor from a busy one.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Wire {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Wire {
    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("send message");
        self.stdin.flush().expect("flush message");
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
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

    fn tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(id, "tools/call", json!({"name":name, "arguments":arguments}))
    }
}

/// `comm` and parent of every process, from /proc/<pid>/stat.
fn processes() -> Vec<(u32, String, u32)> {
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let text = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let (open, close) = (text.find('(')?, text.rfind(')')?);
            let comm = text.get(open + 1..close)?.to_owned();
            let parent = text.get(close + 1..)?.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, comm, parent))
        })
        .collect()
}

/// The kwin_wayland process below `root`.
fn compositor_below(root: u32) -> Option<u32> {
    let all = processes();
    let mut below = vec![root];
    for _ in 0..12 {
        for (pid, _, parent) in &all {
            if below.contains(parent) && !below.contains(pid) {
                below.push(*pid);
            }
        }
    }
    all.iter()
        .find(|(pid, comm, _)| comm == "kwin_wayland" && below.contains(pid))
        .map(|(pid, _, _)| *pid)
}

fn signal(pid: u32, signal: i32) {
    let pid = i32::try_from(pid).expect("pid fits");
    assert_eq!(unsafe { nix::libc::kill(pid, signal) }, 0, "signal {signal}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn a_screenshot_stall_shows_the_compositor_stopped() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-shot-stall-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--autoclean", "--no-viewer"])
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let mut wire = Wire {
        stdin: child.stdin.take().expect("server stdin"),
        stdout: BufReader::new(child.stdout.take().expect("server stdout")),
    };
    let reply = wire.request(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"screenshot-stall-snapshot-e2e","version":"1"}
        }),
    );
    assert!(reply["result"].is_object(), "{reply}");
    wire.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let started = wire.tool(
        2,
        "session_start",
        json!({"browser_profiles":"fresh", "width":1280, "height":720}),
    );
    let session = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let launched = wire.tool(
        3,
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let listed = wire.tool(4, "session_list", json!({}));
    let sandbox = u32::try_from(
        listed["result"]["structuredContent"]["sessions"][0]["sandbox_pid"]
            .as_u64()
            .expect("sandbox pid"),
    )
    .expect("pid fits");
    let compositor = compositor_below(sandbox).expect("kwin_wayland below the sandbox");

    signal(compositor, nix::libc::SIGSTOP);
    let stalled = wire.tool(5, "screenshot", json!({"session_id":session}));
    signal(compositor, nix::libc::SIGCONT);
    let _ = wire.tool(6, "session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(stalled["error"]["data"]["reason"], "frame_unavailable", "{stalled}");
    let snapshot = stalled["error"]["data"]["sandbox_snapshot"]
        .as_str()
        .expect("the stall carries a sandbox snapshot");
    assert!(
        snapshot.contains(&format!("kwin_wayland[{compositor}<-")) && snapshot.contains(" T "),
        "the stopped compositor is not shown as stopped: {snapshot}"
    );
}
