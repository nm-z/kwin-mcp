mod fuse_bridge;
mod input_bridge;
mod wallet_mediator;

use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::deserialize_number_from_string;
use std::path::{Path, PathBuf};
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

// Hard wall-clock limit for the entire session_start tool — abort if exceeded.
const SESSION_START_HARD_TIMEOUT: Duration = Duration::from_secs(20);

// Leave time for sandbox startup after host-live SQLite snapshots. A locked
// database must not keep the host scan running past session_start's limit.
const SQLITE_SNAPSHOT_STARTUP_RESERVE: Duration = Duration::from_secs(5);
const SQLITE_SNAPSHOT_PER_DATABASE_TIMEOUT: Duration = Duration::from_secs(2);
const SQLITE_SNAPSHOT_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const SQLITE_SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(25);
const SQLITE_SNAPSHOT_PAGES_PER_STEP: i32 = 1024;

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

// Settle time between cursor move and button press in mouse_click.
const MOVE_TO_CLICK_DELAY: Duration = Duration::from_millis(200);

// Mouse drag interpolation step count.
const DRAG_STEPS: i32 = 20;

// Pixels per smooth-scroll tick.
const SCROLL_SMOOTH_PIXELS_PER_TICK: f32 = 15.0;

// launch_app: window-appear polling.
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(200);
const LAUNCH_WINDOW_POLLS: u32 = 75;  // 15s total

// KWin can close a capture pipe before the advertised frame is complete while
// several isolated sessions capture concurrently. Retry only that short-read
// boundary instead of retrying arbitrary D-Bus or compositor failures.
const SCREENSHOT_CAPTURE_ATTEMPTS: u32 = 3;

// A screenshot shortly after input waits for the app to handle it and repaint:
// it polls frames until the screen has been unchanged for SCREENSHOT_SETTLE_QUIET,
// giving up SCREENSHOT_SETTLE_LIMIT after the last input event.
const SCREENSHOT_SETTLE_QUIET: Duration = Duration::from_millis(200);
const SCREENSHOT_SETTLE_POLL: Duration = Duration::from_millis(40);
const SCREENSHOT_SETTLE_LIMIT: Duration = Duration::from_millis(1500);

// launch_app: CDP connect retry.
const CDP_CONNECT_POLLS: u32 = 25;    // 5s total (reuses LAUNCH_POLL_INTERVAL)

// screenshot cursor=true: half-edge of crop region around cursor (output covers
// CURSOR_ZOOM_HALF_EDGE*2 source pixels). Render size / source pixels = zoom factor.
const CURSOR_ZOOM_HALF_EDGE: i32 = 200;

// ── Virtual-session display & font settings ──────────────────────────────

// Compiled-in defaults. Overridable at server launch via --width/--height CLI
// flags, and per-session via session_start's optional width/height params
// (unless the server was launched with --no-override, which pins the CLI/
// compiled size and silently ignores tool params).
const VIRTUAL_SCREEN_WIDTH: u32 = 3840;
const VIRTUAL_SCREEN_HEIGHT: u32 = 2160;
// Sanity bounds for any requested dimension. Max matches the viewer's
// PipeWire format-pod range ceiling (kwin-viewer.rs build_format_pod).
const MIN_SCREEN_DIM: u32 = 240;
const MAX_SCREEN_DIM: u32 = 8192;

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

use keyboard_codes::Platform;

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

/// Linux evdev codes for the modifiers a combo may name.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;

const COMBO_SYNTAX: &str = "use modifier+key, e.g. ctrl+shift+t, ctrl+minus, alt+F4; modifiers: ctrl, shift, alt, super; keys: a single character, or a name such as Return, Escape, Tab, Space, Backspace, Delete, Home, End, PageUp, PageDown, Up, Down, Left, Right, F1-F12, NumLock, minus, plus, equal, comma, period, slash, backslash, semicolon, apostrophe, grave, bracketleft, bracketright";

fn modifier_code(token: &str) -> Option<u32> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctl" => Some(KEY_LEFTCTRL),
        "shift" => Some(LINUX_KEY_LEFTSHIFT),
        "alt" | "option" => Some(KEY_LEFTALT),
        "super" | "meta" | "win" | "windows" | "cmd" | "command" | "logo" => Some(KEY_LEFTMETA),
        _ => None,
    }
}

/// Resolve one non-modifier key token to its evdev code and whether it needs
/// Shift, or None when the token names no key.
fn named_key(token: &str) -> Option<(u32, bool)> {
    let named = match token.to_ascii_lowercase().as_str() {
        "return" | "enter" => Some(28_u32),    // KEY_ENTER
        "backspace" => Some(14),               // KEY_BACKSPACE
        "tab" => Some(15),                     // KEY_TAB
        "escape" | "esc" => Some(1),           // KEY_ESC
        "space" => Some(57),                   // KEY_SPACE
        "delete" | "del" => Some(111),         // KEY_DELETE
        "insert" | "ins" => Some(110),         // KEY_INSERT
        "home" => Some(102),                   // KEY_HOME
        "end" => Some(107),                    // KEY_END
        "pageup" | "page_up" | "pgup" => Some(104), // KEY_PAGEUP
        "pagedown" | "page_down" | "pgdn" => Some(109), // KEY_PAGEDOWN
        "up" => Some(103),                     // KEY_UP
        "down" => Some(108),                   // KEY_DOWN
        "left" => Some(105),                   // KEY_LEFT
        "right" => Some(106),                  // KEY_RIGHT
        "numlock" | "num_lock" => Some(69),    // KEY_NUMLOCK
        "capslock" | "caps_lock" => Some(58),  // KEY_CAPSLOCK
        "menu" => Some(127),                   // KEY_COMPOSE
        "print" | "printscreen" => Some(99),   // KEY_SYSRQ
        "minus" | "hyphen" | "dash" => Some(12), // KEY_MINUS
        "equal" | "equals" => Some(13),        // KEY_EQUAL
        "comma" => Some(51),                   // KEY_COMMA
        "period" | "dot" => Some(52),          // KEY_DOT
        "slash" => Some(53),                   // KEY_SLASH
        "backslash" => Some(43),               // KEY_BACKSLASH
        "semicolon" => Some(39),               // KEY_SEMICOLON
        "apostrophe" | "quote" => Some(40),    // KEY_APOSTROPHE
        "grave" | "backtick" => Some(41),      // KEY_GRAVE
        "bracketleft" | "leftbracket" => Some(26), // KEY_LEFTBRACE
        "bracketright" | "rightbracket" => Some(27), // KEY_RIGHTBRACE
        "f1" => Some(59), "f2" => Some(60), "f3" => Some(61), "f4" => Some(62),
        "f5" => Some(63), "f6" => Some(64), "f7" => Some(65), "f8" => Some(66),
        "f9" => Some(67), "f10" => Some(68), "f11" => Some(87), "f12" => Some(88),
        _ => None,
    };
    if let Some(code) = named {
        return Some((code, false));
    }
    if token.eq_ignore_ascii_case("plus") {
        return Some((13, true)); // Shift+KEY_EQUAL
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        // A bare letter in a combo names the key, not an uppercase character.
        (Some(ch), None) => char_key(ch.to_ascii_lowercase()).ok(),
        _ => None,
    }
}

/// Parse a key or combo into modifier codes plus one main key. Every token
/// must resolve; an unparseable combo fails before any input is sent.
fn parse_combo(key: &str) -> Result<(Vec<u32>, u32), McpError> {
    let invalid = |detail: String| McpError::invalid_params(format!("{detail} in key combo '{key}'; {COMBO_SYNTAX}"), None);
    if key.is_empty() {
        return Err(invalid("empty key".to_owned()));
    }
    // A single character (including '+') is always that key.
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let (code, shifted) = char_key(ch).map_err(|_| invalid(format!("unsupported key '{ch}'")))?;
        return Ok((if shifted { vec![LINUX_KEY_LEFTSHIFT] } else { Vec::new() }, code));
    }
    // "ctrl++" names the '+' key: a trailing empty token after '+'.
    let mut tokens: Vec<&str> = key.split('+').collect();
    if key.ends_with("++") {
        tokens.truncate(tokens.len().saturating_sub(2));
        tokens.push("+");
    }
    let Some((last, prefix)) = tokens.split_last() else {
        return Err(invalid("empty key".to_owned()));
    };
    let mut mods = Vec::new();
    for token in prefix {
        let token = token.trim();
        let code = modifier_code(token).ok_or_else(|| invalid(format!("'{token}' is not a modifier")))?;
        if !mods.contains(&code) {
            mods.push(code);
        }
    }
    let last = last.trim();
    if last.is_empty() {
        return Err(invalid("missing key after '+'".to_owned()));
    }
    if let Some(code) = modifier_code(last) {
        // A lone modifier, or a chord of modifiers, presses the last one.
        return Ok((mods, code));
    }
    let (code, shifted) = named_key(last).ok_or_else(|| invalid(format!("unknown key '{last}'")))?;
    if shifted && !mods.contains(&LINUX_KEY_LEFTSHIFT) {
        mods.push(LINUX_KEY_LEFTSHIFT);
    }
    Ok((mods, code))
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

    #[zbus(name = "CaptureScreen")]
    fn capture_screen(
        &self,
        name: &str,
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
    serial: std::sync::atomic::AtomicU32,
    start: std::time::Instant,
}

impl Eis {
    fn next_serial(&self) -> u32 {
        self.serial
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }
    fn now_us(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_micros()).unwrap_or(0)
    }

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
            serial: std::sync::atomic::AtomicU32::new(serial),
            start: std::time::Instant::now(),
        })
    }

    fn move_abs(&self, x: f32, y: f32) -> anyhow::Result<()> {
        self.abs_ptr.motion_absolute(x, y);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        Ok(self.context.flush()?)
    }

    fn button(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed {
            true => reis::ei::button::ButtonState::Press,
            false => reis::ei::button::ButtonState::Released,
        };
        self.btn.button(code, st);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        Ok(self.context.flush()?)
    }

    fn scroll_discrete(&self, dx: i32, dy: i32) -> anyhow::Result<()> {
        self.scroll.scroll_discrete(dx, dy);
        self.scroll.scroll_stop(0, 0, 0);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        Ok(self.context.flush()?)
    }

    fn scroll_smooth(&self, dx: f32, dy: f32) -> anyhow::Result<()> {
        self.scroll.scroll(dx, dy);
        self.scroll.scroll_stop(0, 0, 0);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        Ok(self.context.flush()?)
    }

    fn key(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed {
            true => reis::ei::keyboard::KeyState::Press,
            false => reis::ei::keyboard::KeyState::Released,
        };
        self.kbd.key(code, st);
        self.kbd_dev.frame(self.next_serial(), self.now_us());
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

async fn spawn_dbus_proxy(
    address: &str,
    socket: &Path,
    rules: &[&str],
) -> anyhow::Result<std::process::Child> {
    use std::os::unix::fs::FileTypeExt;
    let _ = std::fs::remove_file(socket);
    let mut command = std::process::Command::new("xdg-dbus-proxy");
    terminate_with_parent(&mut command);
    command.arg(address).arg(socket).arg("--filter").args(rules);
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    let deadline = std::time::Instant::now() + DBUS_PROXY_TIMEOUT;
    while !std::fs::metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("D-Bus proxy socket did not appear at {}", socket.display());
        }
        tokio::time::sleep(DBUS_PROXY_POLL).await;
    }
    Ok(child)
}

fn terminate_with_parent(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)
                .map_err(std::io::Error::other)
        });
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

// ── Mount-aware overlay ──────────────────────────────────────────────────

const HOST_SOCKET_ROOT: &str = "/run/kwin-mcp-host-sockets";

fn mount_descendants(mounts: &[procfs::process::MountInfo], path: &Path) -> Vec<PathBuf> {
    let mut descendants: Vec<PathBuf> = mounts.iter()
        .filter(|mount| mount.mount_point != path && mount.mount_point.starts_with(path))
        .map(|mount| mount.mount_point.clone()).collect();
    descendants.sort_by(|left, right| left.components().count().cmp(&right.components().count()).then_with(|| left.cmp(right)));
    descendants.dedup();
    descendants
}

struct OverlayMount {
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    destination: PathBuf,
}

#[derive(Default)]
struct SocketLinks(Vec<(PathBuf, PathBuf)>);
impl Drop for SocketLinks {
    fn drop(&mut self) { for (path, target) in &self.0 { if std::fs::read_link(path).is_ok_and(|link| link == *target) { let _ = std::fs::remove_file(path); } } } }
struct OverlayPlan {
    staging_root: Option<PathBuf>,
    overlays: Vec<OverlayMount>,
    read_only_binds: Vec<PathBuf>,
    socket_binds: Vec<PathBuf>, socket_links: SocketLinks,
}

impl OverlayPlan {
    /// Overlay upper-layer path that shadows `path` (under `target`), or None
    /// when the path is not under a writable overlay (read-only bind or a
    /// file copied into the split-overlay staging root).
    fn upper_path(&self, path: &Path) -> Option<(PathBuf, &OverlayMount)> {
        if self.read_only_binds.iter().any(|mount| path.starts_with(mount)) {
            return None;
        }
        let overlay = self.overlays.iter()
            .filter(|overlay| path.starts_with(&overlay.destination))
            .max_by_key(|overlay| overlay.destination.components().count())?;
        Some((overlay.upper.join(path.strip_prefix(&overlay.destination).ok()?), overlay))
    }

    fn add_bwrap_args(&self, command: &mut std::process::Command, target: &Path) {
        command.args(["--ro-bind", "/", "/", "--tmpfs", "/run"]);
        if let Some(staging_root) = &self.staging_root {
            command.arg("--bind").arg(staging_root).arg(target);
        }
        for overlay in &self.overlays {
            command
                .arg("--overlay-src")
                .arg(&overlay.lower)
                .arg("--overlay")
                .arg(&overlay.upper)
                .arg(&overlay.work)
                .arg(&overlay.destination);
        }
        for path in &self.read_only_binds {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        for (index, source) in self.socket_binds.iter().enumerate() {
            let destination = PathBuf::from(format!("{HOST_SOCKET_ROOT}/{index}"));
            command.arg("--dir").arg(&destination).arg("--ro-bind").arg(source).arg(destination);
        }
    }

    fn expose_sockets(
        &mut self,
        target: &Path,
        host_runtime: &Path,
        isolated_runtime: &Path,
    ) -> anyhow::Result<()> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let canonical_target = std::fs::canonicalize(target)?;
        let canonical_runtime = std::fs::canonicalize(host_runtime)?;
        let current_uid = procfs::process::Process::myself()?.uid()?;
        let graphical_inodes = graphical_socket_inodes()?;
        let mut sockets: Vec<(PathBuf, PathBuf, PathBuf)> = procfs::net::unix()?.into_iter().filter_map(|entry| {
            if entry.state != procfs::net::UnixState::UNCONNECTED { return None; }
            let path = entry.path?; let parent = std::fs::canonicalize(path.parent()?).ok()?;
            let metadata = std::fs::metadata(&path).ok()?;
            if metadata.uid() != current_uid || !metadata.file_type().is_socket()
                || graphical_inodes.contains(&entry.inode) { return None; }
            if path.starts_with(target) && parent.starts_with(&canonical_target) {
                Some((path.clone(), parent, path))
            } else if path.starts_with(host_runtime) && parent.starts_with(&canonical_runtime) {
                Some((path.clone(), parent, isolated_runtime.join(path.strip_prefix(host_runtime).ok()?)))
            } else {
                None
            }
        }).filter(|(path, _, _)| !self.read_only_binds.iter().any(|mount| path != mount && path.starts_with(mount))).collect();
        sockets.sort(); sockets.dedup();
        for (path, parent, destination) in sockets {
            self.read_only_binds.retain(|mount| mount != &path);
            let index = self.socket_binds.iter().position(|bind| bind == &parent).unwrap_or_else(|| { let index = self.socket_binds.len(); self.socket_binds.push(parent); index });
            let link = if path.starts_with(target) {
                let mount = self.overlays.iter().filter(|overlay| path.starts_with(&overlay.destination)).max_by_key(|overlay| overlay.destination.components().count());
                if let Some(overlay) = mount { overlay.upper.join(path.strip_prefix(&overlay.destination)?) }
                    else { self.staging_root.as_ref().ok_or_else(|| anyhow::anyhow!("no overlay for {}", path.display()))?.join(path.strip_prefix(target)?) }
            } else {
                destination
            };
            std::fs::create_dir_all(link.parent().ok_or_else(|| anyhow::anyhow!("no parent for {}", link.display()))?)?;
            let generated = PathBuf::from(format!("{HOST_SOCKET_ROOT}/{index}")).join(path.file_name().ok_or_else(|| anyhow::anyhow!("unnamed socket {}", path.display()))?);
            match std::fs::symlink_metadata(&link) {
                Ok(_) => anyhow::bail!("refusing to replace {}", link.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            std::os::unix::fs::symlink(&generated, &link)?;
            self.socket_links.0.push((link, generated));
        }
        Ok(())
    }
}

fn graphical_socket_inodes() -> anyhow::Result<std::collections::HashSet<u64>> {
    let host_display = std::env::var_os("DISPLAY");
    let host_wayland = std::env::var_os("WAYLAND_DISPLAY");
    let mut graphical = std::collections::HashSet::new();
    for process in procfs::process::all_processes()?.flatten() {
        let environment = process.environ().unwrap_or_default();
        let cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", process.pid)).unwrap_or_default();
        let display_attached = host_display.as_ref().is_some_and(|value| environment.get(std::ffi::OsStr::new("DISPLAY")) == Some(value))
            || host_wayland.as_ref().is_some_and(|value| environment.get(std::ffi::OsStr::new("WAYLAND_DISPLAY")) == Some(value));
        let desktop_scope = cgroup.contains("/session.slice/")
            || (cgroup.contains("/app.slice/") && cgroup.contains(".scope"));
        let descriptors: Vec<procfs::process::FDInfo> = process.fd().into_iter().flatten().flatten().collect();
        let input_attached = descriptors.iter().any(|descriptor| {
            if let procfs::process::FDTarget::Path(path) = &descriptor.target {
                path == Path::new("/dev/uinput") || path.starts_with("/dev/input")
            } else {
                false
            }
        });
        if display_attached || desktop_scope || input_attached {
            graphical.extend(descriptors.into_iter().filter_map(|descriptor| {
                if let procfs::process::FDTarget::Socket(inode) = descriptor.target {
                    Some(inode)
                } else {
                    None
                }
            }));
        }
    }
    Ok(graphical)
}

fn create_staging_directory(
    source: &Path,
    destination: &Path,
    initialize: bool,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)?;
    if initialize {
        std::fs::set_permissions(destination, std::fs::metadata(source)?.permissions())?;
    }
    Ok(())
}

struct SplitOverlayContext<'a> {
    target: &'a Path,
    staging_root: &'a Path,
    upper_root: &'a Path,
    work_root: &'a Path,
    mounts: &'a [procfs::process::MountInfo],
    initialize: bool,
}

