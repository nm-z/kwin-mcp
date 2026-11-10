#![allow(clippy::expect_used)]

//! `viewer_open` on a session that runs under another kwin-mcp (`--listen`)
//! opens the viewer window on the caller's machine. The session's machine
//! sends frames on a WebSocket of their own, only when the screen changed,
//! scaled to the window: exact pixels whenever the link carries them at a
//! usable rate, DSC only when that is sooner, and a still DSC picture once
//! more exactly. The window's input returns on the same socket. Closing
//! either socket ends the capture.
//!
//! The windows open inside another isolated desktop (`Desk`) instead of the
//! tester's own, which also lets a test size a window and type into it.
//!
//! `KWIN_MCP_TEST_BINARY` names a kwin-mcp to test instead of this build's
//! (its kwin-viewer is the one beside it).

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
    let binary = std::env::var_os("KWIN_MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")));
    let mut command = Command::new(binary);
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
            .stderr(log_file(home, &format!("listener-{port}.log")))
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
    /// Wayland socket; with `dump`, its stream viewers write the picture they
    /// last presented there whenever a second passes without a frame.
    fn start(home: &Path, label: &str, display: Option<&Path>, dump: Option<&Path>) -> Self {
        let mut command = kwin_mcp(home);
        if let Some(display) = display {
            command.env("WAYLAND_DISPLAY", display);
        }
        if let Some(dump) = dump {
            command.env(kwin_mcp::viewer_stream::DUMP_ENV, dump);
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

    /// The sandbox process of a session this server started on another machine.
    fn sandbox(&mut self, session: &str) -> u64 {
        let listed = self.tool("session_list", json!({}));
        record(&listed, "sessions", |listed| {
            listed["session_id"] == session
        })
        .and_then(|listed| listed["sandbox_pid"].as_u64())
        .unwrap_or_else(|| panic!("remote session not listed: {listed}"))
    }
}

/// The desktop the viewer windows open in. Native mode keeps the size a
/// viewer asks for; KWin's Meta + right-button drag resizes it later.
struct Desk {
    server: Server,
    session: String,
    dir: PathBuf,
}

impl Desk {
    fn start(home: &Path, width: u32, height: u32) -> Self {
        let mut server = Server::start(home, "desk", None, None);
        let (session, dir) =
            server.start_session(json!({"width":width, "height":height, "window_mode":"native"}));
        Self {
            server,
            session,
            dir,
        }
    }

    fn display(&self) -> PathBuf {
        self.dir.join("wayland-0")
    }

    fn tool(&mut self, name: &str, mut arguments: Value) -> Value {
        arguments["session_id"] = json!(self.session);
        self.server.tool(name, arguments)
    }

    /// Makes the viewer window of `session` the active one.
    fn activate(&mut self, session: &str) {
        let title = format!("kwin-viewer {session}");
        let window = wait_for(
            "the viewer window on the caller's desktop",
            Duration::from_secs(10),
            || {
                let windows = self.tool("window_list", json!({}));
                record(&windows, "windows", |window| {
                    window["title"]
                        .as_str()
                        .is_some_and(|shown| shown.starts_with(&title))
                })
            },
        )["id"]
            .clone();
        self.tool("window_activate", json!({"window_id":window}));
    }

    /// Drags the active window's bottom right corner from one size to another.
    fn resize(&mut self, from: (u32, u32), to: (u32, u32)) {
        const INSET: u32 = 20;
        self.tool("keyboard_press", json!({"key":"super"}));
        self.tool(
            "mouse_drag",
            json!({
                "button":"right",
                "from_x":from.0 - INSET, "from_y":from.1 - INSET,
                "to_x":to.0 - INSET, "to_y":to.1 - INSET,
            }),
        );
        self.tool("keyboard_release", json!({"key":"super"}));
    }

    fn stop(mut self) {
        self.tool("session_stop", json!({}));
    }
}

fn alive(pid: u64) -> bool {
    // A process that has exited and waits to be reaped is not running.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

/// Processor time a process has used, in seconds (user and system, all
/// threads).
fn cpu_seconds(pid: u64) -> f64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit_once(") ")
        .map_or(Vec::new(), |(_, rest)| rest.split(' ').collect());
    // utime and stime are fields 14 and 15 of the line, in clock ticks of
    // 1/100 s.
    let ticks: f64 = [11, 12]
        .iter()
        .filter_map(|index| fields.get(*index)?.parse::<f64>().ok())
        .sum();
    ticks / 100.0
}

/// Resident memory of a process in KiB.
fn resident_kib(pid: u64) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .filter_map(|line| line.strip_prefix("VmRSS:"))
        .filter_map(|rest| rest.trim().strip_suffix("kB")?.trim().parse().ok())
        .next()
}

