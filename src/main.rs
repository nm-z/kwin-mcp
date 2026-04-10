mod input_bridge;

use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use serde::Deserialize;
use serde_aux::field_attributes::deserialize_number_from_string;
use std::sync::Arc;

type McpError = rmcp::ErrorData;

#[derive(Debug, thiserror::Error)]
enum KwinError {
    #[error(transparent)] Zbus(#[from] zbus::Error),
    #[error(transparent)] Zvariant(#[from] zbus::zvariant::Error),
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error(transparent)] Nix(#[from] nix::Error),
    #[error(transparent)] Anyhow(#[from] anyhow::Error),
    #[error(transparent)] TryFromInt(#[from] std::num::TryFromIntError),
    #[error(transparent)] SerdeJson(#[from] serde_json::Error),
    #[error(transparent)] SystemTime(#[from] std::time::SystemTimeError),
    #[error(transparent)] Atspi(#[from] atspi::AtspiError),
    #[error(transparent)] Png(#[from] png::EncodingError),
    #[error("{0}")] Msg(String),
}

impl From<KwinError> for McpError {
    fn from(e: KwinError) -> Self {
        McpError::internal_error(e.to_string(), None)
    }
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
    // Punctuation keys not in keyboard-codes crate — use evdev codes directly
    let code: u32 = match raw {
        '`' => 41,   // KEY_GRAVE
        '-' => 12,   // KEY_MINUS
        '=' => 13,   // KEY_EQUAL
        '[' => 26,   // KEY_LEFTBRACE
        ']' => 27,   // KEY_RIGHTBRACE
        '\\' => 43,  // KEY_BACKSLASH
        ';' => 39,   // KEY_SEMICOLON
        '\'' => 40,  // KEY_APOSTROPHE
        ',' => 51,   // KEY_COMMA
        '.' => 52,   // KEY_DOT
        '/' => 53,   // KEY_SLASH
        ' ' => 57,   // KEY_SPACE
        '\t' => 15,  // KEY_TAB
        '\n' => 28,  // KEY_ENTER
        _ => {
            let key_str = String::from(raw);
            let input: keyboard_codes::KeyboardInput = key_str
                .parse()
                .map_err(|e| McpError::invalid_params(format!("keycode parse '{ch}': {e}"), None))?;
            u32::try_from(input.to_code(Platform::Linux))
                .map_err(|e| McpError::invalid_params(format!("keycode overflow '{ch}': {e}"), None))?
        }
    };
    Ok((code, shifted))
}

fn parse_combo(key: &str) -> Result<(Vec<u32>, Option<u32>), McpError> {
    // Standalone key names that keyboard-codes can't parse (it requires modifier+key)
    let standalone = match key.to_lowercase().as_str() {
        "return" | "enter" => Some(28_u32),    // KEY_ENTER
        "backspace" => Some(14),               // KEY_BACKSPACE
        "tab" => Some(15),                     // KEY_TAB
        "escape" | "esc" => Some(1),           // KEY_ESC
        "space" => Some(57),                   // KEY_SPACE
        "delete" | "del" => Some(111),         // KEY_DELETE
        "insert" => Some(110),                 // KEY_INSERT
        "home" => Some(102),                   // KEY_HOME
        "end" => Some(107),                    // KEY_END
        "pageup" | "page_up" => Some(104),     // KEY_PAGEUP
        "pagedown" | "page_down" => Some(109), // KEY_PAGEDOWN
        "up" => Some(103),                     // KEY_UP
        "down" => Some(108),                   // KEY_DOWN
        "left" => Some(105),                   // KEY_LEFT
        "right" => Some(106),                  // KEY_RIGHT
        "f1" => Some(59), "f2" => Some(60), "f3" => Some(61), "f4" => Some(62),
        "f5" => Some(63), "f6" => Some(64), "f7" => Some(65), "f8" => Some(66),
        "f9" => Some(67), "f10" => Some(68), "f11" => Some(87), "f12" => Some(88),
        _ => None,
    };
    if let Some(code) = standalone {
        return Ok((Vec::new(), Some(code)));
    }
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
            if dev.is_some() && kb.is_some() { break; }
            if std::time::Instant::now() > deadline { anyhow::bail!("EIS negotiation timed out"); }
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
                                if let (Some(k), None) = (d.interface::<reis::ei::Keyboard>(), &kb) {
                                        kb = Some(k);
                                        kbd_d = Some(d.device().clone());
                                    }
                            }
                            false => {
                                if d.has_capability(reis::event::DeviceCapability::Keyboard) && kb.is_none() {
                                    d.device().start_emulating(serial, 0);
                                    kb = d.interface::<reis::ei::Keyboard>();
                                    kbd_d = Some(d.device().clone());
                                }
                            }
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

