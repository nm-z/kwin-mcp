use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use serde::Deserialize;
use std::sync::Arc;

type McpError = rmcp::ErrorData;

// Claude Code serializes numbers to strings — FlexInt accepts both.
// Implements JsonSchema so rmcp emits a proper schema instead of `true`.
#[derive(Debug, Clone)]
struct FlexInt(i32);
impl<'de> serde::Deserialize<'de> for FlexInt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(deserializer)?;
        match v {
            serde_json::Value::Number(n) => {
                let i = n
                    .as_i64()
                    .ok_or_else(|| serde::de::Error::custom("not an i64"))?;
                let v = i32::try_from(i)
                    .map_err(|e| serde::de::Error::custom(format!("not an i32: {e}")))?;
                Ok(FlexInt(v))
            }
            serde_json::Value::String(s) => s
                .parse::<i32>()
                .map(FlexInt)
                .map_err(serde::de::Error::custom),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Array(_)
            | serde_json::Value::Object(_) => {
                Err(serde::de::Error::custom("expected integer or string"))
            }
        }
    }
}
impl schemars::JsonSchema for FlexInt {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FlexInt".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": ["integer", "string"], "description": "integer or string-encoded integer" })
    }
}

fn parse_int(v: FlexInt) -> i32 {
    v.0
}

const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const STARTUP_POLL: std::time::Duration = std::time::Duration::from_millis(50);

// ── Evdev keycodes ───────────────────────────────────────────────────────

use keyboard_codes::{KeyCodeMapper, Platform};

fn char_key(ch: char) -> Result<(u32, bool), McpError> {
    let (raw, shifted) = match ch {
        'a'..='z'
        | '0'..='9'
        | '`'
        | '-'
        | '='
        | '['
        | ']'
        | '\\'
        | ';'
        | '\''
        | ','
        | '.'
        | '/'
        | ' '
        | '\t'
        | '\n' => (ch, false),
        'A'..='Z' => (ch.to_ascii_lowercase(), true),
        '~' => ('`', true),
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        other => Err(McpError::invalid_params(
            format!("unmapped char '{other}'"),
            None,
        ))?,
    };
    let key_str = match raw {
        ' ' => "Space".to_owned(),
        '\t' => "Tab".to_owned(),
        '\n' => "Return".to_owned(),
        c => String::from(c),
    };
    let input: keyboard_codes::KeyboardInput = key_str
        .parse()
        .map_err(|e| McpError::invalid_params(format!("keycode parse '{ch}': {e}"), None))?;
    let code = u32::try_from(input.to_code(Platform::Linux))
        .map_err(|e| McpError::invalid_params(format!("keycode overflow '{ch}': {e}"), None))?;
    Ok((code, shifted))
}

fn parse_combo(key: &str) -> Result<(Vec<u32>, Option<u32>), McpError> {
    match keyboard_codes::parser::parse_shortcut_with_aliases(key) {
        Ok(shortcut) => {
            let mods: Vec<u32> = shortcut
                .modifiers
                .iter()
                .map(|m| {
                    u32::try_from(
                        keyboard_codes::KeyboardInput::Modifier(*m).to_code(Platform::Linux),
                    )
                    .map_err(|e| McpError::invalid_params(format!("modifier overflow: {e}"), None))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let main = Some(
                u32::try_from(shortcut.key.to_code(Platform::Linux))
                    .map_err(|e| McpError::invalid_params(format!("key overflow: {e}"), None))?,
            );
            Ok((mods, main))
        }
        Err(_parse_err) => match key.chars().next() {
            Some(ch) => {
                let (k, _shifted) = char_key(ch)?;
                Ok((Vec::new(), Some(k)))
            }
            None => Err(McpError::invalid_params(
                format!("empty key combo '{key}'"),
                None,
            )),
        },
    }
}

fn btn_code(btn: Option<&str>) -> Result<u32, McpError> {
    match btn {
        Some("left") | None => Ok(0x110),
        Some("right") => Ok(0x111),
        Some("middle") => Ok(0x112),
        Some(bad) => Err(McpError::invalid_params(
            format!("unknown button '{bad}' — use left/right/middle"),
            None,
        )),
    }
}


// ── KWin D-Bus proxies ──────────────────────────────────────────────────

#[zbus::proxy(
    interface = "org.kde.KWin.EIS.RemoteDesktop",
    default_service = "org.kde.KWin",
    default_path = "/org/kde/KWin/EIS/RemoteDesktop"
)]
trait KWinEis {
    #[zbus(name = "connectToEIS")]
    fn connect_to_eis(
        &self,
        capabilities: i32,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, i32)>;
}

#[zbus::proxy(
    interface = "org.kde.KWin.ScreenShot2",
    default_service = "org.kde.KWin",
    default_path = "/org/kde/KWin/ScreenShot2"
)]
trait KWinScreenShot2 {
    #[zbus(name = "CaptureWindow")]
    fn capture_window(
        &self,
        handle: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        pipe_fd: zbus::zvariant::OwnedFd,
    ) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
}

