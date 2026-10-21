#![allow(clippy::expect_used)]
//! Production-stdio checks for state a caller cannot see without the tool
//! reporting it:
//!   disk_floor    (#606) disk_min_free_mib lowers or raises the HOME overlay free-space floor
//!   stop_reports  (#607) session_start says whether session_stop deletes host_shared_dir
//!   show_desktop  (#608, #609) super+d hides windows unminimized; window_list, screenshot
//!                 and keyboard_type report it, and keyboard_type names the window it typed into
//!   hidden_import (#610) path_not_visible points at import_file, and an imported copy runs
//!   home_overlay  (#611) launch_app reports overlay arguments and a host file the session
//!                 sees a different copy of, and agrees with what the command ran
//!   launch_attribution (#615) a window that is only reactivated while the command still
//!                 runs is not the launch's window
//!   held_modifier (#622) keyboard_type refuses while keyboard_press holds a modifier
//!   unstarted_launch (#631) a launch refused for an invisible path says the command did not start
//!   chooser_typing (#624) typing the location in Chrome's file chooser is not refused
//!   ui_name_whitespace (#633) an accessible name with a non-breaking space matches a normal-space query
//!
//! Run with `KWIN_MCP_E2E=1 cargo test --test session_reports_e2e -- --ignored`;
//! KWIN_MCP_E2E_SERVER selects another server binary.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CALL_TIMEOUT: Duration = Duration::from_secs(120);

fn require_e2e() {
    assert_eq!(
        std::env::var("KWIN_MCP_E2E").as_deref(),
        Ok("1"),
        "set KWIN_MCP_E2E=1 to run"
    );
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos()
}

/// One production-stdio server with a private HOME, removed on drop.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    home: PathBuf,
    next_id: u64,
    session: Option<String>,
}

