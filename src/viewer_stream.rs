//! The remote viewer stream: what a viewer's frame socket carries and how the
//! sender chooses it.
//!
//! `kwin-viewer --stream-serve` runs beside the session and owns the listener's
//! end of the frame socket; `kwin-viewer --stream` runs on the machine with the
//! screen. The sender speaks first.
//!
//! Sender to viewer:
//! * text: one JSON [`Notice`] (`hello` once, then `title` on each change,
//!   `share` whenever the stream's share of its interface's budget changes,
//!   and `clipboard` in answer to a copy request);
//! * binary: a [`Header`] followed by its payload. `Raw` is `width * height`
//!   pixels of 3 bytes (red, green, blue): the session's own values, exact.
//!   `Dsc` is one VESA DSC picture for the picture parameter set last sent.
//!   `Pps` is that 128-byte set, sent before the first DSC picture and again
//!   whenever size or rate changes.
//!
//! Viewer to sender:
//! * binary: 17-byte input records (the viewer's `Op`), which include the
//!   drawable size and the first-presentation notice;
//! * text: one JSON [`Request`]: the clipboard requests, sent only to a
//!   sender whose `hello` says it bridges the clipboard, and `path` from a
//!   viewer that chose the path of its socket ([`crate::links`]).
//!
//! # Clipboard
//!
//! The session's clipboard belongs to the agent, and the clipboard of the
//! viewer's machine to the viewer's user. A paste shortcut pressed in the
//! window sends that machine's clipboard text ahead of the key
//! ([`Request::ClipboardPaste`]); a copy or cut shortcut asks for the
//! session's selection once the key changed it ([`Request::ClipboardCopy`],
//! answered by [`Notice::Clipboard`]). The sender puts the agent's selection
//! back after either. Only UTF-8 text crosses, at most
//! [`CLIPBOARD_TEXT_BYTES`] per message, and a longer one is refused, not cut.
//! The socket is not encrypted, so the text is readable on the network
//! between the two machines. A frame socket belongs to one session (its
//! ticket) and one viewer window, and these messages travel on no other
//! socket, so a viewer bridges its own session only. A viewer takes a
//! `clipboard` notice only as the answer to the last copy request it made.
//! Text messages of a type a peer does not know are ignored, and an older
//! sender ignores every text message, so either end may be older.
//!
//! A picture is sent only when the session's screen changed, scaled down to
//! the viewer's drawable size (never up). One picture is on its way at a time:
//! the next one is the newest capture once the last byte of the previous one
//! has left for the viewer, so nothing queues behind a slow or stalled viewer.
//!
//! # Mode: exact pixels unless DSC is sooner ([`ModeControl`])
//!
//! Measured, per frame socket:
//! * `link`: bits per second the socket carried, from each picture of at
//!   least [`LINK_SAMPLE_BYTES`] and the time until its last byte left. It
//!   starts at the stream's share of the link budget (the interface's speed
//!   times [`LINK_USABLE_PERCENT`], divided among the streams that leave the
//!   interface: [`crate::links::Shares`], which also sets what the socket is
//!   paced to) and then follows the measurements, a quarter of the
//!   difference per picture. When the share changes, it changes in the same
//!   proportion ([`ModeControl::share`]). A stream alone has the whole
//!   budget.
//! * `pack` and `encode`: time to pack a raw picture and to encode a DSC one,
//!   per pixel, smoothed the same way. While raw is not usable and no DSC
//!   picture gives it, the sender encodes one picture just to measure
//!   `encode`: before the first decision that needs it, and again after
//!   every [`ENCODE_REMEASURE_FRAMES`] raw pictures.
//!
//! For a picture of `pixels` pixels:
//! * `raw_time = pack + pixels * 24 / link`;
//! * `rate` = the highest whole DSC rate from 6 to 12 bits per pixel whose
//!   picture crosses the link within [`USABLE_FRAME_TIME`]: it follows the
//!   link alone, never the encode time;
//! * `dsc_time = encode + pixels * rate / link`.
//!
//! Rule:
//! 1. Raw while `raw_time <= USABLE_FRAME_TIME`: the link carries exact
//!    pixels at a usable rate.
//! 2. From raw to DSC when raw is not usable and DSC is at least a quarter
//!    sooner: `dsc_time <= raw_time * DSC_SOONER_PERCENT / 100`.
//! 3. From DSC back to raw when `raw_time <= USABLE_FRAME_TIME *
//!    RAW_RETURN_PERCENT / 100` or DSC is no longer sooner
//!    (`dsc_time >= raw_time`).
//! 4. A change of mode needs the rule to ask for it on
//!    [`SWITCH_AFTER_FRAMES`] pictures in a row; the first picture of a new
//!    size decides at once by 1 and 2.
//! 5. In DSC the rate rises when the link carries a higher one with
//!    [`RATE_RAISE_PERCENT`] of its speed, and falls when it no longer
//!    carries the current one, each on [`SWITCH_AFTER_FRAMES`] pictures in a
//!    row.
//!
//! # Settle to exact ([`Settle`])
//!
//! When the last picture went out as DSC and the screen stays unchanged for
//! [`SETTLE_AFTER`], the same picture goes out once more raw, marked
//! [`Header::settle`]: a still screen is always shown with exact pixels.

