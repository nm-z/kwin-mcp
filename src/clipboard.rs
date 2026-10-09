//! Shared Wayland clipboard ownership and dispatch for the MCP server and viewer.
use nix::poll::{PollFd, PollFlags, PollTimeout};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as data_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as data_offer, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self as data_source, ExtDataControlSourceV1},
};
pub const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(1);
const WL_SEAT_VERSION: u32 = 1;
const DATA_CONTROL_VERSION: u32 = 1;

#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

// Dispatch `queue` until shutdown is requested or the connection ends.
// Readiness is polled with DISPATCH_POLL_INTERVAL rather than blocked on, so a
// stop request is honored even while its compositor is silent and no dispatch
// thread can outlive the viewer.
fn dispatch_once<S>(
    queue: &mut EventQueue<S>,
    state: &mut S,
    wait: Duration,
) -> anyhow::Result<()> {
    queue.dispatch_pending(state)?;
    queue.flush()?;
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    let timeout = PollTimeout::try_from(wait)
        .map_err(|error| anyhow::anyhow!("dispatch timeout: {error}"))?;
    let (ready, revents) = {
        let mut fds = [PollFd::new(guard.connection_fd(), PollFlags::POLLIN)];
        let ready = nix::poll::poll(&mut fds, timeout);
        (ready, fds[0].revents())
    };
    if revents.is_some_and(|flags| {
        flags.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
    }) {
        anyhow::bail!("wayland socket hung up");
    }
    match ready {
        Ok(0) | Err(nix::errno::Errno::EINTR) => drop(guard),
        Ok(_) => match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        },
        Err(error) => anyhow::bail!("poll wayland socket: {error}"),
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

pub fn pump_queue<S>(
    queue: &mut EventQueue<S>,
    state: &mut S,
    shutdown: &Shutdown,
) -> anyhow::Result<()> {
    while !shutdown.requested() {
        dispatch_once(queue, state, DISPATCH_POLL_INTERVAL)?;
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

// A wayland dispatch thread plus the connection it reads. Dropping it requests
// shutdown and joins the thread, so every exit path out of main, including the
// error paths, tears the thread down and closes the socket.
pub struct DispatchThread {
    label: String,
    shutdown: Shutdown,
    handle: Option<JoinHandle<()>>,
    conn: Connection,
}

impl DispatchThread {
    pub fn spawn<S: Send + 'static>(
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
    pub fn connection(&self) -> &Connection {
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

// Clipboard contents as (MIME type, bytes) pairs in offer order. Empty means
// no selection.
pub const GENERATED_SECRET_MIME: &str = "application/x-kwin-mcp-generated-secret";

pub enum ClipboardText {
    Empty,
    Text(Vec<u8>),
    Secret(Option<usize>),
}

pub type ClipContents = Arc<Vec<(String, Vec<u8>)>>;

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
            data_device::Event::PrimarySelection { id: None } | _ => {}
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
pub struct Clipboard {
    label: &'static str,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    qh: QueueHandle<ClipState>,
    shared: Arc<ClipShared>,
    next_sync: AtomicU64,
    dispatch: DispatchThread,
}

impl Clipboard {
    pub fn spawn(
        conn: Connection,
        label: &'static str,
        shutdown: &Shutdown,
    ) -> anyhow::Result<Self> {
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
        Self::initial_sync(&conn, &qh, &mut queue, &mut state, 1)?;
        let manager = state.manager.clone().ok_or_else(|| {
            anyhow::anyhow!("{label} compositor did not advertise ext_data_control_manager_v1")
        })?;
        let seat = state
            .seat
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{label} compositor did not advertise wl_seat"))?;
        let device = manager.get_data_device(&seat, &qh, ());
        Self::initial_sync(&conn, &qh, &mut queue, &mut state, 2)?;
        Ok(Self {
            label,
            manager,
            device,
            qh,
            shared,
            next_sync: AtomicU64::new(2),
            dispatch: DispatchThread::spawn(
                format!("clipboard-{label}"),
                conn,
                queue,
                state,
                shutdown,
            )?,
        })
    }

    fn initial_sync(
        conn: &Connection,
        qh: &QueueHandle<ClipState>,
        queue: &mut EventQueue<ClipState>,
        state: &mut ClipState,
        serial: u64,
    ) -> anyhow::Result<()> {
        conn.display().sync(qh, serial);
        let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
        loop {
            let synced = state
                .shared
                .watch
                .lock()
                .map_err(|_| anyhow::anyhow!("clipboard lock poisoned"))?
                .synced;
            if synced >= serial {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "clipboard compositor did not answer within {CLIPBOARD_TIMEOUT:?}"
            );
            dispatch_once(queue, state, remaining.min(DISPATCH_POLL_INTERVAL))?;
        }
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

    pub fn generation(&self) -> anyhow::Result<u64> {
        self.sync()?;
        let watch = self
            .shared
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{} clipboard mutex poisoned", self.label))?;
        Ok(watch.generation)
    }

    pub fn wait_for_change(&self, generation: u64, timeout: Duration) -> anyhow::Result<bool> {
        self.wait_until(timeout, |watch| watch.generation > generation)
    }
    pub fn wait_for_read_since(
        &self,
        since: Instant,
        settle: Duration,
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        self.wait_until(timeout, |watch| {
            watch
                .served
                .is_some_and(|served| served >= since && served.elapsed() >= settle)
        })
    }

    // Read every MIME type of the current selection.

    pub fn read(&self) -> anyhow::Result<ClipContents> {
        self.read_mimes(false)
    }

    pub fn read_text(&self) -> anyhow::Result<ClipboardText> {
        let contents = self.read_mimes(true)?;
        Ok(match contents.first() {
            None => ClipboardText::Empty,
            Some((mime, bytes)) if mime == GENERATED_SECRET_MIME => ClipboardText::Secret(
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| text.parse().ok()),
            ),
            Some((_, bytes)) => ClipboardText::Text(bytes.clone()),
        })
    }

    fn read_mimes(&self, text_only: bool) -> anyhow::Result<ClipContents> {
        self.sync()?;
        let selection = self
            .shared
            .watch
            .lock()
            .map_err(|_| anyhow::anyhow!("{} clipboard mutex poisoned", self.label))?
            .selection
            .clone();
        let Some((offer, mut mimes)) = selection else {
            return Ok(Arc::default());
        };
        if text_only {
            let preferred = [
                GENERATED_SECRET_MIME,
                "text/plain;charset=utf-8",
                "UTF8_STRING",
                "text/plain",
                "TEXT",
                "STRING",
            ];
            mimes = preferred
                .iter()
                .find_map(|wanted| {
                    mimes
                        .iter()
                        .find(|mime| mime.eq_ignore_ascii_case(wanted))
                        .cloned()
                })
                .into_iter()
                .collect();
        }
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
                    Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
                    Ok(_) => {}
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
    pub fn write(&self, contents: ClipContents) -> anyhow::Result<()> {
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
        self.sync()
    }

    pub fn sync(&self) -> anyhow::Result<()> {
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