fn prepare_split_overlay_directory(
    source_directory: &Path,
    context: &SplitOverlayContext<'_>,
    overlays: &mut Vec<OverlayMount>,
    read_only_binds: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let relative_directory = source_directory.strip_prefix(context.target)?;
    let staged_directory = context.staging_root.join(relative_directory);
    create_staging_directory(source_directory, &staged_directory, context.initialize)?;

    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(source_directory)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let source = entry.path();
        let relative = source.strip_prefix(context.target)?;
        let staged = context.staging_root.join(relative);
        let file_type = entry.file_type()?;
        let staged_exists = std::fs::symlink_metadata(&staged).is_ok();

        if context.mounts.iter().any(|mount| mount.mount_point == source) {
            // Only a stub for bwrap's bind: the mount shadows its mode. Never
            // stat the mount itself; a hung FUSE or network mount would block
            // session_start in the kernel.
            if file_type.is_dir() {
                std::fs::create_dir_all(&staged)?;
            }
            continue;
        }

        if file_type.is_dir() {
            if context.mounts.iter().any(|mount| mount.mount_point != source && mount.mount_point.starts_with(&source)) {
                prepare_split_overlay_directory(&source, context, overlays, read_only_binds)?;
            } else {
                create_staging_directory(&source, &staged, context.initialize)?;
                let upper = context.upper_root.join(relative);
                let work = context.work_root.join(relative);
                std::fs::create_dir_all(&upper)?;
                std::fs::create_dir_all(&work)?;
                overlays.push(OverlayMount {
                    lower: source.clone(),
                    upper,
                    work,
                    destination: source,
                });
            }
            continue;
        }

        if file_type.is_symlink() {
            if context.initialize && !staged_exists {
                std::os::unix::fs::symlink(std::fs::read_link(&source)?, &staged)?;
            }
        } else if file_type.is_file() {
            if context.initialize && !staged_exists {
                match std::fs::copy(&source, &staged) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                        let _ = std::fs::remove_file(&staged);
                        read_only_binds.push(source);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            read_only_binds.push(source);
        }
    }
    Ok(())
}

fn prepare_overlay_plan(
    target: &Path,
    session_tmp: &Path,
    mounts: &[procfs::process::MountInfo],
) -> anyhow::Result<OverlayPlan> {
    let excluded_mounts = mount_descendants(mounts, target);
    if excluded_mounts.is_empty() {
        let upper = session_tmp.join("overlay-upper");
        let work = session_tmp.join("overlay-work");
        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&work)?;
        let overlay = OverlayMount { lower: target.to_path_buf(), upper, work, destination: target.to_path_buf() };
        return Ok(OverlayPlan { staging_root: None, overlays: vec![overlay], read_only_binds: Vec::new(),
            socket_binds: Vec::new(), socket_links: SocketLinks::default() });
    }

    let staging_root = session_tmp.join("overlay-root");
    let initialized_marker = session_tmp.join("overlay-root.initialized");
    let initialize = !initialized_marker.exists();
    let upper_root = session_tmp.join("overlay-upper");
    let work_root = session_tmp.join("overlay-work");
    std::fs::create_dir_all(&upper_root)?;
    std::fs::create_dir_all(&work_root)?;

    let context = SplitOverlayContext {
        target,
        staging_root: &staging_root,
        upper_root: &upper_root,
        work_root: &work_root,
        mounts,
        initialize,
    };
    let mut overlays = Vec::new();
    let mut read_only_binds = excluded_mounts;
    prepare_split_overlay_directory(target, &context, &mut overlays, &mut read_only_binds)?;
    read_only_binds.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    read_only_binds.dedup();
    if initialize {
        std::fs::write(&initialized_marker, b"split-overlay-v1\n")?;
    }

    Ok(OverlayPlan {
        staging_root: Some(staging_root),
        overlays,
        read_only_binds,
        socket_binds: Vec::new(), socket_links: SocketLinks::default(),
    })
}

/// Blocking host scan still owned by an in-flight session_start.
type HostWork = Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;

/// SQLite databases a host process is writing in WAL mode under `target`,
/// found by their open `-wal` files. Largest-first order is not needed; the
/// list is sorted for stable logs.
fn live_sqlite_databases(target: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let Ok(canonical_target) = std::fs::canonicalize(target) else { return Vec::new() };
    let uid = std::fs::metadata("/proc/self").map(|meta| meta.uid()).unwrap_or(u32::MAX);
    let mut databases = Vec::new();
    for process in procfs::process::all_processes().into_iter().flatten().flatten() {
        if process.uid().ok() != Some(uid) {
            continue;
        }
        for descriptor in process.fd().into_iter().flatten().flatten() {
            let procfs::process::FDTarget::Path(path) = descriptor.target else { continue };
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue };
            let Some(database) = name.strip_suffix("-wal") else { continue };
            let Ok(relative) = path.with_file_name(database).strip_prefix(&canonical_target).map(Path::to_path_buf) else { continue };
            databases.push(target.join(relative));
        }
    }
    databases.sort();
    databases.dedup();
    databases.retain(|database| database.is_file());
    databases
}

/// Largest live database session_start snapshots; bigger ones stay shared
/// with the host (and may read as malformed while the host writes them).
const SQLITE_SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Give the session a consistent copy of a host-live SQLite WAL database.
///
/// The HOME overlay's lower layer is the live host HOME. When a host app keeps
/// writing a WAL database, the session sees the database, -wal, and -shm files
/// change underneath it (or copied up at different moments), and SQLite
/// locking does not coordinate across the overlay, so reads in the session
/// fail with "database disk image is malformed" although the host copy is
/// intact. SQLite's online backup takes a consistent snapshot through the
/// host's own locking; it lands in the upper layer with an empty WAL and
/// shared-memory index, so the session owns a self-consistent database.
fn snapshot_live_sqlite(
    plan: &OverlayPlan,
    database: &Path,
    startup_deadline: std::time::Instant,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use rusqlite::backup::StepResult;
    let deadline = std::cmp::min(
        startup_deadline,
        std::time::Instant::now() + SQLITE_SNAPSHOT_PER_DATABASE_TIMEOUT,
    );
    anyhow::ensure!(std::time::Instant::now() < deadline, "snapshot deadline exceeded");
    let (upper, overlay) = plan.upper_path(database).ok_or_else(|| anyhow::anyhow!("not under a writable overlay"))?;
    let size = std::fs::metadata(database)?.len();
    anyhow::ensure!(size <= SQLITE_SNAPSHOT_MAX_BYTES, "{size} bytes exceeds the {SQLITE_SNAPSHOT_MAX_BYTES}-byte snapshot limit");
    // Recreate missing upper parents with the lower directories' modes, since
    // a merged directory shows its upper attributes.
    let parent = upper.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;
    let mut missing = Vec::new();
    let mut cursor = parent.to_path_buf();
    while !cursor.exists() && cursor.starts_with(&overlay.upper) && cursor != overlay.upper {
        missing.push(cursor.clone());
        cursor = cursor.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?.to_path_buf();
    }
    for directory in missing.iter().rev() {
        std::fs::create_dir(directory)?;
        let lower = overlay.destination.join(directory.strip_prefix(&overlay.upper)?);
        if let Ok(meta) = std::fs::metadata(&lower) {
            std::fs::set_permissions(directory, meta.permissions())?;
        }
    }
    let temporary = upper.with_file_name(format!(
        ".{}.kwin-mcp-snapshot",
        upper.file_name().and_then(|name| name.to_str()).unwrap_or("db")
    ));
    let result = (|| -> anyhow::Result<()> {
        let source = rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        source.busy_timeout(SQLITE_SNAPSHOT_BUSY_TIMEOUT)?;
        let mut copy = rusqlite::Connection::open(&temporary)?;
        let backup = rusqlite::backup::Backup::new(&source, &mut copy)?;
        loop {
            anyhow::ensure!(std::time::Instant::now() < deadline, "snapshot deadline exceeded");
            match backup.step(SQLITE_SNAPSHOT_PAGES_PER_STEP)? {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(std::cmp::min(
                        SQLITE_SNAPSHOT_RETRY_DELAY,
                        deadline.saturating_duration_since(std::time::Instant::now()),
                    ));
                }
                _ => anyhow::bail!("unexpected SQLite backup state"),
            }
        }
        drop(backup);
        drop(copy);
        let mode = std::fs::metadata(database)?.permissions().mode();
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&temporary, &upper)?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = upper.with_file_name(format!("{}{suffix}", upper.file_name().and_then(|name| name.to_str()).unwrap_or_default()));
            std::fs::write(&sidecar, b"")?;
            std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_file(temporary.with_file_name(format!(
            "{}-journal",
            temporary.file_name().and_then(|name| name.to_str()).unwrap_or_default()
        )));
    }
    result
}

/// Host state session_start reads before it spawns the sandbox.
struct HostView {
    overlay_plan: OverlayPlan,
    kdeglobals: String,
}

/// Blocking host scan for session_start: mount inventory, HOME overlay plan,
/// host socket exposure, and the host kdeglobals.
fn prepare_host_view(
    target: &Path,
    host_xdg_dir: &Path,
    host_runtime: &Path,
    snapshot_deadline: std::time::Instant,
) -> anyhow::Result<HostView> {
    test_block_host_scan();
    let mount_inventory = procfs::process::Process::myself()
        .and_then(|process| process.mountinfo())
        .map(|mounts| mounts.0)
        .map_err(|e| anyhow::anyhow!("read mount inventory: {e:#}"))?;
    let overlay_exclusions = mount_descendants(&mount_inventory, target);
    eprintln!(
        "session_start: mount inventory={} overlay-exclusions={}",
        mount_inventory.len(),
        overlay_exclusions.len()
    );
    for mount in &overlay_exclusions {
        eprintln!("session_start: excluding mount from overlay: {}", mount.display());
    }
    let mut overlay_plan = prepare_overlay_plan(target, &host_xdg_dir.join("tmp"), &mount_inventory)
        .map_err(|e| anyhow::anyhow!("prepare overlays: {e:#}"))?;
    overlay_plan
        .expose_sockets(target, host_runtime, host_xdg_dir)
        .map_err(|e| anyhow::anyhow!("expose host sockets: {e:#}"))?;
    eprintln!(
        "session_start: overlay plan={} overlays={} read-only-mounts={}",
        if overlay_plan.staging_root.is_some() { "split" } else { "whole" },
        overlay_plan.overlays.len(),
        overlay_plan.read_only_binds.len()
    );
    for database in live_sqlite_databases(target) {
        match snapshot_live_sqlite(&overlay_plan, &database, snapshot_deadline) {
            Ok(()) => eprintln!("session_start: snapshotted live SQLite database {}", database.display()),
            Err(error) => eprintln!("session_start: live SQLite database {} left shared: {error:#}", database.display()),
        }
    }
    let kdeglobals = std::fs::read_to_string(target.join(".config/kdeglobals")).unwrap_or_default();
    Ok(HostView { overlay_plan, kdeglobals })
}

// ── Session ──────────────────────────────────────────────────────────────

struct Session {
    kwin_conn: zbus::Connection,       // talks to KWin via its unique name
    _proxy_conn: zbus::Connection,    // owns org.kde.KWin, has InputDevice objects (kept alive)
    kwin_unique_name: String,
    service_bus_address: String,
    atspi_bus_address: String,
    eis: Eis,
    sandbox_child: std::process::Child,
    sandbox_stdin: std::process::ChildStdin,
    host_xdg_dir: std::path::PathBuf,
    _uinput_mouse: evdev::uinput::VirtualDevice,
    _uinput_keyboard: evdev::uinput::VirtualDevice,
    cdp_browser: Option<Arc<chromiumoxide::Browser>>,
    cdp_forward_port: u16,
    service_proxy_children: Vec<std::process::Child>,
    viewer_child: Option<std::process::Child>,
    /// Why no viewer is running when viewer_child is None.
    viewer_unavailable: Option<String>,
    overlay_work_paths: Vec<PathBuf>,
    _socket_links: SocketLinks,
    screen_width: u32,
    screen_height: u32,
    last_activity: std::time::Instant,
    /// When an input tool last sent events, for screenshot settling.
    last_input: Option<std::time::Instant>,
    /// Mediator between session apps and the host KWallet.
    wallet: Option<wallet_mediator::WalletMediator>,
    /// Whether the sandbox has the FUSE bridge; without it launch_app runs
    /// AppImages in extract-and-run mode.
    fuse_enabled: bool,
}

// ── Server ───────────────────────────────────────────────────────────────

/// Virtual display size policy resolved from CLI flags at server launch.
/// `width`/`height` are the session default (compiled constants unless
/// --width/--height overrode them); `locked` (--no-override) makes
/// session_start ignore its optional width/height params entirely.
#[derive(Clone, Copy)]
struct DisplayConfig {
    width: u32,
    height: u32,
    locked: bool,
    viewer_enabled: bool,
    autoclean: bool,
    ttl: Option<Duration>,
}

const STARTUP_CHILD_TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn signal_child(
    child: &std::process::Child,
    process_group: bool,
    signal: nix::sys::signal::Signal,
) {
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    let target = if process_group { -pid } else { pid };
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(target), signal);
}

/// Terminate a startup or session child without allowing a TERM-resistant
/// process to hold the lifecycle gate forever. The final wait reaps the child
/// after the bounded TERM grace period and SIGKILL escalation.
fn terminate_child(mut child: std::process::Child, process_group: bool, label: &str) {
    signal_child(&child, process_group, nix::sys::signal::Signal::SIGTERM);
    let deadline = std::time::Instant::now() + STARTUP_CHILD_TERMINATION_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) | Err(_) => break,
        }
    }
    eprintln!("{label} did not exit after SIGTERM; escalating to SIGKILL");
    signal_child(&child, process_group, nix::sys::signal::Signal::SIGKILL);
    let deadline = std::time::Instant::now() + STARTUP_CHILD_TERMINATION_GRACE;
    while std::time::Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // SIGKILL is pending, but the child is blocked in the kernel (D state).
    // Reap it off the lifecycle path so stop and start stay bounded.
    eprintln!("{label} still blocked after SIGKILL; reaping in the background");
    let _ = std::thread::Builder::new()
        .name("kwin-mcp-reaper".to_owned())
        .spawn(move || { let _ = child.wait(); });
}

/// Resources created while session_start is still unpublished. The guard owns
/// them until the Session takes them, so cancelling the startup future cannot
/// detach a process or task that has no reachable cleanup path.
struct StartupResources {
    sandbox_child: Option<std::process::Child>,
    sandbox_stdin: Option<std::process::ChildStdin>,
    service_proxy_children: Vec<std::process::Child>,
}

impl StartupResources {
    fn new() -> Self {
        Self {
            sandbox_child: None,
            sandbox_stdin: None,
            service_proxy_children: Vec::new(),
        }
    }

    fn add_proxy(&mut self, child: std::process::Child) {
        self.service_proxy_children.push(child);
    }

    fn cleanup(&mut self) {
        drop(self.sandbox_stdin.take());
        if let Some(sandbox) = self.sandbox_child.take() {
            terminate_child(sandbox, true, "startup sandbox");
        }
        for proxy in self.service_proxy_children.drain(..) {
            terminate_child(proxy, false, "startup D-Bus proxy");
        }
    }

    fn into_parts(mut self) -> (std::process::Child, std::process::ChildStdin, Vec<std::process::Child>) {
        let Some(sandbox_child) = self.sandbox_child.take() else {
            panic!("startup sandbox child missing at Session publication");
        };
        let Some(sandbox_stdin) = self.sandbox_stdin.take() else {
            panic!("startup sandbox stdin missing at Session publication");
        };
        (
            sandbox_child,
            sandbox_stdin,
            std::mem::take(&mut self.service_proxy_children),
        )
    }
}

impl Drop for StartupResources {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Own a spawned viewer until the Session is published. A cancelled startup
/// drops this guard and reaps the viewer before removing the workdir.
struct StartupViewerResources {
    viewer: Option<std::process::Child>,
}

impl StartupViewerResources {
    fn new() -> Self {
        Self { viewer: None }
    }

    fn into_child(mut self) -> Option<std::process::Child> {
        self.viewer.take()
    }
}

impl Drop for StartupViewerResources {
    fn drop(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            terminate_child(viewer, false, "startup viewer");
        }
    }
}

