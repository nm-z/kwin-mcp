#![allow(clippy::expect_used)]

//! find_ui_elements carries what a text input holds, and a refused keyboard_type
//! names what has the focus, so a missed click, a label and a read-only input are
//! told apart without a screenshot.

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
fn inputs_show_their_value_and_a_refused_type_names_the_focus() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-input-value-{}", std::process::id()));
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
            "clientInfo":{"name":"input-value-e2e","version":"1"}
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
        format!("{workdir}/inputs.html"),
        "<html><body style=\"font-size:22px\">\
         <input aria-label=\"answer\" value=\"19\" style=\"font-size:28px;width:400px;height:60px;margin:10px;display:block\">\
         <input aria-label=\"locked\" value=\"graded\" readonly style=\"font-size:28px;width:400px;height:60px;margin:10px;display:block\">\
         </body></html>",
    )
    .expect("write page");
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable file://{workdir}/inputs.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    let answer = wire.tool(
        "find_ui_elements",
        json!({"session_id":session, "query":"answer", "role":"entry", "timeout_seconds":20}),
    );
    let locked = wire.tool(
        "find_ui_elements",
        json!({"session_id":session, "query":"locked", "role":"entry", "timeout_seconds":20}),
    );
    // Click the read-only input (the second box, about 80 px below the first) and type.
    let located = &locked["result"]["structuredContent"]["elements"][0]["bounds"];
    let (x, y) = (
        located["x"].as_f64().unwrap_or(0.0) + located["w"].as_f64().unwrap_or(0.0) / 2.0,
        located["y"].as_f64().unwrap_or(0.0) + located["h"].as_f64().unwrap_or(0.0) / 2.0,
    );
    #[expect(clippy::as_conversions)]
    wire.tool("mouse_click", json!({"session_id":session, "x":x as i64, "y":y as i64}));
    std::thread::sleep(std::time::Duration::from_millis(500));
    let refused = wire.tool("keyboard_type", json!({"session_id":session, "text":"x"}));
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(
        answer["result"]["structuredContent"]["elements"][0]["value"], "19",
        "{answer}"
    );
    assert_eq!(
        locked["result"]["structuredContent"]["elements"][0]["value"], "graded",
        "{locked}"
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("locked"), "the refusal does not name the field: {refused}");
    assert!(message.contains("read-only or disabled"), "{refused}");
}
