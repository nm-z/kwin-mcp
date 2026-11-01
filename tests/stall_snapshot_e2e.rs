#![allow(clippy::expect_used)]

//! A start that runs out of its budget while its sandbox is alive must say what
//! the sandbox's processes were doing: the hard limit drops the start and kills
//! the sandbox, so the evidence has to be recorded before. The debug-only delay
//! hook holds the start right after the sandbox is spawned.

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
}

fn spawn(home: &std::path::Path) -> (Child, Wire) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--autoclean", "--no-viewer"])
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("KWIN_MCP_TEST_STARTUP_DELAY_STAGE", "after-bwrap")
        .env("KWIN_MCP_TEST_STARTUP_DELAY_MS", "30000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let wire = Wire {
        stdin: child.stdin.take().expect("server stdin"),
        stdout: BufReader::new(child.stdout.take().expect("server stdout")),
    };
    (child, wire)
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and a debug build (the delay hook)"]
fn a_stalled_start_names_the_processes_in_its_sandbox() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-stall-snapshot-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    let (mut child, mut wire) = spawn(&home);
    let reply = wire.request(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"stall-snapshot-e2e","version":"1"}
        }),
    );
    assert!(reply["result"].is_object(), "{reply}");
    wire.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let response = wire.request(
        2,
        "tools/call",
        json!({"name":"session_start",
            "arguments":{"browser_profiles":"fresh", "startup_timeout_seconds":5}}),
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(response["error"]["data"]["reason"], "hard_timeout", "{response}");
    let snapshot = response["error"]["data"]["sandbox_snapshot"]
        .as_str()
        .expect("the stall error carries a sandbox snapshot");
    assert!(snapshot.contains("sandbox processes:"), "{snapshot}");
    // The entrypoint's own processes are in the tree, with parent, state and wait channel.
    assert!(snapshot.contains("dbus-daemon["), "{snapshot}");
    assert!(snapshot.contains("<-"), "{snapshot}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("last sandbox snapshot")),
        "{response}"
    );
}