/// Debug-only pause used by the production-stdio cancellation regression. It
/// creates a real cancellation point after a selected startup resource exists,
/// allowing the test to prove the owning guards clean it before workdir removal.
async fn test_startup_delay(stage: &str) {
    if !cfg!(debug_assertions)
        || std::env::var("KWIN_MCP_TEST_STARTUP_DELAY_STAGE").ok().as_deref() != Some(stage)
    {
        return;
    }
    let millis = std::env::var("KWIN_MCP_TEST_STARTUP_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000);
    if millis == 0 {
        return;
    }
    eprintln!("session_start: test delay at {stage} for {millis}ms");
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
}

/// Debug-only stand-in for a host scan blocked in the kernel, such as a stat
/// on a hung FUSE mount: a thread sleep that no async timeout can preempt.
fn test_block_host_scan() {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(millis) = std::env::var("KWIN_MCP_TEST_BLOCK_HOST_SCAN_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    eprintln!("session_start: test host scan blocked for {millis}ms");
    std::thread::sleep(std::time::Duration::from_millis(millis));
}

fn test_stop_bwrap(child: &std::process::Child) {
    if !cfg!(debug_assertions)
        || std::env::var("KWIN_MCP_TEST_STOP_BWRAP").ok().as_deref() != Some("1")
    {
        return;
    }
    signal_child(child, false, nix::sys::signal::Signal::SIGSTOP);
    eprintln!("session_start: test stopped bwrap pid={}", child.id());
}

#[derive(Clone)]
struct KwinMcp {
    session: Arc<tokio::sync::Mutex<Option<Session>>>,
    /// Cleanup ownership of the session workdir under --autoclean. Claimed
    /// before session_start creates the directory and released only once the
    /// directory is gone, so every terminal start, stop, and shutdown outcome
    /// either removes it or leaves it owned with a reachable retry.
    workdir: Arc<WorkdirOwnership>,
    /// Held for the whole of session_start, which is the only writer that
    /// populates the workdir. Shutdown takes it before its terminal cleanup, so
    /// a start still running when the transport closes cannot repopulate a
    /// directory that nothing is left to delete.
    start_gate: Arc<tokio::sync::Mutex<()>>,
    /// Last startup checkpoint reached, named in the hard-limit error.
    start_stage: Arc<std::sync::Mutex<&'static str>>,
    display: DisplayConfig,
}

impl KwinMcp {
    fn new(display: DisplayConfig) -> Self {
        Self {
            session: Arc::new(tokio::sync::Mutex::new(None)),
            workdir: Arc::new(WorkdirOwnership::default()),
            start_gate: Arc::new(tokio::sync::Mutex::new(())),
            start_stage: Arc::new(std::sync::Mutex::new("waiting for the lifecycle gate")),
            display,
        }
    }
    fn set_start_stage(&self, stage: &'static str) {
        eprintln!("session_start: stage: {stage}");
        if let Ok(mut current) = self.start_stage.lock() {
            *current = stage;
        }
    }
    fn start_stage(&self) -> &'static str {
        self.start_stage.lock().map(|stage| *stage).unwrap_or("unknown")
    }
    /// Wait for the lifecycle gate, bounded so a start, stop, or reaper stuck
    /// behind a blocked host call answers with an error instead of hanging.
    async fn lifecycle_gate(
        &self,
        deadline: tokio::time::Instant,
        operation: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, McpError> {
        tokio::time::timeout_at(deadline, self.start_gate.clone().lock_owned())
            .await
            .map_err(|_| {
                McpError::internal_error(
                    format!(
                        "{operation} could not start: another session lifecycle operation is still running (last startup stage: {}). Retry shortly.",
                        self.start_stage()
                    ),
                    Some(serde_json::json!({"reason": "lifecycle_busy", "stage": self.start_stage()})),
                )
            })
    }
    async fn mark_input(&self) {
        if let Some(session) = self.session.lock().await.as_mut() {
            session.last_input = Some(std::time::Instant::now());
        }
    }
    async fn last_input(&self) -> Option<std::time::Instant> {
        self.session.lock().await.as_ref().and_then(|session| session.last_input)
    }
    async fn touch_activity(&self) {
        if let Some(session) = self.session.lock().await.as_mut() {
            session.last_activity = std::time::Instant::now();
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
    /// Terminal cleanup for a session_start attempt that published no Session,
    /// covering the failed paths, the cancelled hard-timeout path, and a start
    /// that never created its directory. A start that did publish a Session
    /// keeps its workdir, which session_stop then owns.
    async fn autoclean_unpublished_workdir(&self, error: McpError) -> McpError {
        if self.session.lock().await.is_some() {
            return error;
        }
        match self.workdir.remove() {
            WorkdirCleanup::NothingOwned => error,
            WorkdirCleanup::Removed(dir) => {
                eprintln!("session_start: autoclean removed {}", dir.display());
                error
            }
            WorkdirCleanup::Retained { dir, error: remove_error } => McpError::internal_error(
                format!(
                    "{}; session workdir {} not removed: {remove_error}. Cleanup is still owned, call session_stop to retry.",
                    error.message,
                    dir.display()
                ),
                None,
            ),
        }
    }
    /// Final terminal transition for transport close or a handled signal.
    async fn shutdown_cleanup(&self) {
        let gate = self.lifecycle_gate(tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT, "shutdown cleanup").await;
        if let Err(error) = &gate {
            eprintln!("shutdown: {}; cleaning up without it", error.message);
        }
        if let Some(dir) = self.workdir.owned() {
            eprintln!("shutdown: autoclean owns {}", dir.display());
        }
        let stopped = self.session.lock().await.take();
        if let Some(sess) = stopped {
            teardown_blocking(sess).await;
        }
        match self.workdir.remove() {
            WorkdirCleanup::NothingOwned => {}
            WorkdirCleanup::Removed(dir) => eprintln!("shutdown: autoclean removed {}", dir.display()),
            WorkdirCleanup::Retained { dir, error } => {
                eprintln!("shutdown: autoclean could not remove {}: {error}", dir.display())
            }
        }
    }
    async fn idle_reaper(self) {
        let Some(ttl) = self.display.ttl else { return };
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let idle = self.session.lock().await.as_ref().is_some_and(|s| s.last_activity.elapsed() >= ttl);
            if !idle {
                continue;
            }
            let Ok(_start_gate) = self.start_gate.try_lock() else { continue };
            let stopped = {
                let mut session = self.session.lock().await;
                if session.as_ref().is_some_and(|s| s.last_activity.elapsed() >= ttl) {
                    session.take()
                } else {
                    None
                }
            };
            if let Some(sess) = stopped {
                eprintln!("ttl: session idle for {} minutes; tearing down", ttl.as_secs() / 60);
                teardown_blocking(sess).await;
                match self.workdir.remove() {
                    WorkdirCleanup::NothingOwned => {}
                    WorkdirCleanup::Removed(dir) => eprintln!("ttl: removed {}", dir.display()),
                    WorkdirCleanup::Retained { dir, error } => {
                        eprintln!("ttl: could not remove {}: {error}", dir.display())
                    }
                }
            }
        }
    }
}


struct CursorSprite {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// Hotspot position within the rasterized cursor PNG — the arrow tip in raw PNG
/// coordinates. Determined once empirically for the fixed render size (see build.rs)
/// so the runtime skips any per-call bbox scanning. If you change the SVG or the
/// rsvg-convert height, rerun the one-shot trim command (`magick cursor.png
/// -alpha extract -threshold 50% -format '%@' info:`) and update these.
const CURSOR_HOTSPOT_X: i32 = 17;
const CURSOR_HOTSPOT_Y: i32 = 10;

/// High-visibility cursor sprite rasterized from cursor_v6_fixed.svg at build time
/// (see build.rs). Lazily decoded once on first use. The tip of the arrow within
/// the returned RGBA buffer is at (CURSOR_HOTSPOT_X, CURSOR_HOTSPOT_Y). Returns
/// None only if the embedded PNG is malformed (shouldn't happen).
fn cursor_sprite() -> Option<&'static CursorSprite> {
    static CACHE: std::sync::OnceLock<Option<CursorSprite>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        const PNG_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cursor.png"));
        let decoder = png::Decoder::new(PNG_BYTES);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        if info.bit_depth != png::BitDepth::Eight { return None; }
        let raw = &buf[..info.buffer_size()];
        let rgba: Vec<u8> = match info.color_type {
            png::ColorType::Rgba => raw.to_vec(),
            png::ColorType::Rgb => {
                let mut out = Vec::with_capacity(raw.len() / 3 * 4);
                for chunk in raw.chunks_exact(3) {
                    out.extend_from_slice(chunk);
                    out.push(255);
                }
                out
            }
            png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha | png::ColorType::Indexed => return None,
        };
        Some(CursorSprite { rgba, w: info.width, h: info.height })
    }).as_ref()
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
        "service_bus_socket",
        "kwallet_bus",
        "kwallet-bus.conf",
        "dbus-ready",
        "bridge-ready",
        "screenshot.png",
        "viewer.log",
        VIEWER_STATUS_FILE,
    ];
    const STALE_DIRS: &[&str] = &[
        "at-spi",
        "browser-bin",
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

// ── Session workdir autoclean ────────────────────────────────────────────
//
// --autoclean owns exactly one directory, the session workdir of this server
// process, and moves through one explicit state machine:
//
//   Idle  --claim (session_start, before the directory is created)--> Owned
//   Owned --remove succeeded (workdir gone)--------------------------> Idle
//   Owned --remove failed (workdir still there)---------------------> Owned
//
// `claim` and `remove` are synchronous and each runs under a single lock
// acquisition, so no await, cancellation, or hard timeout can split a
// transition and strand the directory. Every terminal outcome of session_start,
// session_stop, and server shutdown ends in one of those two transitions, so an
// owned workdir always keeps a reachable retry.

/// Owner read, write, and traverse bits. A directory missing any of them cannot
/// be emptied by its owner, which is what a mode-000 directory in the overlay
/// does to the delete.
const WORKDIR_OWNER_RWX: u32 = 0o700;

/// Depth limit for the permission repair walk inside the owned workdir. Each
/// level holds one open descriptor, so the bound stops a hostile deep tree from
/// exhausting the descriptor limit. A subtree below the limit is left alone; the
/// delete then fails and stays retryable instead of the walk breaking the server.
const WORKDIR_REPAIR_MAX_DEPTH: u32 = 128;
const WORKDIR_LEASE_FILE: &str = ".kwin-mcp-autoclean";
const WORKDIR_LEASE_VERSION: &str = "kwin-mcp-autoclean-v1";

/// The one session workdir this server process owns. `session_start` creates it
/// and cleanup validates against it, so both agree on a single definition.
fn session_workdir_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kwin-mcp-{}", std::process::id()))
}

/// Path naming the inode an open descriptor already refers to. Operating through
/// it keeps chmod and directory reads pinned to the descriptor this walk opened
/// and validated, so a name replaced underneath the walk cannot redirect the
/// operation at a different file.
fn proc_fd_path(fd: &std::os::fd::OwnedFd) -> std::path::PathBuf {
    use std::os::fd::AsRawFd;
    std::path::PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Grant owner rwx to `dir` and to every directory below it so the delete cannot
/// be blocked by a mode-000 directory a launched command created in the overlay.
///
/// `dir` is a descriptor opened with O_PATH | O_NOFOLLOW, so it names the entry
/// itself and never its symlink target. The walk chmods only after `fstat` on
/// that descriptor proves it is a directory, which excludes symlinks, files, and
/// devices, and it descends only through descriptors opened from the parent
/// descriptor with the same flags. There is no path re-resolution and no
/// check-then-use window, so nothing outside the owned workdir can be reached
/// even while the tree is being rewritten concurrently.
fn repair_workdir_directory_tree(dir: std::os::fd::OwnedFd, depth: u32) {
    let Ok(status) = nix::sys::stat::fstat(&dir) else {
        return;
    };
    if status.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR {
        return;
    }
    let mode = status.st_mode & 0o7777;
    if mode & WORKDIR_OWNER_RWX != WORKDIR_OWNER_RWX {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            proc_fd_path(&dir),
            std::fs::Permissions::from_mode(mode | WORKDIR_OWNER_RWX),
        );
    }
    if depth >= WORKDIR_REPAIR_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(proc_fd_path(&dir)) else {
        return;
    };
    let flags = nix::fcntl::OFlag::O_PATH
        | nix::fcntl::OFlag::O_NOFOLLOW
        | nix::fcntl::OFlag::O_CLOEXEC;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Ok(child) = nix::fcntl::openat(&dir, name.as_os_str(), flags, nix::sys::stat::Mode::empty()) {
            repair_workdir_directory_tree(child, depth + 1);
        }
    }
}

/// Delete the owned session workdir. The path is validated against this
/// process's own workdir first, permissions inside that tree are repaired, and
/// the delete itself never follows symlinks, so every effect stays inside the
/// owned directory. An absent directory is the wanted end state, so a retry
/// after a partial delete converges instead of inventing a new failure.
fn remove_session_workdir(dir: &std::path::Path) -> std::io::Result<()> {
    let expected = session_workdir_path();
    if dir != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not this server's session workdir {}", dir.display(), expected.display()),
        ));
    }
    remove_verified_workdir(dir)
}

/// Remove a directory after the caller has proved its ownership. This helper
/// never follows a symlink while repairing nested permissions.
fn remove_verified_workdir(dir: &std::path::Path) -> std::io::Result<()> {
    let flags = nix::fcntl::OFlag::O_PATH
        | nix::fcntl::OFlag::O_NOFOLLOW
        | nix::fcntl::OFlag::O_CLOEXEC;
    if let Ok(root) = nix::fcntl::open(dir, flags, nix::sys::stat::Mode::empty()) {
        repair_workdir_directory_tree(root, 0);
    }
    if let Err(error) = std::fs::remove_dir_all(dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error);
    }
    Ok(())
}

/// A previous --autoclean server explicitly marked this workdir and held its
/// lease until exit. The marker records the sandbox process group so a crash
/// cannot cause a later server to delete a still-running container.
fn sweep_orphaned_workdirs() -> std::io::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let temp = std::env::temp_dir();
    let uid = std::fs::metadata("/proc/self")?.uid();
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    for entry in std::fs::read_dir(&temp)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid_text) = name.strip_prefix("kwin-mcp-") else { continue };
        if pid_text.is_empty() || !pid_text.bytes().all(|byte| byte.is_ascii_digit()) { continue }
        let dir = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&dir) else { continue };
        if !metadata.file_type().is_dir() || metadata.uid() != uid { continue }
        let marker_path = dir.join(WORKDIR_LEASE_FILE);
        let Ok(mut marker) = std::fs::OpenOptions::new().read(true).write(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&marker_path) else { continue };
        let Ok(marker_metadata) = marker.metadata() else { continue };
        if !marker_metadata.file_type().is_file() || marker_metadata.uid() != uid
            || marker_metadata.len() > 128 || marker.try_lock().is_err() { continue }
        let mut contents = String::new();
        if marker.read_to_string(&mut contents).is_err() { continue }
        let mut fields = contents.split_whitespace();
        let (Some(version), Some(owner), Some(group), None) =
            (fields.next(), fields.next(), fields.next(), fields.next()) else { continue };
        let Ok(group) = group.parse::<i32>() else { continue };
        if version != WORKDIR_LEASE_VERSION || owner != pid_text || group <= 1 { continue }
        // kill(..., 0) only queries process-group existence. EPERM also means
        // a live group, so only ESRCH authorizes an orphan transition.
        if unsafe { nix::libc::kill(-group, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(nix::libc::ESRCH) { continue }
        let dir_text = dir.to_string_lossy();
        let mount_prefix = format!("{dir_text}/");
        if mounts.lines().filter_map(|line| line.split_whitespace().nth(4))
            .any(|mount| mount == dir_text || mount.starts_with(&mount_prefix)) { continue }
        match remove_verified_workdir(&dir) {
            Ok(()) => eprintln!("autoclean: removed orphaned {}", dir.display()),
            Err(error) => eprintln!("autoclean: retained orphaned {}: {error}", dir.display()),
        }
    }
    Ok(())
}

/// Result of the one terminal cleanup transition.
enum WorkdirCleanup {
    /// Idle: --autoclean is off, or the workdir was already deleted.
    NothingOwned,
    /// Owned to Idle: the directory is gone and nothing is owned any more.
    Removed(std::path::PathBuf),
    /// Owned to Owned: the directory survived, so ownership and the retry stay.
    Retained {
        dir: std::path::PathBuf,
        error: std::io::Error,
    },
}

/// The single authoritative record of the session workdir --autoclean still owns.
struct WorkdirClaim {
    dir: std::path::PathBuf,
    lease: Option<std::fs::File>,
}

#[derive(Default)]
struct WorkdirOwnership {
    owned: std::sync::Mutex<Option<WorkdirClaim>>,
}

impl WorkdirOwnership {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<WorkdirClaim>> {
        self.owned.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Idle to Owned. The caller creates the directory just before this call,
    /// with no cancellation point between creation and the claim.
    fn claim(&self, dir: &std::path::Path) {
        *self.lock() = Some(WorkdirClaim { dir: dir.to_path_buf(), lease: None });
    }
    fn create_lease(&self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut owned = self.lock();
        let claim = owned.as_mut().ok_or_else(|| std::io::Error::other("no claimed workdir"))?;
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true)
            .mode(0o600).open(claim.dir.join(WORKDIR_LEASE_FILE))?;
        file.try_lock()?;
        claim.lease = Some(file);
        Ok(())
    }
    fn mark_sandbox(&self, group: u32) -> std::io::Result<()> {
        use std::io::{Seek, Write};
        let mut owned = self.lock();
        let file = owned.as_mut().and_then(|claim| claim.lease.as_mut())
            .ok_or_else(|| std::io::Error::other("no workdir lease"))?;
        file.set_len(0)?;
        file.rewind()?;
        writeln!(file, "{WORKDIR_LEASE_VERSION} {} {group}", std::process::id())?;
        file.sync_data()
    }
    /// The owned path, used by shutdown to decide whether it has work to do.
    fn owned(&self) -> Option<std::path::PathBuf> {
        self.lock().as_ref().map(|claim| claim.dir.clone())
    }
    /// The terminal transition: delete the owned workdir and release ownership
    /// only once it is actually gone. Both the delete and the release happen
    /// under one lock acquisition, so concurrent callers cannot observe or lose
    /// a half-finished transition.
    fn remove(&self) -> WorkdirCleanup {
        let mut owned = self.lock();
        let Some(dir) = owned.as_ref().map(|claim| claim.dir.clone()) else {
            return WorkdirCleanup::NothingOwned;
        };
        match remove_session_workdir(&dir) {
            Ok(()) => {
                *owned = None;
                WorkdirCleanup::Removed(dir)
            }
            Err(error) => WorkdirCleanup::Retained { dir, error },
        }
    }
}

/// Teardown signals and reaps processes with bounded sleeps; keep that off the
/// async workers that answer other MCP requests. Host KWallet handles the
/// session opened are released first, while the host bus is still reachable.
async fn teardown_blocking(mut sess: Session) {
    if let Some(wallet) = sess.wallet.take() {
        let _ = tokio::time::timeout(Duration::from_secs(15), wallet.shutdown()).await;
    }
    let _ = tokio::task::spawn_blocking(move || teardown(sess)).await;
}

fn teardown(mut sess: Session) {
    drop(sess.cdp_browser);
    // Kill the viewer first so it can flush any pending wayland requests
    // before the container's compositor disappears.
    if let Some(viewer) = sess.viewer_child.take() {
        terminate_child(viewer, false, "viewer");
    }
    drop(sess.sandbox_stdin);
    // Kill the pasta and bwrap process group (negative PID = entire group).
    terminate_child(sess.sandbox_child, true, "sandbox");
    for proxy in std::mem::take(&mut sess.service_proxy_children) {
        terminate_child(proxy, false, "session D-Bus proxy");
    }
    use std::os::unix::fs::PermissionsExt;
    for path in &sess.overlay_work_paths {
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o700);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
    let _ = std::fs::remove_dir_all(sess.host_xdg_dir.join("tmp"));
    cleanup_stale_session_files(&sess.host_xdg_dir);
}

