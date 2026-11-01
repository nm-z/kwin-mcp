#![allow(clippy::expect_used)]

//! mouse_scroll and zoom shortcuts say whether the view changed (so the end of the
//! content and a key the page ignored are visible), and with CDP the scroll position.

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
fn scroll_and_zoom_report_whether_the_view_changed() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-view-feedback-{}", std::process::id()));
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
            "clientInfo":{"name":"view-feedback-e2e","version":"1"}
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
    let mut page = String::from("<html><head><title>tall</title></head><body style=\"font-size:24px\">");
    for line in 0..120 {
        page.push_str(&format!("<p>line {line} of a tall page with some words on it</p>"));
    }
    page.push_str("</body></html>");
    std::fs::write(format!("{workdir}/tall.html"), page).expect("write page");
    // A profile of its own lets branded Chrome expose CDP, which reports the offset.
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable --user-data-dir={workdir}/profile file://{workdir}/tall.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    let down = wire.tool(
        "mouse_scroll",
        json!({"session_id":session, "x":400, "y":300, "delta":600}),
    );
    // Far more than the page has: the end is reached and a further scroll moves nothing.
    wire.tool(
        "mouse_scroll",
        json!({"session_id":session, "x":400, "y":300, "delta":20000, "speed":4}),
    );
    let at_end = wire.tool(
        "mouse_scroll",
        json!({"session_id":session, "x":400, "y":300, "delta":600}),
    );
    let zoomed = wire.tool("keyboard_key", json!({"session_id":session, "key":"ctrl+minus"}));
    let plain = wire.tool("keyboard_key", json!({"session_id":session, "key":"shift"}));
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    let content = |result: &Value| result["result"]["structuredContent"].clone();
    assert_eq!(content(&down)["view_changed"], true, "{down}");
    assert!(content(&down)["scroll_y"].as_f64().unwrap_or(0.0) > 0.0, "{down}");
    assert_eq!(content(&down)["at_end"], false, "{down}");
    assert_eq!(content(&at_end)["view_changed"], false, "{at_end}");
    assert_eq!(content(&at_end)["at_end"], true, "{at_end}");
    assert!(
        content(&at_end)["scroll_y"].as_f64() == content(&at_end)["scroll_max_y"].as_f64(),
        "{at_end}"
    );
    assert_eq!(content(&zoomed)["view_changed"], true, "{zoomed}");
    assert_eq!(content(&zoomed)["application_effect"], "view_changed", "{zoomed}");
    // An ordinary key keeps the unverified effect and gets no view check.
    assert_eq!(content(&plain)["application_effect"], "unverified", "{plain}");
    assert!(content(&plain).get("view_changed").is_none(), "{plain}");
}