// ── EIS input ───────────────────────────────────────────────────────────

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
        let stream = std::os::unix::net::UnixStream::from(fd);
        let context = reis::ei::Context::new(stream)?;
        let resp = reis::handshake::ei_handshake_blocking(
            &context,
            "kwin-mcp",
            reis::ei::handshake::ContextType::Sender,
        )
        .map_err(|e| anyhow::anyhow!("EIS handshake: {e:?}"))?;
        context.flush()?;
        let mut conv = reis::event::EiEventConverter::new(&context, resp);
        let serial = conv.connection().serial();
        let (mut dev, mut kbd_d) = (None, None);
        let (mut abs, mut bt, mut sc, mut kb) = (None, None, None, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match (&dev, &kb) {
                (Some(_), Some(_)) => break,
                _ => match std::time::Instant::now() > deadline {
                    true => anyhow::bail!("EIS negotiation timed out"),
                    false => {}
                },
            }
            context.read()?;
            while let Some(pending) = context.pending_event() {
                match pending {
                    reis::PendingRequestResult::Request(ev) => {
                        conv.handle_event(ev)
                            .map_err(|e| anyhow::anyhow!("EIS event: {e:?}"))?;
                    }
                    reis::PendingRequestResult::ParseError(e) => {
                        anyhow::bail!("EIS parse: {e}")
                    }
                    reis::PendingRequestResult::InvalidObject(i) => {
                        anyhow::bail!("EIS invalid object: {i}")
                    }
                }
            }
            while let Some(ev) = conv.next_event() {
                match ev {
                    reis::event::EiEvent::SeatAdded(sa) => {
                        sa.seat.bind_capabilities(
                            reis::event::DeviceCapability::Pointer
                                | reis::event::DeviceCapability::PointerAbsolute
                                | reis::event::DeviceCapability::Button
                                | reis::event::DeviceCapability::Scroll
                                | reis::event::DeviceCapability::Keyboard,
                        );
                        context.flush()?;
                    }
                    reis::event::EiEvent::DeviceAdded(da) => {
                        let d = &da.device;
                        match d.has_capability(reis::event::DeviceCapability::PointerAbsolute) {
                            true => {
                                d.device().start_emulating(serial, 0);
                                abs = d.interface::<reis::ei::PointerAbsolute>();
                                bt = d.interface::<reis::ei::Button>();
                                sc = d.interface::<reis::ei::Scroll>();
                                dev = Some(d.device().clone());
                                match (d.interface::<reis::ei::Keyboard>(), &kb) {
                                    (Some(k), None) => {
                                        kb = Some(k);
                                        kbd_d = Some(d.device().clone());
                                    }
                                    _ => {}
                                }
                            }
                            false => match (
                                d.has_capability(reis::event::DeviceCapability::Keyboard),
                                &kb,
                            ) {
                                (true, None) => {
                                    d.device().start_emulating(serial, 0);
                                    kb = d.interface::<reis::ei::Keyboard>();
                                    kbd_d = Some(d.device().clone());
                                }
                                _ => {}
                            },
                        }
                        context.flush()?;
                    }
                    reis::event::EiEvent::Disconnected(_) => anyhow::bail!("EIS disconnected"),
                    reis::event::EiEvent::SeatRemoved(_)
                    | reis::event::EiEvent::DeviceRemoved(_)
                    | reis::event::EiEvent::DevicePaused(_)
                    | reis::event::EiEvent::DeviceResumed(_)
                    | reis::event::EiEvent::KeyboardModifiers(_)
                    | reis::event::EiEvent::Frame(_)
                    | reis::event::EiEvent::DeviceStartEmulating(_)
                    | reis::event::EiEvent::DeviceStopEmulating(_)
                    | reis::event::EiEvent::PointerMotion(_)
                    | reis::event::EiEvent::PointerMotionAbsolute(_)
                    | reis::event::EiEvent::Button(_)
                    | reis::event::EiEvent::ScrollDelta(_)
                    | reis::event::EiEvent::ScrollStop(_)
                    | reis::event::EiEvent::ScrollCancel(_)
                    | reis::event::EiEvent::ScrollDiscrete(_)
                    | reis::event::EiEvent::KeyboardKey(_)
                    | reis::event::EiEvent::TouchDown(_)
                    | reis::event::EiEvent::TouchUp(_)
                    | reis::event::EiEvent::TouchMotion(_)
                    | reis::event::EiEvent::TouchCancel(_) => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(Self {
            context,
            abs_ptr: abs.ok_or_else(|| anyhow::anyhow!("no EIS pointer"))?,
            btn: bt.ok_or_else(|| anyhow::anyhow!("no EIS button"))?,
            scroll: sc.ok_or_else(|| anyhow::anyhow!("no EIS scroll"))?,
            kbd: kb.ok_or_else(|| anyhow::anyhow!("no EIS keyboard"))?,
            ptr_dev: dev.ok_or_else(|| anyhow::anyhow!("no EIS ptr device"))?,
            kbd_dev: kbd_d.ok_or_else(|| anyhow::anyhow!("no EIS kbd device"))?,
            serial,
        })
    }

    fn move_abs(&self, x: f32, y: f32) -> anyhow::Result<()> {
        self.abs_ptr.motion_absolute(x, y);
        self.ptr_dev.frame(self.serial, 0);
        Ok(self.context.flush()?)
    }

    fn button(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed {
            true => reis::ei::button::ButtonState::Press,
            false => reis::ei::button::ButtonState::Released,
        };
        self.btn.button(code, st);
        self.ptr_dev.frame(self.serial, 0);
        Ok(self.context.flush()?)
    }

    fn scroll_discrete(&self, dx: i32, dy: i32) -> anyhow::Result<()> {
        self.scroll.scroll_discrete(dx, dy);
        self.scroll.scroll_stop(0, 0, 0);
        self.ptr_dev.frame(self.serial, 0);
        Ok(self.context.flush()?)
    }

    fn scroll_smooth(&self, dx: f32, dy: f32) -> anyhow::Result<()> {
        self.scroll.scroll(dx, dy);
        self.scroll.scroll_stop(0, 0, 0);
        self.ptr_dev.frame(self.serial, 0);
        Ok(self.context.flush()?)
    }

    fn key(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed {
            true => reis::ei::keyboard::KeyState::Press,
            false => reis::ei::keyboard::KeyState::Released,
        };
        self.kbd.key(code, st);
        self.kbd_dev.frame(self.serial, 0);
        Ok(self.context.flush()?)
    }
}

fn log_tail(path: &std::path::Path, lines: usize) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut tail = contents.lines().rev().take(lines).collect::<Vec<_>>();
    tail.reverse();
    let joined = tail.join(" | ").trim().to_owned();
    (!joined.is_empty()).then_some(joined)
}

fn startup_diagnostics(host_xdg_dir: &std::path::Path) -> String {
    let mut details = Vec::new();
    for name in [
        "bootstrap.log",
        "dbus.log",
        "kwin.log",
        "portal.log",
        "portal-kde.log",
        "pipewire.log",
        "wireplumber.log",
        "atspi.log",
    ] {
        match log_tail(&host_xdg_dir.join(name), 6) {
            Some(tail) => details.push(format!("{name}: {tail}")),
            None => {}
        }
    }
    match details.is_empty() {
        true => String::new(),
        false => format!(" diagnostics: {}", details.join(" ; ")),
    }
}

