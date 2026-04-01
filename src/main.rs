use mcpkit::prelude::*;
use mcpkit::transport::stdio::StdioTransport;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// Claude Code serializes numbers to strings — this handles both formats
fn parse_int(v: serde_json::Value) -> Result<i32, McpError> {
    match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(val) => i32::try_from(val).map_err(|e| McpError::invalid_params("coord", e.to_string())),
            None => Err(McpError::invalid_params("coord", "not an integer")),
        },
        serde_json::Value::String(s) => s.parse::<i32>().map_err(|e| McpError::invalid_params("coord", e.to_string())),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Array(_) | serde_json::Value::Object(_) =>
            Err(McpError::invalid_params("coord", "expected integer")),
    }
}

// ── Evdev keycodes ───────────────────────────────────────────────────────

use keyboard_codes::{KeyCodeMapper, Platform};

fn char_key(ch: char) -> Option<(u32, bool)> {
    match ch {
        'a'..='z' | '0'..='9' | '`' | '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/' | ' ' | '\t' | '\n' => {
            let input: keyboard_codes::KeyboardInput = String::from(ch).parse().ok()?;
            Some((u32::try_from(input.to_code(Platform::Linux)).ok()?, false))
        }
        'A'..='Z' => {
            let input: keyboard_codes::KeyboardInput = String::from(ch.to_ascii_lowercase()).parse().ok()?;
            Some((u32::try_from(input.to_code(Platform::Linux)).ok()?, true))
        }
        '~' | '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '(' | ')' | '_' | '+' |
        '{' | '}' | '|' | ':' | '"' | '<' | '>' | '?' => {
            let unshifted = match ch {
                '~' => '`', '!' => '1', '@' => '2', '#' => '3', '$' => '4', '%' => '5',
                '^' => '6', '&' => '7', '*' => '8', '(' => '9', ')' => '0', '_' => '-',
                '+' => '=', '{' => '[', '}' => ']', '|' => '\\', ':' => ';', '"' => '\'',
                '<' => ',', '>' => '.', '?' => '/',
                _ => return None,
            };
            let input: keyboard_codes::KeyboardInput = String::from(unshifted).parse().ok()?;
            Some((u32::try_from(input.to_code(Platform::Linux)).ok()?, true))
        }
        _ => None,
    }
}

fn parse_combo(key: &str) -> (Vec<u32>, Option<u32>) {
    match keyboard_codes::parser::parse_shortcut_with_aliases(key) {
        Ok(shortcut) => {
            let mods: Vec<u32> = shortcut.modifiers.iter()
                .filter_map(|m| u32::try_from(keyboard_codes::KeyboardInput::Modifier(*m).to_code(Platform::Linux)).ok())
                .collect();
            let main = u32::try_from(shortcut.key.to_code(Platform::Linux)).ok();
            (mods, main)
        }
        Err(_) => {
            // Fallback: try single char
            match key.chars().next() { Some(ch) => (Vec::new(), char_key(ch).map(|(k, _)| k)), None => (Vec::new(), None) }
        }
    }
}

fn btn_code(btn: Option<&str>) -> Result<u32, McpError> {
    match btn {
        Some("left") | None => Ok(0x110),
        Some("right") => Ok(0x111),
        Some("middle") => Ok(0x112),
        Some(bad) => Err(McpError::invalid_params("button", format!("unknown button '{bad}' — use left/right/middle"))),
    }
}

fn write_kde_config(dir: &str, file: &str, entries: &[(&str, &str, &str)]) -> anyhow::Result<()> {
    let mut out = String::new();
    let mut current_group = "";
    for &(group, key, value) in entries {
        if group != current_group { out.push_str(&format!("[{group}]\n")); current_group = group; }
        out.push_str(&format!("{key}={value}\n"));
    }
    std::fs::write(format!("{dir}/{file}"), out)?;
    Ok(())
}

fn detect_xdisplay(before: &std::collections::HashMap<String, u64>, deadline: std::time::Instant) -> anyhow::Result<String> {
    for entry in std::fs::read_dir("/tmp/.X11-unix").into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let ino = std::os::unix::fs::MetadataExt::ino(&entry.metadata().map_err(|e| anyhow::anyhow!("{e}"))?);
        if before.get(&name).is_none_or(|old_ino| *old_ino != ino) {
            return Ok(format!(":{}", name.strip_prefix('X').unwrap_or(&name)));
        }
    }
    anyhow::ensure!(std::time::Instant::now() < deadline, "XWayland display did not appear");
    std::thread::sleep(std::time::Duration::from_millis(100));
    detect_xdisplay(before, deadline)
}

fn snapshot_x_sockets() -> std::collections::HashMap<String, u64> {
    std::fs::read_dir("/tmp/.X11-unix").into_iter().flatten().flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let ino = std::os::unix::fs::MetadataExt::ino(&e.metadata().ok()?);
            Some((name, ino))
        }).collect()
}