/// Running kwin-viewer processes (windows and frame senders) that were
/// started under the test's HOME, with their command lines.
fn viewers_under(home: &Path) -> Vec<(u64, String)> {
    let marker = format!("HOME={}", home.display());
    let mut running = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("read /proc").flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        let Ok(command) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let command = String::from_utf8_lossy(&command).replace('\0', " ");
        let environment = std::fs::read(entry.path().join("environ")).unwrap_or_default();
        let ours = environment
            .split(|byte| *byte == 0)
            .any(|variable| variable == marker.as_bytes());
        if ours && command.contains("kwin-viewer") && alive(pid) {
            running.push((pid, command));
        }
    }
    running
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

/// The viewer window's process, its log and the counters it writes.
struct Viewer {
    pid: u64,
    log: PathBuf,
    counters: PathBuf,
}

impl Viewer {
    /// Checks a `viewer_open` result that must report a ready viewer.
    fn opened(result: &Value, status: &str, host: &str) -> Self {
        let log = PathBuf::from(result["viewer"]["log"].as_str().expect("viewer log"));
        assert_eq!(
            result["status"],
            status,
            "{result}\nviewer log:\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert_eq!(result["viewer"]["state"], "ready", "{result}");
        assert_eq!(result["host"], host, "{result}");
        Self {
            pid: result["viewer"]["pid"].as_u64().expect("viewer pid"),
            counters: log
                .parent()
                .expect("viewer directory")
                .join(kwin_mcp::viewer_stream::COUNTERS_FILE),
            log,
        }
    }

    fn counters(&self) -> Option<Value> {
        read_json(&self.counters)
    }

    fn count(&self, name: &str) -> u64 {
        self.counters()
            .and_then(|counters| counters[name].as_u64())
            .unwrap_or(0)
    }

    fn frames(&self) -> u64 {
        self.count("frames")
    }

    /// Waits until the viewer has gone a second without a frame, and returns
    /// its counters.
    fn idle(&self) -> Value {
        wait_for("an idle second", Duration::from_secs(30), || {
            self.counters()
                .filter(|counters| counters["last_second"]["frames"] == 0)
        })
    }

    /// Waits until the viewer has gone a second without a frame and the
    /// frame it shows is an exact one, and returns its counters. (A settle
    /// frame can take longer than a second to cross a slow link, so a still
    /// screen may first be reported while its exact frame is on the way.)
    fn still_and_exact(&self) -> Value {
        wait_for("a still, exact frame", Duration::from_secs(30), || {
            self.counters().filter(|counters| {
                counters["last_second"]["frames"] == 0 && counters["mode"] == "raw"
            })
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

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
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

fn signal(pid: u64, name: &str) {
    let status = Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -{name} {pid}");
}

/// Lines of a stream log that report a failure of the frame socket or of a
/// picture. A socket closing because the other side ended is not one.
fn stream_errors(log: &str) -> Vec<&str> {
    const FAILURES: [&str; 8] = [
        "frame socket: ",
        "unknown frame message",
        "picture parameter set: ",
        "without its parameter set",
        "invalid input record",
        "unreadable notice",
        "panicked",
        "took none of",
    ];
    log.lines()
        .filter(|line| FAILURES.iter().any(|failure| line.contains(failure)))
        .collect()
}

/// A PNG's size and its pixels as 3 bytes each (red, green, blue).
fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::fs::File::open(path).expect("open PNG"));
    let mut reader = decoder.read_info().expect("PNG header");
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).expect("PNG pixels");
    assert_eq!(info.bit_depth, png::BitDepth::Eight, "{}", path.display());
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other @ (png::ColorType::Grayscale
        | png::ColorType::Indexed
        | png::ColorType::GrayscaleAlpha) => {
            panic!("{}: unexpected colour type {other:?}", path.display())
        }
    };
    let rgb = buffer[..info.buffer_size()]
        .chunks_exact(channels)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    (info.width, info.height, rgb)
}

/// Writes a picture with every value of each channel, ramps of mixed colours
/// and fine detail, so a channel swap, a transfer curve or a lossy codec all
/// change it.
fn write_test_picture(path: &Path, width: u32, height: u32) {
    let mut rgb = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let ramp = u8::try_from(x * 256 / width).expect("ramp");
            let band = y * 8 / height;
            let fine = u8::try_from((x * 7 + y * 13) % 256).expect("detail");
            rgb.extend_from_slice(&match band {
                0 => [ramp, 0, 0],
                1 => [0, ramp, 0],
                2 => [0, 0, ramp],
                3 => [ramp, ramp, ramp],
                4 => [ramp, 255 - ramp, 128],
                5 => [fine, ramp, 255 - fine],
                6 => [
                    if (x + y) % 2 == 0 { 255 } else { 0 },
                    fine,
                    if x % 3 == 0 { 200 } else { 30 },
                ],
                _ => [255 - ramp, fine, ramp],
            });
        }
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path).expect("create picture"));
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("PNG header");
    writer.write_image_data(&rgb).expect("PNG pixels");
}

