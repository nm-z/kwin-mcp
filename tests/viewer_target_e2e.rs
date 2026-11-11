#![allow(clippy::expect_used)]

//! The viewer and the session it shows (#911: #894, #895, #897).
//!
//! `viewer_close` for a session whose owning server is gone needs no desktop.
//! The other cases start real isolated desktops and are ignored by default:
//!
//! ```bash
//! KWIN_MCP_E2E=1 cargo test --release --test viewer_target_e2e -- --ignored --test-threads=1
//! ```
//!
//! Every viewer window opens inside an isolated desktop that the test starts,
//! never on the tester's own.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        // Short, and outside /tmp, which an isolated desktop hides.
        let root = PathBuf::from(format!("/var/tmp/kvt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for directory in [".config", ".local/share", ".cache", ".local/state", ".kde"] {
            std::fs::create_dir_all(root.join("home").join(directory)).expect("private HOME");
        }
        Self(root)
    }
    fn home(&self) -> PathBuf {
        self.0.join("home")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Closing stdin is how a client leaves; the server then stops its
        // sessions. Kill only what does not leave by itself.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server_binary() -> String {
    std::env::var("KWIN_MCP_E2E_SERVER").unwrap_or_else(|_| env!("CARGO_BIN_EXE_kwin-mcp").to_owned())
}

fn viewer_binary() -> PathBuf {
    let server = PathBuf::from(server_binary());
    let beside = server.with_file_name("kwin-viewer");
    if beside.exists() { beside } else { PathBuf::from(env!("CARGO_BIN_EXE_kwin-viewer")) }
}

impl Server {
    /// `wrapper` runs the server under another program (for instance in a new
    /// user namespace); `environment` is added to a private HOME.
    fn start(home: &Path, wrapper: &[&str], arguments: &[&str], environment: &[(&str, &str)]) -> Self {
        let binary = server_binary();
        let mut command = match wrapper.split_first() {
            Some((program, rest)) => {
                let mut command = Command::new(program);
                command.args(rest).arg(&binary);
                command
            }
            None => Command::new(&binary),
        };
        command
            .args(arguments)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("KDEHOME", home.join(".kde"))
            .envs(environment.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn kwin-mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let mut server = Self { child, stdin, stdout, next: 1 };
        let reply = server.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"viewer-target-e2e","version":"1"}
            }),
        );
        assert!(reply["result"].is_object(), "initialize failed: {reply}");
        server.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        server
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("server stdin");
        writeln!(stdin, "{message}").expect("send request");
        stdin.flush().expect("flush request");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
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

    /// Starts a session and returns its ID and workdir.
    fn session(&mut self, width: u32, height: u32) -> (String, PathBuf) {
        let started = self.tool("session_start", json!({"width":width, "height":height}));
        let content = &started["result"]["structuredContent"];
        let id = content["session_id"].as_str().unwrap_or_else(|| panic!("session_start: {started}"));
        let workdir = content["workdir"].as_str().expect("session workdir");
        (id.to_owned(), PathBuf::from(workdir))
    }

    /// Leaves as a client does and waits for the server's own cleanup.
    fn shut_down(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("server exit");
        assert!(status.success(), "server exit: {status}");
    }
}

fn require_e2e() {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"), "set KWIN_MCP_E2E=1 to run");
}

/// A pid with no process, so no live `kwin-mcp` can be it.
fn dead_pid() -> u32 {
    (2..=4_194_000_u32)
        .rev()
        .skip_while(|pid| Path::new(&format!("/proc/{pid}")).exists())
        .next()
        .expect("a free pid")
}