use crate::dsc;
use crate::links::Interface;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Bits per pixel of a raw picture, in 1/16 bit as the DSC rates are.
pub const RAW_BPP_X16: u16 = 24 * 16;
/// Share of the link speed the stream may fill.
pub const LINK_USABLE_PERCENT: u64 = 90;
/// Speed assumed for an interface that reports none (Wi-Fi, virtual links).
pub const UNKNOWN_LINK_MBPS: u64 = 1000;
/// A picture arrives at a usable rate when packing or encoding it and sending
/// it takes no longer than this: ten pictures a second.
pub const USABLE_FRAME_TIME: Duration = Duration::from_millis(100);
/// DSC replaces raw only when it takes at most this share of raw's time.
pub const DSC_SOONER_PERCENT: u64 = 75;
/// Raw replaces DSC once it takes at most this share of [`USABLE_FRAME_TIME`].
pub const RAW_RETURN_PERCENT: u64 = 90;
/// The DSC rate rises when this share of the link carries the higher rate.
pub const RATE_RAISE_PERCENT: u64 = 95;
/// Pictures in a row that must ask for another mode or rate before it changes.
pub const SWITCH_AFTER_FRAMES: u32 = 4;
/// While raw is not usable, the encoder is timed again after this many raw
/// pictures, so one slow measurement does not keep DSC away for good.
pub const ENCODE_REMEASURE_FRAMES: u32 = 32;
/// How long the screen stays unchanged after a DSC picture before the same
/// picture goes out exactly.
pub const SETTLE_AFTER: Duration = Duration::from_millis(150);
/// Smallest picture message whose sending time measures the link.
pub const LINK_SAMPLE_BYTES: usize = 64 << 10;
/// Shortest sending time a link measurement divides by: the sender looks for
/// the last byte's departure this often.
pub const LINK_SAMPLE_TIME: Duration = Duration::from_micros(250);
/// File beside the viewer's log where `kwin-viewer --stream` keeps its
/// counters: frames, bytes, size, mode, rate, mode switches, settles, and the
/// last second's rates and capture-to-present latency.
pub const COUNTERS_FILE: &str = "viewer-stream.json";
/// Names a file for `kwin-viewer --stream`: whenever a second passes without
/// a picture, it writes the picture it last presented there as a PNG.
pub const DUMP_ENV: &str = "KWIN_VIEWER_STREAM_DUMP";
/// Weights of one output sample of the scaler sum to this.
const WEIGHT_ONE: u16 = 256;
const BYTES_PER_PIXEL: usize = 4;
const RAW_BYTES_PER_PIXEL: usize = 3;
/// Measurements move a smoothed value by one part in this many.
const SMOOTHING: u64 = 4;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
/// `SIOCOUTQNSD` of linux/sockios.h: bytes of a socket's send queue that
/// have not been sent yet.
const SIOCOUTQNSD: nix::libc::Ioctl = 0x894B;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StreamError {
    #[error("frame buffer is smaller than its dimensions")]
    FrameTooSmall,
    #[error("output buffer does not have the picture's size")]
    OutputSize,
    #[error("dimensions {0}x{1} cannot be scaled to {2}x{3}")]
    Dimensions(u32, u32, u32, u32),
    #[error("a picture worker panicked")]
    WorkerPanicked,
}

/// Longest clipboard text one message carries, in bytes of UTF-8. Both ends
/// refuse a longer one: nothing is cut short.
pub const CLIPBOARD_TEXT_BYTES: usize = 1 << 20;
/// Longest text message that can hold such a text. JSON writes one byte of
/// text as at most six (`\u0000`); the rest is the fields around it.
pub const CLIPBOARD_MESSAGE_BYTES: usize = CLIPBOARD_TEXT_BYTES * 6 + 256;

/// Clipboard text on the frame socket. It has no `Display`, and its `Debug`
/// shows nothing of it, so it cannot be logged.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClipText(String);

impl ClipText {
    /// None when `text` is longer than `limit` bytes or than
    /// [`CLIPBOARD_TEXT_BYTES`].
    pub fn new(text: String, limit: usize) -> Option<Self> {
        (text.len() <= limit.min(CLIPBOARD_TEXT_BYTES)).then_some(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A message with a longer text does not parse.
impl TryFrom<String> for ClipText {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text, CLIPBOARD_TEXT_BYTES).ok_or("clipboard text is longer than the limit")
    }
}

impl From<ClipText> for String {
    fn from(text: ClipText) -> Self {
        text.0
    }
}

impl std::fmt::Debug for ClipText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClipText(withheld)")
    }
}

/// A text message from the sender.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    /// First message on the socket: the session's size and window title, and
    /// the budget the sender works to.
    Hello {
        width: u32,
        height: u32,
        title: String,
        /// Bits per second; absent when unlimited.
        budget_bits_per_second: Option<u64>,
        link: String,
        /// The sender bridges the clipboard (it takes [`Request`]s), with
        /// texts of at most this many bytes. Absent from a sender that does
        /// not, which a viewer then sends no request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clipboard_text_bytes: Option<u64>,
    },
    Title {
        title: String,
    },
    /// The answer to [`Request::ClipboardCopy`] `id`: the session's selection
    /// after the copy or cut. No text: the selection has none within the
    /// limit, and the viewer's clipboard stays as it is.
    Clipboard {
        id: u64,
        #[serde(default)]
        text: Option<ClipText>,
    },
    /// The stream's share of its interface's budget changed: `streams`
    /// streams that leave the session's machine on that interface are
    /// sending pictures in a row, and this one has `bits_per_second`. Until
    /// the first one, a stream has the whole budget `hello` named. For the
    /// viewer's log and counters only.
    Share {
        bits_per_second: u64,
        streams: u32,
    },
    /// A type this build does not know. It is ignored.
    #[serde(other)]
    Unknown,
}

/// A text message from the viewer: what its user's clipboard shortcut needs
/// of the session's clipboard, which goes out before the shortcut's key
/// record so that the sender handles the two in that order, or what the
/// sender should know about the viewer's link. A sender that does not know a
/// message ignores it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// A paste shortcut follows. `text` is the clipboard of the viewer's
    /// machine and becomes the session's selection for that paste; no text
    /// empties the selection, so the paste never inserts the agent's.
    /// `secret`: the text carries the generated-secret marker.
    ClipboardPaste {
        #[serde(default)]
        text: Option<ClipText>,
        #[serde(default)]
        secret: bool,
    },
    /// A copy or cut shortcut follows. The sender answers with
    /// [`Notice::Clipboard`] `id` once the session's selection changes, with
    /// a text of at most `max_bytes` bytes.
    ClipboardCopy { id: u64, max_bytes: u64 },
    /// The viewer's frame socket is on a path of its own choosing whose
    /// interface carries `bits_per_second`: the sender paces to no more.
    Path { bits_per_second: u64 },
    /// A type this build does not know. It is ignored.
    #[serde(other)]
    Unknown,
}