fn rewrite_bus_address_for_host(
    address: &str,
    container_dir: &str,
    host_dir: &std::path::Path,
) -> String {
    let container_prefix = format!("unix:path={container_dir}");
    let host_prefix = format!("unix:path={}", host_dir.display());
    match address.strip_prefix(&container_prefix) {
        Some(rest) => format!("{host_prefix}{rest}"),
        None => address.to_owned(),
    }
}

async fn wait_for_nonempty_file(
    path: &std::path::Path,
    description: &str,
    deadline: std::time::Instant,
) -> Result<String, String> {
    loop {
        match std::fs::read_to_string(path) {
            Ok(contents) => match contents.trim() {
                "" => {}
                trimmed => return Ok(trimmed.to_owned()),
            },
            Err(e) => {
                #[expect(clippy::wildcard_enum_match_arm)]
                match e.kind() {
                    std::io::ErrorKind::NotFound => {}
                    other => return Err(format!(
                        "failed to read {description} at {} ({other:?}): {e}",
                        path.display()
                    )),
                }
            }
        }
        match std::time::Instant::now() >= deadline {
            true => return Err(format!(
                "{description} did not appear at {} within {}s",
                path.display(),
                STARTUP_TIMEOUT.as_secs()
            )),
            false => tokio::time::sleep(STARTUP_POLL).await,
        }
    }
}

async fn connect_session_bus(
    address: &str,
    deadline: std::time::Instant,
) -> Result<zbus::Connection, String> {
    loop {
        let attempt_error = match zbus::connection::Builder::address(address) {
            Ok(builder) => match builder.build().await {
                Ok(conn) => return Ok(conn),
                Err(e) => e.to_string(),
            },
            Err(e) => return Err(format!("invalid D-Bus address '{address}': {e}")),
        };
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "failed to connect to session bus at {address} within {}s: {attempt_error}",
                STARTUP_TIMEOUT.as_secs(),
            ));
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

// ── Session ──────────────────────────────────────────────────────────────

struct Session {
    zbus_conn: zbus::Connection,
    eis: Eis,
    container: hakoniwa::Container,
    container_child: hakoniwa::Child,
    container_stdin: std::io::PipeWriter,
    host_xdg_dir: std::path::PathBuf,
}

// ── Server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct KwinMcp {
    session: Arc<tokio::sync::Mutex<Option<Session>>>,
}

