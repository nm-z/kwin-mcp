#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::as_conversions,
    clippy::wildcard_enum_match_arm,
    clippy::wildcard_imports,
    dead_code
)]

//! Live viewer for a kwin-mcp container.
//!
//! Connects to /tmp/kwin-mcp-<pid>/ (passed as argv[1]), negotiates a
//! zkde_screencast_unstable_v1 feed against the container's KWin, consumes
//! the resulting PipeWire video node, and renders frames into a screen-13
//! window. Mouse/keyboard events on the window are forwarded back into the
//! container via org_kde_kwin_fake_input.

use async_tungstenite::tungstenite::Message;
use kwin_mcp::clipboard::{Clipboard, DISPATCH_POLL_INTERVAL, DispatchThread, Shutdown};
use kwin_mcp::viewer_stream::{Header, Notice, Payload};
use kwin_mcp::{dsc, remote, viewer_stream};
use screen_13::driver::ash::vk;
use screen_13::driver::buffer::Buffer;
use screen_13::driver::image::{Image, ImageInfo};
use screen_13_window::{FrameContext, WindowBuilder};
use std::cell::Cell;
use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use wayland_protocols_plasma::keystate::client::org_kde_kwin_keystate::{
    self as kde_keystate, OrgKdeKwinKeystate,
};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::{
    self as zs_stream, ZkdeScreencastStreamUnstableV1,
};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::{
    Pointer as ScPointer, ZkdeScreencastUnstableV1,
};
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::keyboard::PhysicalKey;

// Bindings ceilings — the wayland-protocols-plasma 0.3.12 XMLs cap here even
// though the container's KWin advertises higher. Binding above a binding's
// known version panics the scanner-generated code.
const FAKE_INPUT_VERSION: u32 = 5;
const KEYSTATE_VERSION: u32 = 5;
const SCREENCAST_VERSION: u32 = 4;
const WL_OUTPUT_VERSION: u32 = 4;

// Linux input event codes — evdev BTN_* constants (see linux/input-event-codes.h).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// fake_input axis ids (matches wl_pointer axis): 0=vertical, 1=horizontal.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

const NUMLOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

// How long after a viewer copy chord the session's next selection change is
// taken as that copy. A memory-throttled browser can take seconds to answer
// Ctrl+C; the next clipboard shortcut ends the wait early.
const COPY_TIMEOUT: Duration = Duration::from_secs(10);
const COPY_POLL: Duration = Duration::from_millis(100);
// A viewer paste hands the agent's selection back after the pasting app has
// been quiet this long, or after PASTE_TIMEOUT if it never reads.
const PASTE_SETTLE: Duration = Duration::from_millis(200);
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

// A drag delivers a Resized per pointer step; the aspect correction waits until
// the size has held this long, so it never fights the compositor mid-drag.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(4);
const VIEWER_STATUS_HEARTBEAT: Duration = Duration::from_secs(1);
const INITIAL_VIEWER_WIDTH: f64 = 1920.0;
const INITIAL_VIEWER_HEIGHT: f64 = 1080.0;

fn frame_timeout_reason(session: Option<&std::path::Path>) -> String {
    let state = session
        .and_then(|path| std::fs::read_to_string(path.join("viewer-pipewire.json")).ok())
        .unwrap_or_else(|| "not connected".to_owned());
    format!(
        "viewer did not submit a frame within {} seconds; last PipeWire state: {}",
        FIRST_FRAME_TIMEOUT.as_secs(),
        state.trim()
    )
}

/// Status file kwin-mcp reads to report the viewer outcome; the name is shared
/// with src/main.rs.
const VIEWER_STATUS_FILE: &str = "viewer-status.json";
const VIEWER_INPUT_ACTIVITY_FILE: &str = "viewer-input-activity";

fn open_input_activity(session: &std::path::Path) -> anyhow::Result<std::fs::File> {
    let path = session.join(VIEWER_INPUT_ACTIVITY_FILE);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => {
            file.set_times(
                std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH),
            )?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

/// Atomically publish the viewer's lifecycle state: starting,
/// ready (the first session frame's render graph was submitted successfully), closed, or failed.
fn write_status(session: &std::path::Path, state: &str, detail: &str) {
    let temporary = session.join(format!("{VIEWER_STATUS_FILE}.tmp"));
    let body = serde_json::json!({"state": state, "detail": detail, "pid": std::process::id(), "input_activity_reporting": true});
    if std::fs::write(&temporary, body.to_string()).is_ok() {
        let _ = std::fs::rename(&temporary, session.join(VIEWER_STATUS_FILE));
    }
}

struct Frame {
    width: u32,
    height: u32,
    // Tightly packed 4-byte pixels (stride == 4 * width) in the layout the
    // consumer asked for: RGBA8 for a window, BGRx for the stream sender.
    // Video format conversion happens in the pipewire callback so the render
    // path stays trivial.
    pixels: Vec<u8>,
    // When PipeWire delivered the picture, in microseconds since the Unix
    // epoch on the clock of the machine that captured it.
    captured_micros: u64,
    // Counts the pictures of one source: PipeWire deliveries, or the sequence
    // number a stream sender gave.
    serial: u64,
}

// Pixel layout the PipeWire callback hands out.
#[derive(Clone, Copy)]
enum CaptureLayout {
    Rgba,
    Bgrx,
}

fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as u64)
}

// Latest-frame mailbox. PipeWire's process callback writes; the window's
// draw_fn reads. No queue, no backpressure: at <60fps the window simply
// redraws the last frame. Wrapped in Mutex instead of a channel of 1 so the
// producer never blocks if the consumer is slow.
type FrameMailbox = Arc<(Mutex<Option<Frame>>, Condvar)>;

fn store_frame(mailbox: &FrameMailbox, frame: Frame) {
    let (latest, ready) = &**mailbox;
    if let Ok(mut latest) = latest.lock() {
        *latest = Some(frame);
        ready.notify_one();
    }
}

fn take_frame(mailbox: &FrameMailbox) -> Option<Frame> {
    let (latest, _) = &**mailbox;
    latest.lock().ok().and_then(|mut latest| latest.take())
}

