#![allow(clippy::expect_used)]

//! mouse_click_element looks the element up when it clicks, so a layout that moved
//! since the last look does not make the click miss, and it clicks what the element
//! search shows:
//!   a_click_by_name_lands_after_the_layout_moved
//!   a_profile_picker_card_is_clicked_by_name   (#902 #898 #880 #875 #872) a frame whose
//!       accessible name is not its window's caption
//!   a_named_control_is_clicked_or_the_refusal_says_why   (#881 #882) a menu item named
//!       with its shortcut, and a role filter that leaves out the named element
//!   two_windows_with_one_caption_are_refused_with_the_reason   (#907)
//!   a_match_in_another_window_is_clicked_after_activating_it   (#907)
//!
//! Run an ignored case with `systemd-run --user --wait --pipe --collect env
//! KWIN_MCP_E2E_SERVER=<binary> <test exe> --ignored --test-threads=1 NAME`; without
//! KWIN_MCP_E2E_SERVER the binary built with the test is used. KWIN_MCP_E2E_EVIDENCE=<dir>
//! keeps a screenshot before and after each click; tests/evidence/907 holds one run's.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// One stdio server with a private HOME and one session, both removed on drop.
struct Desk {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    home: PathBuf,
    session: String,
    workdir: String,
}

impl Desk {
    /// `prepare` fills the private HOME before the server starts.
    fn start(label: &str, prepare: impl FnOnce(&std::path::Path)) -> Self {
        // Not under /tmp: a session has its own /tmp, which would hide what
        // `prepare` wrote.
        let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let home = root.join(format!("kwin-mcp-click-{label}-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
        prepare(&home);
        let program = std::env::var_os("KWIN_MCP_E2E_SERVER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")));
        let mut child = Command::new(program)
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let mut desk = Self {
            stdin: child.stdin.take().expect("server stdin"),
            stdout: BufReader::new(child.stdout.take().expect("server stdout")),
            child,
            next_id: 0,
            home,
            session: String::new(),
            workdir: String::new(),
        };
        desk.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"click-element-e2e","version":"1"}
            }),
        );
        desk.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        let started = desk.request(
            "tools/call",
            json!({"name":"session_start", "arguments":{"width":1280, "height":720}}),
        );
        let field = |key: &str| {
            started["result"]["structuredContent"][key]
                .as_str()
                .unwrap_or_else(|| panic!("session_start has no {key}: {started}"))
                .to_owned()
        };
        desk.session = field("session_id");
        desk.workdir = field("host_shared_dir");
        desk
    }

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

    /// The whole reply to a tool call in this desk's session.
    fn tool(&mut self, name: &str, mut arguments: Value) -> Value {
        arguments["session_id"] = json!(self.session);
        self.request("tools/call", json!({"name":name, "arguments":arguments}))
    }

    fn launch(&mut self, command: &str) {
        let launched = self.tool("launch_app", json!({"command":command, "window_timeout_seconds":40}));
        assert!(launched["error"].is_null(), "{launched}");
    }

    fn windows(&mut self) -> Vec<Value> {
        let listed = self.tool("window_list", json!({}));
        listed["result"]["structuredContent"]["windows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The title of the active window.
    fn title(&mut self) -> String {
        self.windows()
            .iter()
            .find_map(|window| (window["active"] == json!(true)).then(|| window["title"].as_str()))
            .flatten()
            .unwrap_or_default()
            .to_owned()
    }

    /// Waits until a window's title contains `part`, and returns that window.
    fn window_titled(&mut self, part: &str, seconds: u64) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let found = self.windows().into_iter().find_map(|window| {
                window["title"]
                    .as_str()
                    .is_some_and(|title| title.contains(part))
                    .then_some(window)
            });
            if found.is_some() || Instant::now() >= deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Waits until the element search returns a match for `query`.
    fn shown(&mut self, query: &str, seconds: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let found = self.tool("find_ui_elements", json!({"query":query, "timeout_seconds":20}));
            let matches = &found["result"]["structuredContent"]["matches"];
            if matches.as_u64().is_some_and(|matches| matches > 0) || Instant::now() >= deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn page(&self, name: &str, html: &str) -> String {
        let path = format!("{}/{name}", self.workdir);
        std::fs::write(&path, html).expect("write page");
        format!("file://{path}")
    }

    /// Saves a screenshot of the active window where KWIN_MCP_E2E_EVIDENCE names a
    /// directory; does nothing otherwise.
    fn evidence(&mut self, name: &str) {
        let Some(dir) = std::env::var_os("KWIN_MCP_E2E_EVIDENCE").map(PathBuf::from) else {
            return;
        };
        std::thread::sleep(Duration::from_millis(1500));
        let target = dir.join(format!("{name}.png"));
        let _ = std::fs::remove_file(&target);
        let shot = self.tool("screenshot", json!({"save_as":target}));
        assert!(shot["error"].is_null(), "{shot}");
    }
}

impl Drop for Desk {
    fn drop(&mut self) {
        let _ = self.tool("session_stop", json!({}));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn clicked(reply: &Value) -> &Value {
    &reply["result"]["structuredContent"]["clicked"]
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_click_by_name_lands_after_the_layout_moved() {
    let mut desk = Desk::start("moved", |_| {});
    // Clicking the first button opens a 400 px gap, moving the second one down.
    let url = desk.page(
        "moving.html",
        "<html><head><title>start</title></head><body style=\"font-size:22px\">\
         <button onclick=\"document.getElementById('gap').style.height='400px';document.title='first-clicked'\">First</button>\
         <div id=gap style=\"height:0\"></div>\
         <button onclick=\"document.title='second-clicked'\">Second</button></body></html>",
    );
    desk.launch(&format!("google-chrome-stable {url}"));
    desk.shown("Second", 30);

    let first = desk.tool(
        "mouse_click_element",
        json!({"query":"First", "role":"button", "exact":true}),
    );
    std::thread::sleep(Duration::from_millis(700));
    let after_first = desk.title();
    // The second button is now 400 px lower than any earlier look would say.
    let second = desk.tool(
        "mouse_click_element",
        json!({"query":"Second", "role":"button", "exact":true}),
    );
    std::thread::sleep(Duration::from_millis(700));
    let after_second = desk.title();
    let missing = desk.tool(
        "mouse_click_element",
        json!({"query":"No such button", "exact":true, "timeout_seconds":5}),
    );

    assert!(first["error"].is_null(), "{first}");
    assert!(after_first.contains("first-clicked"), "title after first click: {after_first}");
    assert!(second["error"].is_null(), "{second}");
    assert_eq!(clicked(&second)["name"], "Second");
    assert!(after_second.contains("second-clicked"), "title after second click: {after_second}");
    assert_eq!(missing["error"]["data"]["reason"], "no_clickable_element", "{missing}");
    assert_eq!(missing["error"]["data"]["cause"], "no_match", "{missing}");
}

/// Chrome shows its profile picker when its data directory lists two profiles. The
/// picker's accessible frame is named "Who's using Chrome?" and its window is
/// captioned "Google Chrome", so the search listed the cards with bounds it could
/// not turn into window pixels and the click refused them.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_profile_picker_card_is_clicked_by_name() {
    let mut desk = Desk::start("picker", |home| {
        let chrome = home.join(".config/google-chrome");
        std::fs::create_dir_all(&chrome).expect("create Chrome data directory");
        // Chrome opens the picker only for two profiles used in the last weeks.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_secs_f64();
        let state = json!({"profile":{
            "info_cache":{
                "Default":{"name":"Alpha", "is_using_default_name":false, "active_time":now},
                "Profile 1":{"name":"Beta", "is_using_default_name":false, "active_time":now},
            },
            "last_used":"Default", "last_active_profiles":["Default"],
            "profiles_order":["Default", "Profile 1"], "profiles_created":3,
            "picker_shown":true, "show_picker_on_startup":true,
        }});
        std::fs::write(chrome.join("Local State"), state.to_string()).expect("write Local State");
    });
    desk.launch("google-chrome-stable");
    let cards = desk.shown("Open Beta profile", 40);
    let card = cards["result"]["structuredContent"]["elements"]
        .as_array()
        .and_then(|elements| {
            let card = elements.iter().position(|element| element["role"] == json!("button"))?;
            elements.get(card)
        })
        .cloned()
        .unwrap_or_default();
    let picker = desk.title();
    desk.evidence("picker-before");
    // The query also matches other elements of the picker; the card is the control.
    let reply = desk.tool("mouse_click_element", json!({"query":"Beta", "timeout_seconds":20}));
    // Clicking a card closes the picker and opens that profile's browser window.
    let browser = desk.window_titled(" - Google Chrome", 40);
    desk.evidence("picker-after");

    assert_eq!(picker, "Google Chrome", "the picker's window caption");
    assert_eq!(card["mouse_coordinates_usable"], true, "the card as the search lists it: {cards}");
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(clicked(&reply)["role"], "button", "{reply}");
    assert!(
        clicked(&reply)["name"].as_str().is_some_and(|name| name.contains("Open Beta profile")),
        "{reply}"
    );
    assert!(browser.is_some(), "no browser window opened after {reply}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_named_control_is_clicked_or_the_refusal_says_why() {
    let mut desk = Desk::start("named", |_| {});
    // The menu item's accessible name is its label followed by its shortcut, and the
    // tab's name is its text, which the accessibility tree does not list separately.
    let url = desk.page(
        "named.html",
        "<html><head><title>start</title></head><body style=\"font-size:22px\">\
         <div role=menu><div role=menuitem tabindex=0 style=\"width:400px;padding:16px\" \
         onclick=\"document.title='settings-opened'\"><span>All settings</span> \
         <span style=\"float:right\">Shift+Ctrl+,</span></div></div>\
         <div role=tablist><button role=tab onclick=\"document.title='wallet-opened'\">Wallet</button>\
         <button role=tab>About</button></div>\
         <p>Wallet and All settings are described in this paragraph.</p></body></html>",
    );
    desk.launch(&format!("google-chrome-stable {url}"));
    desk.shown("Wallet", 30);
    desk.evidence("named-before");

    let item = desk.tool(
        "mouse_click_element",
        json!({"query":"All settings", "role":"menu item", "exact":true}),
    );
    std::thread::sleep(Duration::from_millis(700));
    let after_item = desk.title();
    desk.evidence("named-after-menu-item");
    let as_text = desk.tool(
        "mouse_click_element",
        json!({"query":"Wallet", "role":"text", "exact":true}),
    );
    std::thread::sleep(Duration::from_millis(700));
    let after_refusal = desk.title();
    let as_tab = desk.tool(
        "mouse_click_element",
        json!({"query":"Wallet", "role":"page tab", "exact":true}),
    );
    std::thread::sleep(Duration::from_millis(700));
    let after_tab = desk.title();
    desk.evidence("named-after-tab");

    assert!(item["error"].is_null(), "{item}");
    assert_eq!(clicked(&item)["role"], "menu item", "{item}");
    assert!(after_item.contains("settings-opened"), "title after the menu item: {after_item}");
    let refusal = as_text["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(as_text["error"]["data"]["cause"], "role_excluded", "{as_text}");
    assert!(
        refusal.contains("page tab 'Wallet'") && refusal.contains("without role"),
        "the refusal names the element and the next step: {refusal}"
    );
    assert!(after_refusal.contains("settings-opened"), "a refused click changed the page: {after_refusal}");
    assert!(as_tab["error"].is_null(), "{as_tab}");
    assert!(after_tab.contains("wallet-opened"), "title after the tab: {after_tab}");
}

/// Two browser windows on one page have one caption, and Chrome does not mark the
/// active frame, so nothing tells which frame is the active window. The click is
/// refused with that reason instead of landing on whichever frame was listed first.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn two_windows_with_one_caption_are_refused_with_the_reason() {
    let mut desk = Desk::start("twins", |_| {});
    let url = desk.page(
        "twin.html",
        "<html><head><title>twin</title></head><body style=\"font-size:22px\">\
         <button onclick=\"document.title='marked'\">Mark this window</button></body></html>",
    );
    desk.launch(&format!("google-chrome-stable {url}"));
    desk.shown("Mark this window", 30);
    desk.launch(&format!("google-chrome-stable --new-window {url}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while desk.windows().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(3));
    let before = desk.windows();

    let reply = desk.tool(
        "mouse_click_element",
        json!({"query":"Mark this window", "role":"button", "exact":true, "timeout_seconds":20}),
    );
    std::thread::sleep(Duration::from_millis(700));
    let after = desk.windows();
    desk.evidence("one-caption-after");

    assert_eq!(before.len(), 2, "two windows: {before:?}");
    assert_eq!(before[0]["title"], before[1]["title"], "one caption: {before:?}");
    let refusal = reply["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(reply["error"]["data"]["cause"], "frame_not_paired_with_window", "{reply}");
    assert!(
        refusal.contains("several windows") && refusal.contains("caption") && refusal.contains("mouse_click"),
        "the refusal says why and what to do: {refusal}"
    );
    assert!(
        after.iter().all(|window| window["title"].as_str().is_some_and(|title| title.contains("twin"))),
        "a refused click changed a page: {after:?}"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_match_in_another_window_is_clicked_after_activating_it() {
    let mut desk = Desk::start("other", |_| {});
    let back = desk.page(
        "back.html",
        "<html><head><title>back-start</title></head><body style=\"font-size:22px\">\
         <button onclick=\"document.title='back-clicked'\">Only in the back window</button></body></html>",
    );
    let front = desk.page(
        "front.html",
        "<html><head><title>front-start</title></head><body style=\"font-size:22px\">\
         <button>Front button</button></body></html>",
    );
    desk.launch(&format!("google-chrome-stable {back}"));
    desk.shown("Only in the back window", 30);
    desk.launch(&format!("google-chrome-stable --new-window {front}"));
    desk.shown("Front button", 30);
    let before = desk.title();
    let back_window = desk.window_titled("back-start", 5).unwrap_or_default();

    let reply = desk.tool(
        "mouse_click_element",
        json!({"query":"Only in the back window", "role":"button", "exact":true}),
    );
    let clicked_window = desk.window_titled("back-clicked", 5);
    desk.evidence("other-window-after");

    assert!(before.contains("front-start"), "the second window is the active one: {before}");
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        reply["result"]["structuredContent"]["activated_window"]["id"], back_window["id"],
        "{reply}"
    );
    assert!(clicked_window.is_some(), "the back window's button was not clicked: {reply}");
}