/// Linux quotactl command for reading one user's quota (Q_GETQUOTA) and the
/// quota block unit of `struct if_dqblk` (QIF_DQBLKSIZE).
const Q_GETQUOTA: i32 = 0x0080_0007;
const USRQUOTA: i32 = 0;
const QUOTA_BLOCK_BYTES: u64 = 1024;
const GIB: f64 = 1_073_741_824.0;

#[repr(C)]
#[derive(Default)]
struct IfDqblk {
    dqb_bhardlimit: u64,
    dqb_bsoftlimit: u64,
    dqb_curspace: u64,
    dqb_ihardlimit: u64,
    dqb_isoftlimit: u64,
    dqb_curinodes: u64,
    dqb_btime: u64,
    dqb_itime: u64,
    dqb_valid: u32,
}

fn gib(bytes: u64) -> String {
    #[expect(clippy::as_conversions)]
    let value = bytes as f64 / GIB;
    format!("{value:.2} GiB")
}

/// Explain a failed write: for EDQUOT/ENOSPC, name the per-user quota (systemd
/// mounts /tmp as tmpfs with usrquota, so a user can hit its limit while df
/// still shows free space) and the filesystem's own free space.
fn describe_write_failure(path: &Path, error: &std::io::Error) -> String {
    let mut message = format!("writing {} failed: {error}", path.display());
    let Some(code) = error.raw_os_error() else { return message };
    if code != nix::libc::EDQUOT && code != nix::libc::ENOSPC {
        return message;
    }
    let directory = path.parent().unwrap_or(path);
    if let Ok(dir) = std::fs::File::open(directory) {
        use std::os::fd::AsRawFd;
        let mut quota = IfDqblk::default();
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/proc/self").map(|meta| meta.uid()).unwrap_or_default();
        // SAFETY: quotactl_fd writes one struct if_dqblk into `quota`, which is
        // repr(C) with the kernel layout and outlives the call.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_quotactl_fd,
                dir.as_raw_fd(),
                (Q_GETQUOTA << 8) | USRQUOTA,
                uid,
                std::ptr::from_mut(&mut quota),
            )
        };
        if result == 0 && quota.dqb_bhardlimit > 0 {
            let limit = quota.dqb_bhardlimit.saturating_mul(QUOTA_BLOCK_BYTES);
            message.push_str(&format!(
                ". Per-user disk quota for uid {uid} on this filesystem: {} used of a {} limit",
                gib(quota.dqb_curspace),
                gib(limit)
            ));
        }
    }
    if let Ok(stat) = nix::sys::statvfs::statvfs(directory) {
        let free = stat.blocks_available().saturating_mul(stat.fragment_size());
        message.push_str(&format!(
            "; the filesystem itself has {} free. The session workdir, including its HOME overlay upper layer, counts against this limit; free space owned by this user on that filesystem (or stop idle sessions) and retry",
            gib(free)
        ));
    }
    message
}

/// Write `bytes` to `path` via a sibling temporary file and rename, so a
/// failed write never leaves a truncated file at `path`.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("partial");
    let written = std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Root directory of the sandbox's mount namespace as seen from the host:
/// /proc/<pid>/root of bwrap's inner child, two levels below pasta.
fn sandbox_root(sandbox_pid: u32) -> Option<PathBuf> {
    let children = |parent: i32| -> Vec<i32> {
        procfs::process::all_processes()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|process| process.stat().is_ok_and(|stat| stat.ppid == parent))
            .map(|process| process.pid)
            .collect()
    };
    let pasta = i32::try_from(sandbox_pid).ok()?;
    let outer = *children(pasta).first()?;
    let inner = *children(outer).first()?;
    let root = PathBuf::from(format!("/proc/{inner}/root"));
    std::fs::read_dir(&root).ok().map(|_| root)
}

/// Stream-compare two files byte for byte.
fn same_contents(left: &Path, right: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut a, mut b) = (std::fs::File::open(left)?, std::fs::File::open(right)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut left_buf, mut right_buf) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let read = a.read(&mut left_buf)?;
        if read == 0 {
            return Ok(true);
        }
        b.read_exact(&mut right_buf[..read])?;
        if left_buf[..read] != right_buf[..read] {
            return Ok(false);
        }
    }
}

/// Copy a session file (read through the sandbox root) to a host path with a
/// temporary file and rename, then verify the host copy byte for byte.
fn export_session_file(source: &Path, destination: &Path, overwrite: bool) -> Result<u64, String> {
    let metadata = std::fs::metadata(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", source.display()));
    }
    if !overwrite && std::fs::symlink_metadata(destination).is_ok() {
        return Err(format!("{} already exists on the host; pass overwrite=true to replace it", destination.display()));
    }
    let parent = destination.parent().ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    if !parent.is_dir() {
        return Err(format!("host directory {} does not exist", parent.display()));
    }
    let temporary = parent.join(format!(
        ".{}.kwin-mcp-export",
        destination.file_name().and_then(|name| name.to_str()).unwrap_or("file")
    ));
    let copied = std::fs::copy(source, &temporary)
        .map_err(|error| describe_write_failure(&temporary, &error))
        .and_then(|bytes| match same_contents(source, &temporary) {
            Ok(true) => Ok(bytes),
            Ok(false) => Err("host copy differs from the session file (was it still being written?)".to_owned()),
            Err(error) => Err(format!("verify host copy: {error}")),
        })
        .and_then(|bytes| {
            std::fs::rename(&temporary, destination)
                .map(|()| bytes)
                .map_err(|error| format!("rename into {}: {error}", destination.display()))
        });
    if copied.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    copied
}

/// Screenshot settling after recent input; None when no input was recent.
struct SettleReport {
    settled: bool,
    waited: Duration,
}

type Frame = (u32, u32, u32, Vec<u8>);

/// One complete CaptureScreen frame, retrying short pipe reads.
async fn capture_frame(proxy: &KWinScreenShot2Proxy<'_>) -> Result<Frame, McpError> {
    let mut last_size = None;
    for attempt in 1..=SCREENSHOT_CAPTURE_ATTEMPTS {
        let (read_fd, write_fd) = nix::unistd::pipe().map_err(KwinError::from)?;
        let pipe_fd = zbus::zvariant::OwnedFd::from(write_fd);
        let mut opts = std::collections::HashMap::new();
        opts.insert("include-cursor", zbus::zvariant::Value::from(true));
        opts.insert("include-decoration", zbus::zvariant::Value::from(true));
        opts.insert("hide-caller-windows", zbus::zvariant::Value::from(false));
        // CaptureScreen composites all surfaces including popups (xdg_popup menus);
        // CaptureWindow only grabs the toplevel's own framebuffer and misses popups.
        let meta = proxy
            .capture_screen("Virtual-0", opts, pipe_fd)
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
        let expected = usize::try_from(stride * height).map_err(KwinError::from)?;
        let mut pixels = Vec::with_capacity(expected);
        std::io::Read::read_to_end(&mut std::io::BufReader::new(reader_file), &mut pixels)
            .map_err(KwinError::from)?;
        let received = pixels.len();
        if received == expected {
            return Ok((width, height, stride, pixels));
        }
        last_size = Some((expected, received));
        eprintln!(
            "screenshot: incomplete pixel buffer on attempt {attempt}/{SCREENSHOT_CAPTURE_ATTEMPTS}: expected {expected} bytes, received {received}"
        );
    }
    let (expected, received) = last_size.unwrap_or((0, 0));
    Err(McpError::internal_error(
        format!(
            "screenshot: incomplete pixel buffer after {SCREENSHOT_CAPTURE_ATTEMPTS} attempts (expected {expected} bytes, received {received})"
        ),
        None,
    ))
}

/// Capture a frame that reflects recent input. Input tools return once their
/// events are flushed, before the app has handled them and repainted, so a
/// capture shortly after input polls until the screen has been unchanged for
/// SCREENSHOT_SETTLE_QUIET, bounded by SCREENSHOT_SETTLE_LIMIT after the input.
async fn capture_settled_frame(
    proxy: &KWinScreenShot2Proxy<'_>,
    last_input: Option<std::time::Instant>,
) -> Result<(u32, u32, u32, Vec<u8>, Option<SettleReport>), McpError> {
    let started = std::time::Instant::now();
    let mut frame = capture_frame(proxy).await?;
    let Some(input_at) = last_input.filter(|at| at.elapsed() < SCREENSHOT_SETTLE_LIMIT) else {
        let (width, height, stride, pixels) = frame;
        return Ok((width, height, stride, pixels, None));
    };
    let deadline = input_at + SCREENSHOT_SETTLE_LIMIT;
    let mut unchanged_since = std::time::Instant::now();
    let settled = loop {
        let now = std::time::Instant::now();
        if now.duration_since(unchanged_since) >= SCREENSHOT_SETTLE_QUIET
            && now.duration_since(input_at) >= SCREENSHOT_SETTLE_QUIET
        {
            break true;
        }
        if now >= deadline {
            break false;
        }
        tokio::time::sleep(SCREENSHOT_SETTLE_POLL).await;
        let next = capture_frame(proxy).await?;
        if next.3 != frame.3 {
            unchanged_since = std::time::Instant::now();
        }
        frame = next;
    };
    let (width, height, stride, pixels) = frame;
    Ok((width, height, stride, pixels, Some(SettleReport { settled, waited: started.elapsed() })))
}

/// FUSE support for the sandbox, when the host allows it: /dev/fuse exists,
/// bwrap is not setuid (so it may grant capabilities in its user namespace),
/// setpriv can drop them for everything but the helper, and at least one real
/// fusermount binary exists. Returns (host binary, program name) pairs.
fn fuse_support() -> Option<Vec<(PathBuf, &'static str)>> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::FileTypeExt;
    let find = |name: &str| {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
        })
    };
    if !std::fs::metadata("/dev/fuse").is_ok_and(|meta| meta.file_type().is_char_device()) {
        return None;
    }
    let bwrap = find("bwrap")?;
    if std::fs::metadata(&bwrap).ok()?.permissions().mode() & 0o4000 != 0 {
        return None;
    }
    find("setpriv")?;
    let binaries: Vec<(PathBuf, &'static str)> = fuse_bridge::PROGRAMS
        .into_iter()
        .filter_map(|program| Some((std::fs::canonicalize(find(program)?).ok()?, program)))
        .collect();
    (!binaries.is_empty()).then_some(binaries)
}

/// Resolve the kwin-viewer binary by replacing the basename of our own
/// current_exe. Returns None if the binary doesn't exist — the viewer is
/// an optional convenience, not required for MCP tool operation.
fn resolve_viewer_binary() -> Option<std::path::PathBuf> {
    let me = std::env::current_exe().ok()?;
    let dir = me.parent()?;
    let candidate = dir.join("kwin-viewer");
    if candidate.exists() { Some(candidate) } else { None }
}

/// Quote one complete shell expression as a single argument to `bash -c`.
/// This keeps control operators inside the child shell so the required
/// environment reaches every simple command in the expression.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

const BROWSER_COMMANDS: &[&str] = &[
    "google-chrome-stable", "google-chrome", "chrome", "chromium", "chromium-browser",
    "brave", "brave-browser", "vivaldi", "vivaldi-stable", "microsoft-edge",
    "microsoft-edge-stable", "msedge", "code", "codium", "vscodium", "electron",
];

// Bash resolves the browser itself, so compound commands and wrappers need no
// second shell parser in the MCP server.
const BROWSER_WRAPPER: &str = r#"#!/usr/bin/env bash
set -e
name=${0##*/}
real=$(PATH="${PATH#*:}" command -v "$name") || exit 127
ozone=0 wallet=0 a11y=0 debug_port=0
for arg in "$@"; do
  case "$arg" in
    --ozone-platform|--ozone-platform=*) ozone=1 ;;
    --password-store|--password-store=*) wallet=1 ;;
    --force-renderer-accessibility|--force-renderer-accessibility=*) a11y=1 ;;
    --remote-debugging-port|--remote-debugging-port=*) debug_port=1 ;;
  esac
done
flags=()
(( ozone )) || flags+=(--ozone-platform=wayland)
(( wallet )) || flags+=(--password-store=kwallet6)
(( a11y )) || flags+=(--force-renderer-accessibility)
case "$name" in
  google-chrome*|chrome|microsoft-edge*|msedge*) ;;
  *)
    if [[ -n ${KWIN_MCP_CDP_PORT:-} ]]; then
      (( debug_port )) || flags+=("--remote-debugging-port=$KWIN_MCP_CDP_PORT")
      [[ -z ${KWIN_MCP_BROWSER_MARKER:-} ]] || printf cdp > "$KWIN_MCP_BROWSER_MARKER"
    fi
    ;;
esac
exec "$real" "${flags[@]}" "$@"
"#;

fn create_browser_wrappers(workdir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let bin = workdir.join("browser-bin");
    std::fs::create_dir_all(&bin)?;
    let wrapper = bin.join("kwin-browser");
    std::fs::write(&wrapper, BROWSER_WRAPPER)?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    for name in BROWSER_COMMANDS {
        symlink("kwin-browser", bin.join(name))?;
    }
    Ok(bin)
}

static NEXT_BROWSER_LAUNCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Browser launch commands we know how to detect. The container mounts the host
/// root read-only, so anything on the host's PATH is runnable inside the session
/// by the same command. Ordered most- to least-common so the injected hint reads
/// sensibly. `chromium` is called out separately by callers because it's the only
/// one that exposes CDP on its default profile.
const KNOWN_BROWSERS: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "brave",
    "brave-browser",
    "firefox",
    "firefox-esr",
    "vivaldi-stable",
    "vivaldi",
    "microsoft-edge-stable",
    "microsoft-edge",
    "opera",
];

/// Scan the host PATH for known browser commands. Returns the runnable command
/// names in KNOWN_BROWSERS order. Deduplicates so a name reachable from two PATH
/// entries is reported once. Runs once at server launch (see `main`).
fn detect_browsers() -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    KNOWN_BROWSERS
        .iter()
        .filter(|name| {
            dirs.iter().any(|dir| {
                let candidate = dir.join(name);
                std::fs::metadata(&candidate).is_ok_and(|m| m.is_file() || m.file_type().is_symlink())
            })
        })
        .map(|name| (*name).to_owned())
        .collect()
}

async fn host_wayland() -> anyhow::Result<(PathBuf, std::ffi::OsString)> {
    use std::os::unix::fs::FileTypeExt;
    let (inherited_display, inherited_runtime) = (std::env::var_os("WAYLAND_DISPLAY"), std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from));
    let socket = |runtime: &Path, display: &std::ffi::OsStr| if Path::new(display).is_absolute() { PathBuf::from(display) } else { runtime.join(display) };
    if let (Some(runtime), Some(display)) = (&inherited_runtime, &inherited_display) {
        let path = socket(runtime, display); anyhow::ensure!(std::fs::metadata(&path)?.file_type().is_socket(), "{} is not a Unix socket", path.display());
        return Ok((runtime.clone(), display.clone()));
    }
    let system = zbus::Connection::system().await?;
    let login = zbus::Proxy::new(&system, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager").await?;
    let (user_path,): (zbus::zvariant::OwnedObjectPath,) = login.call("GetUserByPID", &(std::process::id(),)).await?;
    let user = zbus::Proxy::new(&system, "org.freedesktop.login1", user_path, "org.freedesktop.login1.User").await?;
    let runtime = inherited_runtime.unwrap_or(PathBuf::from(user.get_property::<String>("RuntimePath").await?));
    let display = if let Some(display) = inherited_display {
        display
    } else {
        let (_, session_path): (String, zbus::zvariant::OwnedObjectPath) = user.get_property("Display").await?;
        let session = zbus::Proxy::new(&system, "org.freedesktop.login1", session_path, "org.freedesktop.login1.Session").await?;
        anyhow::ensure!(session.get_property::<bool>("Active").await?
            && session.get_property::<String>("State").await? == "active"
            && session.get_property::<String>("Type").await? == "wayland", "no active logind Wayland session");
        let managed = async {
            let address = format!("unix:path={}/bus", runtime.display());
            let user_bus = zbus::connection::Builder::address(address.as_str())?.build().await?;
            let manager = zbus::Proxy::new(&user_bus, "org.freedesktop.systemd1", "/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager").await?;
            anyhow::Ok(manager.get_property::<Vec<String>>("Environment").await?.into_iter()
            .find_map(|entry| entry.strip_prefix("WAYLAND_DISPLAY=").map(std::ffi::OsString::from))
            .filter(|display| std::fs::metadata(socket(&runtime, display)).is_ok_and(|meta| meta.file_type().is_socket())))
        }.await.ok().flatten();
        managed.or_else(|| std::fs::read_dir(&runtime).ok()?.filter_map(Result::ok).filter_map(|entry| {
                let name = entry.file_name();
                let number = name.to_str()?.strip_prefix("wayland-")?.parse::<u32>().ok()?;
                entry.file_type().ok()?.is_socket().then_some((number, name))
            }).min_by_key(|entry| entry.0).map(|entry| entry.1))
            .ok_or_else(|| anyhow::anyhow!("no Wayland socket in {}", runtime.display()))?
    };
    let socket = socket(&runtime, &display);
    anyhow::ensure!(std::fs::metadata(&socket)?.file_type().is_socket(), "{} is not a Unix socket", socket.display());
    Ok((runtime, display))
}

/// Status file the viewer writes (see src/bin/kwin-viewer.rs).
const VIEWER_STATUS_FILE: &str = "viewer-status.json";
/// How long session_start and viewer_open wait for the viewer to show a frame
/// before reporting it as still starting.
const VIEWER_READY_WAIT: Duration = Duration::from_secs(4);
const VIEWER_READY_POLL: Duration = Duration::from_millis(50);