fn wait_for_first_frame(
    mailbox: &FrameMailbox,
    shutdown: &Shutdown,
    deadline: Instant,
) -> Option<Frame> {
    let (latest, ready) = &**mailbox;
    let mut latest = latest.lock().ok()?;
    loop {
        if let Some(frame) = latest.take() {
            return Some(frame);
        }
        if shutdown.requested() {
            return None;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        // Shutdown is an atomic flag rather than a condition-variable event;
        // bounded waits make teardown responsive without spinning the UI thread.
        let wait = remaining.min(DISPATCH_POLL_INTERVAL);
        let (next, _) = ready.wait_timeout(latest, wait).ok()?;
        latest = next;
    }
}

// Cooperative stop signal shared by every thread the viewer owns. Whatever
// ends first sets it: the user closing the window, a compositor dropping a
// connection, or the screencast stream failing. The winit loop leaves on the
// next frame and main then joins the threads and closes the connections.
struct WlState {
    output: Option<wl_output::WlOutput>,
    screencast: Option<ZkdeScreencastUnstableV1>,
    fake_input: Option<OrgKdeKwinFakeInput>,
    stream: Option<ZkdeScreencastStreamUnstableV1>,
    node_id: Option<u32>,
    failed: Option<String>,
    closed: bool,
    shutdown: Shutdown,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_output" if state.output.is_none() => {
                    state.output = Some(registry.bind(name, version.min(WL_OUTPUT_VERSION), qh, ()));
                }
                "zkde_screencast_unstable_v1" => {
                    state.screencast = Some(registry.bind(name, version.min(SCREENCAST_VERSION), qh, ()));
                }
                "org_kde_kwin_fake_input" => {
                    state.fake_input = Some(registry.bind(name, version.min(FAKE_INPUT_VERSION), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zs_stream::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zs_stream::Event::Created { node } => state.node_id = Some(node),
            // The feed is the reason the viewer exists: once KWin fails or
            // closes the stream there is nothing left to show, so shut down
            // instead of holding the window open on a stale frame.
            zs_stream::Event::Failed { error } => {
                state.failed = Some(error);
                state.shutdown.request();
            }
            zs_stream::Event::Closed => {
                state.closed = true;
                state.shutdown.request();
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(WlState: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(WlState: ignore ZkdeScreencastUnstableV1);
wayland_client::delegate_noop!(WlState: ignore OrgKdeKwinFakeInput);

// Newest Num Lock state a compositor confirmed over org_kde_kwin_keystate,
// shared between the watcher thread that dispatches the key-state queue and
// the winit thread that synchronizes on focus.
#[derive(Default)]
struct NumLockWatch {
    // None until the compositor reports Num Lock for the first time.
    enabled: Option<bool>,
    // Set when the watcher's queue stops dispatching, so a waiter fails at
    // once instead of blocking on a connection that is already gone.
    ended: bool,
}

#[derive(Default)]
struct NumLockShared {
    watch: Mutex<NumLockWatch>,
    updated: Condvar,
}

impl NumLockShared {
    fn publish(&self, enabled: bool) {
        if let Ok(mut watch) = self.watch.lock() {
            watch.enabled = Some(enabled);
            self.updated.notify_all();
        }
    }

    fn end(&self) {
        if let Ok(mut watch) = self.watch.lock() {
            watch.ended = true;
            self.updated.notify_all();
        }
    }

    // Newest state the compositor confirmed, without asking it again.
    fn latest(&self, source: &str) -> anyhow::Result<bool> {
        let watch = self
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{source} key-state mutex poisoned"))?;
        watch
            .enabled
            .ok_or_else(|| anyhow::anyhow!("{source} compositor did not report Num Lock state"))
    }
}

struct NumLockState {
    proxy: Option<OrgKdeKwinKeystate>,
    shared: Arc<NumLockShared>,
}

impl Drop for NumLockState {
    // The dispatch thread owns this state and drops it when it stops, so a
    // waiter learns immediately that this compositor can no longer confirm
    // anything instead of waiting out the full confirmation timeout.
    fn drop(&mut self) {
        self.shared.end();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for NumLockState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "org_kde_kwin_keystate"
        {
            state.proxy = Some(registry.bind(name, version.min(KEYSTATE_VERSION), qh, ()));
        }
    }
}

impl Dispatch<OrgKdeKwinKeystate, ()> for NumLockState {
    fn event(
        state: &mut Self,
        _: &OrgKdeKwinKeystate,
        event: kde_keystate::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let kde_keystate::Event::StateChanged {
            key,
            state: key_state,
        } = event
            && key == kde_keystate::Key::Numlock as u32
        {
            state
                .shared
                .publish(key_state == kde_keystate::State::Locked as u32);
        }
    }
}

struct NumLockWatcher {
    source: &'static str,
    shared: Arc<NumLockShared>,
    // Dropped last in this struct, after the fields above, so the thread is
    // joined and the key-state socket is closed when the watcher goes away.
    dispatch: DispatchThread,
}

impl NumLockWatcher {
    // KWin republishes the whole key-state set to every bound
    // org_kde_kwin_keystate resource on every LED or modifier change, so at
    // version 5 plain Shift/Ctrl/Alt/Meta typing produces events too. A
    // resource that is only read on demand therefore grows an unread backlog
    // until the compositor tears the connection down. Each watcher owns a
    // dispatch thread that services its queue for the viewer's lifetime and
    // keeps only the newest confirmed Num Lock state.
    fn spawn(conn: Connection, source: &'static str, shutdown: &Shutdown) -> anyhow::Result<Self> {
        let mut queue = conn.new_event_queue::<NumLockState>();
        let _registry = conn.display().get_registry(&queue.handle(), ());
        let shared = Arc::new(NumLockShared::default());
        let mut state = NumLockState {
            proxy: None,
            shared: Arc::clone(&shared),
        };
        queue.roundtrip(&mut state)?;
        let proxy = state.proxy.clone().ok_or_else(|| {
            anyhow::anyhow!("{source} compositor did not advertise org_kde_kwin_keystate")
        })?;
        // The compositor only pushes changes, so ask once for the current set.
        proxy.fetchStates();
        queue.roundtrip(&mut state)?;
        // Confirm the compositor answered before handing the queue to a
        // thread, so a watcher that never reports fails on its own instead of
        // tripping the shared shutdown through the thread it would own.
        shared.latest(source)?;
        Ok(Self {
            source,
            shared,
            dispatch: DispatchThread::spawn(
                format!("keystate-{source}"),
                conn,
                queue,
                state,
                shutdown,
            )?,
        })
    }

    fn latest(&self) -> anyhow::Result<bool> {
        self.shared.latest(self.source)
    }

    // Block until the compositor confirms `expected`, so callers never assume
    // an injected transition landed. The wait ends early when the watcher
    // stops or the viewer starts shutting down, and it is bounded by `timeout`
    // in every case, so a wedged compositor cannot pin the caller.
    fn wait_for(
        &self,
        expected: bool,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> anyhow::Result<()> {
        let wanted = if expected { "enabled" } else { "disabled" };
        let deadline = Instant::now() + timeout;
        let mut watch = self
            .shared
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{} key-state mutex poisoned", self.source))?;
        loop {
            if watch.enabled == Some(expected) {
                return Ok(());
            }
            anyhow::ensure!(
                !watch.ended,
                "{} key-state watcher stopped before confirming Num Lock {wanted}",
                self.source
            );
            anyhow::ensure!(
                !shutdown.requested(),
                "{} Num Lock confirmation dropped: the viewer is shutting down",
                self.source
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "{} compositor did not confirm Num Lock {wanted} within {timeout:?}",
                self.source
            );
            // Wake at least once per dispatch poll interval so a shutdown
            // request is never ignored for the whole confirmation timeout.
            let slice = remaining.min(DISPATCH_POLL_INTERVAL);
            let (guard, _) = self
                .shared
                .updated
                .wait_timeout(watch, slice)
                .map_err(|_| anyhow::anyhow!("{} key-state mutex poisoned", self.source))?;
            watch = guard;
        }
    }
}

fn evdev_code(key: evdev::KeyCode) -> u32 {
    u32::from(key.0)
}

struct NumLockSync {
    host: NumLockWatcher,
    isolated: NumLockWatcher,
    shutdown: Shutdown,
}

impl NumLockSync {
    // Drive the isolated compositor to the host's newest confirmed Num Lock
    // state. Both states come from the watchers, so this never stalls on a
    // round trip and never races the modifier traffic they are draining.
    fn apply(&self, fake_input: &OrgKdeKwinFakeInput, conn: &Connection) -> anyhow::Result<()> {
        let host_enabled = self.host.latest()?;
        if host_enabled == self.isolated.latest()? {
            return Ok(());
        }
        let code = evdev_code(evdev::KeyCode::KEY_NUMLOCK);
        fake_input.keyboard_key(code, 1);
        fake_input.keyboard_key(code, 0);
        conn.flush()?;
        self.isolated
            .wait_for(host_enabled, NUMLOCK_CONFIRM_TIMEOUT, &self.shutdown)?;
        eprintln!(
            "kwin-viewer: synchronized isolated Num Lock {}",
            if host_enabled { "enabled" } else { "disabled" }
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ClipChord {
    Copy,
    Paste,
}

// The chord the user pressed, if it is a standard copy, cut, or paste
// shortcut. `held` already includes `code`.
fn clipboard_chord(code: u32, held: &HashSet<u32>) -> Option<ClipChord> {
    use evdev::KeyCode as K;
    let any = |keys: &[K]| keys.iter().any(|key| held.contains(&evdev_code(*key)));
    if any(&[
        K::KEY_LEFTALT,
        K::KEY_RIGHTALT,
        K::KEY_LEFTMETA,
        K::KEY_RIGHTMETA,
    ]) {
        return None;
    }
    let ctrl = any(&[K::KEY_LEFTCTRL, K::KEY_RIGHTCTRL]);
    let shift = any(&[K::KEY_LEFTSHIFT, K::KEY_RIGHTSHIFT]);
    let is = |key: K| code == evdev_code(key);
    if ctrl && (is(K::KEY_C) || is(K::KEY_X) || is(K::KEY_INSERT))
        || !ctrl && shift && is(K::KEY_DELETE)
    {
        Some(ClipChord::Copy)
    } else if ctrl && is(K::KEY_V) || !ctrl && shift && is(K::KEY_INSERT) {
        Some(ClipChord::Paste)
    } else {
        None
    }
}

// Keeps the viewer user's clipboard on the host. The agent owns the isolated
// session's clipboard, so a copy made through the viewer moves to the host
// and a paste made through the viewer reads the host; the agent's selection
// is put back afterwards either way.
struct ClipboardBridge {
    host: Arc<Clipboard>,
    isolated: Arc<Clipboard>,
    restore: Option<JoinHandle<()>>,
    // Set by the next chord so a pending copy wait stops instead of blocking it.
    superseded: Arc<std::sync::atomic::AtomicBool>,
}

impl ClipboardBridge {
    // Runs before the chord's key press reaches the session.
    fn before(&mut self, chord: ClipChord) -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        self.superseded.store(true, Ordering::Relaxed);
        if let Some(restore) = self.restore.take()
            && restore.join().is_err()
        {
            eprintln!("kwin-viewer: clipboard restore thread panicked");
        }
        self.superseded = Arc::new(AtomicBool::new(false));
        let agent = self.isolated.read()?;
        let isolated = Arc::clone(&self.isolated);
        let restore: Box<dyn FnOnce() -> anyhow::Result<()> + Send> = match chord {
            ClipChord::Copy => {
                let host = Arc::clone(&self.host);
                let before = isolated.generation()?;
                let superseded = Arc::clone(&self.superseded);
                let pressed = Instant::now();
                Box::new(move || {
                    while !isolated.wait_for_change(before, COPY_POLL)? {
                        if superseded.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        if pressed.elapsed() >= COPY_TIMEOUT {
                            eprintln!(
                                "kwin-viewer: copy shortcut: the session clipboard did not change within {COPY_TIMEOUT:?}; the host clipboard is unchanged"
                            );
                            return Ok(());
                        }
                    }
                    host.write(isolated.read()?)?;
                    isolated.write(agent)?;
                    eprintln!(
                        "kwin-viewer: copy shortcut: sent the session selection to the host clipboard after {} ms",
                        pressed.elapsed().as_millis()
                    );
                    Ok(())
                })
            }
            ClipChord::Paste => {
                isolated.write(self.host.read()?)?;
                let pasted = Instant::now();
                // Hand the agent's selection back once the pasting app stops
                // reading the user's.
                Box::new(move || {
                    isolated.wait_for_read_since(pasted, PASTE_SETTLE, PASTE_TIMEOUT)?;
                    isolated.write(agent)
                })
            }
        };
        self.restore = Some(
            std::thread::Builder::new()
                .name("clipboard-restore".to_owned())
                .spawn(move || {
                    if let Err(error) = restore() {
                        eprintln!("kwin-viewer: clipboard handoff failed: {error:#}");
                    }
                })?,
        );
        Ok(())
    }
}

const USAGE: &str = "usage: kwin-viewer /tmp/kwin-mcp-<pid> [width height]\n       kwin-viewer --remote HOST /tmp/kwin-mcp-<pid> [width height]\n       kwin-viewer --serve /tmp/kwin-mcp-<pid> [width height]\n       kwin-viewer --stream HOST:PORT STATUS_DIR          (started by kwin-mcp viewer_open)\n       kwin-viewer --stream-serve /tmp/kwin-mcp-<pid> width height [--link-mbps N]   (started by kwin-mcp --listen)";

fn main() -> anyhow::Result<()> {
    let mut argv = std::env::args().skip(1).peekable();
    match argv.peek().map(String::as_str) {
        Some("--serve") => {
            argv.next();
            serve(argv)
        }
        Some("--stream-serve") => {
            argv.next();
            stream_serve(argv)
        }
        Some("--stream") => {
            argv.next();
            run_stream(argv)
        }
        Some("--remote") => {
            argv.next();
            run_remote(argv)
        }
        _ => {
            let session_dir = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
            let session_path = std::path::PathBuf::from(&session_dir);
            write_status(&session_path, "starting", "");
            let result = run(session_dir, argv);
            match &result {
                Ok(()) => write_status(&session_path, "closed", "viewer exited: window closed or session ended"),
                Err(error) => write_status(&session_path, "failed", &format!("{error:#}")),
            }
            result
        }
    }
}

// Virtual display size, passed by kwin-mcp at spawn. Defaults match the
// server's compiled-in VIRTUAL_SCREEN_WIDTH/HEIGHT for manual invocation.
fn parse_size(argv: &mut impl Iterator<Item = String>) -> anyhow::Result<(u32, u32)> {
    let mut next = |default: u32, name: &str| -> anyhow::Result<u32> {
        match argv.next() {
            Some(v) => v.parse().map_err(|e| anyhow::anyhow!("{name} '{v}': {e}")),
            None => Ok(default),
        }
    };
    let size = (next(3840, "width")?, next(2160, "height")?);
    anyhow::ensure!(size.0 > 0 && size.1 > 0, "viewer dimensions must be positive");
    Ok(size)
}

// The link to a session's compositor: its fake_input, the PipeWire loop that
// feeds the frame mailbox, and the dispatch thread that ends the viewer when
// the screencast stream closes.
struct SessionLink {
    fake_input: OrgKdeKwinFakeInput,
    conn: Connection,
    _wl_dispatch: DispatchThread,
    pw_quit: pipewire::channel::Sender<()>,
    pw_thread: JoinHandle<()>,
}

impl SessionLink {
    fn connect(
        session_path: &std::path::Path,
        virt: (u32, u32),
        shutdown: &Shutdown,
        mailbox: &FrameMailbox,
        layout: CaptureLayout,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            session_path.join("wayland-0").exists(),
            "wayland-0 socket missing in {} — is the session running?",
            session_path.display()
        );
        let wayland_sock = UnixStream::connect(session_path.join("wayland-0"))?;
        let conn = Connection::from_socket(wayland_sock)
            .map_err(|e| anyhow::anyhow!("wayland connect: {e:?}"))?;
        let mut event_queue = conn.new_event_queue::<WlState>();
        let qh = event_queue.handle();
        let _registry = conn.display().get_registry(&qh, ());

        let mut state = WlState {
            output: None,
            screencast: None,
            fake_input: None,
            stream: None,
            node_id: None,
            failed: None,
            closed: false,
            shutdown: shutdown.clone(),
        };

        event_queue.roundtrip(&mut state)?;

        let output = state.output.clone().ok_or_else(|| anyhow::anyhow!("compositor did not advertise wl_output"))?;
        let screencast = state.screencast.clone().ok_or_else(|| anyhow::anyhow!("compositor did not advertise zkde_screencast_unstable_v1"))?;
        let fake_input = state.fake_input.clone().ok_or_else(|| anyhow::anyhow!("compositor did not advertise org_kde_kwin_fake_input"))?;

        // KWin silently drops input from unauthenticated fake_input clients — no
        // error event, just nothing happens. Must be the first request on the
        // proxy, before any pointer/button/key call.
        fake_input.authenticate("kwin-viewer".into(), "live viewer input forwarding".into());

        let stream = screencast.stream_output(&output, ScPointer::Embedded.into(), &qh, ());
        state.stream = Some(stream);

        // Drive the queue until the stream either succeeds or reports failure.
        let node_id: u32 = loop {
            event_queue.blocking_dispatch(&mut state)?;
            if let Some(err) = state.failed.as_deref() {
                anyhow::bail!("zkde_screencast stream failed: {err}");
            }
            if let Some(id) = state.node_id {
                break id;
            }
        };
        eprintln!("kwin-viewer: connected to pipewire node {node_id}");
        write_status(session_path, "starting", &format!("waiting for a frame from pipewire node {node_id}"));

        // The pipewire loop runs on its own thread and is stopped through this
        // channel, so main can join it instead of leaving it behind on exit.
        let pipewire_sock = session_path.join("pipewire-0");
        let (pw_quit, pw_quit_rx) = pipewire::channel::channel::<()>();
        let pw_thread = {
            let mailbox = Arc::clone(mailbox);
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("pipewire".to_owned())
                .spawn(move || {
                    if let Err(e) = run_pipewire(pipewire_sock, node_id, mailbox, virt, pw_quit_rx, layout) {
                        eprintln!("kwin-viewer: pipewire loop exited: {e}");
                    }
                    // Losing the video feed ends the viewer the same way a dead
                    // wayland connection does.
                    shutdown.request();
                })?
        };

        // The screencast connection keeps serving the queue that carries the
        // stream's Closed and Failed events, and it stays the connection the
        // fake_input requests are sent on. Moving the real state here, instead
        // of dispatching a throwaway copy, is what lets a closed stream end the
        // viewer.
        let wl_dispatch = DispatchThread::spawn("screencast".to_owned(), conn, event_queue, state, shutdown)?;
        let conn = wl_dispatch.connection().clone();
        Ok(Self { fake_input, conn, _wl_dispatch: wl_dispatch, pw_quit, pw_thread })
    }

    // Stop the PipeWire loop and wait for it. The caller has already requested
    // shutdown.
    fn close(self) {
        if self.pw_quit.send(()).is_err() {
            eprintln!("kwin-viewer: pipewire loop already gone");
        }
        if self.pw_thread.join().is_err() {
            eprintln!("kwin-viewer: pipewire thread panicked");
        }
    }
}

fn session_title_fallback(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let id = name
        .strip_prefix("kwin-mcp-")
        .map(|suffix| format!("s{suffix}"))
        .unwrap_or_else(|| name.to_owned());
    format!("kwin-viewer {id}")
}
fn session_title(path: &std::path::Path) -> String {
    std::fs::read_to_string(path.join("viewer-title"))
        .unwrap_or_else(|_| session_title_fallback(path))
}

fn run(session_dir: String, mut argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let virt = parse_size(&mut argv)?;
    let session_path = std::path::PathBuf::from(&session_dir);
    anyhow::ensure!(
        session_path.join("wayland-0").exists(),
        "wayland-0 socket missing in {session_dir} — is the session running?"
    );

    // Do NOT touch process env — env vars are process-global, and the host
    // winit/vulkan stack later inherits whatever we set, making it try to
    // connect to the container instead of the host compositor. Both the
    // wayland Connection and the pipewire Context accept explicit socket
    // paths bypassing the env entirely.

    pipewire::init();

    // One signal for the whole process. Every thread created below watches it,
    // and every one of them sets it when its own connection ends.
    let shutdown = Shutdown::default();

    let host_numlock = NumLockWatcher::spawn(Connection::connect_to_env()?, "host", &shutdown)?;
    let isolated_numlock_sock = UnixStream::connect(session_path.join("wayland-0"))?;
    let isolated_numlock = NumLockWatcher::spawn(
        Connection::from_socket(isolated_numlock_sock)
            .map_err(|e| anyhow::anyhow!("isolated key-state connect: {e:?}"))?,
        "isolated",
        &shutdown,
    )?;
    let numlock = NumLockSync {
        host: host_numlock,
        isolated: isolated_numlock,
        shutdown: shutdown.clone(),
    };

    let clipboard = ClipboardBridge {
        host: Arc::new(Clipboard::spawn(
            Connection::connect_to_env()?,
            "host",
            &shutdown,
        )?),
        isolated: Arc::new(Clipboard::spawn(
            Connection::from_socket(UnixStream::connect(session_path.join("wayland-0"))?)
                .map_err(|e| anyhow::anyhow!("isolated clipboard connect: {e:?}"))?,
            "isolated",
            &shutdown,
        )?),
        restore: None,
        superseded: Arc::default(),
    };

    let mailbox: FrameMailbox = Arc::new((Mutex::new(None), Condvar::new()));
    let link = SessionLink::connect(&session_path, virt, &shutdown, &mailbox, CaptureLayout::Rgba)?;
    numlock.apply(&link.fake_input, &link.conn)?;

    let mut input_state = InputState {
        out: InputOut::Local {
            fake_input: link.fake_input.clone(),
            conn: link.conn.clone(),
            activity: open_input_activity(&session_path)?,
        },
        last_pos: None,
        held_buttons: 0,
        active_touch: None,
        held_keys: HashSet::new(),
        clipboard: Some(clipboard),
    };

    let run_result = window_loop(
        ViewerTitle::Local(&session_path),
        Some(&session_path),
        &mailbox,
        &shutdown,
        virt,
        &mut input_state,
        Some((&numlock, &link.fake_input, &link.conn)),
        None,
    );

    // Ordered teardown, reached on every exit including a window error, which
    // is why the run result is held instead of propagated straight away.
    // Requesting shutdown lets every dispatch thread leave its poll within
    // DISPATCH_POLL_INTERVAL; the pipewire loop is quit through its channel and
    // joined here, and dropping the link and numlock joins the remaining
    // threads and closes their sockets.
    shutdown.request();
    link.close();
    run_result
}

// The viewer window: shows the latest frame from the mailbox and forwards the
// window's input through `input_state`. `status_dir` is where a viewer that
// kwin-mcp started reports readiness; `--remote` has none. `stream` collects
// what a `--stream` viewer presented.
enum ViewerTitle<'a> {
    Local(&'a std::path::Path),
    Remote(&'a Mutex<String>, &'a str),
}
impl ViewerTitle<'_> {
    fn text(&self) -> String {
        match self {
            Self::Local(path) => session_title(path),
            Self::Remote(title, host) => {
                let title = title.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                format!("{title} ({host})")
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn window_loop(
    title: ViewerTitle<'_>,
    status_dir: Option<&std::path::Path>,
    mailbox: &FrameMailbox,
    shutdown: &Shutdown,
    virt: (u32, u32),
    input_state: &mut InputState,
    numlock: Option<(&NumLockSync, &OrgKdeKwinFakeInput, &Connection)>,
    stream: Option<&StreamStats>,
) -> anyhow::Result<()> {
    let initial_title = title.text();
    let initial_scale = (INITIAL_VIEWER_WIDTH / f64::from(virt.0))
        .min(INITIAL_VIEWER_HEIGHT / f64::from(virt.1))
        .min(1.0);
    let initial_size = LogicalSize::new(
        f64::from(virt.0) * initial_scale,
        f64::from(virt.1) * initial_scale,
    );
    let window = WindowBuilder::default()
        .window(|wa| wa.with_title(&initial_title).with_inner_size(initial_size))
        .build()?;
    let device = Arc::clone(&window.device);

    // Source image + its GPU upload buffer are recreated on size change.
    // Starting as None so the first frame triggers allocation.
    let mut src_image: Option<Arc<Image>> = None;
    let mut src_dims: (u32, u32) = (0, 0);
    let ready_reported = Cell::new(false);
    let first_frame_queued = Cell::new(false);
    let mut first_host_redraw = false;
    let mut first_frame_consumed = false;
    let mut startup_error = None;
    let started = Instant::now();
    let last_status_heartbeat = Cell::new(started);
    let window_closed = Cell::new(false);
    let mut shown_title = initial_title;
    let mut next_title_check = Instant::now();
    let mut aspect = ViewerAspect::new(virt);
    let resized = Cell::new(None::<(PhysicalSize<u32>, Instant)>);

    let remote_output = match &input_state.out {
        InputOut::Local { .. } => None,
        InputOut::Remote(sink) => Some(sink.clone()),
    };
    // A stream sender scales to the drawable size, so it is told at the first
    // draw and whenever it changes.
    let reported_size = Cell::new(None::<(u32, u32)>);
    let report_size = |size: PhysicalSize<u32>| {
        let Some(OpSink::Socket(_)) = remote_output.as_ref() else { return };
        let size = (size.width, size.height);
        if size.0 == 0 || size.1 == 0 || reported_size.get() == Some(size) {
            return;
        }
        reported_size.set(Some(size));
        eprintln!("kwin-viewer: stream: drawable size {}x{} reported to the sender", size.0, size.1);
        if let Some(sink) = remote_output.as_ref() {
            sink.send(Op::Size(size.0, size.1));
        }
    };
    // Capture time of the picture staged by the current draw, until its
    // submission is counted.
    let staged_capture = Cell::new(None::<u64>);
    let mut draw_frame = |mut frame: FrameContext<'_>| {
        // Leave as soon as anything the viewer depends on has ended, so the
        // window never sits on a dead session and main can join the threads.
        if shutdown.requested() {
            frame.exit();
            return;
        }
        report_size(frame.window.inner_size());

        if !first_host_redraw {
            first_host_redraw = true;
            eprintln!("kwin-viewer: first host window redraw");
        }

        if !ready_reported.get() && started.elapsed() >= FIRST_FRAME_TIMEOUT {
            let reason=frame_timeout_reason(status_dir);
            if let Some(dir)=status_dir {write_status(dir,"failed",&reason);}
            startup_error=Some(reason);
            shutdown.request();
            frame.exit();
            return;
        }
        if let Some((size, at)) = resized.get()
            && at.elapsed() >= RESIZE_SETTLE
        {
            resized.set(None);
            aspect.resize(frame.window, size);
        }
        if Instant::now() >= next_title_check {
            let next = title.text();
            if next != shown_title {
                frame.window.set_title(&next);
                shown_title = next;
            }
            next_title_check = Instant::now() + Duration::from_millis(250);
        }
        // Consume the latest frame if one arrived; upload into src_image.
        // Retain src_image across frames so when the mailbox is momentarily
        // empty we still re-blit the last known picture instead of flashing
        // black.
        let latest = if first_frame_queued.get() {
            take_frame(mailbox)
        } else {
            match wait_for_first_frame(
                mailbox,
                shutdown,
                started + FIRST_FRAME_TIMEOUT,
            ) {
                Some(frame) => Some(frame),
                None if shutdown.requested() => {
                    frame.exit();
                    return;
                }
                None => {
                    let reason = frame_timeout_reason(status_dir);
                    if let Some(dir) = status_dir {
                        write_status(dir, "failed", &reason);
                    }
                    startup_error = Some(reason);
                    shutdown.request();
                    frame.exit();
                    return;
                }
            }
        };
        let mut session_frame_queued = false;
        if let Some(f) = latest {
            if !first_frame_consumed {
                first_frame_consumed = true;
                eprintln!("kwin-viewer: first PipeWire frame consumed by host window");
            }
            if src_dims != (f.width, f.height) {
                let info = ImageInfo::image_2d(
                    f.width,
                    f.height,
                    vk::Format::R8G8B8A8_UNORM,
                    vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
                );
                match Image::create(&device, info) {
                    Ok(img) => {
                        src_image = Some(Arc::new(img));
                        src_dims = (f.width, f.height);
                    }
                    Err(e) => eprintln!("kwin-viewer: image alloc failed: {e:?}"),
                }
            }
            if let Some(image) = src_image.as_ref() {
                match Buffer::create_from_slice(
                    &device,
                    vk::BufferUsageFlags::TRANSFER_SRC,
                    &f.pixels,
                ) {
                    Ok(staging) => {
                        let staging_node = frame.render_graph.bind_node(staging);
                        let image_node = frame.render_graph.bind_node(image);
                        frame.render_graph.copy_buffer_to_image(staging_node, image_node);
                        session_frame_queued = true;
                        staged_capture.set(Some(f.captured_micros));
                    }
                    Err(e) => eprintln!("kwin-viewer: staging buffer failed: {e:?}"),
                }
            }
        }

        if let Some(image) = src_image.as_ref() {
            let image_node = frame.render_graph.bind_node(image);
            frame
                .render_graph
                .blit_image(image_node, frame.swapchain_image, vk::Filter::LINEAR);
            if session_frame_queued {
                first_frame_queued.set(true);
            }
        } else {
            frame.render_graph.clear_color_image(frame.swapchain_image);
        }

        // winit on Wayland won't re-fire RedrawRequested on its own once it
        // decides the queue is idle, and screen-13-window's about_to_wait
        // hook doesn't reliably keep the pump running when no input events
        // are arriving. Explicitly requesting a redraw each frame guarantees
        // PipeWire's async frame arrivals get picked up.
        frame.window.request_redraw();
    };
    window.run_with_submission_idle_and_event_callbacks(
        &mut draw_frame,
        || {
            if let Some(captured) = staged_capture.take()
                && let Some(stream) = stream
            {
                stream.presented(captured);
            }
            if !ready_reported.get() && first_frame_queued.get() {
                ready_reported.set(true);
                eprintln!("kwin-viewer: first frame submitted for presentation");
                if let Some(dir) = status_dir {
                    write_status(
                        dir,
                        "ready",
                        "first session frame render submitted to the host window",
                    );
                }
                last_status_heartbeat.set(Instant::now());
                if let Some(sink) = remote_output.as_ref() {
                    sink.send(Op::Presented);
                }
            } else if ready_reported.get()
                && !window_closed.get()
                && last_status_heartbeat.get().elapsed() >= VIEWER_STATUS_HEARTBEAT
            {
                if let Some(dir) = status_dir {
                    write_status(dir, "ready", "host window is showing the session");
                }
                last_status_heartbeat.set(Instant::now());
            }
        },
        Some(VIEWER_STATUS_HEARTBEAT),
        || {
            if ready_reported.get()
                && !window_closed.get()
                && last_status_heartbeat.get().elapsed() >= VIEWER_STATUS_HEARTBEAT
            {
                if let Some(dir) = status_dir {
                    write_status(dir, "ready", "host window event loop is responsive");
                }
                last_status_heartbeat.set(Instant::now());
            }
        },
        |window, event| {
            // Input and focus-loss releases must run even when a minimized surface
            // receives no redraws. The idle heartbeat follows this same event loop.
            if matches!(event, WindowEvent::CloseRequested) {
                window_closed.set(true);
                if let Some(dir) = status_dir {
                    write_status(dir, "closed", "host window closed");
                }
                shutdown.request();
            }
            if let WindowEvent::Resized(size) = event {
                resized.set(Some((*size, Instant::now())));
                report_size(*size);
            }
            let size = window.inner_size();
            forward_input(event, (size.width, size.height), virt, input_state, numlock);
            true
        },
    )?;
if let Some(error)=startup_error {anyhow::bail!(error);}
    Ok(())
}

// Follow the changed edge instead of undoing a vertical drag with a width-only
// correction. Window and input dimensions remain physical pixels.
struct ViewerAspect {
    source: (u32, u32),
    previous: Option<PhysicalSize<u32>>,
    requested: Option<(PhysicalSize<u32>, PhysicalSize<u32>)>,
}

impl ViewerAspect {
    fn new(source: (u32, u32)) -> Self {
        Self {
            source,
            previous: None,
            requested: None,
        }
    }

    fn resize(&mut self, window: &winit::window::Window, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        let previous = self.previous.replace(size);
        let height = (u64::from(size.width) * u64::from(self.source.1)
            + u64::from(self.source.0) / 2)
            / u64::from(self.source.0);
        let width = (u64::from(size.height) * u64::from(self.source.0)
            + u64::from(self.source.1) / 2)
            / u64::from(self.source.1);
        let height = u32::try_from(height.clamp(1, u64::from(u32::MAX))).unwrap_or(u32::MAX);
        let width = u32::try_from(width.clamp(1, u64::from(u32::MAX))).unwrap_or(u32::MAX);
        if height == size.height || width == size.width {
            self.requested = None;
            return;
        }
        let follow_width = match previous {
            Some(old) => {
                u64::from(size.width.abs_diff(old.width)) * u64::from(old.height)
                    >= u64::from(size.height.abs_diff(old.height)) * u64::from(old.width)
            }
            None => height <= size.height,
        };
        let target = if follow_width {
            PhysicalSize::new(size.width, height)
        } else {
            PhysicalSize::new(width, size.height)
        };
        if self.requested == Some((size, target)) {
            return;
        }
        self.requested = Some((size, target));
        let applied = window.request_inner_size(target);
        eprintln!(
            "kwin-viewer: aspect resize configured={}x{} requested={}x{} applied={applied:?}",
            size.width, size.height, target.width, target.height
        );
    }
}

// Wire format between `--serve` (on the host that runs the session) and
// `--remote` (on the host with the screen), carried over one ssh stdio pipe.
// Server to client: WIRE_FRAME, a u32 little-endian length, then a PNG of the
// frame. WIRE_TITLE uses the same length prefix followed by UTF-8 title text.
// Client to server: fixed 17-byte input records (see Op::encode).
const WIRE_FRAME: u8 = 1;
const WIRE_TITLE: u8 = 2;

fn write_title(out: &mut impl Write, title: &str) -> anyhow::Result<()> {
    out.write_all(&[WIRE_TITLE])?;
    out.write_all(&u32::try_from(title.len())?.to_le_bytes())?;
    out.write_all(title.as_bytes())?;
    out.flush()?;
    Ok(())
}

// Frames are sent at most this often; the mailbox keeps only the newest.
const SERVE_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const SERVE_IDLE_POLL: Duration = Duration::from_millis(20);

fn write_frame(out: &mut impl Write, frame: &Frame) -> anyhow::Result<()> {
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::FilterType::Sub);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&frame.pixels)?;
    }
    out.write_all(&[WIRE_FRAME])?;
    out.write_all(&u32::try_from(png_bytes.len())?.to_le_bytes())?;
    out.write_all(&png_bytes)?;
    out.flush()?;
    Ok(())
}

// Read one frame; None when the stream ends.
fn read_frame(
    input: &mut impl Read,
    title: Option<&Mutex<String>>,
) -> anyhow::Result<Option<Frame>> {
    loop {
        let mut head = [0u8; 5];
        match input.read_exact(&mut head) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        anyhow::ensure!(
            matches!(head[0], WIRE_FRAME | WIRE_TITLE),
            "unknown message type {}",
            head[0]
        );
        let length = usize::try_from(u32::from_le_bytes([head[1], head[2], head[3], head[4]]))?;
        anyhow::ensure!(length <= 64 << 20, "frame of {length} bytes is too large");
        let mut payload = vec![0u8; length];
        input.read_exact(&mut payload)?;
        if head[0] == WIRE_TITLE {
            let text = String::from_utf8(payload)?;
            if let Some(title) = title {
                *title
                    .lock()
                    .map_err(|_| anyhow::anyhow!("viewer title lock poisoned"))? = text;
            }
            continue;
        }
        let mut reader = png::Decoder::new(Cursor::new(payload)).read_info()?;
        let mut rgba = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut rgba)?;
        rgba.truncate(info.buffer_size());
        return Ok(Some(Frame {
            width: info.width,
            height: info.height,
            pixels: rgba,
            captured_micros: unix_micros(),
            serial: 0,
        }));
    }
}

// Run on the host that owns the session: no window. Streams the session's
// frames to stdout and applies each input record read from stdin immediately.
fn serve(mut argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let session_dir = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let virt = parse_size(&mut argv)?;
    let session_path = std::path::PathBuf::from(&session_dir);
    write_status(&session_path, "starting", "remote viewer");
    pipewire::init();
    let shutdown = Shutdown::default();
    let mailbox: FrameMailbox = Arc::new((Mutex::new(None), Condvar::new()));
    let link = SessionLink::connect(&session_path, virt, &shutdown, &mailbox, CaptureLayout::Rgba)?;
    let input = InputOut::Local {
        fake_input: link.fake_input.clone(),
        conn: link.conn.clone(),
        activity: open_input_activity(&session_path)?,
    };

    let submitted = Arc::new(AtomicBool::new(false));
    // Input records come in until the client's ssh pipe closes. The thread
    // blocks in read, so it is left to end with the process.
    {
        let (input, shutdown) = (input, shutdown.clone());
        let submitted = submitted.clone();
        let session_path = session_path.clone();
        std::thread::Builder::new().name("remote-input".to_owned()).spawn(move || {
            let mut stdin = std::io::stdin().lock();
            while let Ok(Some(op)) = Op::decode(&mut stdin) {
                if matches!(op, Op::Presented) {
                    submitted.store(true, Ordering::Release);
                    write_status(
                        &session_path,
                        "ready",
                        "remote viewer submitted its first frame for presentation",
                    );
                } else {
                    input.send(op);
                }
            }
            shutdown.request();
        })?;
    }

    write_status(&session_path, "starting", "waiting for remote first-frame submission");
    let started=Instant::now();
    let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
    let mut result = Ok(());
    let mut sent_title = String::new();
    while !shutdown.requested() {
        if !submitted.load(Ordering::Acquire) && started.elapsed() >= FIRST_FRAME_TIMEOUT {
            result = Err(anyhow::anyhow!(frame_timeout_reason(Some(&session_path))));
            shutdown.request();
            break;
        }
        let title = session_title(&session_path);
        if title != sent_title {
            if let Err(error) = write_title(&mut stdout,&title) { result=Err(error); break; }
            sent_title=title;
        }
        let latest = take_frame(&mailbox);
        match latest {
            Some(frame) => {
                if let Err(error) = write_frame(&mut stdout, &frame) {
                    result = Err(error);
                    break;
                }
                std::thread::sleep(SERVE_FRAME_INTERVAL);
            }
            None => std::thread::sleep(SERVE_IDLE_POLL),
        }
    }
    shutdown.request();
    link.close();
    match &result {
        Ok(()) => write_status(&session_path,"closed","remote viewer disconnected"),
        Err(error) => write_status(&session_path,"failed",&error.to_string()),
    }
    result
}

// Run on the host with the screen: opens the window and starts `--serve` on
// HOST over ssh. The session keeps running where it is.
/// The Wayland socket of the desktop session in `runtime`, found the way a login
/// finds it: the first `wayland-N` there that still accepts a connection.
fn find_wayland_socket(runtime: &std::path::Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(runtime)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix("wayland-")
                .is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .collect();
    names.sort();
    names.into_iter().find(|name| UnixStream::connect(runtime.join(name)).is_ok())
}

/// `--remote` runs on the machine with the screen, often from an SSH shell that
/// has no display variables although the machine's desktop session is up. Point
/// winit at that session. The local viewer never does this: its environment is
/// set up by kwin-mcp, and a host display must not leak into it.
fn ensure_host_display() -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let present = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if present("WAYLAND_DISPLAY") || present("WAYLAND_SOCKET") || present("DISPLAY") {
        return Ok(());
    }
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(runtime) => PathBuf::from(runtime),
        None => PathBuf::from(format!("/run/user/{}", std::fs::metadata("/proc/self")?.uid())),
    };
    let socket = find_wayland_socket(&runtime).ok_or_else(|| {
        anyhow::anyhow!(
            "no display: WAYLAND_DISPLAY, WAYLAND_SOCKET and DISPLAY are unset and {} has no live wayland-N socket. Run this on the machine whose screen should show the session, from its desktop session or with that session's XDG_RUNTIME_DIR and WAYLAND_DISPLAY set",
            runtime.display()
        )
    })?;
    eprintln!("kwin-viewer: no display variables set; using {socket} in {}", runtime.display());
    // SAFETY: this runs first in `--remote`, before any thread exists, so nothing
    // can read the environment while it changes.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("WAYLAND_DISPLAY", &socket);
    }
    Ok(())
}

fn run_remote(mut argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    ensure_host_display()?;
    let host = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let session_dir = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let virt = parse_size(&mut argv)?;
    let plain = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "-_./@:".contains(c));
    anyhow::ensure!(plain(&host) && plain(&session_dir), "host and session directory must be plain names");
    let remote_bin = std::env::var("KWIN_VIEWER_REMOTE_BIN")
        .ok()
        .or_else(|| std::env::current_exe().ok().map(|path| path.display().to_string()))
        .ok_or_else(|| anyhow::anyhow!("cannot locate the remote kwin-viewer; set KWIN_VIEWER_REMOTE_BIN"))?;
    anyhow::ensure!(plain(&remote_bin), "remote kwin-viewer path must be a plain name");
    let shutdown = Shutdown::default();
    let mailbox: FrameMailbox = Arc::new((Mutex::new(None), Condvar::new()));
    let title = Arc::new(Mutex::new(session_title_fallback(std::path::Path::new(&session_dir))));
    let mut child = std::process::Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3"])
        .arg(&host)
        .arg(&remote_bin)
        .args(["--serve", &session_dir, &virt.0.to_string(), &virt.1.to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("ssh stdin missing"))?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("ssh stdout missing"))?;

    let reader = {
        let title = Arc::clone(&title);
        let (mailbox, shutdown) = (Arc::clone(&mailbox), shutdown.clone());
        std::thread::Builder::new().name("remote-frames".to_owned()).spawn(move || {
            loop {
                match read_frame(&mut stdout,Some(&title)) {
                    Ok(Some(frame)) => {
                        store_frame(&mailbox, frame);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("kwin-viewer: remote stream ended: {error:#}");
                        break;
                    }
                }
            }
            shutdown.request();
        })?
    };

    let mut input_state = InputState {
        out: InputOut::Remote(OpSink::Pipe(Arc::new(Mutex::new(stdin)))),
        last_pos: None,
        held_buttons: 0,
        active_touch: None,
        held_keys: HashSet::new(),
        clipboard: None,
    };
    let result = window_loop(
        ViewerTitle::Remote(&title, &host),
        None,
        &mailbox,
        &shutdown,
        virt,
        &mut input_state,
        None,
        None,
    );
    shutdown.request();
    // Closing the pipe ends `--serve` on the far side; ssh then exits.
    drop(input_state);
    let _ = child.kill();
    let _ = child.wait();
    if reader.join().is_err() {
        eprintln!("kwin-viewer: remote frame thread panicked");
    }
    result
}

// ── Remote viewer stream ────────────────────────────────────────────────────
// `--stream-serve` beside the session and `--stream` on the machine with the
// screen, joined by one WebSocket. kwin_mcp::viewer_stream documents what the
// socket carries and the rule that picks each picture's encoding.

const STREAM_REPORT_INTERVAL: Duration = Duration::from_secs(1);
// While the screen is unchanged the sender wakes this often for the window
// title and its own deadlines.
const STREAM_IDLE_POLL: Duration = Duration::from_millis(250);
// The sender speaks first, once the listener has handed it the socket.
const STREAM_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

type FrameSink = futures::stream::SplitSink<remote::FrameSocket, Message>;

// One worker drives the socket; pictures are coded on the calling thread and
// the scoped threads it starts.
fn stream_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("frame-socket")
        .enable_all()
        .build()?)
}