/// How two pictures of the same size differ outside `skip` (x1, y1, x2, y2):
/// the number of differing pixels and the largest channel difference.
fn difference(left: &[u8], right: &[u8], width: u32, skip: (u32, u32, u32, u32)) -> (usize, u8) {
    assert_eq!(left.len(), right.len(), "pictures of different sizes");
    let width = usize::try_from(width).expect("width");
    let inside = |index: usize| {
        let (x, y) = (
            u32::try_from(index % width).expect("x"),
            u32::try_from(index / width).expect("y"),
        );
        x >= skip.0 && x < skip.2 && y >= skip.1 && y < skip.3
    };
    let mut differing = 0;
    let mut largest = 0;
    for (index, (a, b)) in left.chunks_exact(3).zip(right.chunks_exact(3)).enumerate() {
        if a != b && !inside(index) {
            differing += 1;
            largest = a
                .iter()
                .zip(b)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0)
                .max(largest);
        }
    }
    (differing, largest)
}

/// A command for `launch_app` that scrolls text in a maximized Konsole for
/// `scroll` seconds, pauses for `pause` seconds, and starts again.
fn scrolling_text(scroll: u32, pause: u32) -> String {
    let text = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
    format!(
        "konsole -e bash -c 'while :; do end=$((SECONDS+{scroll})); while [ $SECONDS -lt $end ]; do cat {text}; done; sleep {pause}; done'"
    )
}

