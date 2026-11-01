#![allow(clippy::expect_used)]

//! find_ui_elements counts non-control text inside an already matched element instead
//! of listing it again, always lists controls, and `whole_word` skips names that only
//! contain the query inside a longer word.

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

fn names(found: &Value) -> Vec<String> {
    found["result"]["structuredContent"]["elements"]
        .as_array()
        .map(|elements| {
            elements
                .iter()
                .map(|element| element["name"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn child_matches_fold_into_the_parent_and_whole_word_skips_longer_words() {
    let home = std::env::temp_dir().join(format!("kwin-mcp-find-fold-{}", std::process::id()));
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
            "clientInfo":{"name":"find-fold-e2e","version":"1"}
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
        format!("{workdir}/fold.html"),
        "<html><body style=\"font-size:20px\">\
         <div role=group aria-label=\"Start panel\"><span>Start the engine</span> \
         <span>Start the pump</span> <button>Start button</button></div>\
         <div role=group aria-label=\"StartFraction one over two\">math</div>\
         </body></html>",
    )
    .expect("write page");
    let launched = wire.tool(
        "launch_app",
        json!({"session_id":session,
            "command":format!("google-chrome-stable file://{workdir}/fold.html"),
            "window_timeout_seconds":40}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(std::time::Duration::from_secs(4));

    let all = wire.tool(
        "find_ui_elements",
        json!({"session_id":session, "query":"Start", "timeout_seconds":20}),
    );
    let whole = wire.tool(
        "find_ui_elements",
        json!({"session_id":session, "query":"Start", "whole_word":true, "timeout_seconds":20}),
    );
    let _ = wire.tool("session_stop", json!({"session_id":session}));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    let all_names = names(&all);
    // The panel is listed, its text is counted in it, and its button is listed.
    assert!(all_names.iter().any(|name| name == "Start panel"), "{all}");
    assert!(all_names.iter().any(|name| name == "Start button"), "{all}");
    assert!(
        !all_names.iter().any(|name| name == "Start the engine" || name == "Start the pump"),
        "text inside the panel was listed again: {all}"
    );
    let panel = all["result"]["structuredContent"]["elements"]
        .as_array()
        .and_then(|elements| elements.iter().find(|element| element["name"] == "Start panel"))
        .expect("the panel");
    assert!(panel["children_matched"].as_u64().unwrap_or(0) >= 2, "{all}");
    // 'Start' is a whole word in the panel and the button, only a piece of StartFraction.
    assert!(names(&all).iter().any(|name| name.starts_with("StartFraction")), "{all}");
    assert!(!names(&whole).iter().any(|name| name.starts_with("StartFraction")), "{whole}");
    assert!(names(&whole).iter().any(|name| name == "Start panel"), "{whole}");
}
