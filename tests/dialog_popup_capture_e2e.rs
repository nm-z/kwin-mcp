#![allow(clippy::expect_used)]

//! A dropdown that a dialog opens over itself is part of the dialog's screenshot
//! (#983): the image shows it, the pixel check leaves it out, and the metadata gives
//! its window-relative rectangle. A window of another program over the dialog is
//! still refused.
//!   a_wx_style_dropdown_over_an_x11_dialog_is_in_its_screenshot   the report: an
//!       override-redirect dropdown that window_list does not show (Bambu Studio's
//!       filament mapping under X11)
//!   a_combo_menu_over_a_wayland_dialog_is_in_its_screenshot   an xdg_popup
//!   another_programs_window_over_the_dialog_is_still_refused
//!
//! The dialog and its dropdowns are tests/fixtures/dialog_dropdown.c, built with cc
//! and GTK 3 into the session's workdir. Run an ignored case with `systemd-run --user
//! --wait --pipe --collect env KWIN_MCP_E2E_SERVER=<binary> <test exe> --ignored
//! --test-threads=1 NAME`; without KWIN_MCP_E2E_SERVER the binary built with the test
//! is used. KWIN_MCP_E2E_EVIDENCE=<dir> keeps the screenshots and replies.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const MAGENTA: [u8; 3] = [0xff, 0x00, 0xff];
const DIALOG_GREEN: [u8; 3] = [0x2e, 0x7d, 0x32];

/// One stdio server with a private HOME and one session, both removed on drop.
struct Desk {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    home: PathBuf,
    session: String,
    workdir: String,
    label: &'static str,
}

