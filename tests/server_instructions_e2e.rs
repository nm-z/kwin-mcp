#![allow(clippy::expect_used)]

//! The instructions a client receives at `initialize` are the guidance agents
//! follow, so what they say about Chrome's choosers is checked on the wire.
//! No session is created, so no KDE or display is needed.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn instructions_describe_both_chrome_choosers() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-instructions-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--no-viewer"])
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let mut stdin = child.stdin.take().expect("server stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"server-instructions-e2e","version":"1"}
            }
        })
    )
    .expect("send initialize");
    stdin.flush().expect("flush initialize");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read initialize reply");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);
    let reply: serde_json::Value = serde_json::from_str(&line).expect("initialize reply is JSON");
    let instructions = reply["result"]["instructions"]
        .as_str()
        .expect("instructions in the initialize reply");
    assert!(
        instructions.contains("Chrome file chooser: click the file input, ctrl+l"),
        "{instructions}"
    );
    assert!(
        instructions.contains("for folders click its top-right button, not alt+o"),
        "the folder chooser flow is missing: {instructions}"
    );
}
