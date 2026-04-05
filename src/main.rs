use mcpkit::prelude::*;
use mcpkit::transport::stdio::StdioTransport;

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

#[allow(clippy::too_many_arguments)]
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
}

impl Eis {
    fn from_fd(fd: std::os::fd::OwnedFd) -> anyhow::Result<Self> {
        let stream = UnixStream::from(fd);
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
            kbd: kb.ok_or_else(|| anyhow::anyhow!("no kbd"))?, ptr_dev, kbd_dev, serial,
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

// ── Portal session ──────────────────────────────────────────────────────

async fn portal_setup(zbus_conn: &zbus::Connection) -> anyhow::Result<std::os::fd::OwnedFd> {
    use ashpd::desktop::remote_desktop::{RemoteDesktop, DeviceType, SelectDevicesOptions, ConnectToEISOptions, StartOptions};
    use ashpd::desktop::CreateSessionOptions;
    // Pre-seed permission store to skip consent dialog
    // Derive our app_id the same way xdg-desktop-portal does (from systemd cgroup)
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let app_id = cgroup.split('/').filter_map(|seg| {
        seg.strip_prefix("app-").and_then(|s| s.rsplit_once('-')).map(|(name, _)| name.to_owned())
    }).next().unwrap_or_default();
    let perm_proxy: zbus::Proxy = zbus::proxy::Builder::new(zbus_conn)
        .destination("org.freedesktop.impl.portal.PermissionStore").map_err(|e| anyhow::anyhow!("{e}"))?
        .path("/org/freedesktop/impl/portal/PermissionStore").map_err(|e| anyhow::anyhow!("{e}"))?
        .interface("org.freedesktop.impl.portal.PermissionStore").map_err(|e| anyhow::anyhow!("{e}"))?
        .build().await.map_err(|e| anyhow::anyhow!("permission store proxy: {e}"))?;
    let perms: &[&str] = &["yes"];
    // Seed both the derived app_id and empty string to cover all cases
    for id in [app_id.as_str(), ""] {
        let _: Result<(), zbus::Error> = perm_proxy.call("SetPermission", &("kde-authorized", true, "remote-desktop", id, perms)).await;
    }
    // Create RemoteDesktop session
    let rd = RemoteDesktop::with_connection(zbus_conn.clone()).await
        .map_err(|e| anyhow::anyhow!("RemoteDesktop: {e}"))?;
    let session = rd.create_session(CreateSessionOptions::default()).await
        .map_err(|e| anyhow::anyhow!("create_session: {e}"))?;
    rd.select_devices(&session, SelectDevicesOptions::default().set_devices(DeviceType::Keyboard | DeviceType::Pointer)).await
        .map_err(|e| anyhow::anyhow!("select_devices: {e}"))?.response()
        .map_err(|e| anyhow::anyhow!("select_devices response: {e}"))?;
    let started = rd.start(&session, None, StartOptions::default()).await
        .map_err(|e| anyhow::anyhow!("start: {e}"))?.response()
        .map_err(|e| anyhow::anyhow!("start response: {e}"))?;
    let _devices = started.devices();
    let eis_fd = rd.connect_to_eis(&session, ConnectToEISOptions::default()).await
        .map_err(|e| anyhow::anyhow!("connect_to_eis: {e}"))?;
    Ok(eis_fd)
}

// ── Session ──────────────────────────────────────────────────────────────

#[expect(dead_code)] // width, height, zbus_conn, bus_dir stored for future portal/screencast use
struct Session { dbus_pid: u32, bus_dir: tempfile::TempDir, dbus_addr: String, a11y_addr: String, child_pid: u32, scrdir: PathBuf, socket: String, xdisplay: String, xauthority: String, width: u32, height: u32, eis: Eis, zbus_conn: zbus::Connection }

// ── Server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct KwinMcp { session: Arc<Mutex<Option<Session>>> }

impl KwinMcp {
    fn new() -> Self { Self { session: Arc::new(Mutex::new(None)) } }
    fn with_session<R>(&self, f: impl FnOnce(&Session) -> Result<R, McpError>) -> Result<R, McpError> {
        let guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
        match &*guard { Some(s) => f(s), None => Err(McpError::internal("no session — call session_start first")) }
    }
    fn zbus_conn(&self) -> Result<zbus::Connection, McpError> {
        let guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
        match &*guard { Some(s) => Ok(s.zbus_conn.clone()), None => Err(McpError::internal("no session — call session_start first")) }
    }
}

fn eis_err(e: impl std::fmt::Display) -> McpError { McpError::internal(e.to_string()) }

async fn kill_bus_clients(conn: &zbus::Connection) {
    let Ok(proxy) = zbus::fdo::DBusProxy::new(conn).await else { return };
    let Ok(names) = proxy.list_names().await else { return };
    let my_pid = std::process::id();
    for name in &names {
        if name.as_str().starts_with(':') {
            let pid = proxy.get_connection_unix_process_id(name.inner().clone()).await;
            if let Ok(pid) = pid && pid != my_pid && let Ok(p) = i32::try_from(pid) {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGTERM);
            }
        }
    }
}

async fn teardown(sess: Session) {
    drop(sess.eis);
    kill_bus_clients(&sess.zbus_conn).await;
    if let Ok(pid) = i32::try_from(sess.child_pid) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
    }
    if let Ok(pid) = i32::try_from(sess.dbus_pid) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
    }
    // Clean up X11 lock file and socket based on xdisplay (e.g. ":2" -> display 2)
    if let Some(display_num) = sess.xdisplay.strip_prefix(':') {
        let lock_path = format!("/tmp/.X{display_num}-lock");
        let sock_path = format!("/tmp/.X11-unix/X{display_num}");
        let _ = std::fs::remove_file(&lock_path);
        let _ = std::fs::remove_file(&sock_path);
    }
    // Clean up xauthority file
    if !sess.xauthority.is_empty() {
        let _ = std::fs::remove_file(&sess.xauthority);
    }
}

