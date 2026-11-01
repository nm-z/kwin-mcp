#![allow(clippy::expect_used)]

//! `keyboard_type` must find the focused field of a page with far more nodes
//! than the accessibility walk may visit. Typing follows a click, so the field
//! under the pointer is found by hit-testing before the page is walked.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(home: &PathBuf) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let reply = server.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"focused-field-discovery-e2e","version":"1"}
            }),
        );
        assert!(reply["result"].is_object(), "{reply}");
        writeln!(
            server.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("send initialized");
        server
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .expect("send request");
        self.stdin.flush().expect("flush request");
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
fn typing_after_a_click_finds_the_field_on_a_page_with_thousands_of_nodes() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-field-discovery-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    let mut server = Server::start(&home);

    let started = server.tool(
        "session_start",
        json!({"browser_profiles":"fresh", "width":1920, "height":1080}),
    );
    let session = started["result"]["structuredContent"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let workdir = started["result"]["structuredContent"]["host_shared_dir"]
        .as_str()
        .expect("workdir")
        .to_owned();
    // 3000 paragraphs are about 6000 accessibility nodes, over the 5000 the walk
    // may visit, before the field the click focuses.
    let mut page = String::from("<html><body>");
    for index in 1..=3000 {
        page.push_str(&format!("<p>paragraph {index}</p>"));
    }
    page.push_str(
        "<input style=\"position:fixed;top:20px;left:20px;font-size:30px;width:400px\" placeholder=Search></body></html>",
    );
    std::fs::write(format!("{workdir}/heavy.html"), page).expect("write page");

    let launched = server.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable file://{workdir}/heavy.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(6));
    let clicked = server.tool("mouse_click", json!({"session_id":session, "x":200, "y":130}));
    assert!(clicked["error"].is_null(), "{clicked}");
    std::thread::sleep(std::time::Duration::from_secs(1));

    let typed = server.tool("keyboard_type", json!({"session_id":session, "text":"V340"}));
    let _ = server.tool("session_stop", json!({"session_id":session}));
    assert!(
        typed["error"].is_null(),
        "typing into a field on a heavy page was refused: {typed}"
    );
    assert_eq!(
        typed["result"]["structuredContent"]["delivery"]["status"], "verified",
        "{typed}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