/// Spawn the viewer as a sibling host-side process. Intentionally non-fatal:
/// if anything goes wrong the agent's MCP tools still work, and the error is
/// the reason reported in the viewer outcome. Stderr lands in
/// {session_dir}/viewer.log so crashes and input-forwarding diagnostics
/// survive past the spawn.
async fn spawn_viewer(host_xdg_dir: &Path, width: u32, height: u32) -> Result<std::process::Child, String> {
    let log_path = host_xdg_dir.join("viewer.log");
    let _ = std::fs::remove_file(host_xdg_dir.join(VIEWER_STATUS_FILE));
    let mut log_file = std::fs::File::create(&log_path).map_err(|error| format!("create {}: {error}", log_path.display()))?;
    let note = |log_file: &mut std::fs::File, reason: String| {
        let _ = std::io::Write::write_all(log_file, format!("kwin-viewer: {reason}\n").as_bytes());
        eprintln!("session viewer unavailable: {reason}");
        reason
    };
    let Some(bin) = resolve_viewer_binary() else {
        return Err(note(&mut log_file, "kwin-viewer binary not found next to kwin-mcp".to_owned()));
    };
    let (runtime, display) = match host_wayland().await {
        Ok(found) => found,
        Err(error) => return Err(note(&mut log_file, format!("host Wayland resolution failed: {error:#}"))),
    };
    let mut command = std::process::Command::new(&bin);
    command.arg(host_xdg_dir)
        .arg(width.to_string())
        .arg(height.to_string())
        .env("XDG_RUNTIME_DIR", runtime)
        .env("WAYLAND_DISPLAY", display)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));
    terminate_with_parent(&mut command);
    match command.spawn() {
        Ok(child) => {
            eprintln!("session_start: spawned viewer pid={}", child.id());
            Ok(child)
        }
        Err(e) => {
            let reason = format!("spawn failed: {e}");
            let _ = std::fs::write(&log_path, format!("kwin-viewer: {reason}\n"));
            eprintln!("session viewer unavailable: {reason}");
            Err(reason)
        }
    }
}