const TEST_NEEDS: &str =
    "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices";

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn a_remote_session_streams_changed_frames_to_a_viewer_on_the_callers_machine() {
    let home = home_with_secret("viewer", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1600, 1000);

    // A 20 Mb/s link carries 18 Mb/s. A 320x180 frame is 1.4 Mbit raw and
    // crosses it in 77 ms, so it goes out exactly. A 1280x720 frame is 22
    // Mbit raw (1.2 s), and 5.5 Mbit as DSC at 6 bpp, the lowest rate, which
    // is what the link carries: DSC, because it arrives much sooner.
    let mut listener = Listener::start(&home, &["--link-mbps", "20"]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let sandbox = server.sandbox(&session);
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
    desk.activate(&session);

    // The window starts at the session's size: frames of 1280x720, as DSC at
    // the 6 bpp the link carries.
    let full = wait_for("a DSC frame", Duration::from_secs(15), || {
        viewer
            .counters()
            .filter(|counters| counters["stream_mode"] == "dsc")
    });
    assert_eq!(
        (full["width"].as_u64(), full["height"].as_u64()),
        (Some(1280), Some(720))
    );
    // The screen is still, so the same frame follows as exact pixels.
    let still = viewer.still_and_exact();
    assert!(
        still["settles"].as_u64().is_some_and(|settles| settles > 0),
        "{still}"
    );
    assert_eq!(still["stream_mode"], "dsc", "{still}");
    assert_eq!(
        still["frame_bytes"],
        header_bytes() + 1280 * 720 * 3,
        "{still}"
    );
    assert_eq!(still["mode_switches"], 0, "{still}");

    // A window of 320x180 gets frames of 320x180, raw: 3 bytes per pixel.
    desk.resize((1280, 720), (320, 180));
    let small = viewer.sized(320, 180);
    assert_eq!(small["mode"], "raw", "{small}");
    assert_eq!(small["stream_mode"], "raw", "{small}");
    assert_eq!(
        small["frame_bytes"],
        header_bytes() + 320 * 180 * 3,
        "{small}"
    );
    assert!(
        small["mode_switches"]
            .as_u64()
            .is_some_and(|switches| switches >= 1),
        "{small}"
    );

    // An unchanged screen sends nothing. (The agent's input also waits out the
    // quiet second after the Meta key the resize sent through the viewer.)
    let idle = viewer.idle()["frames"].as_u64().expect("frames");
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
        desk.tool("keyboard_key", json!({"key":key}));
    }
    wait_for(
        "the file made from the viewer",
        Duration::from_secs(10),
        || typed.exists().then_some(()),
    );

    // A larger window gets larger frames, DSC again with its parameter set,
    // never below the codec's 6 bpp, and then the exact frame once more.
    let settles = viewer.count("settles");
    desk.resize((320, 180), (1280, 720));
    viewer.sized(1280, 720);
    let large = wait_for("DSC after the resize", Duration::from_secs(15), || {
        viewer
            .counters()
            .filter(|counters| counters["stream_mode"] == "dsc")
    });
    assert!(
        large["parameter_sets"]
            .as_u64()
            .is_some_and(|sets| sets >= 2),
        "{large}"
    );
    let large = viewer.still_and_exact();
    assert!(
        large["settles"].as_u64().is_some_and(|now| now > settles),
        "{large}"
    );
    assert_eq!(large["stream_mode"], "dsc", "{large}");
    let log = viewer.log_text();
    assert!(stream_errors(&log).is_empty(), "{log}");
    let sender_log = std::fs::read_to_string(workdir.join("viewer.log")).expect("sender log");
    assert!(
        sender_log.contains("sending dsc at 6.00 bpp 1280x720"),
        "{sender_log}"
    );
    assert!(stream_errors(&sender_log).is_empty(), "{sender_log}");

    // A second viewer_open keeps the window and only changes its title.
    let again = server.tool(
        "viewer_open",
        json!({"session_id":session, "reason":"still here"}),
    );
    assert_eq!(
        Viewer::opened(&again, "already_open", &host).pid,
        viewer.pid
    );

    // The frame socket ends (the viewer is killed): the capture stops, the
    // session does not.
    signal(viewer.pid, "TERM");
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

    // Leave nothing behind: the listener finishes removing the session, the
    // desk session stops in order, and the killed server's viewer directory
    // (which a later server's sweep would remove) goes with the test's HOME.
    wait_for(
        "the listener to remove the session's files",
        Duration::from_secs(30),
        || (!workdir.exists()).then_some(()),
    );
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    if let Some(directory) = viewer.counters.parent() {
        let _ = std::fs::remove_dir_all(directory);
    }
    std::fs::remove_dir_all(home).expect("remove the test HOME");
    let _ = TEST_NEEDS;
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Gwenview, Vulkan, and input devices"]
fn raw_frames_show_the_sessions_exact_pixels_and_a_still_dsc_frame_becomes_exact() {
    let home = home_with_secret("exact", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1600, 1000);
    let picture = home.join("colours.png");
    write_test_picture(&picture, 1280, 720);
    let dump = home.join("presented.png");
    // The pointer is drawn into the stream but not into a screenshot taken
    // with include_cursor=false; it is parked in this corner and the corner
    // is left out of the comparison.
    const POINTER_CORNER: (u32, u32, u32, u32) = (1280 - 64, 720 - 64, 1280, 720);

    // First over an unlimited link, where every frame is raw; then over a
    // 20 Mb/s link, where 1280x720 goes out as DSC and only the frame sent
    // after the screen stood still is exact.
    for (link, lossy) in [(None, false), (Some("20"), true)] {
        let options: Vec<&str> = link.map_or(Vec::new(), |mbps| vec!["--link-mbps", mbps]);
        let listener = Listener::start(&home, &options);
        let host = listener.address.clone();
        let _ = std::fs::remove_file(&dump);
        let mut server = Server::start(&home, "server", Some(&desk.display()), Some(&dump));
        let (session, _workdir) =
            server.start_session(json!({"host":host, "width":1280, "height":720}));
        server.tool(
            "launch_app",
            json!({
                "session_id":session,
                "command":format!("gwenview {}", picture.display()),
                "window_timeout_seconds":60,
            }),
        );
        server.tool(
            "mouse_move",
            json!({"session_id":session, "x":1279, "y":719}),
        );
        let opened = server.tool("viewer_open", json!({"session_id":session}));
        let viewer = Viewer::opened(&opened, "opened", &host);
        desk.activate(&session);

        // The viewer's window has the session's size, so nothing is scaled.
        let counters = viewer.sized(1280, 720);
        if lossy {
            wait_for("a settled DSC frame", Duration::from_secs(30), || {
                viewer.counters().filter(|counters| {
                    counters["stream_mode"] == "dsc"
                        && counters["settles"]
                            .as_u64()
                            .is_some_and(|settles| settles > 0)
                })
            });
        } else {
            assert_eq!(counters["stream_mode"], "raw", "{counters}");
        }
        // Wait until the screen is still and the viewer has written the
        // frame it presented last; then take the session's own screenshot.
        let (still, shot) = wait_for("a still screen", Duration::from_secs(60), || {
            let before = viewer.idle();
            let shot = server.tool(
                "screenshot",
                json!({"session_id":session, "include_cursor":false}),
            );
            std::thread::sleep(Duration::from_millis(2500));
            let after = viewer.idle();
            (before["frames"] == after["frames"] && after["mode"] == "raw" && dump.exists())
                .then_some((after, shot))
        });
        assert_eq!(still["mode"], "raw", "{still}");
        assert!(
            [
                "B8G8R8A8_UNORM SRGB_NONLINEAR",
                "R8G8B8A8_UNORM SRGB_NONLINEAR"
            ]
            .iter()
            .any(|format| still["swapchain"] == *format),
            "the swapchain is not an 8-bit UNORM sRGB surface: {still}"
        );
        if lossy {
            assert_eq!(still["stream_mode"], "dsc", "{still}");
        } else {
            assert_eq!(still["settles"], 0, "{still}");
            assert_eq!(still["mode_switches"], 0, "{still}");
        }
        let shot_path = PathBuf::from(shot["path"].as_str().expect("screenshot path"));
        let (width, height, session_pixels) = read_png(&shot_path);
        assert_eq!((width, height), (1280, 720), "{shot}");
        // The session shows the picture: all 256 values of a channel occur.
        let reds: std::collections::BTreeSet<u8> = session_pixels
            .chunks_exact(3)
            .map(|pixel| pixel[0])
            .collect();
        assert!(
            reds.len() > 200,
            "the session shows only {} red values",
            reds.len()
        );

        // What the viewer received and uploaded for presentation.
        let (width, height, received) = read_png(&dump);
        assert_eq!((width, height), (1280, 720));
        let (differing, largest) = difference(&session_pixels, &received, 1280, POINTER_CORNER);
        assert_eq!(
            (differing, largest),
            (0, 0),
            "link {link:?}: {differing} pixels of the frame the viewer received differ from the session's screenshot, by up to {largest}"
        );

        // What the viewer's window shows on the desktop it opened in: the
        // same pixels after the swapchain and that desktop's compositor.
        let shown = desk.tool("screenshot", json!({"include_cursor":false}));
        let shown_path = PathBuf::from(shown["path"].as_str().expect("screenshot path"));
        // KWIN_MCP_STREAM_EVIDENCE names a directory that receives the three
        // pictures of each link.
        if let Some(evidence) = std::env::var_os("KWIN_MCP_STREAM_EVIDENCE").map(PathBuf::from) {
            let name = link.map_or("unlimited".to_owned(), |mbps| format!("{mbps}mbps"));
            for (what, path) in [
                ("session-screenshot", &shot_path),
                ("viewer-received", &dump),
                ("viewer-window", &shown_path),
            ] {
                std::fs::copy(path, evidence.join(format!("{name}-{what}.png")))
                    .expect("copy evidence");
            }
        }
        let (width, height, presented) = read_png(&shown_path);
        assert_eq!((width, height), (1280, 720), "{shown}");
        let (differing, largest) = difference(&session_pixels, &presented, 1280, POINTER_CORNER);
        assert_eq!(
            (differing, largest),
            (0, 0),
            "link {link:?}: {differing} pixels of the viewer's window differ from the session's screenshot, by up to {largest}"
        );
        assert!(
            stream_errors(&viewer.log_text()).is_empty(),
            "{}",
            viewer.log_text()
        );

        server.tool("session_stop", json!({"session_id":session}));
        stopped("the window to close with its session", viewer.pid);
        drop(server);
        drop(listener);
    }
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn every_way_a_stream_ends_leaves_nothing_running() {
    let home = home_with_secret("ends", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1600, 1000);
    let mut listener = Listener::start(&home, &[]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let sandbox = server.sandbox(&session);
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":scrolling_text(3600, 1), "window_timeout_seconds":40}),
    );
    let open = |server: &mut Server, desk: &mut Desk| {
        let opened = server.tool("viewer_open", json!({"session_id":session}));
        let viewer = Viewer::opened(&opened, "opened", &host);
        let sender = sender_pid(&workdir);
        wait_for("frames", Duration::from_secs(10), || {
            (viewer.frames() > 0).then_some(())
        });
        desk.activate(&session);
        (viewer, sender)
    };

    // The frame sender dies: the window closes, and viewer_open opens a new one.
    let (viewer, sender) = open(&mut server, &mut desk);
    signal(sender, "KILL");
    stopped("the window to close when its sender is killed", viewer.pid);
    assert!(alive(sandbox), "the session ended with its sender");

    // The viewer is killed outright: the sender stops.
    let (viewer, sender) = open(&mut server, &mut desk);
    signal(viewer.pid, "KILL");
    stopped("the sender to stop when its viewer is killed", sender);
    assert!(alive(sandbox), "the session ended with its viewer");

    // The user closes the window.
    let (viewer, sender) = open(&mut server, &mut desk);
    desk.tool("keyboard_key", json!({"key":"alt+F4"}));
    stopped("the viewer to exit when its window is closed", viewer.pid);
    stopped("the sender to stop when the window is closed", sender);
    assert!(alive(sandbox), "the session ended with its window");

    // A viewer that stops taking frames holds the sender back without
    // growing it, and the stream carries on when the viewer does.
    let (viewer, sender) = open(&mut server, &mut desk);
    std::thread::sleep(Duration::from_secs(3));
    let before = resident_kib(sender).expect("sender memory");
    signal(viewer.pid, "STOP");
    std::thread::sleep(Duration::from_secs(6));
    let stalled = resident_kib(sender).expect("sender memory");
    let frames = viewer.frames();
    signal(viewer.pid, "CONT");
    assert!(
        stalled < before + 32 * 1024,
        "the sender grew from {before} KiB to {stalled} KiB behind a stalled viewer"
    );
    wait_for("frames after the stall", Duration::from_secs(15), || {
        (viewer.frames() > frames + 10).then_some(())
    });
    assert!(alive(sender), "the sender ended during a 6 second stall");

    // viewer_close.
    let closed = server.tool("viewer_close", json!({"session_id":session}));
    assert_eq!(closed["status"], "closed", "{closed}");
    stopped("the viewer to exit after viewer_close", viewer.pid);
    stopped("the sender to stop after viewer_close", sender);
    assert!(
        stream_errors(&viewer.log_text()).is_empty(),
        "{}",
        viewer.log_text()
    );

    // session_stop with the viewer open.
    let (viewer, sender) = open(&mut server, &mut desk);
    server.tool("session_stop", json!({"session_id":session}));
    stopped("the viewer to exit with its session", viewer.pid);
    stopped("the sender to stop with its session", sender);
    wait_for("the session to end", Duration::from_secs(30), || {
        (!alive(sandbox)).then_some(())
    });

    // The listener dies, which drops the control socket and the frame socket
    // from the session's side: the window closes.
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let sandbox = server.sandbox(&session);
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    let opened = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&opened, "opened", &host);
    let sender = sender_pid(&workdir);
    listener.child.kill().expect("kill the listener");
    listener.child.wait().expect("reap the listener");
    stopped("the sender to end with its listener", sender);
    stopped("the window to close when the listener is gone", viewer.pid);
    wait_for(
        "the session to end with its listener",
        Duration::from_secs(30),
        || (!alive(sandbox)).then_some(()),
    );

    wait_for(
        "every viewer and sender to be gone",
        Duration::from_secs(10),
        || viewers_under(&home).is_empty().then_some(()),
    );
    desk.stop();
    drop(server);
    drop(listener);
    // The killed listener could not remove its session's files; the next
    // server that starts for this user does.
    let sweeper = Server::start(&home, "sweeper", None, None);
    wait_for(
        "the dead listener's session files to be swept",
        Duration::from_secs(30),
        || {
            let twins = std::fs::read_dir(home.join(".cache/kwin-mcp"))
                .map_or(0, |entries| entries.count());
            (!workdir.exists() && twins == 0).then_some(())
        },
    );
    drop(sweeper);
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

/// `NAME=a,b,c` from the environment as numbers, or `default`.
fn numbers_from(name: &str, default: &[u64]) -> Vec<u64> {
    std::env::var(name).map_or_else(
        |_| default.to_vec(),
        |text| {
            text.split(',')
                .map(|value| {
                    value
                        .trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("{name}: '{value}'"))
                })
                .collect()
        },
    )
}

#[test]
#[ignore = "measurement; requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn measure_the_stream_under_scrolling_text() {
    // KWIN_MCP_STREAM_LINKS: link speeds in Mb/s, 0 for the unlimited
    // loopback. KWIN_MCP_STREAM_HEIGHTS: session heights (16:9).
    let links = numbers_from("KWIN_MCP_STREAM_LINKS", &[1000, 500, 200, 100]);
    let heights = numbers_from("KWIN_MCP_STREAM_HEIGHTS", &[1080, 720]);
    const SCROLL_SECONDS: u32 = 16;
    const SAMPLE_SECONDS: usize = 10;
    let home = home_with_secret("measure", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 2000, 1200);
    let mut rows = Vec::new();
    for height in heights {
        let width = height * 16 / 9;
        for link in &links {
            let mbps = link.to_string();
            let options: Vec<&str> = if *link == 0 {
                Vec::new()
            } else {
                vec!["--link-mbps", &mbps]
            };
            let listener = Listener::start(&home, &options);
            let host = listener.address.clone();
            let mut server = Server::start(&home, "server", Some(&desk.display()), None);
            let (session, workdir) =
                server.start_session(json!({"host":host, "width":width, "height":height}));
            server.tool(
                "launch_app",
                json!({
                    "session_id":session,
                    "command":scrolling_text(SCROLL_SECONDS, 3600),
                    "window_timeout_seconds":40,
                }),
            );
            let opened = server.tool("viewer_open", json!({"session_id":session}));
            let viewer = Viewer::opened(&opened, "opened", &host);
            desk.activate(&session);
            viewer.sized(width, height);
            // One sample for each second the viewer reports while text scrolls.
            std::thread::sleep(Duration::from_secs(3));
            let sender = sender_pid(&workdir);
            let started = (Instant::now(), cpu_seconds(sender), cpu_seconds(viewer.pid));
            let mut samples: Vec<Value> = Vec::new();
            let mut last_frames = 0;
            while samples.len() < SAMPLE_SECONDS {
                let counters =
                    wait_for("the next second's counters", Duration::from_secs(5), || {
                        viewer
                            .counters()
                            .filter(|counters| counters["frames"] != last_frames)
                    });
                last_frames = counters["frames"].as_u64().expect("frames");
                samples.push(counters);
            }
            // Share of one core each process used while the text scrolled.
            let elapsed = started.0.elapsed().as_secs_f64();
            let sender_cpu = (cpu_seconds(sender) - started.1) / elapsed * 100.0;
            let viewer_cpu = (cpu_seconds(viewer.pid) - started.2) / elapsed * 100.0;
            let mean = |field: &str| {
                samples
                    .iter()
                    .filter_map(|sample| sample["last_second"][field].as_f64())
                    .sum::<f64>()
                    / f64::from(u32::try_from(samples.len()).expect("sample count"))
            };
            let largest = |field: &str| {
                samples
                    .iter()
                    .filter_map(|sample| sample["last_second"][field].as_f64())
                    .fold(0.0, f64::max)
            };
            let moving = samples.last().expect("samples").clone();
            let sender_log =
                std::fs::read_to_string(workdir.join("viewer.log")).unwrap_or_default();
            let sender_line = sender_log
                .lines()
                .rev()
                .filter(|line| line.contains("stream: sent ") && !line.contains("sent 0."))
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            eprintln!("sender: {sender_line}");
            // The text stops: the screen is still. (A build without the
            // settle counters only goes idle.)
            let settles = moving["settles"].as_u64();
            let still = wait_for("a still screen", Duration::from_secs(40), || {
                viewer.counters().filter(|counters| {
                    counters["last_second"]["frames"] == 0
                        && (counters["stream_mode"] != "dsc"
                            || counters["settles"].as_u64() > settles)
                })
            });
            rows.push(format!(
                "| {width}x{height} | {} | {} {} bpp | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {sender_cpu:.0}% | {viewer_cpu:.0}% | {} | {} | {} |",
                if *link == 0 { "unlimited".to_owned() } else { link.to_string() },
                moving["stream_mode"].as_str().or(moving["mode"].as_str()).unwrap_or("?"),
                moving["bits_per_pixel"],
                moving["frame_bytes"],
                mean("frames_per_second"),
                mean("megabits_per_second"),
                mean("latency_ms"),
                largest("latency_max_ms"),
                mean("sender_ms"),
                mean("decode_ms"),
                moving["mode_switches"],
                still["settles"].as_u64().unwrap_or(0) - settles.unwrap_or(0),
                still["mode"].as_str().unwrap_or("?"),
            ));
            eprintln!("{}", rows.last().expect("row"));
            assert!(
                stream_errors(&viewer.log_text()).is_empty(),
                "{}",
                viewer.log_text()
            );
            server.tool("session_stop", json!({"session_id":session}));
            stopped("the window to close with its session", viewer.pid);
            drop(server);
            drop(listener);
        }
    }
    eprintln!(
        "| size | link Mb/s | mode | bytes/frame | frames/s | Mb/s | capture to present ms | max ms | capture to send ms | decode ms | sender CPU | viewer CPU | mode switches | settles after the text stopped | still frame |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for row in &rows {
        eprintln!("{row}");
    }
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

/// One of the soak test's two streams.
struct Soaked {
    session: String,
    workdir: PathBuf,
    viewer: Viewer,
    sender: u64,
    /// The window's size now.
    size: (u32, u32),
    resizes: u64,
    /// Frames the viewer had received at the last look.
    frames: u64,
    /// Resident memory at each look, in KiB.
    sender_memory: Vec<u64>,
    viewer_memory: Vec<u64>,
}

/// The largest value of each half of `series`, its first `skip` values aside.
fn peaks(series: &[u64], skip: usize) -> (u64, u64) {
    let settled = series.get(skip..).unwrap_or(&[]);
    let (early, late) = settled.split_at(settled.len() / 2);
    let peak = |part: &[u64]| part.iter().copied().max().unwrap_or(0);
    (peak(early), peak(late))
}

#[test]
#[ignore = "soak: four minutes; requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn two_viewers_stay_stable_through_minutes_of_changes_resizes_and_mode_switches() {
    // KWIN_MCP_SOAK_SECONDS: how long the streams run.
    let duration = Duration::from_secs(numbers_from("KWIN_MCP_SOAK_SECONDS", &[240])[0]);
    const LOOK_EVERY: Duration = Duration::from_secs(5);
    const LARGE: (u32, u32) = (1280, 720);
    const SMALL: (u32, u32) = (480, 270);
    // Looks left out of the memory comparison: both sizes and both modes
    // have been in use by then.
    const WARM_UP_LOOKS: usize = 10;
    const GROWTH_ALLOWED_KIB: u64 = 48 * 1024;
    let home = home_with_secret("soak", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 2700, 1500);

    // A 40 Mb/s link carries 36 Mb/s. A 1280x720 frame would take 614 ms raw
    // and goes out as DSC at 6 bpp (154 ms); a 480x270 frame takes 86 ms raw
    // and goes out exactly. Every resize between the two changes the mode.
    let listener = Listener::start(&home, &["--link-mbps", "40"]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let mut streams: Vec<Soaked> = (0..2)
        .map(|_| {
            let (session, workdir) =
                server.start_session(json!({"host":host, "width":LARGE.0, "height":LARGE.1}));
            // Text scrolls for four seconds and stands still for one, so
            // still DSC frames settle to exact ones all the time.
            server.tool(
                "launch_app",
                json!({"session_id":session, "command":scrolling_text(4, 1), "window_timeout_seconds":40}),
            );
            let opened = server.tool("viewer_open", json!({"session_id":session}));
            let viewer = Viewer::opened(&opened, "opened", &host);
            let sender = sender_pid(&workdir);
            Soaked {
                session,
                workdir,
                viewer,
                sender,
                size: LARGE,
                resizes: 0,
                frames: 0,
                sender_memory: Vec::new(),
                viewer_memory: Vec::new(),
            }
        })
        .collect();
    let sandboxes: Vec<u64> = streams
        .iter()
        .map(|stream| server.sandbox(&stream.session))
        .collect();

    let started = Instant::now();
    let mut look = 0usize;
    let mut stalled = false;
    while started.elapsed() < duration {
        std::thread::sleep(LOOK_EVERY);
        look += 1;
        // Every stream delivered frames since the last look.
        for (index, stream) in streams.iter_mut().enumerate() {
            let frames = stream.viewer.frames();
            assert!(
                frames > stream.frames,
                "stream {index} delivered no frame in {LOOK_EVERY:?} after {:?}",
                started.elapsed()
            );
            stream.frames = frames;
            assert!(
                alive(stream.sender) && alive(stream.viewer.pid),
                "stream {index} ended"
            );
            stream
                .sender_memory
                .push(resident_kib(stream.sender).expect("sender memory"));
            stream
                .viewer_memory
                .push(resident_kib(stream.viewer.pid).expect("viewer memory"));
        }
        // Every third look one window changes size, the two in turn.
        if look.is_multiple_of(3) {
            let stream = &mut streams[(look / 3) % 2];
            let target = if stream.size == LARGE { SMALL } else { LARGE };
            desk.activate(&stream.session);
            desk.resize(stream.size, target);
            stream
                .viewer
                .sized(u64::from(target.0), u64::from(target.1));
            stream.size = target;
            stream.resizes += 1;
        }
        // Once, a third of the way in: the first viewer stops taking frames
        // for a while. Its sender waits without growing, the other stream
        // carries on, and the stalled one resumes.
        if !stalled && started.elapsed() > duration / 3 {
            stalled = true;
            let before = resident_kib(streams[0].sender).expect("sender memory");
            let other = streams[1].viewer.frames();
            signal(streams[0].viewer.pid, "STOP");
            std::thread::sleep(Duration::from_secs(6));
            let during = resident_kib(streams[0].sender).expect("sender memory");
            let other_now = streams[1].viewer.frames();
            signal(streams[0].viewer.pid, "CONT");
            assert!(
                during < before + 16 * 1024,
                "the sender grew from {before} KiB to {during} KiB behind a stalled viewer"
            );
            assert!(
                other_now > other,
                "the other stream stopped during the stall"
            );
            streams[1].frames = other_now;
        }
    }

    for (index, stream) in streams.iter().enumerate() {
        let counters = stream.viewer.counters().expect("counters");
        eprintln!(
            "stream {index}: {} frames, {} resizes, {} mode switches, {} settles; sender KiB {:?}; viewer KiB {:?}",
            counters["frames"],
            stream.resizes,
            counters["mode_switches"],
            counters["settles"],
            stream.sender_memory,
            stream.viewer_memory,
        );
        // No frame-socket or picture error on either side.
        let viewer_log = stream.viewer.log_text();
        assert!(
            stream_errors(&viewer_log).is_empty(),
            "stream {index}: {viewer_log}"
        );
        let sender_log =
            std::fs::read_to_string(stream.workdir.join("viewer.log")).expect("sender log");
        assert!(
            stream_errors(&sender_log).is_empty(),
            "stream {index}: {sender_log}"
        );
        // Each resize changed the mode, and still DSC frames were settled.
        assert!(
            stream.resizes >= 2,
            "stream {index} was resized {} times",
            stream.resizes
        );
        assert!(
            counters["mode_switches"]
                .as_u64()
                .is_some_and(|switches| switches >= stream.resizes),
            "stream {index}: {counters}"
        );
        assert!(
            counters["settles"]
                .as_u64()
                .is_some_and(|settles| settles > 0),
            "stream {index}: {counters}"
        );
        // Memory does not keep growing: the second half's peak stays near
        // the first half's.
        for (what, series) in [
            ("sender", &stream.sender_memory),
            ("viewer", &stream.viewer_memory),
        ] {
            let (early, late) = peaks(series, WARM_UP_LOOKS);
            assert!(
                late <= early + GROWTH_ALLOWED_KIB,
                "stream {index}: the {what} grew from a peak of {early} KiB to {late} KiB: {series:?}"
            );
        }
    }

    // Clean teardown. The first session stops: its viewer and sender go, the
    // second stream stays.
    server.tool("session_stop", json!({"session_id":streams[0].session}));
    stopped(
        "the first viewer to exit with its session",
        streams[0].viewer.pid,
    );
    stopped(
        "the first sender to stop with its session",
        streams[0].sender,
    );
    assert!(alive(streams[1].sender) && alive(streams[1].viewer.pid));
    // The calling server dies, which drops the control socket: the listener
    // stops the second capture and session, and the window closes.
    server.child.kill().expect("kill the calling server");
    server.child.wait().expect("reap the calling server");
    stopped(
        "the second sender to stop with the control socket",
        streams[1].sender,
    );
    stopped(
        "the second viewer to close with its server",
        streams[1].viewer.pid,
    );
    wait_for("the sessions to end", Duration::from_secs(30), || {
        sandboxes
            .iter()
            .all(|sandbox| !alive(*sandbox))
            .then_some(())
    });
    wait_for(
        "the listener to remove the sessions' files",
        Duration::from_secs(30),
        || {
            streams
                .iter()
                .all(|stream| !stream.workdir.exists())
                .then_some(())
        },
    );
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    for stream in &streams {
        if let Some(directory) = stream.viewer.counters.parent() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}
