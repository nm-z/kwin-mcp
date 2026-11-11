#![allow(clippy::expect_used)]

//! `keyboard_type` and `keyboard_paste` reach fields that are focused and
//! editable even when accessibility describes the window badly.
//!
//! Chrome sometimes names a window's accessibility frame after its in-page
//! search bar once that bar was used and DevTools opened ("<bar title> <page
//! title>" instead of the window caption), and keeps that name until the page
//! title changes. `keyboard_type` matched frames by caption only, so from then
//! on it refused every field of the window: a number input, the DevTools
//! console prompt, the search bar itself (#896, #883, #871). Chrome does not
//! do it on every run, so a unit test of `input_bridge` covers that binding
//! and this case covers the fields: each takes typing, also after DevTools was
//! open, and under memory pressure, when Chrome publishes focus late. A
//! read-only input must still be refused by name, and a field too long to read
//! back is typed into and reported unverified.
//!
//! `keyboard_paste` sent its shortcut even while no window was active, as
//! right after `launch_app`, so the text reached nothing; and it did not say
//! when Konsole answered with its control-character dialog (#878).
//!
//! Run with `KWIN_MCP_E2E_SERVER=<binary>` to test another build, and
//! `KWIN_MCP_E2E_EVIDENCE=<directory>` to keep the screenshots.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

struct Desktop {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    home: PathBuf,
    session: String,
    workdir: String,
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let session = self.session.clone();
        let _ = self.tool("session_stop", json!({"session_id":session}));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

impl Desktop {
    fn start(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("kwin-mcp-{name}-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
        let server = std::env::var_os("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_kwin-mcp").into());
        let mut child = Command::new(server)
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let mut desktop = Self {
            stdin: child.stdin.take().expect("server stdin"),
            stdout: BufReader::new(child.stdout.take().expect("server stdout")),
            child,
            next_id: 0,
            home,
            session: String::new(),
            workdir: String::new(),
        };
        let reply = desktop.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":name,"version":"1"}
            }),
        );
        assert!(reply["result"].is_object(), "{reply}");
        writeln!(
            desktop.stdin,
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
        )
        .expect("send initialized");
        let started = desktop.request(
            "tools/call",
            json!({"name":"session_start", "arguments":{"width":1280, "height":800}}),
        );
        let content = &started["result"]["structuredContent"];
        desktop.session = content["session_id"].as_str().expect("session id").to_owned();
        desktop.workdir = content["host_shared_dir"].as_str().expect("workdir").to_owned();
        desktop
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

    fn tool(&mut self, name: &str, mut arguments: Value) -> Value {
        arguments["session_id"] = json!(self.session);
        self.request("tools/call", json!({"name":name, "arguments":arguments}))
    }

    fn key(&mut self, key: &str) {
        let reply = self.tool("keyboard_key", json!({"key":key}));
        assert!(reply["error"].is_null(), "{key}: {reply}");
    }

    fn click(&mut self, x: i64, y: i64) {
        let reply = self.tool("mouse_click", json!({"x":x, "y":y}));
        assert!(reply["error"].is_null(), "click {x},{y}: {reply}");
        std::thread::sleep(Duration::from_millis(600));
    }

    fn type_text(&mut self, text: &str) -> Value {
        self.tool("keyboard_type", json!({"text":text}))
    }

    /// Saves the screen under `KWIN_MCP_E2E_EVIDENCE` when that is set.
    fn evidence(&mut self, name: &str) {
        let Some(directory) = std::env::var_os("KWIN_MCP_E2E_EVIDENCE") else {
            return;
        };
        let path = PathBuf::from(directory).join(format!("{name}.png"));
        let _ = std::fs::remove_file(&path);
        let shot = self.tool("screenshot", json!({"save_as":path}));
        assert!(shot["error"].is_null(), "{name}: {shot}");
    }
}

