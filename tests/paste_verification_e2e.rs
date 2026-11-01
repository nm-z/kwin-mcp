#![allow(clippy::expect_used)]

//! `keyboard_paste` reads the focused field before and after the paste, so a
//! paste that landed is `verified` and one that cannot be confirmed is not. The
//! tool used to report every paste as unverified, which made "acknowledged but
//! the editor stayed empty" indistinguishable from success.

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
fn a_paste_that_landed_is_verified_and_one_into_a_non_field_is_not() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-paste-verify-{}", std::process::id()));
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
    let reply = wire.request(
        "initialize",
        json!({
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"paste-verification-e2e","version":"1"}
        }),
    );
    assert!(reply["result"].is_object(), "{reply}");
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
        format!("{workdir}/editors.html"),
        "<html><body style=\"font-size:22px\">\
         <div contenteditable=true style=\"border:2px solid #36c;width:600px;height:80px;margin:10px\">A: </div>\
         <p style=\"margin:10px\">not a field, just text</p></body></html>",
    )
    .expect("write page");
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable file://{workdir}/editors.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    // Into the editable div: the text appears, so the paste is verified.
    wire.tool("mouse_click", json!({"session_id":session, "x":200, "y":135}));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let landed = wire.tool(
        "keyboard_paste",
        json!({"session_id":session, "text":"PASTED-TEXT-123"}),
    );
    // With focus on page text that is not a field nothing can be confirmed.
    wire.tool("mouse_click", json!({"session_id":session, "x":100, "y":230}));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let missed = wire.tool(
        "keyboard_paste",
        json!({"session_id":session, "text":"SECOND-PASTE-456"}),
    );
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(
        landed["result"]["structuredContent"]["delivery"]["status"], "verified",
        "{landed}"
    );
    assert_eq!(
        landed["result"]["structuredContent"]["application_text"], "verified",
        "{landed}"
    );
    assert_ne!(
        missed["result"]["structuredContent"]["delivery"]["status"], "verified",
        "a paste with no field focused was reported verified: {missed}"
    );
}