impl Request {
    /// The request in `message`; None when the message is too long to be one
    /// or does not parse. The reason is not reported: it could quote the text.
    pub fn decode(message: &str) -> Option<Self> {
        (message.len() <= CLIPBOARD_MESSAGE_BYTES)
            .then(|| serde_json::from_str(message).ok())
            .flatten()
    }
}

impl Notice {
    /// The notice in `message`, or where it stops parsing. The parser's own
    /// message is not reported: it could quote clipboard text.
    pub fn decode(message: &str) -> Result<Self, String> {
        if message.len() > CLIPBOARD_MESSAGE_BYTES {
            return Err(format!("{} bytes is longer than any notice", message.len()));
        }
        serde_json::from_str(message)
            .map_err(|error| format!("{:?} error at column {}", error.classify(), error.column()))
    }
}

/// What follows a [`Header`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    Raw,
    Dsc,
    Pps,
}

/// Length in bytes of an encoded [`Header`].
pub const HEADER_BYTES: usize = 24;
/// Bit of the header's flag byte that marks a settle picture.
const FLAG_SETTLE: u8 = 1;

/// The fixed head of every binary message from the sender, little-endian:
/// kind (1 raw, 2 DSC, 3 PPS), flags (bit 0: settle), width, height and rate
/// (u16 each), sequence (u32), capture time (u64) and sender time (u32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub payload: Payload,
    /// A raw picture that repeats the DSC picture before it exactly, sent
    /// because the screen stayed unchanged. It is not a change of mode.
    pub settle: bool,
    pub width: u16,
    pub height: u16,
    /// Bits per pixel times 16: [`RAW_BPP_X16`] or the DSC rate.
    pub bpp_x16: u16,
    /// Counts the pictures sent on this socket.
    pub sequence: u32,
    /// When PipeWire delivered the picture to the sender, in microseconds
    /// since the Unix epoch on the sender's clock.
    pub captured_micros: u64,
    /// Microseconds from capture until the picture was ready to send.
    pub sender_micros: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0u8; HEADER_BYTES];
        bytes[0] = match self.payload {
            Payload::Raw => 1,
            Payload::Dsc => 2,
            Payload::Pps => 3,
        };
        bytes[1] = if self.settle { FLAG_SETTLE } else { 0 };
        bytes[2..4].copy_from_slice(&self.width.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.height.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.bpp_x16.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.captured_micros.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.sender_micros.to_le_bytes());
        bytes
    }

    /// The header of `message` and the payload after it; None when the message
    /// is not one of ours.
    pub fn decode(message: &[u8]) -> Option<(Self, &[u8])> {
        let (head, payload) = message.split_at_checked(HEADER_BYTES)?;
        let kind = match head[0] {
            1 => Payload::Raw,
            2 => Payload::Dsc,
            3 => Payload::Pps,
            _ => return None,
        };
        let u16_at = |at: usize| Some(u16::from_le_bytes(head.get(at..at + 2)?.try_into().ok()?));
        let header = Self {
            payload: kind,
            settle: head[1] & FLAG_SETTLE != 0,
            width: u16_at(2)?,
            height: u16_at(4)?,
            bpp_x16: u16_at(6)?,
            sequence: u32::from_le_bytes(head.get(8..12)?.try_into().ok()?),
            captured_micros: u64::from_le_bytes(head.get(12..20)?.try_into().ok()?),
            sender_micros: u32::from_le_bytes(head.get(20..24)?.try_into().ok()?),
        };
        if header.width == 0 || header.height == 0 {
            return None;
        }
        Some((header, payload))
    }

    /// Bytes a raw picture of this size occupies after the header.
    pub fn raw_len(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * RAW_BYTES_PER_PIXEL
    }
}

/// The size pictures are sent at: the viewer's drawable size, but no larger
/// than the session in either direction.
pub fn fit_size(session: (u32, u32), drawable: (u32, u32)) -> (u32, u32) {
    (
        drawable.0.clamp(1, session.0.max(1)),
        drawable.1.clamp(1, session.1.max(1)),
    )
}

/// The bit budget of a frame socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkBudget {
    /// Bits per second the interface's streams may fill together; None is
    /// unlimited.
    pub bits_per_second: Option<u64>,
    /// Where the number came from, for the log and the viewer.
    pub link: String,
    /// The interface's name; empty when the address belongs to none.
    pub interface: String,
}

fn budget_of(override_mbps: Option<u64>, interface: Option<&Interface>) -> LinkBudget {
    let usable = |mbps: u64| Some(mbps.saturating_mul(1_000_000) / 100 * LINK_USABLE_PERCENT);
    let name = interface.map_or("unknown interface", |interface| interface.name.as_str());
    let budget = |bits_per_second: Option<u64>, link: String| LinkBudget {
        bits_per_second,
        link,
        interface: interface.map_or_else(String::new, |interface| interface.name.clone()),
    };
    if let Some(mbps) = override_mbps {
        return budget(usable(mbps), format!("{name}, --link-mbps {mbps}"));
    }
    let Some(interface) = interface else {
        return budget(
            usable(UNKNOWN_LINK_MBPS),
            format!("{name}, assumed {UNKNOWN_LINK_MBPS} Mb/s"),
        );
    };
    if interface.loopback {
        return budget(None, format!("{name}, loopback, unlimited"));
    }
    // A wireless interface has no speed of its own to send at: the rate
    // changes with the station and the second.
    match interface.mbps.filter(|_| !interface.wireless) {
        Some(mbps) => budget(usable(mbps), format!("{name}, {mbps} Mb/s")),
        None => budget(
            usable(UNKNOWN_LINK_MBPS),
            format!("{name} reports no speed, assumed {UNKNOWN_LINK_MBPS} Mb/s"),
        ),
    }
}