fn find_xwayland_info(kwin_pid: u32) -> (String, String) {
    // Find Xwayland processes and check if they're children of our KWin
    let Ok(entries) = std::fs::read_dir("/proc") else { return (String::new(), String::new()) };
    for entry in entries.flatten() {
        let pid_str = entry.file_name().to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) { continue; }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid_str}/cmdline")) else { continue };
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        let is_xwayland = args.first().is_some_and(|a| {
            std::str::from_utf8(a).is_ok_and(|s| s.contains("Xwayland"))
        });
        if !is_xwayland { continue; }
        // Check parent chain leads to our KWin
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid_str}/status")) else { continue };
        let ppid = status.lines().find_map(|l| l.strip_prefix("PPid:\t")).and_then(|s| s.parse::<u32>().ok());
        if ppid != Some(kwin_pid) { continue; }
        // Found our Xwayland — extract display and -auth from args
        let mut display = String::new();
        let mut auth = String::new();
        let mut i = 0;
        while i < args.len() {
            if let Ok(s) = std::str::from_utf8(args[i]) {
                if s.starts_with(':') && display.is_empty() { display = s.to_owned(); }
                if s == "-auth" && let Some(next) = args.get(i + 1) {
                    auth = std::str::from_utf8(next).unwrap_or("").to_owned();
                }
            }
            i += 1;
        }
        return (display, auth);
    }
    (String::new(), String::new())
}

