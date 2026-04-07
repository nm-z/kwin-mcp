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
            serde_json::Value::Number(n) => {
                let i = n.as_i64().ok_or_else(|| serde::de::Error::custom("not an i64"))?;
                let v = i32::try_from(i).map_err(|e| serde::de::Error::custom(format!("not an i32: {e}")))?;
                Ok(FlexInt(v))
            }
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

fn char_key(ch: char) -> Result<(u32, bool), McpError> {
    let (raw, shifted) = match ch {
        'a'..='z' | '0'..='9' | '`' | '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/' | ' ' | '\t' | '\n' => (ch, false),
        'A'..='Z' => (ch.to_ascii_lowercase(), true),
        '~' => ('`', true), '!' => ('1', true), '@' => ('2', true), '#' => ('3', true),
        '$' => ('4', true), '%' => ('5', true), '^' => ('6', true), '&' => ('7', true),
        '*' => ('8', true), '(' => ('9', true), ')' => ('0', true), '_' => ('-', true),
        '+' => ('=', true), '{' => ('[', true), '}' => (']', true), '|' => ('\\', true),
        ':' => (';', true), '"' => ('\'', true), '<' => (',', true), '>' => ('.', true),
        '?' => ('/', true),
        other => Err(McpError::invalid_params(format!("unmapped char '{other}'"), None))?,
    };
    let input: keyboard_codes::KeyboardInput = String::from(raw).parse()
        .map_err(|e| McpError::invalid_params(format!("keycode parse '{ch}': {e}"), None))?;
    let code = u32::try_from(input.to_code(Platform::Linux))
        .map_err(|e| McpError::invalid_params(format!("keycode overflow '{ch}': {e}"), None))?;
    Ok((code, shifted))
}