fn codec_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

fn megabits(bits: u64) -> f64 {
    bits as f64 / 1e6
}

fn bits_per_pixel(bpp_x16: u16) -> f64 {
    f64::from(bpp_x16) / 16.0
}

fn milliseconds(micros: u64, count: u64) -> f64 {
    micros as f64 / 1e3 / count.max(1) as f64
}

// What the viewer told the sender.
struct ViewerLink {
    // Drawable size in physical pixels; None until the first report.
    size: Mutex<Option<(u32, u32)>>,
    presented: AtomicBool,
}

impl ViewerLink {
    fn size(&self) -> Option<(u32, u32)> {
        *self.size.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // Stored under the mailbox lock, which the sender holds from its check of
    // the size until it waits, so the wake-up cannot fall between the two.
    fn resize(&self, mailbox: &FrameMailbox, size: (u32, u32)) {
        let (latest, ready) = &**mailbox;
        let guard = latest.lock();
        *self.size.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(size);
        drop(guard);
        ready.notify_all();
    }
}

struct DscStream {
    size: (u32, u32),
    encoder: dsc::Encoder,
    rate: viewer_stream::RateControl,
    // Whether the viewer has the current picture parameter set.
    pps_sent: bool,
}

// What went out since the last log line.
#[derive(Default)]
struct SendWindow {
    frames: u64,
    bytes: u64,
    pps: u64,
    // Captures identical to the picture already sent.
    unchanged: u64,
    // Captures replaced by a newer one before the sender was free.
    dropped: u64,
    scale: Duration,
    encode: Duration,
    send: Duration,
    slowest: Duration,
}

// The scaler for the sizes of the last picture.
struct SizedScaler {
    source: (u32, u32),
    target: (u32, u32),
    scaler: viewer_stream::Scaler,
}

struct PictureSender {
    // Bits per second; None is unlimited.
    budget: Option<u64>,
    threads: usize,
    sequence: u32,
    scaler: Option<SizedScaler>,
    scaled: Vec<u8>,
    dsc: Option<DscStream>,
    // A size the DSC encoder has no slice layout for goes out raw.
    dsc_unsupported: Option<(u32, u32)>,
    encoded: Vec<u8>,
    // Encoding, size and rate of the last picture, to log each change once.
    mode: Option<(Payload, (u32, u32), u16)>,
    window: SendWindow,
    window_started: Instant,
    last_serial: u64,
}

impl PictureSender {
    fn new(budget: Option<u64>) -> Self {
        Self {
            budget,
            threads: codec_threads(),
            sequence: 0,
            scaler: None,
            scaled: Vec::new(),
            dsc: None,
            dsc_unsupported: None,
            encoded: Vec::new(),
            mode: None,
            window: SendWindow::default(),
            window_started: Instant::now(),
            last_serial: 0,
        }
    }

