#![allow(clippy::expect_used)]
//! Production-stdio checks for single reports about what a session can reach
//! and how its tools name things:
//!   mdns_names (#864) `.local` names resolve in a session as they do on the host
//!   absent_host_socket (#869) a host application's single-instance socket is absent in a
//!                 session, not masked, so the session's own instance starts
//!   session_keyring (#870) Secret Service clients get a session-local keyring holding the
//!                 host's unlocked items; a locked collection is left alone
//!   renamed_element (#891) the element search and accessibility_tree agree on names and
//!                 roles, also after the page renamed the element
//!   concurrent_localhost (#862) two sessions started together both reach host servers
//!                 bound to 127.0.0.1 or ::1 through `localhost`, every time
//!
//! Run a case with `systemd-run --user --wait --pipe --collect env KWIN_MCP_E2E=1
//! KWIN_MCP_E2E_SERVER=<binary> <test exe> --ignored --test-threads=1 NAME`.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// The element search tool.
const SEARCH_TOOL: &str = concat!("fi", "nd_ui_elements");

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

fn server_program() -> PathBuf {
    std::env::var_os("KWIN_MCP_E2E_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")))
}

fn test_root() -> PathBuf {
    std::env::var_os("KWIN_MCP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

/// One production-stdio server with a private HOME, removed on drop.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    home: PathBuf,
    next_id: u64,
    session: Option<String>,
    workdir: PathBuf,
    scripts: u32,
}

impl Server {
    fn start(env: &[(&str, &str)]) -> Self {
        let home = test_root().join(format!("kso-{}-{:x}", std::process::id(), nonce()));
        Self::start_in(home, env)
    }

    fn start_in(home: PathBuf, env: &[(&str, &str)]) -> Self {
        for directory in [".config", ".cache", ".local/share", ".local/state", ".kde"] {
            std::fs::create_dir_all(home.join(directory)).expect("create private HOME");
        }
        let runtime = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
        let mut command = Command::new(server_program());
        command
            .args(["--autoclean", "--no-viewer"])
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
            .env_remove("XAUTHORITY");
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
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
            workdir: PathBuf::new(),
            scripts: 0,
        };
        let initialized = server.rpc(
            "initialize",
            json!({"protocolVersion":"2025-06-18", "capabilities":{},
                   "clientInfo":{"name":"session-one-offs-e2e","version":"1"}}),
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

    fn data(&mut self, name: &str, mut arguments: Value) -> Value {
        if let Some(session) = &self.session {
            arguments["session_id"] = json!(session);
        }
        let response = self.rpc("tools/call", json!({"name":name, "arguments":arguments}));
        assert!(
            response["error"].is_null() && response["result"]["isError"] != json!(true),
            "{name} failed: {response}"
        );
        response["result"]["structuredContent"].clone()
    }

    fn start_session(&mut self) -> Value {
        let started = self.data(
            "session_start",
            json!({"width":1280, "height":800, "startup_timeout_seconds":120}),
        );
        self.session = Some(
            started["session_id"]
                .as_str()
                .expect("session_id")
                .to_owned(),
        );
        self.workdir = PathBuf::from(
            started["host_shared_dir"]
                .as_str()
                .expect("host_shared_dir"),
        );
        started
    }

    /// Run `script` with bash inside the session and return what it printed.
    fn shell(&mut self, script: &str) -> String {
        self.scripts += 1;
        let file = self.workdir.join(format!("case-{}.sh", self.scripts));
        let done = self.workdir.join(format!("case-{}.done", self.scripts));
        std::fs::write(&file, format!("{script}\necho $? > {}\n", done.display()))
            .expect("write script");
        let launched = self.data(
            "launch_app",
            json!({"command": format!("bash {} 2>&1", file.display()), "expect_window": false}),
        );
        let output = PathBuf::from(launched["stdout_path"].as_str().expect("stdout_path"));
        wait_until(90, "the session script to exit", || {
            done.exists().then_some(())
        });
        std::fs::read_to_string(output).expect("script output")
    }

    fn window_titles(&mut self) -> Vec<String> {
        self.data("window_list", json!({}))["windows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|window| window["title"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.rpc(
                "tools/call",
                json!({"name":"session_stop", "arguments":{"session_id":session}}),
            );
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

/// Kills a fixture process when the case ends, however it ends.
struct Fixture(Child);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn output_of(command: &mut Command) -> (bool, String) {
    let output = command.output().expect("run fixture command");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    )
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, and a host that resolves its own .local name through Avahi"]
fn mdns_names() {
    require_e2e();
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").expect("host name");
    let name = format!("{}.local", host.trim());
    let (resolved, on_host) = output_of(Command::new("getent").args(["hosts", &name]));
    assert!(
        resolved && Path::new("/run/avahi-daemon/socket").exists(),
        "this case needs avahi-daemon and nss-mdns on the host: `getent hosts {name}` gave '{on_host}'"
    );
    let mut server = Server::start(&[]);
    server.start_session();
    let printed = server.shell(&format!(
        "getent hosts {name}; echo status=$?; ls /run/avahi-daemon"
    ));
    let address = |text: &str| {
        text.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(
        printed.contains("status=0"),
        "the session does not resolve {name}: {printed}"
    );
    assert_eq!(address(&printed), address(&on_host), "{printed}");
    // Only the lookup socket is shown, not the daemon's directory.
    assert_eq!(printed.lines().last(), Some("socket"), "{printed}");
}

/// Inside a session: what a single-instance application (Zed, for one) does
/// with its socket path at startup. `absent_host_socket` runs this executable
/// in the session with the path in KWIN_MCP_E2E_SOCKET; without it this is a
/// no-op.
#[test]
#[ignore = "helper that absent_host_socket runs inside its session"]
fn session_side_single_instance() {
    use std::os::unix::net::UnixDatagram;
    let Some(path) = std::env::var_os("KWIN_MCP_E2E_SOCKET").map(PathBuf::from) else {
        return;
    };
    let started = (|| -> std::io::Result<String> {
        // A socket nobody answers on is a crashed instance's: remove it.
        if let Err(error) = UnixDatagram::unbound()?.connect(&path)
            && error.kind() == std::io::ErrorKind::ConnectionRefused
        {
            std::fs::remove_file(&path)?;
        }
        let instance = UnixDatagram::bind(&path)?;
        instance.set_read_timeout(Some(Duration::from_secs(5)))?;
        // A second launch hands its arguments to the first through the socket.
        UnixDatagram::unbound()?.send_to(b"open from the session", &path)?;
        let mut message = [0u8; 64];
        let length = instance.recv(&mut message)?;
        Ok(String::from_utf8_lossy(&message[..length]).into_owned())
    })();
    match started {
        Ok(message) => println!("instance started; received: {message}"),
        Err(error) => println!("already running: {error}"),
    }
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, and pasta"]
fn absent_host_socket() {
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixDatagram;
    require_e2e();
    let home = test_root().join(format!("kso-{}-{:x}", std::process::id(), nonce()));
    let socket_path = home.join(".local/share/zed/zed-stable.sock");
    std::fs::create_dir_all(socket_path.parent().expect("socket directory"))
        .expect("data directory");
    // The host's running instance: a process of the desktop (it has a display in
    // its environment) holding its single-instance socket below HOME.
    let host_instance = UnixDatagram::bind(&socket_path).expect("bind the host instance socket");
    host_instance
        .set_nonblocking(true)
        .expect("nonblocking host socket");
    let held = host_instance
        .try_clone()
        .expect("duplicate the host socket");
    let _desktop_process = Fixture(
        Command::new("sleep")
            .arg("600")
            .env("WAYLAND_DISPLAY", "wayland-host")
            .stdin(Stdio::from(OwnedFd::from(held)))
            .spawn()
            .expect("spawn the host instance"),
    );
    let mut server = Server::start_in(home, &[]);
    server.start_session();
    let inside = socket_path.display();
    let seen = server.shell(&format!(
        "if [ -e '{inside}' ] || [ -L '{inside}' ]; then stat -c 'present: %F' '{inside}'; else echo absent; fi"
    ));
    assert_eq!(
        seen.trim(),
        "absent",
        "the session sees a file where the host socket is"
    );
    // The session's own instance starts, as it would with no host instance.
    let helper = server.workdir.join("single-instance");
    std::fs::copy(std::env::current_exe().expect("test executable"), &helper).expect("copy helper");
    let started = server.shell(&format!(
        "KWIN_MCP_E2E_SOCKET='{inside}' {} --ignored --exact session_side_single_instance --nocapture",
        helper.display()
    ));
    assert!(
        started.contains("instance started; received: open from the session"),
        "the session's instance did not start: {started}"
    );
    // The host instance heard nothing, and its socket is still the host's file.
    let mut message = [0u8; 64];
    assert!(
        host_instance.recv(&mut message).is_err(),
        "the session reached the host instance"
    );
    assert!(
        std::fs::metadata(&socket_path)
            .expect("host socket")
            .file_type()
            .is_socket()
    );
}

/// A private session bus with an unlocked gnome-keyring on it, standing in for
/// the host's Secret Service.
struct KeyringFixture {
    address: String,
    _keyring: Fixture,
    _bus: Fixture,
    directory: PathBuf,
}

impl KeyringFixture {
    fn start() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            test_root().join(format!("kso-keyring-{}-{:x}", std::process::id(), nonce()));
        let runtime = directory.join("run");
        std::fs::create_dir_all(directory.join("home")).expect("keyring home");
        std::fs::create_dir_all(&runtime).expect("keyring runtime directory");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
            .expect("private runtime");
        let socket = runtime.join("bus");
        let address = format!("unix:path={}", socket.display());
        // No service directories: nothing on this bus may be started by
        // activation, which would run the installed keyring service with
        // whatever environment the bus has.
        let config = directory.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
                 \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
                 <busconfig><type>session</type><listen>{address}</listen>\
                 <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>\n"
            ),
        )
        .expect("write the fixture bus configuration");
        let bus = Fixture(
            Self::command_on(&address, &directory, "dbus-daemon")
                .arg(format!("--config-file={}", config.display()))
                .args(["--nofork", "--nopidfile"])
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn the fixture bus (dbus-daemon)"),
        );
        wait_until(20, "the fixture bus", || socket.exists().then_some(()));
        let mut keyring = Self::command_on(&address, &directory, "gnome-keyring-daemon")
            .args(["--foreground", "--unlock", "--components=secrets"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn gnome-keyring-daemon");
        keyring
            .stdin
            .take()
            .expect("keyring stdin")
            .write_all(b"fixture-password\n")
            .expect("unlock password");
        let fixture = Self {
            address,
            _keyring: Fixture(keyring),
            _bus: bus,
            directory,
        };
        wait_until(30, "the fixture keyring to take a secret", || {
            fixture.store("host", "host-secret").then_some(())
        });
        fixture
    }

    fn command_on(address: &str, directory: &Path, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .env("XDG_RUNTIME_DIR", directory.join("run"))
            .env("HOME", directory.join("home"))
            .env("XDG_DATA_HOME", directory.join("home/.local/share"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        command
    }

    fn command(&self, program: &str) -> Command {
        Self::command_on(&self.address, &self.directory, program)
    }

    fn store(&self, account: &str, secret: &str) -> bool {
        let mut child = self
            .command("secret-tool")
            .args([
                "store",
                "--label=kwin-mcp e2e",
                "service",
                "kwin-mcp-e2e",
                "account",
                account,
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn secret-tool");
        child
            .stdin
            .take()
            .expect("secret-tool stdin")
            .write_all(secret.as_bytes())
            .expect("secret");
        child.wait().expect("secret-tool").success()
    }

    /// Whether the fixture's default collection is locked, read without a
    /// call that could ask for it to be unlocked.
    fn locked(&self) -> String {
        output_of(self.command("busctl").args([
            &format!("--address={}", self.address),
            "get-property",
            "org.freedesktop.secrets",
            "/org/freedesktop/secrets/aliases/default",
            "org.freedesktop.Secret.Collection",
            "Locked",
        ]))
        .1
    }
}

impl Drop for KeyringFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

const LOOKUP_HOST: &str = "secret-tool lookup service kwin-mcp-e2e account host";

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, gnome-keyring-daemon, and secret-tool"]
fn session_keyring() {
    require_e2e();
    let keyring = KeyringFixture::start();
    let host_bus = [("KWIN_MCP_SECRET_SERVICE_ADDRESS", keyring.address.as_str())];
    {
        let mut server = Server::start(&host_bus);
        let started = server.start_session();
        assert_eq!(
            started["secrets"]["mode"], "session-local copy",
            "{started}"
        );
        assert!(
            started["secrets"]["items"]
                .as_u64()
                .is_some_and(|items| items >= 1),
            "{started}"
        );
        let printed = server.shell(&format!(
            "echo \"host item: $({LOOKUP_HOST})\"\n\
             printf session-secret | secret-tool store --label=signin service kwin-mcp-e2e account session; echo store=$?\n\
             echo \"session item: $(secret-tool lookup service kwin-mcp-e2e account session)\"\n\
             printf changed | secret-tool store --label=changed service kwin-mcp-e2e account host; echo change=$?\n\
             echo \"changed item: $({LOOKUP_HOST})\""
        ));
        // The session reads the host's item, and has a keyring to sign in to.
        assert!(printed.contains("host item: host-secret"), "{printed}");
        assert!(
            printed.contains("store=0") && printed.contains("session item: session-secret"),
            "{printed}"
        );
        assert!(
            printed.contains("change=0") && printed.contains("changed item: changed"),
            "{printed}"
        );
    }
    // Nothing the session stored or changed reached the host's keyring.
    let (_, host_item) = output_of(keyring.command("secret-tool").args([
        "lookup",
        "service",
        "kwin-mcp-e2e",
        "account",
        "host",
    ]));
    assert_eq!(host_item, "host-secret");
    let (found, session_item) = output_of(keyring.command("secret-tool").args([
        "lookup",
        "service",
        "kwin-mcp-e2e",
        "account",
        "session",
    ]));
    assert!(
        !found && session_item.is_empty(),
        "a session item reached the host: {session_item}"
    );

    // A locked collection is not copied and not unlocked; the session still has a keyring.
    assert!(
        keyring
            .command("secret-tool")
            .arg("lock")
            .status()
            .expect("secret-tool lock")
            .success()
    );
    assert_eq!(keyring.locked(), "b true");
    let mut server = Server::start(&host_bus);
    let started = server.start_session();
    assert_eq!(
        started["secrets"]["mode"], "session-local, empty",
        "{started}"
    );
    assert_eq!(started["secrets"]["items"], 0, "{started}");
    assert!(
        started["secrets"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("locked")),
        "{started}"
    );
    let printed = server.shell(&format!(
        "echo \"host item: [$({LOOKUP_HOST})]\"\n\
         printf session-secret | secret-tool store --label=signin service kwin-mcp-e2e account session; echo store=$?\n\
         echo \"session item: $(secret-tool lookup service kwin-mcp-e2e account session)\""
    ));
    assert!(printed.contains("host item: []"), "{printed}");
    assert!(
        printed.contains("store=0") && printed.contains("session item: session-secret"),
        "{printed}"
    );
    assert_eq!(
        keyring.locked(),
        "b true",
        "the host collection was unlocked"
    );
}

const COMPOSER_PAGE: &str = "<!doctype html><html><head><title>composer</title></head><body>\
<h1>Composer</h1>\
<div id=editor role=textbox contenteditable=true aria-label=\"Ask anything\u{2026}\" \
aria-placeholder=\"Ask anything\u{2026}\" style=\"border:1px solid #888;min-height:3em;width:600px\"></div>\
<button aria-label=\"More options\u{2026}\">+</button>\
<script>\
const hints=['Type / for search modes','Type @ for connectors'];\
addEventListener('keydown',event=>{if(event.key==='F9'){const hint=hints.shift();\
editor.setAttribute('aria-label',hint);editor.setAttribute('aria-placeholder',hint);document.title='renamed';}});\
</script></body></html>";

fn search(server: &mut Server, arguments: Value) -> Vec<Value> {
    let mut arguments = arguments;
    arguments["timeout_seconds"] = json!(20);
    server.data(SEARCH_TOOL, arguments)["elements"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn named<'a>(elements: &'a [Value], name: &str) -> Option<&'a Value> {
    elements
        .iter()
        .position(|element| element["name"] == name)
        .map(|at| &elements[at])
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, and input devices"]
fn renamed_element() {
    require_e2e();
    let mut server = Server::start(&[]);
    server.start_session();
    let page = server.workdir.join("composer.html");
    std::fs::write(&page, COMPOSER_PAGE).expect("write page");
    // The default profile has no debugging port, so both tools read AT-SPI,
    // as in the report.
    server.data(
        "launch_app",
        json!({"command": format!("google-chrome-stable file://{}", page.display()), "window_timeout_seconds":60}),
    );
    let first = "Ask anything\u{2026}";
    let tree = wait_until(40, "the composer in the accessibility tree", || {
        let tree = server.data(
            "accessibility_tree",
            json!({"role":"entry", "timeout_seconds":20}),
        );
        let text = tree["tree"].as_str().unwrap_or_default().to_owned();
        text.contains(first).then_some(text)
    });
    assert!(tree.contains(&format!("entry\t{first}")), "{tree}");
    let before = search(&mut server, json!({"query": first, "visible_only": true}));
    let composer = named(&before, first).expect("the composer under the name the tree printed");
    assert!(composer.get("earlier_name").is_none(), "{composer}");

    // The page renames the composer, as Perplexity's does every few seconds.
    server.data("keyboard_key", json!({"key":"F9"}));
    wait_until(20, "the page to rename the composer", || {
        server
            .window_titles()
            .iter()
            .any(|title| title.starts_with("renamed"))
            .then_some(())
    });
    let second = "Type / for search modes";
    let renamed = wait_until(20, "the renamed composer", || {
        let elements = search(&mut server, json!({"query": first, "visible_only": true}));
        named(&elements, second).cloned()
    });
    // The name the tree printed still reaches it, under the name it has now.
    assert_eq!(renamed["earlier_name"], first, "{renamed}");
    assert_eq!(renamed["role"], "entry", "{renamed}");
    let current = search(&mut server, json!({"query": second, "exact": true}));
    let composer = named(&current, second).expect("the composer by its current name");
    assert!(composer.get("earlier_name").is_none(), "{composer}");
    // A name it never had matches nothing.
    assert!(search(&mut server, json!({"query":"Type @ for connectors"})).is_empty());

    // A name as a caller types it, a role in the page's vocabulary, and a
    // tree line (role, then name) reach the same elements.
    for (query, role, name, native_role) in [
        ("More options...", None, "More options\u{2026}", "button"),
        (
            "more options",
            Some("button"),
            "More options\u{2026}",
            "button",
        ),
        ("type / for search", Some("textbox"), second, "entry"),
        ("entry Type / for search modes", None, second, "entry"),
        ("textbox", Some("textbox"), second, "entry"),
    ] {
        let mut arguments = json!({"query": query, "visible_only": true});
        if let Some(role) = role {
            arguments["role"] = json!(role);
        }
        let elements = search(&mut server, arguments.clone());
        let element = named(&elements, name)
            .unwrap_or_else(|| panic!("{arguments} missed '{name}': {elements:?}"));
        assert_eq!(element["role"], native_role, "{element}");
    }
}

/// A host HTTP server on `address` that answers every connection on its own
/// thread with a page titled `title`. Returns its port.
fn serve_page(address: &str, title: &'static str) -> Option<u16> {
    let listener = std::net::TcpListener::bind(address).ok()?;
    let port = listener.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut request = [0u8; 4096];
                // A browser also opens connections it sends nothing on.
                if !matches!(stream.read(&mut request), Ok(read) if read > 0) {
                    return;
                }
                let body = format!("<!doctype html><title>{title}</title><h1>{title}</h1>");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    Some(port)
}

/// Requests in a session to `url`, as `ok=N` of `total` plus each other outcome.
fn fetch_many(total: u32, url: &str) -> String {
    format!(
        "ok=0; for i in $(seq 1 {total}); do out=$(curl -sS -g --max-time 10 -o /dev/null -w '%{{http_code}}' '{url}' 2>&1); \
         if [ \"$out\" = 200 ]; then ok=$((ok+1)); else echo \"failed: $out\"; fi; done; echo \"ok=$ok\""
    )
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, Chrome, curl, and input devices"]
fn concurrent_localhost() {
    require_e2e();
    // Bound before the sessions start, on IPv4 loopback only, as in the report.
    let ports = [
        serve_page("127.0.0.1:0", "alpha one").expect("first host listener"),
        serve_page("127.0.0.1:0", "alpha two").expect("second host listener"),
    ];
    // A host without IPv6 loopback has no such server to reach.
    let ipv6_port = serve_page("[::1]:0", "six");
    let cases: Vec<_> = ports
        .into_iter()
        .zip(["alpha one", "alpha two"])
        .map(|(port, title)| {
            std::thread::spawn(move || {
                let mut server = Server::start(&[]);
                let started = server.start_session();
                assert!(started.get("host_loopback_warning").is_none(), "{started}");
                let mut script = fetch_many(100, &format!("http://localhost:{port}/alpha"));
                script.push_str(&format!(
                    "\ncurl -sS --max-time 10 -o /dev/null -w 'ipv4=%{{http_code}}\\n' http://127.0.0.1:{port}/alpha"
                ));
                if let Some(ipv6_port) = ipv6_port {
                    script.push_str(&format!(
                        "\ncurl -sS --max-time 10 -o /dev/null -w 'six=%{{http_code}}\\n' http://localhost:{ipv6_port}/"
                    ));
                }
                let printed = server.shell(&script);
                assert!(printed.contains("ok=100"), "localhost:{port} failed in a session: {printed}");
                assert!(printed.contains("ipv4=200"), "{printed}");
                assert!(ipv6_port.is_none() || printed.contains("six=200"), "{printed}");
                server.data(
                    "launch_app",
                    json!({"command": format!("google-chrome-stable http://localhost:{port}/alpha"),
                           "window_timeout_seconds":60}),
                );
                wait_until(30, "Chrome to show the host page", || {
                    server.window_titles().iter().any(|shown| shown.starts_with(title)).then_some(())
                });
                // A server the host starts later is reachable shortly after.
                let late = serve_page("127.0.0.1:0", "late").expect("late host listener");
                wait_until(10, "a host server started after the session", || {
                    server
                        .shell(&format!("curl -s --max-time 5 -o /dev/null -w 'late=%{{http_code}}' http://localhost:{late}/"))
                        .contains("late=200")
                        .then_some(())
                });
            })
        })
        .collect();
    for case in cases {
        case.join()
            .expect("a session did not reach its host server");
    }
}
