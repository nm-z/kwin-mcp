#![allow(clippy::expect_used)]

//! A big result body (the clipboard text, the accessibility tree) is sent once, in the
//! half of the result the client shows the model: Claude Code shows only
//! structuredContent, Codex only the text, any other client gets both.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

struct Wire {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Wire {
    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("send message");
        self.stdin.flush().expect("flush message");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
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

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name, "arguments":arguments}))
    }
}

/// The clipboard read and the tree as a client of this name receives them.
fn read_as(client: &str) -> (Value, Value) {
    let home = std::env::temp_dir().join(format!("kwin-mcp-client-body-{}-{client}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
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
        next_id: 0,
    };
    wire.request(
        "initialize",
        json!({
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":client,"version":"1"}
        }),
    );
    wire.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let started = wire.tool("session_start", json!({"browser_profiles":"fresh"}));
    let session = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    wire.tool(
        "clipboard_write",
        json!({"session_id":session, "target":"session", "text":"clipboard-body-xyz"}),
    );
    let clipboard = wire.tool("clipboard_read", json!({"session_id":session}));
    let tree = wire.tool("accessibility_tree", json!({"session_id":session, "max_depth":2}));
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);
    (clipboard, tree)
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn each_client_gets_the_body_in_the_half_it_shows() {
    let text = |result: &Value| result["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
    let structured = |result: &Value, key: &str| result["result"]["structuredContent"][key].as_str().map(str::to_owned);

    let (clipboard, tree) = read_as("claude-code");
    assert_eq!(structured(&clipboard, "text").as_deref(), Some("clipboard-body-xyz"), "{clipboard}");
    assert!(!text(&clipboard).contains("clipboard-body-xyz"), "{clipboard}");
    assert!(structured(&tree, "tree").is_some_and(|tree| tree.contains("Konsole")), "{tree}");
    assert!(text(&tree).len() < 200, "the tree is also in the text: {tree}");

    let (clipboard, tree) = read_as("codex-mcp-client");
    assert_eq!(text(&clipboard), "clipboard-body-xyz", "{clipboard}");
    assert!(structured(&clipboard, "text").is_none(), "{clipboard}");
    assert!(text(&tree).contains("Konsole"), "{tree}");
    assert!(structured(&tree, "tree").is_none(), "the tree is also in structuredContent");

    let (clipboard, tree) = read_as("some-other-client");
    assert_eq!(text(&clipboard), "clipboard-body-xyz", "{clipboard}");
    assert_eq!(structured(&clipboard, "text").as_deref(), Some("clipboard-body-xyz"), "{clipboard}");
    assert!(text(&tree).contains("Konsole") && structured(&tree, "tree").is_some(), "{tree}");
}