    // Counts the captures the mailbox replaced before this one was taken.
    fn captured(&mut self, frame: &Frame) {
        self.window.dropped += frame.serial.saturating_sub(self.last_serial + 1);
        self.last_serial = frame.serial;
    }

    // Scales `frame` to the viewer's drawable size and sends it raw or as DSC,
    // whichever the budget allows, then applies the rate rule.
    fn send(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        sink: &mut FrameSink,
        frame: &Frame,
        drawable: (u32, u32),
        captured_micros: u64,
    ) -> anyhow::Result<()> {
        use futures::SinkExt;
        let started = Instant::now();
        let source = (frame.width, frame.height);
        let target = viewer_stream::fit_size(source, drawable);
        let source_stride = frame.width as usize * 4;
        let (pixels, stride): (&[u8], usize) = if target == source {
            (&frame.pixels, source_stride)
        } else {
            let sized = match self.scaler.take() {
                Some(sized) if (sized.source, sized.target) == (source, target) => sized,
                _ => SizedScaler { source, target, scaler: viewer_stream::Scaler::new(source, target)? },
            };
            let sized = self.scaler.insert(sized);
            sized.scaler.scale(&frame.pixels, source_stride, &mut self.scaled, self.threads)?;
            (&self.scaled, target.0 as usize * 4)
        };
        let scaled = Instant::now();

        let mut raw = viewer_stream::raw_fits(target.0, target.1, self.budget)
            || self.dsc_unsupported == Some(target);
        if !raw && self.dsc.as_ref().is_none_or(|dsc| dsc.size != target) {
            match dsc::Config::new(target.0, target.1).and_then(dsc::Encoder::new) {
                Ok(encoder) => {
                    self.dsc = Some(DscStream {
                        size: target,
                        encoder,
                        rate: viewer_stream::RateControl::default(),
                        pps_sent: false,
                    });
                }
                Err(error) => {
                    eprintln!(
                        "kwin-viewer: stream: no DSC layout for {}x{} ({error}); sending raw over the budget",
                        target.0, target.1
                    );
                    self.dsc_unsupported = Some(target);
                    raw = true;
                }
            }
        }
        let mut header = Header {
            payload: Payload::Raw,
            width: u16::try_from(target.0)?,
            height: u16::try_from(target.1)?,
            bpp_x16: viewer_stream::RAW_BPP_X16,
            sequence: self.sequence,
            captured_micros,
            sender_micros: 0,
        };
        let mut message;
        let mut complexity = None;
        match self.dsc.as_mut().filter(|_| !raw) {
            None => {
                message = Vec::with_capacity(viewer_stream::HEADER_BYTES + header.raw_len());
                message.extend_from_slice(&[0; viewer_stream::HEADER_BYTES]);
                viewer_stream::pack_rgb(pixels, stride, target, &mut message, self.threads)?;
            }
            Some(dsc) => {
                header.payload = Payload::Dsc;
                header.bpp_x16 = dsc.encoder.config().bits_per_pixel_x16;
                if !dsc.pps_sent {
                    let mut pps = Header { payload: Payload::Pps, ..header }.encode().to_vec();
                    pps.extend_from_slice(&dsc.encoder.pps().to_bytes());
                    runtime.block_on(sink.send(Message::Binary(pps.into())))?;
                    dsc.pps_sent = true;
                    self.window.pps += 1;
                }
                let picture = dsc::Frame { data: pixels, stride, format: dsc::PixelFormat::Bgrx };
                let stats = dsc.encoder.encode_parallel(&picture, &mut self.encoded, self.threads)?;
                complexity = Some(stats.complexity_permille());
                message = Vec::with_capacity(viewer_stream::HEADER_BYTES + self.encoded.len());
                message.extend_from_slice(&[0; viewer_stream::HEADER_BYTES]);
                message.extend_from_slice(&self.encoded);
            }
        }
        let encoded = Instant::now();
        header.sender_micros =
            u32::try_from(unix_micros().saturating_sub(captured_micros)).unwrap_or(u32::MAX);
        message[..viewer_stream::HEADER_BYTES].copy_from_slice(&header.encode());
        let bytes = message.len();
        runtime.block_on(sink.send(Message::Binary(message.into())))?;
        let sent = Instant::now();
        self.sequence = self.sequence.wrapping_add(1);

        let mode = (header.payload, target, header.bpp_x16);
        if self.mode != Some(mode) {
            self.mode = Some(mode);
            eprintln!(
                "kwin-viewer: stream: sending {} {}x{} at {:.2} bpp, {bytes} bytes per frame ({:.0} Mb/s at {} Hz)",
                if raw { "raw" } else { "dsc" },
                target.0,
                target.1,
                bits_per_pixel(header.bpp_x16),
                megabits(bytes as u64 * 8 * viewer_stream::FRAME_RATE),
                viewer_stream::FRAME_RATE,
            );
        }
        let time = sent - started;
        if let (Some(complexity), Some(dsc)) = (complexity, self.dsc.as_mut()) {
            let before = dsc.rate.bpp_x16();
            let pixels = u64::from(target.0) * u64::from(target.1);
            if dsc.rate.after_frame(pixels, time, complexity, self.budget) {
                let after = dsc.rate.bpp_x16();
                // On an error the encoder keeps its rate, which the header reports.
                match dsc.encoder.set_bits_per_pixel_x16(after) {
                    Ok(()) => {
                        dsc.pps_sent = false;
                        eprintln!(
                            "kwin-viewer: stream: rate {:.2} -> {:.2} bpp (frame took {:.1} ms, complexity {complexity}/1000, {:.0} Mb/s at {} Hz against a budget of {})",
                            bits_per_pixel(before),
                            bits_per_pixel(after),
                            time.as_secs_f64() * 1e3,
                            megabits(viewer_stream::stream_bits_per_second(pixels, before)),
                            viewer_stream::FRAME_RATE,
                            budget_text(self.budget),
                        );
                    }
                    Err(error) => eprintln!(
                        "kwin-viewer: stream: rate stays at {:.2} bpp: {error}",
                        bits_per_pixel(before)
                    ),
                }
            }
        }
        self.window.frames += 1;
        self.window.bytes += bytes as u64;
        self.window.scale += scaled - started;
        self.window.encode += encoded - scaled;
        self.window.send += sent - encoded;
        self.window.slowest = self.window.slowest.max(time);
        Ok(())
    }

