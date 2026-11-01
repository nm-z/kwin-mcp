#![allow(clippy::expect_used)]

//! A successful `session_start` is read by the model on every start, so its
//! result stays small however many files the inherited browser profiles hold.
//! Start-up timing used to be listed per file in that result: 192 KB of a
//! 196 KB response on a host with many Chrome profiles.

use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

struct Wire {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Wire {
    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("send message");
        self.stdin.flush().expect("flush message");
    }

    /// The reply to request `id`, as the raw line the client receives.
    fn request(&mut self, id: u64, method: &str, params: Value) -> String {
        self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read reply");
            assert!(read > 0, "server closed stdout before reply {id}");
            if serde_json::from_str::<Value>(&line).is_ok_and(|message| message["id"] == id) {
                return line;
            }
        }
    }
}

fn cookie_jar(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().expect("jar has a parent")).expect("create profile");
    let connection = Connection::open(path).expect("open jar");
    connection
        .execute_batch(
            "CREATE TABLE cookies (host_key TEXT NOT NULL, name TEXT NOT NULL, \
             path TEXT NOT NULL, value TEXT, last_update_utc INTEGER NOT NULL, \
             expires_utc INTEGER NOT NULL DEFAULT 0, has_expires INTEGER NOT NULL DEFAULT 0, \
             UNIQUE (host_key, name, path));",
        )
        .expect("create schema");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn an_inherit_start_over_many_browser_profiles_returns_a_small_result() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-response-size-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    for profile in 0..300 {
        cookie_jar(&home.join(format!(".config/google-chrome/Profile {profile}/Network/Cookies")));
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
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
    wire.request(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"response-size-e2e","version":"1"}
        }),
    );
    wire.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let raw = wire.request(
        2,
        "tools/call",
        json!({"name":"session_start",
            "arguments":{"browser_profiles":"inherit", "startup_timeout_seconds":120}}),
    );
    let reply: Value = serde_json::from_str(&raw).expect("a JSON reply");
    let session = reply["result"]["structuredContent"]["session_id"]
        .as_str()
        .map(str::to_owned);
    if let Some(session) = &session {
        wire.request(3, "tools/call", json!({"name":"session_stop", "arguments":{"session_id":session}}));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(session.is_some(), "the start failed: {raw}");
    assert!(
        raw.len() < 16 * 1024,
        "session_start returned {} bytes for 300 browser profiles",
        raw.len()
    );
    assert!(
        reply["result"]["structuredContent"].get("stage_times").is_none(),
        "the success result lists stage times"
    );
}