// #894. Needs no desktop: the owner named by the session ID does not exist.
#[test]
fn viewer_close_succeeds_for_a_session_whose_owner_is_gone() {
    let fixture = Fixture::new("gone");
    let runtime = fixture.0.join("run");
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("restrict runtime");
    let runtime_text = runtime.to_str().expect("runtime path");
    let mut server =
        Server::start(&fixture.home(), &[], &["--no-viewer"], &[("XDG_RUNTIME_DIR", runtime_text)]);
    let dead = dead_pid();
    let session = format!("s{dead}-1");
    let closed = server.tool("viewer_close", json!({"session_id":session}));
    assert!(closed["error"].is_null(), "viewer_close must not fail for an ended session: {closed}");
    let content = &closed["result"]["structuredContent"];
    assert_eq!(content["status"], "already_closed", "{closed}");
    assert_eq!(content["session_status"], "owner_exited", "{closed}");
    assert_eq!(content["session_server_pid"], dead, "{closed}");
    assert_eq!(content["session_server_running"], false, "{closed}");
    assert_eq!(content["viewer"]["state"], "closed", "{closed}");
    // Other tools still say that the session is gone.
    let shot = server.tool("screenshot", json!({"session_id":session}));
    assert_eq!(shot["error"]["data"]["reason"], "different_server_process", "{shot}");
}

// #894 as reported: the owner opened a viewer, shut down and recorded its
// cleanup; another server is then asked to close that viewer.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices and a viewer GPU"]
fn viewer_close_after_the_owner_shut_down_reports_the_stopped_session() {
    require_e2e();
    let fixture = Fixture::new("owner");
    let home = fixture.home();
    let mut desk = Server::start(&home, &[], &["--autoclean"], &[]);
    let (_desk_id, desk_dir) = desk.session(1280, 800);
    let display = desk_dir.join("wayland-0");
    let display = display.to_str().expect("desk display");

    let mut owner = Server::start(&home, &[], &["--autoclean"], &[("WAYLAND_DISPLAY", display)]);
    let (session, workdir) = owner.session(800, 600);
    let opened = owner.tool("viewer_open", json!({"session_id":session}));
    assert_eq!(opened["result"]["structuredContent"]["viewer"]["state"], "ready", "{opened}");
    let viewer = opened["result"]["structuredContent"]["viewer"]["pid"].as_u64().expect("viewer pid");
    owner.shut_down();
    assert!(!Path::new(&format!("/proc/{viewer}")).exists(), "the viewer outlived its server");
    assert!(!workdir.join("wayland-0").exists(), "the session outlived its server");

    let closed = desk.tool("viewer_close", json!({"session_id":session}));
    assert!(closed["error"].is_null(), "{closed}");
    let content = &closed["result"]["structuredContent"];
    assert_eq!(content["status"], "already_closed", "{closed}");
    assert_eq!(content["session_status"], "stopped", "{closed}");
    assert_eq!(content["session_end_reason"], "server_shutdown", "{closed}");
    assert_eq!(content["session_server_running"], false, "{closed}");
}

/// Entries of `directory` that are links into the sandbox's view of the
/// host's runtime directory, by name.
fn host_socket_links(directory: &Path) -> Vec<String> {
    let mut links = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return links;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if std::fs::read_link(&path).is_ok_and(|target| target.starts_with("/run/kwin-mcp-host-sockets")) {
            links.push(entry.file_name().to_string_lossy().into_owned());
        } else if path.is_dir() && !path.is_symlink() {
            for nested in host_socket_links(&path) {
                links.push(format!("{}/{nested}", entry.file_name().to_string_lossy()));
            }
        }
    }
    links.sort();
    links
}

// #897, #895: a server that cannot read other processes' descriptors (its
// launcher confined it: a user namespace here, a Landlock domain elsewhere)
// could not tell the desktop's sockets from helpers and linked all of them
// into the session: the host's wayland-N, pulse/native and pipewire-0. The
// session compositor connects to `pipewire-0` as it starts, so its screencast
// node could land in the host's PipeWire daemon, where the viewer cannot
// reach it.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap, input devices, a viewer GPU and unprivileged user namespaces"]
fn a_confined_server_links_no_host_socket_and_its_viewer_opens() {
    require_e2e();
    let fixture = Fixture::new("blind");
    let home = fixture.home();
    let mut desk = Server::start(&home, &[], &["--autoclean"], &[]);
    let (_desk_id, desk_dir) = desk.session(1280, 800);
    let display = desk_dir.join("wayland-0");
    let display = display.to_str().expect("desk display");

    let mut confined = Server::start(
        &home,
        &["unshare", "--user", "--map-current-user"],
        &["--autoclean"],
        &[("WAYLAND_DISPLAY", display)],
    );
    let (session, workdir) = confined.session(800, 600);
    let links = host_socket_links(&workdir);
    assert!(links.is_empty(), "host sockets were linked into the session: {links:?}");
    let opened = confined.tool("viewer_open", json!({"session_id":session}));
    assert_eq!(opened["result"]["structuredContent"]["viewer"]["state"], "ready", "{opened}");
    let closed = confined.tool("viewer_close", json!({"session_id":session}));
    assert_eq!(closed["result"]["structuredContent"]["status"], "closed", "{closed}");
}

