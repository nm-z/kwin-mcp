#![allow(clippy::expect_used)]

//! Google Chrome and Microsoft Edge get the session's CDP port on the data
//! directory a launch without `--user-data-dir` opens (#948):
//!   chrome_without_a_data_directory_gets_the_port_on_its_default_profile
//!   a_profile_directory_opens_that_profile_with_the_port
//!   the_profile_picker_window_has_the_port
//!   a_preset_config_home_and_a_launcher_flags_file_are_followed   the host shape the
//!       issue was filed from: CHROME_CONFIG_HOME already set, and a launcher whose
//!       flags file names the data directory
//!   cdp_false_starts_chrome_without_a_port
//!   a_browser_that_resolves_the_linked_default_gets_an_unlinked_one
//!   edge_on_its_default_profile_gets_the_port   needs `microsoft-edge-stable` on PATH,
//!       or KWIN_MCP_E2E_EDGE=<its launcher, e.g. an unpacked .deb's
//!       opt/microsoft/msedge/microsoft-edge, on a path a session sees>
//!
//! Every case runs on a synthetic default profile in a private HOME, so no account
//! data is involved. Run an ignored case with `systemd-run --user --wait --pipe
//! --collect env KWIN_MCP_E2E_SERVER=<binary> <test exe> --ignored --test-threads=1
//! NAME`; without KWIN_MCP_E2E_SERVER the binary built with the test is used.
//! KWIN_MCP_E2E_EVIDENCE=<dir> keeps a screenshot of each case.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
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
    workdir: PathBuf,
    steps: u32,
}

impl Desk {
    /// `prepare` fills the private HOME before the server starts. `config_home`
    /// is the server's `CHROME_CONFIG_HOME`, relative to that HOME; without it
    /// the variable is unset, whatever the test runner's environment holds.
    fn start(label: &str, config_home: Option<&str>, prepare: impl FnOnce(&Path)) -> Self {
        // Not under /tmp: a session has its own /tmp, which would hide what
        // `prepare` wrote.
        let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let home = root.join(format!("kwin-mcp-948-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
        std::fs::create_dir_all(home.join(".config")).expect("create private config home");
        let home = home.canonicalize().expect("resolve private HOME");
        prepare(&home);
        let program = std::env::var_os("KWIN_MCP_E2E_SERVER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")));
        let mut command = Command::new(program);
        command
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env_remove("CHROME_CONFIG_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(config_home) = config_home {
            command.env("CHROME_CONFIG_HOME", home.join(config_home));
        }
        let mut child = command.spawn().expect("spawn kwin-mcp");
        let mut desk = Self {
            stdin: child.stdin.take().expect("server stdin"),
            stdout: BufReader::new(child.stdout.take().expect("server stdout")),
            child,
            next_id: 0,
            home,
            session: String::new(),
            workdir: PathBuf::new(),
            steps: 0,
        };
        desk.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"default-profile-cdp-e2e","version":"1"}
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
        desk.workdir = PathBuf::from(field("host_shared_dir"));
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

    /// The structured result of a launch that opened or reused a window.
    fn launch(&mut self, command: &str, mut arguments: Value) -> Value {
        arguments["command"] = json!(command);
        arguments["window_timeout_seconds"] = json!(60);
        let launched = self.tool("launch_app", arguments);
        assert!(launched["error"].is_null(), "{launched}");
        assert_ne!(launched["result"]["isError"], json!(true), "{launched}");
        launched["result"]["structuredContent"].clone()
    }