async fn active_window_info(conn: &zbus::Connection) -> Result<(i32, i32, String), McpError> {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(eis_err)?.as_millis();
    let marker = format!("kwin-mcp-{ts}");
    let cb_path = format!("/KWinMCP/{ts}");
    let our_name = conn.unique_name().ok_or_else(|| McpError::internal("no bus name"))?.to_string();
    let script = format!("var w = workspace.activeWindow;\
        callDBus('{our_name}','{cb_path}','org.kde.KWinMCP','result',\
        w ? JSON.stringify({{x:w.frameGeometry.x,y:w.frameGeometry.y,\
        w:w.frameGeometry.width,h:w.frameGeometry.height,\
        title:w.caption,id:w.internalId.toString()}}) : 'null');");
    let script_file = std::env::temp_dir().join(format!("{marker}.js"));
    std::fs::write(&script_file, &script).map_err(eis_err)?;
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let cb = KWinCallback { tx: std::sync::Mutex::new(Some(tx)) };
    let obj_path = zbus::zvariant::ObjectPath::try_from(cb_path.as_str()).map_err(eis_err)?;
    let registered = conn.object_server().at(&obj_path, cb).await.map_err(eis_err)?;
    eprintln!("active_window_info: our_name={our_name} path={cb_path} registered={registered}");
    if !registered { return Err(McpError::internal(format!("failed to register callback at {cb_path}"))); }
    // Load and run the script
    let scripting: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin").map_err(eis_err)?
        .path("/Scripting").map_err(eis_err)?
        .interface("org.kde.kwin.Scripting").map_err(eis_err)?
        .build().await.map_err(eis_err)?;
    let script_path = script_file.to_string_lossy().to_string();
    let (script_id,): (i32,) = scripting.call("loadScript", &(script_path, &marker)).await.map_err(eis_err)?;
    if script_id < 0 {
        let _ = conn.object_server().remove::<KWinCallback, _>(&obj_path).await;
        let _ = std::fs::remove_file(&script_file);
        return Err(McpError::internal(format!("KWin loadScript failed, id={script_id}")));
    }
    let script_proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin").map_err(eis_err)?
        .path(format!("/Scripting/Script{script_id}")).map_err(eis_err)?
        .interface("org.kde.kwin.Script").map_err(eis_err)?
        .build().await.map_err(eis_err)?;
    let _: () = script_proxy.call("run", &()).await.map_err(eis_err)?;
    // Wait for callback, then cleanup regardless of result
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), rx).await;
    let _ = conn.object_server().remove::<KWinCallback, _>(&obj_path).await;
    let _: Result<(bool,), _> = scripting.call("unloadScript", &(&marker,)).await;
    let _ = std::fs::remove_file(&script_file);
    let json = result.map_err(|_| McpError::internal("KWin script timed out"))?
        .map_err(|_| McpError::internal("KWin callback channel closed"))?;
    if json == "null" { return Err(McpError::internal("KWin script error: No active window")); }
    let v: serde_json::Value = serde_json::from_str(&json).map_err(eis_err)?;
    let x = v.get("x").and_then(|v| v.as_f64()).ok_or_else(|| McpError::internal("no x"))?;
    let y = v.get("y").and_then(|v| v.as_f64()).ok_or_else(|| McpError::internal("no y"))?;
    let id = v.get("id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    #[expect(clippy::as_conversions)]
    Ok((x.round() as i32, y.round() as i32, id))
}

struct KWinCallback { tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>> }