impl KwinMcp {
    fn new() -> Self {
        Self {
            session: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
    async fn with_session<R>(
        &self,
        f: impl FnOnce(&Session) -> Result<R, McpError>,
    ) -> Result<R, McpError> {
        let guard = self.session.lock().await;
        match &*guard {
            Some(s) => f(s),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    async fn zbus_conn(&self) -> Result<zbus::Connection, McpError> {
        let guard = self.session.lock().await;
        match &*guard {
            Some(s) => Ok(s.zbus_conn.clone()),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    async fn host_xdg_dir(&self) -> Result<std::path::PathBuf, McpError> {
        let guard = self.session.lock().await;
        match &*guard {
            Some(s) => Ok(s.host_xdg_dir.clone()),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
}

fn eis_err(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn teardown_container(
    container: hakoniwa::Container,
    mut container_child: hakoniwa::Child,
    mut container_stdin: std::io::PipeWriter,
    host_xdg_dir: &std::path::Path,
) {
    use std::io::Write;
    // Kill bash's children, NOT kill 0 (kills shared process group = kwin-mcp too)
    match writeln!(container_stdin, "pkill -P $$") {
        Err(e) => eprintln!("teardown kill 0: {e}"),
        Ok(()) => {}
    }
    drop(container_stdin);
    match container_child.kill() {
        Err(e) => eprintln!("teardown kill: {e}"),
        Ok(()) => {}
    }
    match container_child.wait() {
        Err(e) => eprintln!("teardown wait: {e}"),
        Ok(_) => {}
    }
    drop(container);
    match std::fs::remove_dir_all(host_xdg_dir) {
        Err(e) => eprintln!("teardown cleanup: {e}"),
        Ok(()) => {}
    }
}

fn teardown(sess: Session) {
    teardown_container(
        sess.container,
        sess.container_child,
        sess.container_stdin,
        &sess.host_xdg_dir,
    );
}

async fn active_window_info(conn: &zbus::Connection, host_xdg_dir: &std::path::Path) -> Result<(i32, i32, String), McpError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(eis_err)?
        .as_millis();
    let marker = format!("kwin-mcp-{ts}");
    let cb_path = format!("/KWinMCP/{ts}");
    let our_name = conn
        .unique_name()
        .ok_or_else(|| McpError::internal_error("no bus name", None))?
        .to_string();
    let script = format!(
        "var w = workspace.activeWindow;\
        callDBus('{our_name}','{cb_path}','org.kde.KWinMCP','result',\
        w ? JSON.stringify({{x:w.frameGeometry.x,y:w.frameGeometry.y,\
        w:w.frameGeometry.width,h:w.frameGeometry.height,\
        title:w.caption,id:w.internalId.toString()}}) : 'null');"
    );
    let script_name = format!("{marker}.js");
    let script_file = host_xdg_dir.join(&script_name);
    std::fs::write(&script_file, &script).map_err(eis_err)?;
    // KWin sees /tmp/xdg/ inside the container
    let container_script_path = format!("/tmp/xdg/{script_name}");
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let cb = KWinCallback {
        tx: std::sync::Mutex::new(Some(tx)),
    };
    let obj_path = zbus::zvariant::ObjectPath::try_from(cb_path.as_str()).map_err(eis_err)?;
    let registered = conn
        .object_server()
        .at(&obj_path, cb)
        .await
        .map_err(eis_err)?;
    eprintln!("active_window_info: our_name={our_name} path={cb_path} registered={registered}");
    match registered {
        true => {}
        false => {
            return Err(McpError::internal_error(
                format!("failed to register callback at {cb_path}"),
                None,
            ));
        }
    }
    // Load and run the script
    let scripting: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")
        .map_err(eis_err)?
        .path("/Scripting")
        .map_err(eis_err)?
        .interface("org.kde.kwin.Scripting")
        .map_err(eis_err)?
        .build()
        .await
        .map_err(eis_err)?;
    let (script_id,): (i32,) = scripting
        .call("loadScript", &(&container_script_path, &marker))
        .await
        .map_err(eis_err)?;
    match script_id >= 0 {
        true => {}
        false => {
            conn.object_server()
                .remove::<KWinCallback, _>(&obj_path)
                .await
                .map_err(eis_err)?;
            std::fs::remove_file(&script_file).map_err(eis_err)?;
            return Err(McpError::internal_error(
                format!("KWin loadScript failed, id={script_id}"),
                None,
            ));
        }
    }
    let script_proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")
        .map_err(eis_err)?
        .path(format!("/Scripting/Script{script_id}"))
        .map_err(eis_err)?
        .interface("org.kde.kwin.Script")
        .map_err(eis_err)?
        .build()
        .await
        .map_err(eis_err)?;
    script_proxy
        .call::<_, (), ()>("run", &())
        .await
        .map_err(eis_err)?;
    // Wait for callback, then cleanup regardless of result
    let json_result = rx
        .await
        .map_err(|_| McpError::internal_error("KWin callback channel closed", None));
    conn.object_server()
        .remove::<KWinCallback, _>(&obj_path)
        .await
        .map_err(eis_err)?;
    let (_,): (bool,) = scripting
        .call("unloadScript", &(&marker,))
        .await
        .map_err(eis_err)?;
    std::fs::remove_file(&script_file).map_err(eis_err)?;
    let json = json_result?;
    match json.as_str() {
        "null" => {
            return Err(McpError::internal_error(
                "KWin script error: No active window",
                None,
            ));
        }
        _ => {}
    }
    let v: serde_json::Value = serde_json::from_str(&json).map_err(eis_err)?;
    let x = v
        .get("x")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| McpError::internal_error("no x", None))?;
    let y = v
        .get("y")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| McpError::internal_error("no y", None))?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    #[expect(clippy::as_conversions)]
    Ok((x.round() as i32, y.round() as i32, id))
}

struct KWinCallback {
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
}

#[zbus::interface(name = "org.kde.KWinMCP")]
impl KWinCallback {
    #[zbus(name = "result")]
    fn result(&self, payload: String) {
        match self.tx.lock() {
            Ok(mut g) => match g.take() {
                Some(tx) => match tx.send(payload) {
                    Ok(()) => {}
                    Err(e) => eprintln!("callback send failed: {e}"),
                },
                None => {}
            },
            Err(e) => eprintln!("callback lock poisoned: {e}"),
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
        format!(
            "{}{}\t{}\t{}\t{:?}",
            "  ".repeat(depth),
            self.role,
            self.name,
            self.states.join("|"),
            self.bounds
        )
    }

    fn is_useful(&self) -> bool {
        let (x, y, w, h) = self.bounds;
        w > 1 && h > 1 && x > -1000000 && y > -1000000 && !self.name.is_empty()
    }
}

fn state_labels(states: &[String]) -> Vec<String> {
    let has = |want: &str| states.iter().any(|s| s == want);
    [
        (
            has("Active") || has("Editable") || has("Checked"),
            "current",
        ),
        (has("Enabled") || has("Sensitive"), "enabled"),
        (has("Focused"), "focused"),
        (has("Focusable"), "focusable"),
        (has("ReadOnly"), "readonly"),
        (has("Transient"), "transient"),
        (has("Checkable"), "checkable"),
        (has("Showing") || has("Visible"), "visible"),
    ]
    .into_iter()
    .filter_map(|(yes, label)| yes.then_some(label.to_owned()))
    .collect()
}

async fn atspi_node(
    acc: &atspi::proxy::accessible::AccessibleProxy<'_>,
) -> Result<AtspiNode, McpError> {
    use atspi::proxy::proxy_ext::ProxyExt;
    let name = acc.name().await.unwrap_or_default();
    let role = acc.get_role_name().await.unwrap_or_default();
    let raw_states = acc
        .get_state()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>();
    let states = state_labels(&raw_states);
    let bounds = match acc.proxies().await.map_err(eis_err)?.component().await {
        Ok(c) => c
            .get_extents(atspi::CoordType::Screen)
            .await
            .unwrap_or_default(),
        Err(_) => (0, 0, 0, 0),
    };
    Ok(AtspiNode {
        name,
        role,
        states,
        bounds,
    })
}

// ── Tool parameter structs ──────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct SessionStartParams {}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseClickParams {
    x: FlexInt,
    y: FlexInt,
    button: Option<String>,
    double: Option<bool>,
    triple: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseMoveParams {
    x: FlexInt,
    y: FlexInt,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseScrollParams {
    x: FlexInt,
    y: FlexInt,
    delta: FlexInt,
    horizontal: Option<bool>,
    discrete: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseDragParams {
    from_x: FlexInt,
    from_y: FlexInt,
    to_x: FlexInt,
    to_y: FlexInt,
    button: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyboardTypeParams {
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyboardKeyParams {
    key: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LaunchAppParams {
    command: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AccessibilityTreeParams {
    app_name: Option<String>,
    max_depth: Option<u32>,
    role: Option<String>,
    show_elements: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FindUiElementsParams {
    query: String,
}

// ── Tool implementations ────────────────────────────────────────────────

impl rmcp::ServerHandler for KwinMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kwin-mcp", "0.1.0"))
            .with_instructions("KDE Wayland desktop automation. Call session_start first. Coordinates are pixels on a 1920x1080 screen.")
    }
}

#[rmcp::tool_router]
impl KwinMcp {
    #[rmcp::tool(
        name = "session_start",
        description = "Start an isolated KDE Wayland session in a container for GUI automation. Must be called before any other tool."
    )]
    async fn session_start(
        &self,
        Parameters(_params): Parameters<SessionStartParams>,
    ) -> Result<CallToolResult, McpError> {
        eprintln!(
            "kwin-mcp v{}.{} ({}) session_start",
            env!("CARGO_PKG_VERSION"),
            env!("BUILD_NUMBER"),
            env!("GIT_HASH")
        );
        let version_stamp = format!(
            "kwin-mcp v{}.{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("BUILD_NUMBER"),
            env!("GIT_HASH")
        );
        let ver_err = |e: String| McpError::internal_error(format!("{version_stamp} — {e}"), None);
        {
            let mut guard = self.session.lock().await;
            match (*guard).take() {
                Some(old) => teardown(old),
                None => {}
            }
        }
        eprintln!("session_start: previous session cleared");
        let pid = std::process::id();
        let uid = unsafe { nix::libc::geteuid() };
        let runtime_dir = format!("/run/user/{uid}");
        let host_xdg_dir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
        match std::fs::remove_dir_all(&host_xdg_dir) {
            Err(_) => {}
            Ok(()) => {}
        }
        std::fs::create_dir_all(&host_xdg_dir).map_err(|e| ver_err(e.to_string()))?;
        eprintln!(
            "session_start: host_xdg_dir ready path={}",
            host_xdg_dir.display()
        );
        let home = std::env::var("HOME").map_err(|e| ver_err(e.to_string()))?;
        let bus_address_path = host_xdg_dir.join("dbus.address");
        // Build container with isolated namespaces
        let mut container = hakoniwa::Container::new();
        container.rootfs("/").map_err(|e| ver_err(e.to_string()))?;
        container.devfsmount("/dev");
        container.bindmount_rw("/dev/dri", "/dev/dri");
        container.bindmount_rw("/dev/fuse", "/dev/fuse");
        container.tmpfsmount("/run");
        container.tmpfsmount("/tmp");
        container.runctl(hakoniwa::Runctl::MountFallback);
        container.bindmount_rw(&host_xdg_dir.to_string_lossy(), "/tmp/xdg");
        container.bindmount_ro(&home, &home);
        container.share(hakoniwa::Namespace::Pid);
        container.bindmount_rw("/proc", "/proc");
        container.unshare(hakoniwa::Namespace::Network);
        eprintln!("session_start: container configuration ready");
        // Entrypoint: start services sequentially with readiness checks
        let xdg_inner = "/tmp/xdg";
        let entrypoint = format!(
            "\
set -u\n\
ulimit -c 0\n\
mkdir -p {runtime_dir} /tmp/cache /dev/dri\n\
chmod 700 {runtime_dir}\n\
printf '#!/bin/sh\\nexit 0\\n' > /tmp/kdialog && chmod +x /tmp/kdialog\n\
dbus-daemon --session --address='unix:path={xdg_inner}/bus' --print-address=3 --print-pid=4 --nofork 3>{xdg_inner}/dbus.address 4>{xdg_inner}/dbus.pid 2>{xdg_inner}/dbus.log &\n\
dbus_pid=$!\n\
n=0; while [ ! -s {xdg_inner}/dbus.address ] && kill -0 \"$dbus_pid\" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
if [ ! -s {xdg_inner}/dbus.address ]; then echo 'dbus-daemon did not announce an address' >> {xdg_inner}/bootstrap.log; wait \"$dbus_pid\" || true; exit 1; fi\n\
KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 KWIN_COMPOSE=O2 kwin_wayland --virtual --width 1920 --height 1080 2>{xdg_inner}/kwin.log &\n\
kwin_pid=$!\n\
n=0; while [ ! -S {runtime_dir}/wayland-0 ] && kill -0 \"$dbus_pid\" 2>/dev/null && kill -0 \"$kwin_pid\" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
if ! kill -0 \"$kwin_pid\" 2>/dev/null; then echo 'kwin_wayland exited before creating wayland-0' >> {xdg_inner}/bootstrap.log; wait \"$kwin_pid\" || true; exit 1; fi\n\
if [ ! -S {runtime_dir}/wayland-0 ]; then echo 'kwin_wayland did not create wayland-0' >> {xdg_inner}/bootstrap.log; exit 1; fi\n\
if ! dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR XDG_CURRENT_DESKTOP XDG_SESSION_TYPE PATH HOME USER QT_QPA_PLATFORM=wayland 2>>{xdg_inner}/bootstrap.log; then echo 'dbus-update-activation-environment failed' >> {xdg_inner}/bootstrap.log; exit 1; fi\n\
pipewire 2>{xdg_inner}/pipewire.log &\n\
at-spi-bus-launcher 2>{xdg_inner}/atspi.log &\n\
wireplumber 2>{xdg_inner}/wireplumber.log &\n\
while read -r cmd; do eval \"$cmd\" & done\n"
        );
        let mut cmd = container.command("/bin/bash");
        cmd.arg("-c").arg(entrypoint.as_str());
        cmd.env(
            "PATH",
            "/tmp:/usr/bin:/usr/sbin:/bin:/sbin:/usr/lib:/usr/libexec:/usr/lib/at-spi2-core",
        );
        cmd.env("HOME", &home);
        let user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
        cmd.env("USER", &user);
        cmd.env("XDG_RUNTIME_DIR", runtime_dir.as_str());
        cmd.env("XDG_CACHE_HOME", "/tmp/cache");
        cmd.env("XDG_DATA_HOME", "/tmp/state");
        cmd.env("XDG_SESSION_TYPE", "wayland");
        cmd.env("XDG_CURRENT_DESKTOP", "KDE");
        cmd.env("QT_QPA_PLATFORM", "wayland");
        let dbus_addr = format!("unix:path={xdg_inner}/bus");
        cmd.env("DBUS_SESSION_BUS_ADDRESS", dbus_addr.as_str());
        cmd.env("WAYLAND_DISPLAY", "wayland-0");
        cmd.env("LIBGL_ALWAYS_SOFTWARE", "1");
        cmd.env("GALLIUM_DRIVER", "llvmpipe");
        cmd.env("KDE_DEBUG", "0");
        cmd.stdin(hakoniwa::Stdio::piped());
        eprintln!("session_start: command environment ready");
        let devnull = std::fs::File::options()
            .write(true)
            .open("/dev/null")
            .map_err(|e| ver_err(format!("open /dev/null for container stdout: {e}")))?;
        cmd.stdout(devnull);
        cmd.stderr(hakoniwa::Stdio::inherit());
        eprintln!("session_start: spawn container child");
        let mut container_child = cmd.spawn().map_err(|e| ver_err(e.to_string()))?;
        eprintln!(
            "session_start: container child spawned pid={}",
            container_child.id()
        );
        match container_child.try_wait() {
            Ok(Some(status)) => {
                eprintln!("session_start: container child exited immediately status={status:?}")
            }
            Ok(None) => eprintln!("session_start: container child still running after spawn"),
            Err(e) => eprintln!("session_start: container child try_wait failed: {e}"),
        }
        eprintln!(
            "session_start: container child stdin available={}",
            container_child.stdin.is_some()
        );
        let container_stdin = match container_child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let diagnostics = startup_diagnostics(&host_xdg_dir);
                match container_child.kill() {
                    Err(e) => eprintln!("teardown kill: {e}"),
                    Ok(()) => {}
                }
                match container_child.wait() {
                    Err(e) => eprintln!("teardown wait: {e}"),
                    Ok(_) => {}
                }
                drop(container);
                match std::fs::remove_dir_all(&host_xdg_dir) {
                    Err(e) => eprintln!("teardown cleanup: {e}"),
                    Ok(()) => {}
                }
                return Err(ver_err(format!(
                    "container stdin not available{diagnostics}"
                )));
            }
        };
        eprintln!("session_start: container stdin ready");
        let cleanup_err = |message: String,
                           container: hakoniwa::Container,
                           container_child: hakoniwa::Child,
                           container_stdin: std::io::PipeWriter| {
            let diagnostics = startup_diagnostics(&host_xdg_dir);
            eprintln!("session_start: startup error: {message}");
            if std::env::var_os("KWIN_MCP_DEBUG_SKIP_STARTUP_TEARDOWN").is_some() {
                eprintln!(
                    "session_start: debug skip teardown host_xdg_dir={}",
                    host_xdg_dir.display()
                );
                std::mem::forget(container);
                std::mem::forget(container_child);
                std::mem::forget(container_stdin);
                Err(ver_err(format!(
                    "{message}{diagnostics} (teardown skipped; host_xdg_dir={})",
                    host_xdg_dir.display()
                )))
            } else {
                teardown_container(container, container_child, container_stdin, &host_xdg_dir);
                Err(ver_err(format!("{message}{diagnostics}")))
            }
        };
        eprintln!(
            "session_start: wait for dbus address path={}",
            bus_address_path.display()
        );
        let bus_addr_raw = match wait_for_nonempty_file(
            &bus_address_path,
            "D-Bus address",
            std::time::Instant::now() + STARTUP_TIMEOUT,
        )
        .await
        {
            Ok(addr) => addr,
            Err(e) => return cleanup_err(e, container, container_child, container_stdin),
        };
        eprintln!("session_start: dbus address ready");
        let bus_addr = rewrite_bus_address_for_host(&bus_addr_raw, xdg_inner, &host_xdg_dir);
        eprintln!("session_start: host dbus address {bus_addr}");
        eprintln!("session_start: connect to session bus");
        let zbus_conn =
            match connect_session_bus(&bus_addr, std::time::Instant::now() + STARTUP_TIMEOUT).await
            {
                Ok(conn) => conn,
                Err(e) => return cleanup_err(e, container, container_child, container_stdin),
            };
        eprintln!("session_start: connected to session bus");
        // Wait for KWin to register on D-Bus
        eprintln!("session_start: wait for org.kde.KWin");
        let kwin_deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
        let dbus_proxy = zbus::fdo::DBusProxy::new(&zbus_conn)
            .await
            .map_err(|e| ver_err(format!("DBus proxy: {e}")))?;
        loop {
            let kwin_name = zbus::names::BusName::try_from("org.kde.KWin")
                .map_err(|e| ver_err(format!("invalid bus name: {e}")))?;
            match dbus_proxy.name_has_owner(kwin_name).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => return cleanup_err(format!("name_has_owner: {e}"), container, container_child, container_stdin),
            }
            match std::time::Instant::now() >= kwin_deadline {
                true => return cleanup_err("org.kde.KWin did not appear on D-Bus".to_owned(), container, container_child, container_stdin),
                false => tokio::time::sleep(STARTUP_POLL).await,
            }
        }
        eprintln!("session_start: org.kde.KWin ready");
        // Connect to KWin EIS for input injection
        eprintln!("session_start: connect to KWin EIS");
        let eis_proxy = match KWinEisProxy::new(&zbus_conn).await {
            Ok(p) => p,
            Err(e) => return cleanup_err(format!("KWin EIS proxy: {e}"), container, container_child, container_stdin),
        };
        // capabilities: 1=keyboard, 2=pointer, 4=touch → 3 = keyboard+pointer
        let (eis_fd, _cookie) = match eis_proxy.connect_to_eis(3).await {
            Ok(r) => r,
            Err(e) => return cleanup_err(format!("connectToEIS: {e}"), container, container_child, container_stdin),
        };
        eprintln!("session_start: EIS fd received, negotiating");
        let eis_owned_fd = std::os::fd::OwnedFd::from(eis_fd);
        let eis = match tokio::task::spawn_blocking(move || Eis::from_fd(eis_owned_fd)).await {
            Ok(Ok(eis)) => eis,
            Ok(Err(e)) => return cleanup_err(format!("EIS negotiation: {e}"), container, container_child, container_stdin),
            Err(e) => return cleanup_err(format!("EIS task: {e}"), container, container_child, container_stdin),
        };
        eprintln!("session_start: EIS ready");
        let bus_name = zbus_conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let msg = format!("{version_stamp} — session started bus={bus_name}");
        let mut guard = self.session.lock().await;
        *guard = Some(Session {
            zbus_conn,
            eis,
            container,
            container_child,
            container_stdin,
            host_xdg_dir,
        });
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[rmcp::tool(
        name = "session_stop",
        description = "Stop the KWin session and clean up all processes.",
        annotations(destructive_hint = true)
    )]
    async fn session_stop(&self) -> Result<CallToolResult, McpError> {
        let mut guard = self.session.lock().await;
        match (*guard).take() {
            Some(sess) => {
                teardown(sess);
                Ok(CallToolResult::success(vec![Content::text(
                    "session stopped",
                )]))
            }
            None => Ok(CallToolResult::success(vec![Content::text(
                "no session running",
            )])),
        }
    }

    #[rmcp::tool(
        name = "screenshot",
        description = "Take a screenshot via the Screenshot portal. Returns the file URI.",
        annotations(read_only_hint = true)
    )]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let conn = self.zbus_conn().await?;
        let xdg = self.host_xdg_dir().await?;
        let (_, _, win_id) = active_window_info(&conn, &xdg).await?;
        let proxy = KWinScreenShot2Proxy::new(&conn).await.map_err(eis_err)?;
        let (read_fd, write_fd) = nix::unistd::pipe().map_err(eis_err)?;
        let pipe_fd = zbus::zvariant::OwnedFd::from(write_fd);
        let mut opts = std::collections::HashMap::new();
        opts.insert("include-cursor", zbus::zvariant::Value::from(true));
        opts.insert("include-decoration", zbus::zvariant::Value::from(true));
        opts.insert("hide-caller-windows", zbus::zvariant::Value::from(false));
        let meta = proxy
            .capture_window(&win_id, opts, pipe_fd)
            .await
            .map_err(eis_err)?;
        let get_u32 = |k: &str| -> Result<u32, McpError> {
            let val = meta
                .get(k)
                .ok_or_else(|| McpError::internal_error(format!("screenshot: no {k}"), None))?;
            let n: u32 = val.try_into().map_err(eis_err)?;
            Ok(n)
        };
        let (width, height, stride) = (get_u32("width")?, get_u32("height")?, get_u32("stride")?);
        let reader_file = std::fs::File::from(read_fd);
        let total = usize::try_from(stride * height).map_err(eis_err)?;
        let mut pixels = vec![0u8; total];
        std::io::Read::read_exact(&mut std::io::BufReader::new(reader_file), &mut pixels)
            .map_err(eis_err)?;
        // BGRA premultiplied → RGBA
        let px = usize::try_from(width * height).map_err(eis_err)?;
        let mut rgba = vec![0u8; px * 4];
        for row in 0..height {
            for col in 0..width {
                let si = usize::try_from(row * stride + col * 4).map_err(eis_err)?;
                let di = usize::try_from((row * width + col) * 4).map_err(eis_err)?;
                rgba[di] = pixels[si + 2];
                rgba[di + 1] = pixels[si + 1];
                rgba[di + 2] = pixels[si];
                rgba[di + 3] = pixels[si + 3];
            }
        }
        let path = xdg.join("screenshot.png");
        let file = std::fs::File::create(&path).map_err(eis_err)?;
        let mut enc = png::Encoder::new(file, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(eis_err)?;
        writer.write_image_data(&rgba).map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{} size={}x{}",
            path.to_string_lossy(),
            width,
            height
        ))]))
    }

    #[rmcp::tool(
        name = "accessibility_tree",
        description = "Get AT-SPI2 accessibility tree with widget roles, names, states, bounding boxes. By default hides zero-rect/internal nodes; set show_elements=true to include them.",
        annotations(read_only_hint = true)
    )]
    async fn accessibility_tree(
        &self,
        Parameters(params): Parameters<AccessibilityTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        use atspi::proxy::accessible::ObjectRefExt;
        let conn = atspi::AccessibilityConnection::new()
            .await
            .map_err(eis_err)?;
        let root = conn.root_accessible_on_registry().await.map_err(eis_err)?;
        let limit = usize::try_from(params.max_depth.unwrap_or(8)).map_err(eis_err)?;
        let app_name = params.app_name.map(|s| s.to_lowercase());
        let role = params.role.map(|s| s.to_lowercase());
        let show_elements = params.show_elements.unwrap_or(false);
        let mut out = Vec::new();
        let mut stack = root
            .get_children()
            .await
            .map_err(eis_err)?
            .into_iter()
            .rev()
            .map(|obj| (obj, 0usize))
            .collect::<Vec<_>>();
        while let Some((obj, depth)) = stack.pop() {
            let acc = obj
                .as_accessible_proxy(conn.connection())
                .await
                .map_err(eis_err)?;
            let node = atspi_node(&acc).await?;
            match (
                depth,
                app_name
                    .as_ref()
                    .map(|needle| node.name.to_lowercase().contains(needle))
                    .unwrap_or(true),
            ) {
                (0, false) => continue,
                (_, _) => {}
            }
            let dominated = role
                .as_ref()
                .map(|needle| node.role.to_lowercase().contains(needle))
                .unwrap_or(true)
                && (show_elements || node.is_useful());
            match dominated {
                true => out.push(node.line(depth)),
                false => {}
            }
            let child_depth = match dominated {
                true => depth + 1,
                false => depth,
            };
            match child_depth <= limit {
                true => {
                    for child in acc
                        .get_children()
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .rev()
                    {
                        stack.push((child, child_depth));
                    }
                }
                false => {}
            }
        }
        Ok(CallToolResult::success(vec![Content::text(out.join("\n"))]))
    }

    #[rmcp::tool(
        name = "find_ui_elements",
        description = "Search UI elements by name/role/description (case-insensitive).",
        annotations(read_only_hint = true)
    )]
    async fn find_ui_elements(
        &self,
        Parameters(params): Parameters<FindUiElementsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.with_session(|_sess| {
            Err(McpError::internal_error(
                format!("AT-SPI2 search not yet implemented: {}", params.query),
                None,
            ))
        })
        .await
    }

    #[rmcp::tool(
        name = "mouse_click",
        description = "Click at window-relative pixel coordinates. button: left/right/middle. double/triple for multi-click."
    )]
    async fn mouse_click(
        &self,
        Parameters(params): Parameters<MouseClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = parse_int(params.x);
        let y = parse_int(params.y);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?, &self.host_xdg_dir().await?).await?;
        let code = btn_code(params.button.as_deref())?;
        let count = match (params.triple, params.double) {
            (Some(true), _) => 3,
            (_, Some(true)) => 2,
            (Some(false) | None, Some(false) | None) => 1,
        };
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(eis_err)?), f32::from(i16::try_from(wy + y).map_err(eis_err)?));
        sess.eis.move_abs(ax, ay).map_err(eis_err)?;
        for n in 0..count {
            match n {
                0 => {}
                _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
            sess.eis.button(code, true).map_err(eis_err)?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sess.eis.button(code, false).map_err(eis_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "clicked ({x},{y}) x{count}"
        ))]))
    }

    #[rmcp::tool(
        name = "mouse_move",
        description = "Move cursor to window-relative pixel coordinates. Triggers hover effects.",
        annotations(read_only_hint = true)
    )]
    async fn mouse_move(
        &self,
        Parameters(params): Parameters<MouseMoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = parse_int(params.x);
        let y = parse_int(params.y);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?, &self.host_xdg_dir().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(eis_err)?), f32::from(i16::try_from(wy + y).map_err(eis_err)?));
        sess.eis.move_abs(ax, ay).map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "moved ({x},{y})"
        ))]))
    }

    #[rmcp::tool(
        name = "mouse_scroll",
        description = "Scroll at window-relative pixel coords. delta: positive=down/right, negative=up/left. horizontal/discrete are optional."
    )]
    async fn mouse_scroll(
        &self,
        Parameters(params): Parameters<MouseScrollParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = parse_int(params.x);
        let y = parse_int(params.y);
        let delta = parse_int(params.delta);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?, &self.host_xdg_dir().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(eis_err)?), f32::from(i16::try_from(wy + y).map_err(eis_err)?));
        sess.eis.move_abs(ax, ay).map_err(eis_err)?;
        let horiz = params.horizontal.unwrap_or_default();
        match params.discrete.unwrap_or_default() {
            true => {
                let (dx, dy) = match horiz {
                    true => (delta, 0),
                    false => (0, delta),
                };
                sess.eis.scroll_discrete(dx, dy).map_err(eis_err)?;
            }
            false => {
                let (dx, dy) = match horiz {
                    true => (f32::from(i16::try_from(delta).map_err(eis_err)?) * 15.0, 0.0),
                    false => (0.0, f32::from(i16::try_from(delta).map_err(eis_err)?) * 15.0),
                };
                sess.eis.scroll_smooth(dx, dy).map_err(eis_err)?;
            }
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "scrolled {delta} at ({x},{y})"
        ))]))
    }

    #[rmcp::tool(
        name = "mouse_drag",
        description = "Drag between window-relative pixel coords. Smooth 20-step interpolation. button: left/right/middle."
    )]
    async fn mouse_drag(
        &self,
        Parameters(params): Parameters<MouseDragParams>,
    ) -> Result<CallToolResult, McpError> {
        let from_x = parse_int(params.from_x);
        let from_y = parse_int(params.from_y);
        let to_x = parse_int(params.to_x);
        let to_y = parse_int(params.to_y);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?, &self.host_xdg_dir().await?).await?;
        let code = btn_code(params.button.as_deref())?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let ax = f32::from(i16::try_from(wx + from_x).map_err(eis_err)?);
        let ay = f32::from(i16::try_from(wy + from_y).map_err(eis_err)?);
        sess.eis.move_abs(ax, ay).map_err(eis_err)?;
        sess.eis.button(code, true).map_err(eis_err)?;
        let steps = 20i32;
        for step in 1..=steps {
            let cx = f32::from(i16::try_from(wx + from_x + (to_x - from_x) * step / steps).map_err(eis_err)?);
            let cy = f32::from(i16::try_from(wy + from_y + (to_y - from_y) * step / steps).map_err(eis_err)?);
            sess.eis.move_abs(cx, cy).map_err(eis_err)?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        sess.eis.button(code, false).map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "dragged ({from_x},{from_y})->({to_x},{to_y})"
        ))]))
    }

    #[rmcp::tool(
        name = "keyboard_type",
        description = "Type ASCII text character by character. For non-ASCII use keyboard_type_unicode."
    )]
    async fn keyboard_type(
        &self,
        Parameters(params): Parameters<KeyboardTypeParams>,
    ) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        for ch in params.text.chars() {
            let (code, needs_shift) = char_key(ch)?;
            match needs_shift {
                true => sess.eis.key(42, true).map_err(eis_err)?,
                false => {}
            }
            sess.eis.key(code, true).map_err(eis_err)?;
            sess.eis.key(code, false).map_err(eis_err)?;
            match needs_shift {
                true => sess.eis.key(42, false).map_err(eis_err)?,
                false => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "typed: {}",
            params.text
        ))]))
    }

    #[rmcp::tool(
        name = "keyboard_key",
        description = "Press key combo (e.g. 'Return', 'ctrl+c', 'alt+F4', 'shift+Tab')."
    )]
    async fn keyboard_key(
        &self,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        for m in &mods {
            sess.eis.key(*m, true).map_err(eis_err)?;
        }
        match main {
            Some(k) => {
                sess.eis.key(k, true).map_err(eis_err)?;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                sess.eis.key(k, false).map_err(eis_err)?;
            }
            None => {
                return Err(McpError::invalid_params(
                    format!("unknown key in combo '{}'", params.key),
                    None,
                ));
            }
        }
        for m in mods.iter().rev() {
            sess.eis.key(*m, false).map_err(eis_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "key: {}",
            params.key
        ))]))
    }

    #[rmcp::tool(
        name = "launch_app",
        description = "Launch an application inside the container by command (e.g. 'kate', 'konsole')."
    )]
    async fn launch_app(
        &self,
        Parameters(params): Parameters<LaunchAppParams>,
    ) -> Result<CallToolResult, McpError> {
        use std::io::Write;
        let mut guard = self.session.lock().await;
        let sess = guard.as_mut().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        writeln!(sess.container_stdin, "{}", params.command).map_err(eis_err)?;
        sess.container_stdin.flush().map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "launched: {}",
            params.command
        ))]))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
    }
    let kwin = KwinMcp::new();
    let router =
        rmcp::handler::server::router::Router::new(kwin).with_tools(KwinMcp::tool_router());
    let transport = rmcp::transport::io::stdio();
    let service = router.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