    // One log line per second in which something was captured.
    fn report(&mut self) {
        let elapsed = self.window_started.elapsed();
        if elapsed < STREAM_REPORT_INTERVAL {
            return;
        }
        let window = std::mem::take(&mut self.window);
        self.window_started = Instant::now();
        if window.frames == 0 && window.unchanged == 0 && window.dropped == 0 {
            return;
        }
        let per_frame = |total: Duration| total.as_secs_f64() * 1e3 / window.frames.max(1) as f64;
        eprintln!(
            "kwin-viewer: stream: sent {:.1} frames/s, {:.1} Mb/s; per frame scale {:.1} ms, encode {:.1} ms, send {:.1} ms, slowest {:.1} ms; {} parameter sets, {} unchanged captures skipped, {} captures replaced by a newer one",
            window.frames as f64 / elapsed.as_secs_f64(),
            megabits(window.bytes * 8) / elapsed.as_secs_f64(),
            per_frame(window.scale),
            per_frame(window.encode),
            per_frame(window.send),
            window.slowest.as_secs_f64() * 1e3,
            window.pps,
            window.unchanged,
            window.dropped,
        );
    }
}

fn budget_text(budget: Option<u64>) -> String {
    budget.map_or_else(|| "unlimited".to_owned(), |bits| format!("{:.0} Mb/s", megabits(bits)))
}

fn send_notice(
    runtime: &tokio::runtime::Runtime,
    sink: &mut FrameSink,
    notice: &Notice,
) -> anyhow::Result<()> {
    use futures::SinkExt;
    let text = serde_json::to_string(notice)?;
    runtime.block_on(sink.send(Message::text(text)))?;
    Ok(())
}

// Run by `kwin-mcp --listen` beside the session, with the viewer's frame
// socket as stdin: no window. Sends each changed picture of the session,
// scaled to the viewer's size, and applies the input records that come back.
fn stream_serve(mut argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let session_dir = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let session_path = PathBuf::from(&session_dir);
    write_status(&session_path, "starting", "remote viewer stream");
    let result = stream_sender(&session_path, argv);
    match &result {
        Ok(()) => write_status(&session_path, "closed", "remote viewer disconnected"),
        Err(error) => write_status(&session_path, "failed", &format!("{error:#}")),
    }
    result
}

fn stream_sender(
    session_path: &std::path::Path,
    mut argv: impl Iterator<Item = String>,
) -> anyhow::Result<()> {
    use futures::{SinkExt, StreamExt};
    use std::os::fd::AsFd;
    let virt = parse_size(&mut argv)?;
    let link_mbps = match argv.next().as_deref() {
        None => None,
        Some("--link-mbps") => {
            let value = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
            Some(value.parse::<u64>().map_err(|error| anyhow::anyhow!("--link-mbps '{value}': {error}"))?)
        }
        Some(other) => anyhow::bail!("unknown argument '{other}'\n{USAGE}"),
    };
    let socket = std::net::TcpStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let budget = viewer_stream::link_budget(socket.local_addr()?.ip(), link_mbps);
    eprintln!(
        "kwin-viewer: stream: viewer {} over {}; budget {}",
        socket.peer_addr()?,
        budget.link,
        budget_text(budget.bits_per_second)
    );

    pipewire::init();
    let shutdown = Shutdown::default();
    let mailbox: FrameMailbox = Arc::new((Mutex::new(None), Condvar::new()));
    let link = SessionLink::connect(session_path, virt, &shutdown, &mailbox, CaptureLayout::Bgrx)?;
    let input = InputOut::Local {
        fake_input: link.fake_input.clone(),
        conn: link.conn.clone(),
        activity: open_input_activity(session_path)?,
    };
    let runtime = stream_runtime()?;
    let socket = runtime.block_on(remote::adopt_frames(socket)).map_err(anyhow::Error::msg)?;
    let (mut sink, mut stream) = StreamExt::split(socket);

    let viewer = Arc::new(ViewerLink { size: Mutex::new(None), presented: AtomicBool::new(false) });
    runtime.spawn({
        let (viewer, mailbox, shutdown) = (Arc::clone(&viewer), Arc::clone(&mailbox), shutdown.clone());
        let session_path = session_path.to_owned();
        async move {
            while let Some(Ok(message)) = stream.next().await {
                let record = match message {
                    Message::Binary(record) => record,
                    Message::Close(_) => break,
                    Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                };
                let Ok(Some(op)) = Op::decode(&mut &record[..]) else {
                    eprintln!("kwin-viewer: stream: invalid input record of {} bytes", record.len());
                    break;
                };
                match op {
                    Op::Presented => {
                        viewer.presented.store(true, Ordering::Release);
                        write_status(
                            &session_path,
                            "ready",
                            "remote viewer submitted its first frame for presentation",
                        );
                    }
                    Op::Size(width, height) => viewer.resize(&mailbox, (width, height)),
                    Op::Motion(..) | Op::Button(..) | Op::Axis(..) | Op::Key(..) => input.send(op),
                }
            }
            // The frame socket ended: the capture ends with it.
            shutdown.request();
            mailbox.1.notify_all();
        }
    });

    let mut sent_title = session_title(session_path);
    send_notice(
        &runtime,
        &mut sink,
        &Notice::Hello {
            width: virt.0,
            height: virt.1,
            title: sent_title.clone(),
            budget_bits_per_second: budget.bits_per_second,
            link: budget.link.clone(),
        },
    )?;
    let mut sender = PictureSender::new(budget.bits_per_second);
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    // The newest capture, whether it still has to go out, and the size the
    // last picture went out at.
    let mut latest: Option<Frame> = None;
    let mut pending = false;
    let mut sent_size = None;
    let result = loop {
        if shutdown.requested() {
            break Ok(());
        }
        let presented = viewer.presented.load(Ordering::Acquire);
        if !presented && started.elapsed() >= FIRST_FRAME_TIMEOUT {
            break Err(anyhow::anyhow!(
                "remote {}",
                frame_timeout_reason(Some(session_path))
            ));
        }
        if presented && heartbeat.elapsed() >= VIEWER_STATUS_HEARTBEAT {
            write_status(session_path, "ready", "remote viewer is showing the session");
            heartbeat = Instant::now();
        }
        let title = session_title(session_path);
        if title != sent_title {
            if let Err(error) = send_notice(&runtime, &mut sink, &Notice::Title { title: title.clone() }) {
                eprintln!("kwin-viewer: stream: frame socket closed: {error:#}");
                break Ok(());
            }
            sent_title = title;
        }
        // Wait for the screen to change, unless the picture in hand has to go
        // out again because the viewer changed size.
        let fresh = {
            let (slot, ready) = &*mailbox;
            let mut slot = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let resized = latest.is_some() && viewer.size().is_some_and(|size| Some(size) != sent_size);
            if slot.is_none() && !resized && !shutdown.requested() {
                slot = ready
                    .wait_timeout(slot, STREAM_IDLE_POLL)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            slot.take()
        };
        let mut captured_micros = None;
        if let Some(frame) = fresh {
            sender.captured(&frame);
            // PipeWire delivers on damage; a delivery with the same pixels is
            // still not a change and is never sent.
            let unchanged = latest.as_ref().is_some_and(|shown| {
                (shown.width, shown.height) == (frame.width, frame.height) && shown.pixels == frame.pixels
            });
            if unchanged {
                sender.window.unchanged += 1;
            } else {
                pending = true;
                captured_micros = Some(frame.captured_micros);
                latest = Some(frame);
            }
        }
        sender.report();
        let Some(size) = viewer.size() else { continue };
        pending |= Some(size) != sent_size;
        let Some(frame) = latest.as_ref().filter(|_| pending) else { continue };
        // A picture sent again for a new size is timed from now.
        let captured_micros = captured_micros.unwrap_or_else(unix_micros);
        if let Err(error) = sender.send(&runtime, &mut sink, frame, size, captured_micros) {
            if error.is::<async_tungstenite::tungstenite::Error>() {
                eprintln!("kwin-viewer: stream: frame socket closed: {error:#}");
                break Ok(());
            }
            break Err(error);
        }
        pending = false;
        sent_size = Some(size);
    };
    shutdown.request();
    let _ = runtime.block_on(async { tokio::time::timeout(STREAM_CLOSE_TIMEOUT, sink.close()).await });
    link.close();
    result
}

// What a `--stream` viewer received and presented.
#[derive(Default)]
struct StreamStats {
    counters: Mutex<StreamCounters>,
}

#[derive(Default, Clone)]
struct StreamCounters {
    // Since the viewer started.
    frames: u64,
    bytes: u64,
    presented: u64,
    parameter_sets: u64,
    // The last picture.
    width: u16,
    height: u16,
    raw: bool,
    bpp_x16: u16,
    frame_bytes: u64,
    // Since the last report.
    recent: RecentCounters,
}

#[derive(Default, Clone)]
struct RecentCounters {
    frames: u64,
    bytes: u64,
    // Pictures a newer one replaced before they were decoded.
    skipped: u64,
    presented: u64,
    decode_micros: u64,
    sender_micros: u64,
    // Capture on the session's host to submission for presentation here, on
    // the two machines' wall clocks.
    latency_micros: u64,
    latency_max_micros: u64,
}

impl StreamStats {
    fn counters(&self) -> std::sync::MutexGuard<'_, StreamCounters> {
        self.counters.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // A picture was decoded. Its bytes were counted when it arrived.
    fn received(&self, header: &Header, bytes: usize, decode: Duration) {
        let mut counters = self.counters();
        counters.frames += 1;
        counters.width = header.width;
        counters.height = header.height;
        counters.raw = header.payload == Payload::Raw;
        counters.bpp_x16 = header.bpp_x16;
        counters.frame_bytes = bytes as u64;
        counters.recent.frames += 1;
        counters.recent.decode_micros += decode.as_micros() as u64;
        counters.recent.sender_micros += u64::from(header.sender_micros);
    }

    fn presented(&self, captured_micros: u64) {
        let latency = unix_micros().saturating_sub(captured_micros);
        let mut counters = self.counters();
        counters.presented += 1;
        counters.recent.presented += 1;
        counters.recent.latency_micros += latency;
        counters.recent.latency_max_micros = counters.recent.latency_max_micros.max(latency);
    }

    // The counters, with `recent` starting again.
    fn take(&self) -> StreamCounters {
        let mut counters = self.counters();
        let taken = counters.clone();
        counters.recent = RecentCounters::default();
        taken
    }
}

// Once a second: a log line while pictures arrive and one when they stop, and
// the counters file whenever a line is written.
fn report_stream(stats: &StreamStats, status_dir: &std::path::Path, shutdown: &Shutdown) {
    let mut active = false;
    let mut last = Instant::now();
    while !shutdown.requested() {
        std::thread::sleep(DISPATCH_POLL_INTERVAL.min(STREAM_REPORT_INTERVAL));
        let elapsed = last.elapsed();
        if elapsed < STREAM_REPORT_INTERVAL {
            continue;
        }
        last = Instant::now();
        let counters = stats.take();
        let recent = &counters.recent;
        if recent.frames == 0 && recent.presented == 0 && !active {
            continue;
        }
        active = recent.frames > 0 || recent.presented > 0;
        let mode = if counters.raw { "raw" } else { "dsc" };
        let megabits_per_second = megabits(recent.bytes * 8) / elapsed.as_secs_f64();
        let frames_per_second = recent.frames as f64 / elapsed.as_secs_f64();
        let latency = milliseconds(recent.latency_micros, recent.presented);
        let latency_max = milliseconds(recent.latency_max_micros, 1);
        let sender = milliseconds(recent.sender_micros, recent.frames);
        let decode = milliseconds(recent.decode_micros, recent.frames);
        if active {
            eprintln!(
                "kwin-viewer: stream: {frames_per_second:.1} frames/s {mode} {}x{} at {:.2} bpp, {} bytes/frame, {megabits_per_second:.1} Mb/s; capture to present {latency:.1} ms mean, {latency_max:.1} ms max (sender {sender:.1} ms, decode {decode:.1} ms); {} presented, {} skipped for a newer one; {} frames and {} parameter sets in total",
                counters.width,
                counters.height,
                bits_per_pixel(counters.bpp_x16),
                counters.frame_bytes,
                recent.presented,
                recent.skipped,
                counters.frames,
                counters.parameter_sets,
            );
        } else {
            eprintln!(
                "kwin-viewer: stream: idle, 0 frames in the last second; {} frames and {} bytes in total",
                counters.frames, counters.bytes
            );
        }
        let body = serde_json::json!({
            "frames": counters.frames, "bytes": counters.bytes, "presented": counters.presented,
            "parameter_sets": counters.parameter_sets,
            "width": counters.width, "height": counters.height, "mode": mode,
            "bits_per_pixel": bits_per_pixel(counters.bpp_x16), "frame_bytes": counters.frame_bytes,
            "last_second": {
                "frames": recent.frames, "presented": recent.presented, "skipped": recent.skipped,
                "frames_per_second": frames_per_second, "megabits_per_second": megabits_per_second,
                "latency_ms": latency, "latency_max_ms": latency_max,
                "sender_ms": sender, "decode_ms": decode,
            },
        });
        let temporary = status_dir.join(format!("{}.tmp", viewer_stream::COUNTERS_FILE));
        if std::fs::write(&temporary, body.to_string()).is_ok() {
            let _ = std::fs::rename(&temporary, status_dir.join(viewer_stream::COUNTERS_FILE));
        }
    }
}

// The newest picture message that is not decoded yet, with the decoder for
// the parameter set that was current when it arrived.
type Undecoded = Arc<(Mutex<Option<(async_tungstenite::tungstenite::Bytes, Option<Arc<dsc::Decoder>>)>>, Condvar)>;

// One picture message from the sender, decoded for the window.
fn receive_picture(
    message: &[u8],
    decoder: Option<&dsc::Decoder>,
    mailbox: &FrameMailbox,
    stats: &StreamStats,
    threads: usize,
) -> anyhow::Result<()> {
    let arrived = Instant::now();
    let (header, payload) = Header::decode(message)
        .ok_or_else(|| anyhow::anyhow!("unknown frame message of {} bytes", message.len()))?;
    let (width, height) = (usize::from(header.width), usize::from(header.height));
    let mut rgba = Vec::new();
    match header.payload {
        Payload::Pps => anyhow::bail!("a picture parameter set where a picture belongs"),
        Payload::Raw => {
            anyhow::ensure!(
                payload.len() == header.raw_len(),
                "raw {width}x{height} frame has {} bytes",
                payload.len()
            );
            viewer_stream::unpack_rgb(payload, width, &mut rgba, threads);
        }
        Payload::Dsc => {
            let decoder = decoder
                .filter(|decoder| {
                    (decoder.pps().pic_width, decoder.pps().pic_height) == (header.width, header.height)
                })
                .ok_or_else(|| anyhow::anyhow!("DSC {width}x{height} frame without its parameter set"))?;
            rgba.resize(width * height * 4, 0);
            decoder.decode_parallel(payload, &mut rgba, width * 4, dsc::PixelFormat::Rgbx, threads)?;
        }
    }
    stats.received(&header, message.len(), arrived.elapsed());
    store_frame(
        mailbox,
        Frame {
            width: u32::from(header.width),
            height: u32::from(header.height),
            pixels: rgba,
            captured_micros: header.captured_micros,
            serial: u64::from(header.sequence),
        },
    );
    Ok(())
}

// Started by kwin-mcp `viewer_open` for a session on another machine: the
// window on this machine, the session's pictures from that machine's listener.
fn run_stream(mut argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let authority = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let status_dir = PathBuf::from(argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?);
    write_status(&status_dir, "starting", "connecting to the session's host");
    let result = stream_viewer(&authority, &status_dir);
    match &result {
        Ok(()) => write_status(&status_dir, "closed", "viewer exited: window closed or stream ended"),
        Err(error) => write_status(&status_dir, "failed", &format!("{error:#}")),
    }
    result
}

fn stream_viewer(authority: &str, status_dir: &std::path::Path) -> anyhow::Result<()> {
    use futures::{SinkExt, StreamExt};
    ensure_host_display()?;
    let ticket = std::env::var(remote::TICKET_ENV).map_err(|_| {
        anyhow::anyhow!("{} is not set; kwin-mcp viewer_open starts this viewer", remote::TICKET_ENV)
    })?;
    let secret = remote::read_secret().map_err(anyhow::Error::msg)?;
    let runtime = stream_runtime()?;
    let socket = runtime
        .block_on(remote::connect_frames(authority, &secret, &ticket))
        .map_err(|error| anyhow::anyhow!("frame socket to {authority}: {error}"))?;
    let (mut sink, mut stream) = StreamExt::split(socket);
    let hello = runtime.block_on(async { tokio::time::timeout(STREAM_HELLO_TIMEOUT, stream.next()).await });
    let (virt, first_title) = match hello {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Notice>(text.as_str())? {
            Notice::Hello { width, height, title, budget_bits_per_second, link } => {
                eprintln!(
                    "kwin-viewer: stream: session {width}x{height} on {authority}; sender link {link}, budget {}",
                    budget_text(budget_bits_per_second)
                );
                ((width, height), title)
            }
            Notice::Title { .. } => anyhow::bail!("{authority} did not begin the frame socket with hello"),
        },
        Ok(Some(Ok(_))) => anyhow::bail!("{authority} did not begin the frame socket with hello"),
        Ok(Some(Err(error))) => anyhow::bail!("frame socket to {authority}: {error}"),
        Ok(None) => anyhow::bail!(
            "{authority} closed the frame socket before sending anything: the session's capture did not start"
        ),
        Err(_) => anyhow::bail!("{authority} sent nothing on the frame socket within {STREAM_HELLO_TIMEOUT:?}"),
    };
    anyhow::ensure!(virt.0 > 0 && virt.1 > 0, "session dimensions must be positive");

    let title = Arc::new(Mutex::new(first_title));
    let shutdown = Shutdown::default();
    let mailbox: FrameMailbox = Arc::new((Mutex::new(None), Condvar::new()));
    let stats = Arc::new(StreamStats::default());

    // Input records go out as they are queued, whatever the frame thread does.
    let (ops, mut queued) = tokio::sync::mpsc::unbounded_channel::<Op>();
    let (closed, sink_closed) = tokio::sync::oneshot::channel::<()>();
    runtime.spawn(async move {
        while let Some(op) = queued.recv().await {
            if sink.send(Message::Binary(op.encode().to_vec().into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
        let _ = closed.send(());
    });
    // One thread takes messages off the socket as they arrive and keeps only
    // the newest picture; another decodes it. A viewer that decodes slower
    // than pictures arrive therefore skips to the newest instead of falling
    // behind, and the sender's send time measures the link, not this machine.
    let undecoded: Undecoded = Arc::new((Mutex::new(None), Condvar::new()));
    {
        let (undecoded, mailbox, stats, shutdown) =
            (Arc::clone(&undecoded), Arc::clone(&mailbox), Arc::clone(&stats), shutdown.clone());
        std::thread::Builder::new().name("stream-decode".to_owned()).spawn(move || {
            let threads = codec_threads();
            let (newest, arrived) = &*undecoded;
            loop {
                let picture = {
                    let mut newest = newest.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    while newest.is_none() && !shutdown.requested() {
                        newest = arrived
                            .wait_timeout(newest, DISPATCH_POLL_INTERVAL)
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0;
                    }
                    newest.take()
                };
                let Some((message, decoder)) = picture else { break };
                if let Err(error) = receive_picture(&message, decoder.as_deref(), &mailbox, &stats, threads) {
                    eprintln!("kwin-viewer: stream: {error:#}");
                    shutdown.request();
                    break;
                }
            }
        })?;
    }
    {
        let (title, stats, shutdown) = (Arc::clone(&title), Arc::clone(&stats), shutdown.clone());
        let handle = runtime.handle().clone();
        std::thread::Builder::new().name("stream-frames".to_owned()).spawn(move || {
            let mut decoder: Option<Arc<dsc::Decoder>> = None;
            loop {
                match handle.block_on(stream.next()) {
                    Some(Ok(Message::Binary(message))) => {
                        let parameter_set = match Header::decode(&message) {
                            Some((header, payload)) if header.payload == Payload::Pps => Some(payload),
                            Some(_) => None,
                            None => {
                                eprintln!("kwin-viewer: stream: unknown frame message of {} bytes", message.len());
                                break;
                            }
                        };
                        if let Some(parameter_set) = parameter_set {
                            match dsc::Pps::from_bytes(parameter_set).and_then(dsc::Decoder::new) {
                                Ok(next) => decoder = Some(Arc::new(next)),
                                Err(error) => {
                                    eprintln!("kwin-viewer: stream: picture parameter set: {error}");
                                    break;
                                }
                            }
                            stats.counters().parameter_sets += 1;
                            continue;
                        }
                        {
                            let mut counters = stats.counters();
                            counters.bytes += message.len() as u64;
                            counters.recent.bytes += message.len() as u64;
                        }
                        let (newest, arrived) = &*undecoded;
                        let replaced = newest
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .replace((message, decoder.clone()));
                        if replaced.is_some() {
                            stats.counters().recent.skipped += 1;
                        }
                        arrived.notify_one();
                    }
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Notice>(text.as_str()) {
                        Ok(Notice::Title { title: next }) => {
                            *title.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = next;
                        }
                        Ok(Notice::Hello { .. }) => {}
                        Err(error) => eprintln!("kwin-viewer: stream: unreadable notice: {error}"),
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        eprintln!("kwin-viewer: stream: the session's host closed the frame socket");
                        break;
                    }
                    Some(Err(error)) => {
                        eprintln!("kwin-viewer: stream: frame socket: {error}");
                        break;
                    }
                }
            }
            shutdown.request();
        })?;
    }
    {
        let (stats, shutdown, status_dir) = (Arc::clone(&stats), shutdown.clone(), status_dir.to_owned());
        std::thread::Builder::new()
            .name("stream-report".to_owned())
            .spawn(move || report_stream(&stats, &status_dir, &shutdown))?;
    }

    let mut input_state = InputState {
        out: InputOut::Remote(OpSink::Socket(ops)),
        last_pos: None,
        held_buttons: 0,
        active_touch: None,
        held_keys: HashSet::new(),
        clipboard: None,
    };
    let result = window_loop(
        ViewerTitle::Remote(&title, authority),
        Some(status_dir),
        &mailbox,
        &shutdown,
        virt,
        &mut input_state,
        None,
        Some(&stats),
    );
    shutdown.request();
    // Dropping the queue closes the socket, which ends the capture on the
    // session's host. The frame thread may still be parked on the socket; it
    // ends with the process.
    drop(input_state);
    let _ = runtime.block_on(async { tokio::time::timeout(STREAM_CLOSE_TIMEOUT, sink_closed).await });
    result
}

#[derive(Clone, Copy)]
enum Op {
    Motion(f64, f64),
    Button(u32, u32),
    Axis(u32, f64),
    Key(u32, u32),
    // The remote viewer has queued its first rendered frame for presentation.
    Presented,
    // A stream viewer's drawable size in physical pixels.
    Size(u32, u32),
}

impl Op {
    // One input record on the remote pipe: a tag and two 8-byte little-endian
    // words (coordinates as f64 bits, codes and states as u64).
    fn encode(self) -> [u8; 17] {
        let (tag, a, b) = match self {
            Op::Motion(x, y) => (1u8, x.to_bits(), y.to_bits()),
            Op::Button(code, state) => (2, u64::from(code), u64::from(state)),
            Op::Axis(axis, value) => (3, u64::from(axis), value.to_bits()),
            Op::Key(code, state) => (4, u64::from(code), u64::from(state)),
            Op::Presented => (5,0,0),
            Op::Size(width, height) => (6, u64::from(width), u64::from(height)),
        };
        let mut record = [0u8; 17];
        record[0] = tag;
        record[1..9].copy_from_slice(&a.to_le_bytes());
        record[9..17].copy_from_slice(&b.to_le_bytes());
        record
    }

    // Read one record; None when the pipe closed or the record is not valid.
    fn decode(input: &mut impl Read) -> std::io::Result<Option<Op>> {
        let mut record = [0u8; 17];
        match input.read_exact(&mut record) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        let word = |at: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&record[at..at + 8]);
            u64::from_le_bytes(bytes)
        };
        let (a, b) = (word(1), word(9));
        let narrow = |value: u64| u32::try_from(value).ok();
        Ok(match record[0] {
            1 => Some(Op::Motion(f64::from_bits(a), f64::from_bits(b))),
            2 => narrow(a).zip(narrow(b)).map(|(code, state)| Op::Button(code, state)),
            3 => narrow(a).map(|axis| Op::Axis(axis, f64::from_bits(b))),
            4 => narrow(a).zip(narrow(b)).map(|(code, state)| Op::Key(code, state)),
            5 if a==0 && b==0 => Some(Op::Presented),
            6 => narrow(a).zip(narrow(b)).map(|(width, height)| Op::Size(width, height)),
            _ => None,
        })
    }

    /// What kind of input this is, for the activity marker.
    fn kind(&self) -> &'static str {
        match self {
            Op::Motion(..) => "pointer motion",
            Op::Button(..) => "mouse button",
            Op::Axis(..) => "scroll wheel",
            Op::Key(..) => "key",
            Op::Presented => "presented frame",
            Op::Size(..) => "viewer size",
        }
    }

    fn emit(self, fake_input: &OrgKdeKwinFakeInput) {
        match self {
            Op::Motion(x, y) => fake_input.pointer_motion_absolute(x, y),
            Op::Button(code, state) => fake_input.button(code, state),
            Op::Axis(axis, value) => fake_input.axis(axis, value),
            Op::Key(code, state) => fake_input.keyboard_key(code, state),
            Op::Presented | Op::Size(..) => {}
        }
    }
}

// The way to the process beside the session: the ssh pipe of `--serve`, or the
// frame socket of `--stream-serve` (a task writes each record as one binary
// WebSocket message).
#[derive(Clone)]
enum OpSink {
    Pipe(Arc<Mutex<std::process::ChildStdin>>),
    Socket(tokio::sync::mpsc::UnboundedSender<Op>),
}

impl OpSink {
    fn send(&self, op: Op) {
        match self {
            OpSink::Pipe(pipe) => send_remote_op(pipe, op),
            OpSink::Socket(queue) => {
                if queue.send(op).is_err() {
                    eprintln!("kwin-viewer: frame socket closed");
                }
            }
        }
    }
}

// Where forwarded input goes: directly to the local compositor, or to the
// process beside a remote session that forwards it to its local compositor.
enum InputOut {
    Local {
        fake_input: OrgKdeKwinFakeInput,
        conn: Connection,
        activity: std::fs::File,
    },
    Remote(OpSink),
}

impl InputOut {
    fn send(&self, op: Op) {
        match self {
            InputOut::Local {
                fake_input,
                conn,
                activity,
            } => {
                if !matches!(op, Op::Presented | Op::Size(..)) {
                    // The marker's time is the last input; its content names the
                    // kind, so a pause the user did not cause can be traced.
                    let kind = op.kind();
                    if let Err(error) = activity
                        .set_len(kind.len() as u64)
                        .and_then(|()| std::os::unix::fs::FileExt::write_all_at(activity, kind.as_bytes(), 0))
                        .and_then(|()| {
                            activity.set_times(
                                std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                            )
                        })
                    {
                        eprintln!("kwin-viewer: update input activity marker: {error}");
                    }
                }
                op.emit(fake_input);
                let _ = conn.flush();
            }
            InputOut::Remote(sink) => sink.send(op),
        }
    }
}

fn send_remote_op(pipe: &Arc<Mutex<std::process::ChildStdin>>, op: Op) {
    let Ok(mut pipe) = pipe.lock() else { return };
    if pipe
        .write_all(&op.encode())
        .and_then(|()| pipe.flush())
        .is_err()
    {
        eprintln!("kwin-viewer: remote input pipe closed");
    }
}

struct InputState {
    out: InputOut,
    // Last cursor position in window pixel coords, updated on every
    // CursorMoved regardless of whether the move is forwarded. Needed so a
    // fresh click can snap the container's cursor to the click position
    // before the button press, without ever leaking intervening moves.
    last_pos: Option<(f64, f64)>,
    // Currently-held mouse buttons. Non-empty means we're in a drag and
    // pointer motions should be forwarded so the drag actually drags.
    held_buttons: u32,
    // One touch controls the same pointer used by local and remote mouse input.
    active_touch: Option<u64>,
    // Evdev keycodes currently held inside the container. We forcibly
    // release them on focus loss; otherwise a missed Released event (e.g.
    // user releases Shift outside the viewer window) leaves a modifier
    // stuck inside the container, and every subsequent letter the user
    // types arrives shifted — looks exactly like "I cant type."
    held_keys: HashSet<u32>,
    // Copy/paste handoff between the host and the session clipboards; a remote
    // viewer has none yet.
    clipboard: Option<ClipboardBridge>,
}

fn map_window_to_virtual(pos: (f64, f64), win_w: u32, win_h: u32, virt: (u32, u32)) -> Option<(f64, f64)> {
    if win_w == 0 || win_h == 0 { return None }
    Some((
        pos.0 * f64::from(virt.0) / f64::from(win_w),
        pos.1 * f64::from(virt.1) / f64::from(win_h),
    ))
}

fn forward_input(
    event: &WindowEvent,
    window_size: (u32, u32),
    virt: (u32, u32),
    state: &mut InputState,
    numlock: Option<(&NumLockSync, &OrgKdeKwinFakeInput, &Connection)>,
) {
    if let WindowEvent::Focused(true) = event
        && let Some((numlock, fake_input, conn)) = numlock
        && let Err(error) = numlock.apply(fake_input, conn)
    {
        eprintln!("kwin-viewer: Num Lock synchronization failed: {error}");
    }
    if let WindowEvent::Focused(false) = event {
        if state.active_touch.take().is_some() {
            state.out.send(Op::Button(BTN_LEFT, 0));
        }
        // Drain any keys that were forwarded as pressed but whose Released
        // event we may not see — release them all so no modifier stays
        // stuck inside the container while the user is elsewhere on host.
        for &code in &state.held_keys {
            state.out.send(Op::Key(code, 0));
        }
        if !state.held_keys.is_empty() {
            eprintln!(
                "kwin-viewer: focus lost — released {} held key(s)",
                state.held_keys.len()
            );
            state.held_keys.clear();
        }
    }
    match event {
        WindowEvent::Touch(touch) => {
            let begin = touch.phase == TouchPhase::Started
                && state.active_touch.is_none()
                && state.held_buttons == 0;
            let mapped = map_window_to_virtual(
                (touch.location.x, touch.location.y),
                window_size.0,
                window_size.1,
                virt,
            );
            if begin {
                if mapped.is_none() {
                    return;
                }
                state.active_touch = Some(touch.id);
            }
            if state.active_touch != Some(touch.id) {
                return;
            }
            if touch.phase != TouchPhase::Cancelled
                && let Some((x, y)) = mapped
            {
                state.last_pos = Some((touch.location.x, touch.location.y));
                state.out.send(Op::Motion(x, y));
            }
            if begin {
                state.out.send(Op::Button(BTN_LEFT, 1));
            } else if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                state.out.send(Op::Button(BTN_LEFT, 0));
                state.active_touch = None;
            }
        }
        WindowEvent::CursorMoved { position, .. } => {
            // Keep the latest host cursor position for a subsequent click,
            // while forwarding motion only during an active drag. Idle hover
            // does not change the agent's session pointer.
            state.last_pos = Some((position.x, position.y));
            if state.held_buttons == 0 {
                return;
            }
            if let Some((x, y)) =
                map_window_to_virtual((position.x, position.y), window_size.0, window_size.1, virt)
            {
                state.out.send(Op::Motion(x, y));
            }
        }
        WindowEvent::MouseInput {
            state: btn_state,
            button,
            ..
        } => {
            if state.active_touch.is_some() {
                return;
            }
            let code = match button {
                MouseButton::Left => BTN_LEFT,
                MouseButton::Right => BTN_RIGHT,
                MouseButton::Middle => BTN_MIDDLE,
                _ => return,
            };
            let pressed = matches!(btn_state, ElementState::Pressed);
            if pressed {
                // Snap the session cursor to the latest host position before
                // the button press so the click lands where the user expects.
                if let Some(pos) = state.last_pos
                    && let Some((x, y)) =
                        map_window_to_virtual(pos, window_size.0, window_size.1, virt)
                {
                    state.out.send(Op::Motion(x, y));
                }
                state.held_buttons = state.held_buttons.saturating_add(1);
            } else {
                state.held_buttons = state.held_buttons.saturating_sub(1);
            }
            state
                .out
                .send(Op::Button(code, if pressed { 1 } else { 0 }));
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (dx, dy) = match delta {
                MouseScrollDelta::LineDelta(x, y) => (f64::from(*x) * 15.0, f64::from(*y) * 15.0),
                MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
            };
            if dy != 0.0 {
                state.out.send(Op::Axis(AXIS_VERTICAL, -dy));
            }
            if dx != 0.0 {
                state.out.send(Op::Axis(AXIS_HORIZONTAL, -dx));
            }
        }
        WindowEvent::KeyboardInput { event: key, .. } => {
            let PhysicalKey::Code(kc) = key.physical_key else {
                return;
            };
            let Some(evdev) = key_code_to_evdev(kc) else {
                return;
            };
            let op = key_op(evdev, key.state, key.repeat);
            if matches!(key.state, ElementState::Pressed) {
                let first_press = state.held_keys.insert(evdev);
                if first_press
                    && let Some(chord) = clipboard_chord(evdev, &state.held_keys)
                    && let Some(bridge) = state.clipboard.as_mut()
                    && let Err(error) = bridge.before(chord)
                {
                    eprintln!("kwin-viewer: clipboard handoff failed: {error:#}");
                }
            } else {
                state.held_keys.remove(&evdev);
            }
            if let Some(op) = op {
                state.out.send(op);
            }
        }
        _ => {}
    }
}

/// Forward initial key presses and releases. The session generates its own
/// repeats for a held key, so forwarding the host's repeats would double them.
fn key_op(evdev: u32, state: ElementState, repeat: bool) -> Option<Op> {
    if repeat {
        return None;
    }
    Some(Op::Key(
        evdev,
        u32::from(matches!(state, ElementState::Pressed)),
    ))
}

fn key_code_to_evdev(kc: winit::keyboard::KeyCode) -> Option<u32> {
    use winit::keyboard::KeyCode as K;
    // Linux input-event-codes.h values. Intentionally a flat match — expands
    // only for keys we actually need to forward.
    Some(match kc {
        K::KeyA => 30, K::KeyB => 48, K::KeyC => 46, K::KeyD => 32, K::KeyE => 18,
        K::KeyF => 33, K::KeyG => 34, K::KeyH => 35, K::KeyI => 23, K::KeyJ => 36,
        K::KeyK => 37, K::KeyL => 38, K::KeyM => 50, K::KeyN => 49, K::KeyO => 24,
        K::KeyP => 25, K::KeyQ => 16, K::KeyR => 19, K::KeyS => 31, K::KeyT => 20,
        K::KeyU => 22, K::KeyV => 47, K::KeyW => 17, K::KeyX => 45, K::KeyY => 21,
        K::KeyZ => 44,
        K::Digit0 => 11, K::Digit1 => 2, K::Digit2 => 3, K::Digit3 => 4, K::Digit4 => 5,
        K::Digit5 => 6, K::Digit6 => 7, K::Digit7 => 8, K::Digit8 => 9, K::Digit9 => 10,
        K::Enter => 28, K::Escape => 1, K::Backspace => 14, K::Tab => 15, K::Space => 57,
        K::Minus => 12, K::Equal => 13,
        K::BracketLeft => 26, K::BracketRight => 27, K::Backslash => 43, K::Semicolon => 39,
        K::Quote => 40, K::Backquote => 41, K::Comma => 51, K::Period => 52, K::Slash => 53,
        K::CapsLock => 58,
        K::NumLock => evdev_code(evdev::KeyCode::KEY_NUMLOCK),
        K::Numpad0 => evdev_code(evdev::KeyCode::KEY_KP0),
        K::Numpad1 => evdev_code(evdev::KeyCode::KEY_KP1),
        K::Numpad2 => evdev_code(evdev::KeyCode::KEY_KP2),
        K::Numpad3 => evdev_code(evdev::KeyCode::KEY_KP3),
        K::Numpad4 => evdev_code(evdev::KeyCode::KEY_KP4),
        K::Numpad5 => evdev_code(evdev::KeyCode::KEY_KP5),
        K::Numpad6 => evdev_code(evdev::KeyCode::KEY_KP6),
        K::Numpad7 => evdev_code(evdev::KeyCode::KEY_KP7),
        K::Numpad8 => evdev_code(evdev::KeyCode::KEY_KP8),
        K::Numpad9 => evdev_code(evdev::KeyCode::KEY_KP9),
        K::NumpadAdd => evdev_code(evdev::KeyCode::KEY_KPPLUS),
        K::NumpadComma => evdev_code(evdev::KeyCode::KEY_KPCOMMA),
        K::NumpadDecimal => evdev_code(evdev::KeyCode::KEY_KPDOT),
        K::NumpadDivide => evdev_code(evdev::KeyCode::KEY_KPSLASH),
        K::NumpadEnter => evdev_code(evdev::KeyCode::KEY_KPENTER),
        K::NumpadEqual => evdev_code(evdev::KeyCode::KEY_KPEQUAL),
        K::NumpadMultiply => evdev_code(evdev::KeyCode::KEY_KPASTERISK),
        K::NumpadSubtract => evdev_code(evdev::KeyCode::KEY_KPMINUS),
        K::F1 => 59, K::F2 => 60, K::F3 => 61, K::F4 => 62, K::F5 => 63, K::F6 => 64,
        K::F7 => 65, K::F8 => 66, K::F9 => 67, K::F10 => 68, K::F11 => 87, K::F12 => 88,
        K::ArrowUp => 103, K::ArrowDown => 108, K::ArrowLeft => 105, K::ArrowRight => 106,
        K::Home => 102, K::End => 107, K::PageUp => 104, K::PageDown => 109,
        K::Delete => 111, K::Insert => 110,
        K::ShiftLeft => 42, K::ShiftRight => 54,
        K::ControlLeft => 29, K::ControlRight => 97,
        K::AltLeft => 56, K::AltRight => 100,
        K::SuperLeft => 125, K::SuperRight => 126,
        _ => return None,
    })
}

// PipeWire path: connect to the container's PIPEWIRE_REMOTE socket, create an
// input stream targeting the screencast node KWin handed us, advertise SHM
// RGBA-family formats, and copy each frame into the mailbox.
fn run_pipewire(
    socket_path: PathBuf,
    node_id: u32,
    mailbox: FrameMailbox,
    virt: (u32, u32),
    quit: pipewire::channel::Receiver<()>,
    layout: CaptureLayout,
) -> anyhow::Result<()> {
    use pipewire as pw;
    use pw::spa;
    use spa::pod::Pod;

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    // Lets main stop this loop and join the thread instead of leaving it
    // running on a session that is going away.
    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    // remote.name as an absolute path bypasses XDG_RUNTIME_DIR joining, so we
    // can keep the host's env untouched and still land on the container's
    // pipewire socket.
    let remote_props = pw::properties::properties! {
        *pw::keys::REMOTE_NAME => socket_path.to_string_lossy().to_string(),
    };
    let core = context.connect_rc(Some(remote_props))?;

    struct UserData {
        format: spa::param::video::VideoInfoRaw,
        mailbox: FrameMailbox,
        first_frame_received: bool,
        layout: CaptureLayout,
        serial: u64,
    }
    let data = UserData {
        format: spa::param::video::VideoInfoRaw::default(),
        mailbox,
        first_frame_received: false,
        layout,
        serial: 0,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "kwin-viewer",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state_path=socket_path.parent().unwrap_or(std::path::Path::new(".")).join("viewer-pipewire.json");
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| {
            eprintln!("kwin-viewer: pw stream {old:?} -> {new:?}");
            let temporary=state_path.with_extension("tmp");
            if std::fs::write(&temporary,serde_json::json!({"node_id":node_id,"state":format!("{new:?}")}).to_string()).is_ok() {
                let _=std::fs::rename(temporary,&state_path);
            }
        })
        .param_changed(|_, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() { return }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else { return };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if ud.format.parse(param).is_err() { return }
            eprintln!(
                "kwin-viewer: negotiated {:?} {}x{} @ {}/{}",
                ud.format.format(),
                ud.format.size().width,
                ud.format.size().height,
                ud.format.framerate().num,
                ud.format.framerate().denom,
            );
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() { return }
            let d = &mut datas[0];
            let chunk = d.chunk();
            let size = chunk.size() as usize;
            let stride = chunk.stride() as usize;
            let Some(raw) = d.data() else { return };
            if raw.is_empty() || size == 0 { return }
            let w = ud.format.size().width;
            let h = ud.format.size().height;
            if w == 0 || h == 0 { return }
            let src_fmt = ud.format.format();

            let captured_micros = unix_micros();
            let pixels = match ud.layout {
                CaptureLayout::Rgba => {
                    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
                    convert_to_rgba(raw, &mut rgba, w as usize, h as usize, stride, src_fmt);
                    rgba
                }
                CaptureLayout::Bgrx => copy_as_bgrx(raw, w as usize, h as usize, stride, src_fmt),
            };
            ud.serial += 1;

            if !ud.first_frame_received {
                eprintln!(
                    "kwin-viewer: first PipeWire frame received: {}x{}, chunk={}, stride={}, mapped={}",
                    w,
                    h,
                    size,
                    stride,
                    raw.len()
                );
                ud.first_frame_received = true;
            }
            store_frame(&ud.mailbox, Frame { width: w, height: h, pixels, captured_micros, serial: ud.serial });
        })
        .register()?;

