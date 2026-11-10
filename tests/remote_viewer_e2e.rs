#![allow(clippy::expect_used)]

//! `viewer_open` on a session that runs under another kwin-mcp (`--listen`)
//! opens the viewer window on the caller's machine. The session's machine
//! sends frames on a WebSocket of their own, only when the screen changed,
//! scaled to the window, raw or as DSC by the link budget; the window's input
//! returns on the same socket. Closing either socket ends the capture.
//!
//! The window opens inside a third, isolated desktop (`desk`) instead of the
//! tester's own, which also lets the test size the window and type into it.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// Length of the header before each frame's pixels.
fn header_bytes() -> u64 {
    u64::try_from(kwin_mcp::viewer_stream::HEADER_BYTES).expect("header length")
}

/// A private HOME holding the shared secret, under `KWIN_MCP_TEST_TMPDIR` or
/// `/var/tmp`: the sessions' disk overlays live in it, and `/tmp` has a quota.
fn home_with_secret(label: &str, secret: &str) -> PathBuf {
    let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    let home = root.join(format!("kwin-mcp-remote-{label}-{}", std::process::id()));
    let config = home.join(".config/kwin-mcp");
    std::fs::create_dir_all(&config).expect("create config directory");
    std::fs::create_dir_all(home.join(".cache")).expect("create cache directory");
    let path = config.join("remote-secret");
    std::fs::write(&path, format!("{secret}\n")).expect("write secret");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("restrict secret");
    home
}

fn kwin_mcp(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"));
    command
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("XDG_CONFIG_HOME");
    command
}

fn log_file(home: &Path, name: &str) -> Stdio {
    Stdio::from(std::fs::File::create(home.join(name)).expect("create log"))
}

struct Listener {
    child: Child,
    address: String,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Listener {
    fn start(home: &Path, options: &[&str]) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|socket| socket.local_addr())
            .expect("reserve a port")
            .port();
        let address = format!("127.0.0.1:{port}");
        let mut child = kwin_mcp(home)
            .args(["--listen", &address, "--autoclean"])
            .args(options)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_file(home, "listener.log"))
            .spawn()
            .expect("spawn listener");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(&address).is_err() {
            assert!(
                child.try_wait().expect("poll listener").is_none(),
                "the listener exited before accepting connections"
            );
            assert!(Instant::now() < deadline, "the listener never accepted");
            std::thread::sleep(Duration::from_millis(100));
        }
        Self { child, address }
    }
}

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
    /// A stdio server; with `display`, its viewer windows open on that
    /// Wayland socket.
    fn start(home: &Path, label: &str, display: Option<&Path>) -> Self {
        let mut command = kwin_mcp(home);
        if let Some(display) = display {
            command.env("WAYLAND_DISPLAY", display);
        }
        let mut child = command
            .arg("--autoclean")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log_file(home, &format!("{label}.log")))
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
                "clientInfo":{"name":"remote-viewer-e2e","version":"1"}
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

    /// Calls a tool and returns its structured content.
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name":name, "arguments":arguments}));
        assert!(reply["error"].is_null(), "{name} failed: {reply}");
        reply["result"]["structuredContent"].clone()
    }

    /// Starts a session and returns its ID and work directory.
    fn start_session(&mut self, arguments: Value) -> (String, PathBuf) {
        let started = self.tool("session_start", arguments);
        let session = started["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {started}"))
            .to_owned();
        let workdir = PathBuf::from(started["host_shared_dir"].as_str().expect("workdir"));
        (session, workdir)
    }
}

