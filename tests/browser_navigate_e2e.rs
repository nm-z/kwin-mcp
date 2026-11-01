#![allow(clippy::expect_used)]

//! browser_navigate opens the address that was typed, removing the address bar's
//! inline completion, and reports the page it reached.

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
fn navigate_opens_the_typed_address_and_reports_where_it_landed() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-navigate-{}", std::process::id()));
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
            "clientInfo":{"name":"navigate-e2e","version":"1"}
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
    for (name, title) in [("a", "page-alpha"), ("b", "page-beta")] {
        std::fs::write(
            format!("{workdir}/{name}.html"),
            format!("<html><head><title>{title}</title></head><body>{title}</body></html>"),
        )
        .expect("write page");
    }
    // A profile of its own lets branded Chrome expose CDP, which reports the final url.
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable --user-data-dir={workdir}/profile file://{workdir}/a.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    let target = format!("file://{workdir}/b.html");
    let navigated = wire.tool("browser_navigate", json!({"session_id":session, "url":target}));
    let invalid = wire.tool("browser_navigate", json!({"session_id":session, "url":"two words"}));
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(navigated["error"].is_null(), "{navigated}");
    let content = &navigated["result"]["structuredContent"];
    assert!(
        content["title"].as_str().unwrap_or_default().contains("page-beta"),
        "{navigated}"
    );
    assert!(content.get("matched").is_none(), "landed elsewhere: {navigated}");
    assert!(
        content["url"].as_str().unwrap_or_default().ends_with("/b.html"),
        "the result does not report the page's url: {navigated}"
    );
    assert_eq!(invalid["error"]["data"]["reason"], "invalid_url", "{invalid}");
}
