#![allow(clippy::expect_used)]

//! Typing inserts at the caret, so text already in a field stays. A result must say
//! what the field held and what it holds now, warn when the old text was not
//! selected, and `replace=true` must overwrite it.

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

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn typing_into_a_filled_field_reports_before_and_after_and_replace_overwrites() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-type-over-{}", std::process::id()));
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
            "clientInfo":{"name":"type-over-e2e","version":"1"}
        }),
    );
    wire.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    let started = wire.tool(
        "session_start",
        json!({"browser_profiles":"fresh", "width":1280, "height":720}),
    );
    let session = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let workdir = started["result"]["structuredContent"]["host_shared_dir"]
        .as_str()
        .expect("workdir")
        .to_owned();
    std::fs::write(
        format!("{workdir}/field.html"),
        "<html><body style=\"font-size:22px\">\
         <input id=a value=\"1\" style=\"font-size:28px;width:500px;height:70px;margin:10px\"></body></html>",
    )
    .expect("write page");
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable file://{workdir}/field.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    // Click at the right end of the field: the caret lands after the existing "1".
    wire.tool("mouse_click", json!({"session_id":session, "x":450, "y":140}));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let inserted = wire.tool("keyboard_type", json!({"session_id":session, "text":"0.5"}));
    let after_insert = &inserted["result"]["structuredContent"];

    // Same field again with replace=true: the old value is selected and overwritten.
    let replaced = wire.tool(
        "keyboard_type",
        json!({"session_id":session, "text":"9", "replace":true}),
    );
    let after_replace = &replaced["result"]["structuredContent"];
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(after_insert["value_before"], "1", "{inserted}");
    assert_eq!(after_insert["value_after"], "10.5", "{inserted}");
    let warning = after_insert["delivery"]["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("replace=true"), "{inserted}");

    assert_eq!(after_replace["value_before"], "10.5", "{replaced}");
    assert_eq!(after_replace["value_after"], "9", "{replaced}");
    assert!(
        after_replace["delivery"].get("warning").is_none(),
        "replace=true still warned: {replaced}"
    );
}