impl Desk {
    fn start(label: &'static str) -> Self {
        // Not under /tmp: a session has its own /tmp.
        let root = std::env::var_os("KWIN_MCP_TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let home = root.join(format!("kwin-mcp-popup-{label}-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
        let program = std::env::var_os("KWIN_MCP_E2E_SERVER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")));
        let mut child = Command::new(program)
            .args(["--autoclean", "--no-viewer"])
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kwin-mcp");
        let mut desk = Self {
            stdin: child.stdin.take().expect("server stdin"),
            stdout: BufReader::new(child.stdout.take().expect("server stdout")),
            child,
            next_id: 0,
            home,
            session: String::new(),
            workdir: String::new(),
            label,
        };
        desk.request(
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"dialog-popup-capture-e2e","version":"1"}
            }),
        );
        desk.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        // Native size: the dialog stays smaller than its parent, as dialogs are.
        let started = desk.request(
            "tools/call",
            json!({"name":"session_start", "arguments":{"width":1280, "height":800, "window_mode":"native"}}),
        );
        let field = |key: &str| {
            started["result"]["structuredContent"][key]
                .as_str()
                .unwrap_or_else(|| panic!("session_start has no {key}: {started}"))
                .to_owned()
        };
        desk.session = field("session_id");
        desk.workdir = field("host_shared_dir");
        desk
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("send message");
        self.stdin.flush().expect("flush message");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read reply");
            assert!(read > 0, "server closed stdout before reply {id}");
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message["id"] == id
            {
                return message;
            }
        }
    }

    /// The whole reply to a tool call in this desk's session.
    fn tool(&mut self, name: &str, mut arguments: Value) -> Value {
        arguments["session_id"] = json!(self.session);
        self.request("tools/call", json!({"name":name, "arguments":arguments}))
    }

    /// Builds the fixture into the workdir, which the session sees at the same path.
    fn fixture(&self) -> String {
        let source = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/dialog_dropdown.c"
        );
        let output = format!("{}/dialog-dropdown", self.workdir);
        let flags = Command::new("pkg-config")
            .args(["--cflags", "--libs", "gtk+-3.0"])
            .output()
            .expect("run pkg-config");
        assert!(
            flags.status.success(),
            "pkg-config gtk+-3.0: {}",
            String::from_utf8_lossy(&flags.stderr)
        );
        let flags = String::from_utf8(flags.stdout).expect("pkg-config prints text");
        let built = Command::new("cc")
            .args([
                "-std=c11", "-Wall", "-Wextra", "-Werror", source, "-o", &output,
            ])
            .args(flags.split_whitespace())
            .output()
            .expect("run cc");
        assert!(
            built.status.success(),
            "cc: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        output
    }

    fn launch(&mut self, command: &str, expect_window: bool) {
        let launched = self.tool(
            "launch_app",
            json!({"command":command, "window_timeout_seconds":40, "expect_window":expect_window}),
        );
        assert!(launched["error"].is_null(), "{launched}");
    }

    fn windows(&mut self) -> Vec<Value> {
        let listed = self.tool("window_list", json!({}));
        listed["result"]["structuredContent"]["windows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Waits until a listed window satisfies `wanted`, and returns it.
    fn window_where(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let windows = self.windows();
            if let Some(window) = windows.iter().position(&wanted).map(|at| &windows[at]) {
                return window.clone();
            }
            assert!(Instant::now() < deadline, "no such window: {windows:?}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Waits until the active window is the fixture's dialog, transient for its
    /// main window (GTK 3 makes it modal under X11 only), and returns it.
    fn active_dialog(&mut self) -> Value {
        self.window_where(|window| {
            window["active"] == json!(true)
                && window["transient"] == json!(true)
                && window["title"] == "Dropdown dialog"
        })
    }

    /// Clicks the middle of the active window, where the fixture's opener is.
    fn click_middle(&mut self, window: &Value) {
        let (width, height) = (number(&window["w"]), number(&window["h"]));
        let clicked = self.tool("mouse_click", json!({"x": width / 2, "y": height / 2}));
        assert!(clicked["error"].is_null(), "{clicked}");
    }

    /// A screenshot taken half a second after the dialog's dropdown first shows
    /// in the metadata, so that it has been drawn. Any refusal fails the test:
    /// the dialog is the active window throughout.
    fn shot_with_popup(&mut self, arguments: Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        loop {
            let shot = self.tool("screenshot", arguments.clone());
            if !shot["error"].is_null() {
                self.keep("refused", &shot);
                panic!("screenshot refused the dialog while its dropdown is open: {shot}");
            }
            let listed = content(&shot)["popups"]
                .as_array()
                .is_some_and(|popups| !popups.is_empty());
            if seen {
                assert!(listed, "the dropdown left the metadata: {shot}");
                return shot;
            }
            assert!(
                Instant::now() < deadline,
                "the dropdown never showed in the screenshot metadata: {shot}"
            );
            seen = listed;
            std::thread::sleep(Duration::from_millis(if seen { 500 } else { 300 }));
        }
    }

    /// Keeps a reply and its image where KWIN_MCP_E2E_EVIDENCE names a directory.
    fn keep(&self, name: &str, reply: &Value) {
        let Some(dir) = std::env::var_os("KWIN_MCP_E2E_EVIDENCE").map(PathBuf::from) else {
            return;
        };
        let dir = dir.join(self.label);
        std::fs::create_dir_all(&dir).expect("create evidence directory");
        let text = serde_json::to_string_pretty(reply).expect("reply as JSON");
        std::fs::write(dir.join(format!("{name}.json")), text).expect("keep reply");
        if let Some(path) = content(reply)["path"].as_str() {
            std::fs::copy(path, dir.join(format!("{name}.png"))).expect("keep image");
        }
    }
}

impl Drop for Desk {
    fn drop(&mut self) {
        let _ = self.tool("session_stop", json!({}));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn content(reply: &Value) -> &Value {
    &reply["result"]["structuredContent"]
}

/// A coordinate of a listed window, which KWin gives as a float.
#[expect(clippy::as_conversions)]
fn number(value: &Value) -> i64 {
    let coordinate = value
        .as_f64()
        .unwrap_or_else(|| panic!("not a number: {value}"));
    coordinate.round() as i64
}

/// The screen rectangle "x y width height" the fixture writes once a window maps.
fn reported(path: &str) -> [i64; 4] {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let fields: Vec<i64> = text
                .split_whitespace()
                .map(|field| field.parse().expect("an integer field"))
                .collect();
            return fields.try_into().expect("four fields");
        }
        assert!(Instant::now() < deadline, "{path} was never written");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `[x, y, width, height]` on the screen as `[x1, y1, x2, y2]` relative to `window`.
fn relative_to(window: &Value, [x, y, width, height]: [i64; 4]) -> [i64; 4] {
    let (left, top) = (number(&window["x"]), number(&window["y"]));
    [x - left, y - top, x - left + width, y - top + height]
}

struct Image {
    width: i64,
    height: i64,
    rgba: Vec<u8>,
}

impl Image {
    fn of(reply: &Value) -> Self {
        let path = content(reply)["path"].as_str().expect("a saved image");
        let decoder = png::Decoder::new(std::fs::File::open(Path::new(path)).expect("open image"));
        let mut reader = decoder.read_info().expect("read PNG header");
        let mut rgba = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut rgba).expect("decode PNG");
        assert_eq!(frame.color_type, png::ColorType::Rgba);
        rgba.truncate(frame.buffer_size());
        Self {
            width: i64::from(frame.width),
            height: i64::from(frame.height),
            rgba,
        }
    }

    /// The share of the pixels in `[x1, y1, x2, y2]` (image pixels) within three
    /// units of `color` in every channel.
    fn share(&self, [x1, y1, x2, y2]: [i64; 4], color: [u8; 3]) -> f64 {
        assert!(
            0 <= x1 && x1 < x2 && x2 <= self.width && 0 <= y1 && y1 < y2 && y2 <= self.height,
            "[{x1},{y1},{x2},{y2}] is not inside a {}x{} image",
            self.width,
            self.height
        );
        let mut matching = 0_u32;
        let mut total = 0_u32;
        for y in y1..y2 {
            for x in x1..x2 {
                let at = usize::try_from((y * self.width + x) * 4).expect("pixel offset");
                total += 1;
                if self.rgba[at..at + 3]
                    .iter()
                    .zip(color)
                    .all(|(pixel, wanted)| pixel.abs_diff(wanted) <= 3)
                {
                    matching += 1;
                }
            }
        }
        f64::from(matching) / f64::from(total)
    }
}

/// `rect` shrunk by `inset` on every side and clipped to `[0, 0, width, height]`.
fn inner(rect: [i64; 4], inset: i64, width: i64, height: i64) -> [i64; 4] {
    [
        (rect[0] + inset).max(0),
        (rect[1] + inset).max(0),
        (rect[2] - inset).min(width),
        (rect[3] - inset).min(height),
    ]
}

#[test]
#[ignore = "requires KDE, KWin with Xwayland, bubblewrap, pasta, cc, GTK 3, and input devices"]
fn a_wx_style_dropdown_over_an_x11_dialog_is_in_its_screenshot() {
    let mut desk = Desk::start("x11-dropdown");
    let fixture = desk.fixture();
    let report = format!("{}/dropdown.txt", desk.workdir);
    desk.launch(&format!("GDK_BACKEND=x11 {fixture} plain {report}"), true);
    let dialog = desk.active_dialog();
    assert_eq!(dialog["modal"], json!(true), "{dialog}");
    let (width, height) = (number(&dialog["w"]), number(&dialog["h"]));
    let before = desk.tool("screenshot", json!({}));
    desk.keep("closed", &before);
    assert!(before["error"].is_null(), "{before}");
    assert!(content(&before).get("popups").is_none(), "{before}");

    desk.click_middle(&dialog);
    // The fixture's own account of its dropdown: an override-redirect window,
    // which KWin manages no more than it lists.
    let dropdown = relative_to(&dialog, reported(&report));
    let listed = desk.windows();
    assert_eq!(
        listed.len(),
        2,
        "only the main window and the dialog: {listed:?}"
    );

    let shot = desk.shot_with_popup(json!({}));
    desk.keep("open", &shot);
    let data = content(&shot);
    assert_eq!(data["window"]["id"], dialog["id"], "{shot}");
    let check = &data["transient_pixel_check"];
    assert_eq!(check["status"], "matches_window_capture", "{shot}");
    assert!(check["popup_excluded_pixels"].as_u64() > Some(0), "{shot}");
    assert_eq!(
        data["popups"],
        json!([{"region": dropdown, "unlisted": true}]),
        "{shot}"
    );
    // The default image is still the dialog, 1:1 and window-relative.
    assert!(data.get("region").is_none(), "{shot}");
    assert_eq!(
        [data["width"].as_i64(), data["height"].as_i64()],
        [Some(width), Some(height)]
    );
    let image = Image::of(&shot);
    let shown = inner(dropdown, 4, width, height);
    assert!(
        image.share(shown, MAGENTA) > 0.95,
        "the dropdown is not in the image"
    );
    assert!(
        image.share([8, 8, 100, 100], DIALOG_GREEN) > 0.95,
        "the dialog is not in the image"
    );

    // The dropdown hangs past the dialog's bottom edge; a region shows it whole.
    assert!(dropdown[3] > height, "{dropdown:?}");
    let cropped = desk.tool("screenshot", json!({"region": dropdown}));
    desk.keep("region", &cropped);
    assert!(cropped["error"].is_null(), "{cropped}");
    assert_eq!(content(&cropped)["region"], json!(dropdown), "{cropped}");
    let whole = Image::of(&cropped);
    let all = inner(
        [0, 0, whole.width, whole.height],
        2,
        whole.width,
        whole.height,
    );
    assert!(
        whole.share(all, MAGENTA) > 0.95,
        "the region does not show the dropdown"
    );

    let zoomed = desk.tool("screenshot", json!({"cursor": true}));
    desk.keep("cursor", &zoomed);
    assert!(zoomed["error"].is_null(), "{zoomed}");
}

#[test]
#[ignore = "requires KDE, KWin, bubblewrap, pasta, cc, GTK 3, and input devices"]
fn a_combo_menu_over_a_wayland_dialog_is_in_its_screenshot() {
    let mut desk = Desk::start("wayland-combo");
    let fixture = desk.fixture();
    desk.launch(&format!("GDK_BACKEND=wayland {fixture} combo"), true);
    let dialog = desk.active_dialog();
    let (width, height) = (number(&dialog["w"]), number(&dialog["h"]));
    desk.click_middle(&dialog);
    // An xdg_popup is listed, transient for the dialog.
    let menu = desk.window_where(|window| {
        window["popup"] == json!(true) && window["transientFor"] == dialog["id"]
    });
    let region = relative_to(
        &dialog,
        [&menu["x"], &menu["y"], &menu["w"], &menu["h"]].map(number),
    );

    let shot = desk.shot_with_popup(json!({}));
    desk.keep("open", &shot);
    let data = content(&shot);
    assert_eq!(data["window"]["id"], dialog["id"], "{shot}");
    assert_eq!(
        data["transient_pixel_check"]["status"], "matches_window_capture",
        "{shot}"
    );
    assert_eq!(
        data["popups"],
        json!([{"region": region, "id": menu["id"]}]),
        "{shot}"
    );
    // The menu's right end holds no text.
    let image = Image::of(&shot);
    let [_, top, right, bottom] = inner(region, 10, width, height);
    assert!(
        image.share([right - 30, top, right, bottom], MAGENTA) > 0.9,
        "the menu is not in the image"
    );
}

#[test]
#[ignore = "requires KDE, KWin with Xwayland, bubblewrap, pasta, cc, GTK 3, and input devices"]
fn another_programs_window_over_the_dialog_is_still_refused() {
    let mut desk = Desk::start("x11-cover");
    let fixture = desk.fixture();
    let report = format!("{}/dropdown.txt", desk.workdir);
    desk.launch(&format!("GDK_BACKEND=x11 {fixture} plain {report}"), true);
    let dialog = desk.active_dialog();
    let clear = desk.tool("screenshot", json!({}));
    assert!(clear["error"].is_null(), "{clear}");

    // A second process maps an override-redirect window over part of the dialog.
    let cover = format!("{}/cover.txt", desk.workdir);
    let (left, top) = (number(&dialog["x"]) + 40, number(&dialog["y"]) + 40);
    desk.launch(
        &format!("GDK_BACKEND=x11 {fixture} cover {cover} {left} {top} 200 150"),
        false,
    );
    assert_eq!(reported(&cover), [left, top, 200, 150]);
    let windows = desk.windows();
    assert!(
        windows
            .iter()
            .any(|window| window["active"] == json!(true) && window["id"] == dialog["id"]),
        "the dialog is no longer active: {windows:?}"
    );

    let refused = desk.tool("screenshot", json!({}));
    desk.keep("refused", &refused);
    assert_eq!(
        refused["error"]["data"]["reason"], "transient_capture_unverified",
        "another program's window over the dialog was accepted: {refused}"
    );
    assert!(refused["result"].is_null(), "{refused}");
}