fn alive(pid: u64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Polls `check` until it returns a value.
fn wait_for<T>(what: &str, within: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The first record of `list[key]` that `wanted` accepts.
fn record(list: &Value, key: &str, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
    for item in list[key].as_array()? {
        if wanted(item) {
            return Some(item.clone());
        }
    }
    None
}

/// The viewer window's process and the counters it writes.
struct Viewer {
    pid: u64,
    counters: PathBuf,
}

impl Viewer {
    /// Checks a `viewer_open` result that must report a ready viewer.
    fn opened(result: &Value, status: &str, host: &str) -> Self {
        assert_eq!(result["status"], status, "{result}");
        assert_eq!(result["viewer"]["state"], "ready", "{result}");
        assert_eq!(result["host"], host, "{result}");
        let log = Path::new(result["viewer"]["log"].as_str().expect("viewer log"));
        Self {
            pid: result["viewer"]["pid"].as_u64().expect("viewer pid"),
            counters: log
                .parent()
                .expect("viewer directory")
                .join(kwin_mcp::viewer_stream::COUNTERS_FILE),
        }
    }

    fn counters(&self) -> Option<Value> {
        read_json(&self.counters)
    }

    fn frames(&self) -> u64 {
        self.counters()
            .and_then(|counters| counters["frames"].as_u64())
            .unwrap_or(0)
    }

    /// Waits until the viewer has gone a second without a frame, and returns
    /// how many it has received.
    fn idle(&self) -> u64 {
        wait_for("an idle second", Duration::from_secs(20), || {
            let counters = self.counters()?;
            (counters["last_second"]["frames"] == 0).then(|| counters["frames"].as_u64())?
        })
    }

    /// Waits for frames of the given size and returns the counters.
    fn sized(&self, width: u64, height: u64) -> Value {
        wait_for(
            "frames of the window's size",
            Duration::from_secs(15),
            || {
                self.counters()
                    .filter(|counters| counters["width"] == width && counters["height"] == height)
            },
        )
    }
}

/// The frame sender of `workdir`'s session, once it reports the viewer ready.
fn sender_pid(workdir: &Path) -> u64 {
    wait_for("the frame sender", Duration::from_secs(10), || {
        let status = read_json(&workdir.join("viewer-status.json"))?;
        (status["state"] == "ready").then(|| status["pid"].as_u64())?
    })
}

fn stopped(what: &str, pid: u64) {
    wait_for(what, Duration::from_secs(10), || {
        (!alive(pid)).then_some(())
    });
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn a_remote_session_streams_changed_frames_to_a_viewer_on_the_callers_machine() {
    let home = home_with_secret("viewer", "shared-secret-0123456789");

    // The desktop the viewer window opens in. Native mode keeps the size the
    // viewer asks for; KWin's Meta + right-button drag resizes it later.
    let mut desk = Server::start(&home, "desk", None);
    let (desk_session, desk_dir) =
        desk.start_session(json!({"width":1600, "height":1000, "window_mode":"native"}));
    // Drags the active window's bottom right corner from one size to another.
    let resize_viewer = |desk: &mut Server, from: (u32, u32), to: (u32, u32)| {
        const INSET: u32 = 20;
        desk.tool(
            "keyboard_press",
            json!({"session_id":desk_session, "key":"super"}),
        );
        desk.tool(
            "mouse_drag",
            json!({
                "session_id":desk_session, "button":"right",
                "from_x":from.0 - INSET, "from_y":from.1 - INSET,
                "to_x":to.0 - INSET, "to_y":to.1 - INSET,
            }),
        );
        desk.tool(
            "keyboard_release",
            json!({"session_id":desk_session, "key":"super"}),
        );
    };

    // A 400 Mb/s link carries 360 Mb/s: 640x360 fits raw at 60 Hz (332 Mb/s),
    // 1280x720 does not (1327 Mb/s) and exceeds it even as DSC at 8 bpp (442 Mb/s).
    let mut listener = Listener::start(&home, &["--link-mbps", "400"]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk_dir.join("wayland-0")));
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let listed = server.tool("session_list", json!({}));
    let sandbox = record(&listed, "sessions", |listed| {
        listed["session_id"] == session.as_str()
    })
    .and_then(|listed| listed["sandbox_pid"].as_u64())
    .unwrap_or_else(|| panic!("remote session not listed: {listed}"));
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );

    // The window opens on the caller's side and reports ready as a local one does.
    let opened = server.tool(
        "viewer_open",
        json!({"session_id":session, "reason":"watch"}),
    );
    let viewer = Viewer::opened(&opened, "opened", &host);
    let sender = sender_pid(&workdir);
    wait_for("the first frame", Duration::from_secs(10), || {
        (viewer.frames() > 0).then_some(())
    });
    let title = format!("kwin-viewer {session}: watch");
    let windows = desk.tool("window_list", json!({"session_id":desk_session}));
    let window = record(&windows, "windows", |window| {
        window["title"]
            .as_str()
            .is_some_and(|shown| shown.starts_with(&title))
    })
    .unwrap_or_else(|| panic!("no viewer window on the caller's desktop: {windows}"))["id"]
        .clone();
    desk.tool(
        "window_activate",
        json!({"session_id":desk_session, "window_id":window}),
    );

    // The window starts at the session's size: frames of 1280x720, as DSC.
    let full = viewer.sized(1280, 720);
    assert_eq!(full["mode"], "dsc", "{full}");

    // A window of 640x360 gets frames of 640x360, raw: 3 bytes per pixel.
    resize_viewer(&mut desk, (1280, 720), (640, 360));
    let small = viewer.sized(640, 360);
    assert_eq!(small["mode"], "raw", "{small}");
    assert_eq!(
        small["frame_bytes"],
        header_bytes() + 640 * 360 * 3,
        "{small}"
    );

    // An unchanged screen sends nothing. (The agent's input also waits out the
    // quiet second after the Meta key the resize sent through the viewer.)
    let idle = viewer.idle();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        viewer.frames(),
        idle,
        "frames arrived although the screen was idle"
    );

    // The agent's typing changes the screen, so frames follow.
    server.tool(
        "keyboard_type",
        json!({"session_id":session, "text":"echo frames"}),
    );
    wait_for("frames after typing", Duration::from_secs(10), || {
        (viewer.frames() > idle).then_some(())
    });
    server.tool(
        "keyboard_key",
        json!({"session_id":session, "key":"ctrl+c"}),
    );

    // The agent's screenshot is the session's own picture, not the viewer's.
    let shot = server.tool("screenshot", json!({"session_id":session}));
    assert_eq!(
        (shot["width"].as_u64(), shot["height"].as_u64()),
        (Some(1280), Some(720)),
        "{shot}"
    );

    // Keys typed into the window reach the session over the frame socket.
    let typed = workdir.join("typed-in-viewer");
    // keyboard_type needs an accessible target, which the viewer window is
    // not, so the command goes in key by key.
    for character in format!("touch {}\n", typed.display()).chars() {
        let key = match character {
            ' ' => "Space".to_owned(),
            '/' => "slash".to_owned(),
            '-' => "minus".to_owned(),
            '\n' => "Return".to_owned(),
            other => other.to_string(),
        };
        desk.tool(
            "keyboard_key",
            json!({"session_id":desk_session, "key":key}),
        );
    }
    wait_for(
        "the file made from the viewer",
        Duration::from_secs(10),
        || typed.exists().then_some(()),
    );

    // A larger window gets larger frames, now DSC; the budget lowers the rate
    // from 8 bpp, and each rate arrives with its parameter set.
    resize_viewer(&mut desk, (640, 360), (1280, 720));
    viewer.sized(1280, 720);
    // Agent input waits out the quiet second after the viewer's keys.
    std::thread::sleep(Duration::from_millis(1500));
    server.tool(
        "keyboard_type",
        json!({"session_id":session, "text":"echo compressed frames"}),
    );
    let large = wait_for("a lowered DSC rate", Duration::from_secs(15), || {
        viewer.counters().filter(|counters| {
            counters["bits_per_pixel"]
                .as_f64()
                .is_some_and(|rate| rate < 8.0)
        })
    });
    assert_eq!(large["mode"], "dsc", "{large}");
    assert!(
        large["bits_per_pixel"]
            .as_f64()
            .is_some_and(|rate| rate >= 6.0),
        "{large}"
    );
    assert!(
        large["parameter_sets"]
            .as_u64()
            .is_some_and(|sets| sets >= 2),
        "{large}"
    );
    assert!(
        large["frame_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes < header_bytes() + 1280 * 720),
        "{large}"
    );

    // A second viewer_open keeps the window and only changes its title.
    let again = server.tool(
        "viewer_open",
        json!({"session_id":session, "reason":"still here"}),
    );
    assert_eq!(
        Viewer::opened(&again, "already_open", &host).pid,
        viewer.pid
    );

    // The frame socket ends (the user closes the window): the capture stops,
    // the session does not.
    Command::new("kill")
        .arg(viewer.pid.to_string())
        .status()
        .expect("close the viewer");
    stopped("the frame sender to stop with its socket", sender);
    assert!(alive(sandbox), "the session ended with its viewer");

    // viewer_open opens a new window; viewer_close closes it and the sender.
    let reopened = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&reopened, "opened", &host);
    let sender = sender_pid(&workdir);
    let closed = server.tool("viewer_close", json!({"session_id":session}));
    assert_eq!(closed["status"], "closed", "{closed}");
    assert_eq!(closed["viewer"]["state"], "closed", "{closed}");
    assert!(!alive(viewer.pid), "viewer_close left the window open");
    stopped("the frame sender to stop after viewer_close", sender);
    assert!(alive(sandbox), "the session ended with its viewer");

    // The control connection ends (the caller dies): the listener stops the
    // capture and the session, and the window closes with its server.
    let last = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&last, "opened", &host);
    let sender = sender_pid(&workdir);
    server.child.kill().expect("kill the calling server");
    server.child.wait().expect("reap the calling server");
    wait_for(
        "the listener to stop the capture and the session",
        Duration::from_secs(30),
        || (!alive(sender) && !alive(sandbox)).then_some(()),
    );
    stopped("the window to close with its server", viewer.pid);
    assert!(
        listener.child.try_wait().expect("poll listener").is_none(),
        "the listener exited with its peer"
    );

    drop(server);
    drop(listener);
    drop(desk);
    let _ = std::fs::remove_dir_all(home);
}