fn eis_devices_ready(dev: &Option<reis::ei::Device>, kb: &Option<reis::ei::Keyboard>) -> bool {
    matches!((dev, kb), (Some(_), Some(_)))
}

// ── Recursive helpers ────────────────────────────────────────────────────

fn wait_for_path(path: &str, deadline: std::time::Instant) -> anyhow::Result<()> {
    match std::path::Path::new(path).exists() {
        true => Ok(()),
        false => match std::time::Instant::now() > deadline {
            true => anyhow::bail!("socket {path} did not appear"),
            false => { std::thread::sleep(std::time::Duration::from_millis(100)); wait_for_path(path, deadline) }
        },
    }
}

#[expect(clippy::too_many_arguments)]
fn negotiate_eis(
    context: &reis::ei::Context, conv: &mut reis::event::EiEventConverter, serial: u32,
    deadline: std::time::Instant,
    dev: &mut Option<reis::ei::Device>, kbd_dev: &mut Option<reis::ei::Device>,
    abs: &mut Option<reis::ei::PointerAbsolute>,
    bt: &mut Option<reis::ei::Button>, sc: &mut Option<reis::ei::Scroll>, kb: &mut Option<reis::ei::Keyboard>,
) -> anyhow::Result<()> {
    if std::time::Instant::now() > deadline { return Ok(()); }
    context.read()?;
    drain_eis_pending(context, conv)?;
    drain_eis_events(context, conv, serial, dev, kbd_dev, abs, bt, sc, kb)?;
    match eis_devices_ready(dev, kb) {
        true => Ok(()),
        false => { std::thread::sleep(std::time::Duration::from_millis(100));
            negotiate_eis(context, conv, serial, deadline, dev, kbd_dev, abs, bt, sc, kb) }
    }
}

fn drain_eis_pending(context: &reis::ei::Context, conv: &mut reis::event::EiEventConverter) -> anyhow::Result<()> {
    match context.pending_event() {
        Some(reis::PendingRequestResult::Request(ev)) => {
            conv.handle_event(ev).map_err(|e| anyhow::anyhow!("eis handle_event: {e:?}"))?;
            drain_eis_pending(context, conv)
        }
        Some(reis::PendingRequestResult::ParseError(e)) => anyhow::bail!("EIS parse: {e}"),
        Some(reis::PendingRequestResult::InvalidObject(i)) => anyhow::bail!("EIS invalid: {i}"),
        None => Ok(()),
    }
}

#[expect(clippy::too_many_arguments)]
fn drain_eis_events(
    context: &reis::ei::Context, conv: &mut reis::event::EiEventConverter, serial: u32,
    dev: &mut Option<reis::ei::Device>, kbd_dev: &mut Option<reis::ei::Device>,
    abs: &mut Option<reis::ei::PointerAbsolute>,
    bt: &mut Option<reis::ei::Button>, sc: &mut Option<reis::ei::Scroll>, kb: &mut Option<reis::ei::Keyboard>,
) -> anyhow::Result<()> {
    match conv.next_event() {
        Some(reis::event::EiEvent::SeatAdded(sa)) => {
            sa.seat.bind_capabilities(&[reis::event::DeviceCapability::Pointer,
                reis::event::DeviceCapability::PointerAbsolute, reis::event::DeviceCapability::Button,
                reis::event::DeviceCapability::Scroll, reis::event::DeviceCapability::Keyboard]);
            context.flush()?;
            drain_eis_events(context, conv, serial, dev, kbd_dev, abs, bt, sc, kb)
        }
        Some(reis::event::EiEvent::DeviceAdded(da)) => {
            register_eis_device(&da, serial, dev, kbd_dev, abs, bt, sc, kb);
            context.flush()?;
            drain_eis_events(context, conv, serial, dev, kbd_dev, abs, bt, sc, kb)
        }
        Some(reis::event::EiEvent::Disconnected(dc)) => { eprintln!("eis: disconnected {:?}", dc); anyhow::bail!("EIS disconnected") }
        Some(other) => { eprintln!("eis: event {:?}", std::mem::discriminant(&other)); drain_eis_events(context, conv, serial, dev, kbd_dev, abs, bt, sc, kb) }
        None => Ok(()),
    }
}

