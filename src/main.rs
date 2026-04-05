use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo, Implementation};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::ServiceExt;
use serde::Deserialize;
use std::sync::Arc;
use ashpd::desktop::remote_desktop::{RemoteDesktop, DeviceType, KeyState, Axis,
    NotifyKeyboardKeycodeOptions, NotifyPointerMotionAbsoluteOptions,
    NotifyPointerButtonOptions, NotifyPointerAxisDiscreteOptions,
    NotifyPointerAxisOptions};

type McpError = rmcp::ErrorData;

// Claude Code serializes numbers to strings — FlexInt accepts both.
// Implements JsonSchema so rmcp emits a proper schema instead of `true`.
#[derive(Debug, Clone)]
struct FlexInt(i32);
impl<'de> serde::Deserialize<'de> for FlexInt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(deserializer)?;
        match v {
            serde_json::Value::Number(n) => n.as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .map(FlexInt)
                .ok_or_else(|| serde::de::Error::custom("not an i32")),
            serde_json::Value::String(s) => s.parse::<i32>().map(FlexInt)
                .map_err(serde::de::Error::custom),
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Array(_) | serde_json::Value::Object(_) => Err(serde::de::Error::custom("expected integer or string")),
        }
    }
}
impl schemars::JsonSchema for FlexInt {
    fn schema_name() -> std::borrow::Cow<'static, str> { "FlexInt".into() }
    fn inline_schema() -> bool { true }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": ["integer", "string"], "description": "integer or string-encoded integer" })
    }
}

fn parse_int(v: FlexInt) -> i32 { v.0 }

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

fn btn_code(btn: Option<&str>) -> Result<i32, McpError> {
    match btn {
        Some("left") | None => Ok(0x110),
        Some("right") => Ok(0x111),
        Some("middle") => Ok(0x112),
        Some(bad) => Err(McpError::invalid_params(format!("unknown button '{bad}' — use left/right/middle"), None)),
    }
}


// ── Portal session ──────────────────────────────────────────────────────

struct PortalSession {
    rd: RemoteDesktop,
    session: ashpd::desktop::Session<RemoteDesktop>,
    stream_id: u32,
    pw_fd: std::os::fd::OwnedFd,
}

async fn portal_setup(zbus_conn: &zbus::Connection) -> anyhow::Result<PortalSession> {
    use ashpd::desktop::remote_desktop::{SelectDevicesOptions, StartOptions};
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType, CursorMode};
    use ashpd::desktop::CreateSessionOptions;
    let rd = RemoteDesktop::with_connection(zbus_conn.clone()).await
        .map_err(|e| anyhow::anyhow!("RemoteDesktop: {e}"))?;
    let session = rd.create_session(CreateSessionOptions::default()).await
        .map_err(|e| anyhow::anyhow!("create_session: {e}"))?;
    rd.select_devices(&session, SelectDevicesOptions::default().set_devices(DeviceType::Keyboard | DeviceType::Pointer)).await
        .map_err(|e| anyhow::anyhow!("select_devices: {e}"))?.response()
        .map_err(|e| anyhow::anyhow!("select_devices response: {e}"))?;
    let sc = Screencast::with_connection(zbus_conn.clone()).await
        .map_err(|e| anyhow::anyhow!("Screencast: {e}"))?;
    sc.select_sources(&session, SelectSourcesOptions::default()
        .set_sources(SourceType::Virtual | SourceType::Monitor)
        .set_cursor_mode(CursorMode::Embedded)).await
        .map_err(|e| anyhow::anyhow!("select_sources: {e}"))?.response()
        .map_err(|e| anyhow::anyhow!("select_sources response: {e}"))?;
    let started = rd.start(&session, None, StartOptions::default()).await
        .map_err(|e| anyhow::anyhow!("start: {e}"))?.response()
        .map_err(|e| anyhow::anyhow!("start response: {e}"))?;
    let stream_id = started.streams().first().map(|s| s.pipe_wire_node_id()).unwrap_or(0);
    let pw_fd = sc.open_pipe_wire_remote(&session, ashpd::desktop::screencast::OpenPipeWireRemoteOptions::default()).await
        .map_err(|e| anyhow::anyhow!("open_pipe_wire_remote: {e}"))?;
    eprintln!("portal: stream_id={stream_id} streams={}", started.streams().len());
    Ok(PortalSession { rd, session, stream_id, pw_fd })
}

// ── KWin ScreenShot2 typed proxy ─────────────────────────────────────────

