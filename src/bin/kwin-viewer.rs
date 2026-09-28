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

use nix::poll::{PollFd, PollFlags, PollTimeout};
use screen_13::driver::ash::vk;
use screen_13::driver::buffer::Buffer;
use screen_13::driver::image::{Image, ImageInfo};
use screen_13_window::WindowBuilder;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_callback, wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as data_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as data_offer, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self as data_source, ExtDataControlSourceV1},
};
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
use winit::event::{ElementState, Event, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::PhysicalKey;

// Bindings ceilings — the wayland-protocols-plasma 0.3.12 XMLs cap here even
// though the container's KWin advertises higher. Binding above a binding's
// known version panics the scanner-generated code.
const FAKE_INPUT_VERSION: u32 = 5;
const KEYSTATE_VERSION: u32 = 5;
const SCREENCAST_VERSION: u32 = 4;
const WL_OUTPUT_VERSION: u32 = 4;
const WL_SEAT_VERSION: u32 = 1;
const DATA_CONTROL_VERSION: u32 = 1;

// Linux input event codes — evdev BTN_* constants (see linux/input-event-codes.h).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// fake_input axis ids (matches wl_pointer axis): 0=vertical, 1=horizontal.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

const NUMLOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

// Bound on a clipboard owner answering one read, and on the compositor
// applying a selection.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(1);
// How long after a viewer copy chord the session's next selection change is
// taken as that copy.
const COPY_TIMEOUT: Duration = Duration::from_secs(1);
// A viewer paste hands the agent's selection back after the pasting app has
// been quiet this long, or after PASTE_TIMEOUT if it never reads.
const PASTE_SETTLE: Duration = Duration::from_millis(200);
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

/// Status file kwin-mcp reads to report the viewer outcome; the name is shared
/// with src/main.rs.
const VIEWER_STATUS_FILE: &str = "viewer-status.json";

/// Atomically publish the viewer's lifecycle state: starting, streaming,
/// ready (a frame is on the host window), closed, or failed.
fn write_status(session: &std::path::Path, state: &str, detail: &str) {
    let temporary = session.join(format!("{VIEWER_STATUS_FILE}.tmp"));
    let body = serde_json::json!({"state": state, "detail": detail, "pid": std::process::id()});
    if std::fs::write(&temporary, body.to_string()).is_ok() {
        let _ = std::fs::rename(&temporary, session.join(VIEWER_STATUS_FILE));
    }
}

struct Frame {
    width: u32,
    height: u32,
    // Tightly packed RGBA8 (stride == 4 * width). Video format conversion
    // happens in the pipewire callback so the render path stays trivial.
    rgba: Vec<u8>,
}

// Latest-frame mailbox. PipeWire's process callback writes; the window's
// draw_fn reads. No queue, no backpressure: at <60fps the window simply
// redraws the last frame. Wrapped in Mutex instead of a channel of 1 so the
// producer never blocks if the consumer is slow.
type FrameMailbox = Arc<Mutex<Option<Frame>>>;

// Cooperative stop signal shared by every thread the viewer owns. Whatever
// ends first sets it: the user closing the window, a compositor dropping a
// connection, or the screencast stream failing. The winit loop leaves on the
// next frame and main then joins the threads and closes the connections.
#[derive(Clone, Default)]
struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

// Dispatch `queue` until shutdown is requested or the connection ends.
// Readiness is polled with DISPATCH_POLL_INTERVAL rather than blocked on, so a
// stop request is honored even while its compositor is silent and no dispatch
// thread can outlive the viewer.
fn pump_queue<S>(
    queue: &mut EventQueue<S>,
    state: &mut S,
    shutdown: &Shutdown,
) -> anyhow::Result<()> {
    let timeout = PollTimeout::try_from(DISPATCH_POLL_INTERVAL)
        .map_err(|error| anyhow::anyhow!("dispatch poll interval: {error}"))?;
    while !shutdown.requested() {
        queue.dispatch_pending(state)?;
        queue.flush()?;
        // None means events are already buffered; dispatch them first.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        // Poll the descriptor this guard reads, and end the borrow before the
        // guard is consumed below.
        let (ready, revents) = {
            let mut fds = [PollFd::new(guard.connection_fd(), PollFlags::POLLIN)];
            let ready = nix::poll::poll(&mut fds, timeout);
            (ready, fds[0].revents())
        };
        // poll reports hangup, error, and invalid-descriptor conditions
        // whether or not they were requested. They all mean the compositor is
        // gone, and reading a half-closed socket that still holds a partial
        // message answers WouldBlock, so stop here rather than spin.
        if revents.is_some_and(|flags| {
            flags.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
        }) {
            anyhow::bail!("wayland socket hung up");
        }
        match ready {
            Ok(0) => drop(guard),
            Ok(_) => match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            },
            Err(nix::errno::Errno::EINTR) => drop(guard),
            Err(error) => return Err(anyhow::anyhow!("poll wayland socket: {error}")),
        }
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

// A wayland dispatch thread plus the connection it reads. Dropping it requests
// shutdown and joins the thread, so every exit path out of main, including the
// error paths, tears the thread down and closes the socket.
struct DispatchThread {
    label: String,
    shutdown: Shutdown,
    handle: Option<JoinHandle<()>>,
    conn: Connection,
}

impl DispatchThread {
    fn spawn<S: Send + 'static>(
        label: String,
        conn: Connection,
        mut queue: EventQueue<S>,
        mut state: S,
        shutdown: &Shutdown,
    ) -> anyhow::Result<Self> {
        let thread_shutdown = shutdown.clone();
        let thread_label = label.clone();
        let handle = std::thread::Builder::new()
            .name(label.clone())
            .spawn(move || {
                if let Err(error) = pump_queue(&mut queue, &mut state, &thread_shutdown) {
                    eprintln!("kwin-viewer: {thread_label} connection ended: {error}");
                }
                // A connection that ended cannot be recovered, and a viewer
                // without it would sit on a frozen picture, so end the viewer.
                thread_shutdown.request();
            })?;
        Ok(Self {
            label,
            shutdown: shutdown.clone(),
            handle: Some(handle),
            conn,
        })
    }

    // The connection this thread reads, for sending requests on it.
    fn connection(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for DispatchThread {
    fn drop(&mut self) {
        self.shutdown.request();
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("kwin-viewer: {} dispatch thread panicked", self.label);
        }
    }
}

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

// Clipboard contents as (MIME type, bytes) pairs in offer order. Empty means
// no selection.
type ClipContents = Arc<Vec<(String, Vec<u8>)>>;

#[derive(Default)]
struct ClipWatch {
    selection: Option<(ExtDataControlOfferV1, Vec<String>)>,
    // Counts selection changes, including the ones this viewer makes.
    generation: u64,
    // When a compositor last asked a source this viewer set for its data.
    served: Option<Instant>,
    synced: u64,
    ended: bool,
}

#[derive(Default)]
struct ClipShared {
    watch: Mutex<ClipWatch>,
    changed: Condvar,
}

impl ClipShared {
    fn update(&self, change: impl FnOnce(&mut ClipWatch)) {
        if let Ok(mut watch) = self.watch.lock() {
            change(&mut watch);
            self.changed.notify_all();
        }
    }
}

struct ClipState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ExtDataControlManagerV1>,
    // MIME types of offers that are not the selection yet.
    offered: HashMap<ObjectId, Vec<String>>,
    shared: Arc<ClipShared>,
}

impl Drop for ClipState {
    fn drop(&mut self) {
        self.shared.update(|watch| watch.ended = true);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, WL_SEAT_VERSION, qh, ()));
                }
                "ext_data_control_manager_v1" => {
                    state.manager = Some(registry.bind(name, DATA_CONTROL_VERSION, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            data_device::Event::DataOffer { id } => {
                state.offered.insert(id.id(), Vec::new());
            }
            data_device::Event::Selection { id } => {
                let selection = id.map(|offer| {
                    let mimes = state.offered.remove(&offer.id()).unwrap_or_default();
                    (offer, mimes)
                });
                state.shared.update(|watch| {
                    if let Some((old, _)) = std::mem::replace(&mut watch.selection, selection) {
                        old.destroy();
                    }
                    watch.generation += 1;
                });
            }
            data_device::Event::PrimarySelection { id: Some(offer) } => {
                state.offered.remove(&offer.id());
                offer.destroy();
            }
            data_device::Event::Finished => state.shared.update(|watch| watch.ended = true),
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClipState, ExtDataControlDeviceV1, [
        data_device::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let data_offer::Event::Offer { mime_type } = event
            && let Some(mimes) = state.offered.get_mut(&offer.id())
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ClipContents> for ClipState {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: data_source::Event,
        contents: &ClipContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            data_source::Event::Send { mime_type, fd } => {
                state
                    .shared
                    .update(|watch| watch.served = Some(Instant::now()));
                let contents = Arc::clone(contents);
                // A slow reader must not stall this connection's dispatch.
                std::thread::spawn(move || {
                    let bytes = contents.iter().find(|(mime, _)| *mime == mime_type);
                    if let Some((_, bytes)) = bytes
                        && let Err(error) = std::fs::File::from(fd).write_all(bytes)
                    {
                        eprintln!("kwin-viewer: clipboard send {mime_type}: {error}");
                    }
                });
            }
            data_source::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for ClipState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state
                .shared
                .update(|watch| watch.synced = watch.synced.max(*serial));
        }
    }
}

wayland_client::delegate_noop!(ClipState: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(ClipState: ignore ExtDataControlManagerV1);

// One compositor's clipboard, read and written through ext_data_control_v1,
// which works without keyboard focus.
struct Clipboard {
    label: &'static str,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    qh: QueueHandle<ClipState>,
    shared: Arc<ClipShared>,
    next_sync: AtomicU64,
    dispatch: DispatchThread,
}

impl Clipboard {
    fn spawn(conn: Connection, label: &'static str, shutdown: &Shutdown) -> anyhow::Result<Self> {
        let mut queue = conn.new_event_queue::<ClipState>();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let shared = Arc::new(ClipShared::default());
        let mut state = ClipState {
            seat: None,
            manager: None,
            offered: HashMap::new(),
            shared: Arc::clone(&shared),
        };
        queue.roundtrip(&mut state)?;
        let manager = state.manager.clone().ok_or_else(|| {
            anyhow::anyhow!("{label} compositor did not advertise ext_data_control_manager_v1")
        })?;
        let seat = state
            .seat
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{label} compositor did not advertise wl_seat"))?;
        let device = manager.get_data_device(&seat, &qh, ());
        queue.roundtrip(&mut state)?;
        Ok(Self {
            label,
            manager,
            device,
            qh,
            shared,
            next_sync: AtomicU64::new(0),
            dispatch: DispatchThread::spawn(
                format!("clipboard-{label}"),
                conn,
                queue,
                state,
                shutdown,
            )?,
        })
    }

    // Wait until `done` holds or `timeout` passes, and report which. Fails if
    // the connection ends first.
    fn wait_until(
        &self,
        timeout: Duration,
        done: impl Fn(&ClipWatch) -> bool,
    ) -> anyhow::Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut watch = self
            .shared
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{} clipboard mutex poisoned", self.label))?;
        loop {
            if done(&watch) {
                return Ok(true);
            }
            anyhow::ensure!(!watch.ended, "{} clipboard connection ended", self.label);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let (guard, _) = self
                .shared
                .changed
                .wait_timeout(watch, remaining.min(DISPATCH_POLL_INTERVAL))
                .map_err(|_| anyhow::anyhow!("{} clipboard mutex poisoned", self.label))?;
            watch = guard;
        }
    }

    fn generation(&self) -> anyhow::Result<u64> {
        let watch = self
            .shared
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{} clipboard mutex poisoned", self.label))?;
        Ok(watch.generation)
    }

    // Read every MIME type of the current selection.
    fn read(&self) -> anyhow::Result<ClipContents> {
        let selection = self
            .shared
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{} clipboard mutex poisoned", self.label))?
            .selection
            .clone();
        let Some((offer, mimes)) = selection else {
            return Ok(Arc::default());
        };
        let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
        let mut contents = Vec::with_capacity(mimes.len());
        for mime in mimes {
            let (mut reader, writer) = std::io::pipe()?;
            offer.receive(mime.clone(), writer.as_fd());
            self.dispatch.connection().flush()?;
            drop(writer);
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 65536];
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                anyhow::ensure!(
                    !remaining.is_zero(),
                    "{} clipboard owner did not send {mime} within {CLIPBOARD_TIMEOUT:?}",
                    self.label
                );
                let timeout = PollTimeout::try_from(remaining)
                    .map_err(|error| anyhow::anyhow!("clipboard poll timeout: {error}"))?;
                match nix::poll::poll(
                    &mut [PollFd::new(reader.as_fd(), PollFlags::POLLIN)],
                    timeout,
                ) {
                    Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                    Err(error) => anyhow::bail!("poll {} clipboard pipe: {error}", self.label),
                }
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
            }
            contents.push((mime, bytes));
        }
        Ok(Arc::new(contents))
    }

    // Make `contents` the selection, and return once the compositor applied it.
    fn write(&self, contents: ClipContents) -> anyhow::Result<()> {
        if contents.is_empty() {
            self.device.set_selection(None);
        } else {
            let source = self
                .manager
                .create_data_source(&self.qh, Arc::clone(&contents));
            for (mime, _) in contents.iter() {
                source.offer(mime.clone());
            }
            self.device.set_selection(Some(&source));
        }
        let serial = self.next_sync.fetch_add(1, Ordering::Relaxed) + 1;
        self.dispatch.connection().display().sync(&self.qh, serial);
        self.dispatch.connection().flush()?;
        anyhow::ensure!(
            self.wait_until(CLIPBOARD_TIMEOUT, |watch| watch.synced >= serial)?,
            "{} compositor did not apply the selection within {CLIPBOARD_TIMEOUT:?}",
            self.label
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
}

impl ClipboardBridge {
    // Runs before the chord's key press reaches the session.
    fn before(&mut self, chord: ClipChord) -> anyhow::Result<()> {
        if let Some(restore) = self.restore.take()
            && restore.join().is_err()
        {
            eprintln!("kwin-viewer: clipboard restore thread panicked");
        }
        let agent = self.isolated.read()?;
        let isolated = Arc::clone(&self.isolated);
        let restore: Box<dyn FnOnce() -> anyhow::Result<()> + Send> = match chord {
            ClipChord::Copy => {
                let host = Arc::clone(&self.host);
                let before = isolated.generation()?;
                Box::new(move || {
                    if isolated.wait_until(COPY_TIMEOUT, |watch| watch.generation > before)? {
                        host.write(isolated.read()?)?;
                        isolated.write(agent)?;
                    }
                    Ok(())
                })
            }
            ClipChord::Paste => {
                isolated.write(self.host.read()?)?;
                let pasted = Instant::now();
                // Hand the agent's selection back once the pasting app stops
                // reading the user's.
                Box::new(move || {
                    isolated.wait_until(PASTE_TIMEOUT, |watch| {
                        watch.served.is_some_and(|served| {
                            served >= pasted && served.elapsed() >= PASTE_SETTLE
                        })
                    })?;
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

fn main() -> anyhow::Result<()> {
    let mut argv = std::env::args().skip(1);
    let session_dir = argv
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: kwin-viewer /tmp/kwin-mcp-<pid> [width height]"))?;
    let session_path = std::path::PathBuf::from(&session_dir);
    write_status(&session_path, "starting", "");
    let result = run(session_dir, argv);
    match &result {
        Ok(()) => write_status(&session_path, "closed", "viewer exited: window closed or session ended"),
        Err(error) => write_status(&session_path, "failed", &format!("{error:#}")),
    }
    result
}

fn run(session_dir: String, mut argv: impl Iterator<Item = String>) -> anyhow::Result<()> {
    // Virtual display size, passed by kwin-mcp at spawn. Defaults match the
    // server's compiled-in VIRTUAL_SCREEN_WIDTH/HEIGHT for manual invocation.
    let virt_w: u32 = match argv.next() {
        Some(v) => v.parse().map_err(|e| anyhow::anyhow!("width '{v}': {e}"))?,
        None => 3840,
    };
    let virt_h: u32 = match argv.next() {
        Some(v) => v.parse().map_err(|e| anyhow::anyhow!("height '{v}': {e}"))?,
        None => 2160,
    };
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
    };

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
    numlock.apply(&fake_input, &conn)?;

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
    write_status(&session_path, "streaming", &format!("pipewire node {node_id}"));

    let mailbox: FrameMailbox = Arc::new(Mutex::new(None));

    // The pipewire loop runs on its own thread and is stopped through this
    // channel, so main can join it instead of leaving it behind on exit.
    let pipewire_sock = session_path.join("pipewire-0");
    let (pw_quit, pw_quit_rx) = pipewire::channel::channel::<()>();
    let pw_thread = {
        let mailbox = Arc::clone(&mailbox);
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("pipewire".to_owned())
            .spawn(move || {
                if let Err(e) = run_pipewire(
                    pipewire_sock,
                    node_id,
                    mailbox,
                    (virt_w, virt_h),
                    pw_quit_rx,
                ) {
                    eprintln!("kwin-viewer: pipewire loop exited: {e}");
                }
                // Losing the video feed ends the viewer the same way a dead
                // wayland connection does.
                shutdown.request();
            })?
    };

    // The screencast connection keeps serving the queue that carries the
    // stream's Closed and Failed events, and it stays the connection the winit
    // loop sends fake_input requests on. Moving the real state here, instead of
    // dispatching a throwaway copy, is what lets a closed stream end the viewer.
    let wl_dispatch =
        DispatchThread::spawn("screencast".to_owned(), conn, event_queue, state, &shutdown)?;
    let conn = wl_dispatch.connection();

    let window = WindowBuilder::default()
        .window(|wa| wa.with_title("kwin-viewer").with_inner_size(winit::dpi::LogicalSize::new(1920, 1080)))
        .build()?;
    let device = Arc::clone(&window.device);

    // Source image + its GPU upload buffer are recreated on size change.
    // Starting as None so the first frame triggers allocation.
    let mut src_image: Option<Arc<Image>> = None;
    let mut src_dims: (u32, u32) = (0, 0);

    let mut input_state = InputState {
        last_pos: None,
        held_buttons: 0,
        held_keys: HashSet::new(),
        clipboard,
    };
    let mut ready_reported = false;

    let run_result = window.run(|mut frame| {
        // Leave as soon as anything the viewer depends on has ended, so the
        // window never sits on a dead session and main can join the threads.
        if shutdown.requested() {
            frame.exit();
            return;
        }

        for event in frame.events {
            forward_input(
                event,
                &fake_input,
                conn,
                (frame.width, frame.height),
                (virt_w, virt_h),
                &mut input_state,
                &numlock,
            );
        }

        // Consume the latest frame if one arrived; upload into src_image.
        // Retain src_image across frames so when the mailbox is momentarily
        // empty we still re-blit the last known picture instead of flashing
        // black.
        let latest = mailbox.lock().ok().and_then(|mut g| g.take());
        if let Some(f) = latest {
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
                    &f.rgba,
                ) {
                    Ok(staging) => {
                        let staging_node = frame.render_graph.bind_node(staging);
                        let image_node = frame.render_graph.bind_node(image);
                        frame.render_graph.copy_buffer_to_image(staging_node, image_node);
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
            if !ready_reported {
                ready_reported = true;
                eprintln!("kwin-viewer: first frame presented");
                write_status(&session_path, "ready", "host window is showing the session");
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
    });

    // Ordered teardown, reached on every exit including a window error, which
    // is why the run result is held instead of propagated straight away.
    // Requesting shutdown lets every dispatch thread leave its poll within
    // DISPATCH_POLL_INTERVAL; the pipewire loop is quit through its channel and
    // joined here, and dropping wl_dispatch and numlock joins the remaining
    // threads and closes their sockets.
    shutdown.request();
    if pw_quit.send(()).is_err() {
        eprintln!("kwin-viewer: pipewire loop already gone");
    }
    if pw_thread.join().is_err() {
        eprintln!("kwin-viewer: pipewire thread panicked");
    }
    run_result?;
    Ok(())
}

struct InputState {
    // Last cursor position in window pixel coords, updated on every
    // CursorMoved regardless of whether the move is forwarded. Needed so a
    // fresh click can snap the container's cursor to the click position
    // before the button press, without ever leaking intervening moves.
    last_pos: Option<(f64, f64)>,
    // Currently-held mouse buttons. Non-empty means we're in a drag and
    // pointer motions should be forwarded so the drag actually drags.
    held_buttons: u32,
    // Evdev keycodes currently held inside the container. We forcibly
    // release them on focus loss; otherwise a missed Released event (e.g.
    // user releases Shift outside the viewer window) leaves a modifier
    // stuck inside the container, and every subsequent letter the user
    // types arrives shifted — looks exactly like "I cant type."
    held_keys: HashSet<u32>,
    clipboard: ClipboardBridge,
}

fn map_window_to_virtual(pos: (f64, f64), win_w: u32, win_h: u32, virt: (u32, u32)) -> Option<(f64, f64)> {
    if win_w == 0 || win_h == 0 { return None }
    Some((
        pos.0 * f64::from(virt.0) / f64::from(win_w),
        pos.1 * f64::from(virt.1) / f64::from(win_h),
    ))
}

fn forward_input(
    event: &Event<()>,
    fake_input: &OrgKdeKwinFakeInput,
    conn: &Connection,
    window_size: (u32, u32),
    virt: (u32, u32),
    state: &mut InputState,
    numlock: &NumLockSync,
) {
    let Event::WindowEvent { event, .. } = event else { return };
    if let WindowEvent::Focused(true) = event
        && let Err(error) = numlock.apply(fake_input, conn)
    {
        eprintln!("kwin-viewer: Num Lock synchronization failed: {error}");
    }
    if let WindowEvent::Focused(false) = event {
        // Drain any keys that were forwarded as pressed but whose Released
        // event we may not see — release them all so no modifier stays
        // stuck inside the container while the user is elsewhere on host.
        for &code in &state.held_keys {
            fake_input.keyboard_key(code, 0);
        }
        if !state.held_keys.is_empty() {
            eprintln!(
                "kwin-viewer: focus lost — released {} held key(s)",
                state.held_keys.len()
            );
            state.held_keys.clear();
            let _ = conn.flush();
        }
    }
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            // Always record the latest cursor position locally so a subsequent
            // click can snap the container's cursor to it. Only forward the
            // motion over the wire when the user is actively clicking/dragging
            // — idle hover must not touch the agent's session.
            state.last_pos = Some((position.x, position.y));
            if state.held_buttons == 0 { return }
            if let Some((x, y)) =
                map_window_to_virtual((position.x, position.y), window_size.0, window_size.1, virt)
            {
                fake_input.pointer_motion_absolute(x, y);
                let _ = conn.flush();
            }
        }
        WindowEvent::MouseInput { state: btn_state, button, .. } => {
            let code = match button {
                MouseButton::Left => BTN_LEFT,
                MouseButton::Right => BTN_RIGHT,
                MouseButton::Middle => BTN_MIDDLE,
                _ => return,
            };
            let pressed = matches!(btn_state, ElementState::Pressed);
            if pressed {
                // Snap the container's cursor to the window position first
                // so the press lands where the user's eyes are, not wherever
                // the container cursor happened to stop last session.
                if let Some(pos) = state.last_pos
                    && let Some((x, y)) =
                        map_window_to_virtual(pos, window_size.0, window_size.1, virt)
                {
                    fake_input.pointer_motion_absolute(x, y);
                }
                state.held_buttons = state.held_buttons.saturating_add(1);
            } else {
                state.held_buttons = state.held_buttons.saturating_sub(1);
            }
            fake_input.button(code, if pressed { 1 } else { 0 });
            let _ = conn.flush();
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (dx, dy) = match delta {
                MouseScrollDelta::LineDelta(x, y) => (f64::from(*x) * 15.0, f64::from(*y) * 15.0),
                MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
            };
            if dy != 0.0 { fake_input.axis(AXIS_VERTICAL, -dy); }
            if dx != 0.0 { fake_input.axis(AXIS_HORIZONTAL, -dx); }
            let _ = conn.flush();
        }
        WindowEvent::KeyboardInput { event: key, .. } => {
            let PhysicalKey::Code(kc) = key.physical_key else { return };
            let Some(evdev) = key_code_to_evdev(kc) else { return };
            let pressed = matches!(key.state, ElementState::Pressed);
            if pressed {
                state.held_keys.insert(evdev);
                if !key.repeat
                    && let Some(chord) = clipboard_chord(evdev, &state.held_keys)
                    && let Err(error) = state.clipboard.before(chord)
                {
                    eprintln!("kwin-viewer: clipboard handoff failed: {error:#}");
                }
            } else {
                state.held_keys.remove(&evdev);
            }
            fake_input.keyboard_key(evdev, if pressed { 1 } else { 0 });
            let _ = conn.flush();
        }
        _ => {}
    }
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
    }
    let data = UserData {
        format: spa::param::video::VideoInfoRaw::default(),
        mailbox,
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

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| {
            eprintln!("kwin-viewer: pw stream {old:?} -> {new:?}");
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

            let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
            convert_to_rgba(raw, &mut rgba, w as usize, h as usize, stride, src_fmt);

            if let Ok(mut g) = ud.mailbox.lock() {
                *g = Some(Frame { width: w, height: h, rgba });
            }
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
