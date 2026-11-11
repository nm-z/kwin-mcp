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

fn server_binary() -> PathBuf {
    std::env::var_os("KWIN_MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")))
}

fn kwin_mcp(home: &Path) -> Command {
    kwin_mcp_at(&server_binary(), home)
}

fn kwin_mcp_at(binary: &Path, home: &Path) -> Command {
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
        Self::start_command(kwin_mcp(home), home, options)
    }

    /// A listener that runs `command`, a kwin-mcp of any build.
    fn start_command(mut command: Command, home: &Path, options: &[&str]) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|socket| socket.local_addr())
            .expect("reserve a port")
            .port();
        let address = format!("127.0.0.1:{port}");
        let mut child = command
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
        Self::start_command(kwin_mcp(home), home, label, display, dump)
    }

    /// A stdio server that runs a private copy of the binary and re-executes
    /// itself when that copy changes. Returns the copy's path too.
    fn start_copy(home: &Path, label: &str, display: Option<&Path>) -> (Self, PathBuf) {
        Self::start_copy_with(home, label, display, &[])
    }

    /// The same, with `environment` set for the server and its viewers.
    fn start_copy_with(
        home: &Path,
        label: &str,
        display: Option<&Path>,
        environment: &[(&str, &Path)],
    ) -> (Self, PathBuf) {
        let source = server_binary();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).expect("create the private bin directory");
        let program = bin.join("kwin-mcp");
        std::fs::copy(&source, &program).expect("copy kwin-mcp");
        std::fs::copy(
            source.with_file_name("kwin-viewer"),
            bin.join("kwin-viewer"),
        )
        .expect("copy kwin-viewer");
        let mut command = kwin_mcp_at(&program, home);
        command.env("KWIN_MCP_BINARY", &program);
        command.envs(environment.iter().copied());
        let server = Self::start_command(command, home, label, display, None);
        (server, program)
    }

    fn start_command(
        mut command: Command,
        home: &Path,
        label: &str,
        display: Option<&Path>,
        dump: Option<&Path>,
    ) -> Self {
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

/// The text file the sessions scroll: `KWIN_MCP_TEST_SCROLL_TEXT`, or this
/// repository's README where the test was built (a test built on another
/// machine names a file of this one).
fn text_to_scroll() -> String {
    std::env::var("KWIN_MCP_TEST_SCROLL_TEXT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/README.md").to_owned())
}

/// A command for `launch_app` that scrolls text in a maximized Konsole for
/// `scroll` seconds, pauses for `pause` seconds, and starts again.
fn scrolling_text(scroll: u32, pause: u32) -> String {
    let text = text_to_scroll();
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
    let presented_idle = viewer.count("presented");

    // The agent's typing changes the screen, so frames follow.
    server.tool(
        "keyboard_type",
        json!({"session_id":session, "text":"echo frames"}),
    );
    wait_for("frames after typing", Duration::from_secs(10), || {
        (viewer.frames() > idle).then_some(())
    });
    // And the window presents them.
    wait_for(
        "the frames after typing to be presented",
        Duration::from_secs(10),
        || (viewer.count("presented") > presented_idle).then_some(()),
    );
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
        wait_for("frames, presented", Duration::from_secs(10), || {
            (viewer.frames() > 0 && viewer.count("presented") > 0).then_some(())
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

/// The process that would reap `pid`.
fn parent(pid: u64) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.split(' ').nth(1)?.parse().ok()
}

/// How many lines of the log at `path` contain `text`.
fn logged(path: &Path, text: &str) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(text))
        .count()
}

/// The sockets among a process's descriptors that a program it executes
/// would inherit, as `fd -> socket:[inode]`.
fn inheritable_sockets(pid: u64) -> Vec<String> {
    const CLOSE_ON_EXEC: u32 = 0o2000000;
    let mut open = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("read the descriptor table")
        .flatten()
    {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.display().to_string();
        let number = entry.file_name().to_string_lossy().into_owned();
        let flags = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{number}"))
            .ok()
            .and_then(|info| {
                let flags = info.lines().find_map(|line| line.strip_prefix("flags:"))?;
                u32::from_str_radix(flags.trim(), 8).ok()
            });
        if target.starts_with("socket:") && flags.is_some_and(|flags| flags & CLOSE_ON_EXEC == 0) {
            open.push(format!("{number} -> {target}"));
        }
    }
    open
}

/// Copies the PNG a `screenshot` result names into the directory
/// `KWIN_MCP_TEST_EVIDENCE` names, when it is set.
fn keep_evidence(screenshot: &Value, name: &str) {
    let Some(directory) = std::env::var_os("KWIN_MCP_TEST_EVIDENCE").map(PathBuf::from) else {
        return;
    };
    let path = screenshot["path"].as_str().expect("screenshot path");
    std::fs::create_dir_all(&directory).expect("create the evidence directory");
    std::fs::copy(path, directory.join(name)).expect("keep a screenshot");
}

/// Gives the server's binary a new modification time, which a running server
/// takes for a new build, and waits until the server has re-executed itself
/// and continued on its connection to `host`. `reloads` counts them.
fn reexecute(program: &Path, server_log: &Path, host: &str, reloads: usize) {
    // A running executable cannot be opened for writing; its owner can still
    // set its times through a read-only handle.
    std::fs::File::open(program)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .expect("touch the server binary");
    let kept = format!("reload: kept the connection to {host} and its 1 sessions");
    wait_for(
        "the server to re-execute and keep its connection",
        Duration::from_secs(60),
        || {
            let lost = logged(server_log, "reload: lost the connection");
            assert_eq!(lost, 0, "the re-executed server lost its connection");
            (logged(server_log, &kept) == reloads).then_some(())
        },
    );
}