#[zbus::proxy(
    interface = "org.kde.KWin.ScreenShot2",
    default_service = "org.kde.KWin",
    default_path = "/org/kde/KWin/ScreenShot2"
)]
trait KWinScreenShot2 {
    #[zbus(name = "CaptureWindow")]
    fn capture_window(&self, window_id: &str, options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>, pipe_fd: zbus::zvariant::OwnedFd) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
}

// ── Session ──────────────────────────────────────────────────────────────

struct Session { portal: PortalSession, zbus_conn: zbus::Connection }

// ── Server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct KwinMcp { session: Arc<tokio::sync::Mutex<Option<Session>>> }

impl KwinMcp {
    fn new() -> Self { Self { session: Arc::new(tokio::sync::Mutex::new(None)) } }
    async fn with_session<R>(&self, f: impl FnOnce(&Session) -> Result<R, McpError>) -> Result<R, McpError> {
        let guard = self.session.lock().await;
        match &*guard { Some(s) => f(s), None => Err(McpError::internal_error("no session — call session_start first", None)) }
    }
    async fn zbus_conn(&self) -> Result<zbus::Connection, McpError> {
        let guard = self.session.lock().await;
        match &*guard { Some(s) => Ok(s.zbus_conn.clone()), None => Err(McpError::internal_error("no session — call session_start first", None)) }
    }
}

fn eis_err(e: impl std::fmt::Display) -> McpError { McpError::internal_error(e.to_string(), None) }

fn teardown(sess: Session) {
    drop(sess.portal);
}