    /// Runs a shell script in the session and returns what it printed.
    fn run(&mut self, script: &str) -> String {
        self.steps += 1;
        let done = self.workdir.join(format!("step-{}.out", self.steps));
        let part = self.workdir.join(format!("step-{}.part", self.steps));
        let command = format!(
            "{{ {script}\n}} > '{}' 2>&1; mv '{}' '{}'",
            part.display(),
            part.display(),
            done.display()
        );
        let launched = self.tool("launch_app", json!({"command":command, "expect_window":false}));
        assert!(launched["error"].is_null(), "{launched}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(output) = std::fs::read_to_string(&done) {
                return output;
            }
            assert!(Instant::now() < deadline, "the session never finished: {script}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The command line of the session's browser process named `process`.
    fn browser_argv(&mut self, process: &str) -> Vec<String> {
        self.run(&format!("tr '\\0' '\\n' < /proc/$(pgrep -o -x {process})/cmdline"))
            // Chrome rewrites its command line as one string, so spaces part
            // the switches too; no path in these cases holds one.
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// How many browsers (not their helper processes) named `process` run.
    fn browsers(&mut self, process: &str) -> usize {
        self.run(&format!(
            "for pid in $(pgrep -x {process}); do tr '\\0' ' ' < /proc/$pid/cmdline; echo; done"
        ))
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.contains("--type="))
        .count()
    }

    /// Ends the session's browser and waits until its processes are gone.
    fn quit(&mut self, process: &str) {
        self.run(&format!(
            "pkill -x {process}; while pgrep -x {process} > /dev/null; do sleep 0.1; done"
        ));
    }

    fn windows(&mut self) -> Vec<Value> {
        let listed = self.tool("window_list", json!({}));
        listed["result"]["structuredContent"]["windows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Waits until a window's title contains `part`.
    fn window_titled(&mut self, part: &str, seconds: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let shown = self.windows().iter().any(|window| {
                window["title"]
                    .as_str()
                    .is_some_and(|title| title.contains(part))
            });
            if shown || Instant::now() >= deadline {
                return shown;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// A page titled `name` that the session reads from the shared workdir.
    fn page(&self, name: &str) -> String {
        let path = self.workdir.join(format!("{name}.html"));
        std::fs::write(&path, format!("<!doctype html><title>{name}</title><h1>{name}</h1>"))
            .expect("write page");
        format!("file://{}", path.display())
    }

    /// The config home the wrapper gives a browser whose own is `config_home`.
    fn linked_config_home(&self, config_home: &Path) -> PathBuf {
        PathBuf::from(format!(
            "{}/browser-config{}",
            self.workdir.display(),
            config_home.display()
        ))
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

/// The session's forwarded port from a launch result that connected.
fn cdp_port(launched: &Value) -> u16 {
    assert_eq!(launched["cdp"], json!(true), "no CDP connection: {launched}");
    launched["cdp_port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or_else(|| panic!("no cdp_port: {launched}"))
}

/// The addresses of the tabs the browser behind the host's `port` lists.
fn tabs(port: u16) -> Vec<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the CDP port");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("CDP read timeout");
    write!(
        stream,
        "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("CDP request");
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => reply.extend_from_slice(&chunk[..read]),
        }
        let text = String::from_utf8_lossy(&reply);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .filter_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(str::to_owned))
                .filter_map(|value| value.trim().parse::<usize>().ok())
                .next();
            if length.is_some_and(|length| body.len() >= length) {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&reply).into_owned();
    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    serde_json::from_str::<Value>(body)
        .unwrap_or_else(|error| panic!("the CDP port did not answer with JSON ({error}): {text}"))
        .as_array()
        .map(|targets| {
            targets
                .iter()
                .filter(|target| target["type"] == json!("page"))
                .filter_map(|target| target["url"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Waits until the browser lists a tab for every one of `addresses`.
fn tabs_include(port: u16, addresses: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let open = tabs(port);
        if addresses.iter().all(|address| open.iter().any(|tab| tab == address))
            || Instant::now() >= deadline
        {
            return open;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// `--user-data-dir` values on a command line, in order.
fn data_dirs(argv: &[String]) -> Vec<&str> {
    argv.iter()
        .filter_map(|argument| argument.strip_prefix("--user-data-dir="))
        .collect()
}

fn has_port(argv: &[String]) -> bool {
    argv.iter()
        .any(|argument| argument.starts_with("--remote-debugging-port="))
}

/// The first `name` on the test's PATH: the browser the wrapper would run.
fn on_path(name: &str) -> Option<PathBuf> {
    for directory in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A data directory that lists two profiles used just now, so Chrome opens its
/// profile picker for a launch that names neither.
fn two_profiles(home: &Path) {
    let chrome = home.join(".config/google-chrome");
    std::fs::create_dir_all(&chrome).expect("create Chrome data directory");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs_f64();
    let state = json!({"profile":{
        "info_cache":{
            "Default":{"name":"Alpha", "is_using_default_name":false, "active_time":now},
            "Profile 2":{"name":"Beta", "is_using_default_name":false, "active_time":now},
        },
        "last_used":"Default", "last_active_profiles":["Default"],
        "profiles_order":["Default", "Profile 2"], "profiles_created":3,
        "picker_shown":true, "show_picker_on_startup":true,
    }});
    std::fs::write(chrome.join("Local State"), state.to_string()).expect("write Local State");
}

/// Which of the two profiles have been opened in the session.
fn opened_profiles(desk: &mut Desk) -> (bool, bool) {
    let listed = desk.run(
        "cd \"$HOME/.config/google-chrome\" && for profile in Default 'Profile 2'; do test -d \"$profile\" && echo \"opened=$profile\"; done; true",
    );
    (
        listed.contains("opened=Default"),
        listed.contains("opened=Profile 2"),
    )
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn chrome_without_a_data_directory_gets_the_port_on_its_default_profile() {
    let mut desk = Desk::start("default", None, |_| {});
    let profile = desk.home.join(".config/google-chrome");
    let pages: Vec<String> = ["one", "two", "three", "four", "five"]
        .iter()
        .map(|name| desk.page(name))
        .collect();
    let chrome = on_path("google-chrome-stable").expect("google-chrome-stable on PATH");

    let first = desk.launch(&format!("google-chrome-stable {}", pages[0]), json!({}));
    let port = cdp_port(&first);
    let argv = desk.browser_argv("chrome");
    let state = desk.run("test -f \"$HOME/.config/google-chrome/Local State\" && echo state=present");
    // The variable is the browser's alone.
    let session_variable = desk.run("echo \"variable=${CHROME_CONFIG_HOME-unset}\"");
    let link = desk.linked_config_home(&desk.home.join(".config")).join("google-chrome");
    let linked_to = std::fs::read_link(&link);

    let navigated = desk.tool("browser_navigate", json!({"url":pages[1]}));
    let scrolled = desk.tool("mouse_scroll", json!({"x":400, "y":300, "delta":120}));
    // A second start through the wrapper, one by absolute path as a desktop
    // file or xdg-open makes it, and one as a program Chrome started makes it
    // (with the variable Chrome was given): each reaches the running browser.
    let second = desk.launch(&format!("google-chrome-stable {}", pages[2]), json!({}));
    let direct = desk.launch(&format!("{} {}", chrome.display(), pages[3]), json!({}));
    let inherited = desk.launch(
        &format!(
            "CHROME_CONFIG_HOME='{}' {} {}",
            desk.linked_config_home(&desk.home.join(".config")).display(),
            chrome.display(),
            pages[4]
        ),
        json!({}),
    );
    let expected: Vec<&str> = pages[1..].iter().map(String::as_str).collect();
    let open = tabs_include(port, &expected);
    let browsers = desk.browsers("chrome");
    desk.evidence("default-profile");

    assert_eq!(data_dirs(&argv), [profile.to_string_lossy()], "{argv:?}");
    assert!(state.contains("state=present"), "the default directory holds no profile: {state}");
    assert!(session_variable.contains("variable=unset"), "{session_variable}");
    assert_eq!(linked_to.ok(), Some(profile), "{}", link.display());
    assert_eq!(
        navigated["result"]["structuredContent"]["method"], "cdp",
        "{navigated}"
    );
    assert!(
        scrolled["result"]["structuredContent"]["scroll_y"].is_number(),
        "scroll reporting needs CDP: {scrolled}"
    );
    for (how, launched) in [("wrapper", &second), ("absolute path", &direct), ("inherited", &inherited)] {
        assert_eq!(launched["window_outcome"], "reused_existing", "{how}: {launched}");
    }
    assert_eq!(cdp_port(&second), port);
    for page in &expected {
        assert!(open.iter().any(|tab| tab == page), "{page} is not among {open:?}");
    }
    assert_eq!(browsers, 1, "every launch reaches one browser");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_profile_directory_opens_that_profile_with_the_port() {
    let mut desk = Desk::start("profile", None, two_profiles);
    let page = desk.page("second-profile");
    let launched = desk.launch(
        &format!("google-chrome-stable --profile-directory=\"Profile 2\" {page}"),
        json!({}),
    );
    let port = cdp_port(&launched);
    let open = tabs_include(port, &[&page]);
    let (default, second) = opened_profiles(&mut desk);
    let argv = desk.browser_argv("chrome");
    desk.evidence("profile-directory");

    assert!(open.contains(&page), "{open:?}");
    assert!(second && !default, "opened Default: {default}, Profile 2: {second}");
    assert_eq!(
        data_dirs(&argv),
        [desk.home.join(".config/google-chrome").to_string_lossy()]
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn the_profile_picker_window_has_the_port() {
    let mut desk = Desk::start("picker", None, two_profiles);
    let page = desk.page("after-picker");
    let launched = desk.launch("google-chrome-stable", json!({}));
    let port = cdp_port(&launched);
    desk.evidence("picker");
    let reply = desk.tool("mouse_click_element", json!({"query":"Beta", "timeout_seconds":30}));
    let browser = desk.window_titled(" - Google Chrome", 40);
    let navigated = desk.tool("browser_navigate", json!({"url":page}));
    let open = tabs_include(port, &[&page]);
    let (default, second) = opened_profiles(&mut desk);
    desk.evidence("picker-after");

    assert_eq!(launched["window_title"], "Google Chrome", "the picker: {launched}");
    assert!(reply["error"].is_null(), "{reply}");
    assert!(browser, "no browser window opened after {reply}");
    assert_eq!(
        navigated["result"]["structuredContent"]["method"], "cdp",
        "{navigated}"
    );
    assert!(open.contains(&page), "{open:?}");
    assert!(second && !default, "opened Default: {default}, Profile 2: {second}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_preset_config_home_and_a_launcher_flags_file_are_followed() {
    let mut desk = Desk::start("preset", Some(".config/chrome-default"), |home| {
        let profile = home.join(".config/google-chrome");
        let preset = home.join(".config/chrome-default");
        std::fs::create_dir_all(&profile).expect("create Chrome data directory");
        std::fs::create_dir_all(&preset).expect("create the preset config home");
        std::os::unix::fs::symlink(&profile, preset.join("google-chrome")).expect("link the default");
        // Read by a launcher that has such a file (Arch); ignored by others.
        std::fs::write(
            home.join(".config/chrome-flags.conf"),
            format!("# kwin-mcp e2e\n--user-data-dir={}\n", profile.display()),
        )
        .expect("write the flags file");
    });
    let profile = desk.home.join(".config/google-chrome");
    let page = desk.page("preset");
    let launched = desk.launch(&format!("google-chrome-stable {page}"), json!({}));
    let port = cdp_port(&launched);
    let argv = desk.browser_argv("chrome");
    let open = tabs_include(port, &[&page]);
    let link = desk
        .linked_config_home(&desk.home.join(".config/chrome-default"))
        .join("google-chrome");
    let linked_to = std::fs::read_link(&link);
    desk.evidence("preset-config-home");

    // A caller's own directory still wins over the flags file.
    desk.quit("chrome");
    let own = desk.launch(
        &format!("google-chrome-stable --user-data-dir=\"$HOME/own-profile\" {page}"),
        json!({}),
    );
    let own_port = cdp_port(&own);
    let own_argv = desk.browser_argv("chrome");
    let own_state = desk.run("test -f \"$HOME/own-profile/Local State\" && echo state=present");

    // Once, whether the launcher or the wrapper named it.
    assert_eq!(data_dirs(&argv), [profile.to_string_lossy()], "{argv:?}");
    assert!(open.contains(&page), "{open:?}");
    assert_eq!(linked_to.ok(), Some(profile), "{}", link.display());
    assert_eq!(own_port, port);
    assert_eq!(
        data_dirs(&own_argv).last().copied(),
        Some(desk.home.join("own-profile").to_string_lossy().as_ref()),
        "{own_argv:?}"
    );
    assert!(own_state.contains("state=present"), "{own_state}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn cdp_false_starts_chrome_without_a_port() {
    let mut desk = Desk::start("optout", None, |_| {});
    let page = desk.page("no-port");
    let launched = desk.launch(&format!("google-chrome-stable {page}"), json!({"cdp":false}));
    let argv = desk.browser_argv("chrome");
    let navigated = desk.tool("browser_navigate", json!({"url":desk.page("typed")}));

    assert_ne!(launched["cdp"], json!(true), "{launched}");
    assert!(launched["cdp_port"].is_null(), "{launched}");
    assert!(!has_port(&argv) && data_dirs(&argv).is_empty(), "{argv:?}");
    assert!(!desk.workdir.join("browser-config").exists());
    let method = navigated["result"]["structuredContent"]["method"].as_str();
    assert!(method.is_some_and(|method| method != "cdp"), "{navigated}");
}

/// A later Chrome may resolve the default it derives. This stand-in does: it
/// refuses the port, in Chrome's words, when its default is a symlink.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn a_browser_that_resolves_the_linked_default_gets_an_unlinked_one() {
    let chrome = on_path("google-chrome-stable").expect("google-chrome-stable on PATH");
    let mut desk = Desk::start("resolving", None, |home| {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join("resolving-bin");
        std::fs::create_dir_all(&bin).expect("create the stand-in's directory");
        let stand_in = bin.join("google-chrome-stable");
        std::fs::write(
            &stand_in,
            format!(
                "#!/usr/bin/env bash\nargs=()\nfor arg in \"$@\"; do\n  if [[ $arg == --remote-debugging-port=* && -L $CHROME_CONFIG_HOME/google-chrome ]]; then\n    echo 'ERROR: DevTools remote debugging requires a non-default data directory. Specify this using --user-data-dir.' >&2\n  else\n    args+=(\"$arg\")\n  fi\ndone\nexec '{}' \"${{args[@]}}\"\n",
                chrome.display()
            ),
        )
        .expect("write the stand-in");
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");
    });
    let page = desk.page("resolving");
    // The wrapper's directory first, as launch_app puts it, then the stand-in.
    let command = format!(
        "PATH=\"{}/browser-bin:$HOME/resolving-bin:$PATH\" google-chrome-stable {page}",
        desk.workdir.display()
    );
    let refused = desk.launch(&command, json!({}));
    let mark = desk.workdir.join("browser-config/unlinked").exists();
    desk.quit("chrome");
    let accepted = desk.launch(&command, json!({}));
    let port = cdp_port(&accepted);
    let argv = desk.browser_argv("chrome");
    let open = tabs_include(port, &[&page]);

    assert_ne!(refused["cdp"], json!(true), "{refused}");
    assert!(
        refused["cdp_refused"].as_str().is_some_and(|note| note.contains("launch it again")),
        "{refused}"
    );
    assert!(mark, "the refusal left no mark for the wrapper");
    assert!(accepted["cdp_refused"].is_null(), "{accepted}");
    assert_eq!(
        data_dirs(&argv),
        [desk.home.join(".config/google-chrome").to_string_lossy()]
    );
    assert!(open.contains(&page), "{open:?}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Microsoft Edge, and input devices"]
fn edge_on_its_default_profile_gets_the_port() {
    let unpacked = std::env::var_os("KWIN_MCP_E2E_EDGE").map(PathBuf::from);
    if unpacked.is_none() && on_path("microsoft-edge-stable").is_none() {
        eprintln!("skipped: no microsoft-edge-stable on PATH and no KWIN_MCP_E2E_EDGE");
        return;
    }
    let mut desk = Desk::start("edge", None, |home| {
        if let Some(launcher) = &unpacked {
            let bin = home.join("edge-bin");
            std::fs::create_dir_all(&bin).expect("create the Edge directory");
            std::os::unix::fs::symlink(launcher, bin.join("microsoft-edge-stable"))
                .expect("link the Edge launcher");
        }
    });
    let profile = desk.home.join(".config/microsoft-edge");
    let (one, two) = (desk.page("edge-one"), desk.page("edge-two"));
    let edge = format!(
        "PATH=\"{}/browser-bin:$HOME/edge-bin:$PATH\" microsoft-edge-stable",
        desk.workdir.display()
    );
    let first = desk.launch(&format!("{edge} {one}"), json!({}));
    let port = cdp_port(&first);
    let argv = desk.browser_argv("msedge");
    let navigated = desk.tool("browser_navigate", json!({"url":desk.page("edge-navigated")}));
    let second = desk.launch(&format!("{edge} {two}"), json!({}));
    let open = tabs_include(port, &[&two]);
    let browsers = desk.browsers("msedge");
    desk.evidence("edge-default-profile");

    assert_eq!(data_dirs(&argv), [profile.to_string_lossy()], "{argv:?}");
    assert_eq!(
        navigated["result"]["structuredContent"]["method"], "cdp",
        "{navigated}"
    );
    assert_eq!(second["window_outcome"], "reused_existing", "{second}");
    assert!(open.contains(&two), "{open:?}");
    assert_eq!(browsers, 1);
}