async fn wait_for_socket(
    path: &std::path::Path,
    description: &str,
    deadline: std::time::Instant,
) -> Result<(), String> {
    loop {
        if path.exists() { return Ok(()); }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{description} did not appear at {} within {}s",
                path.display(),
                STARTUP_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

async fn connect_session_bus(
    address: &str,
    deadline: std::time::Instant,
) -> Result<zbus::Connection, String> {
    loop {
        let attempt_error = match zbus::connection::Builder::address(address) {
            Ok(builder) => match builder.auth_mechanism(zbus::AuthMechanism::Anonymous).build().await {
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

// ── uinput virtual devices ──────────────────────────────────────────────

fn create_uinput_devices() -> Result<(evdev::uinput::VirtualDevice, std::path::PathBuf, evdev::uinput::VirtualDevice, std::path::PathBuf), KwinError> {
    // Mouse: buttons + relative axes
    let mut mouse_keys = evdev::AttributeSet::<evdev::KeyCode>::new();
    mouse_keys.insert(evdev::KeyCode::BTN_LEFT);
    mouse_keys.insert(evdev::KeyCode::BTN_RIGHT);
    mouse_keys.insert(evdev::KeyCode::BTN_MIDDLE);

    let mut mouse_axes = evdev::AttributeSet::<evdev::RelativeAxisCode>::new();
    mouse_axes.insert(evdev::RelativeAxisCode::REL_X);
    mouse_axes.insert(evdev::RelativeAxisCode::REL_Y);
    mouse_axes.insert(evdev::RelativeAxisCode::REL_WHEEL);
    mouse_axes.insert(evdev::RelativeAxisCode::REL_HWHEEL);

    let mut mouse_dev = evdev::uinput::VirtualDevice::builder()?
        .name("kwin-mcp-virtual-mouse")
        .with_keys(&mouse_keys)?
        .with_relative_axes(&mouse_axes)?
        .build()?;

    let mouse_path = mouse_dev
        .enumerate_dev_nodes_blocking()?
        .next()
        .ok_or_else(|| KwinError::Msg("uinput mouse: no devnode".to_owned()))??;

    // Keyboard: all standard keys (KEY_ESC=1 through KEY_MAX=0x2ff)
    let mut kbd_keys = evdev::AttributeSet::<evdev::KeyCode>::new();
    let mut code: u16 = 1;
    loop {
        if code > 0x2ff { break; }
        kbd_keys.insert(evdev::KeyCode::new(code));
        code = match code.checked_add(1) {
            Some(v) => v,
            None => break,
        };
    }

    let mut kbd_dev = evdev::uinput::VirtualDevice::builder()?
        .name("kwin-mcp-virtual-keyboard")
        .with_keys(&kbd_keys)?
        .build()?;

    let kbd_path = kbd_dev
        .enumerate_dev_nodes_blocking()?
        .next()
        .ok_or_else(|| KwinError::Msg("uinput keyboard: no devnode".to_owned()))??;

    Ok((mouse_dev, mouse_path, kbd_dev, kbd_path))
}

// ── Session ──────────────────────────────────────────────────────────────

struct Session {
    kwin_conn: zbus::Connection,       // talks to KWin via its unique name
    _proxy_conn: zbus::Connection,    // owns org.kde.KWin, has InputDevice objects (kept alive)
    kwin_unique_name: String,
    eis: Eis,
    bwrap_child: std::process::Child,
    bwrap_stdin: std::process::ChildStdin,
    host_xdg_dir: std::path::PathBuf,
    _uinput_mouse: evdev::uinput::VirtualDevice,
    _uinput_keyboard: evdev::uinput::VirtualDevice,
    cdp_browser: Option<Arc<chromiumoxide::Browser>>,
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
    async fn kwin_conn(&self) -> Result<zbus::Connection, McpError> {
        let guard = self.session.lock().await;
        match &*guard {
            Some(s) => Ok(s.kwin_conn.clone()),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    async fn kwin_unique_name(&self) -> Result<String, McpError> {
        let guard = self.session.lock().await;
        match &*guard {
            Some(s) => Ok(s.kwin_unique_name.clone()),
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


async fn structured_result(peer: &rmcp::Peer<rmcp::RoleServer>, text: impl Into<String>, structured: serde_json::Value) -> CallToolResult {
    let s: String = text.into();
    let _ = peer.notify_logging_message(rmcp::model::LoggingMessageNotificationParam::new(
        rmcp::model::LoggingLevel::Info,
        serde_json::json!(s),
    )).await;
    let mut r = CallToolResult::success(vec![Content::text(s)]);
    r.structured_content = Some(structured);
    r
}

fn teardown(mut sess: Session) {
    drop(sess.cdp_browser);
    drop(sess.bwrap_stdin);
    // Kill the bwrap process group (negative PID = entire group)
    let pid = sess.bwrap_child.id();
    if let Ok(neg) = i32::try_from(pid).map(|p| -p) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(neg), nix::sys::signal::Signal::SIGTERM);
    }
    let _ = sess.bwrap_child.wait();
    if let Err(e) = std::fs::remove_dir_all(&sess.host_xdg_dir) { eprintln!("teardown cleanup: {e}"); }
}

async fn active_window_info(conn: &zbus::Connection, kwin_unique: &str, host_xdg_dir: &std::path::Path) -> Result<(i32, i32, WindowGeometry), KwinError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let marker = format!("kwin-mcp-{ts}");
    let cb_path = format!("/KWinMCP/{ts}");
    let our_name = conn
        .unique_name()
        .ok_or(KwinError::Msg("no bus name".to_owned()))?
        .to_string();
    let script = format!(
        "var w = workspace.activeWindow;\
        callDBus('{our_name}','{cb_path}','org.kde.KWinMCP','result',\
        w ? JSON.stringify({{x:w.frameGeometry.x,y:w.frameGeometry.y,\
        w:w.frameGeometry.width,h:w.frameGeometry.height,\
        title:w.caption,id:w.internalId.toString(),\
        resourceClass:w.resourceClass,resourceName:w.resourceName,\
        pid:w.pid}}) : 'null');"
    );
    let script_name = format!("{marker}.js");
    let script_file = host_xdg_dir.join(&script_name);
    std::fs::write(&script_file, &script)?;
    // host_xdg_dir is bind-mounted at the same path inside bwrap
    let container_script_path = script_file.to_string_lossy().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let cb = KWinCallback {
        tx: std::sync::Mutex::new(Some(tx)),
    };
    let obj_path = zbus::zvariant::ObjectPath::try_from(cb_path.as_str())?;
    let registered = conn.object_server().at(&obj_path, cb).await?;
    eprintln!("active_window_info: our_name={our_name} path={cb_path} registered={registered}");
    if !registered {
        return Err(KwinError::Msg(format!("failed to register callback at {cb_path}")));
    }
    // Load and run the script — target KWin's unique name, not org.kde.KWin (we own that)
    let scripting: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination(kwin_unique)?
        .path("/Scripting")?
        .interface("org.kde.kwin.Scripting")?
        .build()
        .await?;
    let (script_id,): (i32,) = scripting
        .call("loadScript", &(&container_script_path, &marker))
        .await?;
    if script_id < 0 {
        conn.object_server().remove::<KWinCallback, _>(&obj_path).await?;
        std::fs::remove_file(&script_file)?;
        return Err(KwinError::Msg(format!("KWin loadScript failed, id={script_id}")));
    }
    let script_proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination(kwin_unique)?
        .path(format!("/Scripting/Script{script_id}"))?
        .interface("org.kde.kwin.Script")?
        .build()
        .await?;
    script_proxy.call::<_, (), ()>("run", &()).await?;
    // Wait for callback, then cleanup regardless of result
    let json_result = rx
        .await
        .map_err(|_| KwinError::Msg("KWin callback channel closed".to_owned()));
    conn.object_server().remove::<KWinCallback, _>(&obj_path).await?;
    let (_,): (bool,) = scripting.call("unloadScript", &(&marker,)).await?;
    std::fs::remove_file(&script_file)?;
    let json = json_result?;
    if json == "null" {
        return Err(KwinError::Msg("KWin script error: No active window".to_owned()));
    }
    let info: WindowGeometry = serde_json::from_str(&json)?;
    #[expect(clippy::as_conversions)]
    let (x, y) = (info.x.round() as i32, info.y.round() as i32);
    Ok((x, y, info))
}

struct KWinCallback {
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
}

#[zbus::interface(name = "org.kde.KWinMCP")]
impl KWinCallback {
    #[zbus(name = "result")]
    fn result(&self, payload: String) {
        match self.tx.lock() {
            Ok(mut g) => {
                if let Some(tx) = g.take()
                    && let Err(e) = tx.send(payload) {
                    eprintln!("callback send failed: {e}");
                }
            }
            Err(e) => eprintln!("callback lock poisoned: {e}"),
        }
    }
}

#[derive(Deserialize)]
struct WindowGeometry {
    x: f64,
    y: f64,
    #[serde(default)]
    id: String,
    #[serde(default, rename = "resourceClass")]
    resource_class: String,
    #[serde(default, rename = "resourceName")]
    resource_name: String,
    #[serde(default)]
    pid: i32,
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
) -> Result<AtspiNode, KwinError> {
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
    let bounds = match acc.proxies().await?.component().await {
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
struct SessionStartParams {
    /// When true, agent writes persist to the host filesystem.
    /// When false (default), all writes are ephemeral.
    #[serde(default)]
    #[allow(dead_code)]
    writable: bool,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ScreenshotParams {
    /// Crop region [x1, y1, x2, y2] for pixel-level detail on a specific area.
    /// Coordinates are window-relative pixels. Omit for full screenshot.
    #[serde(default)]
    region: Option<[i32; 4]>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseClickParams {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    y: i32,
    button: Option<String>,
    double: Option<bool>,
    triple: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseMoveParams {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    y: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseScrollParams {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    y: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    delta: i32,
    horizontal: Option<bool>,
    discrete: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseDragParams {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    from_x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    from_y: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    to_x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    to_y: i32,
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_logging().build())
            .with_server_info(Implementation::new("kwin-mcp", "0.1.0"))
            .with_instructions("KDE Wayland desktop automation. Call session_start first. Coordinates are pixels on a 1280x800 screen.")
    }
}

#[rmcp::tool_router]
impl KwinMcp {
    #[rmcp::tool(
        name = "session_start",
        description = "Start an isolated KDE Wayland session. Set writable=true to persist writes to host filesystem (default: false, all writes ephemeral). Must be called before any other tool."
    )]
    async fn session_start(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<SessionStartParams>,
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
            if let Some(old) = (*guard).take() { teardown(old); }
        }
        eprintln!("session_start: previous session cleared");
        let pid = std::process::id();
        let host_xdg_dir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
        let _ = std::fs::remove_dir_all(&host_xdg_dir);
        std::fs::create_dir_all(&host_xdg_dir).map_err(|e| ver_err(e.to_string()))?;
        eprintln!(
            "session_start: host_xdg_dir ready path={}",
            host_xdg_dir.display()
        );
        let xdg_dir_str = host_xdg_dir.display().to_string();
        // Write AT-SPI dbus config with ANONYMOUS auth for cross-namespace access
        let atspi_conf_path = host_xdg_dir.join("accessibility.conf");
        std::fs::write(&atspi_conf_path, format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
            \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
            <busconfig><type>accessibility</type>\
            <servicedir>/usr/share/dbus-1/accessibility-services</servicedir>\
            <auth>EXTERNAL</auth><auth>ANONYMOUS</auth><allow_anonymous/>\
            <listen>unix:dir={xdg_dir_str}</listen>\
            <policy context=\"default\"><allow user=\"root\"/>\
            <allow send_destination=\"*\"/><allow receive_type=\"method_call\"/>\
            <allow receive_type=\"method_return\"/><allow receive_type=\"error\"/>\
            <allow receive_type=\"signal\"/><allow own=\"*\"/></policy>\
            </busconfig>"
        )).map_err(|e| ver_err(format!("write atspi config: {e}")))?;
        // Write kwin-mcp display config files to host_xdg_dir for --ro-bind mounting.
        // Protected from agent writes in both ephemeral and writable modes.
        let kwinrc_path = host_xdg_dir.join("kwinrc");
        std::fs::write(&kwinrc_path,
            "[org.kde.kdecoration2]\nBorderSize=None\nShadowSize=0\n\n\
             [Compositing]\nLockScreenAutoLockEnabled=false\n"
        ).map_err(|e| ver_err(format!("write kwinrc: {e}")))?;
        let kwinrulesrc_path = host_xdg_dir.join("kwinrulesrc");
        std::fs::write(&kwinrulesrc_path,
            "[1]\nDescription=No decorations, maximized\nnoborder=true\nnoborderrule=2\n\
             maximizehoriz=true\nmaximizehorizrule=2\nmaximizevert=true\nmaximizevertrule=2\n\
             wmclassmatch=0\n\n[General]\ncount=1\nrules=1\n"
        ).map_err(|e| ver_err(format!("write kwinrulesrc: {e}")))?;
        let kscreenlockerrc_path = host_xdg_dir.join("kscreenlockerrc");
        std::fs::write(&kscreenlockerrc_path,
            "[Daemon]\nAutolock=false\nLockOnResume=false\nTimeout=0\n"
        ).map_err(|e| ver_err(format!("write kscreenlockerrc: {e}")))?;
        let kcmfonts_path = host_xdg_dir.join("kcmfonts");
        std::fs::write(&kcmfonts_path,
            "[General]\nforceFontDPI=96\n"
        ).map_err(|e| ver_err(format!("write kcmfonts: {e}")))?;
        let fonts_conf_path = host_xdg_dir.join("fonts.conf");
        std::fs::write(&fonts_conf_path,
            "<?xml version=\"1.0\"?>\n\
             <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
             <fontconfig>\n\
             <match target=\"font\">\n\
             <edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\n\
             <edit name=\"hintstyle\" mode=\"assign\"><const>hintnone</const></edit>\n\
             <edit name=\"antialias\" mode=\"assign\"><bool>true</bool></edit>\n\
             <edit name=\"rgba\" mode=\"assign\"><const>none</const></edit>\n\
             </match>\n\
             </fontconfig>\n"
        ).map_err(|e| ver_err(format!("write fonts.conf: {e}")))?;
        // Read host kdeglobals and patch display settings for the virtual session
        let home = std::env::var("HOME").map_err(|e| ver_err(e.to_string()))?;
        let real_kdeglobals = std::path::Path::new(&home).join(".config/kdeglobals");
        let mut kdeglobals_content = std::fs::read_to_string(&real_kdeglobals).unwrap_or_default();
        let replacements = [
            ("ScaleFactor=", "ScaleFactor=1"),
            ("ScreenScaleFactors=", "ScreenScaleFactors="),
            ("XftHintStyle=", "XftHintStyle=hintnone"),
            ("XftSubPixel=", "XftSubPixel=none"),
            ("font=", "font=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
            ("menuFont=", "menuFont=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
            ("smallestReadableFont=", "smallestReadableFont=Noto Sans,12,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
            ("toolBarFont=", "toolBarFont=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
            ("activeFont=", "activeFont=Noto Sans,14,-1,5,700,0,0,0,0,0,0,0,0,0,0,1,Bold"),
            ("fixed=", "fixed=Hack,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
        ];
        for (prefix, replacement) in replacements {
            kdeglobals_content = kdeglobals_content
                .lines()
                .map(|line| {
                    if line.starts_with(prefix) { replacement.to_owned() }
                    else { line.to_owned() }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        let kdeglobals_path = host_xdg_dir.join("kdeglobals");
        std::fs::write(&kdeglobals_path, &kdeglobals_content)
            .map_err(|e| ver_err(format!("write kdeglobals: {e}")))?;
        // Write fontconfig system overrides to bind-mount over files that force hinting/subpixel
        let fc_hinting_path = host_xdg_dir.join("10-hinting-none.conf");
        std::fs::write(&fc_hinting_path, "\
            <?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
            <fontconfig>\n\
            <match target=\"font\"><edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\
            <edit name=\"hintstyle\" mode=\"assign\"><const>hintnone</const></edit></match>\n\
            <match target=\"pattern\"><edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\
            <edit name=\"hintstyle\" mode=\"assign\"><const>hintnone</const></edit></match>\n\
            </fontconfig>\n"
        ).map_err(|e| ver_err(format!("write fontconfig hinting: {e}")))?;
        let fc_lcd_path = host_xdg_dir.join("11-lcdfilter-none.conf");
        std::fs::write(&fc_lcd_path, "\
            <?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
            <fontconfig>\n\
            <match target=\"font\"><edit name=\"lcdfilter\" mode=\"assign\"><const>lcdnone</const></edit>\
            <edit name=\"rgba\" mode=\"assign\"><const>none</const></edit></match>\n\
            <match target=\"pattern\"><edit name=\"lcdfilter\" mode=\"assign\"><const>lcdnone</const></edit>\
            <edit name=\"rgba\" mode=\"assign\"><const>none</const></edit></match>\n\
            </fontconfig>\n"
        ).map_err(|e| ver_err(format!("write fontconfig lcd: {e}")))?;
        let fc_hinting_str = fc_hinting_path.display().to_string();
        let fc_lcd_str = fc_lcd_path.display().to_string();
        // Inline entrypoint: starts dbus/kwin/services, reads stdin for launch_app
        let entrypoint = format!(
            "set -u\n\
            export XDG_RUNTIME_DIR={xdg_dir_str}\n\
            export WAYLAND_DISPLAY=wayland-0\n\
            export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1\n\
            export QT_SCALE_FACTOR=1\n\
            export GDK_SCALE=1\n\
            export FREETYPE_PROPERTIES=truetype:interpreter-version=35\n\
            export FONTCONFIG_CACHE=/tmp/fontconfig-cache\n\
            export ATSPI_DBUS_IMPLEMENTATION=dbus-daemon\n\
            mkdir -p /tmp/fontconfig-cache && fc-cache -f 2>/dev/null\n\
            printf '<busconfig><include>/usr/share/dbus-1/session.conf</include><auth>ANONYMOUS</auth><allow_anonymous/></busconfig>' > /tmp/mcp-dbus.conf\n\
            dbus-daemon --config-file=/tmp/mcp-dbus.conf --address='unix:path={xdg_dir_str}/bus' --nofork &\n\
            dbus_pid=$!\n\
            n=0; while [ ! -S '{xdg_dir_str}/bus' ] && kill -0 \"$dbus_pid\" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
            export DBUS_SESSION_BUS_ADDRESS='unix:path={xdg_dir_str}/bus'\n\
            touch '{xdg_dir_str}/dbus-ready'\n\
            n=0; while [ ! -f '{xdg_dir_str}/bridge-ready' ] && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
            KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 \
            kwin_wayland --virtual --xwayland --no-lockscreen --width 1280 --height 800 &\n\
            sleep 0.3\n\
            dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR QT_QPA_PLATFORM PATH HOME USER ATSPI_DBUS_IMPLEMENTATION\n\
            at-spi-bus-launcher --launch-immediately &\n\
            pipewire &\n\
            wireplumber &\n\
            while read -r cmd; do\n\
                $cmd &\n\
            done\n"
        );
        // Create uinput virtual devices before bwrap so we can bind-mount them
        let (uinput_mouse, mouse_evdev, uinput_keyboard, kbd_evdev) =
            create_uinput_devices().map_err(|e| ver_err(format!("uinput: {e}")))?;
        let mouse_evdev_str = mouse_evdev.display().to_string();
        let kbd_evdev_str = kbd_evdev.display().to_string();
        eprintln!("session_start: uinput mouse={mouse_evdev_str} keyboard={kbd_evdev_str}");

        let writable = params.writable;
        let mut cmd = std::process::Command::new("bwrap");
        cmd.args(["--die-with-parent", "--unshare-pid", "--unshare-uts", "--unshare-ipc"]);
        if writable {
            cmd.args(["--bind", "/", "/"]);
        } else {
            cmd.args(["--ro-bind", "/", "/", "--overlay-src", &home, "--tmp-overlay", &home]);
        }
        let kwinrc_str = kwinrc_path.display().to_string();
        let kdeglobals_str = kdeglobals_path.display().to_string();
        let kwinrulesrc_str = kwinrulesrc_path.display().to_string();
        let kscreenlockerrc_str = kscreenlockerrc_path.display().to_string();
        let kcmfonts_str = kcmfonts_path.display().to_string();
        let fonts_conf_str = fonts_conf_path.display().to_string();
        let home_kwinrc = format!("{home}/.config/kwinrc");
        let home_kdeglobals = format!("{home}/.config/kdeglobals");
        let home_kwinrulesrc = format!("{home}/.config/kwinrulesrc");
        let home_kscreenlockerrc = format!("{home}/.config/kscreenlockerrc");
        let home_kcmfonts = format!("{home}/.config/kcmfonts");
        let home_fonts_conf = format!("{home}/.config/fontconfig/fonts.conf");
        cmd.args([
            "--dev", "/dev",
            "--dev-bind", "/dev/dri", "/dev/dri",
            "--dev-bind", "/dev/uinput", "/dev/uinput",
            "--dev-bind", &mouse_evdev_str, &mouse_evdev_str,
            "--dev-bind", &kbd_evdev_str, &kbd_evdev_str,
            "--proc", "/proc",
            "--tmpfs", "/tmp",
            "--tmpfs", "/run",
            "--bind", &xdg_dir_str, &xdg_dir_str,
            // System config overrides (read-only)
            "--ro-bind", &atspi_conf_path.display().to_string(), "/usr/share/defaults/at-spi2/accessibility.conf",
            "--ro-bind", &fc_hinting_str, "/usr/share/fontconfig/conf.default/10-hinting-slight.conf",
            "--ro-bind", &fc_lcd_str, "/usr/share/fontconfig/conf.default/11-lcdfilter-default.conf",
            // $HOME config overrides (read-only — protects display settings from agent writes)
            "--ro-bind", &kwinrc_str, &home_kwinrc,
            "--ro-bind", &kdeglobals_str, &home_kdeglobals,
            "--ro-bind", &kwinrulesrc_str, &home_kwinrulesrc,
            "--ro-bind", &kscreenlockerrc_str, &home_kscreenlockerrc,
            "--ro-bind", &kcmfonts_str, &home_kcmfonts,
            "--ro-bind", &fonts_conf_str, &home_fonts_conf,
            "--", "bash", "-c", &entrypoint,
        ]);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::inherit());
        eprintln!("session_start: spawning bwrap");
        let mut bwrap_child = cmd.spawn().map_err(|e| ver_err(e.to_string()))?;
        eprintln!("session_start: bwrap spawned pid={:?}", bwrap_child.id());
        let bwrap_stdin = match bwrap_child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = bwrap_child.kill();
                let _ = bwrap_child.wait();
                let _ = std::fs::remove_dir_all(&host_xdg_dir);
                return Err(ver_err("bwrap stdin not available".to_owned()));
            }
        };
        let cleanup_err = |message: String,
                           mut bwrap_child: std::process::Child,
                           bwrap_stdin: std::process::ChildStdin| {
            eprintln!("session_start: startup error: {message}");
            drop(bwrap_stdin);
            let pid = bwrap_child.id();
            if let Ok(neg) = i32::try_from(pid).map(|p| -p) {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(neg), nix::sys::signal::Signal::SIGTERM);
            }
            let _ = bwrap_child.wait();
            let _ = std::fs::remove_dir_all(&host_xdg_dir);
            Err(ver_err(message))
        };
        // Wait for dbus-ready marker (entrypoint touches it after dbus-daemon starts)
        let dbus_ready_path = host_xdg_dir.join("dbus-ready");
        eprintln!("session_start: wait for dbus-ready at {}", dbus_ready_path.display());
        if let Err(e) = wait_for_socket(
            &dbus_ready_path,
            "dbus-ready marker",
            std::time::Instant::now() + STARTUP_TIMEOUT,
        ).await {
            return cleanup_err(e, bwrap_child, bwrap_stdin);
        }
        eprintln!("session_start: dbus-ready");
        let bus_addr = format!("unix:path={xdg_dir_str}/bus");

        // Create proxy_conn: claims org.kde.KWin, registers InputDevice objects
        // This must happen BEFORE KWin starts so we own the well-known name
        eprintln!("session_start: creating proxy_conn");
        let proxy_conn =
            match connect_session_bus(&bus_addr, std::time::Instant::now() + STARTUP_TIMEOUT).await
            {
                Ok(conn) => conn,
                Err(e) => return cleanup_err(e, bwrap_child, bwrap_stdin),
            };
        // Claim org.kde.KWin on proxy_conn (before KWin starts, so we get it first)
        if let Err(e) = proxy_conn.request_name("org.kde.KWin").await {
            return cleanup_err(format!("claim org.kde.KWin: {e}"), bwrap_child, bwrap_stdin);
        }
        eprintln!("session_start: proxy_conn owns org.kde.KWin");

        // Register InputDevice objects on proxy_conn
        let mouse_sysname = mouse_evdev
            .file_name()
            .ok_or_else(|| ver_err("no mouse sysname".to_owned()))?
            .to_string_lossy()
            .to_string();
        let kbd_sysname = kbd_evdev
            .file_name()
            .ok_or_else(|| ver_err("no keyboard sysname".to_owned()))?
            .to_string_lossy()
            .to_string();
        let mouse_dev = input_bridge::InputDevice::new_pointer(mouse_sysname);
        let kbd_dev = input_bridge::InputDevice::new_keyboard(kbd_sysname);
        if let Err(e) = input_bridge::register_devices(&proxy_conn, vec![mouse_dev, kbd_dev]).await {
            return cleanup_err(format!("register input devices: {e}"), bwrap_child, bwrap_stdin);
        }
        eprintln!("session_start: input devices registered on proxy_conn");

        // Signal bridge-ready so the entrypoint starts KWin
        let bridge_ready_path = host_xdg_dir.join("bridge-ready");
        std::fs::write(&bridge_ready_path, "").map_err(|e| ver_err(format!("write bridge-ready: {e}")))?;
        eprintln!("session_start: bridge-ready signaled, KWin starting");

        // Create kwin_conn: separate connection for talking to KWin
        let kwin_conn =
            match connect_session_bus(&bus_addr, std::time::Instant::now() + STARTUP_TIMEOUT).await
            {
                Ok(conn) => conn,
                Err(e) => return cleanup_err(e, bwrap_child, bwrap_stdin),
            };

        // Wait for KWin's wayland-0 socket to appear (proves KWin is running)
        let wayland_socket = host_xdg_dir.join("wayland-0");
        eprintln!("session_start: wait for wayland-0");
        if let Err(e) = wait_for_socket(
            &wayland_socket,
            "wayland-0 socket",
            std::time::Instant::now() + STARTUP_TIMEOUT,
        ).await {
            return cleanup_err(e, bwrap_child, bwrap_stdin);
        }
        eprintln!("session_start: wayland-0 ready");

        // Discover KWin's unique bus name — try each unique name for EIS interface
        eprintln!("session_start: discovering KWin unique name");
        let dbus_proxy = zbus::fdo::DBusProxy::new(&kwin_conn)
            .await
            .map_err(|e| ver_err(format!("DBus proxy: {e}")))?;
        let kwin_unique_name;
        let kwin_deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
        // Skip our own connections
        let proxy_unique = proxy_conn.unique_name()
            .map(|n| n.to_string()).unwrap_or_default();
        let kwin_conn_unique = kwin_conn.unique_name()
            .map(|n| n.to_string()).unwrap_or_default();
        loop {
            let names = dbus_proxy.list_names().await
                .map_err(|e| ver_err(format!("ListNames: {e}")))?;
            let mut found = None;
            for name in &names {
                let name_str = name.as_str();
                if !name_str.starts_with(':') { continue; }
                if name_str == proxy_unique || name_str == kwin_conn_unique { continue; }
                // Quick probe with timeout — Introspect the EIS path
                let probe_result = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    async {
                        let p: zbus::Proxy = zbus::proxy::Builder::new(&kwin_conn)
                            .destination(name_str)?
                            .path("/org/kde/KWin/EIS/RemoteDesktop")?
                            .interface("org.freedesktop.DBus.Introspectable")?
                            .build()
                            .await?;
                        let r: (String,) = p.call("Introspect", &()).await?;
                        Ok::<String, zbus::Error>(r.0)
                    }
                ).await;
                if let Ok(Ok(xml)) = probe_result {
                    if xml.contains("connectToEIS") {
                        found = Some(name_str.to_owned());
                        break;
                    }
                }
            }
            if let Some(name) = found {
                kwin_unique_name = name;
                break;
            }
            if std::time::Instant::now() >= kwin_deadline {
                return cleanup_err("could not discover KWin unique name".to_owned(), bwrap_child, bwrap_stdin);
            }
            tokio::time::sleep(STARTUP_POLL).await;
        }
        eprintln!("session_start: KWin unique name = {kwin_unique_name}");

        // Connect to KWin EIS using its unique name
        eprintln!("session_start: connect to KWin EIS");
        let eis_builder = KWinEisProxy::builder(&kwin_conn)
            .destination(kwin_unique_name.as_str())
            .map_err(|e| ver_err(format!("EIS proxy builder: {e}")))?;
        let eis_proxy = match eis_builder.build().await {
            Ok(p) => p,
            Err(e) => return cleanup_err(format!("KWin EIS proxy: {e}"), bwrap_child, bwrap_stdin),
        };
        // capabilities: 1=keyboard, 2=pointer, 4=touch -> 3 = keyboard+pointer
        let (eis_fd, _cookie) = match eis_proxy.connect_to_eis(3).await {
            Ok(r) => r,
            Err(e) => return cleanup_err(format!("connectToEIS: {e}"), bwrap_child, bwrap_stdin),
        };
        eprintln!("session_start: EIS fd received, negotiating");
        let eis_owned_fd = std::os::fd::OwnedFd::from(eis_fd);
        let eis = match tokio::task::spawn_blocking(move || Eis::from_fd(eis_owned_fd)).await {
            Ok(Ok(eis)) => eis,
            Ok(Err(e)) => return cleanup_err(format!("EIS negotiation: {e}"), bwrap_child, bwrap_stdin),
            Err(e) => return cleanup_err(format!("EIS task: {e}"), bwrap_child, bwrap_stdin),
        };
        eprintln!("session_start: EIS ready");

        // Forward host wallet/secret services into the container's D-Bus
        let wallet_conn = match connect_session_bus(&bus_addr, std::time::Instant::now() + STARTUP_TIMEOUT).await {
            Ok(conn) => conn,
            Err(e) => return cleanup_err(format!("wallet_conn: {e}"), bwrap_child, bwrap_stdin),
        };
        let host_conn = match zbus::Connection::session().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("session_start: host bus unavailable, skipping wallet forwarding: {e}");
                // Non-fatal — wallet forwarding is best-effort
                let bus_name = kwin_conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
                let workdir = host_xdg_dir.display().to_string();
                let msg = format!("{version_stamp} — session started bus={bus_name} kwin={kwin_unique_name} writable={writable}");
                let mut guard = self.session.lock().await;
                *guard = Some(Session {
                    kwin_conn, _proxy_conn: proxy_conn, kwin_unique_name: kwin_unique_name.clone(),
                    eis, bwrap_child, bwrap_stdin, host_xdg_dir,
                    _uinput_mouse: uinput_mouse, _uinput_keyboard: uinput_keyboard,
                    cdp_browser: None,
                });
                return Ok(structured_result(&peer, msg, serde_json::json!({
                    "status": "started", "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
                    "commit": env!("GIT_HASH"), "bus": bus_name, "kwin_unique": kwin_unique_name,
                    "workdir": workdir, "writable": writable,
                })).await);
            }
        };
        // Discover wallet/secret services on host bus
        let host_dbus = zbus::fdo::DBusProxy::new(&host_conn).await
            .map_err(|e| ver_err(format!("host DBus proxy: {e}")))?;
        let host_names = host_dbus.list_names().await
            .map_err(|e| ver_err(format!("host ListNames: {e}")))?;
        let wallet_names: Vec<String> = host_names.iter()
            .filter(|n| {
                let s = n.as_str();
                s.contains("wallet") || s.contains("kwalletd") || s == "org.freedesktop.secrets"
            })
            .map(|n| n.to_string())
            .collect();
        for name in &wallet_names {
            match wallet_conn.request_name(name.as_str()).await {
                Ok(_) => eprintln!("session_start: wallet forwarding claimed {name}"),
                Err(e) => eprintln!("session_start: wallet forwarding skip {name}: {e}"),
            }
        }
        if !wallet_names.is_empty() {
            // Spawn forwarding task: container bus → host bus
            tokio::spawn(async move {
                use futures::StreamExt;
                let mut container_stream = zbus::MessageStream::from(&wallet_conn);
                let mut host_stream = zbus::MessageStream::from(&host_conn);
                let serial_counter = std::sync::atomic::AtomicU32::new(1);
                while let Some(Ok(msg)) = container_stream.next().await {
                    if msg.message_type() != zbus::message::Type::MethodCall { continue; }
                    let header = msg.header();
                    let path = match header.path() {
                        Some(p) => p.to_owned(),
                        None => continue,
                    };
                    let iface = header.interface().map(|i| i.to_owned());
                    let member = match header.member() {
                        Some(m) => m.to_owned(),
                        None => continue,
                    };
                    let dest = match header.destination() {
                        Some(d) => d.to_string(),
                        None => continue,
                    };
                    let body = msg.body();
                    let body_data = body.data();
                    let sig = body.signature().to_owned();
                    let serial = serial_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let serial_nz = match std::num::NonZeroU32::new(serial) {
                        Some(s) => s,
                        None => continue,
                    };
                    let host_msg = unsafe {
                        zbus::message::Message::method_call(path, member)
                            .and_then(|b| b.destination(dest.as_str()))
                            .and_then(|b| match &iface { Some(i) => b.interface(i.clone()), None => Ok(b) })
                            .map(|b| b.serial(serial_nz))
                            .and_then(|b| b.build_raw_body(body_data.bytes(), sig, vec![]))
                    };
                    let host_msg = match host_msg {
                        Ok(m) => m,
                        Err(e) => { eprintln!("wallet fwd: build error: {e}"); continue; }
                    };
                    if let Err(e) = host_conn.send(&host_msg).await {
                        eprintln!("wallet fwd: send error: {e}");
                        continue;
                    }
                    // Wait for reply matching our serial (5s timeout)
                    let reply_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        async {
                            while let Some(Ok(reply)) = host_stream.next().await {
                                if reply.header().reply_serial() == Some(serial_nz) {
                                    return Some(reply);
                                }
                            }
                            None
                        }
                    ).await;
                    let reply = match reply_result {
                        Ok(Some(r)) => r,
                        _ => { eprintln!("wallet fwd: reply timeout serial={serial}"); continue; }
                    };
                    // Forward reply back to container
                    let fwd_reply = match reply.message_type() {
                        zbus::message::Type::MethodReturn => {
                            let rb = reply.body();
                            let rd = rb.data();
                            unsafe {
                                zbus::message::Message::method_return(&msg.header())
                                    .and_then(|b| b.build_raw_body(rd.bytes(), rb.signature().to_owned(), vec![]))
                            }
                        }
                        zbus::message::Type::Error => {
                            let err_name = match reply.header().error_name() {
                                Some(e) => e.to_owned(),
                                None => match "org.freedesktop.DBus.Error.Failed".try_into() {
                                    Ok(n) => n,
                                    Err(_) => continue,
                                },
                            };
                            let rb = reply.body();
                            let rd = rb.data();
                            unsafe {
                                zbus::message::Message::error(&msg.header(), err_name)
                                    .and_then(|b| b.build_raw_body(rd.bytes(), rb.signature().to_owned(), vec![]))
                            }
                        }
                        zbus::message::Type::MethodCall | zbus::message::Type::Signal => continue,
                    };
                    if let Ok(fwd) = fwd_reply {
                        let _ = wallet_conn.send(&fwd).await;
                    }
                }
            });
        }

        let bus_name = kwin_conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let workdir = host_xdg_dir.display().to_string();
        let msg = format!("{version_stamp} — session started bus={bus_name} kwin={kwin_unique_name} writable={writable}");
        let mut guard = self.session.lock().await;
        *guard = Some(Session {
            kwin_conn,
            _proxy_conn: proxy_conn,
            kwin_unique_name: kwin_unique_name.clone(),
            eis,
            bwrap_child,
            bwrap_stdin,
            host_xdg_dir,
            _uinput_mouse: uinput_mouse,
            _uinput_keyboard: uinput_keyboard,
            cdp_browser: None,
        });
        Ok(structured_result(&peer, msg, serde_json::json!({
            "status": "started",
            "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
            "commit": env!("GIT_HASH"),
            "bus": bus_name,
            "kwin_unique": kwin_unique_name,
            "workdir": workdir,
            "writable": writable,
        })).await)
    }

    #[rmcp::tool(
        name = "session_stop",
        description = "Stop the KWin session and clean up all processes.",
        annotations(destructive_hint = true)
    )]
    async fn session_stop(&self, peer: rmcp::Peer<rmcp::RoleServer>) -> Result<CallToolResult, McpError> {
        let mut guard = self.session.lock().await;
        match (*guard).take() {
            Some(sess) => {
                teardown(sess);
                Ok(structured_result(&peer, "session stopped", serde_json::json!({"status": "stopped"})).await)
            }
            None => Ok(structured_result(&peer, "no session running", serde_json::json!({"status": "none"})).await),
        }
    }

    #[rmcp::tool(
        name = "screenshot",
        description = "Take a screenshot. Pass region [x1, y1, x2, y2] to crop a specific area at full resolution for pixel-level detail.",
        annotations(read_only_hint = true)
    )]
    async fn screenshot(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        let (_, _, win_geo) = active_window_info(&conn, &kwin_unique, &xdg).await?;
        let win_id = win_geo.id;
        let proxy = KWinScreenShot2Proxy::builder(&conn)
            .destination(kwin_unique.as_str())
            .map_err(KwinError::from)?
            .build()
            .await
            .map_err(KwinError::from)?;
        let (read_fd, write_fd) = nix::unistd::pipe().map_err(KwinError::from)?;
        let pipe_fd = zbus::zvariant::OwnedFd::from(write_fd);
        let mut opts = std::collections::HashMap::new();
        opts.insert("include-cursor", zbus::zvariant::Value::from(true));
        opts.insert("include-decoration", zbus::zvariant::Value::from(true));
        opts.insert("hide-caller-windows", zbus::zvariant::Value::from(false));
        let meta = proxy
            .capture_window(&win_id, opts, pipe_fd)
            .await
            .map_err(KwinError::from)?;
        let get_u32 = |k: &str| -> Result<u32, McpError> {
            let val = meta
                .get(k)
                .ok_or_else(|| McpError::internal_error(format!("screenshot: no {k}"), None))?;
            let n: u32 = val.try_into().map_err(KwinError::from)?;
            Ok(n)
        };
        let (width, height, stride) = (get_u32("width")?, get_u32("height")?, get_u32("stride")?);
        let reader_file = std::fs::File::from(read_fd);
        let total = usize::try_from(stride * height).map_err(KwinError::from)?;
        let mut pixels = vec![0u8; total];
        std::io::Read::read_exact(&mut std::io::BufReader::new(reader_file), &mut pixels)
            .map_err(KwinError::from)?;
        // BGRA premultiplied → RGBA
        let px = usize::try_from(width * height).map_err(KwinError::from)?;
        let mut rgba = vec![0u8; px * 4];
        for row in 0..height {
            for col in 0..width {
                let si = usize::try_from(row * stride + col * 4).map_err(KwinError::from)?;
                let di = usize::try_from((row * width + col) * 4).map_err(KwinError::from)?;
                rgba[di] = pixels[si + 2];
                rgba[di + 1] = pixels[si + 1];
                rgba[di + 2] = pixels[si];
                rgba[di + 3] = pixels[si + 3];
            }
        }
        // Crop if region specified
        let (out_rgba, out_w, out_h) = if let Some([x1, y1, x2, y2]) = params.region {
            let cx1 = u32::try_from(x1.max(0)).map_err(KwinError::from)?.min(width);
            let cy1 = u32::try_from(y1.max(0)).map_err(KwinError::from)?.min(height);
            let cx2 = u32::try_from(x2.max(0)).map_err(KwinError::from)?.min(width);
            let cy2 = u32::try_from(y2.max(0)).map_err(KwinError::from)?.min(height);
            let cw = cx2.saturating_sub(cx1);
            let ch = cy2.saturating_sub(cy1);
            if cw == 0 || ch == 0 {
                return Err(McpError::invalid_params("region has zero area", None));
            }
            let mut cropped = vec![0u8; usize::try_from(cw * ch * 4).map_err(KwinError::from)?];
            for row in 0..ch {
                let src = usize::try_from((cy1 + row) * width * 4 + cx1 * 4).map_err(KwinError::from)?;
                let dst = usize::try_from(row * cw * 4).map_err(KwinError::from)?;
                let len = usize::try_from(cw * 4).map_err(KwinError::from)?;
                cropped[dst..dst + len].copy_from_slice(&rgba[src..src + len]);
            }
            (cropped, cw, ch)
        } else {
            (rgba, width, height)
        };
        let path = xdg.join("screenshot.png");
        let file = std::fs::File::create(&path).map_err(KwinError::from)?;
        let mut enc = png::Encoder::new(file, out_w, out_h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(KwinError::from)?;
        writer.write_image_data(&out_rgba).map_err(KwinError::from)?;
        let path_str = path.to_string_lossy().to_string();
        Ok(structured_result(&peer, format!("{path_str} size={out_w}x{out_h}"), serde_json::json!({
            "path": path_str,
            "width": out_w,
            "height": out_h,
        })).await)
    }

    #[rmcp::tool(
        name = "accessibility_tree",
        description = "Get AT-SPI2 accessibility tree with widget roles, names, states, bounding boxes. By default hides zero-rect/internal nodes; set show_elements=true to include them.",
        annotations(read_only_hint = true)
    )]
    async fn accessibility_tree(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<AccessibilityTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        use atspi::proxy::accessible::ObjectRefExt;
        let zbus_conn = self.with_session(|s| {
            Ok(s.kwin_conn.clone())
        }).await?;
        let a11y_addr: String = atspi::proxy::bus::BusProxy::new(&zbus_conn)
            .await
            .map_err(KwinError::from)?
            .get_address()
            .await
            .map_err(KwinError::from)?;
        let a11y_bus = connect_session_bus(&a11y_addr, std::time::Instant::now() + STARTUP_TIMEOUT)
            .await
            .map_err(|e| McpError::internal_error(format!("AT-SPI bus: {e}"), None))?;
        let root = atspi::proxy::accessible::AccessibleProxy::builder(&a11y_bus)
            .destination("org.a11y.atspi.Registry")
            .map_err(KwinError::from)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(KwinError::from)?;
        let limit = usize::try_from(params.max_depth.unwrap_or(8)).map_err(KwinError::from)?;
        let app_name = params.app_name.map(|s| s.to_lowercase());
        let role = params.role.map(|s| s.to_lowercase());
        let show_elements = params.show_elements.unwrap_or(false);
        let mut out = Vec::new();
        let mut stack = root
            .get_children()
            .await
            .map_err(KwinError::from)?
            .into_iter()
            .rev()
            .map(|obj| (obj, 0usize))
            .collect::<Vec<_>>();
        while let Some((obj, depth)) = stack.pop() {
            let acc = match obj.as_accessible_proxy(&a11y_bus).await {
                Ok(a) => a,
                Err(_) => continue,
            };
            let node = match atspi_node(&acc).await {
                Ok(n) => n,
                Err(_) => continue,
            };
            if depth == 0 && !app_name.as_ref().map(|needle| node.name.to_lowercase().contains(needle)).unwrap_or(true) {
                continue;
            }
            let dominated = role
                .as_ref()
                .map(|needle| node.role.to_lowercase().contains(needle))
                .unwrap_or(true)
                && (show_elements || node.is_useful());
            if dominated { out.push(node.line(depth)); }
            let child_depth = if dominated { depth + 1 } else { depth };
            if child_depth <= limit {
                for child in acc.get_children().await.unwrap_or_default().into_iter().rev() {
                    stack.push((child, child_depth));
                }
            }
        }
        let tree = out.join("\n");
        Ok(structured_result(&peer, tree.clone(), serde_json::json!({"tree": tree})).await)
    }

    #[rmcp::tool(
        name = "find_ui_elements",
        description = "Search UI elements by name/role/description (case-insensitive). Uses CDP DOM queries for Chromium/Electron apps, AT-SPI for native apps.",
        annotations(read_only_hint = true)
    )]
    async fn find_ui_elements(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<FindUiElementsParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = params.query.to_lowercase();
        let mut out = Vec::new();

        let cdp_browser = self.session.lock().await
            .as_ref()
            .and_then(|s| s.cdp_browser.clone());

        match cdp_browser {
            Some(browser) => {
                // CDP path for Chromium/Electron apps
                if let Ok(pages) = browser.pages().await {
                    if let Some(page) = pages.into_iter().next() {
                        let js = r#"JSON.stringify(
                            [...document.querySelectorAll('button, a, input, select, textarea, [role], [onclick], [tabindex]')]
                                .filter(el => el.offsetParent !== null)
                                .map(el => {
                                    const r = el.getBoundingClientRect();
                                    return {
                                        role: el.getAttribute('role') || el.tagName.toLowerCase(),
                                        text: (el.textContent || '').trim().slice(0, 80),
                                        x: Math.round(r.x), y: Math.round(r.y),
                                        w: Math.round(r.width), h: Math.round(r.height)
                                    };
                                })
                        )"#;
                        #[derive(Deserialize)]
                        struct CdpElement { role: String, text: String, x: i32, y: i32, w: i32, h: i32 }
                        if let Ok(result) = page.evaluate(js).await
                            && let Some(val) = result.value()
                            && let Ok(json_str) = serde_json::from_value::<String>(val.clone())
                            && let Ok(elements) = serde_json::from_str::<Vec<CdpElement>>(&json_str)
                        {
                            for el in &elements {
                                if el.w > 1 && el.h > 1
                                    && (el.text.to_lowercase().contains(&query)
                                        || el.role.to_lowercase().contains(&query))
                                {
                                    out.push(format!(
                                        "{}\t{}\t({}, {}, {}x{})",
                                        el.role, el.text, el.x, el.y, el.w, el.h
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            None => {
                // AT-SPI path for native apps (5s timeout)
                let atspi_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    use atspi::proxy::accessible::ObjectRefExt;
                    let zbus_conn = self.with_session(|s| {
                        Ok(s.kwin_conn.clone())
                    }).await?;
                    let a11y_addr: String = atspi::proxy::bus::BusProxy::new(&zbus_conn)
                        .await
                        .map_err(KwinError::from)?
                        .get_address()
                        .await
                        .map_err(KwinError::from)?;
                    let a11y_bus = connect_session_bus(&a11y_addr, std::time::Instant::now() + STARTUP_TIMEOUT)
                        .await
                        .map_err(|e| McpError::internal_error(format!("AT-SPI bus: {e}"), None))?;
                    let root = atspi::proxy::accessible::AccessibleProxy::builder(&a11y_bus)
                        .destination("org.a11y.atspi.Registry")
                        .map_err(KwinError::from)?
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                        .map_err(KwinError::from)?;
                    let mut stack = root
                        .get_children()
                        .await
                        .map_err(KwinError::from)?
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>();
                    let mut results = Vec::new();
                    while let Some(obj) = stack.pop() {
                        let acc = match obj.as_accessible_proxy(&a11y_bus).await {
                            Ok(a) => a,
                            Err(_) => continue,
                        };
                        let node = match atspi_node(&acc).await {
                            Ok(n) => n,
                            Err(_) => continue,
                        };
                        if node.is_useful()
                            && (node.name.to_lowercase().contains(&query)
                                || node.role.to_lowercase().contains(&query))
                        {
                            let (x, y, w, h) = node.bounds;
                            results.push(format!(
                                "{}\t{}\t({}, {}, {}x{})",
                                node.role, node.name, x, y, w, h
                            ));
                        }
                        for child in acc.get_children().await.unwrap_or_default().into_iter().rev() {
                            stack.push(child);
                        }
                    }
                    Ok::<Vec<String>, McpError>(results)
                }).await;
                match atspi_result {
                    Ok(Ok(results)) => out.extend(results),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => eprintln!("find_ui_elements: AT-SPI traversal timed out after 5s"),
                }
            }
        }

        if out.is_empty() {
            Ok(structured_result(&peer, format!("no elements matching '{}'", params.query), serde_json::json!({"matches": 0, "query": params.query})).await)
        } else {
            let results = out.join("\n");
            Ok(structured_result(&peer, results.clone(), serde_json::json!({"matches": out.len(), "query": params.query, "results": results})).await)
        }
    }

    #[rmcp::tool(
        name = "mouse_click",
        description = "Click at window-relative pixel coordinates. button: left/right/middle. double/triple for multi-click."
    )]
    async fn mouse_click(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = params.x;
        let y = params.y;
        let (wx, wy, _) = active_window_info(&self.kwin_conn().await?, &self.kwin_unique_name().await?, &self.host_xdg_dir().await?).await?;
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
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(KwinError::from)?), f32::from(i16::try_from(wy + y).map_err(KwinError::from)?));
        sess.eis.move_abs(ax, ay).map_err(KwinError::from)?;
        for n in 0..count {
            if n > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            sess.eis.button(code, true).map_err(KwinError::from)?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sess.eis.button(code, false).map_err(KwinError::from)?;
        }
        Ok(structured_result(&peer, format!("clicked ({x},{y}) x{count}"), serde_json::json!({
            "action": "click", "x": x, "y": y, "count": count,
        })).await)
    }

    #[rmcp::tool(
        name = "mouse_move",
        description = "Move cursor to window-relative pixel coordinates. Triggers hover effects.",
        annotations(read_only_hint = true)
    )]
    async fn mouse_move(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseMoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = params.x;
        let y = params.y;
        let (wx, wy, _) = active_window_info(&self.kwin_conn().await?, &self.kwin_unique_name().await?, &self.host_xdg_dir().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(KwinError::from)?), f32::from(i16::try_from(wy + y).map_err(KwinError::from)?));
        sess.eis.move_abs(ax, ay).map_err(KwinError::from)?;
        Ok(structured_result(&peer, format!("moved ({x},{y})"), serde_json::json!({
            "action": "move", "x": x, "y": y,
        })).await)
    }

    #[rmcp::tool(
        name = "mouse_scroll",
        description = "Scroll at window-relative pixel coords. delta: positive=down/right, negative=up/left. horizontal/discrete are optional."
    )]
    async fn mouse_scroll(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseScrollParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = params.x;
        let y = params.y;
        let delta = params.delta;
        let (wx, wy, _) = active_window_info(&self.kwin_conn().await?, &self.kwin_unique_name().await?, &self.host_xdg_dir().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(KwinError::from)?), f32::from(i16::try_from(wy + y).map_err(KwinError::from)?));
        sess.eis.move_abs(ax, ay).map_err(KwinError::from)?;
        let horiz = params.horizontal.unwrap_or_default();
        if params.discrete.unwrap_or_default() {
            let (dx, dy) = if horiz { (delta, 0) } else { (0, delta) };
            sess.eis.scroll_discrete(dx, dy).map_err(KwinError::from)?;
        } else {
            let d = f32::from(i16::try_from(delta).map_err(KwinError::from)?) * 15.0;
            let (dx, dy) = if horiz { (d, 0.0) } else { (0.0, d) };
            sess.eis.scroll_smooth(dx, dy).map_err(KwinError::from)?;
        }
        Ok(structured_result(&peer, format!("scrolled {delta} at ({x},{y})"), serde_json::json!({
            "action": "scroll", "x": x, "y": y, "delta": delta,
        })).await)
    }

    #[rmcp::tool(
        name = "mouse_drag",
        description = "Drag between window-relative pixel coords. Smooth 20-step interpolation. button: left/right/middle."
    )]
    async fn mouse_drag(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseDragParams>,
    ) -> Result<CallToolResult, McpError> {
        let from_x = params.from_x;
        let from_y = params.from_y;
        let to_x = params.to_x;
        let to_y = params.to_y;
        let (wx, wy, _) = active_window_info(&self.kwin_conn().await?, &self.kwin_unique_name().await?, &self.host_xdg_dir().await?).await?;
        let code = btn_code(params.button.as_deref())?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let ax = f32::from(i16::try_from(wx + from_x).map_err(KwinError::from)?);
        let ay = f32::from(i16::try_from(wy + from_y).map_err(KwinError::from)?);
        sess.eis.move_abs(ax, ay).map_err(KwinError::from)?;
        sess.eis.button(code, true).map_err(KwinError::from)?;
        let steps = 20i32;
        for step in 1..=steps {
            let cx = f32::from(i16::try_from(wx + from_x + (to_x - from_x) * step / steps).map_err(KwinError::from)?);
            let cy = f32::from(i16::try_from(wy + from_y + (to_y - from_y) * step / steps).map_err(KwinError::from)?);
            sess.eis.move_abs(cx, cy).map_err(KwinError::from)?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        sess.eis.button(code, false).map_err(KwinError::from)?;
        Ok(structured_result(&peer, format!("dragged ({from_x},{from_y})->({to_x},{to_y})"), serde_json::json!({
            "action": "drag", "from_x": from_x, "from_y": from_y, "to_x": to_x, "to_y": to_y,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_type",
        description = "Type ASCII text character by character. For non-ASCII use keyboard_type_unicode."
    )]
    async fn keyboard_type(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardTypeParams>,
    ) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        for ch in params.text.chars() {
            let (code, needs_shift) = char_key(ch)?;
            if needs_shift { sess.eis.key(42, true).map_err(KwinError::from)?; }
            sess.eis.key(code, true).map_err(KwinError::from)?;
            sess.eis.key(code, false).map_err(KwinError::from)?;
            if needs_shift { sess.eis.key(42, false).map_err(KwinError::from)?; }
        }
        Ok(structured_result(&peer, format!("typed: {}", params.text), serde_json::json!({
            "action": "type", "text": params.text,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_key",
        description = "Press key combo (e.g. 'Return', 'ctrl+c', 'alt+F4', 'shift+Tab')."
    )]
    async fn keyboard_key(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        for m in &mods {
            sess.eis.key(*m, true).map_err(KwinError::from)?;
        }
        let k = main.ok_or_else(|| {
            McpError::invalid_params(format!("unknown key in combo '{}'", params.key), None)
        })?;
        sess.eis.key(k, true).map_err(KwinError::from)?;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        sess.eis.key(k, false).map_err(KwinError::from)?;
        for m in mods.iter().rev() {
            sess.eis.key(*m, false).map_err(KwinError::from)?;
        }
        Ok(structured_result(&peer, format!("key: {}", params.key), serde_json::json!({
            "action": "key", "key": params.key,
        })).await)
    }

    #[rmcp::tool(
        name = "launch_app",
        description = "Launch an application inside the container. Automatically detects Chromium/Electron apps and enables CDP-based element discovery in find_ui_elements."
    )]
    async fn launch_app(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<LaunchAppParams>,
    ) -> Result<CallToolResult, McpError> {
        use std::io::Write;
        use futures::StreamExt;

        // Detect Chromium/Electron from command string to inject CDP flag
        let cmd_lower = params.command.to_lowercase();
        let cmd_chromium = cmd_lower.contains("chrom") || cmd_lower.contains("electron")
            || cmd_lower.contains("code") || cmd_lower.contains("brave")
            || cmd_lower.contains("edge") || cmd_lower.contains("vivaldi");

        let cdp_port = if cmd_chromium {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(KwinError::from)?;
            let port = listener.local_addr().map_err(KwinError::from)?.port();
            drop(listener);
            Some(port)
        } else {
            None
        };

        // Record current active window ID before launching
        let (conn, kwin_unique, xdg) = {
            let guard = self.session.lock().await;
            let sess = guard.as_ref().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            (sess.kwin_conn.clone(), sess.kwin_unique_name.clone(), sess.host_xdg_dir.clone())
        };
        let prev_window_id = active_window_info(&conn, &kwin_unique, &xdg).await
            .map(|(_, _, geo)| geo.id)
            .ok();

        // Launch (with CDP port + isolated user-data-dir if Chromium)
        let launch_cmd = match cdp_port {
            Some(port) => {
                let mut cmd = params.command.clone();
                if !cmd.contains("--user-data-dir") {
                    cmd.push_str(" --user-data-dir=/tmp/chrome-cdp");
                }
                format!("{cmd} --remote-debugging-port={port}")
            }
            None => params.command.clone(),
        };
        {
            let mut guard = self.session.lock().await;
            let sess = guard.as_mut().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            writeln!(sess.bwrap_stdin, "{launch_cmd}").map_err(KwinError::from)?;
            sess.bwrap_stdin.flush().map_err(KwinError::from)?;
        }

        // Poll until a NEW window appears (different ID from before launch)
        let mut win_geo = None;
        for _ in 0..75_u32 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Ok((_, _, geo)) = active_window_info(&conn, &kwin_unique, &xdg).await
                && prev_window_id.as_deref() != Some(&geo.id) {
                win_geo = Some(geo);
                break;
            }
        }

        // Connect CDP if command hinted Chromium OR window confirms it (5s timeout)
        let mut cdp_connected = false;
        let win_chromium = win_geo.as_ref().map(|g| {
            let rc = g.resource_class.to_lowercase();
            let rn = g.resource_name.to_lowercase();
            eprintln!("launch_app: resourceClass={rc} resourceName={rn} pid={}", g.pid);
            rc.contains("electron") || rn.contains("electron")
                || rc.contains("chromium") || rn.contains("chromium")
                || rc.contains("chrome") || rn.contains("chrome")
        }).unwrap_or(false);
        if let Some(port) = cdp_port.filter(|_| cmd_chromium || win_chromium) {
            let cdp_url = format!("http://127.0.0.1:{port}");
            for _ in 0..25_u32 {
                match chromiumoxide::Browser::connect(&cdp_url).await {
                    Ok((browser, mut handler)) => {
                        tokio::spawn(async move { while handler.next().await.is_some() {} });
                        let mut guard = self.session.lock().await;
                        if let Some(sess) = guard.as_mut() {
                            sess.cdp_browser = Some(Arc::new(browser));
                        }
                        cdp_connected = true;
                        break;
                    }
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }

        match win_geo {
            Some(geo) => Ok(structured_result(&peer, format!("launched: {} window: {}", params.command, geo.id), serde_json::json!({
                "action": "launch", "command": params.command, "window": geo.id,
                "cdp": cdp_connected,
            })).await),
            None => Ok(structured_result(&peer, format!("launched: {} (no window after 15s)", params.command), serde_json::json!({
                "action": "launch", "command": params.command, "window": "timeout",
                "cdp": false,
            })).await),
        }
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