async fn active_window_info(conn: &zbus::Connection) -> Result<(i32, i32, String), McpError> {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(eis_err)?.as_millis();
    let marker = format!("kwin-mcp-{ts}");
    let cb_path = format!("/KWinMCP/{ts}");
    let our_name = conn.unique_name().ok_or_else(|| McpError::internal_error("no bus name", None))?.to_string();
    let script = format!("var w = workspace.activeWindow;\
        if (!w) {{ var wl = workspace.windowList(); for (var i = 0; i < wl.length; i++) {{ if (wl[i].normalWindow) {{ w = wl[i]; workspace.activeWindow = w; break; }} }} }}\
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
    if !registered { return Err(McpError::internal_error(format!("failed to register callback at {cb_path}"), None)); }
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
        return Err(McpError::internal_error(format!("KWin loadScript failed, id={script_id}"), None));
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
    let json = result.map_err(|_| McpError::internal_error("KWin script timed out", None))?
        .map_err(|_| McpError::internal_error("KWin callback channel closed", None))?;
    if json == "null" { return Err(McpError::internal_error("KWin script error: No active window", None)); }
    let v: serde_json::Value = serde_json::from_str(&json).map_err(eis_err)?;
    let x = v.get("x").and_then(|v| v.as_f64()).ok_or_else(|| McpError::internal_error("no x", None))?;
    let y = v.get("y").and_then(|v| v.as_f64()).ok_or_else(|| McpError::internal_error("no y", None))?;
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
    #[rmcp::tool(name = "session_start", description = "Connect to the running KDE Wayland session for GUI automation. Must be called before any other tool.")]
    async fn session_start(&self, Parameters(_params): Parameters<SessionStartParams>) -> Result<CallToolResult, McpError> {
        eprintln!("kwin-mcp v{} ({}) session_start", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"));
        let version_stamp = format!("kwin-mcp v{} ({})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"));
        let ver_err = |e: String| McpError::internal_error(format!("{version_stamp} — {e}"), None);
        {
            let mut guard = self.session.lock().await;
            if let Some(old) = (*guard).take() { teardown(old); }
        }
        let pid = std::process::id();
        let scrdir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
        std::fs::create_dir_all(&scrdir).map_err(|e| ver_err(e.to_string()))?;
        // Connect to the user's existing session bus
        let zbus_conn = zbus::Connection::session().await.map_err(|e| ver_err(e.to_string()))?;
        let bus_name = zbus_conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
        // Set up RemoteDesktop portal for input injection
        let portal = portal_setup(&zbus_conn).await.map_err(|e| ver_err(e.to_string()))?;
        let msg = format!("{version_stamp} — session started bus={bus_name}");
        let mut guard = self.session.lock().await;
        *guard = Some(Session { portal, zbus_conn });
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[rmcp::tool(name = "session_stop", description = "Stop the KWin session and clean up all processes.", annotations(destructive_hint = true))]
    async fn session_stop(&self) -> Result<CallToolResult, McpError> {
        let mut guard = self.session.lock().await;
        match (*guard).take() {
            Some(sess) => { teardown(sess); Ok(CallToolResult::success(vec![Content::text("session stopped")])) }
            None => Ok(CallToolResult::success(vec![Content::text("no session running")])),
        }
    }

    #[rmcp::tool(name = "screenshot", description = "Capture frame from the virtual output. Returns PNG path.", annotations(read_only_hint = true))]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let stream_id = sess.portal.stream_id;
        // Dup the fd so PipeWire can own it without consuming the portal's fd
        let owned_fd = sess.portal.pw_fd.try_clone().map_err(eis_err)?;
        drop(guard);
        let path = std::env::temp_dir().join(format!("kwin-mcp-{}.png", std::process::id()));
        let path_clone = path.clone();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), tokio::task::spawn_blocking(move || -> anyhow::Result<(u32, u32)> {
            let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, u32, u32)>();
            let mainloop = pipewire::main_loop::MainLoopRc::new(None)
                .map_err(|e| anyhow::anyhow!("pw mainloop: {e}"))?;
            let context = pipewire::context::ContextRc::new(&mainloop, None)
                .map_err(|e| anyhow::anyhow!("pw context: {e}"))?;
            let core = context.connect_fd_rc(owned_fd, None)
                .map_err(|e| anyhow::anyhow!("pw connect_fd: {e}"))?;
            let stream = pipewire::stream::StreamRc::new(core.clone(), "kwin-mcp-screenshot",
                pipewire::properties::properties! {
                    *pipewire::keys::MEDIA_TYPE => "Video",
                    *pipewire::keys::MEDIA_CATEGORY => "Capture",
                    *pipewire::keys::MEDIA_ROLE => "Screen",
                }).map_err(|e| anyhow::anyhow!("pw stream: {e}"))?;
            let loop_clone = mainloop.clone();
            let _listener = stream.add_local_listener::<()>()
                .param_changed(|_, _id, _user_data, _pod| {})
                .process(move |stream, _user_data| {
                    if let Some(mut buf) = stream.dequeue_buffer() {
                        if let Some(data) = buf.datas_mut().first_mut() {
                            let chunk = data.chunk();
                            let stride = u32::try_from(chunk.stride()).unwrap_or(0);
                            let w = stride / 4;
                            let h = chunk.size() / stride.max(1);
                            if let Some(slice) = data.data() {
                                let s = usize::try_from(stride).unwrap_or(0);
                                let wu = usize::try_from(w).unwrap_or(0);
                                let mut rgba = Vec::with_capacity(wu * usize::try_from(h).unwrap_or(0) * 4);
                                for row in slice.chunks(s) {
                                    for px in row.get(..wu * 4).unwrap_or_default().chunks_exact(4) {
                                        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                                    }
                                }
                                let _ = tx.send((rgba, w, h));
                            }
                        }
                        loop_clone.quit();
                    }
                })
                .register().map_err(|e| anyhow::anyhow!("pw listener: {e}"))?;
            stream.connect(
                libspa::utils::Direction::Input,
                Some(stream_id),
                pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
                &mut [],
            ).map_err(|e| anyhow::anyhow!("pw stream connect: {e}"))?;
            mainloop.run();
            let (rgba, width, height) = rx.recv().map_err(|e| anyhow::anyhow!("no frame: {e}"))?;
            let file = std::fs::File::create(&path_clone).map_err(|e| anyhow::anyhow!("create png: {e}"))?;
            let mut enc = png::Encoder::new(file, width, height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(|e| anyhow::anyhow!("png header: {e}"))?;
            writer.write_image_data(&rgba).map_err(|e| anyhow::anyhow!("png data: {e}"))?;
            Ok((width, height))
        })).await.map_err(|_| McpError::internal_error("screenshot timed out — no frame from virtual output", None))?.map_err(eis_err)?.map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!("{} size={}x{}", path.to_string_lossy(), result.0, result.1))]))
    }

    #[rmcp::tool(name = "accessibility_tree", description = "Get AT-SPI2 accessibility tree with widget roles, names, states, bounding boxes. By default hides zero-rect/internal nodes; set show_elements=true to include them.", annotations(read_only_hint = true))]
    async fn accessibility_tree(&self, Parameters(params): Parameters<AccessibilityTreeParams>) -> Result<CallToolResult, McpError> {
        use atspi::proxy::accessible::ObjectRefExt;
        let conn = atspi::AccessibilityConnection::new().await.map_err(eis_err)?;
        let root = conn.root_accessible_on_registry().await.map_err(eis_err)?;
        let limit = usize::try_from(params.max_depth.unwrap_or(8)).map_err(eis_err)?;
        let app_name = params.app_name.map(|s| s.to_lowercase());
        let role = params.role.map(|s| s.to_lowercase());
        let show_elements = params.show_elements.unwrap_or(false);
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
        Ok(CallToolResult::success(vec![Content::text(out.join("\n"))]))
    }

    #[rmcp::tool(name = "find_ui_elements", description = "Search UI elements by name/role/description (case-insensitive).", annotations(read_only_hint = true))]
    async fn find_ui_elements(&self, Parameters(params): Parameters<FindUiElementsParams>) -> Result<CallToolResult, McpError> {
        let _ = &params.query;
        self.with_session(|_sess| { Err(McpError::internal_error("AT-SPI2 search not yet implemented", None)) }).await
    }

    #[rmcp::tool(name = "mouse_click", description = "Click at window-relative pixel coordinates. button: left/right/middle. double/triple for multi-click.")]
    async fn mouse_click(&self, Parameters(params): Parameters<MouseClickParams>) -> Result<CallToolResult, McpError> {
        let x = parse_int(params.x); let y = parse_int(params.y);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?).await?;
        let code = btn_code(params.button.as_deref())?;
        let count = match (params.triple, params.double) {
            (Some(true), _) => 3, (_, Some(true)) => 2,
            (Some(false) | None, Some(false) | None) => 1,
        };
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let (ax, ay) = (f64::from(wx + x), f64::from(wy + y));
        sess.portal.rd.notify_pointer_motion_absolute(&sess.portal.session, sess.portal.stream_id, ax, ay, NotifyPointerMotionAbsoluteOptions::default()).await.map_err(eis_err)?;
        for n in 0..count {
            if n > 0 { tokio::time::sleep(std::time::Duration::from_millis(50)).await; }
            sess.portal.rd.notify_pointer_button(&sess.portal.session, code, KeyState::Pressed, NotifyPointerButtonOptions::default()).await.map_err(eis_err)?;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            sess.portal.rd.notify_pointer_button(&sess.portal.session, code, KeyState::Released, NotifyPointerButtonOptions::default()).await.map_err(eis_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text(format!("clicked ({x},{y}) x{count}"))]))
    }

    #[rmcp::tool(name = "mouse_move", description = "Move cursor to window-relative pixel coordinates. Triggers hover effects.", annotations(read_only_hint = true))]
    async fn mouse_move(&self, Parameters(params): Parameters<MouseMoveParams>) -> Result<CallToolResult, McpError> {
        let x = parse_int(params.x); let y = parse_int(params.y);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let (ax, ay) = (f64::from(wx + x), f64::from(wy + y));
        sess.portal.rd.notify_pointer_motion_absolute(&sess.portal.session, sess.portal.stream_id, ax, ay, NotifyPointerMotionAbsoluteOptions::default()).await.map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!("moved ({x},{y})"))]))
    }

    #[rmcp::tool(name = "mouse_scroll", description = "Scroll at window-relative pixel coords. delta: positive=down/right, negative=up/left. horizontal/discrete are optional.")]
    async fn mouse_scroll(&self, Parameters(params): Parameters<MouseScrollParams>) -> Result<CallToolResult, McpError> {
        let x = parse_int(params.x); let y = parse_int(params.y); let delta = parse_int(params.delta);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?).await?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let (ax, ay) = (f64::from(wx + x), f64::from(wy + y));
        sess.portal.rd.notify_pointer_motion_absolute(&sess.portal.session, sess.portal.stream_id, ax, ay, NotifyPointerMotionAbsoluteOptions::default()).await.map_err(eis_err)?;
        let horiz = params.horizontal.unwrap_or_default();
        if params.discrete.unwrap_or_default() {
            let axis = if horiz { Axis::Horizontal } else { Axis::Vertical };
            sess.portal.rd.notify_pointer_axis_discrete(&sess.portal.session, axis, delta, NotifyPointerAxisDiscreteOptions::default()).await.map_err(eis_err)?;
        } else {
            let (dx, dy) = if horiz { (f64::from(delta) * 15.0, 0.0) } else { (0.0, f64::from(delta) * 15.0) };
            sess.portal.rd.notify_pointer_axis(&sess.portal.session, dx, dy, NotifyPointerAxisOptions::default()).await.map_err(eis_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text(format!("scrolled {delta} at ({x},{y})"))]))
    }

    #[rmcp::tool(name = "mouse_drag", description = "Drag between window-relative pixel coords. Smooth 20-step interpolation. button: left/right/middle.")]
    async fn mouse_drag(&self, Parameters(params): Parameters<MouseDragParams>) -> Result<CallToolResult, McpError> {
        let from_x = parse_int(params.from_x); let from_y = parse_int(params.from_y);
        let to_x = parse_int(params.to_x); let to_y = parse_int(params.to_y);
        let (wx, wy, _) = active_window_info(&self.zbus_conn().await?).await?;
        let code = btn_code(params.button.as_deref())?;
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let (ax, ay) = (f64::from(wx + from_x), f64::from(wy + from_y));
        sess.portal.rd.notify_pointer_motion_absolute(&sess.portal.session, sess.portal.stream_id, ax, ay, NotifyPointerMotionAbsoluteOptions::default()).await.map_err(eis_err)?;
        sess.portal.rd.notify_pointer_button(&sess.portal.session, code, KeyState::Pressed, NotifyPointerButtonOptions::default()).await.map_err(eis_err)?;
        let steps = 20i32;
        for step in 1..=steps {
            let cx = f64::from(wx + from_x + (to_x - from_x) * step / steps);
            let cy = f64::from(wy + from_y + (to_y - from_y) * step / steps);
            sess.portal.rd.notify_pointer_motion_absolute(&sess.portal.session, sess.portal.stream_id, cx, cy, NotifyPointerMotionAbsoluteOptions::default()).await.map_err(eis_err)?;
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        sess.portal.rd.notify_pointer_button(&sess.portal.session, code, KeyState::Released, NotifyPointerButtonOptions::default()).await.map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!("dragged ({from_x},{from_y})->({to_x},{to_y})"))]))
    }

    #[rmcp::tool(name = "keyboard_type", description = "Type ASCII text character by character. For non-ASCII use keyboard_type_unicode.")]
    async fn keyboard_type(&self, Parameters(params): Parameters<KeyboardTypeParams>) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        for ch in params.text.chars() {
            match char_key(ch) {
                Some((code, needs_shift)) => {
                    let kc = i32::try_from(code).map_err(eis_err)?;
                    if needs_shift {
                        sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, 42, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
                    }
                    sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
                    sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Released, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
                    if needs_shift {
                        sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, 42, KeyState::Released, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                None => return Err(McpError::invalid_params(format!("unmapped char '{}' — ASCII only", ch), None)),
            }
        }
        Ok(CallToolResult::success(vec![Content::text(format!("typed: {}", params.text))]))
    }

    #[rmcp::tool(name = "keyboard_key", description = "Press key combo (e.g. 'Return', 'ctrl+c', 'alt+F4', 'shift+Tab').")]
    async fn keyboard_key(&self, Parameters(params): Parameters<KeyboardKeyParams>) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let (mods, main) = parse_combo(&params.key);
        for m in &mods {
            let kc = i32::try_from(*m).map_err(eis_err)?;
            sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
        }
        match main {
            Some(k) => {
                let kc = i32::try_from(k).map_err(eis_err)?;
                sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Released, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
            }
            None => return Err(McpError::invalid_params(format!("unknown key in combo '{}'", params.key), None)),
        }
        for m in mods.iter().rev() {
            let kc = i32::try_from(*m).map_err(eis_err)?;
            sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Released, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text(format!("key: {}", params.key))]))
    }

    #[rmcp::tool(name = "launch_app", description = "Launch an application by desktop file ID (e.g. 'org.kde.konsole').")]
    async fn launch_app(&self, Parameters(params): Parameters<LaunchAppParams>) -> Result<CallToolResult, McpError> {
        let conn = self.zbus_conn().await?;
        let launcher = ashpd::desktop::dynamic_launcher::DynamicLauncherProxy::with_connection(conn).await.map_err(eis_err)?;
        launcher.launch(&params.command, ashpd::desktop::dynamic_launcher::LaunchOptions::default()).await.map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!("launched: {}", params.command))]))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        nix::libc::signal(nix::libc::SIGCHLD, nix::libc::SIG_IGN);
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
    }
    let kwin = KwinMcp::new();
    let router = rmcp::handler::server::router::Router::new(kwin)
        .with_tools(KwinMcp::tool_router());
    let transport = rmcp::transport::io::stdio();
    let service = router.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