fn parse_combo(key: &str) -> Result<(Vec<u32>, Option<u32>), McpError> {
    match keyboard_codes::parser::parse_shortcut_with_aliases(key) {
        Ok(shortcut) => {
            let mods: Vec<u32> = shortcut.modifiers.iter()
                .map(|m| u32::try_from(keyboard_codes::KeyboardInput::Modifier(*m).to_code(Platform::Linux))
                    .map_err(|e| McpError::invalid_params(format!("modifier overflow: {e}"), None)))
                .collect::<Result<Vec<_>, _>>()?;
            let main = Some(u32::try_from(shortcut.key.to_code(Platform::Linux))
                .map_err(|e| McpError::invalid_params(format!("key overflow: {e}"), None))?);
            Ok((mods, main))
        }
        Err(_parse_err) => {
            match key.chars().next() {
                Some(ch) => { let (k, _shifted) = char_key(ch)?; Ok((Vec::new(), Some(k))) }
                None => Err(McpError::invalid_params(format!("empty key combo '{key}'"), None))
            }
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
    eprintln!("portal: stream_id={stream_id} streams={}", started.streams().len());
    Ok(PortalSession { rd, session, stream_id })
}

// ── Session ──────────────────────────────────────────────────────────────

struct Session {
    portal: PortalSession,
    zbus_conn: zbus::Connection,
    container: hakoniwa::Container,
    container_child: hakoniwa::Child,
    container_stdin: std::io::PipeWriter,
    host_xdg_dir: std::path::PathBuf,
}

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

fn teardown(mut sess: Session) {
    use std::io::Write;
    drop(sess.portal);
    // Kill all container processes via bash process group
    match writeln!(sess.container_stdin, "kill 0") { Err(e) => eprintln!("teardown kill 0: {e}"), Ok(()) => {} }
    drop(sess.container_stdin);
    match sess.container_child.kill() { Err(e) => eprintln!("teardown kill: {e}"), Ok(()) => {} }
    match sess.container_child.wait() { Err(e) => eprintln!("teardown wait: {e}"), Ok(_) => {} }
    drop(sess.container);
    match std::fs::remove_dir_all(&sess.host_xdg_dir) { Err(e) => eprintln!("teardown cleanup: {e}"), Ok(()) => {} }
}


async fn active_window_info(conn: &zbus::Connection) -> Result<(i32, i32, String), McpError> {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(eis_err)?.as_millis();
    let marker = format!("kwin-mcp-{ts}");
    let cb_path = format!("/KWinMCP/{ts}");
    let our_name = conn.unique_name().ok_or_else(|| McpError::internal_error("no bus name", None))?.to_string();
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
    match registered { true => {} false => return Err(McpError::internal_error(format!("failed to register callback at {cb_path}"), None)) }
    // Load and run the script
    let scripting: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin").map_err(eis_err)?
        .path("/Scripting").map_err(eis_err)?
        .interface("org.kde.kwin.Scripting").map_err(eis_err)?
        .build().await.map_err(eis_err)?;
    let script_path = script_file.to_string_lossy().to_string();
    let (script_id,): (i32,) = scripting.call("loadScript", &(script_path, &marker)).await.map_err(eis_err)?;
    match script_id >= 0 {
        true => {}
        false => {
            conn.object_server().remove::<KWinCallback, _>(&obj_path).await.map_err(eis_err)?;
            std::fs::remove_file(&script_file).map_err(eis_err)?;
            return Err(McpError::internal_error(format!("KWin loadScript failed, id={script_id}"), None));
        }
    }
    let script_proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin").map_err(eis_err)?
        .path(format!("/Scripting/Script{script_id}")).map_err(eis_err)?
        .interface("org.kde.kwin.Script").map_err(eis_err)?
        .build().await.map_err(eis_err)?;
    script_proxy.call::<_, (), ()>("run", &()).await.map_err(eis_err)?;
    // Wait for callback, then cleanup regardless of result
    let json_result = rx.await.map_err(|_| McpError::internal_error("KWin callback channel closed", None));
    conn.object_server().remove::<KWinCallback, _>(&obj_path).await.map_err(eis_err)?;
    let (_, ): (bool, ) = scripting.call("unloadScript", &(&marker,)).await.map_err(eis_err)?;
    std::fs::remove_file(&script_file).map_err(eis_err)?;
    let json = json_result?;
    match json.as_str() { "null" => return Err(McpError::internal_error("KWin script error: No active window", None)), _ => {} }
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
        match self.tx.lock() {
            Ok(mut g) => match g.take() {
                Some(tx) => match tx.send(payload) { Ok(()) => {} Err(e) => eprintln!("callback send failed: {e}") }
                None => {}
            }
            Err(e) => eprintln!("callback lock poisoned: {e}")
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
    #[rmcp::tool(name = "session_start", description = "Start an isolated KDE Wayland session in a container for GUI automation. Must be called before any other tool.")]
    async fn session_start(&self, Parameters(_params): Parameters<SessionStartParams>) -> Result<CallToolResult, McpError> {
        eprintln!("kwin-mcp v{}.{} ({}) session_start", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER"), env!("GIT_HASH"));
        let version_stamp = format!("kwin-mcp v{}.{} ({})", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER"), env!("GIT_HASH"));
        let ver_err = |e: String| McpError::internal_error(format!("{version_stamp} — {e}"), None);
        {
            let mut guard = self.session.lock().await;
            match (*guard).take() { Some(old) => teardown(old), None => {} }
        }
        let pid = std::process::id();
        let host_xdg_dir = std::env::temp_dir().join(format!("kwin-mcp-{pid}"));
        match std::fs::remove_dir_all(&host_xdg_dir) { Err(_) => {} Ok(()) => {} }
        std::fs::create_dir_all(&host_xdg_dir).map_err(|e| ver_err(e.to_string()))?;
        let home = std::env::var("HOME").map_err(|e| ver_err(e.to_string()))?;
        let bus_path = host_xdg_dir.join("bus");
        let bus_addr = format!("unix:path={}", bus_path.to_string_lossy());
        // Build container with isolated namespaces
        let mut container = hakoniwa::Container::new();
        container.rootfs("/").map_err(|e| ver_err(e.to_string()))?;
        container.devfsmount("/dev");
        container.bindmount_rw("/dev/dri", "/dev/dri");
        container.tmpfsmount("/run");
        container.tmpfsmount("/tmp");
        container.runctl(hakoniwa::Runctl::MountFallback);
        container.bindmount_rw(&host_xdg_dir.to_string_lossy(), "/tmp/xdg");
        container.bindmount_ro(&home, &home);
        container.share(hakoniwa::Namespace::Pid);
        container.bindmount_rw("/proc", "/proc");
        container.unshare(hakoniwa::Namespace::Network);
        // Entrypoint: start services sequentially with readiness checks
        let xdg_inner = "/tmp/xdg";
        let entrypoint = format!("\
ulimit -c 0\n\
mkdir -p /run/user /tmp/cache /tmp/state /dev/dri\n\
printf '#!/bin/sh\\nexit 0\\n' > /tmp/kdialog && chmod +x /tmp/kdialog\n\
dbus-daemon --session --address='unix:path={xdg_inner}/bus' --nofork &\n\
kwin_wayland --drm --width 1920 --height 1080 2>/tmp/xdg/kwin.log &\n\
n=0; while [ ! -S /run/user/wayland-0 ] && [ $n -lt 100 ]; do read -t 0.05 junk </dev/zero 2>/dev/null; n=$((n+1)); done\n\
pipewire 2>/tmp/pipewire.log &\n\
at-spi-bus-launcher 2>/tmp/atspi.log &\n\
wireplumber 2>/tmp/wireplumber.log &\n\
xdg-desktop-portal 2>/tmp/xdg/portal.log &\n\
{{ read -t 1 junk </dev/zero 2>/dev/null; xdg-desktop-portal-kde 2>/tmp/xdg/portal-kde.log; }} &\n\
while read -r cmd; do eval \"$cmd\" & done\n");
        let mut cmd = container.command("/bin/bash");
        cmd.arg("-c").arg(entrypoint.as_str());
        cmd.env("PATH", "/tmp:/usr/bin:/usr/sbin:/bin:/sbin:/usr/lib:/usr/libexec:/usr/lib/at-spi2-core");
        cmd.env("HOME", &home);
        let user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
        cmd.env("USER", &user);
        cmd.env("XDG_RUNTIME_DIR", "/run/user");
        cmd.env("XDG_CACHE_HOME", "/tmp/cache");
        cmd.env("XDG_DATA_HOME", "/tmp/state");
        cmd.env("XDG_SESSION_TYPE", "wayland");
        cmd.env("XDG_CURRENT_DESKTOP", "KDE");
        let dbus_addr = format!("unix:path={xdg_inner}/bus");
        cmd.env("DBUS_SESSION_BUS_ADDRESS", dbus_addr.as_str());
        cmd.env("WAYLAND_DISPLAY", "wayland-0");
        cmd.env("LIBGL_ALWAYS_SOFTWARE", "1");
        cmd.env("GALLIUM_DRIVER", "llvmpipe");
        cmd.env("KDE_DEBUG", "0");
        cmd.env("KWIN_DRM_VIRTUAL_BACKENDS", "1");
        cmd.env("XDG_DESKTOP_PORTAL_TEST_APP_ID", "kwin-mcp");
        cmd.stdin(hakoniwa::Stdio::piped());
        cmd.stderr(hakoniwa::Stdio::inherit());
        let mut container_child = cmd.spawn().map_err(|e| ver_err(e.to_string()))?;
        let container_stdin = container_child.stdin.take()
            .ok_or_else(|| ver_err("container stdin not available".to_string()))?;
        // Wait for D-Bus socket to appear on host via bind-mount
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match bus_path.exists() {
                true => break,
                false => match std::time::Instant::now() < deadline {
                    true => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                    false => return Err(ver_err("container D-Bus socket did not appear within 5s".to_string())),
                }
            }
        }
        // Connect to the container's session bus
        let addr: zbus::address::Address = bus_addr.as_str().try_into().map_err(|e: zbus::Error| ver_err(e.to_string()))?;
        let zbus_conn = zbus::connection::Builder::address(addr)
            .map_err(|e| ver_err(e.to_string()))?
            .build().await.map_err(|e| ver_err(e.to_string()))?;
        let bus_name = zbus_conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
        // Pre-approve RemoteDesktop permission so the portal doesn't show a dialog
        let perm_store: zbus::Proxy = zbus::proxy::Builder::new(&zbus_conn)
            .destination("org.freedesktop.impl.portal.PermissionStore").map_err(|e| ver_err(e.to_string()))?
            .path("/org/freedesktop/impl/portal/PermissionStore").map_err(|e| ver_err(e.to_string()))?
            .interface("org.freedesktop.impl.portal.PermissionStore").map_err(|e| ver_err(e.to_string()))?
            .build().await.map_err(|e| ver_err(e.to_string()))?;
        let perms: Vec<&str> = vec!["yes"];
        match perm_store.call("SetPermission", &("kde-authorized", true, "remote-desktop", "", &perms)).await {
            Ok(()) => eprintln!("permission store: approved remote-desktop"),
            Err(e) => eprintln!("permission store: {e}"),
        }
        match perm_store.call("SetPermission", &("kde-authorized", true, "screencast", "", &perms)).await {
            Ok(()) => eprintln!("permission store: approved screencast"),
            Err(e) => eprintln!("permission store screencast: {e}"),
        }
        // Wait for portal services to start, then set up RemoteDesktop
        let portal = match tokio::time::timeout(std::time::Duration::from_secs(5), portal_setup(&zbus_conn)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return Err(ver_err(format!("portal setup: {e}"))),
            Err(_) => return Err(ver_err("portal setup hung after 5s".to_string())),
        };
        let msg = format!("{version_stamp} — session started bus={bus_name}");
        let mut guard = self.session.lock().await;
        *guard = Some(Session { portal, zbus_conn, container, container_child, container_stdin, host_xdg_dir });
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

    #[rmcp::tool(name = "screenshot", description = "Take a screenshot via the Screenshot portal. Returns the file URI.", annotations(read_only_hint = true))]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let conn = self.zbus_conn().await?;
        let result = ashpd::desktop::screenshot::Screenshot::request()
            .connection(Some(conn))
            .send().await.map_err(eis_err)?
            .response().map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(result.uri().to_string())]))
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
            match (depth, app_name.as_ref().map(|needle| node.name.to_lowercase().contains(needle)).unwrap_or(true)) {
                (0, false) => continue,
                (_, _) => {}
            }
            let dominated = role.as_ref().map(|needle| node.role.to_lowercase().contains(needle)).unwrap_or(true) && (show_elements || node.is_useful());
            match dominated { true => out.push(node.line(depth)), false => {} }
            let child_depth = match dominated { true => depth + 1, false => depth };
            match child_depth <= limit { true => {
                for child in acc.get_children().await.unwrap_or_default().into_iter().rev() { stack.push((child, child_depth)); }
            } false => {} }
        }
        Ok(CallToolResult::success(vec![Content::text(out.join("\n"))]))
    }

    #[rmcp::tool(name = "find_ui_elements", description = "Search UI elements by name/role/description (case-insensitive).", annotations(read_only_hint = true))]
    async fn find_ui_elements(&self, Parameters(params): Parameters<FindUiElementsParams>) -> Result<CallToolResult, McpError> {
        self.with_session(|_sess| { Err(McpError::internal_error(format!("AT-SPI2 search not yet implemented: {}", params.query), None)) }).await
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
            match n { 0 => {} _ => { tokio::time::sleep(std::time::Duration::from_millis(50)).await; } }
            sess.portal.rd.notify_pointer_button(&sess.portal.session, code, KeyState::Pressed, NotifyPointerButtonOptions::default()).await.map_err(eis_err)?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        match params.discrete.unwrap_or_default() {
            true => {
                let axis = match horiz { true => Axis::Horizontal, false => Axis::Vertical };
                sess.portal.rd.notify_pointer_axis_discrete(&sess.portal.session, axis, delta, NotifyPointerAxisDiscreteOptions::default()).await.map_err(eis_err)?;
            }
            false => {
                let (dx, dy) = match horiz { true => (f64::from(delta) * 15.0, 0.0), false => (0.0, f64::from(delta) * 15.0) };
                sess.portal.rd.notify_pointer_axis(&sess.portal.session, dx, dy, NotifyPointerAxisOptions::default()).await.map_err(eis_err)?;
            }
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
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        sess.portal.rd.notify_pointer_button(&sess.portal.session, code, KeyState::Released, NotifyPointerButtonOptions::default()).await.map_err(eis_err)?;
        Ok(CallToolResult::success(vec![Content::text(format!("dragged ({from_x},{from_y})->({to_x},{to_y})"))]))
    }

    #[rmcp::tool(name = "keyboard_type", description = "Type ASCII text character by character. For non-ASCII use keyboard_type_unicode.")]
    async fn keyboard_type(&self, Parameters(params): Parameters<KeyboardTypeParams>) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        for ch in params.text.chars() {
            let (code, needs_shift) = char_key(ch)?;
            let kc = i32::try_from(code).map_err(eis_err)?;
            match needs_shift { true => { sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, 42, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?; } false => {} }
            sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
            sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Released, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
            match needs_shift { true => { sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, 42, KeyState::Released, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?; } false => {} }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(CallToolResult::success(vec![Content::text(format!("typed: {}", params.text))]))
    }

    #[rmcp::tool(name = "keyboard_key", description = "Press key combo (e.g. 'Return', 'ctrl+c', 'alt+F4', 'shift+Tab').")]
    async fn keyboard_key(&self, Parameters(params): Parameters<KeyboardKeyParams>) -> Result<CallToolResult, McpError> {
        let guard = self.session.lock().await;
        let sess = guard.as_ref().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        let (mods, main) = parse_combo(&params.key)?;
        for m in &mods {
            let kc = i32::try_from(*m).map_err(eis_err)?;
            sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
        }
        match main {
            Some(k) => {
                let kc = i32::try_from(k).map_err(eis_err)?;
                sess.portal.rd.notify_keyboard_keycode(&sess.portal.session, kc, KeyState::Pressed, NotifyKeyboardKeycodeOptions::default()).await.map_err(eis_err)?;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

    #[rmcp::tool(name = "launch_app", description = "Launch an application inside the container by command (e.g. 'kate', 'konsole').")]
    async fn launch_app(&self, Parameters(params): Parameters<LaunchAppParams>) -> Result<CallToolResult, McpError> {
        use std::io::Write;
        let mut guard = self.session.lock().await;
        let sess = guard.as_mut().ok_or_else(|| McpError::internal_error("no session — call session_start first", None))?;
        writeln!(sess.container_stdin, "{}", params.command).map_err(eis_err)?;
        sess.container_stdin.flush().map_err(eis_err)?;
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