fn register_eis_device(
    da: &reis::event::DeviceAdded, serial: u32,
    dev: &mut Option<reis::ei::Device>, kbd_dev: &mut Option<reis::ei::Device>,
    abs: &mut Option<reis::ei::PointerAbsolute>,
    bt: &mut Option<reis::ei::Button>, sc: &mut Option<reis::ei::Scroll>, kb: &mut Option<reis::ei::Keyboard>,
) {
    let has_ptr = da.device.has_capability(reis::event::DeviceCapability::PointerAbsolute);
    let has_kbd = da.device.has_capability(reis::event::DeviceCapability::Keyboard);
    // grab pointer device
    match (has_ptr, &dev) {
        (true, None) => {
            da.device.device().start_emulating(serial, 0);
            *abs = da.device.interface::<reis::ei::PointerAbsolute>();
            *bt = da.device.interface::<reis::ei::Button>();
            *sc = da.device.interface::<reis::ei::Scroll>();
            *dev = Some(da.device.device().clone());
            match (da.device.interface::<reis::ei::Keyboard>(), &kb) {
                (Some(k), None) => { *kb = Some(k); *kbd_dev = Some(da.device.device().clone()); }
                (Some(_), Some(_)) | (None, Some(_)) | (None, None) => eprintln!("eis: ptr dev registered"),
            }
        }
        (true, Some(_)) | (false, Some(_)) | (false, None) => {
            match (has_kbd, &kb) {
                (true, None) => {
                    da.device.device().start_emulating(serial, 0);
                    *kb = da.device.interface::<reis::ei::Keyboard>();
                    *kbd_dev = Some(da.device.device().clone());
                }
                (true, Some(_)) => eprintln!("eis: kbd already registered"),
                (false, Some(_)) | (false, None) => eprintln!("eis: skipping device"),
            }
        }
    }
}

// ── EIS Input ────────────────────────────────────────────────────────────

struct Eis {
    context: reis::ei::Context,
    abs_ptr: reis::ei::PointerAbsolute,
    btn: reis::ei::Button,
    scroll: reis::ei::Scroll,
    kbd: reis::ei::Keyboard,
    ptr_dev: reis::ei::Device,
    kbd_dev: reis::ei::Device,
    serial: u32,
    #[expect(dead_code)] // held alive for EIS fd lifetime
    conn: Box<dbus::blocking::Connection>,
}

impl Eis {
    #[expect(clippy::wildcard_enum_match_arm)]
    fn connect(dbus_addr: &str) -> anyhow::Result<Self> {
        let mut ch = dbus::channel::Channel::open_private(dbus_addr).map_err(|e| anyhow::anyhow!("dbus: {e}"))?;
        ch.register().map_err(|e| anyhow::anyhow!("dbus reg: {e}"))?;
        let conn = dbus::blocking::Connection::from(ch);
        let proxy = conn.with_proxy("org.kde.KWin", "/org/kde/KWin/EIS/RemoteDesktop", std::time::Duration::from_secs(5));
        let (fd, _): (dbus::arg::OwnedFd, i32) = proxy.method_call("org.kde.KWin.EIS.RemoteDesktop", "connectToEIS", (63i32,))
            .map_err(|e| anyhow::anyhow!("connectToEIS: {e}"))?;
        let stream = UnixStream::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd.into_raw_fd()) });
        let context = reis::ei::Context::new(stream)?;
        let resp = reis::handshake::ei_handshake_blocking(&context, "kwin-mcp", reis::ei::handshake::ContextType::Sender)
            .map_err(|e| anyhow::anyhow!("handshake: {e:?}"))?;
        context.flush()?;
        let mut conv = reis::event::EiEventConverter::new(&context, resp);
        let serial = conv.connection().serial();
        let (mut dev, mut kbd_d, mut abs, mut bt, mut sc, mut kb) = (None, None, None, None, None, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        negotiate_eis(&context, &mut conv, serial, deadline, &mut dev, &mut kbd_d, &mut abs, &mut bt, &mut sc, &mut kb)?;
        anyhow::ensure!(eis_devices_ready(&dev, &kb), "EIS negotiation timed out");
        let ptr_dev = dev.ok_or_else(|| anyhow::anyhow!("no ptr dev"))?;
        let kbd_dev = kbd_d.ok_or_else(|| anyhow::anyhow!("no kbd dev"))?;
        Ok(Self {
            context, abs_ptr: abs.ok_or_else(|| anyhow::anyhow!("no abs"))?,
            btn: bt.ok_or_else(|| anyhow::anyhow!("no btn"))?, scroll: sc.ok_or_else(|| anyhow::anyhow!("no scroll"))?,
            kbd: kb.ok_or_else(|| anyhow::anyhow!("no kbd"))?, ptr_dev, kbd_dev,
            serial, conn: Box::new(conn),
        })
    }

    fn move_abs(&self, x: i32, y: i32) -> anyhow::Result<()> {
        self.abs_ptr.motion_absolute(f32::from(i16::try_from(x)?), f32::from(i16::try_from(y)?));
        self.ptr_dev.frame(self.serial, 0); Ok(self.context.flush()?)
    }
    fn button(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed { true => reis::ei::button::ButtonState::Press, false => reis::ei::button::ButtonState::Released };
        self.btn.button(code, st); self.ptr_dev.frame(self.serial, 0); Ok(self.context.flush()?)
    }
    fn scroll_do(&self, dx: i32, dy: i32, discrete: bool) -> anyhow::Result<()> {
        match discrete { true => self.scroll.scroll_discrete(dx, dy), false => self.scroll.scroll(f32::from(i16::try_from(dx)?), f32::from(i16::try_from(dy)?)) }
        self.scroll.scroll_stop(0, 0, 0); self.ptr_dev.frame(self.serial, 0); Ok(self.context.flush()?)
    }
    fn key(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed { true => reis::ei::keyboard::KeyState::Press, false => reis::ei::keyboard::KeyState::Released };
        self.kbd.key(code, st); self.kbd_dev.frame(self.serial, 0); Ok(self.context.flush()?)
    }
}

