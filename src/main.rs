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

// ── Kernel / protocol constants ──────────────────────────────────────────

// Linux evdev keycode for LeftShift (include/uapi/linux/input-event-codes.h).
const LINUX_KEY_LEFTSHIFT: u32 = 42;

// KWin org.kde.KWin.EIS.RemoteDesktop.connectToEIS() capabilities bitfield.
// bit 0 (0b001) = keyboard, bit 1 (0b010) = pointer, bit 2 (0b100) = touch.
// 0b011 = keyboard + pointer (what this server needs).
const EIS_CAPS_KBD_POINTER: i32 = 0b011;

// ── Timings ──────────────────────────────────────────────────────────────

use std::time::Duration;

// Session startup: general session-bus / wayland socket wait.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_POLL: Duration = Duration::from_millis(50);

// EIS (Emulated Input Sender) negotiation.
const EIS_NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const EIS_NEGOTIATION_POLL: Duration = Duration::from_millis(50);

// xdg-dbus-proxy socket appearance.
const DBUS_PROXY_TIMEOUT: Duration = Duration::from_secs(3);
const DBUS_PROXY_POLL: Duration = Duration::from_millis(20);

// KWin unique-name discovery: per-candidate introspect probe timeout.
const KWIN_NAME_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

// AT-SPI tree traversal hard timeout (find_ui_elements).
const ATSPI_TRAVERSAL_TIMEOUT: Duration = Duration::from_secs(5);

// Input-event pacing (clicks, drag steps, key hold).
const INPUT_EVENT_DELAY: Duration = Duration::from_millis(50);

// Mouse drag interpolation step count.
const DRAG_STEPS: i32 = 20;

// Pixels per smooth-scroll tick.
const SCROLL_SMOOTH_PIXELS_PER_TICK: f32 = 15.0;

// launch_app: window-appear polling.
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(200);
const LAUNCH_WINDOW_POLLS: u32 = 75;  // 15s total

// launch_app: CDP connect retry.
const CDP_CONNECT_POLLS: u32 = 25;    // 5s total (reuses LAUNCH_POLL_INTERVAL)

// ── Virtual-session display & font settings ──────────────────────────────

const VIRTUAL_SCREEN_WIDTH: u32 = 2000;
const VIRTUAL_SCREEN_HEIGHT: u32 = 1875;

const KDE_SCALE_FACTOR: &str = "1"; // 1 | 2 | 3
const KDE_FORCE_FONT_DPI: u32 = 96; // 96 | 120 | 144 | 192
const KDE_HINT_STYLE: &str = "hintnone"; // hintnone | hintslight | hintmedium | hintfull
const KDE_SUB_PIXEL: &str = "none"; // none | rgb | bgr | vrgb | vbgr

const UI_FONT_FAMILY: &str = "Noto Sans";
const UI_FONT_SIZE: u32 = 14;
const UI_FONT_SIZE_SMALL: u32 = 12;

const FIXED_FONT_FAMILY: &str = "Hack";
const FIXED_FONT_SIZE: u32 = 14;

const FONT_WEIGHT_REGULAR: u32 = 400;
const FONT_WEIGHT_BOLD: u32 = 700;

fn qt_font_spec(family: &str, size: u32, weight: u32, bold_suffix: bool) -> String {
    // Qt KConfig font format: family,size,-1,5,weight,0,0,0,0,0,0,0,0,0,0,1[,Bold]
    let suffix = if bold_suffix { ",Bold" } else { "" };
    format!("{family},{size},-1,5,{weight},0,0,0,0,0,0,0,0,0,0,1{suffix}")
}

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
        let deadline = std::time::Instant::now() + EIS_NEGOTIATION_TIMEOUT;
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
            std::thread::sleep(EIS_NEGOTIATION_POLL);
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
    _wallet_conn: zbus::Connection,   // owns org.kde.kwalletd6, serves KWalletEmulator (kept alive)
    kwin_unique_name: String,
    eis: Eis,
    bwrap_child: std::process::Child,
    bwrap_stdin: std::process::ChildStdin,
    host_xdg_dir: std::path::PathBuf,
    _uinput_mouse: evdev::uinput::VirtualDevice,
    _uinput_keyboard: evdev::uinput::VirtualDevice,
    cdp_browser: Option<Arc<chromiumoxide::Browser>>,
    dbus_proxy_child: Option<std::process::Child>,
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