#[zbus::interface(name = "org.kde.KWinMCP")]
impl KWinCallback {
    #[zbus(name = "result")]
    fn result(&self, payload: String) {
        if let Some(tx) = self.tx.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(payload);
        }
    }
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
        let old = {
            let mut guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
            (*guard).take()
        };
        if let Some(old) = old { teardown(old).await; }
        let w = u32::try_from(width.map(parse_int).transpose()?.unwrap_or(1920)).map_err(|e| McpError::invalid_params("width", e.to_string()))?;
        let h = u32::try_from(height.map(parse_int).transpose()?.unwrap_or(1080)).map_err(|e| McpError::invalid_params("height", e.to_string()))?;
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
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
                ("Xwayland", "XwaylandEisNoPrompt", "true"),
            ])?;
            write_kde_config(&config_dir, "kcminputrc", &[("Mouse", "cursorTheme", "breeze_cursors")])?;
            write_kde_config(&config_dir, "kdeglobals", &[("General", "ColorScheme", "BreezeDark")])?;
            std::fs::write(format!("{config_dir}/kwinrulesrc"),
                "[1]\nDescription=nodecor\nnoborder=true\nnoborderrule=2\nwmclass=.*\nwmclassmatch=3\n[General]\ncount=1\n")?;
            // Launch private D-Bus daemon with permissive policy for portal inter-service replies
            let bus_dir = tempfile::tempdir().map_err(|e| anyhow::anyhow!("tempdir: {e}"))?;
            let bus_conf = format!(r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={dir}</listen>
  <servicedir>/usr/share/dbus-1/services/</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>"#, dir = bus_dir.path().display());
            std::fs::write(bus_dir.path().join("bus.conf"), &bus_conf)?;
            let mut dbus_child = std::process::Command::new("dbus-daemon")
                .args(["--config-file", &bus_dir.path().join("bus.conf").to_string_lossy(), "--nofork", "--print-address"])
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
                .spawn().map_err(|e| anyhow::anyhow!("dbus-daemon spawn: {e}"))?;
            let dbus_stdout = dbus_child.stdout.take().ok_or_else(|| anyhow::anyhow!("no dbus stdout"))?;
            let mut dbus_reader = std::io::BufReader::new(dbus_stdout);
            let mut dbus_addr = String::new();
            std::io::BufRead::read_line(&mut dbus_reader, &mut dbus_addr).map_err(|e| anyhow::anyhow!("dbus addr: {e}"))?;
            let dbus_addr = dbus_addr.trim().to_owned();
            anyhow::ensure!(!dbus_addr.is_empty(), "dbus-daemon returned empty address");
            let dbus_pid = dbus_child.id();
            // Pre-create Xwayland sockets mirroring KWin's XwaylandSocket setup:
            // 1. Lock file /tmp/.X{n}-lock
            // 2. Filesystem socket /tmp/.X11-unix/X{n}
            // 3. Abstract socket @/tmp/.X11-unix/X{n} (Linux)
            let xw_display = {
                let mut n = 2u32;
                loop {
                    let lock_path = format!("/tmp/.X{n}-lock");
                    let sock_path = format!("/tmp/.X11-unix/X{n}");
                    if !std::path::Path::new(&lock_path).exists() && !std::path::Path::new(&sock_path).exists() {
                        break n;
                    }
                    n += 1;
                    anyhow::ensure!(n < 100, "no free X display");
                }
            };
            let xdisplay = format!(":{xw_display}");
            let xauth_path = format!("{xdg}/xauth_mcp_{pid}");
            // Create lock file (format: 10-char padded PID + newline, like KWin does)
            let lock_path = format!("/tmp/.X{xw_display}-lock");
            {
                use std::io::Write;
                let mut lock_file = std::fs::OpenOptions::new()
                    .write(true).create_new(true).open(&lock_path)
                    .map_err(|e| anyhow::anyhow!("create lock file {lock_path}: {e}"))?;
                writeln!(lock_file, "{:>10}", pid).map_err(|e| anyhow::anyhow!("write lock file: {e}"))?;
            }
            // Generate xauth cookie
            let mut cookie = [0u8; 16];
            let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut cookie));
            let cookie_hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
            let xauth_status = std::process::Command::new("xauth")
                .args(["-f", &xauth_path, "add", &xdisplay, "MIT-MAGIC-COOKIE-1", &cookie_hex])
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            if !xauth_status.is_ok_and(|s| s.success()) {
                let _ = std::fs::remove_file(&lock_path);
                anyhow::bail!("xauth failed to add cookie for {xdisplay}");
            }
            // Create filesystem X11 listen socket
            let xw_sock_path = format!("/tmp/.X11-unix/X{xw_display}");
            let _ = std::fs::remove_file(&xw_sock_path); // remove stale if any
            let xw_unix_listener = std::os::unix::net::UnixListener::bind(&xw_sock_path)
                .map_err(|e| { let _ = std::fs::remove_file(&lock_path); anyhow::anyhow!("bind X socket {xw_sock_path}: {e}") })?;
            let xw_unix_fd = std::os::unix::io::AsRawFd::as_raw_fd(&xw_unix_listener);
            // Create abstract X11 listen socket (Linux-specific)
            let xw_abstract_fd = {
                let sock_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
                if sock_fd < 0 {
                    let _ = std::fs::remove_file(&lock_path);
                    let _ = std::fs::remove_file(&xw_sock_path);
                    anyhow::bail!("failed to create abstract socket");
                }
                // Build abstract sockaddr_un: sun_path[0] = '\0', then path without NUL terminator
                let path_bytes = xw_sock_path.as_bytes();
                let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
                addr.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX)
                    .map_err(|_| anyhow::anyhow!("AF_UNIX conversion failed"))?;
                // Abstract: first byte is NUL, then the path
                addr.sun_path[0] = 0;
                for (i, &b) in path_bytes.iter().enumerate() {
                    if i + 1 >= addr.sun_path.len() { break; }
                    addr.sun_path[i + 1] = libc::c_char::try_from(i8::try_from(b).map_err(|_| anyhow::anyhow!("byte conversion"))?)
                        .map_err(|_| anyhow::anyhow!("c_char conversion"))?;
                }
                // Length = offset of sun_path + 1 (NUL) + path length (no trailing NUL for abstract)
                let addr_len = libc::socklen_t::try_from(std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + path_bytes.len())
                    .map_err(|_| anyhow::anyhow!("addr_len conversion"))?;
                let bind_res = unsafe {
                    libc::bind(sock_fd, std::ptr::from_ref(&addr).cast::<libc::sockaddr>(), addr_len)
                };
                if bind_res < 0 {
                    unsafe { libc::close(sock_fd); }
                    let _ = std::fs::remove_file(&lock_path);
                    let _ = std::fs::remove_file(&xw_sock_path);
                    anyhow::bail!("failed to bind abstract socket");
                }
                if unsafe { libc::listen(sock_fd, 1) } < 0 {
                    unsafe { libc::close(sock_fd); }
                    let _ = std::fs::remove_file(&lock_path);
                    let _ = std::fs::remove_file(&xw_sock_path);
                    anyhow::bail!("failed to listen on abstract socket");
                }
                sock_fd
            };
            // Spawn KWin with both Xwayland socket fds
            let xw_unix_fd_str = xw_unix_fd.to_string();
            let xw_abstract_fd_str = xw_abstract_fd.to_string();
            let mut cmd = std::process::Command::new("kwin_wayland");
            cmd.args(["--virtual", "--no-lockscreen", "--xwayland",
                       "--xwayland-fd", &xw_unix_fd_str,
                       "--xwayland-fd", &xw_abstract_fd_str,
                       "--xwayland-display", &xdisplay,
                       "--xwayland-xauthority", &xauth_path,
                       "--width", &w.to_string(), "--height", &h.to_string(), "--socket", &sock])
                .env("DBUS_SESSION_BUS_ADDRESS", &dbus_addr)
                .env("KDE_FULL_SESSION", "true").env("KDE_SESSION_VERSION", "6")
                .env("XDG_SESSION_TYPE", "wayland").env("XDG_CURRENT_DESKTOP", "KDE")
                .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1").env("QT_ACCESSIBILITY", "1")
                .env("XCURSOR_THEME", "breeze_cursors").env("XDG_CONFIG_HOME", &config_dir)
                .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1").env("KWIN_SCREENSHOT_NO_PERMISSION_CHECKS", "1")
                .env_remove("WAYLAND_DISPLAY").env_remove("DISPLAY").env_remove("QT_QPA_PLATFORM")
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            let xauthority = xauth_path.clone();
            let child = (unsafe { cmd.pre_exec(move || {
                // Keep both X socket fds open for KWin to inherit
                let _ = nix::fcntl::fcntl(xw_unix_fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()));
                let _ = nix::fcntl::fcntl(xw_abstract_fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()));
                nix::unistd::setsid().map(drop).map_err(std::io::Error::from)
            }) }).spawn()?;
            // We can drop the listeners now — KWin inherited the fds
            drop(xw_unix_listener);
            unsafe { libc::close(xw_abstract_fd); }
            let child_pid = child.id();
            // Wait for wayland socket and XWayland display
            let sock_path = format!("{xdg}/{sock}");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            std::thread::sleep(std::time::Duration::from_millis(300));
            wait_for_path(&sock_path, deadline)?;
            // Xwayland display and auth are known from pre-creation above
            let scrdir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
            std::fs::create_dir_all(&scrdir)?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok((dbus_pid, bus_dir, dbus_addr, child_pid, scrdir, sock, xdisplay, xauthority, w, h))
        }).await.map_err(|e| McpError::internal(e.to_string()))?;
        let (dbus_pid, bus_dir, dbus_addr, child_pid, scrdir, socket, xdisplay, xauthority, width, height) =
            result.map_err(|e| McpError::internal(e.to_string()))?;
        // Async: connect to our private bus via zbus, set up portal session, get EIS fd
        let zbus_conn = zbus::connection::Builder::address(dbus_addr.as_str()).map_err(eis_err)?
            .build().await.map_err(eis_err)?;
        // Get AT-SPI bus address
        let a11y_proxy: zbus::Proxy = zbus::proxy::Builder::new(&zbus_conn)
            .destination("org.a11y.Bus").map_err(eis_err)?
            .path("/org/a11y/bus").map_err(eis_err)?
            .interface("org.a11y.Bus").map_err(eis_err)?
            .build().await.map_err(eis_err)?;
        let (a11y_addr,): (String,) = a11y_proxy.call("GetAddress", &()).await.map_err(eis_err)?;
        // Update D-Bus activation environment
        let env_vars: std::collections::HashMap<&str, &str> = [("WAYLAND_DISPLAY", socket.as_str()), ("QT_QPA_PLATFORM", "wayland")].into_iter().collect();
        let bus_proxy: zbus::Proxy = zbus::proxy::Builder::new(&zbus_conn)
            .destination("org.freedesktop.DBus").map_err(eis_err)?
            .path("/org/freedesktop/DBus").map_err(eis_err)?
            .interface("org.freedesktop.DBus").map_err(eis_err)?
            .build().await.map_err(eis_err)?;
        let _: () = bus_proxy.call("UpdateActivationEnvironment", &(env_vars,)).await.map_err(eis_err)?;
        let eis_fd = portal_setup(&zbus_conn).await.map_err(eis_err)?;
        // Blocking: reis negotiation over the EIS fd
        let eis = tokio::task::spawn_blocking(move || Eis::from_fd(eis_fd))
            .await.map_err(|e| McpError::internal(e.to_string()))?
            .map_err(eis_err)?;
        let msg = format!("session started pid={child_pid} dbus={dbus_addr} socket={socket} display={xdisplay} geometry={width}x{height}");
        let mut guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
        *guard = Some(Session { dbus_pid, bus_dir, dbus_addr, a11y_addr, child_pid, scrdir, socket, xdisplay, xauthority, width, height, eis, zbus_conn: zbus_conn.clone() });
        Ok(ToolOutput::text(msg))
    }

    #[tool(description = "Stop the KWin session and clean up all processes.", destructive = true)]
    async fn session_stop(&self) -> Result<ToolOutput, McpError> {
        let taken = {
            let mut guard = self.session.lock().map_err(|e| McpError::internal(e.to_string()))?;
            (*guard).take()
        };
        match taken {
            Some(sess) => { let pid = sess.child_pid; teardown(sess).await; Ok(ToolOutput::text(format!("stopped pid={pid}"))) }
            None => Ok(ToolOutput::text("no session running")),
        }
    }

    #[tool(description = "Screenshot the active window. Returns PNG path + window position/size.", read_only = true)]
    async fn screenshot(&self) -> Result<ToolOutput, McpError> {
        let zbus_conn = self.zbus_conn()?;
        let (_, _, win_id) = active_window_info(&zbus_conn).await?;
        let path = self.with_session(|sess| Ok(sess.scrdir.join("screenshot.png")))?;
        let (read_fd, write_fd) = nix::unistd::pipe().map_err(eis_err)?;
        let pipe_fd = zbus::zvariant::OwnedFd::from(write_fd);
        let opts: std::collections::HashMap<&str, zbus::zvariant::Value> = [
            ("include-cursor", zbus::zvariant::Value::from(true)),
            ("include-decoration", zbus::zvariant::Value::from(true)),
            ("include-shadow", zbus::zvariant::Value::from(false)),
        ].into_iter().collect();
        let ss_proxy: zbus::Proxy = zbus::proxy::Builder::new(&zbus_conn)
            .destination("org.kde.KWin").map_err(eis_err)?
            .path("/org/kde/KWin/ScreenShot2").map_err(eis_err)?
            .interface("org.kde.KWin.ScreenShot2").map_err(eis_err)?
            .build().await.map_err(eis_err)?;
        let reply: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
            ss_proxy.call("CaptureWindow", &(win_id.as_str(), &opts, pipe_fd)).await.map_err(eis_err)?;
        let get_meta = |key: &str| -> Result<u32, McpError> {
            let val = reply.get(key).ok_or_else(|| McpError::internal(format!("no {key}")))?;
            let n: u32 = val.try_into().map_err(|e: zbus::zvariant::Error| McpError::internal(e.to_string()))?;
            Ok(n)
        };
        let width = get_meta("width")?;
        let height = get_meta("height")?;
        let stride = get_meta("stride")?;
        // read raw ARGB32 pixels from pipe
        let mut reader = std::io::BufReader::new(std::fs::File::from(read_fd));
        let total = usize::try_from(stride.checked_mul(height).ok_or_else(|| McpError::internal("overflow"))?).map_err(eis_err)?;
        let mut pixels = vec![0u8; total];
        std::io::Read::read_exact(&mut reader, &mut pixels).map_err(eis_err)?;
        // BGRA (little-endian ARGB32) -> RGBA for PNG
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
        Ok(ToolOutput::text(format!("{} size={}x{}",
            path.to_string_lossy(), width, height)))
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
        let (wx, wy, _) = active_window_info(&self.zbus_conn()?).await?;
        self.with_session(|sess| {
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
        let (wx, wy, _) = active_window_info(&self.zbus_conn()?).await?;
        self.with_session(|sess| {
            sess.eis.move_abs(wx + x, wy + y).map_err(eis_err)?;
            Ok(ToolOutput::text(format!("moved ({x},{y})")))
        })
    }

    #[tool(description = "Scroll at window-relative pixel coords. delta: positive=down/right, negative=up/left. horizontal/discrete are optional.")]
    async fn mouse_scroll(&self, x: serde_json::Value, y: serde_json::Value, delta: serde_json::Value, horizontal: Option<bool>, discrete: Option<bool>) -> Result<ToolOutput, McpError> {
        let x = parse_int(x)?; let y = parse_int(y)?; let delta = parse_int(delta)?;
        let (wx, wy, _) = active_window_info(&self.zbus_conn()?).await?;
        self.with_session(|sess| {
            sess.eis.move_abs(wx + x, wy + y).map_err(eis_err)?;
            let horiz = horizontal.unwrap_or_default();
            let disc = discrete.unwrap_or_default();
            let (dx, dy) = match horiz { true => (delta, 0), false => (0, delta) };
            sess.eis.scroll_do(dx, dy, disc).map_err(eis_err)?;
            Ok(ToolOutput::text(format!("scrolled {delta} at ({x},{y})")))
        })
    }

    #[tool(description = "Drag between window-relative pixel coords. Smooth 20-step interpolation. button: left/right/middle.")]
    async fn mouse_drag(&self, from_x: serde_json::Value, from_y: serde_json::Value, to_x: serde_json::Value, to_y: serde_json::Value, button: Option<String>) -> Result<ToolOutput, McpError> {
        let from_x = parse_int(from_x)?; let from_y = parse_int(from_y)?;
        let to_x = parse_int(to_x)?; let to_y = parse_int(to_y)?;
        let (wx, wy, _) = active_window_info(&self.zbus_conn()?).await?;
        self.with_session(|sess| {
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
            // Refresh Xwayland info if not yet available (lazy start)
            let (xdisp, xauth) = match sess.xdisplay.is_empty() {
                true => find_xwayland_info(sess.child_pid),
                false => (sess.xdisplay.clone(), sess.xauthority.clone()),
            };
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", &command]).env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").env_remove("DBUS_SESSION_BUS_ADDRESS").env_remove("XAUTHORITY")
                .env("DBUS_SESSION_BUS_ADDRESS", &sess.dbus_addr).env("WAYLAND_DISPLAY", &sess.socket)
                .env("QT_QPA_PLATFORM", "wayland")
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            if !xdisp.is_empty() { cmd.env("DISPLAY", &xdisp); }
            if !xauth.is_empty() { cmd.env("XAUTHORITY", &xauth); }
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