// ── Session ──────────────────────────────────────────────────────────────

struct Session { dbus_daemon: dbus_launch::Daemon, dbus_addr: String, a11y_addr: String, child_pid: u32, scrdir: PathBuf, socket: String, xdisplay: String, width: u32, height: u32, eis: Eis }

// ── Server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct KwinMcp { session: Arc<Mutex<Option<Session>>> }

impl KwinMcp {
    fn new() -> Self { Self { session: Arc::new(Mutex::new(None)) } }
    fn with_session<R>(&self, f: impl FnOnce(&Session) -> Result<R, McpError>) -> Result<R, McpError> {
        let guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
        match &*guard { Some(s) => f(s), None => Err(McpError::internal("no session — call session_start first")) }
    }
}

fn eis_err(e: impl std::fmt::Display) -> McpError { McpError::internal(e.to_string()) }

fn kill_bus_clients(dbus_addr: &str) {
    let Ok(mut ch) = dbus::channel::Channel::open_private(dbus_addr) else { return };
    if ch.register().is_err() { return; }
    let conn = dbus::blocking::Connection::from(ch);
    let proxy = conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", std::time::Duration::from_secs(2));
    let names: Result<(Vec<String>,), _> = proxy.method_call("org.freedesktop.DBus", "ListNames", ());
    let Ok((names,)) = names else { return };
    let my_pid = std::process::id();
    for name in &names {
        if name.starts_with(':') {
            let pid: Result<(u32,), _> = proxy.method_call("org.freedesktop.DBus", "GetConnectionUnixProcessID", (name.as_str(),));
            if let Ok((pid,)) = pid && pid != my_pid && let Ok(p) = i32::try_from(pid) {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGTERM);
            }
        }
    }
}

fn teardown(sess: Session) {
    drop(sess.eis);
    kill_bus_clients(&sess.dbus_addr);
    if let Ok(pid) = i32::try_from(sess.child_pid) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
    }
    drop(sess.dbus_daemon);
}

fn win_pos(sess: &Session) -> Result<(i32, i32), McpError> {
    unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &sess.dbus_addr) };
    let win = kdotool::get_active_window_info().map_err(eis_err)?;
    #[expect(clippy::as_conversions)]
    Ok((win.x.round() as i32, win.y.round() as i32))
}

struct AtspiNode {
    name: String,
    role: String,
    states: Vec<String>,
    bounds: (i32, i32, i32, i32),
}

impl AtspiNode {
    fn line(&self, depth: usize) -> String {
        format!("{}{}\t{}\t{}\t{:?}", "  ".repeat(depth), self.role, self.name, self.states.join("|"), self.bounds)
    }

    fn is_useful(&self) -> bool { let (x, y, w, h) = self.bounds; w > 1 && h > 1 && x > -1000000 && y > -1000000 && !self.name.is_empty() }
}

fn state_labels(states: &[String]) -> Vec<String> {
    let has = |want: &str| states.iter().any(|s| s == want);
    [
        (has("Active") || has("Editable") || has("Checked"), "current"),
        (has("Enabled") || has("Sensitive"), "enabled"),
        (has("Focused"), "focused"),
        (has("Focusable"), "focusable"),
        (has("ReadOnly"), "readonly"),
        (has("Transient"), "transient"),
        (has("Checkable"), "checkable"),
        (has("Showing") || has("Visible"), "visible"),
    ].into_iter().filter_map(|(yes, label)| yes.then_some(label.to_owned())).collect()
}

async fn atspi_node(acc: &atspi::proxy::accessible::AccessibleProxy<'_>) -> Result<AtspiNode, McpError> {
    use atspi::proxy::proxy_ext::ProxyExt;
    let name = acc.name().await.unwrap_or_default();
    let role = acc.get_role_name().await.unwrap_or_default();
    let raw_states = acc.get_state().await.unwrap_or_default().into_iter().map(|s| format!("{s:?}")).collect::<Vec<_>>();
    let states = state_labels(&raw_states);
    let bounds = match acc.proxies().await.map_err(eis_err)?.component().await { Ok(c) => c.get_extents(atspi::CoordType::Screen).await.unwrap_or_default(), Err(_) => (0, 0, 0, 0) };
    Ok(AtspiNode { name, role, states, bounds })
}