/// A server that re-executes itself (every install does this to all running
/// servers) keeps the sessions it started on other machines, and their open
/// viewer windows: the control socket's TCP connection goes to the new
/// process image, so the listener sees no disconnect. A server that dies
/// still loses them at once.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn a_remote_session_and_its_viewer_survive_the_callers_reexecution() {
    let home = home_with_secret("reload", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1600, 1000);
    let mut listener = Listener::start(&home, &[]);
    let host = listener.address.clone();
    let port = host.rsplit(':').next().expect("listener port");
    let listener_log = home.join(format!("listener-{port}.log"));
    let server_log = home.join("server.log");
    let (mut server, program) = Server::start_copy(&home, "server", Some(&desk.display()));
    let server_pid = u64::from(server.child.id());

    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let sandbox = server.sandbox(&session);
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    let opened = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&opened, "opened", &host);
    let sender = sender_pid(&workdir);
    wait_for("frames, presented", Duration::from_secs(10), || {
        (viewer.frames() > 0 && viewer.count("presented") > 0).then_some(())
    });
    desk.activate(&session);
    let windows_before = server.tool("window_list", json!({"session_id":session}));
    let konsole = record(&windows_before, "windows", |window| {
        window["title"]
            .as_str()
            .is_some_and(|title| title.contains("Konsole"))
    })
    .unwrap_or_else(|| panic!("no Konsole window: {windows_before}"));

    // Twice: the second re-execution hands over a connection the first one
    // inherited.
    for reloads in 1..=2 {
        reexecute(&program, &server_log, &host, reloads);
        assert_eq!(
            logged(&listener_log, "disconnected"),
            0,
            "the listener saw its peer disconnect"
        );
        assert_eq!(u64::from(server.child.id()), server_pid);
        assert!(alive(sandbox), "the session ended with the re-execution");
        assert!(
            alive(sender),
            "the frame sender ended with the re-execution"
        );
        assert!(alive(viewer.pid), "the viewer closed with the re-execution");
        assert_eq!(
            parent(viewer.pid),
            Some(server_pid),
            "the viewer is no longer the server's child"
        );

        // The same session ID still reaches the session, with its host.
        let listed = server.tool("session_list", json!({}));
        let entry = record(&listed, "sessions", |listed| {
            listed["session_id"] == session
        })
        .unwrap_or_else(|| panic!("the session is not listed after the reload: {listed}"));
        assert_eq!(entry["host"], host, "{entry}");
        assert_eq!(entry["sandbox_pid"], sandbox, "{entry}");
        let windows = server.tool("window_list", json!({"session_id":session}));
        assert!(
            record(&windows, "windows", |window| window["id"] == konsole["id"]).is_some(),
            "{windows}"
        );
        let shot = server.tool("screenshot", json!({"session_id":session}));
        assert_eq!(
            (shot["width"].as_u64(), shot["height"].as_u64()),
            (Some(1280), Some(720)),
            "{shot}"
        );

        // The window still receives and presents what changes in the session.
        let (frames, presented) = (viewer.idle()["frames"].as_u64(), viewer.count("presented"));
        let frames = frames.expect("frames");
        server.tool(
            "keyboard_type",
            json!({"session_id":session, "text":format!("echo reload {reloads}")}),
        );
        server.tool(
            "keyboard_key",
            json!({"session_id":session, "key":"Return"}),
        );
        wait_for(
            "frames after the re-execution",
            Duration::from_secs(10),
            || (viewer.frames() > frames && viewer.count("presented") > presented).then_some(()),
        );
        // And the server still knows it as this session's viewer.
        let again = server.tool("viewer_open", json!({"session_id":session}));
        assert_eq!(
            Viewer::opened(&again, "already_open", &host).pid,
            viewer.pid
        );
        // The inherited control socket is closed on exec again: a child that
        // held it would keep the session alive after the server died.
        assert_eq!(
            inheritable_sockets(server_pid),
            Vec::<String>::new(),
            "sockets the re-executed server would pass to its children"
        );
        viewer.idle();
        let window = desk.tool("screenshot", json!({}));
        keep_evidence(&window, &format!("reload-{reloads}-viewer-window.png"));
        let shot = server.tool("screenshot", json!({"session_id":session}));
        keep_evidence(&shot, &format!("reload-{reloads}-session.png"));
    }
    let log = viewer.log_text();
    assert!(stream_errors(&log).is_empty(), "{log}");

    // session_stop still stops the session, its capture and its window.
    let stopped_session = server.tool("session_stop", json!({"session_id":session}));
    assert_eq!(stopped_session["status"], "stopped", "{stopped_session}");
    stopped("the viewer to exit with its session", viewer.pid);
    stopped("the sender to stop with its session", sender);
    wait_for("the session to end", Duration::from_secs(30), || {
        (!alive(sandbox)).then_some(())
    });
    let listed = server.tool("session_list", json!({}));
    assert!(
        record(&listed, "sessions", |listed| listed["session_id"]
            == session)
        .is_none(),
        "{listed}"
    );
    assert_eq!(logged(&listener_log, "disconnected"), 0);

    // A server that dies announces nothing and hands nothing over: the
    // listener sees the connection end and stops the session at once, also
    // for a server that has re-executed itself before.
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let sandbox = server.sandbox(&session);
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
    );
    let opened = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&opened, "opened", &host);
    let sender = sender_pid(&workdir);
    server.child.kill().expect("kill the calling server");
    server.child.wait().expect("reap the calling server");
    wait_for(
        "the listener to notice its peer is gone",
        Duration::from_secs(5),
        || {
            (logged(
                &listener_log,
                "disconnected; stopping the sessions it started",
            ) == 1)
                .then_some(())
        },
    );
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

    wait_for(
        "the listener to remove the session's files",
        Duration::from_secs(30),
        || (!workdir.exists()).then_some(()),
    );
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    // The killed server's viewer directory, which a later server's sweep
    // would remove.
    if let Some(directory) = viewer.counters.parent() {
        let _ = std::fs::remove_dir_all(directory);
    }
    if let Some(directory) = std::env::var_os("KWIN_MCP_TEST_EVIDENCE").map(PathBuf::from) {
        for log in [&server_log, &listener_log] {
            let name = log.file_name().expect("log name");
            std::fs::copy(log, directory.join(name)).expect("keep a log");
        }
    }
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

/// A server only hands its control sockets to a binary that continues on
/// them. One that does not (an older build installed over this one) would
/// hold them open without knowing it and pass them to its children, and the
/// listener would keep sessions nobody can reach. Before such a re-execution
/// the connection closes, and the listener stops the session at once.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and input devices"]
fn a_reexecution_into_a_binary_without_the_handover_closes_the_connection() {
    let home = home_with_secret("downgrade", "shared-secret-0123456789");
    let listener = Listener::start(&home, &[]);
    let host = listener.address.clone();
    let port = host.rsplit(':').next().expect("listener port");
    let listener_log = home.join(format!("listener-{port}.log"));
    let server_log = home.join("server.log");
    let (mut server, program) = Server::start_copy(&home, "server", None);
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    let sandbox = server.sandbox(&session);

    // The same build without the text that marks the handover: to the running
    // server, a binary that would not continue on its sockets.
    let mark = kwin_mcp::remote::ADOPTS_CONTROL_MARK.as_bytes();
    let mut image = std::fs::read(&program).expect("read the server binary");
    let at = image
        .windows(mark.len())
        .position(|window| window == mark)
        .expect("the server binary carries the handover mark");
    image[at] ^= 0x20;
    let replacement = program.with_extension("new");
    std::fs::write(&replacement, image).expect("write the replacement binary");
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755))
        .expect("make the replacement executable");
    std::fs::rename(&replacement, &program).expect("install the replacement binary");

    wait_for("the server to re-execute", Duration::from_secs(60), || {
        (logged(&server_log, "reload: reattached 0 sessions") == 1).then_some(())
    });
    assert_eq!(
        logged(
            &server_log,
            "the new binary does not continue on inherited control sockets"
        ),
        1,
        "{}",
        std::fs::read_to_string(&server_log).unwrap_or_default()
    );
    assert_eq!(logged(&server_log, "reload: kept the connection"), 0);
    wait_for(
        "the listener to notice its peer is gone",
        Duration::from_secs(5),
        || {
            (logged(
                &listener_log,
                "disconnected; stopping the sessions it started",
            ) == 1)
                .then_some(())
        },
    );
    wait_for("the session to end", Duration::from_secs(30), || {
        (!alive(sandbox)).then_some(())
    });
    assert_eq!(
        inheritable_sockets(u64::from(server.child.id())),
        Vec::<String>::new()
    );

    // The server still knows the handle and says what became of it.
    let listed = server.tool("session_list", json!({}));
    let entry = record(&listed, "sessions", |listed| {
        listed["session_id"] == session
    })
    .unwrap_or_else(|| panic!("the lost session is not listed: {listed}"));
    assert_eq!(entry["status"], "connection_lost", "{entry}");
    assert_eq!(entry["host"], host, "{entry}");
    let cleared = server.tool("session_stop", json!({"session_id":session}));
    assert_eq!(cleared["status"], "already_cleared", "{cleared}");

    wait_for(
        "the listener to remove the session's files",
        Duration::from_secs(30),
        || (!workdir.exists()).then_some(()),
    );
    drop(server);
    drop(listener);
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
            assert!(
                mean("presented") * 2.0 >= mean("frames"),
                "the window presented {:.1} of {:.1} frames a second\n{}",
                mean("presented"),
                mean("frames"),
                viewer.log_text()
            );
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
    /// Frames the viewer had received and presented at the last look.
    frames: u64,
    presented: u64,
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

    // A 80 Mb/s link carries 72 Mb/s, which the two streams share: 36 Mb/s
    // each while both scroll, all of it for one while the other pauses. A
    // 1280x720 frame would take 307 ms raw even alone and goes out as DSC
    // (6 bpp, 154 ms, with half the link); a 480x270 frame takes 86 ms raw
    // even with half and goes out exactly. Every resize between the two
    // changes the mode.
    let listener = Listener::start(&home, &["--link-mbps", "80"]);
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
                presented: 0,
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
            // And its window presented some of them.
            let presented = stream.viewer.count("presented");
            assert!(
                presented > stream.presented,
                "stream {index} presented no frame in {LOOK_EVERY:?} after {:?}",
                started.elapsed()
            );
            stream.presented = presented;
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