/// The delivery status of a typing or paste reply; empty when it was refused.
fn status(reply: &Value) -> &str {
    reply["result"]["structuredContent"]["delivery"]["status"]
        .as_str()
        .unwrap_or_default()
}

fn refusal(reply: &Value) -> &str {
    reply["error"]["message"].as_str().unwrap_or_default()
}

/// Page fields at fixed places: a 1280x800 Chrome window shows the page from
/// y = 95, so a field at page (20, top) with size 400x60 has its centre at
/// window (220, top + 125).
const PAGE: &str = "<!doctype html><html><head><title>typing targets</title><style>\
    input,textarea{position:absolute;left:20px;width:400px;height:60px;font-size:28px;box-sizing:border-box}\
    </style></head><body>\
    <input type=\"number\" aria-label=\"answer\" style=\"top:20px\">\
    <input aria-label=\"locked\" value=\"graded\" readonly style=\"top:100px\">\
    <textarea aria-label=\"long\" style=\"top:180px\"></textarea>\
    <p style=\"position:absolute;top:260px;left:20px;font-size:22px\">Enter the quotient</p>\
    <script>const long=document.querySelector('textarea');long.value='lorem '.repeat(60000);\
    long.addEventListener('input',event=>{document.title='long field got '+event.data})</script>\
    </body></html>";