#[mcp_server(name = "kwin-mcp", version = "0.1.0", instructions = "KDE Wayland desktop automation. Call session_start first. Coordinates are pixels on a 1920x1080 screen.")]
impl KwinMcp {
    #[tool(description = "Start an isolated KWin Wayland session. Must be called before any other tool.")]
    async fn session_start(&self, width: Option<serde_json::Value>, height: Option<serde_json::Value>) -> Result<ToolOutput, McpError> {
        {
            let mut guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
            if let Some(old) = (*guard).take() { teardown(old); }
        }
        let w = u32::try_from(width.map(parse_int).transpose()?.unwrap_or(1920)).map_err(|e| McpError::invalid_params("width", e.to_string()))?;
        let h = u32::try_from(height.map(parse_int).transpose()?.unwrap_or(1080)).map_err(|e| McpError::invalid_params("height", e.to_string()))?;
        let result: Result<Session, anyhow::Error> = tokio::task::spawn_blocking(move || {
            let pid = std::process::id();
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| anyhow::anyhow!("{e}"))?.as_secs();
            let sock = format!("wayland-mcp-{pid}-{ts}");
            let xdg = std::env::var("XDG_RUNTIME_DIR").map_err(|e| anyhow::anyhow!("XDG_RUNTIME_DIR: {e}"))?;
            let config_dir = format!("{}/.config/kwin-mcp", std::env::var("HOME").map_err(|e| anyhow::anyhow!("HOME: {e}"))?);
            // Write KDE config files
            std::fs::create_dir_all(&config_dir)?;
            write_kde_config(&config_dir, "kwinrc", &[
                ("Compositing", "ShadowSize", "0"),
                ("org.kde.kdecoration2", "ShadowSize", "0"),
            ])?;
            write_kde_config(&config_dir, "kcminputrc", &[("Mouse", "cursorTheme", "breeze_cursors")])?;
            write_kde_config(&config_dir, "kdeglobals", &[("General", "ColorScheme", "BreezeDark")])?;
            std::fs::write(format!("{config_dir}/kwinrulesrc"),
                "[1]\nDescription=nodecor\nnoborder=true\nnoborderrule=2\nwmclass=.*\nwmclassmatch=3\n[General]\ncount=1\n")?;
            // Launch private D-Bus daemon
            let mut launcher = dbus_launch::Launcher::daemon();
            launcher.bus_type(dbus_launch::BusType::Session);
            launcher.service_dir("/usr/share/dbus-1/services/");
            let daemon = launcher.launch().map_err(|e| anyhow::anyhow!("dbus-launch: {e}"))?;
            let dbus_addr = daemon.address().to_owned();
            // Get AT-SPI bus address
            let mut ch = dbus::channel::Channel::open_private(&dbus_addr).map_err(|e| anyhow::anyhow!("dbus: {e}"))?;
            ch.register().map_err(|e| anyhow::anyhow!("dbus reg: {e}"))?;
            let conn = dbus::blocking::Connection::from(ch);
            let proxy = conn.with_proxy("org.a11y.Bus", "/org/a11y/bus", std::time::Duration::from_secs(10));
            let (a11y_addr,): (String,) = proxy.method_call("org.a11y.Bus", "GetAddress", ())
                .map_err(|e| anyhow::anyhow!("a11y GetAddress: {e}"))?;
            // Update D-Bus activation environment
            let env_vars: std::collections::HashMap<&str, &str> = [("WAYLAND_DISPLAY", sock.as_str()), ("QT_QPA_PLATFORM", "wayland")].into_iter().collect();
            let bus_proxy = conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", std::time::Duration::from_secs(5));
            let _: () = bus_proxy.method_call("org.freedesktop.DBus", "UpdateActivationEnvironment", (env_vars,))
                .map_err(|e| anyhow::anyhow!("UpdateActivationEnvironment: {e}"))?;
            // Snapshot X sockets, spawn KWin
            let x_before = snapshot_x_sockets();
            let mut cmd = std::process::Command::new("kwin_wayland");
            cmd.args(["--virtual", "--no-lockscreen", "--xwayland",
                       "--width", &w.to_string(), "--height", &h.to_string(), "--socket", &sock])
                .env("DBUS_SESSION_BUS_ADDRESS", &dbus_addr)
                .env("KDE_FULL_SESSION", "true").env("KDE_SESSION_VERSION", "6")
                .env("XDG_SESSION_TYPE", "wayland").env("XDG_CURRENT_DESKTOP", "KDE")
                .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1").env("QT_ACCESSIBILITY", "1")
                .env("XCURSOR_THEME", "breeze_cursors").env("XDG_CONFIG_HOME", &config_dir)
                .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1").env("KWIN_SCREENSHOT_NO_PERMISSION_CHECKS", "1")
                .env_remove("WAYLAND_DISPLAY").env_remove("DISPLAY").env_remove("QT_QPA_PLATFORM")
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            let child = (unsafe { cmd.pre_exec(|| nix::unistd::setsid().map(drop).map_err(std::io::Error::from)) }).spawn()?;
            let child_pid = child.id();
            // Wait for wayland socket and XWayland display
            let sock_path = format!("{xdg}/{sock}");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            std::thread::sleep(std::time::Duration::from_millis(300));
            wait_for_path(&sock_path, deadline)?;
            let xdisplay = detect_xdisplay(&x_before, std::time::Instant::now() + std::time::Duration::from_secs(5))?;
            let scrdir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
            std::fs::create_dir_all(&scrdir)?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            let eis = Eis::connect(&dbus_addr)?;
            Ok(Session { dbus_daemon: daemon, dbus_addr, a11y_addr, child_pid, scrdir, socket: sock, xdisplay, width: w, height: h, eis })
        }).await.map_err(|e| McpError::internal(e.to_string()))?;
        match result {
            Ok(sess) => {
                let msg = format!("session started pid={} dbus={} socket={} geometry={}x{}", sess.child_pid, sess.dbus_addr, sess.socket, sess.width, sess.height);
                let mut guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
                *guard = Some(sess);
                Ok(ToolOutput::text(msg))
            }
            Err(e) => Err(McpError::internal(e.to_string())),
        }
    }

    #[tool(description = "Stop the KWin session and clean up all processes.", destructive = true)]
    async fn session_stop(&self) -> Result<ToolOutput, McpError> {
        let mut guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
        match (*guard).take() {
            Some(sess) => { let pid = sess.child_pid; teardown(sess); Ok(ToolOutput::text(format!("stopped pid={pid}"))) }
            None => Ok(ToolOutput::text("no session running")),
        }
    }

    #[tool(description = "Screenshot the active window. Returns PNG path + window position/size.", read_only = true)]
    async fn screenshot(&self) -> Result<ToolOutput, McpError> {
        self.with_session(|sess| {
            unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &sess.dbus_addr) };
            let win = kdotool::get_active_window_info().map_err(eis_err)?;
            let path = sess.scrdir.join("screenshot.png");
            let (read_fd, write_fd) = nix::unistd::pipe().map_err(eis_err)?;
            let write_raw = write_fd.into_raw_fd();
            let mut ch = dbus::channel::Channel::open_private(&sess.dbus_addr).map_err(eis_err)?;
            ch.register().map_err(eis_err)?;
            let conn = dbus::blocking::Connection::from(ch);
            let pipe_fd = unsafe { dbus::arg::OwnedFd::new(write_raw) };
            fn dbus_variant(v: Box<dyn dbus::arg::RefArg>) -> dbus::arg::Variant<Box<dyn dbus::arg::RefArg>> { dbus::arg::Variant(v) }
            let opts: std::collections::HashMap<String, dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>> = [
                ("include-cursor".to_owned(), dbus_variant(Box::new(true))),
                ("include-decoration".to_owned(), dbus_variant(Box::new(true))),
                ("include-shadow".to_owned(), dbus_variant(Box::new(false))),
            ].into_iter().collect::<std::collections::HashMap<_, _>>();
            // CaptureWindow by UUID — window-only, no shadow
            let msg = {
                let m = dbus::Message::new_method_call("org.kde.KWin", "/org/kde/KWin/ScreenShot2",
                    "org.kde.KWin.ScreenShot2", "CaptureWindow").map_err(eis_err)?;
                m.append3::<&str, &std::collections::HashMap<String, dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>>, dbus::arg::OwnedFd>(win.id.as_str(), &opts, pipe_fd)
            };
            let reply = conn.channel().send_with_reply_and_block(msg, std::time::Duration::from_secs(5)).map_err(eis_err)?;
            // parse metadata from reply — first arg is a{sv} dict
            let meta: std::collections::HashMap<String, dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>> =
                reply.read1().map_err(eis_err)?;
            let get_meta = |key: &str| -> Result<u32, McpError> {
                let val = meta.get(key).ok_or_else(|| McpError::internal(format!("no {key}")))?;
                let n = val.0.as_u64().ok_or_else(|| McpError::internal(format!("{key} not u64")))?;
                u32::try_from(n).map_err(|e| McpError::internal(e.to_string()))
            };
            let width = get_meta("width")?;
            let height = get_meta("height")?;
            let stride = get_meta("stride")?;
            // read raw ARGB32 pixels from pipe
            let mut reader = std::io::BufReader::new(unsafe { std::fs::File::from_raw_fd(read_fd.into_raw_fd()) });
            let total = usize::try_from(stride.checked_mul(height).ok_or_else(|| McpError::internal("overflow"))?).map_err(eis_err)?;
            let mut pixels = vec![0u8; total];
            std::io::Read::read_exact(&mut reader, &mut pixels).map_err(eis_err)?;
            // BGRA (little-endian ARGB32) → RGBA for PNG
            let px_count = usize::try_from(width.checked_mul(height).ok_or_else(|| McpError::internal("overflow"))?).map_err(eis_err)?;
            let mut rgba = vec![0u8; px_count * 4];
            for row in 0..height {
                for col in 0..width {
                    let si = usize::try_from(row * stride + col * 4).map_err(eis_err)?;
                    let di = usize::try_from((row * width + col) * 4).map_err(eis_err)?;
                    rgba[di] = pixels[si + 2]; rgba[di + 1] = pixels[si + 1];
                    rgba[di + 2] = pixels[si]; rgba[di + 3] = pixels[si + 3];
                }
            }
            // write PNG
            let file = std::fs::File::create(&path).map_err(eis_err)?;
            let mut enc = png::Encoder::new(file, width, height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(eis_err)?;
            writer.write_image_data(&rgba).map_err(eis_err)?;
            Ok(ToolOutput::text(format!("{} size={}x{} title={}",
                path.to_string_lossy(), width, height, win.title)))
        })
    }

    #[tool(description = "Get AT-SPI2 accessibility tree with widget roles, names, states, bounding boxes. By default hides zero-rect/internal nodes; set show_elements=true to include them.", read_only = true)]
    async fn accessibility_tree(&self, app_name: Option<String>, max_depth: Option<u32>, role: Option<String>, show_elements: Option<bool>) -> Result<ToolOutput, McpError> {
        use atspi::proxy::accessible::ObjectRefExt;
        let a11y = self.with_session(|s| Ok(s.a11y_addr.clone()))?;
        let conn = atspi::AccessibilityConnection::from_address(a11y.parse().map_err(eis_err)?).await.map_err(eis_err)?;
        let root = conn.root_accessible_on_registry().await.map_err(eis_err)?;
        let limit = usize::try_from(max_depth.unwrap_or(8)).map_err(eis_err)?;
        let app_name = app_name.map(|s| s.to_lowercase());
        let role = role.map(|s| s.to_lowercase());
        let show_elements = show_elements.unwrap_or(false);
        let mut out = Vec::new();
        let mut stack = root.get_children().await.map_err(eis_err)?.into_iter().rev().map(|obj| (obj, 0usize)).collect::<Vec<_>>();
        while let Some((obj, depth)) = stack.pop() {
            let acc = obj.as_accessible_proxy(conn.connection()).await.map_err(eis_err)?;
            let node = atspi_node(&acc).await?;
            if depth == 0 && !app_name.as_ref().map(|needle| node.name.to_lowercase().contains(needle)).unwrap_or(true) { continue; }
            let dominated = role.as_ref().map(|needle| node.role.to_lowercase().contains(needle)).unwrap_or(true) && (show_elements || node.is_useful());
            if dominated { out.push(node.line(depth)); }
            let child_depth = if dominated { depth + 1 } else { depth };
            if child_depth <= limit {
                for child in acc.get_children().await.unwrap_or_default().into_iter().rev() { stack.push((child, child_depth)); }
            }
        }
        Ok(ToolOutput::text(out.join("\n")))
    }

    #[tool(description = "Search UI elements by name/role/description (case-insensitive).", read_only = true)]
    async fn find_ui_elements(&self, _query: String, _app_name: Option<String>) -> Result<ToolOutput, McpError> {
        self.with_session(|sess| { eprintln!("atspi stub dbus={}", sess.dbus_addr); Err(McpError::internal("AT-SPI2 search not yet implemented")) })
    }

    #[tool(description = "Click at window-relative pixel coordinates. button: left/right/middle. double/triple for multi-click.")]
    async fn mouse_click(&self, x: serde_json::Value, y: serde_json::Value, button: Option<String>, double: Option<bool>, triple: Option<bool>) -> Result<ToolOutput, McpError> {
        let x = parse_int(x)?; let y = parse_int(y)?;
        self.with_session(|sess| {
            let (wx, wy) = win_pos(sess)?;
            let code = btn_code(button.as_deref())?;
            let count = match (triple, double) {
                (Some(true), _) => 3, (_, Some(true)) => 2,
                (Some(false) | None, Some(false) | None) => 1,
            };
            sess.eis.move_abs(wx + x, wy + y).map_err(eis_err)?;
            sess.eis.button(code, true).map_err(eis_err)?;
            std::thread::sleep(std::time::Duration::from_millis(10));
            sess.eis.button(code, false).map_err(eis_err)?;
            for _n in 1..count {
                std::thread::sleep(std::time::Duration::from_millis(50));
                sess.eis.button(code, true).map_err(eis_err)?;
                std::thread::sleep(std::time::Duration::from_millis(10));
                sess.eis.button(code, false).map_err(eis_err)?;
            }
            Ok(ToolOutput::text(format!("clicked ({x},{y}) x{count}")))
        })
    }

    #[tool(description = "Move cursor to window-relative pixel coordinates. Triggers hover effects.", read_only = true)]
    async fn mouse_move(&self, x: serde_json::Value, y: serde_json::Value) -> Result<ToolOutput, McpError> {
        let x = parse_int(x)?; let y = parse_int(y)?;
        self.with_session(|sess| {
            let (wx, wy) = win_pos(sess)?;
            sess.eis.move_abs(wx + x, wy + y).map_err(eis_err)?;
            Ok(ToolOutput::text(format!("moved ({x},{y})")))
        })
    }

    #[tool(description = "Scroll at window-relative pixel coords. delta: positive=down/right, negative=up/left. horizontal/discrete are optional.")]
    async fn mouse_scroll(&self, x: serde_json::Value, y: serde_json::Value, delta: serde_json::Value, horizontal: Option<bool>, discrete: Option<bool>) -> Result<ToolOutput, McpError> {
        let x = parse_int(x)?; let y = parse_int(y)?; let delta = parse_int(delta)?;
        self.with_session(|sess| {
            let (wx, wy) = win_pos(sess)?;
            sess.eis.move_abs(wx + x, wy + y).map_err(eis_err)?;
            let horiz = match horizontal { Some(v) => v, None => false };
            let disc = match discrete { Some(v) => v, None => false };
            let (dx, dy) = match horiz { true => (delta, 0), false => (0, delta) };
            sess.eis.scroll_do(dx, dy, disc).map_err(eis_err)?;
            Ok(ToolOutput::text(format!("scrolled {delta} at ({x},{y})")))
        })
    }

    #[tool(description = "Drag between window-relative pixel coords. Smooth 20-step interpolation. button: left/right/middle.")]
    async fn mouse_drag(&self, from_x: serde_json::Value, from_y: serde_json::Value, to_x: serde_json::Value, to_y: serde_json::Value, button: Option<String>) -> Result<ToolOutput, McpError> {
        let from_x = parse_int(from_x)?; let from_y = parse_int(from_y)?;
        let to_x = parse_int(to_x)?; let to_y = parse_int(to_y)?;
        self.with_session(|sess| {
            let (wx, wy) = win_pos(sess)?;
            let code = btn_code(button.as_deref())?;
            sess.eis.move_abs(wx + from_x, wy + from_y).map_err(eis_err)?;
            sess.eis.button(code, true).map_err(eis_err)?;
            let steps = 20i32;
            for step in 1..=steps {
                let cx = wx + from_x + (to_x - from_x) * step / steps;
                let cy = wy + from_y + (to_y - from_y) * step / steps;
                sess.eis.move_abs(cx, cy).map_err(eis_err)?;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            sess.eis.button(code, false).map_err(eis_err)?;
            Ok(ToolOutput::text(format!("dragged ({from_x},{from_y})->({to_x},{to_y})")))
        })
    }

    #[tool(description = "Type ASCII text character by character. For non-ASCII use keyboard_type_unicode.")]
    async fn keyboard_type(&self, text: String) -> Result<ToolOutput, McpError> {
        self.with_session(|sess| {
            for ch in text.chars() {
                match char_key(ch) {
                    Some((code, needs_shift)) => {
                        let shift: &[u32] = match needs_shift { true => &[42], false => &[] };
                        for s in shift { sess.eis.key(*s, true).map_err(eis_err)?; }
                        sess.eis.key(code, true).map_err(eis_err)?;
                        sess.eis.key(code, false).map_err(eis_err)?;
                        for s in shift { sess.eis.key(*s, false).map_err(eis_err)?; }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    None => return Err(McpError::invalid_params("text", format!("unmapped char '{ch}' — ASCII only"))),
                }
            }
            Ok(ToolOutput::text(format!("typed: {text}")))
        })
    }


    #[tool(description = "Press key combo (e.g. 'Return', 'ctrl+c', 'alt+F4', 'shift+Tab').")]
    async fn keyboard_key(&self, key: String) -> Result<ToolOutput, McpError> {
        self.with_session(|sess| {
            let (mods, main) = parse_combo(&key);
            for m in &mods { sess.eis.key(*m, true).map_err(eis_err)?; }
            match main {
                Some(k) => {
                    sess.eis.key(k, true).map_err(eis_err)?;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    sess.eis.key(k, false).map_err(eis_err)?;
                }
                None => return Err(McpError::invalid_params("key", format!("unknown key in combo '{key}'"))),
            }
            for m in mods.iter().rev() { sess.eis.key(*m, false).map_err(eis_err)?; }
            Ok(ToolOutput::text(format!("key: {key}")))
        })
    }

    #[tool(description = "Launch app in session (non-blocking). Returns PID.")]
    async fn launch_app(&self, command: String) -> Result<ToolOutput, McpError> {
        self.with_session(|sess| {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", &command]).env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env("DBUS_SESSION_BUS_ADDRESS", &sess.dbus_addr).env("WAYLAND_DISPLAY", &sess.socket)
                .env("DISPLAY", &sess.xdisplay).env("QT_QPA_PLATFORM", "wayland")
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            match cmd.spawn() {
                Ok(child) => Ok(ToolOutput::text(format!("launched pid={}", child.id()))),
                Err(e) => Err(McpError::internal(format!("spawn: {e}"))),
            }
        })
    }


}

#[tokio::main]
async fn main() -> Result<(), McpError> {
    unsafe {
        nix::libc::signal(nix::libc::SIGCHLD, nix::libc::SIG_IGN);
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
    }
    let kwin = KwinMcp::new();
    let server = ServerBuilder::new(kwin.clone()).with_tools(kwin).build();
    server.serve(StdioTransport::new()).await
}