impl Server {
    fn start(flags: &[&str]) -> Self {
        let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let home = root.join(format!("ksr-{}-{:x}", std::process::id(), nonce()));
        for directory in [".config", ".cache", ".local/share", ".local/state", ".kde"] {
            std::fs::create_dir_all(home.join(directory)).expect("create private HOME");
        }
        let runtime = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
        let program = std::env::var("KWIN_MCP_E2E_SERVER")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned());
        let mut child = Command::new(program)
            .args(flags)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("KDEHOME", home.join(".kde"))
            .env("KWIN_MCP_REPO", home.join("no-source"))
            .env("CARGO", "/usr/bin/false")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                std::env::var("DBUS_SESSION_BUS_ADDRESS")
                    .unwrap_or_else(|_| format!("unix:path={runtime}/bus")),
            )
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn kwin-mcp");
        let stdout = child.stdout.take().expect("server stdout");
        let (sender, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut server = Self {
            child,
            stdin,
            responses,
            home,
            next_id: 0,
            session: None,
        };
        let initialized = server.rpc(
            "initialize",
            json!({"protocolVersion":"2025-06-18", "capabilities":{},
                   "clientInfo":{"name":"session-reports-e2e","version":"1"}}),
        );
        assert!(
            initialized["result"].is_object(),
            "initialize: {initialized}"
        );
        server.write(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        server
    }

    fn write(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("open server stdin");
        writeln!(stdin, "{message}").expect("write request");
        stdin.flush().expect("flush request");
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let response = self
                .responses
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("{method} #{id}: {error}"));
            if response["id"] == json!(id) {
                return response;
            }
        }
    }

    /// The raw response, for calls whose failure is the expected outcome.
    fn call(&mut self, name: &str, mut arguments: Value) -> Value {
        if let Some(session) = &self.session {
            arguments["session_id"] = json!(session);
        }
        self.rpc("tools/call", json!({"name":name, "arguments":arguments}))
    }

    fn data(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.call(name, arguments);
        assert!(
            response["error"].is_null() && response["result"]["isError"] != json!(true),
            "{name} failed: {response}"
        );
        response["result"]["structuredContent"].clone()
    }

    fn start_session(&mut self, extra: Value) -> Value {
        let mut arguments = json!({"width":1280, "height":800, "browser_profiles":"fresh",
                                   "startup_timeout_seconds":60});
        if let (Some(arguments), Some(extra)) = (arguments.as_object_mut(), extra.as_object()) {
            arguments.extend(extra.clone());
        }
        let started = self.data("session_start", arguments);
        self.session = Some(
            started["session_id"]
                .as_str()
                .expect("session_id")
                .to_owned(),
        );
        started
    }

    fn stop_session(&mut self) -> Value {
        let stopped = self.data("session_stop", json!({}));
        self.session = None;
        stopped
    }

    fn windows(&mut self) -> Value {
        self.data("window_list", json!({}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.session.is_some() {
            let _ = self.call("session_stop", json!({}));
        }
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Error text from a JSON-RPC error or an isError tool result.
fn failure_text(response: &Value) -> String {
    let mut text = response["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    for item in response["result"]["content"]
        .as_array()
        .into_iter()
        .flatten()
    {
        text.push(' ');
        text.push_str(item["text"].as_str().unwrap_or_default());
    }
    text
}

fn wait_until<T>(seconds: u64, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {seconds}s waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn file_holds(path: &Path, expected: &str) -> Option<()> {
    (std::fs::read_to_string(path).ok()? == expected).then_some(())
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn disk_floor() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    let refused = server.call(
        "session_start",
        json!({"width":1280, "height":800, "browser_profiles":"fresh",
               "startup_timeout_seconds":60, "disk_min_free_mib":100_000_000_u64}),
    );
    assert!(
        !refused["error"].is_null() || refused["result"]["isError"] == json!(true),
        "an impossible floor started a session: {refused}"
    );
    let text = failure_text(&refused);
    assert!(
        text.contains("disk_min_free_mib") && text.contains("free"),
        "the refusal does not name disk_min_free_mib: {text}"
    );
    server.start_session(json!({"disk_min_free_mib":64}));
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn stop_reports() {
    require_e2e();
    for (flags, removed, ttl) in [
        (&["--autoclean"][..], true, Value::Null),
        (&[][..], false, Value::Null),
        (&["--ttl", "7"][..], true, json!(7)),
    ] {
        let mut server = Server::start(flags);
        let started = server.start_session(json!({}));
        assert_eq!(
            started["workdir_removed_on_stop"],
            json!(removed),
            "{flags:?}: {started}"
        );
        assert_eq!(started["ttl_minutes"], ttl, "{flags:?}: {started}");
        let shared = PathBuf::from(
            started["host_shared_dir"]
                .as_str()
                .expect("host_shared_dir"),
        );
        let output = shared.join("shots/kept.png");
        std::fs::create_dir_all(shared.join("shots")).expect("create output directory");
        std::fs::write(&output, "png bytes").expect("write host output");
        let stopped = server.stop_session();
        let survived = output.exists();
        assert_eq!(
            survived, !removed,
            "{flags:?}: reported removal {removed}: {stopped}"
        );
        if survived {
            std::fs::remove_dir_all(&shared).expect("remove retained workdir");
        }
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn show_desktop() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    let started = server.start_session(json!({}));
    let shared = PathBuf::from(
        started["host_shared_dir"]
            .as_str()
            .expect("host_shared_dir"),
    );
    let launched = server.data(
        "launch_app",
        json!({"command":"konsole", "window_timeout_seconds":30}),
    );
    let konsole = launched["window"]
        .as_str()
        .expect("Konsole window")
        .to_owned();
    let before = server.windows();
    assert_eq!(before["showing_desktop"], json!(false), "{before}");
    assert!(
        before["windows"]
            .as_array()
            .expect("windows")
            .iter()
            .all(|window| window["hidden"] == json!(false)),
        "{before}"
    );
    server.data("keyboard_key", json!({"key":"super+d"}));
    let hidden = wait_until(5, "show desktop", || {
        let list = server.windows();
        (list["showing_desktop"] == json!(true)).then_some(list)
    });
    assert!(
        hidden["windows"]
            .as_array()
            .expect("windows")
            .iter()
            .all(|window| window["hidden"] == json!(true) && window["minimized"] == json!(false)),
        "windows not all hidden and unminimized: {hidden}"
    );
    assert!(
        hidden["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("show desktop")),
        "{hidden}"
    );
    let shot = server.data("screenshot", json!({}));
    assert_eq!(shot["showing_desktop"], json!(true), "{shot}");
    assert!(
        shot["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("show desktop"))
    );
    let marker = shared.join("typed-marker");
    let typed = server.data(
        "keyboard_type",
        json!({"text": format!("printf typed > {}\n", marker.display())}),
    );
    assert_eq!(typed["window"]["id"], json!(konsole), "{typed}");
    assert_eq!(
        typed["window"]["resourceClass"],
        json!("org.kde.konsole"),
        "{typed}"
    );
    assert_eq!(typed["window"]["hidden"], json!(true), "{typed}");
    assert!(
        typed["warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("show desktop"))
    );
    wait_until(10, "the command typed into the hidden Konsole", || {
        file_holds(&marker, "typed")
    });
    server.data("window_activate", json!({"window_id":konsole}));
    let shown = wait_until(5, "show desktop to end", || {
        let list = server.windows();
        (list["showing_desktop"] == json!(false)).then_some(list)
    });
    assert!(
        shown["windows"]
            .as_array()
            .expect("windows")
            .iter()
            .all(|window| window["hidden"] == json!(false)),
        "windows still hidden after window_activate: {shown}"
    );
    let after = server.data("screenshot", json!({}));
    assert!(
        after.get("warning").is_none(),
        "screenshot still warns: {after}"
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn hidden_import() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    let started = server.start_session(json!({}));
    let shared = PathBuf::from(
        started["host_shared_dir"]
            .as_str()
            .expect("host_shared_dir"),
    );
    let host_dir = PathBuf::from(format!("/tmp/kwin-610-{:x}", nonce()));
    std::fs::create_dir(&host_dir).expect("create host /tmp directory");
    let script = host_dir.join("hello.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf ran > \"$1\"\n").expect("write host script");
    let refused = server.call(
        "launch_app",
        json!({"command": format!("sh {} {}/hidden-marker", script.display(), shared.display()),
               "expect_window": false}),
    );
    let error = &refused["error"];
    assert_eq!(
        error["data"]["reason"],
        json!("path_not_visible"),
        "{refused}"
    );
    assert_eq!(
        error["data"]["import_tool"],
        json!("import_file"),
        "{refused}"
    );
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("import_file")),
        "{refused}"
    );
    let inside = "/tmp/kwin-610-hello.sh";
    server.data(
        "import_file",
        json!({"host_path": script, "session_path": inside}),
    );
    let marker = shared.join("import-marker");
    server.data(
        "launch_app",
        json!({"command": format!("sh {inside} {}", marker.display()), "expect_window": false}),
    );
    wait_until(10, "the imported script marker", || {
        file_holds(&marker, "ran")
    });
    std::fs::remove_dir_all(&host_dir).expect("remove host /tmp directory");
}

/// The result text blocks of a tool result, joined.
fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn reported_copies(result: &Value) -> Vec<String> {
    result["structuredContent"]["home_overlay"]["different_copies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|copy| copy["path"].as_str().map(str::to_owned))
        .collect()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn home_overlay() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    let work = server.home.join("work");
    std::fs::create_dir_all(&work).expect("create HOME work directory");
    let script = work.join("run611.sh");
    std::fs::write(&script, "printf old-wording > \"$1\"\n").expect("write host script");
    let started = server.start_session(json!({}));
    let shared = PathBuf::from(
        started["host_shared_dir"]
            .as_str()
            .expect("host_shared_dir"),
    );
    let run = |server: &mut Server, marker: &str| -> (Value, String) {
        let marker = shared.join(marker);
        let response = server.call(
            "launch_app",
            json!({"command": format!("sh {} {}", script.display(), marker.display()),
                   "expect_window": false}),
        );
        assert!(response["error"].is_null(), "{response}");
        let ran = wait_until(10, "the script's marker", || {
            std::fs::read_to_string(&marker)
                .ok()
                .filter(|text| !text.is_empty())
        });
        (response["result"].clone(), ran)
    };
    let script_text = script.display().to_string();

    let (first, ran) = run(&mut server, "first");
    assert_eq!(ran, "old-wording");
    assert_eq!(
        first["structuredContent"]["home_overlay"]["overlay_paths"],
        json!([script_text]),
        "{first}"
    );
    assert!(reported_copies(&first).is_empty(), "{first}");

    // An editor's save: a new file renamed over the old one.
    let saved = work.join(".run611.sh.swp");
    std::fs::write(&saved, "printf new-wording-611 > \"$1\"\n").expect("write edited script");
    std::fs::rename(&saved, &script).expect("replace host script");
    let (second, ran) = run(&mut server, "second");
    let reported = reported_copies(&second) == vec![script_text.clone()];
    assert_eq!(
        ran == "old-wording",
        reported,
        "the report must match the copy that ran ({ran:?}): {second}"
    );
    if reported {
        assert!(result_text(&second).contains("import_file"), "{second}");
    }
    eprintln!("home_overlay: after the host edit the session ran {ran:?}");

    server.data(
        "import_file",
        json!({"host_path": script, "overwrite": true}),
    );
    let (third, ran) = run(&mut server, "third");
    assert_eq!(ran, "new-wording-611");
    assert!(reported_copies(&third).is_empty(), "{third}");

    let output = work.join("out-611");
    let made = server.data(
        "launch_app",
        json!({"command": format!("mkdir -p {}", output.display()), "expect_window": false}),
    );
    assert_eq!(
        made["home_overlay"]["overlay_paths"],
        json!([output.display().to_string()]),
        "{made}"
    );
    std::thread::sleep(Duration::from_secs(1));
    assert!(!output.exists(), "a session write reached the host HOME");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, and input devices"]
fn launch_attribution() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    server.start_session(json!({"window_mode":"native"}));
    let window = |launched: &Value| launched["window"].as_str().expect("window id").to_owned();
    let first = window(&server.data(
        "launch_app",
        json!({"command":"konsole -e sleep 600", "window_timeout_seconds":30}),
    ));
    // This window closes by itself while the third launch is still waiting for
    // its own window; KWin then reactivates the first window.
    let closing = window(&server.data(
        "launch_app",
        json!({"command":"konsole -e sleep 4", "window_timeout_seconds":30}),
    ));
    let third = server.data(
        "launch_app",
        json!({"command":"sleep 9; exec konsole -e sleep 600", "window_timeout_seconds":40}),
    );
    let attributed = window(&third);
    assert_eq!(
        third["window_outcome"],
        json!("new"),
        "a command that is still running was given an older window: {third}"
    );
    assert!(
        attributed != first && attributed != closing,
        "the launch named an existing window: {third}"
    );
}

#[test]
#[ignore = "needs a real KDE session, bubblewrap and a Konsole"]
fn held_modifier() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    server.start_session(json!({}));
    server.data(
        "launch_app",
        json!({"command":"konsole", "window_timeout_seconds":30}),
    );
    // keyboard_press leaves Ctrl down; typing "h" now would be Ctrl+H.
    let pressed = server.data("keyboard_press", json!({"key":"CTRL+L"}));
    assert_eq!(pressed["held"], json!(true), "{pressed}");
    let refused = server.call("keyboard_type", json!({"text":"h"}));
    let text = failure_text(&refused);
    assert!(
        text.contains("still held") && text.contains("keyboard_release"),
        "typing with Ctrl held was not refused: {refused}"
    );
    server.data("keyboard_release", json!({"key":"CTRL+L"}));
    let typed = server.call("keyboard_type", json!({"text":"h"}));
    assert!(
        !failure_text(&typed).contains("still held"),
        "typing was refused after release: {typed}"
    );
}

#[test]
#[ignore = "needs a real KDE session, bubblewrap and Chrome"]
fn chooser_typing() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    server.start_session(json!({}));
    server.data(
        "launch_app",
        json!({"command":"google-chrome-stable --user-data-dir=$HOME/chrome-chooser about:blank",
               "window_timeout_seconds":40}),
    );
    // Chrome's file chooser is a transient GTK window that exposes nothing to
    // AT-SPI, so typing its location is unverified, not refused.
    server.data("keyboard_key", json!({"key":"ctrl+o"}));
    wait_until(20, "Chrome's file chooser", || {
        server.windows()["windows"]
            .as_array()?
            .iter()
            .any(|window| window["transient"] == json!(true) && window["active"] == json!(true))
            .then_some(())
    });
    server.data("keyboard_key", json!({"key":"ctrl+l"}));
    let typed = server.call("keyboard_type", json!({"text":"/tmp"}));
    assert!(
        typed["error"].is_null() && typed["result"]["isError"] != json!(true),
        "typing the chooser's location was refused: {typed}"
    );
}

#[test]
#[ignore = "needs a real KDE session, bubblewrap and Chrome"]
fn ui_name_whitespace() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    server.start_session(json!({"width":1920, "height":1080}));
    let fixture = server.home.join("issue633.html");
    let accessible_name = "Sign\u{00a0}in";
    std::fs::write(
        &fixture,
        format!("<!doctype html><button aria-label=\"{accessible_name}\"></button>"),
    )
    .expect("write accessible-name fixture");
    server.data(
        "launch_app",
        json!({"command":"google-chrome-stable --user-data-dir=$HOME/chrome-issue633 file://$HOME/issue633.html",
               "window_timeout_seconds":40}),
    );
    let tree = wait_until(30, "the accessible Sign in button", || {
        let response = server.call(
            "accessibility_tree",
            json!({"show_elements":true, "max_depth":8, "timeout_seconds":10}),
        );
        let text = response["result"]["content"]
            .as_array()?
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        text.contains(&format!("button\t{accessible_name}"))
            .then_some(text)
    });
    assert!(
        tree.contains(&format!("button\t{accessible_name}")),
        "accessibility_tree did not expose the fixture button: {tree}"
    );
    let found = server.data(
        "find_ui_elements",
        json!({"query":"Sign in", "role":"button", "visible_only":true}),
    );
    let target = found["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|element| {
            element["backend"] == json!("cdp")
                && element["role"] == json!("button")
                && element["name"] == json!(accessible_name)
        });
    assert!(
        target.is_some(),
        "find_ui_elements missed the button returned by accessibility_tree: {found}"
    );
    server.stop_session();
}

#[test]
#[ignore = "needs a real KDE session and bubblewrap"]
fn unstarted_launch() {
    require_e2e();
    let mut server = Server::start(&["--autoclean"]);
    let started = server.start_session(json!({}));
    let shared = PathBuf::from(
        started["host_shared_dir"]
            .as_str()
            .expect("host_shared_dir"),
    );
    let marker = shared.join("ran");
    // The command names /run/user, which the session does not see, so the
    // session shell stops before running it.
    let refused = server.call(
        "launch_app",
        json!({"command":format!("sh -c 'printf x > {}' /run/user", marker.display()),
               "expect_window":false}),
    );
    let data = &refused["error"]["data"];
    assert_eq!(data["reason"], json!("path_not_visible"), "{refused}");
    assert_eq!(data["command_started"], json!(false), "{refused}");
    assert_eq!(data["retry_safe"], json!(true), "{refused}");
    assert!(!marker.exists(), "the refused command ran");
    // The recovery the message names: the same work from a script file.
    let script = shared.join("probe.sh");
    std::fs::write(&script, format!("printf ok > {}\n", marker.display())).expect("write script");
    server.data(
        "launch_app",
        json!({"command":format!("sh {}", script.display()), "expect_window":false}),
    );
    wait_until(10, "the script's output", || file_holds(&marker, "ok"));
}