    let format_pod = build_format_pod(virt)?;
    let mut params = [Pod::from_bytes(&format_pod).ok_or_else(|| anyhow::anyhow!("format pod invalid"))?];

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    mainloop.run();
    Ok(())
}

fn build_format_pod(virt: (u32, u32)) -> anyhow::Result<Vec<u8>> {
    use pipewire as pw;
    use pw::spa;
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice, Enum, Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice, Range, Rectangle,
            spa::utils::Rectangle { width: virt.0, height: virt.1 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice, Range, Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("pod serialize: {e}"))?
    .0
    .into_inner();
    Ok(bytes)
}

// The captured rows without their stride padding, as BGRx: the layout KWin
// delivers and the DSC encoder and the raw packer read.
fn copy_as_bgrx(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    fmt: pipewire::spa::param::video::VideoFormat,
) -> Vec<u8> {
    use pipewire::spa::param::video::VideoFormat as F;
    let row = w * 4;
    let mut out = Vec::with_capacity(row * h);
    for y in 0..h {
        let Some(line) = src.get(y * stride..y * stride + row) else { break };
        out.extend_from_slice(line);
    }
    out.resize(row * h, 0);
    if matches!(fmt, F::RGBA | F::RGBx) {
        for pixel in out.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    out
}

fn convert_to_rgba(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    h: usize,
    stride: usize,
    fmt: pipewire::spa::param::video::VideoFormat,
) {
    use pipewire::spa::param::video::VideoFormat as F;
    for y in 0..h {
        let row_off = y * stride;
        let dst_off = y * w * 4;
        if row_off + w * 4 > src.len() || dst_off + w * 4 > dst.len() { break }
        let row = &src[row_off..row_off + w * 4];
        let out = &mut dst[dst_off..dst_off + w * 4];
        match fmt {
            F::RGBA | F::RGBx => out.copy_from_slice(row),
            F::BGRA | F::BGRx => {
                for x in 0..w {
                    let i = x * 4;
                    out[i] = row[i + 2];
                    out[i + 1] = row[i + 1];
                    out[i + 2] = row[i];
                    out[i + 3] = if matches!(fmt, F::BGRA) { row[i + 3] } else { 255 };
                }
            }
            _ => {
                // Unsupported — paint magenta so the bug is visible.
                for x in 0..w {
                    let i = x * 4;
                    out[i] = 255; out[i + 1] = 0; out[i + 2] = 255; out[i + 3] = 255;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod display_discovery_tests {
    use super::find_wayland_socket;
    use std::os::unix::net::UnixListener;

    #[test]
    fn the_live_wayland_socket_wins_over_stale_and_unrelated_names() {
        let runtime = std::env::temp_dir().join(format!("kwin-viewer-display-{}", std::process::id()));
        std::fs::create_dir_all(&runtime).expect("create runtime directory");
        // A bound-then-dropped listener leaves a socket file nothing answers on.
        drop(UnixListener::bind(runtime.join("wayland-0")).expect("stale socket"));
        let _live = UnixListener::bind(runtime.join("wayland-1")).expect("live socket");
        std::fs::write(runtime.join("wayland-0.lock"), []).expect("lock file");
        std::fs::write(runtime.join("wayland-x"), []).expect("unrelated file");
        let found = find_wayland_socket(&runtime);
        let _ = std::fs::remove_dir_all(&runtime);
        assert_eq!(found.as_deref(), Some("wayland-1"));
    }

    #[test]
    fn no_display_socket_is_none() {
        let runtime = std::env::temp_dir().join(format!("kwin-viewer-nodisplay-{}", std::process::id()));
        std::fs::create_dir_all(&runtime).expect("create runtime directory");
        assert_eq!(find_wayland_socket(&runtime), None);
        assert_eq!(find_wayland_socket(&runtime.join("missing")), None);
        let _ = std::fs::remove_dir_all(&runtime);
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn same(a: Op, b: Op) -> bool {
        a.encode() == b.encode()
    }

    #[test]
    fn input_records_round_trip() -> std::io::Result<()> {
        let ops = [Op::Motion(12.5, 700.25), Op::Button(BTN_LEFT, 1), Op::Axis(AXIS_VERTICAL, -30.0), Op::Key(30, 0), Op::Presented, Op::Size(1280, 720)];
        let bytes: Vec<u8> = ops.iter().flat_map(|op| op.encode()).collect();
        let mut input = Cursor::new(bytes);
        for op in ops {
            let decoded = Op::decode(&mut input)?.ok_or(std::io::ErrorKind::InvalidData)?;
            assert!(same(op, decoded));
        }
        assert!(Op::decode(&mut input)?.is_none());
        Ok(())
    }

    #[test]
    fn host_auto_repeat_sends_nothing() {
        assert!(key_op(30, ElementState::Pressed, true).is_none());
        assert!(
            key_op(30, ElementState::Pressed, false).is_some_and(|op| same(op, Op::Key(30, 1)))
        );
        assert!(
            key_op(30, ElementState::Released, false).is_some_and(|op| same(op, Op::Key(30, 0)))
        );
    }

    #[test]
    fn unknown_or_out_of_range_records_end_the_stream() -> std::io::Result<()> {
        let mut record = Op::Key(30, 1).encode();
        record[0] = 99;
        assert!(Op::decode(&mut Cursor::new(record))?.is_none());
        let mut record = Op::Key(30, 1).encode();
        record[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Op::decode(&mut Cursor::new(record))?.is_none());
        Ok(())
    }

    #[test]
    fn frames_keep_their_pixels_across_the_wire() -> anyhow::Result<()> {
        let rgba: Vec<u8> = (0..4 * 6 * 3).map(|i| u8::try_from(i * 7 % 256).unwrap_or(0)).collect();
        let frame = Frame { width: 6, height: 3, pixels: rgba.clone(), captured_micros: 0, serial: 0 };
        let mut wire = Vec::new();
        write_title(&mut wire,"kwin-viewer s1: First instruction")?;
        write_frame(&mut wire, &frame)?;
        write_title(&mut wire,"kwin-viewer s1: Updated instruction")?;
        write_frame(&mut wire, &frame)?;
        let mut input = Cursor::new(wire);
        let title = Mutex::new(String::new());
        for expected in ["kwin-viewer s1: First instruction","kwin-viewer s1: Updated instruction"] {
            let got = read_frame(&mut input,Some(&title))?.ok_or_else(|| anyhow::anyhow!("stream ended early"))?;
            assert_eq!(*title.lock().unwrap(),expected);
            assert_eq!((got.width, got.height), (6, 3));
            assert_eq!(got.pixels, rgba);
        }
        assert!(read_frame(&mut input,None)?.is_none());
        Ok(())
    }
}
