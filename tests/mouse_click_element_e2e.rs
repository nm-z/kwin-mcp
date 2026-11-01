#![allow(clippy::expect_used)]

//! mouse_click_element finds the element when it clicks, so a layout that moved since
//! the last look does not make the click miss.

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

fn title(wire: &mut Wire, session: &str) -> String {
    let listed = wire.tool("window_list", json!({"session_id":session}));
    listed["result"]["structuredContent"]["windows"][0]["title"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_click_by_name_lands_after_the_layout_moved() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-click-element-{}", std::process::id()));
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
            "clientInfo":{"name":"click-element-e2e","version":"1"}
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
    // Clicking the first button opens a 400 px gap, moving the second one down.
    std::fs::write(
        format!("{workdir}/moving.html"),
        "<html><head><title>start</title></head><body style=\"font-size:22px\">\
         <button onclick=\"document.getElementById('gap').style.height='400px';document.title='first-clicked'\">First</button>\
         <div id=gap style=\"height:0\"></div>\
         <button onclick=\"document.title='second-clicked'\">Second</button></body></html>",
    )
    .expect("write page");
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable file://{workdir}/moving.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    let first = wire.tool(
        "mouse_click_element",
        json!({"session_id":session, "query":"First", "role":"button", "exact":true}),
    );
    std::thread::sleep(std::time::Duration::from_millis(700));
    let after_first = title(&mut wire, &session);
    // The second button is now 400 px lower than any earlier look would say.
    let second = wire.tool(
        "mouse_click_element",
        json!({"session_id":session, "query":"Second", "role":"button", "exact":true}),
    );
    std::thread::sleep(std::time::Duration::from_millis(700));
    let after_second = title(&mut wire, &session);
    let missing = wire.tool(
        "mouse_click_element",
        json!({"session_id":session, "query":"No such button", "exact":true, "timeout_seconds":5}),
    );
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(first["error"].is_null(), "{first}");
    assert!(after_first.contains("first-clicked"), "title after first click: {after_first}");
    assert!(second["error"].is_null(), "{second}");
    assert_eq!(second["result"]["structuredContent"]["clicked"]["name"], "Second");
    assert!(after_second.contains("second-clicked"), "title after second click: {after_second}");
    assert_eq!(
        missing["error"]["data"]["reason"], "no_clickable_element",
        "{missing}"
    );
}