/// A child that leads its own process group, ended with the whole group:
/// `dbus-run-session` leaves its bus and the compositor behind when it alone
/// is killed.
struct Kill(Child);

impl Kill {
    fn spawn(command: &mut Command, what: &str) -> Self {
        use std::os::unix::process::CommandExt;
        Self(command.process_group(0).spawn().unwrap_or_else(|error| panic!("spawn {what}: {error}")))
    }

    fn signal(&self, signal: nix::sys::signal::Signal) {
        if let Ok(group) = i32::try_from(self.0.id()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), signal);
        }
    }
}

impl Drop for Kill {
    fn drop(&mut self) {
        self.signal(nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && matches!(self.0.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        // What the leader left behind has had its moment to write and leave.
        std::thread::sleep(Duration::from_millis(300));
        self.signal(nix::sys::signal::Signal::SIGKILL);
        let _ = self.0.wait();
    }
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn daemon(program: &str, arguments: &[&str], home: &Path, runtime: &Path) -> Kill {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("PIPEWIRE_REMOTE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Kill::spawn(&mut command, program)
}

// #897, #895, the viewer's half: a compositor whose screencast node is in
// another PipeWire daemon than the one beside its Wayland socket. The session
// manager of the daemon the viewer reads answers "no target node available",
// and the viewer used to wait four seconds for a frame and report only that.
// Two private daemons stand in for the host's and the session's.
#[test]
#[ignore = "requires KDE, KWin, PipeWire, WirePlumber, bubblewrap, input devices and a viewer GPU"]
fn the_viewer_names_a_screencast_node_that_is_in_another_pipewire_daemon() {
    require_e2e();
    let fixture = Fixture::new("node");
    let home = fixture.home();
    let mut desk = Server::start(&home, &[], &["--autoclean"], &[]);
    let (_desk_id, desk_dir) = desk.session(1280, 800);
    let host_runtime = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");

    // `other` is the daemon the compositor finds under its own runtime
    // directory's name when it starts; `own` is started there afterwards.
    let other = fixture.0.join("other");
    let own = fixture.0.join("own");
    for directory in [&other, &own] {
        std::fs::create_dir_all(directory).expect("runtime directory");
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).expect("restrict runtime");
    }
    // The private session managers leave the tester's sound cards, cameras
    // and Bluetooth alone.
    let manager_config = home.join(".config/wireplumber/wireplumber.conf.d");
    std::fs::create_dir_all(&manager_config).expect("session manager configuration directory");
    std::fs::write(
        manager_config.join("50-no-hardware.conf"),
        "wireplumber.profiles = {\n  main = {\n    hardware.audio = disabled\n    hardware.bluetooth = disabled\n    hardware.video-capture = disabled\n  }\n}\n",
    )
    .expect("session manager configuration");
    let _other_pipewire = daemon("pipewire", &[], &home, &other);
    wait_for("the other PipeWire daemon", || other.join("pipewire-0").exists());
    let _other_manager = daemon("wireplumber", &[], &home, &other);
    std::os::unix::fs::symlink(other.join("pipewire-0"), own.join("pipewire-0")).expect("link the other daemon");
    let _compositor = {
        let mut command = Command::new("dbus-run-session");
        command
            .args(["--", "kwin_wayland", "--virtual", "--no-lockscreen", "--width", "800", "--height", "600", "--socket", "wayland-0"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_RUNTIME_DIR", &own)
            .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1")
            .env("KWIN_SCREENSHOT_NO_PERMISSION_CHECKS", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Kill::spawn(&mut command, "kwin_wayland")
    };
    wait_for("the compositor's Wayland socket", || own.join("wayland-0").exists());
    // The compositor connects to PipeWire as it starts.
    std::thread::sleep(Duration::from_secs(3));
    let _own_pipewire = daemon("pipewire", &[], &home, &own);
    wait_for("the session's own PipeWire daemon", || {
        std::fs::symlink_metadata(own.join("pipewire-0")).is_ok_and(|metadata| !metadata.is_symlink())
    });
    let _own_manager = daemon("wireplumber", &[], &home, &own);
    std::thread::sleep(Duration::from_secs(2));

    let started = Instant::now();
    let output = Command::new(viewer_binary())
        .arg(&own)
        .args(["800", "600"])
        .env("XDG_RUNTIME_DIR", &host_runtime)
        .env("WAYLAND_DISPLAY", desk_dir.join("wayland-0"))
        .stdin(Stdio::null())
        .output()
        .expect("run kwin-viewer");
    let took = started.elapsed();
    let log = String::from_utf8_lossy(&output.stderr);
    let status: Value = serde_json::from_str(
        &std::fs::read_to_string(own.join("viewer-status.json")).expect("viewer status"),
    )
    .expect("viewer status JSON");
    assert_eq!(status["state"], "failed", "{status}\n{log}");
    let detail = status["detail"].as_str().expect("failure detail");
    assert!(
        detail.contains("is not in the session's PipeWire daemon")
            && detail.contains("connected to another PipeWire daemon"),
        "the viewer did not name the cause: {detail}\n{log}"
    );
    assert!(!output.status.success(), "the viewer exited successfully\n{log}");
    assert!(
        took < Duration::from_secs(3),
        "the viewer waited {took:?} for a frame that cannot come\n{log}"
    );
}

/// A running session's scope unit and the directory of its cgroup.
fn session_scope(server: &mut Server, session: &str) -> (String, PathBuf) {
    let listed = server.tool("session_list", json!({}));
    let record = listed["result"]["structuredContent"]["sessions"]
        .as_array()
        .and_then(|sessions| sessions.iter().skip_while(|entry| entry["session_id"] != session).next())
        .unwrap_or_else(|| panic!("session is not listed: {listed}"))
        .clone();
    let unit = record["scope_unit"].as_str().expect("scope unit").to_owned();
    let sandbox = record["sandbox_pid"].as_u64().expect("sandbox pid");
    let group = std::fs::read_to_string(format!("/proc/{sandbox}/cgroup")).expect("sandbox cgroup");
    let group = group.lines().filter_map(|line| line.strip_prefix("0::")).next().expect("cgroup v2 path");
    let (parent, _) = group.rsplit_once(unit.as_str()).expect("scope in cgroup path");
    let directory = PathBuf::from(format!("/sys/fs/cgroup{parent}{unit}"));
    assert!(directory.join("cgroup.procs").exists(), "scope cgroup: {}", directory.display());
    (unit, directory)
}

// #922: session_stop returns after the session's scope is empty, or says
// which processes are still leaving it. A process that ignores SIGTERM makes
// the scope outlive the three seconds its stop is given, so it is killed; the
// scope must not still hold processes when session_stop has answered without
// naming them.
#[test]
#[ignore = "requires KDE, KWin, bubblewrap and input devices"]
fn session_stop_answers_after_the_scope_is_empty_or_names_what_is_left() {
    require_e2e();
    let fixture = Fixture::new("scope");
    let mut server = Server::start(&fixture.home(), &[], &["--autoclean", "--no-viewer"], &[]);
    let (session, _workdir) = server.session(800, 600);
    let (unit, directory) = session_scope(&mut server, &session);

    let launched = server.tool(
        "launch_app",
        json!({"session_id":session, "command":"bash -c 'trap \"\" TERM; while :; do sleep 1; done'", "expect_window":false}),
    );
    assert!(launched["error"].is_null(), "{launched}");
    let stopped = server.tool("session_stop", json!({"session_id":session}));
    let members = std::fs::read_to_string(directory.join("cgroup.procs")).unwrap_or_default();
    let content = &stopped["result"]["structuredContent"];
    assert_eq!(content["status"], "stopped", "{stopped}");
    let named = content["processes_exiting"]["processes"].as_array().map_or(0, Vec::len);
    assert!(
        members.trim().is_empty() || named > 0,
        "session_stop answered stopped with processes still in {unit} and named none: {members:?}\n{stopped}"
    );
}
