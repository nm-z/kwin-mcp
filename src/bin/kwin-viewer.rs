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

use screen_13::driver::ash::vk;
use screen_13::driver::buffer::Buffer;
use screen_13::driver::image::{Image, ImageInfo};
use screen_13_window::WindowBuilder;
use std::io::Cursor;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
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
const SCREENCAST_VERSION: u32 = 4;
const WL_OUTPUT_VERSION: u32 = 4;

// Linux input event codes — evdev BTN_* constants (see linux/input-event-codes.h).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// fake_input axis ids (matches wl_pointer axis): 0=vertical, 1=horizontal.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

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

struct WlState {
    output: Option<wl_output::WlOutput>,
    screencast: Option<ZkdeScreencastUnstableV1>,
    fake_input: Option<OrgKdeKwinFakeInput>,
    stream: Option<ZkdeScreencastStreamUnstableV1>,
    node_id: Option<u32>,
    failed: Option<String>,
    closed: bool,
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
            zs_stream::Event::Failed { error } => state.failed = Some(error),
            zs_stream::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(WlState: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(WlState: ignore ZkdeScreencastUnstableV1);
wayland_client::delegate_noop!(WlState: ignore OrgKdeKwinFakeInput);

fn main() -> anyhow::Result<()> {
    let session_dir = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: kwin-viewer /tmp/kwin-mcp-<pid>"))?;
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

    let mailbox: FrameMailbox = Arc::new(Mutex::new(None));

    // Owning handle so the pipewire thread keeps working. We never touch the
    // wayland connection after this from here — the pipewire thread spins its
    // own loop, the main thread runs winit, fake_input requests flush
    // synchronously when called.
    let pipewire_sock = session_path.join("pipewire-0");
    let _pw_thread = {
        let mailbox = Arc::clone(&mailbox);
        std::thread::spawn(move || {
            if let Err(e) = run_pipewire(pipewire_sock, node_id, mailbox) {
                eprintln!("kwin-viewer: pipewire loop exited: {e}");
            }
        })
    };

    // Dispatch thread for Wayland events (stream closed, roundtrips for fake_input flushes).
    let conn_dispatch = conn.clone();
    let _wl_thread = std::thread::spawn(move || {
        let mut eq = conn_dispatch.new_event_queue::<WlState>();
        let _ = conn_dispatch.display().get_registry(&eq.handle(), ());
        let mut dummy = WlState {
            output: None, screencast: None, fake_input: None,
            stream: None, node_id: None, failed: None, closed: false,
        };
        loop {
            if eq.blocking_dispatch(&mut dummy).is_err() {
                break;
            }
        }
    });

    let window = WindowBuilder::default()
        .window(|wa| wa.with_title("kwin-viewer").with_inner_size(winit::dpi::LogicalSize::new(1232, 924)))
        .build()?;
    let device = Arc::clone(&window.device);

    // Source image + its GPU upload buffer are recreated on size change.
    // Starting as None so the first frame triggers allocation.
    let mut src_image: Option<Arc<Image>> = None;
    let mut src_dims: (u32, u32) = (0, 0);

    let fake_input_for_loop = fake_input.clone();
    let conn_for_loop = conn.clone();
    let mut input_state = InputState::default();

    window.run(move |frame| {
        for event in frame.events {
            forward_input(
                event,
                &fake_input_for_loop,
                &conn_for_loop,
                frame.width,
                frame.height,
                &mut input_state,
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
        } else {
            frame.render_graph.clear_color_image(frame.swapchain_image);
        }

        // winit on Wayland won't re-fire RedrawRequested on its own once it
        // decides the queue is idle, and screen-13-window's about_to_wait
        // hook doesn't reliably keep the pump running when no input events
        // are arriving. Explicitly requesting a redraw each frame guarantees
        // PipeWire's async frame arrivals get picked up.
        frame.window.request_redraw();
    })?;

    Ok(())
}

#[derive(Default)]
struct InputState {
    // Last cursor position in window pixel coords, updated on every
    // CursorMoved regardless of whether the move is forwarded. Needed so a
    // fresh click can snap the container's cursor to the click position
    // before the button press, without ever leaking intervening moves.
    last_pos: Option<(f64, f64)>,
    // Currently-held mouse buttons. Non-empty means we're in a drag and
    // pointer motions should be forwarded so the drag actually drags.
    held_buttons: u32,
}

fn map_window_to_virtual(pos: (f64, f64), win_w: u32, win_h: u32) -> Option<(f64, f64)> {
    if win_w == 0 || win_h == 0 { return None }
    Some((
        pos.0 * 1232.0 / f64::from(win_w),
        pos.1 * 924.0 / f64::from(win_h),
    ))
}

fn forward_input(
    event: &Event<()>,
    fake_input: &OrgKdeKwinFakeInput,
    conn: &Connection,
    win_w: u32,
    win_h: u32,
    state: &mut InputState,
) {
    let Event::WindowEvent { event, .. } = event else { return };
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            // Always record the latest cursor position locally so a subsequent
            // click can snap the container's cursor to it. Only forward the
            // motion over the wire when the user is actively clicking/dragging
            // — idle hover must not touch the agent's session.
            state.last_pos = Some((position.x, position.y));
            if state.held_buttons == 0 { return }
            if let Some((x, y)) = map_window_to_virtual((position.x, position.y), win_w, win_h) {
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
                    && let Some((x, y)) = map_window_to_virtual(pos, win_w, win_h)
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
            if let Some(evdev) = key_code_to_evdev(kc) {
                let pressed = matches!(key.state, ElementState::Pressed);
                fake_input.keyboard_key(evdev, if pressed { 1 } else { 0 });
                let _ = conn.flush();
            }
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
fn run_pipewire(socket_path: PathBuf, node_id: u32, mailbox: FrameMailbox) -> anyhow::Result<()> {
    use pipewire as pw;
    use pw::spa;
    use spa::pod::Pod;

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
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

    let format_pod = build_format_pod()?;
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

fn build_format_pod() -> anyhow::Result<Vec<u8>> {
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
            spa::utils::Rectangle { width: 1232, height: 924 },
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