const NUMBER: (i64, i64) = (220, 145);
const LOCKED: (i64, i64) = (220, 225);
const LONG: (i64, i64) = (220, 305);

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn chrome_fields_that_are_focused_and_editable_take_typing() {
    let mut desktop = Desktop::start("typing-targets-e2e");
    let workdir = desktop.workdir.clone();
    std::fs::write(format!("{workdir}/targets.html"), PAGE).expect("write page");
    let launched = desktop.tool(
        "launch_app",
        json!({"command":format!(
            "google-chrome-stable --disable-extensions --user-data-dir={workdir}/profile file://{workdir}/targets.html"
        ), "window_timeout_seconds":60}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    std::thread::sleep(Duration::from_secs(4));

    // A number input is a spin button to accessibility.
    desktop.click(NUMBER.0, NUMBER.1);
    let number = desktop.type_text("-10");
    desktop.evidence("1-number-input");

    // The in-page search bar, opened with ctrl+f, is browser UI in its own frame.
    desktop.key("ctrl+f");
    std::thread::sleep(Duration::from_secs(1));
    let search = desktop.type_text("quotient");
    desktop.evidence("2-search-bar");
    desktop.key("Escape");
    std::thread::sleep(Duration::from_millis(500));

    // Opening DevTools after that can leave the window's frame named after the
    // search bar. The console prompt has focus.
    desktop.key("ctrl+shift+j");
    std::thread::sleep(Duration::from_secs(5));
    let console = desktop.type_text("2+3");
    desktop.evidence("3-devtools-console");
    desktop.key("F12");
    std::thread::sleep(Duration::from_secs(2));

    // A renamed frame keeps its name with DevTools closed.
    desktop.click(NUMBER.0, NUMBER.1);
    let number_again = desktop.type_text("5");
    desktop.evidence("4-number-input-after-devtools");
    desktop.key("ctrl+f");
    std::thread::sleep(Duration::from_secs(1));
    let search_again = desktop.type_text("Enter");
    desktop.evidence("5-search-bar-after-devtools");
    desktop.key("Escape");
    std::thread::sleep(Duration::from_millis(500));

    // A read-only input has focus but takes no text: refused, by name.
    desktop.click(LOCKED.0, LOCKED.1);
    let locked = desktop.type_text("x");
    // 360,000 characters are more than a snapshot reads: typed, not verified.
    desktop.click(LONG.0, LONG.1);
    let long = desktop.type_text("z");
    // The page puts what the field received into its title.
    let mut long_received = false;
    for _ in 0..50 {
        let windows = desktop.tool("window_list", json!({})).to_string();
        long_received = windows.contains("long field got z");
        if long_received {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    desktop.evidence("6-unreadable-field");
    drop(desktop);
    for (step, reply) in [
        ("number input", &number),
        ("search bar", &search),
        ("DevTools console", &console),
        ("number input after DevTools", &number_again),
        ("search bar after DevTools", &search_again),
        ("read-only input", &locked),
        ("unreadable field", &long),
    ] {
        eprintln!("{step}: {} {}", status(reply), refusal(reply));
    }

    assert_eq!(status(&number), "verified", "number input: {number}");
    assert_eq!(status(&search), "verified", "search bar: {search}");
    assert!(
        matches!(status(&console), "verified" | "unverified"),
        "the DevTools console prompt was refused: {console}"
    );
    assert_eq!(
        status(&number_again), "verified",
        "number input after DevTools: {number_again}"
    );
    assert_eq!(
        number_again["result"]["structuredContent"]["value_after"], "-105",
        "{number_again}"
    );
    assert_eq!(
        status(&search_again), "verified",
        "search bar after DevTools: {search_again}"
    );
    assert!(
        refusal(&locked).contains("\"locked\"") && refusal(&locked).contains("read-only or disabled"),
        "the read-only input was not refused by name: {locked}"
    );
    assert_eq!(status(&long), "unverified", "unreadable field: {long}");
    assert!(long_received, "the unreadable field did not receive the key: {long}");
    let warning = long["result"]["structuredContent"]["delivery"]["warning"]
        .as_str()
        .unwrap_or_default();
    assert!(
        warning.contains("\"long\"") && warning.contains("could not read"),
        "the unverified result does not name the field: {long}"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn konsole_paste_waits_for_the_window_and_names_its_confirmation_dialog() {
    let mut desktop = Desktop::start("konsole-paste-e2e");
    let proof = format!("{}/paste-proof.txt", desktop.workdir);
    // With no window the shortcut reaches nothing, so nothing may be sent.
    let nowhere = desktop.tool("keyboard_paste", json!({"text":"echo nowhere"}));
    let launched = desktop.tool(
        "launch_app",
        json!({"command":"konsole", "window_timeout_seconds":60}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    // No pause: the window is mapped but may not be active yet.
    let pasted = desktop.tool(
        "keyboard_paste",
        json!({"text":format!("echo arrived > {proof}; printf 'status=%s\\n' \"$?\"")}),
    );
    std::thread::sleep(Duration::from_secs(1));
    desktop.evidence("7-konsole-pasted-command");
    desktop.key("Return");
    let mut arrived = String::new();
    for _ in 0..50 {
        arrived = std::fs::read_to_string(&proof).unwrap_or_default();
        if !arrived.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    desktop.evidence("8-konsole-command-ran");

    // Konsole asks before it pastes a control character.
    let guarded = desktop.tool("keyboard_paste", json!({"text":"echo ctl\u{1}char"}));
    desktop.evidence("9-konsole-paste-dialog");
    desktop.key("Escape");
    drop(desktop);

    assert_eq!(nowhere["error"]["data"]["reason"], "no_active_window", "{nowhere}");
    assert_eq!(nowhere["error"]["data"]["shortcut_submitted"], false, "{nowhere}");
    assert_eq!(nowhere["error"]["data"]["retry_safe"], true, "{nowhere}");
    assert!(pasted["error"].is_null(), "{pasted}");
    assert_eq!(
        pasted["result"]["structuredContent"]["delivery"]["method"], "terminal",
        "{pasted}"
    );
    assert_eq!(arrived.trim(), "arrived", "the pasted command did not run: {pasted}");
    let content = &guarded["result"]["structuredContent"];
    assert_eq!(content["delivery"]["method"], "paste_dialog", "{guarded}");
    assert!(
        content["dialog"]["title"]
            .as_str()
            .unwrap_or_default()
            .contains("Confirm Paste"),
        "{guarded}"
    );
}