/// A command for `launch_app` that scrolls text in Konsole for as long as the
/// file `gate` exists. A session's work directory is the same path inside
/// the session and outside, so the test starts and stops the scrolling.
fn gated_scrolling_text(gate: &Path) -> String {
    let text = text_to_scroll();
    format!(
        "konsole -e bash -c 'while :; do if [ -e {} ]; then cat {text}; else sleep 0.1; fi; done'",
        gate.display()
    )
}

/// One of several streams that leave a listener on one interface.
struct Shared {
    session: String,
    workdir: PathBuf,
    /// The session scrolls text while this file exists.
    gate: PathBuf,
    viewer: Viewer,
}

impl Shared {
    fn open(server: &mut Server, host: &str, size: (u32, u32)) -> Self {
        let (session, workdir) =
            server.start_session(json!({"host":host, "width":size.0, "height":size.1}));
        let gate = workdir.join("scroll");
        server.tool(
            "launch_app",
            json!({
                "session_id":session,
                "command":gated_scrolling_text(&gate),
                "window_timeout_seconds":40,
            }),
        );
        let opened = server.tool("viewer_open", json!({"session_id":session}));
        let viewer = Viewer::opened(&opened, "opened", host);
        viewer.sized(u64::from(size.0), u64::from(size.1));
        Self {
            session,
            workdir,
            gate,
            viewer,
        }
    }

    fn scroll(&self, on: bool) {
        if on {
            std::fs::write(&self.gate, "").expect("start the scrolling");
        } else {
            let _ = std::fs::remove_file(&self.gate);
        }
    }

    /// Waits until the viewer's counters say the stream has `share` Mb/s
    /// among `streams` streams and `accepted` holds, and returns them.
    fn sharing(&self, share: f64, streams: u64, accepted: impl Fn(&Value) -> bool) -> Value {
        wait_for(
            &format!("a share of {share} Mb/s among {streams} streams"),
            Duration::from_secs(30),
            || {
                self.viewer.counters().filter(|counters| {
                    counters["path"]["streams"] == streams
                        && counters["path"]["share_mbps"].as_f64() == Some(share)
                        && accepted(counters)
                })
            },
        )
    }

    fn sender_log(&self) -> String {
        std::fs::read_to_string(self.workdir.join("viewer.log")).unwrap_or_default()
    }
}