fn cleanup_stale_session_files(dir: &std::path::Path) {
    const STALE_FILES: &[&str] = &[
        "bus",
        "wayland-0",
        "wayland-0.lock",
        "pipewire-0",
        "pipewire-0.lock",
        "pipewire-0-manager",
        "pipewire-0-manager.lock",
        "system_bus_socket",
        "dbus-ready",
        "bridge-ready",
        "screenshot.png",
    ];
    const STALE_DIRS: &[&str] = &[
        "at-spi",
        "dbus-1",
        "dconf",
        "doc",
    ];
    for name in STALE_FILES {
        let _ = std::fs::remove_file(dir.join(name));
    }
    for name in STALE_DIRS {
        let _ = std::fs::remove_dir_all(dir.join(name));
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("script_") && name_str.ends_with(".js") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
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
    if let Some(mut proxy) = sess.dbus_proxy_child.take() {
        let _ = proxy.kill();
        let _ = proxy.wait();
    }
    cleanup_stale_session_files(&sess.host_xdg_dir);
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
        w ? JSON.stringify({{x:w.clientGeometry.x,y:w.clientGeometry.y,\
        w:w.clientGeometry.width,h:w.clientGeometry.height,\
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

// ── KWallet emulator — serves snapshot of host kwallet inside container ──

struct WalletData {
    network_wallet: String,
    // folder → key → password bytes
    entries: std::sync::Mutex<std::collections::HashMap<String, std::collections::HashMap<String, Vec<u8>>>>,
}

struct KWalletEmulator {
    data: Arc<WalletData>,
}

#[zbus::interface(name = "org.kde.KWallet")]
impl KWalletEmulator {
    #[zbus(name = "isEnabled")]
    async fn is_enabled(&self) -> bool { true }

    #[zbus(name = "networkWallet")]
    async fn network_wallet(&self) -> String { self.data.network_wallet.clone() }

    #[zbus(name = "localWallet")]
    async fn local_wallet(&self) -> String { self.data.network_wallet.clone() }

    #[zbus(name = "wallets")]
    async fn wallets(&self) -> Vec<String> { vec![self.data.network_wallet.clone()] }

    #[zbus(name = "isOpen")]
    async fn is_open(&self, _wallet: String) -> bool { true }

    #[zbus(name = "open")]
    async fn open(&self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        wallet: String, _w_id: i64, _appid: String,
    ) -> i32 {
        let emitter = emitter.to_owned();
        tokio::spawn(async move {
            let _ = Self::wallet_opened(&emitter, &wallet).await;
        });
        1
    }

    #[zbus(name = "openPath")]
    async fn open_path(&self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        path: String, _w_id: i64, _appid: String,
    ) -> i32 {
        let emitter = emitter.to_owned();
        tokio::spawn(async move {
            let _ = Self::wallet_opened(&emitter, &path).await;
        });
        1
    }

    #[zbus(name = "openAsync")]
    async fn open_async(&self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        _wallet: String, _w_id: i64, _appid: String, _handle_session: bool,
    ) -> i32 {
        let emitter = emitter.to_owned();
        // tId must be positive and match what we'll emit in the walletAsyncOpened signal.
        let tid: i32 = 1;
        let handle: i32 = 1;
        tokio::spawn(async move {
            let _ = Self::wallet_async_opened(&emitter, tid, handle).await;
        });
        tid
    }

    #[zbus(signal, name = "walletAsyncOpened")]
    async fn wallet_async_opened(emitter: &zbus::object_server::SignalEmitter<'_>, t_id: i32, handle: i32) -> zbus::Result<()>;

    #[zbus(signal, name = "walletOpened")]
    async fn wallet_opened(emitter: &zbus::object_server::SignalEmitter<'_>, wallet: &str) -> zbus::Result<()>;

    #[zbus(signal, name = "walletClosed")]
    async fn wallet_closed(emitter: &zbus::object_server::SignalEmitter<'_>, wallet: &str) -> zbus::Result<()>;

    #[zbus(name = "close")]
    async fn close(&self, _handle: i32, _force: bool, _appid: String) -> i32 { 0 }

    #[zbus(name = "closeWallet")]
    async fn close_wallet(&self, _wallet: String, _force: bool) -> i32 { 0 }

    #[zbus(name = "sync")]
    async fn sync(&self, _handle: i32, _appid: String) {}

    #[zbus(name = "disconnectApplication")]
    async fn disconnect_application(&self, _wallet: String, _appid: String) -> bool { true }

    #[zbus(name = "folderList")]
    async fn folder_list(&self, _handle: i32, _appid: String) -> Vec<String> {
        match self.data.entries.lock() {
            Ok(e) => e.keys().cloned().collect(),
            Err(_) => vec![],
        }
    }

    #[zbus(name = "hasFolder")]
    async fn has_folder(&self, _handle: i32, folder: String, _appid: String) -> bool {
        match self.data.entries.lock() {
            Ok(e) => e.contains_key(&folder),
            Err(_) => false,
        }
    }

    #[zbus(name = "createFolder")]
    async fn create_folder(&self, _handle: i32, folder: String, _appid: String) -> bool {
        if let Ok(mut e) = self.data.entries.lock() {
            e.entry(folder).or_insert_with(std::collections::HashMap::new);
            true
        } else { false }
    }

    #[zbus(name = "entryList")]
    async fn entry_list(&self, _handle: i32, folder: String, _appid: String) -> Vec<String> {
        match self.data.entries.lock() {
            Ok(e) => e.get(&folder).map(|f| f.keys().cloned().collect()).unwrap_or_default(),
            Err(_) => vec![],
        }
    }

    #[zbus(name = "hasEntry")]
    async fn has_entry(&self, _handle: i32, folder: String, key: String, _appid: String) -> bool {
        match self.data.entries.lock() {
            Ok(e) => e.get(&folder).is_some_and(|f| f.contains_key(&key)),
            Err(_) => false,
        }
    }

    #[zbus(name = "entryType")]
    async fn entry_type(&self, _handle: i32, folder: String, key: String, _appid: String) -> i32 {
        match self.data.entries.lock() {
            Ok(e) => if e.get(&folder).is_some_and(|f| f.contains_key(&key)) { 1 } else { 0 },
            Err(_) => 0,
        }
    }

    #[zbus(name = "readPassword")]
    async fn read_password(&self, _handle: i32, folder: String, key: String, _appid: String) -> String {
        match self.data.entries.lock() {
            Ok(e) => e.get(&folder)
                .and_then(|f| f.get(&key))
                .and_then(|b| String::from_utf8(b.clone()).ok())
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    #[zbus(name = "readEntry")]
    async fn read_entry(&self, _handle: i32, folder: String, key: String, _appid: String) -> Vec<u8> {
        match self.data.entries.lock() {
            Ok(e) => e.get(&folder).and_then(|f| f.get(&key)).cloned().unwrap_or_default(),
            Err(_) => vec![],
        }
    }

    #[zbus(name = "writePassword")]
    async fn write_password(&self, _handle: i32, folder: String, key: String, value: String, _appid: String) -> i32 {
        if let Ok(mut e) = self.data.entries.lock() {
            e.entry(folder).or_insert_with(std::collections::HashMap::new).insert(key, value.into_bytes());
        }
        0
    }

    #[zbus(name = "writeEntry")]
    async fn write_entry(&self, _handle: i32, folder: String, key: String, value: Vec<u8>, _entry_type: i32, _appid: String) -> i32 {
        if let Ok(mut e) = self.data.entries.lock() {
            e.entry(folder).or_insert_with(std::collections::HashMap::new).insert(key, value);
        }
        0
    }

    #[zbus(name = "removeEntry")]
    async fn remove_entry(&self, _handle: i32, folder: String, key: String, _appid: String) -> i32 {
        if let Ok(mut e) = self.data.entries.lock()
            && let Some(f) = e.get_mut(&folder)
        {
            f.remove(&key);
        }
        0
    }

    #[zbus(name = "removeFolder")]
    async fn remove_folder(&self, _handle: i32, folder: String, _appid: String) -> bool {
        if let Ok(mut e) = self.data.entries.lock() {
            e.remove(&folder);
            true
        } else { false }
    }
}

async fn dump_host_wallet(host_conn: &zbus::Connection)
    -> Result<(String, std::collections::HashMap<String, std::collections::HashMap<String, Vec<u8>>>), String>
{
    let dest = "org.kde.kwalletd6";
    let path = "/modules/kwalletd6";
    let iface = "org.kde.KWallet";
    let appid = "kwin-mcp";

    let reply = host_conn.call_method(Some(dest), path, Some(iface), "networkWallet", &()).await
        .map_err(|e| format!("networkWallet: {e}"))?;
    let network_wallet: String = reply.body().deserialize()
        .map_err(|e| format!("networkWallet decode: {e}"))?;

    let reply = host_conn.call_method(Some(dest), path, Some(iface), "open",
        &(network_wallet.as_str(), 0i64, appid)).await
        .map_err(|e| format!("open: {e}"))?;
    let handle: i32 = reply.body().deserialize()
        .map_err(|e| format!("open decode: {e}"))?;
    if handle < 0 {
        return Err(format!("host kwallet open returned {handle} (user denied access)"));
    }

    let reply = host_conn.call_method(Some(dest), path, Some(iface), "folderList",
        &(handle, appid)).await
        .map_err(|e| format!("folderList: {e}"))?;
    let folders: Vec<String> = reply.body().deserialize()
        .map_err(|e| format!("folderList decode: {e}"))?;

    let mut out: std::collections::HashMap<String, std::collections::HashMap<String, Vec<u8>>> =
        std::collections::HashMap::new();
    for folder in folders {
        let reply = match host_conn.call_method(Some(dest), path, Some(iface), "entryList",
            &(handle, folder.as_str(), appid)).await
        {
            Ok(r) => r,
            Err(e) => { eprintln!("entryList {folder}: {e}"); continue; }
        };
        let keys: Vec<String> = match reply.body().deserialize() {
            Ok(k) => k,
            Err(e) => { eprintln!("entryList {folder} decode: {e}"); continue; }
        };
        let mut f_entries: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        for key in keys {
            let reply = match host_conn.call_method(Some(dest), path, Some(iface), "readEntry",
                &(handle, folder.as_str(), key.as_str(), appid)).await
            {
                Ok(r) => r,
                Err(e) => { eprintln!("readEntry {folder}/{key}: {e}"); continue; }
            };
            if let Ok(bytes) = reply.body().deserialize::<Vec<u8>>() {
                f_entries.insert(key, bytes);
            }
        }
        out.insert(folder, f_entries);
    }

    let _ = host_conn.call_method(Some(dest), path, Some(iface), "close",
        &(handle, false, appid)).await;

    Ok((network_wallet, out))
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
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "KDE Wayland desktop automation in an isolated container. \
                Required first step: call session_start — every other tool fails until it succeeds. It is idempotent; if a session is already up you get its info back without restarting it (call session_stop + session_start to restart). \
                Typical flow: session_start → launch_app → find_ui_elements or accessibility_tree → mouse_click / keyboard_type / keyboard_key → screenshot to verify → session_stop when done. \
                All mouse/screenshot coordinates are pixels relative to the active window's top-left (not the virtual display). \
                The virtual display is {VIRTUAL_SCREEN_WIDTH}x{VIRTUAL_SCREEN_HEIGHT} but windows are auto-maximized; a window-relative click at (100,100) lands 100px from the window's top-left corner."
            ))
    }
}

#[rmcp::tool_router]
impl KwinMcp {
    #[rmcp::tool(
        name = "session_start",
        description = "Boot an isolated KDE Wayland desktop. Required before every other tool — all fail with 'no session' until this succeeds. Idempotent: if a session is already running, returns its bus name and workdir without disturbing it (status=already_running). To restart with different settings (e.g. toggle writable), call session_stop first. Set writable=true only if the task needs to persist files to the host; default false keeps writes ephemeral via an overlay."
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
            let guard = self.session.lock().await;
            if let Some(existing) = guard.as_ref() {
                let bus_name = existing.kwin_conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
                let workdir = existing.host_xdg_dir.display().to_string();
                let msg = format!(
                    "{version_stamp} — session already running bus={bus_name} kwin={} workdir={workdir}. Call session_stop first to restart.",
                    existing.kwin_unique_name,
                );
                return Ok(structured_result(&peer, msg, serde_json::json!({
                    "status": "already_running",
                    "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
                    "commit": env!("GIT_HASH"),
                    "bus": bus_name,
                    "kwin_unique": existing.kwin_unique_name,
                    "workdir": workdir,
                })).await);
            }
        }
        let pid = std::process::id();
        let host_xdg_dir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
        std::fs::create_dir_all(&host_xdg_dir).map_err(|e| ver_err(e.to_string()))?;
        cleanup_stale_session_files(&host_xdg_dir);
        std::fs::create_dir_all(host_xdg_dir.join("tmp")).map_err(|e| ver_err(e.to_string()))?;
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
            format!("[General]\nforceFontDPI={KDE_FORCE_FONT_DPI}\n")
        ).map_err(|e| ver_err(format!("write kcmfonts: {e}")))?;
        let fonts_conf_path = host_xdg_dir.join("fonts.conf");
        std::fs::write(&fonts_conf_path, format!(
            "<?xml version=\"1.0\"?>\n\
             <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
             <fontconfig>\n\
             <match target=\"font\">\n\
             <edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\n\
             <edit name=\"hintstyle\" mode=\"assign\"><const>{KDE_HINT_STYLE}</const></edit>\n\
             <edit name=\"antialias\" mode=\"assign\"><bool>true</bool></edit>\n\
             <edit name=\"rgba\" mode=\"assign\"><const>{KDE_SUB_PIXEL}</const></edit>\n\
             </match>\n\
             </fontconfig>\n"
        )).map_err(|e| ver_err(format!("write fonts.conf: {e}")))?;
        // Read host kdeglobals and patch display settings for the virtual session
        let home = std::env::var("HOME").map_err(|e| ver_err(e.to_string()))?;
        let real_kdeglobals = std::path::Path::new(&home).join(".config/kdeglobals");
        let mut kdeglobals_content = std::fs::read_to_string(&real_kdeglobals).unwrap_or_default();
        let ui_regular = qt_font_spec(UI_FONT_FAMILY, UI_FONT_SIZE, FONT_WEIGHT_REGULAR, false);
        let ui_small = qt_font_spec(UI_FONT_FAMILY, UI_FONT_SIZE_SMALL, FONT_WEIGHT_REGULAR, false);
        let ui_bold = qt_font_spec(UI_FONT_FAMILY, UI_FONT_SIZE, FONT_WEIGHT_BOLD, true);
        let fixed = qt_font_spec(FIXED_FONT_FAMILY, FIXED_FONT_SIZE, FONT_WEIGHT_REGULAR, false);
        let replacements: [(&str, String); 10] = [
            ("ScaleFactor=", format!("ScaleFactor={KDE_SCALE_FACTOR}")),
            ("ScreenScaleFactors=", "ScreenScaleFactors=".to_owned()),
            ("XftHintStyle=", format!("XftHintStyle={KDE_HINT_STYLE}")),
            ("XftSubPixel=", format!("XftSubPixel={KDE_SUB_PIXEL}")),
            ("font=", format!("font={ui_regular}")),
            ("menuFont=", format!("menuFont={ui_regular}")),
            ("smallestReadableFont=", format!("smallestReadableFont={ui_small}")),
            ("toolBarFont=", format!("toolBarFont={ui_regular}")),
            ("activeFont=", format!("activeFont={ui_bold}")),
            ("fixed=", format!("fixed={fixed}")),
        ];
        for (prefix, replacement) in &replacements {
            kdeglobals_content = kdeglobals_content
                .lines()
                .map(|line| {
                    if line.starts_with(prefix) { replacement.clone() }
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
        std::fs::write(&fc_hinting_path, format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
            <fontconfig>\n\
            <match target=\"font\"><edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\
            <edit name=\"hintstyle\" mode=\"assign\"><const>{KDE_HINT_STYLE}</const></edit></match>\n\
            <match target=\"pattern\"><edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\
            <edit name=\"hintstyle\" mode=\"assign\"><const>{KDE_HINT_STYLE}</const></edit></match>\n\
            </fontconfig>\n"
        )).map_err(|e| ver_err(format!("write fontconfig hinting: {e}")))?;
        let fc_lcd_path = host_xdg_dir.join("11-lcdfilter-none.conf");
        std::fs::write(&fc_lcd_path, format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
            <fontconfig>\n\
            <match target=\"font\"><edit name=\"lcdfilter\" mode=\"assign\"><const>lcdnone</const></edit>\
            <edit name=\"rgba\" mode=\"assign\"><const>{KDE_SUB_PIXEL}</const></edit></match>\n\
            <match target=\"pattern\"><edit name=\"lcdfilter\" mode=\"assign\"><const>lcdnone</const></edit>\
            <edit name=\"rgba\" mode=\"assign\"><const>{KDE_SUB_PIXEL}</const></edit></match>\n\
            </fontconfig>\n"
        )).map_err(|e| ver_err(format!("write fontconfig lcd: {e}")))?;
        let fc_hinting_str = fc_hinting_path.display().to_string();
        let fc_lcd_str = fc_lcd_path.display().to_string();
        // Inline entrypoint: starts dbus/kwin/services, reads stdin for launch_app
        let entrypoint = format!(
            "set -u\n\
            export XDG_RUNTIME_DIR={xdg_dir_str}\n\
            export WAYLAND_DISPLAY=wayland-0\n\
            export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1\n\
            export QT_SCALE_FACTOR={KDE_SCALE_FACTOR}\n\
            export GDK_SCALE={KDE_SCALE_FACTOR}\n\
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
            kwin_wayland --virtual --xwayland --no-lockscreen --width {VIRTUAL_SCREEN_WIDTH} --height {VIRTUAL_SCREEN_HEIGHT} &\n\
            sleep 0.3\n\
            dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR QT_QPA_PLATFORM PATH HOME USER ATSPI_DBUS_IMPLEMENTATION\n\
            at-spi-bus-launcher --launch-immediately &\n\
            pipewire &\n\
            wireplumber &\n\
            while read -r cmd; do\n\
                eval \"$cmd\" &\n\
            done\n"
        );
        // Create uinput virtual devices before bwrap so we can bind-mount them
        let (uinput_mouse, mouse_evdev, uinput_keyboard, kbd_evdev) =
            create_uinput_devices().map_err(|e| ver_err(format!("uinput: {e}")))?;
        let mouse_evdev_str = mouse_evdev.display().to_string();
        let kbd_evdev_str = kbd_evdev.display().to_string();
        eprintln!("session_start: uinput mouse={mouse_evdev_str} keyboard={kbd_evdev_str}");

        // Spawn xdg-dbus-proxy to expose ONLY NetworkManager from host system bus
        // into the container. Chromium needs NM to detect online state and not hang page loads.
        let proxy_sock = host_xdg_dir.join("system_bus_socket");
        let proxy_sock_str = proxy_sock.display().to_string();
        let mut dbus_proxy_cmd = std::process::Command::new("xdg-dbus-proxy");
        dbus_proxy_cmd.args([
            "unix:path=/run/dbus/system_bus_socket",
            &proxy_sock_str,
            "--filter",
            "--talk=org.freedesktop.NetworkManager",
        ]);
        dbus_proxy_cmd.stdout(std::process::Stdio::null());
        dbus_proxy_cmd.stderr(std::process::Stdio::inherit());
        eprintln!("session_start: spawning xdg-dbus-proxy → {proxy_sock_str}");
        let dbus_proxy_child = dbus_proxy_cmd.spawn().map_err(|e| ver_err(format!("xdg-dbus-proxy: {e}")))?;
        let proxy_deadline = std::time::Instant::now() + DBUS_PROXY_TIMEOUT;
        while !proxy_sock.exists() && std::time::Instant::now() < proxy_deadline {
            std::thread::sleep(DBUS_PROXY_POLL);
        }
        if !proxy_sock.exists() {
            return Err(ver_err("xdg-dbus-proxy socket never appeared".to_owned()));
        }

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
            "--ro-bind-try", &proxy_sock_str, "/run/dbus/system_bus_socket",
            "--bind", &xdg_dir_str, &xdg_dir_str,
            // System config overrides (read-only)
            "--ro-bind", &atspi_conf_path.display().to_string(), "/usr/share/defaults/at-spi2/accessibility.conf",
            "--ro-bind", &fc_hinting_str, "/usr/share/fontconfig/conf.default/10-hinting-slight.conf",
            "--ro-bind", &fc_lcd_str, "/usr/share/fontconfig/conf.default/11-lcdfilter-default.conf",
            // Mask dbus service files so the container's dbus-daemon doesn't auto-activate
            // the real kwalletd6/ksecretd — our emulator owns org.kde.kwalletd6 instead.
            "--ro-bind", "/dev/null", "/usr/share/dbus-1/services/org.kde.kwalletd6.service",
            "--ro-bind", "/dev/null", "/usr/share/dbus-1/services/org.kde.secretservicecompat.service",
            "--ro-bind", "/dev/null", "/usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.kwallet.service",
            "--ro-bind", "/dev/null", "/usr/share/dbus-1/services/org.kde.secretprompter.service",
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
                return Err(ver_err("bwrap stdin not available".to_owned()));
            }
        };
        let dbus_proxy_pid = dbus_proxy_child.id();
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
            if let Ok(signed) = i32::try_from(dbus_proxy_pid) {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(signed), nix::sys::signal::Signal::SIGTERM);
            }
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
                    KWIN_NAME_PROBE_TIMEOUT,
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
                if let Ok(Ok(xml)) = probe_result
                    && xml.contains("connectToEIS") {
                    found = Some(name_str.to_owned());
                    break;
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
        let (eis_fd, _cookie) = match eis_proxy.connect_to_eis(EIS_CAPS_KBD_POINTER).await {
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
                    kwin_conn, _proxy_conn: proxy_conn, _wallet_conn: wallet_conn,
                    kwin_unique_name: kwin_unique_name.clone(),
                    eis, bwrap_child, bwrap_stdin, host_xdg_dir,
                    _uinput_mouse: uinput_mouse, _uinput_keyboard: uinput_keyboard,
                    cdp_browser: None,
                    dbus_proxy_child: Some(dbus_proxy_child),
                });
                return Ok(structured_result(&peer, msg, serde_json::json!({
                    "status": "started", "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
                    "commit": env!("GIT_HASH"), "bus": bus_name, "kwin_unique": kwin_unique_name,
                    "workdir": workdir, "writable": writable,
                })).await);
            }
        };
        // Carbon-copy host kwallet into container at startup. Once dumped, serve it
        // locally from an in-container emulator — no runtime host round-trips, no password prompts.
        let (network_wallet, wallet_entries) = match dump_host_wallet(&host_conn).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("session_start: host kwallet dump failed: {e}");
                ("kdewallet".to_owned(), std::collections::HashMap::new())
            }
        };
        let total_entries: usize = wallet_entries.values().map(|v| v.len()).sum();
        eprintln!("session_start: kwallet dump: {} folders, {} entries", wallet_entries.len(), total_entries);
        let wallet_data = Arc::new(WalletData {
            network_wallet,
            entries: std::sync::Mutex::new(wallet_entries),
        });
        let emulator = KWalletEmulator { data: wallet_data };
        if let Err(e) = wallet_conn.object_server().at("/modules/kwalletd6", emulator).await {
            eprintln!("session_start: register kwallet emulator failed: {e}");
        }
        if let Err(e) = wallet_conn.request_name("org.kde.kwalletd6").await {
            eprintln!("session_start: claim org.kde.kwalletd6 failed: {e}");
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
            _wallet_conn: wallet_conn,
            kwin_unique_name: kwin_unique_name.clone(),
            eis,
            bwrap_child,
            bwrap_stdin,
            host_xdg_dir,
            _uinput_mouse: uinput_mouse,
            _uinput_keyboard: uinput_keyboard,
            cdp_browser: None,
            dbus_proxy_child: Some(dbus_proxy_child),
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
        description = "Tear down the current session and kill every process in the container. Call when finished — sessions do not auto-clean on disconnect. No-op if no session is running.",
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
        description = "Capture the active window as a PNG written to the session workdir. Use this when you need to see what the UI looks like, verify a state change visually, or read text/images the accessibility tree can't expose. Pass region=[x1,y1,x2,y2] in window-relative pixels to crop — prefer cropping over full captures when you already know which area matters, it returns a much smaller file. Requires an active window (call launch_app first if needed).",
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
        description = "Dump the full widget hierarchy of the active app — roles, names, states, bounds — indented by depth. Use this when you need structural context (what exists, what contains what, what state things are in). Prefer find_ui_elements when you already know the name/role of one specific widget. app_name filters to matching top-level apps; max_depth caps traversal (default 8); role filters to matching role names. show_elements=true keeps zero-rect and unnamed nodes — default false trims them out.",
        annotations(read_only_hint = true)
    )]
    async fn accessibility_tree(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<AccessibilityTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        // CDP path for Chromium/Electron apps
        let cdp_browser = self.session.lock().await
            .as_ref()
            .and_then(|s| s.cdp_browser.clone());
        if let Some(browser) = cdp_browser
            && let Ok(pages) = browser.pages().await {
                for page in &pages {
                    let url = page.url().await.ok().flatten().unwrap_or_default();
                    if url.starts_with("chrome://") || url.starts_with("chrome-extension://") {
                        continue;
                    }
                    use chromiumoxide::cdp::browser_protocol::accessibility::{
                        GetFullAxTreeParams, GetFullAxTreeReturns,
                    };
                    let depth = params.max_depth.map(i64::from);
                    let mut cmd = GetFullAxTreeParams::builder();
                    if let Some(d) = depth { cmd = cmd.depth(d); }
                    if let Ok(result) = page.execute(cmd.build()).await {
                        let returns: &GetFullAxTreeReturns = &result;
                        // Build parent→children index
                        let mut children_map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                        let mut node_map: std::collections::HashMap<String, &chromiumoxide::cdp::browser_protocol::accessibility::AxNode> = std::collections::HashMap::new();
                        let mut root_ids = Vec::new();
                        for node in &returns.nodes {
                            let id = node.node_id.inner().to_string();
                            node_map.insert(id.clone(), node);
                            if let Some(ref pid) = node.parent_id {
                                children_map.entry(pid.inner().to_string()).or_default().push(id);
                            } else {
                                root_ids.push(id);
                            }
                        }
                        // Walk tree depth-first
                        let show = params.show_elements.unwrap_or(false);
                        let role_filter = params.role.as_ref().map(|s| s.to_lowercase());
                        let mut out = Vec::new();
                        let mut stack: Vec<(String, usize)> = root_ids.into_iter().rev().map(|id| (id, 0_usize)).collect();
                        while let Some((id, depth_level)) = stack.pop() {
                            if let Some(node) = node_map.get(&id) {
                                if node.ignored && !show { /* skip */ } else {
                                    let role = node.role.as_ref()
                                        .and_then(|v| v.value.as_ref())
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("none");
                                    let name = node.name.as_ref()
                                        .and_then(|v| v.value.as_ref())
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if !name.is_empty() || show {
                                        let dominated = role_filter.as_ref()
                                            .map(|f| role.to_lowercase().contains(f))
                                            .unwrap_or(true);
                                        if dominated {
                                            out.push(format!("{}{}\t{}", "  ".repeat(depth_level), role, name));
                                        }
                                    }
                                }
                                if let Some(kids) = children_map.get(&id) {
                                    for kid in kids.iter().rev() {
                                        stack.push((kid.clone(), depth_level + 1));
                                    }
                                }
                            }
                        }
                        let tree = out.join("\n");
                        return Ok(structured_result(&peer, tree.clone(), serde_json::json!({"tree": tree, "source": "cdp"})).await);
                    }
                }
            }
        // AT-SPI path for native apps
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
        description = "Search the active app for widgets whose name or role contains query (case-insensitive). Returns each match's role, text, and bounding box — feed those coordinates into mouse_click/mouse_move. Use this when you know what you're looking for ('Submit', 'button', 'password'); use accessibility_tree instead when you need to explore structure first.",
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
                // CDP path for Chromium/Electron apps — query all non-chrome:// pages
                if let Ok(pages) = browser.pages().await {
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
                    for page in &pages {
                        let url = page.url().await.ok().flatten().unwrap_or_default();
                        if url.starts_with("chrome://") || url.starts_with("chrome-extension://") {
                            continue;
                        }
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
                let atspi_result = tokio::time::timeout(ATSPI_TRAVERSAL_TIMEOUT, async {
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
        description = "Move the cursor to (x,y) and click. Coordinates are pixels relative to the active window's top-left — the same frame returned by find_ui_elements and accessibility_tree, no manual offset needed. button defaults to left (use right for context menus, middle rarely). double=true for file-manager-style open, triple=true to select a whole paragraph. No need to call mouse_move first; the click already positions the cursor."
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
                tokio::time::sleep(INPUT_EVENT_DELAY).await;
            }
            sess.eis.button(code, true).map_err(KwinError::from)?;
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
            sess.eis.button(code, false).map_err(KwinError::from)?;
        }
        Ok(structured_result(&peer, format!("clicked ({x},{y}) x{count}"), serde_json::json!({
            "action": "click", "x": x, "y": y, "count": count,
        })).await)
    }

    #[rmcp::tool(
        name = "mouse_move",
        description = "Move the cursor to (x,y) in window-relative pixels without clicking. Use only when you need to trigger a hover effect (tooltip, CSS :hover, menu reveal). For clicks, call mouse_click directly — it already moves the cursor.",
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
        description = "Scroll at (x,y) in window-relative pixels — the cursor moves there first, then a wheel event fires. delta is signed: positive = down (or right, with horizontal=true); negative = up/left. Default is smooth scroll (per-pixel, good for documents and web); set discrete=true for notch-style single clicks (better for lists, dropdowns, sliders). Choose (x,y) inside the element you want to scroll, not just anywhere in the window."
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
            let d = f32::from(i16::try_from(delta).map_err(KwinError::from)?) * SCROLL_SMOOTH_PIXELS_PER_TICK;
            let (dx, dy) = if horiz { (d, 0.0) } else { (0.0, d) };
            sess.eis.scroll_smooth(dx, dy).map_err(KwinError::from)?;
        }
        Ok(structured_result(&peer, format!("scrolled {delta} at ({x},{y})"), serde_json::json!({
            "action": "scroll", "x": x, "y": y, "delta": delta,
        })).await)
    }

    #[rmcp::tool(
        name = "mouse_drag",
        description = "Press button at (from_x, from_y), smoothly move to (to_x, to_y), release. Use for text selection, window dragging, drag-and-drop, and slider adjustments — a plain mouse_click followed by mouse_move will NOT trigger drag handlers, because the button is already released by then. button defaults to left. All coords are window-relative pixels."
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
        for step in 1..=DRAG_STEPS {
            let cx = f32::from(i16::try_from(wx + from_x + (to_x - from_x) * step / DRAG_STEPS).map_err(KwinError::from)?);
            let cy = f32::from(i16::try_from(wy + from_y + (to_y - from_y) * step / DRAG_STEPS).map_err(KwinError::from)?);
            sess.eis.move_abs(cx, cy).map_err(KwinError::from)?;
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
        }
        sess.eis.button(code, false).map_err(KwinError::from)?;
        Ok(structured_result(&peer, format!("dragged ({from_x},{from_y})->({to_x},{to_y})"), serde_json::json!({
            "action": "drag", "from_x": from_x, "from_y": from_y, "to_x": to_x, "to_y": to_y,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_type",
        description = "Type printable ASCII (letters, digits, standard punctuation, space, tab, newline) into whatever currently has keyboard focus. Click or Tab into the target field first — this tool never focuses anything. For key combinations (Ctrl+A, Return, Escape, arrows, function keys) use keyboard_key instead. Non-ASCII chars are not supported and will error."
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
            if needs_shift { sess.eis.key(LINUX_KEY_LEFTSHIFT, true).map_err(KwinError::from)?; }
            sess.eis.key(code, true).map_err(KwinError::from)?;
            sess.eis.key(code, false).map_err(KwinError::from)?;
            if needs_shift { sess.eis.key(LINUX_KEY_LEFTSHIFT, false).map_err(KwinError::from)?; }
        }
        Ok(structured_result(&peer, format!("typed: {}", params.text), serde_json::json!({
            "action": "type", "text": params.text,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_key",
        description = "Press a single key or modifier combo — sent to whatever has focus. Syntax: bare names for standalone keys ('Return', 'Escape', 'Tab', 'Backspace', 'Delete', arrow keys, F1-F12, Home/End/PageUp/PageDown) or 'mod+mod+key' for combos ('ctrl+c', 'alt+F4', 'shift+Tab', 'ctrl+shift+t'). Use keyboard_type for literal text input instead."
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
        tokio::time::sleep(INPUT_EVENT_DELAY).await;
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
        description = "Launch a program inside the container by shell command (e.g. 'chromium https://example.com', 'kate /tmp/file.txt', 'konsole'). Blocks up to ~15s for a new window and returns its ID. Chromium-family apps (chromium, brave, vivaldi, electron, VS Code) get CDP auto-wired for DOM-based element discovery; Google Chrome and Edge block CDP on the default profile, so use chromium when you need CDP. The launched app inherits the container's isolated HOME — any writes it makes are ephemeral unless session_start was called with writable=true."
    )]
    async fn launch_app(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<LaunchAppParams>,
    ) -> Result<CallToolResult, McpError> {
        use std::io::Write;
        use futures::StreamExt;

        // Detect Chromium/Electron apps that support CDP on the default profile
        // Google Chrome and Edge block CDP without --user-data-dir, so skip them
        let cmd_lower = params.command.to_lowercase();
        let cmd_chromium = if cmd_lower.contains("google-chrome") || cmd_lower.contains("edge") {
            false
        } else {
            cmd_lower.contains("chromium") || cmd_lower.contains("electron")
                || cmd_lower.contains("code") || cmd_lower.contains("brave")
                || cmd_lower.contains("vivaldi")
        };

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

        let launch_cmd = match cdp_port {
            Some(port) => format!("{} --remote-debugging-port={port}", params.command),
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
        for _ in 0..LAUNCH_WINDOW_POLLS {
            tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;
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
            for _ in 0..CDP_CONNECT_POLLS {
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
                        tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;
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