/// The viewer outcome: ready (a frame is showing in a host window), starting,
/// unavailable with a reason, or disabled.
fn viewer_report(host_xdg_dir: &Path, child: Option<&mut std::process::Child>, unavailable: Option<&str>) -> serde_json::Value {
    let log = host_xdg_dir.join("viewer.log").display().to_string();
    let status: Option<serde_json::Value> = std::fs::read_to_string(host_xdg_dir.join(VIEWER_STATUS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let field = |name: &str| status.as_ref().and_then(|value| value[name].as_str()).unwrap_or_default().to_owned();
    let Some(child) = child else {
        let reason = unavailable.unwrap_or("viewer not running");
        let state = if reason.contains("--no-viewer") { "disabled" } else { "unavailable" };
        return serde_json::json!({"state": state, "reason": reason, "log": log});
    };
    if let Ok(Some(exit)) = child.try_wait() {
        let detail = field("detail");
        let reason = if detail.is_empty() { format!("viewer exited ({exit})") } else { format!("viewer exited ({exit}): {detail}") };
        return serde_json::json!({"state": "unavailable", "reason": reason, "log": log});
    }
    match field("state").as_str() {
        "ready" => serde_json::json!({"state": "ready", "pid": child.id(), "log": log}),
        "failed" | "closed" => serde_json::json!({"state": "unavailable", "reason": field("detail"), "pid": child.id(), "log": log}),
        phase => serde_json::json!({
            "state": "starting",
            "phase": if phase.is_empty() { "launching" } else { phase },
            "pid": child.id(),
            "log": log,
        }),
    }
}

/// Poll the viewer until it is ready, has failed, or VIEWER_READY_WAIT passes.
async fn wait_for_viewer(host_xdg_dir: &Path, child: &mut std::process::Child) -> serde_json::Value {
    let deadline = std::time::Instant::now() + VIEWER_READY_WAIT;
    loop {
        let report = viewer_report(host_xdg_dir, Some(child), None);
        if report["state"] != "starting" || std::time::Instant::now() >= deadline {
            return report;
        }
        tokio::time::sleep(VIEWER_READY_POLL).await;
    }
}

/// One-line summary of a viewer report for tool text output.
fn viewer_summary(report: &serde_json::Value) -> String {
    match report["state"].as_str().unwrap_or_default() {
        "ready" => "viewer=ready".to_owned(),
        "starting" => format!("viewer=starting ({})", report["phase"].as_str().unwrap_or_default()),
        state => format!("viewer={state}: {}", report["reason"].as_str().unwrap_or_default()),
    }
}

async fn run_kwin_script(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &std::path::Path,
    script_body: &str,
) -> Result<String, KwinError> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let marker = format!("kwin-mcp-{ts}");
    let cb_path = format!("/KWinMCP/{ts}");
    let our_name = conn
        .unique_name()
        .ok_or(KwinError::Msg("no bus name".to_owned()))?
        .to_string();
    let our_name_json = serde_json::to_string(&our_name)?;
    let cb_path_json = serde_json::to_string(&cb_path)?;
    let script = format!(
        "{script_body}\n\
        callDBus({our_name_json},{cb_path_json},'org.kde.KWinMCP','result',JSON.stringify(result));"
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
    eprintln!("run_kwin_script: our_name={our_name} path={cb_path} registered={registered}");
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
    if let Err(error) = script_proxy.call::<_, (), ()>("run", &()).await {
        conn.object_server().remove::<KWinCallback, _>(&obj_path).await?;
        let (_,): (bool,) = scripting.call("unloadScript", &(&marker,)).await?;
        std::fs::remove_file(&script_file)?;
        return Err(error.into());
    }
    // Wait for callback, then cleanup regardless of result
    let result = rx
        .await
        .map_err(|_| KwinError::Msg("KWin callback channel closed".to_owned()));
    conn.object_server().remove::<KWinCallback, _>(&obj_path).await?;
    let (_,): (bool,) = scripting.call("unloadScript", &(&marker,)).await?;
    std::fs::remove_file(&script_file)?;
    result
}

async fn active_window_info(conn: &zbus::Connection, kwin_unique: &str, host_xdg_dir: &std::path::Path) -> Result<(i32, i32, WindowGeometry), KwinError> {
    let json = run_kwin_script(
        conn,
        kwin_unique,
        host_xdg_dir,
        "var w = workspace.activeWindow;\
         var c = workspace.cursorPos;\
         var result = w ? {x:w.clientGeometry.x,y:w.clientGeometry.y,\
         w:w.clientGeometry.width,h:w.clientGeometry.height,\
         title:w.caption,id:w.internalId.toString(),\
         cx:c.x,cy:c.y} : null;",
    )
    .await?;
    if json == "null" {
        return Err(KwinError::Msg("KWin script error: No active window".to_owned()));
    }
    let info: WindowGeometry = serde_json::from_str(&json)?;
    #[expect(clippy::as_conversions)]
    let (x, y) = (info.x.round() as i32, info.y.round() as i32);
    Ok((x, y, info))
}

#[derive(Deserialize, Serialize)]
struct WindowSnapshot {
    id: String,
    title: String,
    #[serde(rename = "resourceClass")]
    resource_class: String,
    #[serde(rename = "resourceName")]
    resource_name: String,
    pid: i32,
    active: bool,
    minimized: bool,
    modal: bool,
    transient: bool,
    dialog: bool,
    popup: bool,
    #[serde(rename = "transientFor")]
    transient_for: Option<String>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    #[serde(rename = "stackingOrder")]
    stacking_order: i32,
}

impl WindowSnapshot {
    fn summary(&self) -> String {
        let mut flags = Vec::new();
        if self.active {
            flags.push("active");
        }
        if self.minimized {
            flags.push("minimized");
        }
        if self.modal {
            flags.push("modal");
        }
        if self.transient {
            flags.push("transient");
        }
        if self.dialog {
            flags.push("dialog");
        }
        if self.popup {
            flags.push("popup");
        }
        let flags = if flags.is_empty() {
            "normal".to_owned()
        } else {
            flags.join(",")
        };
        let transient_for = self
            .transient_for
            .as_deref()
            .map(|id| format!(" transient_for={id}"))
            .unwrap_or_default();
        format!(
            "[{flags}] id={} title={:?} app={}/{} pid={} geometry=({:.0},{:.0} {:.0}x{:.0}) stack={}{}",
            self.id,
            self.title,
            self.resource_class,
            self.resource_name,
            self.pid,
            self.x,
            self.y,
            self.w,
            self.h,
            self.stacking_order,
            transient_for,
        )
    }
}

async fn window_snapshots(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
) -> Result<Vec<WindowSnapshot>, KwinError> {
    let json = run_kwin_script(
        conn,
        kwin_unique,
        host_xdg_dir,
        "var result = [];\
         var windows = workspace.stackingOrder;\
         for (var i = windows.length - 1; i >= 0; --i) {\
           var w = windows[i];\
           if (!w.managed || w.deleted || w.desktopWindow || w.dock || w.outline) continue;\
           var g = w.clientGeometry;\
           result.push({\
             id:w.internalId.toString(),title:w.caption,\
             resourceClass:w.resourceClass,resourceName:w.resourceName,pid:w.pid,\
             active:w.active,minimized:w.minimized,modal:w.modal,transient:w.transient,\
             dialog:w.dialog,popup:w.popupWindow,\
             transientFor:w.transientFor ? w.transientFor.internalId.toString() : null,\
             x:g.x,y:g.y,w:g.width,h:g.height,stackingOrder:w.stackingOrder\
           });\
         }",
    )
    .await?;
    Ok(serde_json::from_str(&json)?)
}

async fn activate_window(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
    window_id: &str,
) -> Result<WindowSnapshot, KwinError> {
    let window_id_json = serde_json::to_string(window_id)?;
    let script = format!(
        "var targetId = {window_id_json};\
         var result = false;\
         var windows = workspace.stackingOrder;\
         for (var i = 0; i < windows.length; ++i) {{\
           var w = windows[i];\
           if (w.internalId.toString() !== targetId) continue;\
           w.minimized = false;\
           workspace.raiseWindow(w);\
           workspace.activeWindow = w;\
           result = true;\
           break;\
         }}"
    );
    let activated: bool = serde_json::from_str(
        &run_kwin_script(conn, kwin_unique, host_xdg_dir, &script).await?,
    )?;
    if !activated {
        return Err(KwinError::Msg(format!("no window with id {window_id}")));
    }
    tokio::time::sleep(INPUT_EVENT_DELAY).await;
    let windows = window_snapshots(conn, kwin_unique, host_xdg_dir).await?;
    let window = windows
        .into_iter()
        .find(|window| window.id == window_id)
        .ok_or_else(|| KwinError::Msg(format!("window {window_id} disappeared")))?;
    if !window.active {
        return Err(KwinError::Msg(format!(
            "KWin did not activate window {window_id}"
        )));
    }
    Ok(window)
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
    w: f64,
    #[serde(default)]
    h: f64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    cx: f64,
    #[serde(default)]
    cy: f64,
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
struct ScreenshotParams {
    /// Crop region [x1, y1, x2, y2] in window-relative pixels (the same space
    /// as mouse input). It may extend past the window edges, e.g. negative
    /// values to include a popup; it is clamped to the screen. Omit to capture
    /// the whole active window.
    #[serde(default)]
    region: Option<[i32; 4]>,
    /// When true, return a 10x-upscaled crop centered on the current cursor
    /// position. Useful for confirming exactly what the cursor is hovering or
    /// where a click would land. Mutually exclusive with `region`.
    #[serde(default)]
    cursor: bool,
    /// When true, attach the PNG inline (base64) to the tool result so the
    /// calling model can see the image directly without a follow-up read.
    /// The file is still written to disk either way.
    #[serde(default)]
    inline: bool,
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
struct ExportFileParams {
    /// Absolute path of the file as the session sees it, e.g. the path Chrome
    /// reported for a finished download (/home/<user>/Downloads/report.pdf).
    session_path: String,
    /// Absolute host path to write, or an existing host directory to write
    /// into under the same file name. Omit to use session_path itself, which
    /// hands a download over to the real directory it names.
    host_path: Option<String>,
    /// Replace an existing host file. Default false: an existing file is left
    /// alone and the export fails.
    #[serde(default)]
    overwrite: bool,
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

#[derive(Deserialize, schemars::JsonSchema)]
struct SessionStartParams {
    /// Virtual display width in pixels for this session. Omit to use the
    /// server default. Ignored when the server was launched with --no-override.
    width: Option<u32>,
    /// Virtual display height in pixels for this session. Omit to use the
    /// server default. Ignored when the server was launched with --no-override.
    height: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WindowActivateParams {
    /// Exact window ID returned by window_list.
    window_id: String,
}

// ── Tool implementations ────────────────────────────────────────────────

impl rmcp::ServerHandler for KwinMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_logging().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "KDE Wayland desktop automation in an isolated container. \
                Required first step: call session_start — every other tool fails until it succeeds. It is idempotent; if a session is already up you get its info back without restarting it (call session_stop + session_start to restart). \
                Typical flow: session_start → launch_app → screenshot → mouse_click / keyboard_type / keyboard_key → screenshot to verify → session_stop when done. Use find_ui_elements for a specific named control; use a filtered accessibility_tree only when structure helps. If the tree is empty or costly, continue with screenshots and input. \
                If an expected prompt or app is missing, call window_list before concluding it is absent; use window_activate with its ID, then screenshot and interact normally. \
                To attach a file in Chrome's file chooser: click the page's file input, press ctrl+l, type the absolute path, then press alt+o (or click Open at the top right); Enter in the location bar cancels Chrome's chooser and attaches nothing. \
                All mouse/screenshot coordinates are pixels relative to the active window's top-left (not the virtual display). \
                {size_line} Windows are auto-maximized; a window-relative click at (100,100) lands 100px from the window's top-left corner. \
                Screenshots are 1:1 pixels (no DPI scaling, no resampling) of the active window, so a pixel you read off a default screenshot is the coordinate you pass to mouse_click, even for a small dialog away from the display origin. A cropped screenshot reports region=[x1,y1,x2,y2]; its pixel (px,py) is mouse_click (px+x1, py+y1).",
                size_line = if self.display.locked {
                    format!("The virtual display is fixed at {}x{} (server launched with --no-override; session_start size params are ignored).", self.display.width, self.display.height)
                } else {
                    format!("The virtual display defaults to {}x{}; session_start accepts optional width/height to override it for that session.", self.display.width, self.display.height)
                },
            ))
    }
}

#[rmcp::tool_router]
impl KwinMcp {
    #[rmcp::tool(
        name = "session_start",
        description = "Boot a black box carbon copy live session. Required before every other tool; all fail with 'no session' until this succeeds. Idempotent: if a session is already running, returns its bus name and workdir without disturbing it (status=already_running). Optional width/height (pixels) set the virtual display size for this session, overriding the server default; they are ignored if the server was launched with --no-override, and ignored on an already-running session (session_stop first to resize). The result reports the actual width/height in effect, and a separate viewer outcome (ready, starting, unavailable with the reason, or disabled); a missing viewer never fails the session, and viewer_open can retry it. Container writes to $HOME land in a per-session overlay at /tmp/kwin-mcp-<pid>/tmp/overlay-upper/. The lower layer remains read-only, and session_stop discards the upper layer; use export_file to hand a session file (e.g. a download) to a real host directory."
    )]
    async fn session_start(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<SessionStartParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        // The hard limit covers the gate wait too, so a start queued behind a
        // blocked lifecycle operation still answers within it.
        let deadline = tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT;
        let snapshot_deadline = std::time::Instant::now()
            + (SESSION_START_HARD_TIMEOUT - SQLITE_SNAPSHOT_STARTUP_RESERVE);
        let gate = self.lifecycle_gate(deadline, "session_start").await?;
        self.set_start_stage("preparing the session workdir");
        let host_work: HostWork = Arc::new(std::sync::Mutex::new(None));
        let outcome = match tokio::time::timeout_at(deadline, self.session_start_inner(peer, params, host_work.clone(), snapshot_deadline)).await {
            Ok(res) => res,
            Err(_) => {
                let stage = self.start_stage();
                let message = format!(
                    "session_start exceeded {}s hard limit while {stage}",
                    SESSION_START_HARD_TIMEOUT.as_secs()
                );
                eprintln!("session_start: {message}");
                let blocked = host_work.lock().ok().and_then(|mut slot| slot.take()).filter(|handle| !handle.is_finished());
                if let Some(handle) = blocked {
                    // A host call is still blocked in the kernel and may yet
                    // write into the workdir. Keep the gate and the workdir
                    // until it returns, then run the usual failed-start cleanup.
                    let this = self.clone();
                    tokio::spawn(async move {
                        let _ = handle.await;
                        let error = this.autoclean_unpublished_workdir(McpError::internal_error("deferred startup cleanup", None)).await;
                        eprintln!("session_start: blocked host scan returned; {}", error.message);
                        drop(gate);
                    });
                    return Err(McpError::internal_error(
                        format!("{message}. A host filesystem or /proc call has not returned (check for hung FUSE or network mounts under $HOME and processes stuck in D state); cleanup runs when it does, and lifecycle calls report busy until then."),
                        Some(serde_json::json!({"reason": "hard_timeout", "stage": stage, "host_call_blocked": true})),
                    ));
                }
                Err(McpError::internal_error(
                    message,
                    Some(serde_json::json!({"reason": "hard_timeout", "stage": stage, "host_call_blocked": false})),
                ))
            }
        };
        let result = match outcome {
            Ok(result) => Ok(result),
            Err(error) => Err(self.autoclean_unpublished_workdir(error).await),
        };
        drop(gate);
        result
    }

    async fn session_start_inner(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        params: SessionStartParams,
        host_work: HostWork,
        snapshot_deadline: std::time::Instant,
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
            if let Some(existing) = guard.as_mut() {
                let bus_name = existing.kwin_conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
                let workdir = existing.host_xdg_dir.display().to_string();
                let viewer = viewer_report(&existing.host_xdg_dir, existing.viewer_child.as_mut(), existing.viewer_unavailable.as_deref());
                let msg = format!(
                    "{version_stamp} — session already running bus={bus_name} kwin={} display={}x{} workdir={workdir}. Call session_stop first to restart.",
                    existing.kwin_unique_name, existing.screen_width, existing.screen_height,
                );
                return Ok(structured_result(&peer, msg, serde_json::json!({
                    "status": "already_running",
                    "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
                    "commit": env!("GIT_HASH"),
                    "bus": bus_name,
                    "kwin_unique": existing.kwin_unique_name,
                    "workdir": workdir,
                    "width": existing.screen_width,
                    "height": existing.screen_height,
                    "viewer": viewer,
                })).await);
            }
        }
        // Resolve the virtual display size: tool params > CLI flags > compiled
        // defaults — unless --no-override locked it at the CLI/compiled value.
        let (screen_w, screen_h) = if self.display.locked {
            if params.width.is_some() || params.height.is_some() {
                eprintln!(
                    "session_start: size params ignored (--no-override), using {}x{}",
                    self.display.width, self.display.height
                );
            }
            (self.display.width, self.display.height)
        } else {
            let w = params.width.unwrap_or(self.display.width);
            let h = params.height.unwrap_or(self.display.height);
            for (name, v) in [("width", w), ("height", h)] {
                if !(MIN_SCREEN_DIM..=MAX_SCREEN_DIM).contains(&v) {
                    return Err(McpError::invalid_params(
                        format!("{name} {v} out of range {MIN_SCREEN_DIM}..={MAX_SCREEN_DIM}"),
                        None,
                    ));
                }
            }
            (w, h)
        };
        eprintln!("session_start: virtual display {screen_w}x{screen_h}");
        let host_xdg_dir = session_workdir_path();
        // Creation and the ownership claim are synchronous with no cancellation
        // point between them. Never claim an existing path from another run.
        std::fs::create_dir(&host_xdg_dir).map_err(|e| ver_err(e.to_string()))?;
        if self.display.autoclean {
            self.workdir.claim(&host_xdg_dir);
            self.workdir.create_lease().map_err(|e| ver_err(format!("create workdir lease: {e}")))?;
        }
        cleanup_stale_session_files(&host_xdg_dir);
        std::fs::create_dir_all(host_xdg_dir.join("tmp")).map_err(|e| ver_err(e.to_string()))?;
        create_browser_wrappers(&host_xdg_dir).map_err(|e| ver_err(format!("browser wrappers: {e}")))?;
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
        // Protected from agent writes: the ro-bind shadows the overlay-upper entry.
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
        let overlay_target = PathBuf::from(&home);
        if !overlay_target.is_absolute() {
            return Err(ver_err(format!("overlay target must be absolute: {}", overlay_target.display())));
        }
        let host_runtime = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .map_err(|error| ver_err(format!("host runtime directory: {error}")))?;
        // Mount, socket, and /proc scans can block in the kernel on a hung
        // FUSE or network mount, or on a process stuck in D state. Run them on
        // a blocking thread so the hard limit can still answer; the handle
        // stays in `host_work` so a timed-out start defers workdir cleanup
        // until this thread has stopped writing into it.
        self.set_start_stage("scanning host mounts, sockets, and processes");
        let (view_tx, view_rx) = tokio::sync::oneshot::channel();
        let view_target = overlay_target.clone();
        let view_xdg = host_xdg_dir.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _ = view_tx.send(prepare_host_view(&view_target, &view_xdg, &host_runtime, snapshot_deadline));
        });
        if let Ok(mut slot) = host_work.lock() {
            *slot = Some(handle);
        }
        let host_view = view_rx
            .await
            .map_err(|error| ver_err(format!("host scan thread: {error}")))?
            .map_err(|error| ver_err(format!("{error:#}")))?;
        if let Ok(mut slot) = host_work.lock() {
            slot.take();
        }
        let HostView { mut overlay_plan, kdeglobals: mut kdeglobals_content } = host_view;
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
        let fc_hinting_dest = std::fs::canonicalize("/usr/share/fontconfig/conf.default/10-hinting-slight.conf")
            .map_err(|e| ver_err(format!("resolve fontconfig hinting destination: {e}")))?;
        let fc_lcd_dest = std::fs::canonicalize("/usr/share/fontconfig/conf.default/11-lcdfilter-default.conf")
            .map_err(|e| ver_err(format!("resolve fontconfig LCD destination: {e}")))?;
        let fc_hinting_dest_str = fc_hinting_dest.display().to_string();
        let fc_lcd_dest_str = fc_lcd_dest.display().to_string();
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
            mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix\n\
            mkdir -p /tmp/fontconfig-cache && fc-cache -f 2>/dev/null\n\
            printf '<busconfig><include>/usr/share/dbus-1/session.conf</include><auth>ANONYMOUS</auth><allow_anonymous/></busconfig>' > /tmp/mcp-dbus.conf\n\
            dbus-daemon --config-file=/tmp/mcp-dbus.conf --address='unix:path={xdg_dir_str}/bus' --nofork &\n\
            dbus_pid=$!\n\
            n=0; while [ ! -S '{xdg_dir_str}/bus' ] && kill -0 \"$dbus_pid\" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
            export DBUS_SESSION_BUS_ADDRESS='unix:path={xdg_dir_str}/bus'\n\
            touch '{xdg_dir_str}/dbus-ready'\n\
            n=0; while [ ! -f '{xdg_dir_str}/bridge-ready' ] && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
            KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 \
            kwin_wayland --virtual --xwayland --no-lockscreen --width {screen_w} --height {screen_h} &\n\
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

        let cleanup_err = |message: String, startup: &mut StartupResources| {
            eprintln!("session_start: startup error: {message}");
            startup.cleanup();
            Err(ver_err(message))
        };
        // Begin ownership before the first proxy is spawned. Any failure or
        // cancellation between proxy acquisitions is therefore still covered.
        let mut startup = StartupResources::new();
        self.set_start_stage("starting host D-Bus proxies");
        let system_proxy_socket = host_xdg_dir.join("system_bus_socket");
        match spawn_dbus_proxy(
            "unix:path=/run/dbus/system_bus_socket",
            &system_proxy_socket,
            &[
                "--call=org.freedesktop.NetworkManager=org.freedesktop.DBus.Properties.Get@/*",
                "--call=org.freedesktop.NetworkManager=org.freedesktop.DBus.Properties.GetAll@/*",
                "--broadcast=org.freedesktop.NetworkManager=org.freedesktop.DBus.Properties.PropertiesChanged@/*",
            ],
        ).await {
            Ok(child) => {
                if cfg!(debug_assertions)
                    && std::env::var("KWIN_MCP_TEST_FAIL_AFTER_FIRST_PROXY").ok().as_deref()
                        == Some("1")
                {
                    eprintln!("session_start: test first proxy registered pid={}", child.id());
                }
                startup.add_proxy(child);
                if cfg!(debug_assertions)
                    && std::env::var("KWIN_MCP_TEST_FAIL_AFTER_FIRST_PROXY").ok().as_deref()
                        == Some("1")
                {
                    return cleanup_err("test failure after first proxy".to_owned(), &mut startup);
                }
            }
            Err(error) => return cleanup_err(format!("system D-Bus proxy: {error:#}"), &mut startup),
        }
        let host_session_bus = match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
            Ok(address) => address,
            Err(error) => return cleanup_err(format!("host session D-Bus address: {error}"), &mut startup),
        };
        // Session apps reach KWallet through a mediator on a private bus (see
        // wallet_mediator): it fails closed on an unhealthy host wallet and
        // releases every host handle the session opens.
        self.set_start_stage("starting the KWallet mediator");
        let wallet_bus_socket = host_xdg_dir.join("kwallet_bus");
        let wallet_bus_config = host_xdg_dir.join("kwallet-bus.conf");
        if let Err(error) = std::fs::write(&wallet_bus_config, format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
             <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>\n",
            wallet_bus_socket.display()
        )) {
            return cleanup_err(format!("write KWallet bus config: {error}"), &mut startup);
        }
        let mut wallet_bus = std::process::Command::new("dbus-daemon");
        wallet_bus
            .arg(format!("--config-file={}", wallet_bus_config.display()))
            .args(["--nofork", "--nopidfile"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        terminate_with_parent(&mut wallet_bus);
        match wallet_bus.spawn() {
            Ok(child) => startup.add_proxy(child),
            Err(error) => return cleanup_err(format!("start KWallet bus: {error}"), &mut startup),
        }
        if let Err(error) = wait_for_socket(&wallet_bus_socket, "KWallet bus socket", std::time::Instant::now() + DBUS_PROXY_TIMEOUT).await {
            return cleanup_err(error, &mut startup);
        }
        let wallet_bus_address = format!("unix:path={}", wallet_bus_socket.display());
        let host_bus = match zbus::connection::Builder::address(host_session_bus.as_str()) {
            Ok(builder) => builder.build().await,
            Err(error) => Err(error),
        };
        let (wallet_preflight, wallet) = match host_bus {
            Ok(host_bus) => {
                let preflight = wallet_mediator::preflight(&host_bus).await;
                eprintln!("session_start: KWallet: {}", preflight.reason);
                match wallet_mediator::WalletMediator::start(&wallet_bus_address, host_bus, preflight.wallet.clone()).await {
                    Ok(mediator) => (preflight, mediator),
                    Err(error) => return cleanup_err(format!("KWallet mediator: {error}"), &mut startup),
                }
            }
            Err(error) => return cleanup_err(format!("host session bus: {error}"), &mut startup),
        };
        let service_proxy_socket = host_xdg_dir.join("service_bus_socket");
        match spawn_dbus_proxy(
            &wallet_bus_address,
            &service_proxy_socket,
            &[
                // Upstream is the KWallet mediator's private bus, which does
                // the real filtering (wallet_mediator); these rules only keep
                // the session to KWallet calls on that bus.
                "--see=org.kde.kwalletd6",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Introspectable.Introspect@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Peer.GetMachineId@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Peer.Ping@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Properties.Get@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Properties.GetAll@/*",
                "--call=org.kde.kwalletd6=org.kde.KWallet.isEnabled@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.networkWallet@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.localWallet@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.wallets@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.isOpen@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.open@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.openAsync@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.openPath@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.openPathAsync@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.folderList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.hasFolder@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.entryList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.entriesList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.hasEntry@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.entryType@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.readEntry@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.readMap@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.readPassword@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.mapList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.passwordList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.users@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.close@/modules/kwalletd6",
                "--broadcast=org.kde.kwalletd6=org.kde.KWallet.walletAsyncOpened@/modules/kwalletd6",
                "--broadcast=org.kde.kwalletd6=org.kde.KWallet.walletOpened@/modules/kwalletd6",
            ],
        ).await {
            Ok(child) => startup.add_proxy(child),
            Err(error) => return cleanup_err(format!("session D-Bus proxy: {error:#}"), &mut startup),
        }
        let service_bus_address = format!("unix:path={}", service_proxy_socket.display());
        let proxy_sock_str = system_proxy_socket.display().to_string();

        let cdp_forward_port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr().map(|address| address.port()))
            .map_err(|error| ver_err(format!("reserve CDP forwarding port: {error}")))?;
        let host_name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map_err(|error| ver_err(format!("read host name: {error}")))?;
        let host_name = host_name.trim();
        if host_name.is_empty() {
            return Err(ver_err("host name is empty".to_owned()));
        }
        use std::os::unix::fs::MetadataExt;
        let identity = std::fs::metadata("/proc/self")
            .map_err(|error| ver_err(format!("read current UID and GID: {error}")))?;
        let uid = identity.uid().to_string();
        let gid = identity.gid().to_string();
        // pasta creates private loopback and forwards only the session's CDP
        // port to host loopback. Disable its automatic port and gateway maps.
        let mut cmd = std::process::Command::new("pasta");
        let cdp_forward_spec = format!("127.0.0.1/{cdp_forward_port}");
        cmd.args([
            "--quiet", "--config-net", "--no-map-gw", "--host-lo-to-ns-lo",
            "--tcp-ports", &cdp_forward_spec, "--udp-ports", "none",
            "--tcp-ns", "none", "--udp-ns", "none", "--", "bwrap",
        ]);
        // FUSE needs a process holding CAP_SYS_ADMIN in the user namespace that
        // owns the sandbox's mount namespace. With --uid other than 0, bwrap
        // (needing root for devpts) nests a second user namespace, where the
        // capability cannot mount. So with FUSE, bwrap stays at uid 0 in one
        // namespace for the helper, and the entrypoint nests the namespace
        // that maps the real uid for everything else.
        let fuse = fuse_support().zip(std::env::current_exe().ok());
        let (sandbox_uid, sandbox_gid) = if fuse.is_some() { ("0", "0") } else { (uid.as_str(), gid.as_str()) };
        cmd.args([
            "--die-with-parent", "--unshare-user", "--uid", sandbox_uid, "--gid", sandbox_gid,
            "--unshare-pid", "--unshare-uts", "--hostname", host_name, "--unshare-ipc",
        ]);
        overlay_plan.add_bwrap_args(&mut cmd, &overlay_target);
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
            "--ro-bind-try", &proxy_sock_str, "/run/dbus/system_bus_socket",
            "--bind", &xdg_dir_str, &xdg_dir_str,
            // System config overrides (read-only)
            "--ro-bind", &atspi_conf_path.display().to_string(), "/usr/share/defaults/at-spi2/accessibility.conf",
            "--ro-bind", &fc_hinting_str, &fc_hinting_dest_str,
            "--ro-bind", &fc_lcd_str, &fc_lcd_dest_str,
            // Mask dbus service files so the container's dbus-daemon doesn't auto-activate
            // $HOME config overrides (read-only — protects display settings from agent writes)
            "--ro-bind", &kwinrc_str, &home_kwinrc,
            "--ro-bind", &kdeglobals_str, &home_kdeglobals,
            "--ro-bind", &kwinrulesrc_str, &home_kwinrulesrc,
            "--ro-bind", &kscreenlockerrc_str, &home_kscreenlockerrc,
            "--ro-bind", &kcmfonts_str, &home_kcmfonts,
            "--ro-bind", &fonts_conf_str, &home_fonts_conf,
        ]);
        // NVIDIA GPUs expose their GBM/EGL driver through char nodes that live OUTSIDE
        // /dev/dri (/dev/nvidia0, nvidiactl, nvidia-modeset, nvidia-uvm, …). Without
        // them the NVIDIA render node (often the primary GBM device) loads as
        // driver=(null) → eglInitialize fails → KWin's virtual backend can't composite
        // → ScreenShot2 returns Error.Cancelled. Glob so we pick up whatever this host
        // exposes; -try so non-NVIDIA hosts (no matches) still start.
        for path in glob::glob("/dev/nvidia*").into_iter().flatten().flatten() {
            cmd.arg("--dev-bind-try").arg(&path).arg(&path);
        }
        // FUSE: /dev/fuse, CAP_SYS_ADMIN in the sandbox's user namespace for
        // the helper only, and fusermount shims over the real binaries.
        let sandbox_command = match &fuse {
            Some((binaries, exe)) => {
                cmd.args(["--dev-bind", "/dev/fuse", "/dev/fuse", "--cap-add", "CAP_SYS_ADMIN"]);
                for (real, program) in binaries {
                    let hidden = format!("{}/{program}", fuse_bridge::REAL_BINARY_DIR);
                    cmd.arg("--ro-bind").arg(real).arg(&hidden);
                    cmd.arg("--ro-bind").arg(exe).arg(real);
                }
                let entrypoint_path = host_xdg_dir.join("entrypoint.sh");
                std::fs::write(&entrypoint_path, &entrypoint).map_err(|e| ver_err(format!("write entrypoint: {e}")))?;
                eprintln!("session_start: FUSE bridge enabled ({})", binaries.iter().map(|(_, program)| *program).collect::<Vec<_>>().join(", "));
                // The AppImage runtime only accepts a setuid-root fusermount
                // from PATH, which no binary is inside a user namespace; it
                // takes FUSERMOUNT_PROG as given.
                let prog = binaries.iter().find(|(_, program)| *program == "fusermount3")
                    .map(|(real, _)| format!("export FUSERMOUNT_PROG={}\n", shell_quote(&real.display().to_string())))
                    .unwrap_or_default();
                format!(
                    "{prog}{} --fuse-helper {} </dev/null >/dev/null &\nexec unshare --user --map-user={uid} --map-group={gid} setpriv --inh-caps=-all --ambient-caps=-all bash {}",
                    shell_quote(&exe.display().to_string()),
                    fuse_bridge::HELPER_SOCKET,
                    shell_quote(&entrypoint_path.display().to_string()),
                )
            }
            None => {
                eprintln!("session_start: FUSE unavailable on this host (needs /dev/fuse, non-setuid bwrap, setpriv, fusermount)");
                entrypoint.clone()
            }
        };
        cmd.args(["--", "bash", "-c", &sandbox_command]);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::inherit());
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        terminate_with_parent(&mut cmd);
        self.set_start_stage("spawning the pasta and bwrap sandbox");
        let sandbox_child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => return cleanup_err(format!("start pasta: {error} (install passt)"), &mut startup),
        };
        startup.sandbox_child = Some(sandbox_child);
        let sandbox_pid = startup.sandbox_child.as_ref().map(std::process::Child::id).unwrap_or_default();
        if self.display.autoclean
            && let Err(error) = self.workdir.mark_sandbox(sandbox_pid) {
                return cleanup_err(format!("record sandbox owner: {error}"), &mut startup);
            }
        eprintln!("session_start: pasta spawned pid={sandbox_pid:?}");
        if let Some(child) = startup.sandbox_child.as_ref() {
            test_stop_bwrap(child);
        }
        let sandbox_stdin = startup
            .sandbox_child
            .as_mut()
            .and_then(|child| child.stdin.take());
        match sandbox_stdin {
            Some(stdin) => startup.sandbox_stdin = Some(stdin),
            None => return cleanup_err("sandbox stdin not available".to_owned(), &mut startup),
        }
        test_startup_delay("after-bwrap").await;
        // Wait for dbus-ready marker (entrypoint touches it after dbus-daemon starts)
        let dbus_ready_path = host_xdg_dir.join("dbus-ready");
        self.set_start_stage("waiting for the container D-Bus daemon");
        if let Err(e) = wait_for_socket(
            &dbus_ready_path,
            "dbus-ready marker",
            std::time::Instant::now() + STARTUP_TIMEOUT,
        ).await {
            return cleanup_err(e, &mut startup);
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
                Err(e) => return cleanup_err(e, &mut startup),
            };
        // Claim org.kde.KWin on proxy_conn (before KWin starts, so we get it first)
        if let Err(e) = proxy_conn.request_name("org.kde.KWin").await {
            return cleanup_err(format!("claim org.kde.KWin: {e}"), &mut startup);
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
            return cleanup_err(format!("register input devices: {e}"), &mut startup);
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
                Err(e) => return cleanup_err(e, &mut startup),
            };

        // Wait for KWin's wayland-0 socket to appear (proves KWin is running)
        let wayland_socket = host_xdg_dir.join("wayland-0");
        self.set_start_stage("waiting for KWin's Wayland socket");
        if let Err(e) = wait_for_socket(
            &wayland_socket,
            "wayland-0 socket",
            std::time::Instant::now() + STARTUP_TIMEOUT,
        ).await {
            return cleanup_err(e, &mut startup);
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
                return cleanup_err("could not discover KWin unique name".to_owned(), &mut startup);
            }
            tokio::time::sleep(STARTUP_POLL).await;
        }
        eprintln!("session_start: KWin unique name = {kwin_unique_name}");

        // Connect to KWin EIS using its unique name
        self.set_start_stage("connecting to KWin EIS input");
        let eis_builder = KWinEisProxy::builder(&kwin_conn)
            .destination(kwin_unique_name.as_str())
            .map_err(|e| ver_err(format!("EIS proxy builder: {e}")))?;
        let eis_proxy = match eis_builder.build().await {
            Ok(p) => p,
            Err(e) => return cleanup_err(format!("KWin EIS proxy: {e}"), &mut startup),
        };
        let (eis_fd, _cookie) = match eis_proxy.connect_to_eis(EIS_CAPS_KBD_POINTER).await {
            Ok(r) => r,
            Err(e) => return cleanup_err(format!("connectToEIS: {e}"), &mut startup),
        };
        eprintln!("session_start: EIS fd received, negotiating");
        let eis_owned_fd = std::os::fd::OwnedFd::from(eis_fd);
        let eis = match tokio::task::spawn_blocking(move || Eis::from_fd(eis_owned_fd)).await {
            Ok(Ok(eis)) => eis,
            Ok(Err(e)) => return cleanup_err(format!("EIS negotiation: {e}"), &mut startup),
            Err(e) => return cleanup_err(format!("EIS task: {e}"), &mut startup),
        };
        eprintln!("session_start: EIS ready");

        let atspi_bus_address = atspi::proxy::bus::BusProxy::new(&kwin_conn)
            .await
            .map_err(KwinError::from)?
            .get_address()
            .await
            .map_err(KwinError::from)?;

        // Chromium's ATK bridge stays dormant while org.a11y.Status reports
        // accessibility disabled — Chrome then registers on the AT-SPI bus but
        // exposes zero children. Non-fatal: Qt apps are unaffected either way
        // (QT_LINUX_ACCESSIBILITY_ALWAYS_ON is exported in the entrypoint).
        if let Err(e) = kwin_conn.call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &("org.a11y.Status", "IsEnabled", zbus::zvariant::Value::from(true)),
        ).await {
            eprintln!("session_start: enabling org.a11y.Status failed (Chromium AT-SPI trees will be empty): {e}");
        }

        let bus_name = kwin_conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let workdir = host_xdg_dir.display().to_string();
        let msg = format!("{version_stamp} — session started bus={bus_name} kwin={kwin_unique_name} display={screen_w}x{screen_h}");
        self.set_start_stage("starting the host viewer");
        let mut viewer_resources = StartupViewerResources::new();
        let mut viewer_unavailable = None;
        if self.display.viewer_enabled {
            match spawn_viewer(&host_xdg_dir, screen_w, screen_h).await {
                Ok(child) => viewer_resources.viewer = Some(child),
                Err(reason) => viewer_unavailable = Some(reason),
            }
        } else {
            eprintln!("session_start: viewer disabled (--no-viewer)");
            viewer_unavailable = Some("disabled by --no-viewer; call viewer_open to show it".to_owned());
        }
        test_startup_delay("after-viewer").await;
        let viewer = match viewer_resources.viewer.as_mut() {
            Some(child) => wait_for_viewer(&host_xdg_dir, child).await,
            None => viewer_report(&host_xdg_dir, None, viewer_unavailable.as_deref()),
        };
        let msg = format!("{msg} {}", viewer_summary(&viewer));
        let socket_links = std::mem::take(&mut overlay_plan.socket_links);
        let overlay_work_paths = overlay_plan.overlays.iter()
            .map(|overlay| overlay.work.join("work"))
            .collect();
        let mut guard = self.session.lock().await;
        let viewer_child = viewer_resources.into_child();
        let (sandbox_child, sandbox_stdin, service_proxy_children) = startup.into_parts();
        *guard = Some(Session {
            kwin_conn,
            _proxy_conn: proxy_conn,
            kwin_unique_name: kwin_unique_name.clone(),
            service_bus_address,
            atspi_bus_address,
            eis,
            sandbox_child,
            sandbox_stdin,
            host_xdg_dir,
            _uinput_mouse: uinput_mouse,
            _uinput_keyboard: uinput_keyboard,
            cdp_browser: None,
            cdp_forward_port,
            service_proxy_children,
            viewer_child,
            viewer_unavailable,
            overlay_work_paths,
            _socket_links: socket_links,
            screen_width: screen_w,
            screen_height: screen_h,
            last_activity: std::time::Instant::now(),
            last_input: None,
            fuse_enabled: fuse.is_some(),
            wallet: Some(wallet),
        });
        Ok(structured_result(&peer, msg, serde_json::json!({
            "status": "started",
            "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
            "commit": env!("GIT_HASH"),
            "bus": bus_name,
            "kwin_unique": kwin_unique_name,
            "workdir": workdir,
            "width": screen_w,
            "height": screen_h,
            "viewer": viewer,
            "wallet": {
                "forwarding": wallet_preflight.wallet.is_some(),
                "reason": wallet_preflight.reason,
            },
        })).await)
    }

    #[rmcp::tool(
        name = "viewer_open",
        description = "Open the live viewer window for the current session on the user's desktop, so the user can watch or review what the session shows. Reuses the viewer that session_start opens; if one is already running it is left as is. Returns the viewer outcome: ready (a host window is showing the session), starting, or unavailable with the reason (for example no host Wayland session on a remote host). Works even when the server runs with --no-viewer."
    )]
    async fn viewer_open(&self, peer: rmcp::Peer<rmcp::RoleServer>) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let (host_xdg_dir, width, height) = {
            let mut guard = self.session.lock().await;
            let sess = guard.as_mut().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            let running = match sess.viewer_child.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(None)),
                None => false,
            };
            if running {
                let viewer = viewer_report(&sess.host_xdg_dir, sess.viewer_child.as_mut(), None);
                let message = format!("viewer already open: {}", viewer_summary(&viewer));
                drop(guard);
                return Ok(structured_result(&peer, message, serde_json::json!({"status": "already_open", "viewer": viewer})).await);
            }
            (sess.host_xdg_dir.clone(), sess.screen_width, sess.screen_height)
        };
        let spawned = spawn_viewer(&host_xdg_dir, width, height).await;
        let mut guard = self.session.lock().await;
        let Some(sess) = guard.as_mut().filter(|sess| sess.host_xdg_dir == host_xdg_dir) else {
            // The session stopped while the viewer was starting.
            if let Ok(child) = spawned {
                terminate_child(child, false, "orphaned viewer");
            }
            return Err(McpError::internal_error("session stopped while opening the viewer", None));
        };
        match spawned {
            Ok(child) => {
                if let Some(old) = sess.viewer_child.replace(child) {
                    terminate_child(old, false, "exited viewer");
                }
                sess.viewer_unavailable = None;
            }
            Err(reason) => sess.viewer_unavailable = Some(reason),
        }
        let mut child = sess.viewer_child.take();
        let unavailable = sess.viewer_unavailable.clone();
        drop(guard);
        let viewer = match child.as_mut() {
            Some(child) if unavailable.is_none() => wait_for_viewer(&host_xdg_dir, child).await,
            _ => viewer_report(&host_xdg_dir, None, unavailable.as_deref()),
        };
        let mut guard = self.session.lock().await;
        match guard.as_mut().filter(|sess| sess.host_xdg_dir == host_xdg_dir) {
            Some(sess) => sess.viewer_child = child,
            None => {
                if let Some(child) = child {
                    terminate_child(child, false, "orphaned viewer");
                }
            }
        }
        drop(guard);
        let status = if viewer["state"] == "unavailable" { "unavailable" } else { "opened" };
        Ok(structured_result(&peer, format!("viewer {status}: {}", viewer_summary(&viewer)), serde_json::json!({"status": status, "viewer": viewer})).await)
    }

    #[rmcp::tool(
        name = "session_stop",
        description = "Tear down the current session and its container processes. With --autoclean or --ttl, remove the session workdir; if removal fails, calling session_stop again retries it. Transport shutdown also cleans up. No-op if no session is running and nothing is left to clean.",
        annotations(destructive_hint = true)
    )]
    async fn session_stop(&self, peer: rmcp::Peer<rmcp::RoleServer>) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        // Serialize stop with the entire start attempt, including workdir
        // creation and failed-start cleanup. Without this gate, stop can see no
        // published Session, delete a workdir that startup is still using, and
        // release cleanup ownership while start continues against the path.
        let _start_gate = self
            .lifecycle_gate(tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT, "session_stop")
            .await?;
        let stopped = self.session.lock().await.take();
        let had_session = stopped.is_some();
        if let Some(sess) = stopped {
            teardown_blocking(sess).await;
        }
        let dir = match self.workdir.remove() {
            WorkdirCleanup::NothingOwned => {
                return Ok(if had_session {
                    structured_result(&peer, "session stopped", serde_json::json!({"status": "stopped"})).await
                } else {
                    structured_result(&peer, "no session running", serde_json::json!({"status": "none"})).await
                });
            }
            WorkdirCleanup::Removed(dir) => dir,
            WorkdirCleanup::Retained { dir, error } => {
                // The Session is already torn down, so name that too: cleanup
                // ownership is the only state left, and another session_stop is
                // its retry.
                let stopped_note = if had_session { "session stopped, but " } else { "" };
                return Err(McpError::internal_error(
                    format!(
                        "{stopped_note}session workdir {} not removed: {error}. Cleanup is still owned, call session_stop again to retry.",
                        dir.display()
                    ),
                    None,
                ));
            }
        };
        let workdir = dir.display().to_string();
        let (status, message) = if had_session {
            ("stopped", format!("session stopped, workdir {workdir} removed"))
        } else {
            ("cleaned", format!("no session running, leftover workdir {workdir} removed"))
        };
        Ok(structured_result(&peer, message, serde_json::json!({
            "status": status,
            "workdir_removed": workdir,
        })).await)
    }

    #[rmcp::tool(
        name = "window_list",
        description = "List every managed app window in the isolated session, ordered from topmost to bottommost. Includes IDs, titles, app classes, geometry, active/minimized state, and modal/transient relationships. Call this whenever an expected prompt or transition is not visible in screenshot; a fully covered window still appears here. Use window_activate with the returned ID to reveal and inspect it.",
        annotations(read_only_hint = true)
    )]
    async fn window_list(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        let windows = window_snapshots(&conn, &kwin_unique, &xdg).await?;
        let text = if windows.is_empty() {
            "no managed windows".to_owned()
        } else {
            windows
                .iter()
                .map(WindowSnapshot::summary)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let count = windows.len();
        Ok(structured_result(
            &peer,
            text,
            serde_json::json!({"count": count, "windows": windows}),
        )
        .await)
    }

    #[rmcp::tool(
        name = "window_activate",
        description = "Reveal a specific isolated-session window by exact ID from window_list: unminimize it, raise it above covering windows, and give it focus. Then call screenshot to render it and use the normal input tools. This replaces blind Alt+Tab cycling when a modal, prompt, or app window is hidden."
    )]
    async fn window_activate(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<WindowActivateParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        let window = activate_window(&conn, &kwin_unique, &xdg, &params.window_id).await?;
        self.mark_input().await;
        let text = format!("activated {}", window.summary());
        Ok(structured_result(
            &peer,
            text,
            serde_json::json!({"status": "activated", "window": window}),
        )
        .await)
    }

    // Alpha-blends the high-visibility cursor sprite onto an RGBA buffer so that
    // the arrow tip lands exactly at (cx, cy). The sprite is the raw rasterized
    // PNG (with transparent padding and soft shadow); we shift its origin by the
    // hardcoded CURSOR_HOTSPOT_* offsets instead of cropping at runtime.
    fn overlay_cursor(rgba: &mut [u8], img_w: u32, img_h: u32, cx: i32, cy: i32) {
        let Some(sprite) = cursor_sprite() else { return };
        let origin_x = cx - CURSOR_HOTSPOT_X;
        let origin_y = cy - CURSOR_HOTSPOT_Y;
        for dy in 0..sprite.h {
            let Ok(dy_i) = i32::try_from(dy) else { continue };
            let py = origin_y + dy_i;
            if py < 0 { continue; }
            let Ok(py_u) = u32::try_from(py) else { continue };
            if py_u >= img_h { continue; }
            for dx in 0..sprite.w {
                let Ok(dx_i) = i32::try_from(dx) else { continue };
                let px = origin_x + dx_i;
                if px < 0 { continue; }
                let Ok(px_u) = u32::try_from(px) else { continue };
                if px_u >= img_w { continue; }
                let Some(s_lin) = dy.checked_mul(sprite.w).and_then(|v| v.checked_add(dx)).and_then(|v| v.checked_mul(4)) else { continue };
                let Some(d_lin) = py_u.checked_mul(img_w).and_then(|v| v.checked_add(px_u)).and_then(|v| v.checked_mul(4)) else { continue };
                let Ok(s) = usize::try_from(s_lin) else { continue };
                let Ok(d) = usize::try_from(d_lin) else { continue };
                if s + 3 >= sprite.rgba.len() || d + 3 >= rgba.len() { continue; }
                let a = u32::from(sprite.rgba[s + 3]);
                if a == 0 { continue; }
                let inv = 255 - a;
                for c in 0..3 {
                    let src_c = u32::from(sprite.rgba[s + c]);
                    let dst_c = u32::from(rgba[d + c]);
                    rgba[d + c] = u8::try_from((src_c * a + dst_c * inv) / 255).unwrap_or(255);
                }
                rgba[d + 3] = 255;
            }
        }
    }

    #[rmcp::tool(
        name = "screenshot",
        description = "Capture the active window as a PNG written to the session workdir. Pixels are 1:1 with the display (no scaling) and the image is window-relative, the same space as mouse input: pixel (x,y) of a default screenshot is mouse_click (x,y), including for a non-maximized dialog at any screen position. The metadata names the captured window (id, title, display x/y, size) and region=[x1,y1,x2,y2], the window-relative rectangle the image covers; for any crop, image pixel (px,py) is mouse_click (px+x1, py+y1). Use this when you need to see what the UI looks like, verify a state change visually, or read text/images the accessibility tree can't expose. Pass cursor=true to get an image of the region centered on the cursor. Use this after clicking to verify the click landed: a screenshot within 1.5s of an input tool waits until the screen has been unchanged for 200ms, so it reflects input the app has already handled (metadata settle.settled=false means the screen was still changing, e.g. an animation). Pass region=[x1,y1,x2,y2] in window-relative pixels to crop (it may extend past the window, e.g. negative values to include a popup outside a small dialog) — prefer cropping over full captures when you already know which area matters, it returns a much smaller file. region and cursor are mutually exclusive. Pass inline=true to get the PNG returned directly in the tool result (base64) so you can see it immediately without a separate file read; the file on disk is written either way. Requires an open app (call launch_app first if needed).",
        annotations(read_only_hint = true)
    )]
    async fn screenshot(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        if params.cursor && params.region.is_some() {
            return Err(McpError::invalid_params("region and cursor are mutually exclusive", None));
        }
        let (win_x, win_y, win_geo) = active_window_info(&conn, &kwin_unique, &xdg).await?;
        #[expect(clippy::as_conversions)]
        let (win_w, win_h) = (win_geo.w.round() as i32, win_geo.h.round() as i32);
        // Every coordinate here is window-relative, matching mouse input: by
        // default the image is the active window itself, so image pixel (x,y)
        // is mouse_click (x,y). A region or cursor crop may extend past the
        // window edges (e.g. to include a popup) and is clamped to the screen.
        let region = if params.cursor {
            #[expect(clippy::as_conversions)]
            let (cx, cy) = (win_geo.cx.round() as i32 - win_x, win_geo.cy.round() as i32 - win_y);
            [
                cx - CURSOR_ZOOM_HALF_EDGE,
                cy - CURSOR_ZOOM_HALF_EDGE,
                cx + CURSOR_ZOOM_HALF_EDGE,
                cy + CURSOR_ZOOM_HALF_EDGE,
            ]
        } else {
            params.region.unwrap_or([0, 0, win_w, win_h])
        };
        let proxy = KWinScreenShot2Proxy::builder(&conn)
            .destination(kwin_unique.as_str())
            .map_err(KwinError::from)?
            .build()
            .await
            .map_err(KwinError::from)?;
        let (width, height, stride, pixels, settle) = capture_settled_frame(&proxy, self.last_input().await).await?;
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
        // Window-relative region -> clamped display rectangle.
        let [x1, y1, x2, y2] = region;
        let clamp_x = |v: i32| u32::try_from(v.saturating_add(win_x).max(0)).unwrap_or(0).min(width);
        let clamp_y = |v: i32| u32::try_from(v.saturating_add(win_y).max(0)).unwrap_or(0).min(height);
        let (cx1, cy1, cx2, cy2) = (clamp_x(x1), clamp_y(y1), clamp_x(x2), clamp_y(y2));
        let cw = cx2.saturating_sub(cx1);
        let ch = cy2.saturating_sub(cy1);
        if cw == 0 || ch == 0 {
            return Err(McpError::invalid_params(
                format!("region [{x1},{y1},{x2},{y2}] (window-relative) has zero area on screen"),
                None,
            ));
        }
        let mut out_rgba = vec![0u8; usize::try_from(cw * ch * 4).map_err(KwinError::from)?];
        for row in 0..ch {
            let src = usize::try_from((cy1 + row) * width * 4 + cx1 * 4).map_err(KwinError::from)?;
            let dst = usize::try_from(row * cw * 4).map_err(KwinError::from)?;
            let len = usize::try_from(cw * 4).map_err(KwinError::from)?;
            out_rgba[dst..dst + len].copy_from_slice(&rgba[src..src + len]);
        }
        let (out_w, out_h) = (cw, ch);
        // Image pixel (0,0) in window-relative input coordinates.
        let origin_x = i32::try_from(cx1).map_err(KwinError::from)? - win_x;
        let origin_y = i32::try_from(cy1).map_err(KwinError::from)? - win_y;
        let out_region = [origin_x, origin_y, origin_x + i32::try_from(cw).map_err(KwinError::from)?, origin_y + i32::try_from(ch).map_err(KwinError::from)?];
        // Overlay the high-visibility cursor; its position is absolute on the
        // screen, so shift it into the output frame by the crop's display origin.
        #[expect(clippy::as_conversions)]
        let cursor_abs_x = win_geo.cx.round() as i32;
        #[expect(clippy::as_conversions)]
        let cursor_abs_y = win_geo.cy.round() as i32;
        let crop_ox_i = i32::try_from(cx1).unwrap_or(0);
        let crop_oy_i = i32::try_from(cy1).unwrap_or(0);
        Self::overlay_cursor(&mut out_rgba, out_w, out_h, cursor_abs_x - crop_ox_i, cursor_abs_y - crop_oy_i);
        let path = xdg.join("screenshot.png");
        let mut png_bytes: Vec<u8> = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, out_w, out_h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(KwinError::from)?;
            writer.write_image_data(&out_rgba).map_err(KwinError::from)?;
        }
        // Never leave a partial or stale capture at the path: on failure the
        // previous screenshot is removed too, so the file is always this call's.
        let write_error = write_atomically(&path, &png_bytes).err().map(|error| {
            let _ = std::fs::remove_file(&path);
            describe_write_failure(&path, &error)
        });
        if let Some(error) = &write_error {
            eprintln!("screenshot: {error}");
            if !params.inline {
                return Err(McpError::internal_error(
                    format!("screenshot captured but not saved: {error}. Pass inline=true to receive the image without writing it."),
                    Some(serde_json::json!({"reason": "write_failed"})),
                ));
            }
        }
        let path_str = path.to_string_lossy().to_string();
        let mut payload = serde_json::json!({
            "path": if write_error.is_some() { serde_json::Value::Null } else { serde_json::json!(path_str) },
            "width": out_w,
            "height": out_h,
            // Window-relative rectangle the image covers: image pixel (px,py)
            // is mouse_click (px + region[0], py + region[1]).
            "region": out_region,
            "window": {
                "id": win_geo.id,
                "title": win_geo.title,
                "x": win_x,
                "y": win_y,
                "width": win_w,
                "height": win_h,
            },
        });
        if let Some(settle) = &settle {
            payload["settle"] = serde_json::json!({
                "settled": settle.settled,
                "waited_ms": u64::try_from(settle.waited.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if let Some(error) = &write_error {
            payload["write_error"] = serde_json::json!(error);
        }
        let text = match &write_error {
            None => format!("{path_str} size={out_w}x{out_h}"),
            Some(error) => format!("inline only, file not saved ({error}) size={out_w}x{out_h}"),
        };
        if params.inline {
            // Claude Code's MCP client hides content[] when structured_content is also
            // set, so we can't return both the image AND a structured field at the
            // same time. Instead, serialize the payload into a text block so the
            // image renders AND the machine-readable data is still recoverable by
            // parsing that block as JSON.
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
            let payload_text = serde_json::to_string(&payload).unwrap_or_else(|_| text.clone());
            let _ = peer.notify_logging_message(rmcp::model::LoggingMessageNotificationParam::new(
                rmcp::model::LoggingLevel::Info,
                serde_json::json!(text),
            )).await;
            Ok(CallToolResult::success(vec![
                Content::text(text),
                Content::text(payload_text),
                Content::image(b64, "image/png"),
            ]))
        } else {
            Ok(structured_result(&peer, text, payload).await)
        }
    }

    #[rmcp::tool(
        name = "accessibility_tree",
        description = "Dump the active app's widget hierarchy when structural context is needed. Trees can be large or empty if an app exposes no accessibility nodes; use screenshot and input in that case. Prefer find_ui_elements for one named control. app_name filters top-level apps; max_depth caps traversal (default 8); role filters role names. show_elements=true keeps zero-rect and unnamed nodes; default false trims them out.",
        annotations(read_only_hint = true)
    )]
    async fn accessibility_tree(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<AccessibilityTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
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
        description = "Search the active app for widgets whose name or role contains query (case-insensitive). Returns each match's role, text, and bounding box for mouse_click/mouse_move. Use this compact result for a known control; if no useful match appears, inspect a screenshot and continue with visual input. Use a filtered accessibility_tree only when you need structure.",
        annotations(read_only_hint = true)
    )]
    async fn find_ui_elements(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<FindUiElementsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
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
        self.touch_activity().await;
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
        tokio::time::sleep(MOVE_TO_CLICK_DELAY).await;
        for n in 0..count {
            if n > 0 {
                tokio::time::sleep(INPUT_EVENT_DELAY).await;
            }
            sess.eis.button(code, true).map_err(KwinError::from)?;
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
            sess.eis.button(code, false).map_err(KwinError::from)?;
        }
        drop(guard);
        self.mark_input().await;
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
        self.touch_activity().await;
        let x = params.x;
        let y = params.y;
        let (wx, wy, _) = active_window_info(&self.kwin_conn().await?, &self.kwin_unique_name().await?, &self.host_xdg_dir().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (f32::from(i16::try_from(wx + x).map_err(KwinError::from)?), f32::from(i16::try_from(wy + y).map_err(KwinError::from)?));
        sess.eis.move_abs(ax, ay).map_err(KwinError::from)?;
        drop(guard);
        self.mark_input().await;
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
        self.touch_activity().await;
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
        drop(guard);
        self.mark_input().await;
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
        self.touch_activity().await;
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
        drop(guard);
        self.mark_input().await;
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
        self.touch_activity().await;
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
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(&peer, format!("typed: {}", params.text), serde_json::json!({
            "action": "type", "text": params.text,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_key",
        description = "Press a key or combo in the focused window. Combos use modifier+key syntax (ctrl+shift+t, alt+F4, ctrl+minus); modifiers are ctrl, shift, alt, super. Keys are a single character or a name: Return, Escape, Tab, Space, Backspace, Delete, Home, End, PageUp, PageDown, arrows (Up/Down/Left/Right), F1-F12, NumLock, and punctuation names minus, plus, equal, comma, period, slash, backslash, semicolon, apostrophe, grave, bracketleft, bracketright. A combo that does not fully resolve fails with invalid_params and sends nothing. Use keyboard_type for text."
    )]
    async fn keyboard_key(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        for m in &mods {
            sess.eis.key(*m, true).map_err(KwinError::from)?;
        }
        if !mods.is_empty() {
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
        }
        let k = main;
        sess.eis.key(k, true).map_err(KwinError::from)?;
        tokio::time::sleep(INPUT_EVENT_DELAY).await;
        sess.eis.key(k, false).map_err(KwinError::from)?;
        if !mods.is_empty() {
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
        }
        for m in mods.iter().rev() {
            sess.eis.key(*m, false).map_err(KwinError::from)?;
        }
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(&peer, format!("key: {}", params.key), serde_json::json!({
            "action": "key", "key": params.key,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_press",
        description = "Press a key or combo WITHOUT releasing — useful when you need to hold the chord across a screenshot to verify a transient UI (e.g. a menu that opens on combo). Call keyboard_release with the same combo afterwards to release.")]
    async fn keyboard_press(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        for m in &mods {
            sess.eis.key(*m, true).map_err(KwinError::from)?;
        }
        if !mods.is_empty() {
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
        }
        let k = main;
        sess.eis.key(k, true).map_err(KwinError::from)?;
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(&peer, format!("press: {}", params.key), serde_json::json!({
            "action": "press", "key": params.key,
        })).await)
    }

    #[rmcp::tool(
        name = "keyboard_release",
        description = "Release a key or combo previously pressed via keyboard_press. Pass the same string.")]
    async fn keyboard_release(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        let k = main;
        sess.eis.key(k, false).map_err(KwinError::from)?;
        if !mods.is_empty() {
            tokio::time::sleep(INPUT_EVENT_DELAY).await;
        }
        for m in mods.iter().rev() {
            sess.eis.key(*m, false).map_err(KwinError::from)?;
        }
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(&peer, format!("release: {}", params.key), serde_json::json!({
            "action": "release", "key": params.key,
        })).await)
    }

    #[rmcp::tool(
        name = "export_file",
        description = "Copy a file out of the isolated session onto the real host filesystem and verify it. Session writes (downloads, exports, saved files) stay in the session overlay and never reach the host on their own; call this when the user wants the file in a real directory. session_path is the absolute path the session sees (e.g. a finished Chrome download in ~/Downloads). host_path is the host file or existing directory to write; omit it to use the same path on the host. An existing host file is not replaced unless overwrite=true. Returns the host path and byte count after a byte-for-byte comparison, or an error saying why nothing was written (including a download still in progress)."
    )]
    async fn export_file(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ExportFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let sandbox_pid = self.with_session(|sess| Ok(sess.sandbox_child.id())).await?;
        let session_path = PathBuf::from(&params.session_path);
        if !session_path.is_absolute() {
            return Err(McpError::invalid_params(format!("session_path must be absolute: {}", params.session_path), None));
        }
        let mut destination = PathBuf::from(params.host_path.as_deref().unwrap_or(&params.session_path));
        if !destination.is_absolute() {
            return Err(McpError::invalid_params(format!("host_path must be absolute: {}", destination.display()), None));
        }
        if destination.is_dir() {
            let name = session_path.file_name().ok_or_else(|| McpError::invalid_params("session_path has no file name", None))?;
            destination = destination.join(name);
        }
        let overwrite = params.overwrite;
        let outcome = tokio::task::spawn_blocking(move || {
            let root = sandbox_root(sandbox_pid).ok_or_else(|| "cannot reach the session filesystem (sandbox not running)".to_owned())?;
            let relative = session_path.strip_prefix("/").map_err(|error| error.to_string())?;
            let source = root.join(relative);
            if !source.exists() {
                for partial in ["crdownload", "part", "download"] {
                    let name = format!("{}.{partial}", session_path.file_name().and_then(|name| name.to_str()).unwrap_or_default());
                    if source.with_file_name(&name).exists() {
                        return Err(format!("{} is still downloading ({name} exists); wait for the download to finish", session_path.display()));
                    }
                }
                return Err(format!("{} does not exist in the session", session_path.display()));
            }
            export_session_file(&source, &destination, overwrite).map(|bytes| (destination, bytes))
        })
        .await
        .map_err(|error| McpError::internal_error(format!("export task: {error}"), None))?;
        match outcome {
            Ok((destination, bytes)) => {
                let text = format!("exported {} -> {} ({bytes} bytes, verified)", params.session_path, destination.display());
                Ok(structured_result(&peer, text, serde_json::json!({
                    "status": "exported",
                    "session_path": params.session_path,
                    "host_path": destination.display().to_string(),
                    "bytes": bytes,
                    "verified": true,
                })).await)
            }
            Err(reason) => Err(McpError::internal_error(
                format!("export_file wrote nothing: {reason}"),
                Some(serde_json::json!({"status": "not_exported", "reason": reason})),
            )),
        }
    }

    #[rmcp::tool(
        name = "launch_app",
        description = "Run a shell command in the isolated desktop and wait up to 15s for a new window. Browser names resolved through PATH get Wayland, KWallet, and accessibility switches unless already set; Chromium-family programs also get CDP. Google Chrome and Edge block CDP on their default profile. Writes under the isolated HOME stay in the session overlay; export_file copies one to the host."
    )]
    async fn launch_app(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<LaunchAppParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        use std::io::Write;
        use futures::StreamExt;

        // Record current active window ID before launching
        let (conn, kwin_unique, xdg, service_bus_address, atspi_bus_address, cdp_port, fuse_enabled) = {
            let guard = self.session.lock().await;
            let sess = guard.as_ref().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            (
                sess.kwin_conn.clone(),
                sess.kwin_unique_name.clone(),
                sess.host_xdg_dir.clone(),
                sess.service_bus_address.clone(),
                sess.atspi_bus_address.clone(),
                sess.cdp_forward_port,
                sess.fuse_enabled,
            )
        };
        let prev_window_id = active_window_info(&conn, &kwin_unique, &xdg).await
            .map(|(_, _, geo)| geo.id)
            .ok();

        let browser_marker = xdg.join(format!(
            "browser-launch-{}",
            NEXT_BROWSER_LAUNCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let launch_cmd = format!(
            "for x_socket in /tmp/.X11-unix/X*; do if [ -S \"$x_socket\" ]; then export DISPLAY=\":${{x_socket##*X}}\"; break; fi; done; env {}PATH={}:\"$PATH\" DBUS_SESSION_BUS_ADDRESS={} AT_SPI_BUS_ADDRESS={} KWIN_MCP_CDP_PORT={cdp_port} KWIN_MCP_BROWSER_MARKER={} bash -c {}",
            if fuse_enabled { "" } else { "APPIMAGE_EXTRACT_AND_RUN=1 " },
            shell_quote(&xdg.join("browser-bin").display().to_string()),
            shell_quote(&service_bus_address),
            shell_quote(&atspi_bus_address),
            shell_quote(&browser_marker.display().to_string()),
            shell_quote(&params.command),
        );
        {
            let mut guard = self.session.lock().await;
            let sess = guard.as_mut().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            writeln!(sess.sandbox_stdin, "{launch_cmd}").map_err(KwinError::from)?;
            sess.sandbox_stdin.flush().map_err(KwinError::from)?;
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

        // The browser wrapper records CDP intent only when it actually ran.
        let cdp_requested = std::fs::read(&browser_marker).is_ok_and(|value| value == b"cdp");
        let _ = std::fs::remove_file(&browser_marker);
        let mut cdp_connected = false;
        if cdp_requested && win_geo.is_some() {
            let cdp_url = format!("http://127.0.0.1:{cdp_port}");
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

fn parse_cli_args() -> Result<DisplayConfig, String> {
    let mut cfg = DisplayConfig {
        width: VIRTUAL_SCREEN_WIDTH,
        height: VIRTUAL_SCREEN_HEIGHT,
        locked: false,
        viewer_enabled: true,
        autoclean: false,
        ttl: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--width" => cfg.width = parse_dim_arg(&mut args, "--width")?,
            "--height" => cfg.height = parse_dim_arg(&mut args, "--height")?,
            "--no-override" => cfg.locked = true,
            "--no-viewer" => cfg.viewer_enabled = false,
            "--autoclean" => cfg.autoclean = true,
            "--ttl" => cfg.ttl = Some(parse_ttl_arg(&mut args)?),
            other => {
                return Err(format!(
                    "unknown argument '{other}': usage: kwin-mcp [--width N] [--height N] [--no-override] [--no-viewer] [--autoclean] [--ttl MINUTES]"
                ))
            }
        }
    }
    // Expiry owns the same workdir terminal transition as --autoclean.
    cfg.autoclean |= cfg.ttl.is_some();
    Ok(cfg)
}

fn parse_ttl_arg(args: &mut impl Iterator<Item = String>) -> Result<Duration, String> {
    let value = args.next().ok_or_else(|| "--ttl requires minutes".to_owned())?;
    let minutes: u64 = value.parse().map_err(|error| format!("--ttl '{value}': {error}"))?;
    if minutes == 0 { return Err("--ttl must be positive".to_owned()); }
    let seconds = minutes.checked_mul(60).ok_or_else(|| "--ttl is too large".to_owned())?;
    Ok(Duration::from_secs(seconds))
}

fn parse_dim_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u32, String> {
    let v = args.next().ok_or_else(|| format!("{flag} requires a value"))?;
    let n: u32 = v.parse().map_err(|e| format!("{flag} '{v}': {e}"))?;
    if !(MIN_SCREEN_DIM..=MAX_SCREEN_DIM).contains(&n) {
        return Err(format!("{flag} {n} out of range {MIN_SCREEN_DIM}..={MAX_SCREEN_DIM}"));
    }
    Ok(n)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Inside the sandbox this binary also serves FUSE (see fuse_bridge).
    let mut argv = std::env::args();
    if let Some(program) = argv.next().as_deref().and_then(fuse_bridge::shim_program) {
        std::process::exit(fuse_bridge::run_client(program));
    }
    if argv.next().as_deref() == Some("--fuse-helper") {
        let socket = argv.next().unwrap_or_else(|| fuse_bridge::HELPER_SOCKET.to_owned());
        fuse_bridge::run_helper(Path::new(&socket))?;
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(run_server());
    // Tokio's stdio reader uses a blocking thread. A signal leaves stdin open,
    // so waiting indefinitely for that thread would keep the server alive
    // after its session and workdir have already been cleaned up.
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
    }
    let display = parse_cli_args()?;
    if display.autoclean
        && let Err(error) = sweep_orphaned_workdirs() {
            eprintln!("autoclean: orphan sweep skipped: {error}");
    }
    eprintln!(
        "kwin-mcp: display default {}x{}{}; viewer {}",
        display.width,
        display.height,
        if display.locked { " (locked, --no-override)" } else { "" },
        if display.viewer_enabled { "enabled" } else {
            "disabled (--no-viewer)"
        }
    );
    let kwin = KwinMcp::new(display);
    let shutdown = kwin.clone();
    // Inject the host's installed browsers into the launch_app description so the
    // agent knows what it can actually run without guessing (issue #28).
    let mut tool_router = KwinMcp::tool_router();
    let browsers = detect_browsers();
    eprintln!("kwin-mcp: detected browsers: {}", if browsers.is_empty() { "(none)".to_owned() } else { browsers.join(", ") });
    if let Some(route) = tool_router.map.get_mut("launch_app") {
        let hint = if browsers.is_empty() {
            "\n\nNo known browser was found on this host's PATH.".to_owned()
        } else {
            format!(
                "\n\nBrowsers installed on this host (runnable by these exact commands): {}. \
                 Only 'chromium' exposes CDP DOM queries on its default profile; the others still \
                 launch, screenshot, and accept input normally.",
                browsers.join(", ")
            )
        };
        let base = route.attr.description.take().map(std::borrow::Cow::into_owned).unwrap_or_default();
        route.attr.description = Some(std::borrow::Cow::Owned(base + &hint));
    }
    let router =
        rmcp::handler::server::router::Router::new(kwin).with_tools(tool_router);
    let transport = rmcp::transport::io::stdio();
    let service = router.serve(transport).await?;
    let ttl_reaper = tokio::spawn(shutdown.clone().idle_reaper());
    let cancellation = service.cancellation_token();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let waited = {
        let wait_future = service.waiting();
        tokio::pin!(wait_future);
        tokio::select! {
            result = &mut wait_future => Some(result),
            _ = sigterm.recv() => { eprintln!("shutdown: SIGTERM"); None },
            _ = sigint.recv() => { eprintln!("shutdown: SIGINT"); None },
            _ = sighup.recv() => { eprintln!("shutdown: SIGHUP"); None },
        }
    };
    if waited.is_none() { cancellation.cancel(); }
    ttl_reaper.abort();
    let _ = ttl_reaper.await;
    shutdown.shutdown_cleanup().await;
    if let Some(result) = waited { result?; }
    Ok(())
}