/// What each of `streams` received in Mb/s, as the mean of the seconds its
/// viewer reported during `seconds` seconds.
fn received_mbps(streams: &[&Shared], seconds: u64) -> Vec<f64> {
    let mut samples: Vec<Vec<(u64, f64)>> = streams.iter().map(|_| Vec::new()).collect();
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        for (stream, samples) in streams.iter().zip(&mut samples) {
            let Some(counters) = stream.viewer.counters() else {
                continue;
            };
            // One report a second; the total of frames tells them apart.
            let report = (
                counters["frames"].as_u64().unwrap_or(0),
                counters["last_second"]["megabits_per_second"]
                    .as_f64()
                    .unwrap_or(0.0),
            );
            if samples.last().is_none_or(|last| last.0 != report.0) {
                samples.push(report);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    samples
        .iter()
        .map(|samples| {
            // The first report may hold a second from before the sampling.
            let counted = samples.get(1..).unwrap_or(&[]);
            assert!(counted.len() >= 2, "too few reports: {samples:?}");
            counted.iter().map(|sample| sample.1).sum::<f64>()
                / f64::from(u32::try_from(counted.len()).expect("sample count"))
        })
        .collect()
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn viewers_on_one_interface_share_its_budget_and_an_idle_ones_share_is_taken_over() {
    // A 80 Mb/s link carries 72 Mb/s. A 640x360 frame is 5.5 Mbit raw:
    // 77 ms with the whole budget, so a stream alone sends exact pixels;
    // 154 ms with half of it, so two streams send DSC at 12 bpp (2.8 Mbit,
    // 77 ms); with a third of it the rate falls below 12 bpp.
    const SIZE: (u32, u32) = (640, 360);
    const BUDGET: f64 = 72.0;
    const SAMPLE_SECONDS: u64 = 8;
    let home = home_with_secret("share", "shared-secret-0123456789");
    let desk = Desk::start(&home, 2200, 900);
    let listener = Listener::start(&home, &["--link-mbps", "80"]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let scrolling = |counters: &Value, mode: &str| {
        counters["stream_mode"] == mode
            && counters["last_second"]["frames"]
                .as_u64()
                .is_some_and(|frames| frames >= 3)
    };
    let mut rows = Vec::new();
    let mut measured = |what: &str, streams: &[&Shared]| {
        let rates = received_mbps(streams, SAMPLE_SECONDS);
        let total: f64 = rates.iter().sum();
        let row = format!(
            "| {what} | {} | {total:.1} | {BUDGET} |",
            rates
                .iter()
                .map(|rate| format!("{rate:.1}"))
                .collect::<Vec<_>>()
                .join(" + ")
        );
        eprintln!("{row}");
        rows.push(row);
        (rates, total)
    };

    // One stream scrolls alone: the whole budget, exact pixels.
    let first = Shared::open(&mut server, &host, SIZE);
    // viewer_open names the path and the share.
    let opened = server.tool("viewer_open", json!({"session_id":first.session}));
    assert_eq!(opened["status"], "already_open", "{opened}");
    let path = &opened["viewer"]["path"];
    assert_eq!(path["interface"], "lo", "{opened}");
    assert_eq!(path["bound"], "route", "{opened}");
    assert_eq!(path["budget_mbps"].as_f64(), Some(BUDGET), "{opened}");
    assert_eq!(path["share_mbps"].as_f64(), Some(BUDGET), "{opened}");
    assert_eq!(path["streams"], 1, "{opened}");
    assert!(
        path["sender_link"]
            .as_str()
            .is_some_and(|link| link.contains("--link-mbps 80")),
        "{opened}"
    );
    first.scroll(true);
    let alone = first.sharing(BUDGET, 1, |counters| scrolling(counters, "raw"));
    assert_eq!(
        alone["frame_bytes"],
        header_bytes() + u64::from(SIZE.0 * SIZE.1) * 3,
        "{alone}"
    );
    let (_, total) = measured("one stream", &[&first]);
    assert!(
        (BUDGET * 0.7..=BUDGET * 1.03).contains(&total),
        "a stream alone received {total:.1} Mb/s of {BUDGET}"
    );

    // A second viewer scrolls too: half each, and both go to DSC, because
    // exact pixels would now take longer than a tenth of a second.
    let second = Shared::open(&mut server, &host, SIZE);
    second.scroll(true);
    for stream in [&first, &second] {
        // (The last frame of a second may be a settle frame, which is raw.)
        let halved = stream.sharing(BUDGET / 2.0, 2, |counters| {
            scrolling(counters, "dsc") && counters["mode"] == "dsc"
        });
        assert_eq!(halved["bits_per_pixel"].as_f64(), Some(12.0), "{halved}");
    }
    let (rates, total) = measured("two streams", &[&first, &second]);
    assert!(
        (BUDGET * 0.8..=BUDGET * 1.03).contains(&total),
        "two streams received {total:.1} Mb/s of {BUDGET}: {rates:?}"
    );
    for rate in &rates {
        assert!(
            (BUDGET * 0.35..=BUDGET * 0.65).contains(rate),
            "the two streams received {rates:?} Mb/s"
        );
    }

    // The second viewer stops taking frames while its session scrolls on (its
    // process is stopped, as a viewer behind a dead path would be silent):
    // a stream that sends nothing holds no part of the link, so the first
    // has the whole budget until the second takes frames again.
    signal(second.viewer.pid, "STOP");
    first.sharing(BUDGET, 1, |counters| scrolling(counters, "raw"));
    let (_, total) = measured("two streams, one viewer stalled", &[&first]);
    signal(second.viewer.pid, "CONT");
    assert!(
        (BUDGET * 0.7..=BUDGET * 1.03).contains(&total),
        "beside a stalled stream the other received {total:.1} Mb/s of {BUDGET}"
    );
    for stream in [&first, &second] {
        stream.sharing(BUDGET / 2.0, 2, |counters| scrolling(counters, "dsc"));
    }

    // A third: a third each, at a lower DSC rate.
    let third = Shared::open(&mut server, &host, SIZE);
    third.scroll(true);
    for stream in [&first, &second, &third] {
        let third_each = stream.sharing(BUDGET / 3.0, 3, |counters| {
            scrolling(counters, "dsc")
                && counters["bits_per_pixel"]
                    .as_f64()
                    .is_some_and(|rate| rate < 12.0)
        });
        assert!(
            third_each["bits_per_pixel"]
                .as_f64()
                .is_some_and(|rate| rate >= 6.0),
            "{third_each}"
        );
    }
    let (rates, total) = measured("three streams", &[&first, &second, &third]);
    // What the two ends of one of them log each second.
    let last_line = |log: &str, mark: &str| {
        let line = log.lines().rev().filter(|line| line.contains(mark)).nth(1);
        line.unwrap_or_default().to_owned()
    };
    eprintln!(
        "sender: {}",
        last_line(&first.sender_log(), "stream: sent ")
    );
    eprintln!(
        "viewer: {}",
        last_line(&first.viewer.log_text(), " frames/s ")
    );
    assert!(
        (BUDGET * 0.8..=BUDGET * 1.03).contains(&total),
        "three streams received {total:.1} Mb/s of {BUDGET}: {rates:?}"
    );
    for rate in &rates {
        assert!(
            (BUDGET * 0.2..=BUDGET * 0.47).contains(rate),
            "the three streams received {rates:?} Mb/s"
        );
    }

    // Two of them stop: their screens settle to exact pixels, and the one
    // that still scrolls has the whole budget again and returns to raw.
    second.scroll(false);
    third.scroll(false);
    for stream in [&second, &third] {
        let still = stream.viewer.still_and_exact();
        assert_eq!(still["mode"], "raw", "{still}");
    }
    let again = first.sharing(BUDGET, 1, |counters| scrolling(counters, "raw"));
    assert!(
        again["mode_switches"]
            .as_u64()
            .is_some_and(|switches| switches >= 2),
        "{again}"
    );
    let (_, total) = measured("one stream again", &[&first]);
    assert!(
        (BUDGET * 0.7..=BUDGET * 1.03).contains(&total),
        "the stream that carried on received {total:.1} Mb/s of {BUDGET}"
    );
    first.scroll(false);
    first.viewer.still_and_exact();

    // Both ends name the share in their logs, and nothing failed.
    let sender_log = first.sender_log();
    for expected in [
        "share 72 Mb/s among 1 streams",
        "36 Mb/s, its share of 72 Mb/s among 2 streams on lo, --link-mbps 80",
        "24 Mb/s, its share of 72 Mb/s among 3 streams on lo, --link-mbps 80",
    ] {
        assert!(sender_log.contains(expected), "{expected}\n{sender_log}");
    }
    let viewer_log = first.viewer.log_text();
    for expected in [
        "path lo (127.0.0.1, no speed reported) from ",
        "share 36 Mb/s of 72 Mb/s among 2 streams",
        "share 24 Mb/s of 72 Mb/s among 3 streams",
    ] {
        assert!(viewer_log.contains(expected), "{expected}\n{viewer_log}");
    }
    for stream in [&first, &second, &third] {
        let log = stream.viewer.log_text();
        assert!(stream_errors(&log).is_empty(), "{log}");
        let log = stream.sender_log();
        assert!(stream_errors(&log).is_empty(), "{log}");
    }
    eprintln!("| streams scrolling | received Mb/s | sum | budget |\n|---|---|---|---|");
    for row in &rows {
        eprintln!("{row}");
    }

    for stream in [&first, &second, &third] {
        server.tool("session_stop", json!({"session_id":stream.session}));
        stopped("the window to close with its session", stream.viewer.pid);
    }
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

/// What a stream viewer last wrote about its path.
fn stream_path(viewer: &Viewer) -> Option<Value> {
    read_json(&viewer.log.parent()?.join(kwin_mcp::links::PATH_FILE))
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn a_stream_is_bound_to_its_path_and_moves_to_another_when_that_path_dies() {
    let home = home_with_secret("paths", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1800, 1000);
    // Two paths to the listener in place of this machine's interfaces. Both
    // are loopback addresses, so the sockets are real: "lo" is a wired
    // gigabit path, and "spare0" a wireless one that carries more than half
    // of it. No interface is called spare0, so that socket can be bound to
    // its address only.
    let table = home.join("paths.json");
    let registry = home.join("streams");
    let list = |paths: Value| {
        let temporary = table.with_extension("tmp");
        std::fs::write(
            &temporary,
            json!({"registry":registry, "paths":paths}).to_string(),
        )
        .expect("write the path table");
        std::fs::rename(&temporary, &table).expect("place the path table");
    };
    let wired = json!({"interface":"lo", "source":"127.0.0.2", "mbps":1000});
    let wireless =
        json!({"interface":"spare0", "source":"127.0.0.3", "mbps":2400, "wireless":true});
    list(json!([wireless, wired]));
    let listener = Listener::start(&home, &[]);
    let host = listener.address.clone();
    let (mut server, program) = Server::start_copy_with(
        &home,
        "server",
        Some(&desk.display()),
        &[(kwin_mcp::links::PATHS_ENV, &table)],
    );
    let open = |server: &mut Server| {
        let (session, workdir) =
            server.start_session(json!({"host":host, "width":800, "height":450}));
        server.tool(
            "launch_app",
            json!({"session_id":session, "command":"konsole", "window_timeout_seconds":40}),
        );
        let opened = server.tool("viewer_open", json!({"session_id":session}));
        let viewer = Viewer::opened(&opened, "opened", &host);
        (session, workdir, viewer, opened)
    };
    let sender_log =
        |workdir: &Path| std::fs::read_to_string(workdir.join("viewer.log")).unwrap_or_default();

    // The first stream takes the wired path: its socket is bound to that
    // path's address before it connects, and the listener sees it come from
    // there.
    let (first, first_dir, first_viewer, opened) = open(&mut server);
    let path = &opened["viewer"]["path"];
    assert_eq!(path["interface"], "lo", "{opened}");
    assert_eq!(path["source"], "127.0.0.2", "{opened}");
    assert_eq!(path["link_mbps"], 1000, "{opened}");
    assert_eq!(path["reconnects"], 0, "{opened}");
    // SO_BINDTODEVICE is allowed without privileges since Linux 5.7.
    let bound = path["bound"].as_str().expect("how the socket was bound");
    eprintln!("the socket on lo was bound to its {bound}");
    assert!(["device", "address"].contains(&bound), "{opened}");
    let log = sender_log(&first_dir);
    assert!(log.contains("stream: viewer 127.0.0.2:"), "{log}");
    // The server re-executes itself, as every install makes it do: the
    // window stays, and the new image answers it when its path dies.
    reexecute(&program, &home.join("server.log"), &host, 1);
    assert!(alive(first_viewer.pid), "the window closed with the reload");

    // The second takes the wireless path, which gives it more than half of
    // the wired one: bound to that path's address.
    let (second, second_dir, second_viewer, opened) = open(&mut server);
    let path = &opened["viewer"]["path"];
    assert_eq!(path["interface"], "spare0", "{opened}");
    assert_eq!(path["source"], "127.0.0.3", "{opened}");
    assert_eq!(path["bound"], "address", "{opened}");
    assert_eq!(path["wireless"], true, "{opened}");
    let log = sender_log(&second_dir);
    assert!(log.contains("stream: viewer 127.0.0.3:"), "{log}");
    // Both are counted where the next viewer looks.
    let counted = std::fs::read_dir(&registry)
        .expect("the registry")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "json"))
        .count();
    assert_eq!(counted, 2);

    // The first window shows a still screen, with the session's pointer
    // where the window's is.
    desk.activate(&first);
    desk.tool("mouse_move", json!({"x":520, "y":300}));
    std::thread::sleep(Duration::from_secs(1));
    let frames_before = first_viewer.idle()["frames"].as_u64().expect("frames");
    let sender_before = sender_pid(&first_dir);
    // The window as its desktop shows it now, kept as evidence under `name`.
    let shoot = |desk: &mut Desk, name: &str| {
        let shot = desk.tool("screenshot", json!({}));
        keep_evidence(&shot, name);
        decoded_png(Path::new(shot["path"].as_str().expect("screenshot path")))
    };
    let before = shoot(&mut desk, "916-before-the-path-died.png");

    // The wired path dies. The stream moves to the other one: a new frame
    // socket from that path's address and a new sender, the same window.
    list(json!([wireless]));
    let moved = wait_for(
        "the stream on the other path",
        Duration::from_secs(30),
        || stream_path(&first_viewer).filter(|path| path["reconnects"] == 1),
    );
    // The new sender's first frame is the screen as it is.
    wait_for("a frame on the other path", Duration::from_secs(15), || {
        (first_viewer.frames() > frames_before).then_some(())
    });
    assert_eq!(moved["interface"], "spare0", "{moved}");
    assert_eq!(moved["source"], "127.0.0.3", "{moved}");
    assert!(alive(first_viewer.pid), "the window closed");
    let sender_after = wait_for("the new frame sender", Duration::from_secs(15), || {
        read_json(&first_dir.join("viewer-status.json"))
            .filter(|status| status["state"] == "ready")
            .and_then(|status| status["pid"].as_u64())
            .filter(|pid| *pid != sender_before)
    });
    stopped("the sender on the dead path", sender_before);
    assert!(alive(sender_after));
    let log = sender_log(&first_dir);
    assert!(log.contains("stream: viewer 127.0.0.3:"), "{log}");
    let viewer_log = first_viewer.log_text();
    for expected in [
        "the path over lo died",
        "the window keeps its picture",
        "reconnected: path spare0 (127.0.0.3, 2400 Mb/s, wireless)",
    ] {
        assert!(viewer_log.contains(expected), "{expected}\n{viewer_log}");
    }
    // The window shows the picture it showed before, the session's pointer
    // included, once the new sender's frames have arrived.
    desk.activate(&first);
    let after = wait_for(
        "the picture the window showed before",
        Duration::from_secs(15),
        || {
            Some(shoot(&mut desk, "916-after-the-stream-moved.png"))
                .filter(|after| *after == before)
        },
    );

    // The stream works on its new path, in both directions: keys typed into
    // the window reach the session, and what they change arrives as frames.
    let frames = first_viewer.frames();
    let typed = first_dir.join("typed-after-the-move");
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
        "the file made from the window",
        Duration::from_secs(10),
        || typed.exists().then_some(()),
    );
    wait_for("frames on the new path", Duration::from_secs(10), || {
        (first_viewer.frames() > frames).then_some(())
    });
    first_viewer.idle();
    let typed_shot = shoot(&mut desk, "916-typed-on-the-new-path.png");
    assert!(
        typed_shot != after,
        "the window did not show what was typed"
    );
    // viewer_open reports the window it has, on the path it has now.
    let again = server.tool("viewer_open", json!({"session_id":first}));
    assert_eq!(
        Viewer::opened(&again, "already_open", &host).pid,
        first_viewer.pid
    );
    assert_eq!(again["viewer"]["path"]["interface"], "spare0", "{again}");
    assert_eq!(again["viewer"]["path"]["reconnects"], 1, "{again}");

    // The other stream never moved.
    let other = stream_path(&second_viewer).expect("the second path");
    assert_eq!(other["reconnects"], 0, "{other}");
    assert!(alive(second_viewer.pid));
    for viewer in [&first_viewer, &second_viewer] {
        let log = viewer.log_text();
        assert!(stream_errors(&log).is_empty(), "{log}");
    }

    for (session, viewer) in [(&first, &first_viewer), (&second, &second_viewer)] {
        server.tool("session_stop", json!({"session_id":session}));
        stopped("the window to close with its session", viewer.pid);
    }
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

/// Waits for a second in which the viewer received at least five frames and
/// returns that second's counters. Whatever it received, it must have
/// presented: a window that takes frames off the socket and draws none of
/// them shows a stale picture.
fn presenting(viewer: &Viewer, what: &str) -> Value {
    let before = viewer.counters();
    let counters = wait_for("a second with frames", Duration::from_secs(15), || {
        viewer.counters().filter(|counters| {
            Some(counters) != before.as_ref()
                && counters["last_second"]["frames"]
                    .as_u64()
                    .is_some_and(|frames| frames >= 5)
        })
    });
    assert!(
        counters["last_second"]["presented"]
            .as_u64()
            .is_some_and(|presented| presented >= 3),
        "{what}: frames arrive and the window presents none: {counters}\n{}",
        viewer.log_text()
    );
    counters
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn a_window_the_compositor_does_not_show_still_presents_the_frames_it_receives() {
    let home = home_with_secret("unshown", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1600, 1000);
    let listener = Listener::start(&home, &[]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":scrolling_text(3600, 1), "window_timeout_seconds":40}),
    );
    let opened = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&opened, "opened", &host);
    let sender = sender_pid(&workdir);
    desk.activate(&session);

    // Shown: the compositor asks for every redraw.
    let shown = presenting(&viewer, "shown");
    assert_eq!(
        shown["last_second"]["presented_unshown"]
            .as_u64()
            .unwrap_or(0),
        0,
        "the shown window drew without frame callbacks: {shown}"
    );

    // Minimized, the window gets no frame callbacks, so nothing asks it to
    // redraw. It keeps presenting what arrives, so a screenshot, a window
    // thumbnail or the person who brings it back sees the current picture.
    let title = format!("kwin-viewer {session}");
    let minimized = |desk: &mut Desk| {
        let windows = desk.tool("window_list", json!({}));
        record(&windows, "windows", |window| {
            window["title"]
                .as_str()
                .is_some_and(|shown| shown.starts_with(&title))
        })
        .map(|window| window["minimized"] == true)
    };
    desk.tool("keyboard_key", json!({"key":"super+pagedown"}));
    wait_for(
        "the window to be minimized",
        Duration::from_secs(10),
        || (minimized(&mut desk) == Some(true)).then_some(()),
    );
    std::thread::sleep(Duration::from_secs(2));
    let hidden = presenting(&viewer, "minimized");
    assert!(
        hidden["last_second"]["presented_unshown"]
            .as_u64()
            .is_some_and(|unshown| unshown >= 3),
        "the minimized window drew on frame callbacks: {hidden}"
    );
    let again = presenting(&viewer, "still minimized");
    assert!(again["presented"].as_u64() > hidden["presented"].as_u64());

    // Shown again, it is back to the compositor's redraws.
    desk.activate(&session);
    wait_for(
        "the window to be shown again",
        Duration::from_secs(10),
        || (minimized(&mut desk) == Some(false)).then_some(()),
    );
    std::thread::sleep(Duration::from_secs(2));
    let back = presenting(&viewer, "shown again");
    assert_eq!(back["last_second"]["presented_unshown"], 0, "{back}");
    assert!(
        stream_errors(&viewer.log_text()).is_empty(),
        "{}",
        viewer.log_text()
    );

    // A stream that ends while the window is minimized still closes it,
    // although no redraw comes in which it could notice.
    desk.tool("keyboard_key", json!({"key":"super+pagedown"}));
    wait_for(
        "the window to be minimized",
        Duration::from_secs(10),
        || (minimized(&mut desk) == Some(true)).then_some(()),
    );
    signal(sender, "KILL");
    stopped(
        "the minimized window to close when its stream ends",
        viewer.pid,
    );
    server.tool("session_stop", json!({"session_id":session}));
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn the_local_viewer_keeps_showing_a_changing_session() {
    // The local viewer shares the window code with the remote one: the same
    // swapchain and the same draws without a frame callback.
    let home = home_with_secret("local", "shared-secret-0123456789");
    let mut desk = Desk::start(&home, 1600, 1000);
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let (session, _workdir) = server.start_session(json!({"width":1280, "height":720}));
    server.tool(
        "launch_app",
        json!({"session_id":session, "command":scrolling_text(3600, 1), "window_timeout_seconds":40}),
    );
    let opened = server.tool("viewer_open", json!({"session_id":session}));
    assert_eq!(opened["viewer"]["state"], "ready", "{opened}");
    let viewer = opened["viewer"]["pid"].as_u64().expect("viewer pid");
    desk.activate(&session);
    // Two screenshots of the window while the text scrolls differ.
    let shot = |desk: &mut Desk| {
        let shown = desk.tool("screenshot", json!({"include_cursor":false}));
        let path = PathBuf::from(shown["path"].as_str().expect("screenshot path"));
        read_png(&path)
    };
    let first = shot(&mut desk);
    assert_eq!((first.0, first.1), (1280, 720));
    wait_for(
        "the viewer window to show another picture",
        Duration::from_secs(10),
        || (shot(&mut desk).2 != first.2).then_some(()),
    );
    let closed = server.tool("viewer_close", json!({"session_id":session}));
    assert_eq!(closed["status"], "closed", "{closed}");
    stopped("the viewer to exit after viewer_close", viewer);
    server.tool("session_stop", json!({"session_id":session}));
    desk.stop();
    drop(server);
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

/// The text a `clipboard_read` result holds; None for no text and for a
/// generated secret, whose value the tool withholds.
fn clipboard_text(read: &Value) -> Option<String> {
    read["text"].as_str().map(str::to_owned)
}

/// The text a `clipboard_read` result holds after a whole saved KWrite
/// document was copied: without the line end KWrite keeps at its end.
fn copied_text(read: &Value) -> Option<String> {
    clipboard_text(read).map(|text| text.trim_end_matches('\n').to_owned())
}

/// Agent input is refused for a second after input through a viewer.
fn after_viewer_input() {
    std::thread::sleep(Duration::from_millis(1200));
}

/// A session on `host` with an empty KWrite document and an open viewer.
/// Returns the session, its work directory, the document and the viewer.
fn session_with_document(server: &mut Server, host: &str) -> (String, PathBuf, PathBuf, Viewer) {
    let (session, workdir) = server.start_session(json!({"host":host, "width":1280, "height":720}));
    // The work directory is the same path inside the session.
    let document = workdir.join("document.txt");
    std::fs::write(&document, "").expect("create the document");
    server.tool(
        "launch_app",
        json!({
            "session_id":session,
            "command":format!("kwrite {}", document.display()),
            "window_timeout_seconds":40,
        }),
    );
    let opened = server.tool("viewer_open", json!({"session_id":session}));
    let viewer = Viewer::opened(&opened, "opened", host);
    wait_for("the first frame", Duration::from_secs(10), || {
        (viewer.frames() > 0).then_some(())
    });
    (session, workdir, document, viewer)
}

/// The agent saves the session's document until the file holds what
/// `expected` accepts, and returns that text (without the line end KWrite
/// adds). The file is the session's own account of what was typed into it.
fn saved(
    server: &mut Server,
    session: &str,
    document: &Path,
    what: &str,
    expected: impl Fn(&str) -> bool,
) -> String {
    after_viewer_input();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        server.tool(
            "keyboard_key",
            json!({"session_id":session, "key":"ctrl+s"}),
        );
        std::thread::sleep(Duration::from_millis(400));
        let text = std::fs::read_to_string(document).unwrap_or_default();
        let text = text.trim_end_matches('\n');
        if expected(text) {
            return text.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "{what}: the document holds {text:?}"
        );
    }
}

/// The agent replaces the session's document with `text`, selects it all and
/// leaves `selection` as its own clipboard selection.
fn document_selected(
    server: &mut Server,
    session: &str,
    document: &Path,
    text: &str,
    selection: &str,
) {
    after_viewer_input();
    server.tool(
        "keyboard_key",
        json!({"session_id":session, "key":"ctrl+a"}),
    );
    server.tool("keyboard_paste", json!({"session_id":session, "text":text}));
    saved(server, session, document, "the agent's own text", |held| {
        held == text
    });
    server.tool(
        "clipboard_write",
        json!({"session_id":session, "target":"session", "text":selection}),
    );
    server.tool(
        "keyboard_key",
        json!({"session_id":session, "key":"ctrl+a"}),
    );
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, KWrite, Vulkan, and input devices"]
fn the_remote_viewer_bridges_the_callers_clipboard_and_gives_the_agent_its_selection_back() {
    const AGENT: &str = "the agent's own selection";
    const OTHER_AGENT: &str = "the other session's agent selection";
    const CALLER: &str = "from the caller's clipboard: naïve — 日本語 \"quoted\" ✓";
    const COPIED: &str = "copied in the remote session: Grüße, 東京 ✓";
    const CUT: &str = "cut in the remote session: åäö 456";
    const PREPARED: &str = "prepared with target=host: ünïcode ✓";
    const SECRET_LENGTH: usize = 24;

    let home = home_with_secret("clipboard", "shared-secret-0123456789");
    // The caller's desktop. The calling server opens its viewer windows in
    // it, so its clipboard is the clipboard of the machine the viewer window
    // is on, and the tester's own clipboard is never involved.
    let mut desk = Desk::start(&home, 1600, 1000);
    let listener = Listener::start(&home, &[]);
    let host = listener.address.clone();
    let mut server = Server::start(&home, "server", Some(&desk.display()), None);
    let callers_clipboard = |desk: &mut Desk| desk.tool("clipboard_read", json!({}));
    let sessions_clipboard = |server: &mut Server, session: &str| {
        clipboard_text(&server.tool("clipboard_read", json!({"session_id":session})))
    };
    let agents_selection_returns = |server: &mut Server, session: &str, after: &str| {
        wait_for(
            &format!("the agent's selection to return after the {after}"),
            Duration::from_secs(10),
            || (sessions_clipboard(server, session).as_deref() == Some(AGENT)).then_some(()),
        );
    };

    // Two sessions on the listener, each with a document, a viewer and a
    // selection of its agent's.
    let (session, workdir, document, viewer) = session_with_document(&mut server, &host);
    let (other, other_workdir, other_document, other_viewer) =
        session_with_document(&mut server, &host);
    for (id, selection) in [(&session, AGENT), (&other, OTHER_AGENT)] {
        server.tool(
            "clipboard_write",
            json!({"session_id":id, "target":"session", "text":selection}),
        );
    }
    desk.activate(&session);

    // Paste: Ctrl+V in the window pastes the caller's clipboard into the
    // remote session, not the agent's selection, which then returns.
    desk.tool(
        "clipboard_write",
        json!({"target":"session", "text":CALLER}),
    );
    desk.tool("keyboard_key", json!({"key":"ctrl+v"}));
    saved(
        &mut server,
        &session,
        &document,
        "the caller's clipboard pasted through the viewer",
        |text| text == CALLER,
    );
    agents_selection_returns(&mut server, &session, "paste");
    assert_eq!(
        clipboard_text(&callers_clipboard(&mut desk)).as_deref(),
        Some(CALLER)
    );
    // With `KWIN_MCP_TEST_EVIDENCE`: the viewer window on the caller's
    // desktop, showing the remote document with the pasted text.
    keep_evidence(
        &desk.tool("screenshot", json!({"include_cursor":false})),
        "clipboard-pasted-through-the-viewer.png",
    );

    // Copy: Ctrl+C in the window puts the remote session's selection on the
    // caller's clipboard, and the agent's selection returns.
    document_selected(&mut server, &session, &document, COPIED, AGENT);
    desk.tool("keyboard_key", json!({"key":"ctrl+c"}));
    wait_for(
        "the session's selection on the caller's clipboard",
        Duration::from_secs(15),
        || (copied_text(&callers_clipboard(&mut desk)).as_deref() == Some(COPIED)).then_some(()),
    );
    agents_selection_returns(&mut server, &session, "copy");

    // Cut: Ctrl+X does the same and empties the document.
    document_selected(&mut server, &session, &document, CUT, AGENT);
    desk.tool("keyboard_key", json!({"key":"ctrl+x"}));
    wait_for(
        "the cut text on the caller's clipboard",
        Duration::from_secs(15),
        || (copied_text(&callers_clipboard(&mut desk)).as_deref() == Some(CUT)).then_some(()),
    );
    agents_selection_returns(&mut server, &session, "cut");
    saved(
        &mut server,
        &session,
        &document,
        "the document after the cut",
        str::is_empty,
    );

    // Shift+Insert pastes the caller's clipboard too: the text just cut.
    desk.tool("keyboard_key", json!({"key":"shift+Insert"}));
    saved(
        &mut server,
        &session,
        &document,
        "the caller's clipboard pasted with Shift+Insert",
        |text| text == CUT,
    );
    agents_selection_returns(&mut server, &session, "Shift+Insert paste");

    // clipboard_write target=host on the remote session writes the caller's
    // clipboard, where the user is, and a viewer paste carries it over.
    let prepared = server.tool(
        "clipboard_write",
        json!({"session_id":session, "target":"host", "text":PREPARED}),
    );
    assert_eq!(prepared["ok"], true, "{prepared}");
    assert_eq!(prepared["session_host"], host, "{prepared}");
    assert_eq!(
        clipboard_text(&callers_clipboard(&mut desk)).as_deref(),
        Some(PREPARED)
    );
    // target=session and clipboard_read still mean the remote session's.
    assert_eq!(
        sessions_clipboard(&mut server, &session).as_deref(),
        Some(AGENT)
    );
    document_selected(&mut server, &session, &document, "to be replaced", AGENT);
    desk.tool("keyboard_key", json!({"key":"ctrl+v"}));
    saved(
        &mut server,
        &session,
        &document,
        "the text prepared with target=host pasted through the viewer",
        |text| text == PREPARED,
    );
    agents_selection_returns(&mut server, &session, "paste of the prepared text");
    keep_evidence(
        &desk.tool("screenshot", json!({"include_cursor":false})),
        "clipboard-target-host-pasted-through-the-viewer.png",
    );

    // A generated secret is made by the calling server and written to the
    // caller's clipboard with its marker: no result and no reading tool has
    // its value.
    let generated = server.tool(
        "clipboard_write",
        json!({
            "session_id":session, "target":"host",
            "generate_secret":{
                "length":SECRET_LENGTH,
                "character_classes":["lowercase", "uppercase", "digits"],
            },
        }),
    );
    let withheld = callers_clipboard(&mut desk);
    assert_eq!(withheld["protected"], true, "{withheld}");
    assert_eq!(withheld["length"], SECRET_LENGTH, "{withheld}");
    assert!(withheld["text"].is_null(), "{withheld}");
    // The user pastes it where it is needed, which is the only time it
    // crosses to the session's machine.
    document_selected(&mut server, &session, &document, "to be replaced", AGENT);
    desk.tool("keyboard_key", json!({"key":"ctrl+v"}));
    let secret = saved(
        &mut server,
        &session,
        &document,
        "the generated secret pasted through the viewer",
        |text| {
            text.chars().count() == SECRET_LENGTH
                && text
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        },
    );
    agents_selection_returns(&mut server, &session, "paste of the secret");
    assert!(!generated.to_string().contains(&secret), "{generated}");

    // The other session saw none of it: its document is empty, its agent's
    // selection is its own, and neither end of its viewer handled a shortcut.
    server.tool("keyboard_key", json!({"session_id":other, "key":"ctrl+s"}));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        std::fs::read_to_string(&other_document).unwrap_or_default(),
        ""
    );
    assert_eq!(
        sessions_clipboard(&mut server, &other).as_deref(),
        Some(OTHER_AGENT)
    );
    let sender_log =
        |workdir: &Path| std::fs::read_to_string(workdir.join("viewer.log")).expect("sender log");
    let other_logs = [other_viewer.log_text(), sender_log(&other_workdir)];
    for log in &other_logs {
        let handled: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("shortcut:"))
            .collect();
        assert_eq!(handled, [""; 0], "the other session's viewer bridged");
    }
    // This session's sender handled every one of them.
    let logs = [viewer.log_text(), sender_log(&workdir)];
    assert_eq!(
        logs[1]
            .matches("paste shortcut: the session pastes")
            .count(),
        4,
        "{}",
        logs[1]
    );
    assert_eq!(
        logs[1]
            .matches("copy shortcut: took the session selection")
            .count(),
        2,
        "{}",
        logs[1]
    );
    assert_eq!(
        logs[0]
            .matches("copy shortcut: put the session's selection")
            .count(),
        2,
        "{}",
        logs[0]
    );

    // Neither end of the stream logged a text that crossed, and the streams
    // had no failure.
    for log in &logs {
        assert!(stream_errors(log).is_empty(), "{log}");
        for text in [secret.as_str(), CALLER, COPIED, CUT, PREPARED] {
            assert!(!log.contains(text), "a stream log holds clipboard text");
        }
    }
    // No server's log holds the secret either: the calling server's, the
    // listener's, the caller's desktop's.
    let server_logs: Vec<PathBuf> = std::fs::read_dir(&home)
        .expect("read the test HOME")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect();
    assert!(server_logs.len() >= 3, "{server_logs:?}");
    for path in &server_logs {
        let log = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            !log.contains(&secret),
            "{} holds the secret",
            path.display()
        );
    }

    for id in [&session, &other] {
        let closed = server.tool("viewer_close", json!({"session_id":id}));
        assert_eq!(closed["status"], "closed", "{closed}");
        server.tool("session_stop", json!({"session_id":id}));
    }
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    desk.stop();
    drop(server);
    drop(listener);
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}

#[test]
#[ignore = "needs KWIN_MCP_TEST_OLD_BINARY, a kwin-mcp built before the clipboard bridge; requires KDE, KWin, bubblewrap, pasta, KWrite, Vulkan, and input devices"]
fn a_peer_built_before_the_clipboard_bridge_streams_and_takes_keys_as_before() {
    const AGENT: &str = "the agent's own selection";
    const CALLER: &str = "the caller's clipboard";
    const PREPARED: &str = "prepared with target=host";
    let old = PathBuf::from(
        std::env::var_os("KWIN_MCP_TEST_OLD_BINARY")
            .expect("KWIN_MCP_TEST_OLD_BINARY names a kwin-mcp built before the clipboard bridge"),
    );
    let new = server_binary();
    for (label, listener_binary, caller_binary, caller_bridges) in [
        ("old-listener", &old, &new, true),
        ("old-caller", &new, &old, false),
    ] {
        let home = home_with_secret(label, "shared-secret-0123456789");
        let mut desk = Desk::start(&home, 1600, 1000);
        // The isolated desktop is every server's host desktop here, the
        // listener's too, so no build can reach the tester's own clipboard.
        let mut listening = kwin_mcp_at(listener_binary, &home);
        listening.env("WAYLAND_DISPLAY", desk.display());
        let listener = Listener::start_command(listening, &home, &[]);
        let host = listener.address.clone();
        let mut server = Server::start_command(
            kwin_mcp_at(caller_binary, &home),
            &home,
            "server",
            Some(&desk.display()),
            None,
        );
        let (session, workdir, document, viewer) = session_with_document(&mut server, &host);
        server.tool(
            "clipboard_write",
            json!({"session_id":session, "target":"session", "text":AGENT}),
        );
        desk.tool(
            "clipboard_write",
            json!({"target":"session", "text":CALLER}),
        );
        desk.activate(&session);

        // One end has no bridge, so the shortcut is a key like any other, as
        // it was before the bridge: the session pastes its own selection, and
        // the caller's clipboard stays where it is.
        desk.tool("keyboard_key", json!({"key":"ctrl+v"}));
        saved(
            &mut server,
            &session,
            &document,
            "the session's own selection pasted",
            |text| text == AGENT,
        );
        let callers_clipboard =
            |desk: &mut Desk| clipboard_text(&desk.tool("clipboard_read", json!({})));
        assert_eq!(callers_clipboard(&mut desk).as_deref(), Some(CALLER));

        let written = server.request(
            "tools/call",
            json!({
                "name":"clipboard_write",
                "arguments":{"session_id":session, "target":"host", "text":PREPARED},
            }),
        );
        if caller_bridges {
            // This build's caller writes its own desktop's clipboard, whatever
            // the listener's build.
            assert!(written["error"].is_null(), "{written}");
            assert_eq!(callers_clipboard(&mut desk).as_deref(), Some(PREPARED));
        } else {
            // The old caller forwards the call. This build's listener does
            // not write a clipboard on the session's machine.
            assert_eq!(
                written["error"]["data"]["reason"], "host_clipboard_is_the_callers",
                "{written}"
            );
            assert_eq!(callers_clipboard(&mut desk).as_deref(), Some(CALLER));
        }

        let logs = [
            viewer.log_text(),
            std::fs::read_to_string(workdir.join("viewer.log")).expect("sender log"),
        ];
        for log in &logs {
            assert!(stream_errors(log).is_empty(), "{log}");
            assert!(!log.contains("shortcut:"), "{log}");
        }
        assert_eq!(
            logs[0].contains("does not bridge the clipboard"),
            caller_bridges,
            "{}",
            logs[0]
        );

        let closed = server.tool("viewer_close", json!({"session_id":session}));
        assert_eq!(closed["status"], "closed", "{closed}");
        server.tool("session_stop", json!({"session_id":session}));
        assert_eq!(viewers_under(&home), [], "viewers or senders still run");
        desk.stop();
        drop(server);
        drop(listener);
        std::fs::remove_dir_all(home).expect("remove the test HOME");
    }
}

/// A PNG's decoded bytes, whatever its pixel layout.
fn decoded_png(path: &Path) -> Vec<u8> {
    let decoder = png::Decoder::new(std::fs::File::open(path).expect("open PNG"));
    let mut reader = decoder.read_info().expect("PNG header");
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).expect("PNG pixels");
    buffer.truncate(info.buffer_size());
    buffer
}

/// Runs a desktop tool and returns what it printed, or None when it failed.
fn desktop_tool(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
#[ignore = "opens a viewer window on the tester's own desktop for some seconds; requires KDE, KWin, bubblewrap, pasta, Konsole, Vulkan, and input devices"]
fn the_viewer_presents_the_frames_it_receives_on_the_testers_own_desktop() {
    // The other tests open their windows in an isolated KWin desktop: one
    // virtual SDR output at scale 1 that is always on. Only the desktop the
    // tester sits at has real outputs with their scale and HDR mode, and the
    // states in which its compositor repaints nothing and so asks no window
    // to redraw: outputs powered off, a locked screen, another virtual
    // desktop. This test opens the window there, in whatever state that
    // desktop is in, and checks that the window presents what arrives. The
    // log lines it copies say which presents the compositor did not ask for
    // ("N of them while the window was not shown").
    //
    // A session of 1920x1080 on an output at scale 1.25 gets a window of
    // 2400x1350 device pixels, larger than the session; a session of
    // 2400x1350 gets the same window at one device pixel per session pixel.
    //
    // KWIN_MCP_STREAM_HEIGHTS: session heights (16:9).
    // KWIN_MCP_REAL_DESKTOP_SECONDS: how long each window stays open.
    // KWIN_MCP_STREAM_EVIDENCE: a directory that receives the window's log
    // and two screenshots of it (kdotool and spectacle), which must differ.
    let heights = numbers_from("KWIN_MCP_STREAM_HEIGHTS", &[1080]);
    let seconds = numbers_from("KWIN_MCP_REAL_DESKTOP_SECONDS", &[6])[0];
    let evidence = std::env::var_os("KWIN_MCP_STREAM_EVIDENCE").map(PathBuf::from);
    let home = home_with_secret("real", "shared-secret-0123456789");
    for height in heights {
        let width = height * 16 / 9;
        let listener = Listener::start(&home, &["--link-mbps", "1000"]);
        let host = listener.address.clone();
        let mut server = Server::start(&home, "server", None, None);
        let (session, _workdir) =
            server.start_session(json!({"host":host, "width":width, "height":height}));
        server.tool(
            "launch_app",
            json!({
                "session_id":session,
                "command":scrolling_text(3600, 1),
                "window_timeout_seconds":40,
            }),
        );
        let opened = server.tool(
            "viewer_open",
            json!({"session_id":session, "reason":"present check"}),
        );
        let viewer = Viewer::opened(&opened, "opened", &host);
        // KWIN_MCP_REAL_DESKTOP_MOVE=X,Y moves the window there (kdotool), to
        // put it on a particular output.
        let place = numbers_from("KWIN_MCP_REAL_DESKTOP_MOVE", &[]);
        if let [x, y] = place[..] {
            let title = format!("kwin-viewer {session}");
            let window = wait_for(
                "kdotool to see the viewer window",
                Duration::from_secs(5),
                || {
                    desktop_tool("kdotool", &["search", "--name", &title])
                        .and_then(|found| found.lines().next().map(str::to_owned))
                },
            );
            desktop_tool(
                "kdotool",
                &["windowmove", &window, &x.to_string(), &y.to_string()],
            )
            .expect("kdotool windowmove");
            std::thread::sleep(Duration::from_secs(1));
        }
        // Every second the viewer reports, it presented frames.
        let mut seen = 0;
        let mut last = viewer.counters();
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200));
            let Some(counters) = viewer
                .counters()
                .filter(|counters| Some(counters) != last.as_ref())
            else {
                continue;
            };
            eprintln!("{width}x{height}: {counters}");
            if counters["last_second"]["frames"]
                .as_u64()
                .is_some_and(|frames| frames >= 5)
            {
                assert!(
                    counters["last_second"]["presented"]
                        .as_u64()
                        .is_some_and(|presented| presented >= 3),
                    "frames arrive and the window presents none: {counters}\n{}",
                    viewer.log_text()
                );
                seen += 1;
            }
            last = Some(counters);
        }
        assert!(seen >= 2, "the viewer reported {seen} seconds with frames");
        if let Some(evidence) = evidence.as_deref() {
            // Only the viewer's own window is captured: it is made the active
            // window, and the screenshot is taken if it then is.
            let title = format!("kwin-viewer {session}");
            let window = desktop_tool("kdotool", &["search", "--name", &title])
                .and_then(|found| found.lines().next().map(str::to_owned));
            if let Some(window) = window {
                desktop_tool("kdotool", &["windowactivate", &window]);
                std::thread::sleep(Duration::from_millis(700));
                let active = desktop_tool("kdotool", &["getactivewindow", "getwindowname"]);
                if active.is_some_and(|name| name.starts_with(&title)) {
                    // Two screenshots while the text scrolls: the window's
                    // picture has to change between them.
                    let pictures = ["a", "b"].map(|shot| {
                        let picture =
                            evidence.join(format!("real-desktop-{width}x{height}-{shot}.png"));
                        let _ = std::fs::remove_file(&picture);
                        desktop_tool(
                            "spectacle",
                            &[
                                "--background",
                                "--activewindow",
                                "--nonotify",
                                "--output",
                                &picture.to_string_lossy(),
                            ],
                        );
                        std::thread::sleep(Duration::from_millis(1200));
                        picture
                    });
                    if pictures.iter().all(|picture| picture.exists()) {
                        assert_ne!(
                            decoded_png(&pictures[0]),
                            decoded_png(&pictures[1]),
                            "the window showed the same picture 1.2 s apart while text scrolled"
                        );
                    }
                }
            }
            std::fs::copy(
                &viewer.log,
                evidence.join(format!("real-desktop-{width}x{height}.log")),
            )
            .expect("copy the viewer log");
        }
        let closed = server.tool("viewer_close", json!({"session_id":session}));
        assert_eq!(closed["status"], "closed", "{closed}");
        stopped("the viewer to exit after viewer_close", viewer.pid);
        server.tool("session_stop", json!({"session_id":session}));
        drop(server);
        drop(listener);
    }
    assert_eq!(viewers_under(&home), [], "viewers or senders still run");
    std::fs::remove_dir_all(home).expect("remove the test HOME");
}