/// The budget for a frame socket whose local address is `local`: the speed of
/// the interface that holds the address (the one that routes to the peer)
/// times [`LINK_USABLE_PERCENT`]; unlimited on loopback; `override_mbps`
/// replaces the interface's speed. The streams that leave the interface
/// share this budget ([`crate::links::Shares`]).
pub fn link_budget(local: std::net::IpAddr, override_mbps: Option<u64>) -> LinkBudget {
    let table = crate::links::interfaces();
    budget_of(
        override_mbps,
        crate::links::first(&table, |interface| interface.holds(local)),
    )
}

/// Has the kernel send `socket`'s bytes no faster than `bits_per_second`
/// (`SO_MAX_PACING_RATE`), so the stream stays inside its budget on the real
/// link and `--link-mbps` behaves like a link of that speed on any other.
pub fn pace(socket: &impl std::os::fd::AsFd, bits_per_second: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let bytes_per_second: u64 = (bits_per_second / 8).max(1);
    let length = nix::libc::socklen_t::try_from(size_of::<u64>()).map_err(std::io::Error::other)?;
    // SAFETY: the kernel reads `length` bytes from `bytes_per_second`, which
    // lives until the call returns.
    let result = unsafe {
        nix::libc::setsockopt(
            socket.as_fd().as_raw_fd(),
            nix::libc::SOL_SOCKET,
            nix::libc::SO_MAX_PACING_RATE,
            (&raw const bytes_per_second).cast(),
            length,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// How long ago the TCP `socket` last sent data (`tcpi_last_data_sent` of
/// the kernel's `TCP_INFO`). On a socket with data to send, a long time means
/// that the peer takes none or that nothing comes back over the path.
pub fn since_data_sent(socket: &impl std::os::fd::AsFd) -> std::io::Result<Duration> {
    use std::os::fd::AsRawFd;
    let mut info = std::mem::MaybeUninit::<nix::libc::tcp_info>::zeroed();
    let mut length = nix::libc::socklen_t::try_from(size_of::<nix::libc::tcp_info>())
        .map_err(std::io::Error::other)?;
    // SAFETY: the kernel writes at most `length` bytes through the pointer,
    // which is valid for that many.
    let result = unsafe {
        nix::libc::getsockopt(
            socket.as_fd().as_raw_fd(),
            nix::libc::IPPROTO_TCP,
            nix::libc::TCP_INFO,
            info.as_mut_ptr().cast(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: every field is an integer, for which the zeroes the kernel
    // left are a value too.
    let info = unsafe { info.assume_init() };
    Ok(Duration::from_millis(u64::from(info.tcpi_last_data_sent)))
}

/// Bytes written to the TCP `socket` that the kernel has not sent yet. Zero
/// means the last byte has left for the peer.
pub fn unsent_bytes(socket: &impl std::os::fd::AsFd) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    let mut unsent: nix::libc::c_int = 0;
    // SAFETY: SIOCOUTQNSD writes one int through the pointer, which is valid
    // for the call.
    let result =
        unsafe { nix::libc::ioctl(socket.as_fd().as_raw_fd(), SIOCOUTQNSD, &raw mut unsent) };
    if result == 0 {
        Ok(usize::try_from(unsent).unwrap_or(0))
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// How a picture goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Exact pixels.
    Raw,
    /// VESA DSC at this many 1/16 bits per pixel.
    Dsc { bpp_x16: u16 },
}

impl Mode {
    pub fn is_raw(self) -> bool {
        self == Self::Raw
    }
}

/// What one sent picture cost.
#[derive(Clone, Copy, Debug)]
pub struct Sent {
    pub mode: Mode,
    pub pixels: u64,
    /// Packing (raw) or encoding (DSC); scaling is not part of it.
    pub prepare: Duration,
    /// Length of the picture's message.
    pub bytes: usize,
    /// From handing the message to the socket until its last byte left.
    pub wire: Duration,
}

/// What the rule expects of the next picture of some size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// None: unlimited and not measured yet.
    pub link_bits_per_second: Option<u64>,
    pub raw: Duration,
    pub dsc: Duration,
    /// The rate `dsc` is for.
    pub dsc_bpp_x16: u16,
}

fn smoothed(current: Option<u64>, sample: u64) -> u64 {
    match current {
        None => sample,
        Some(current) if sample >= current => current + (sample - current) / SMOOTHING,
        Some(current) => current - (current - sample) / SMOOTHING,
    }
}

fn narrow(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn nanos(duration: Duration) -> u64 {
    narrow(duration.as_nanos())
}

/// The mode of each picture on one frame socket. See the module text for the
/// rule.
#[derive(Clone, Debug)]
pub struct ModeControl {
    /// What the interface's streams may fill together.
    budget: Option<u64>,
    /// What this stream has of it.
    share: Option<u64>,
    /// Bits per second; the share until a picture measured it.
    link: Option<u64>,
    link_measured: bool,
    /// Picoseconds per pixel.
    pack: Option<u64>,
    encode: Option<u64>,
    /// The size the mode was chosen for.
    pixels: Option<u64>,
    mode: Mode,
    /// Pictures in a row for which the rule asked for the other mode.
    against_mode: u32,
    /// Pictures in a row for which the rule asked for another rate.
    against_rate: u32,
    switches: u64,
    /// Raw pictures since the encoder was last timed.
    raw_since_encode: u32,
}

impl ModeControl {
    /// `budget` is the link budget in bits per second; None is unlimited.
    pub fn new(budget: Option<u64>) -> Self {
        Self {
            budget,
            share: budget,
            link: budget,
            link_measured: false,
            pack: None,
            encode: None,
            pixels: None,
            mode: Mode::Raw,
            against_mode: 0,
            against_rate: 0,
            switches: 0,
            raw_since_encode: 0,
        }
    }

    /// Times the mode changed between raw and DSC.
    pub fn switches(&self) -> u64 {
        self.switches
    }

    /// The stream now has `share` bits per second of its interface's budget
    /// ([`crate::links::Grant::share`]). The link speed the rule works with
    /// changes in the same proportion at once, so the next picture is
    /// decided for the share the stream has; measurements carry on from
    /// there. A stream without a budget has no share.
    pub fn share(&mut self, share: u64) {
        let (Some(budget), Some(before)) = (self.budget, self.share) else {
            return;
        };
        let share = share.clamp(1, budget.max(1));
        if share == before {
            return;
        }
        self.share = Some(share);
        self.link = Some(match self.link.filter(|_| self.link_measured) {
            Some(link) => narrow(u128::from(link) * u128::from(share) / u128::from(before.max(1)))
                .clamp(1, budget.max(1)),
            None => share,
        });
    }

    fn wire(&self, bits: u64) -> Duration {
        self.link.map_or(Duration::ZERO, |link| {
            Duration::from_nanos(narrow(
                u128::from(bits) * NANOS_PER_SECOND / u128::from(link.max(1)),
            ))
        })
    }

    fn per_picture(picos_per_pixel: Option<u64>, pixels: u64) -> Duration {
        Duration::from_nanos(picos_per_pixel.unwrap_or(0).saturating_mul(pixels) / 1000)
    }

    /// The highest whole rate whose picture crosses `percent` of the link
    /// within [`USABLE_FRAME_TIME`], within the codec's range.
    fn carried_rate(&self, pixels: u64, percent: u64) -> u16 {
        let Some(link) = self.link else {
            return dsc::MAX_BPP_X16;
        };
        let bits = u128::from(link) * u128::from(percent) / 100 * USABLE_FRAME_TIME.as_nanos()
            / NANOS_PER_SECOND;
        let whole = narrow(bits / u128::from(pixels.max(1)));
        u16::try_from(whole.saturating_mul(16))
            .unwrap_or(dsc::MAX_BPP_X16)
            .clamp(dsc::MIN_BPP_X16, dsc::MAX_BPP_X16)
    }

    fn estimate_at(&self, pixels: u64, dsc_bpp_x16: u16) -> Estimate {
        let raw_bits = pixels.saturating_mul(u64::from(RAW_BPP_X16)) / 16;
        let dsc_bits = pixels.saturating_mul(u64::from(dsc_bpp_x16)) / 16;
        Estimate {
            link_bits_per_second: self.link,
            raw: Self::per_picture(self.pack, pixels) + self.wire(raw_bits),
            dsc: Self::per_picture(self.encode, pixels) + self.wire(dsc_bits),
            dsc_bpp_x16,
        }
    }

    /// What a picture of `pixels` pixels is expected to take: DSC at the
    /// current rate while that is the mode for this size, otherwise at the
    /// rate the link carries.
    pub fn estimate(&self, pixels: u64) -> Estimate {
        let rate = match self.mode {
            Mode::Dsc { bpp_x16 } if self.pixels == Some(pixels) => bpp_x16,
            Mode::Dsc { .. } | Mode::Raw => self.carried_rate(pixels, 100),
        };
        self.estimate_at(pixels, rate)
    }

    /// Whether the rule asks for DSC, given the mode it is in.
    fn wants_dsc(estimate: &Estimate, lossy: bool) -> bool {
        let (raw, dsc, usable) = (
            estimate.raw.as_nanos(),
            estimate.dsc.as_nanos(),
            USABLE_FRAME_TIME.as_nanos(),
        );
        if lossy {
            raw * 100 > usable * u128::from(RAW_RETURN_PERCENT) && dsc < raw
        } else {
            raw > usable && dsc * 100 <= raw * u128::from(DSC_SOONER_PERCENT)
        }
    }

    /// Whether the rule can give DSC for the next picture of `pixels`
    /// pixels: DSC is the mode for this size, or raw is too slow for the rule
    /// to keep or return to it at once. Otherwise the sender needs no encoder.
    pub fn may_choose_dsc(&self, pixels: u64) -> bool {
        let lossy = !self.mode.is_raw() && self.pixels == Some(pixels);
        lossy
            || self.estimate(pixels).raw.as_nanos() * 100
                > USABLE_FRAME_TIME.as_nanos() * u128::from(RAW_RETURN_PERCENT)
    }

    /// Whether the next decision for `pixels` pixels needs an encode time
    /// the sender has to measure: raw is not usable, and no picture has been
    /// encoded yet or none for [`ENCODE_REMEASURE_FRAMES`] raw pictures. The
    /// sender then encodes one picture and reports it with
    /// [`Self::encode_measured`].
    pub fn wants_encode_time(&self, pixels: u64) -> bool {
        (self.encode.is_none() || self.raw_since_encode >= ENCODE_REMEASURE_FRAMES)
            && self.estimate(pixels).raw > USABLE_FRAME_TIME
    }

    /// An encode of `pixels` pixels took `time`.
    pub fn encode_measured(&mut self, pixels: u64, time: Duration) {
        let sample = narrow(time.as_nanos() * 1000 / u128::from(pixels.max(1)));
        self.encode = Some(smoothed(self.encode, sample));
        self.raw_since_encode = 0;
    }

    fn switch_to(&mut self, mode: Mode) {
        if mode.is_raw() != self.mode.is_raw() {
            self.switches += 1;
        }
        self.mode = mode;
        self.against_mode = 0;
        self.against_rate = 0;
    }

    /// The mode for the next picture of `pixels` pixels. `dsc_available` is
    /// false for a size the codec has no slice layout for, which goes out
    /// raw.
    pub fn mode(&mut self, pixels: u64, dsc_available: bool) -> Mode {
        let first = self.pixels.is_none();
        let resized = self.pixels != Some(pixels);
        self.pixels = Some(pixels);
        if !dsc_available {
            if !first {
                self.switch_to(Mode::Raw);
            }
            return self.mode;
        }
        if resized {
            let carried = self.carried_rate(pixels, 100);
            let chosen = if Self::wants_dsc(&self.estimate_at(pixels, carried), false) {
                Mode::Dsc { bpp_x16: carried }
            } else {
                Mode::Raw
            };
            if first {
                self.mode = chosen;
            } else {
                self.switch_to(chosen);
            }
            return self.mode;
        }
        let lossy = !self.mode.is_raw();
        if Self::wants_dsc(&self.estimate(pixels), lossy) == lossy {
            self.against_mode = 0;
        } else {
            self.against_mode += 1;
            if self.against_mode >= SWITCH_AFTER_FRAMES {
                self.switch_to(if lossy {
                    Mode::Raw
                } else {
                    Mode::Dsc {
                        bpp_x16: self.carried_rate(pixels, 100),
                    }
                });
                return self.mode;
            }
        }
        if let Mode::Dsc { bpp_x16 } = self.mode {
            let raised = self.carried_rate(pixels, RATE_RAISE_PERCENT);
            let carried = self.carried_rate(pixels, 100);
            let wanted = if raised > bpp_x16 {
                raised
            } else {
                carried.min(bpp_x16)
            };
            if wanted == bpp_x16 {
                self.against_rate = 0;
            } else {
                self.against_rate += 1;
                if self.against_rate >= SWITCH_AFTER_FRAMES {
                    self.against_rate = 0;
                    self.mode = Mode::Dsc { bpp_x16: wanted };
                }
            }
        }
        self.mode
    }

    /// Accounts for one sent picture: its pack or encode time and, when the
    /// message was large enough to measure it, the link's speed.
    pub fn sent(&mut self, sent: &Sent) {
        let sample = narrow(sent.prepare.as_nanos() * 1000 / u128::from(sent.pixels.max(1)));
        match sent.mode {
            Mode::Raw => {
                self.pack = Some(smoothed(self.pack, sample));
                self.raw_since_encode = self.raw_since_encode.saturating_add(1);
            }
            Mode::Dsc { .. } => {
                self.encode = Some(smoothed(self.encode, sample));
                self.raw_since_encode = 0;
            }
        }
        if sent.bytes < LINK_SAMPLE_BYTES {
            return;
        }
        let bits = u128::try_from(sent.bytes).unwrap_or(u128::MAX) * 8;
        let wire = u128::from(nanos(sent.wire.max(LINK_SAMPLE_TIME)));
        let mut speed = narrow(bits * NANOS_PER_SECOND / wire);
        if let Some(budget) = self.budget {
            speed = speed.min(budget);
        }
        // The first measurement replaces the nominal speed.
        let known = self.link.filter(|_| self.link_measured);
        self.link = Some(smoothed(known, speed).max(1));
        self.link_measured = true;
    }
}

/// When a still picture that went out as DSC goes out again exactly.
#[derive(Clone, Copy, Debug, Default)]
pub struct Settle {
    /// When the last picture went out, if it was DSC.
    lossy_since: Option<Instant>,
}

impl Settle {
    /// A picture went out at `now`; `lossy` when it was DSC.
    pub fn sent(&mut self, lossy: bool, now: Instant) {
        self.lossy_since = lossy.then_some(now);
    }

    /// How long from `now` until the exact picture is due, as long as the
    /// screen stays unchanged: zero when it is due, None when the viewer
    /// already has exact pixels.
    pub fn due_in(&self, now: Instant) -> Option<Duration> {
        self.lossy_since
            .map(|since| (since + SETTLE_AFTER).saturating_duration_since(now))
    }
}

/// One batch of borrowed work for the threads of [`Workers`].
#[derive(Clone, Copy)]
struct Batch {
    work: &'static (dyn Fn(usize) + Sync),
    count: usize,
}

#[derive(Default)]
struct WorkState {
    batch: Option<Batch>,
    /// The next index of the batch that nobody has taken.
    next: usize,
    /// Indices taken and not finished.
    running: usize,
    panicked: bool,
    closing: bool,
}

#[derive(Default)]
struct WorkShared {
    state: Mutex<WorkState>,
    /// A batch was published, or the pool is closing.
    published: Condvar,
    /// The last running index of a batch finished.
    finished: Condvar,
}

impl WorkShared {
    fn state(&self) -> MutexGuard<'_, WorkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(state: &mut WorkState) -> Option<(Batch, usize)> {
        let batch = state.batch.filter(|batch| state.next < batch.count)?;
        let index = state.next;
        state.next += 1;
        state.running += 1;
        Some((batch, index))
    }

    fn run_one(&self, batch: Batch, index: usize) {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (batch.work)(index)));
        let mut state = self.state();
        state.running -= 1;
        state.panicked |= outcome.is_err();
        if state.running == 0 {
            self.finished.notify_all();
        }
    }

    fn serve(&self) {
        let mut state = self.state();
        while !state.closing {
            match Self::take(&mut state) {
                Some((batch, index)) => {
                    drop(state);
                    self.run_one(batch, index);
                    state = self.state();
                }
                None => {
                    state = self
                        .published
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

/// Threads that live as long as a stream and share out each picture's work,
/// so no thread is created per picture.
pub struct Workers {
    shared: Arc<WorkShared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Workers {
    /// `threads` threads in all, counting the caller of [`Self::run`].
    pub fn new(threads: usize) -> Self {
        let shared = Arc::new(WorkShared::default());
        let threads = (1..threads)
            .filter_map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::Builder::new()
                    .name("picture-worker".to_owned())
                    .spawn(move || shared.serve())
                    .ok()
            })
            .collect();
        Self { shared, threads }
    }

    /// One thread per core this process may use.
    pub fn all_cores() -> Self {
        Self::new(std::thread::available_parallelism().map_or(1, usize::from))
    }

    /// Threads that share a batch, counting the caller.
    pub fn threads(&self) -> usize {
        self.threads.len() + 1
    }

    /// Calls `work(index)` once for every index below `count`, on this
    /// thread and the pool's, and returns when all calls have returned.
    pub fn run(&mut self, count: usize, work: &(dyn Fn(usize) + Sync)) -> Result<(), StreamError> {
        if count <= 1 || self.threads.is_empty() {
            (0..count).for_each(work);
            return Ok(());
        }
        // SAFETY: the reference outlives its borrow only inside `state.batch`
        // and the copies `take` hands out. Every copy is counted in
        // `state.running` under the lock, this call returns only after
        // `running` is back to zero with no index left to take, and it clears
        // `batch` before it returns. A panic in `work` is caught in
        // `run_one`, on this thread too, so nothing unwinds past the wait.
        let work = unsafe {
            std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(work)
        };
        {
            let mut state = self.shared.state();
            state.batch = Some(Batch { work, count });
            state.next = 0;
            state.panicked = false;
        }
        self.shared.published.notify_all();
        loop {
            let taken = WorkShared::take(&mut self.shared.state());
            let Some((batch, index)) = taken else { break };
            self.shared.run_one(batch, index);
        }
        let mut state = self.shared.state();
        while state.running > 0 {
            state = self
                .shared
                .finished
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.batch = None;
        if state.panicked {
            return Err(StreamError::WorkerPanicked);
        }
        Ok(())
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.shared.state().closing = true;
        self.shared.published.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Most bands one picture is cut into.
const MAX_BANDS: usize = 64;

/// Runs `work(first_row, rows, state)` over `out` cut into bands of whole
/// rows, one band per element of `states`, on the workers.
fn in_bands<S: Send>(
    workers: &mut Workers,
    out: &mut [u8],
    row_bytes: usize,
    states: &mut [S],
    work: impl Fn(usize, &mut [u8], &mut S) + Sync,
) -> Result<(), StreamError> {
    type Band<'a, S> = Mutex<Option<(usize, &'a mut [u8], &'a mut S)>>;
    let rows = out.len() / row_bytes.max(1);
    let bands = states
        .len()
        .min(workers.threads())
        .min(MAX_BANDS)
        .clamp(1, rows.max(1));
    let band_rows = rows.div_ceil(bands).max(1);
    let slots: [Band<'_, S>; MAX_BANDS] = std::array::from_fn(|_| Mutex::new(None));
    let mut count = 0;
    for (index, ((band, state), slot)) in out
        .chunks_mut(band_rows * row_bytes.max(1))
        .zip(states.iter_mut())
        .zip(&slots)
        .enumerate()
    {
        *slot.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((index * band_rows, band, state));
        count = index + 1;
    }
    workers.run(count, &|index| {
        let taken = slots
            .get(index)
            .and_then(|slot| slot.lock().unwrap_or_else(PoisonError::into_inner).take());
        if let Some((first_row, band, state)) = taken {
            work(first_row, band, state);
        }
    })
}

fn low_byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

/// The source samples one output sample averages, and their weights.
struct Tap {
    first: usize,
    weights: std::ops::Range<usize>,
}

/// One direction of an area downscale: every output sample is the mean of the
/// source interval it covers, each source sample weighted by its overlap.
struct Axis {
    taps: Vec<Tap>,
    weights: Vec<u16>,
}

impl Axis {
    fn new(source: usize, target: usize) -> Self {
        let (source_wide, target_wide) = (
            u64::try_from(source).unwrap_or(u64::MAX),
            u64::try_from(target).unwrap_or(u64::MAX),
        );
        let mut taps = Vec::with_capacity(target);
        let mut weights: Vec<u16> = Vec::new();
        // In units of 1/target of a source sample, output `index` covers
        // [index * source, (index + 1) * source) and source sample `sample`
        // covers [sample * target, (sample + 1) * target).
        for index in 0..target_wide {
            let (low, high) = (index * source_wide, (index + 1) * source_wide);
            let (first, last) = (low / target_wide, (high - 1) / target_wide);
            let start = weights.len();
            let mut total = 0u16;
            let mut widest = (start, 0u64);
            for sample in first..=last {
                let overlap = high.min((sample + 1) * target_wide) - low.max(sample * target_wide);
                let weight = u16::try_from(overlap * u64::from(WEIGHT_ONE) / source_wide)
                    .unwrap_or(WEIGHT_ONE);
                if overlap > widest.1 {
                    widest = (weights.len(), overlap);
                }
                total += weight;
                weights.push(weight);
            }
            // Rounding down leaves a remainder; the widest overlap takes it so
            // a flat colour stays exactly that colour.
            if let Some(weight) = weights.get_mut(widest.0) {
                *weight += WEIGHT_ONE - total;
            }
            taps.push(Tap {
                first: usize::try_from(first).unwrap_or(usize::MAX),
                weights: start..weights.len(),
            });
        }
        Self { taps, weights }
    }
}

/// One band's working rows of the scaler, kept between pictures.
#[derive(Default)]
struct ScaleRows {
    /// One source-width row: the source rows an output row covers, summed.
    sums: Vec<u16>,
    /// Their mean.
    mean: Vec<u8>,
}

/// Area (box) downscale of 4-byte pixels from one fixed size to another.
pub struct Scaler {
    source: (usize, usize),
    target: (usize, usize),
    columns: Axis,
    rows: Axis,
    bands: Vec<ScaleRows>,
}

impl Scaler {
    /// A scaler from `source` to `target`, which must be no larger in either
    /// direction.
    pub fn new(source: (u32, u32), target: (u32, u32)) -> Result<Self, StreamError> {
        let invalid = || StreamError::Dimensions(source.0, source.1, target.0, target.1);
        if target.0 == 0 || target.1 == 0 || target.0 > source.0 || target.1 > source.1 {
            return Err(invalid());
        }
        let size = |value: u32| usize::try_from(value).map_err(|_| invalid());
        let (source, target) = (
            (size(source.0)?, size(source.1)?),
            (size(target.0)?, size(target.1)?),
        );
        Ok(Self {
            source,
            target,
            columns: Axis::new(source.0, target.0),
            rows: Axis::new(source.1, target.1),
            bands: Vec::new(),
        })
    }

    /// Bytes of a scaled picture: tightly packed rows of the target size.
    pub fn scaled_len(&self) -> usize {
        self.target.0 * self.target.1 * BYTES_PER_PIXEL
    }

    /// Scales `source` (rows `stride` bytes apart) into `out`, which must be
    /// [`Self::scaled_len`] bytes. The fourth byte of each pixel is averaged
    /// like the others. Nothing is allocated after the first picture.
    pub fn scale(
        &mut self,
        workers: &mut Workers,
        source: &[u8],
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), StreamError> {
        let source_row = self.source.0 * BYTES_PER_PIXEL;
        if stride < source_row || source.len() < (self.source.1 - 1) * stride + source_row {
            return Err(StreamError::FrameTooSmall);
        }
        if out.len() != self.scaled_len() {
            return Err(StreamError::OutputSize);
        }
        let target_row = self.target.0 * BYTES_PER_PIXEL;
        let Self {
            columns,
            rows,
            bands,
            ..
        } = self;
        if bands.len() != workers.threads() {
            bands.resize_with(workers.threads(), ScaleRows::default);
        }
        in_bands(
            workers,
            out,
            target_row,
            bands,
            |first_row, band, scratch| {
                scratch.sums.resize(source_row, 0);
                scratch.mean.resize(source_row, 0);
                let (sums, mean) = (&mut scratch.sums, &mut scratch.mean);
                for (row, out_row) in band.chunks_exact_mut(target_row).enumerate() {
                    let Some(tap) = rows.taps.get(first_row + row) else {
                        return;
                    };
                    sums.fill(0);
                    let weights = rows.weights.get(tap.weights.clone()).unwrap_or(&[]);
                    for (offset, weight) in weights.iter().enumerate() {
                        let start = (tap.first + offset) * stride;
                        let Some(line) = source.get(start..start + source_row) else {
                            return;
                        };
                        for (sum, sample) in sums.iter_mut().zip(line) {
                            *sum += weight * u16::from(*sample);
                        }
                    }
                    for (byte, sum) in mean.iter_mut().zip(sums.iter()) {
                        *byte = (sum + WEIGHT_ONE / 2).to_le_bytes()[1];
                    }
                    for (pixel, tap) in out_row.chunks_exact_mut(BYTES_PER_PIXEL).zip(&columns.taps)
                    {
                        let weights = columns.weights.get(tap.weights.clone()).unwrap_or(&[]);
                        let start = tap.first * BYTES_PER_PIXEL;
                        let samples = mean
                            .get(start..start + weights.len() * BYTES_PER_PIXEL)
                            .unwrap_or(&[]);
                        let mut sum = [u32::from(WEIGHT_ONE / 2); BYTES_PER_PIXEL];
                        for (weight, sample) in
                            weights.iter().zip(samples.chunks_exact(BYTES_PER_PIXEL))
                        {
                            for (sum, channel) in sum.iter_mut().zip(sample) {
                                *sum += u32::from(*weight) * u32::from(*channel);
                            }
                        }
                        for (byte, sum) in pixel.iter_mut().zip(sum) {
                            *byte = low_byte(sum >> 8);
                        }
                    }
                }
            },
        )
    }
}

/// Bytes of a raw payload of this size.
pub fn packed_len(size: (u32, u32)) -> usize {
    usize::try_from(u64::from(size.0) * u64::from(size.1))
        .unwrap_or(usize::MAX)
        .saturating_mul(RAW_BYTES_PER_PIXEL)
}

/// Writes the picture's pixels to `out` as 3 bytes each (red, green, blue),
/// dropping the padding byte of `BGRx` rows `stride` bytes apart. `out` must
/// be [`packed_len`] bytes.
pub fn pack_rgb(
    workers: &mut Workers,
    source: &[u8],
    stride: usize,
    size: (u32, u32),
    out: &mut [u8],
) -> Result<(), StreamError> {
    let (width, height) = (
        usize::try_from(size.0).map_err(|_| StreamError::FrameTooSmall)?,
        usize::try_from(size.1).map_err(|_| StreamError::FrameTooSmall)?,
    );
    let source_row = width * BYTES_PER_PIXEL;
    if width == 0
        || height == 0
        || stride < source_row
        || source.len() < (height - 1) * stride + source_row
    {
        return Err(StreamError::FrameTooSmall);
    }
    let packed_row = width * RAW_BYTES_PER_PIXEL;
    if out.len() != packed_row * height {
        return Err(StreamError::OutputSize);
    }
    let mut bands = [(); MAX_BANDS];
    in_bands(
        workers,
        out,
        packed_row,
        &mut bands,
        |first_row, band, ()| {
            for (row, packed) in band.chunks_exact_mut(packed_row).enumerate() {
                let at = (first_row + row) * stride;
                let Some(line) = source.get(at..at + source_row) else {
                    return;
                };
                for (rgb, bgrx) in packed
                    .chunks_exact_mut(RAW_BYTES_PER_PIXEL)
                    .zip(line.chunks_exact(BYTES_PER_PIXEL))
                {
                    rgb[0] = bgrx[2];
                    rgb[1] = bgrx[1];
                    rgb[2] = bgrx[0];
                }
            }
        },
    )
}

/// Expands a raw payload (3 bytes per pixel, `width` pixels a row) into `out`
/// as `RGBA` with full alpha. `out` must hold 4 bytes for each pixel.
pub fn unpack_rgb(
    workers: &mut Workers,
    rgb: &[u8],
    width: usize,
    out: &mut [u8],
) -> Result<(), StreamError> {
    let width = width.max(1);
    if rgb.len() / RAW_BYTES_PER_PIXEL * BYTES_PER_PIXEL != out.len() {
        return Err(StreamError::OutputSize);
    }
    let mut bands = [(); MAX_BANDS];
    in_bands(
        workers,
        out,
        width * BYTES_PER_PIXEL,
        &mut bands,
        |first_row, band, ()| {
            let at = first_row * width * RAW_BYTES_PER_PIXEL;
            let Some(source) = rgb.get(at..) else { return };
            for (rgba, rgb) in band
                .chunks_exact_mut(BYTES_PER_PIXEL)
                .zip(source.chunks_exact(RAW_BYTES_PER_PIXEL))
            {
                rgba[0] = rgb[0];
                rgba[1] = rgb[1];
                rgba[2] = rgb[2];
                rgba[3] = 255;
            }
        },
    )
}

#[cfg(test)]
mod tests;
