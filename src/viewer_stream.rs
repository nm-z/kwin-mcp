//! The remote viewer stream: what a viewer's frame socket carries and how the
//! sender chooses it.
//!
//! `kwin-viewer --stream-serve` runs beside the session and owns the listener's
//! end of the frame socket; `kwin-viewer --stream` runs on the machine with the
//! screen. The sender speaks first.
//!
//! Sender to viewer:
//! * text: one JSON [`Notice`] (`hello` once, then `title` on each change);
//! * binary: a [`Header`] followed by its payload. `Raw` is `width * height`
//!   pixels of 3 bytes (red, green, blue). `Dsc` is one VESA DSC picture for
//!   the picture parameter set last sent. `Pps` is that 128-byte set, sent
//!   before the first DSC picture and again whenever size or rate changes.
//!
//! Viewer to sender: binary 17-byte input records (the viewer's `Op`), which
//! include the drawable size and the first-presentation notice.
//!
//! A picture is sent only when the session's screen changed, scaled down to
//! the viewer's drawable size (never up), so every picture of one viewer size
//! and mode has the same length.
//!
//! Mode: with `budget` = the link's speed times [`LINK_USABLE_PERCENT`], a
//! picture goes out raw when `width * height * 24 * 60 <= budget`, otherwise
//! as DSC starting at 8 bits per pixel.
//!
//! Rate rule ([`RateControl`]), applied after each DSC picture with that
//! picture's time (scaling, encoding and sending) and its complexity `c`
//! (`FrameStats::complexity_permille`, 0 = coded without quantization):
//! 1. slow: each of the last [`CONSISTENT_FRAMES`] pictures took longer than
//!    one 60 Hz period;
//! 2. over budget: `width * height * bpp * 60 > budget`;
//! 3. on slow or over budget, lower the rate by `1/4 + 3/4 * (1000 - c) / 1000`
//!    bits per pixel (a simple picture gives up the most), to no less than 6,
//!    and when only the budget asks, to no less than the budget allows;
//! 4. otherwise, when each of the last [`CONSISTENT_FRAMES`] pictures took less
//!    than half a period, raise it by `c / 1000` bits per pixel (a picture
//!    already coded exactly gains nothing), to no more than 12 or the budget;
//! 5. a change starts both counts again, and the new picture parameter set
//!    goes out before the next picture.

use crate::dsc;
use std::time::Duration;

/// Pictures per second the budget and the rate rule are measured against.
pub const FRAME_RATE: u64 = 60;
/// One period of [`FRAME_RATE`].
pub const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / FRAME_RATE);
/// Bits per pixel of a raw picture, in 1/16 bit as the DSC rates are.
pub const RAW_BPP_X16: u16 = 24 * 16;
/// Share of the link speed the stream may fill.
pub const LINK_USABLE_PERCENT: u64 = 90;
/// Speed assumed for an interface that reports none (Wi-Fi, virtual links).
pub const UNKNOWN_LINK_MBPS: u64 = 1000;
/// How many pictures in a row make a timing "consistent" for the rate rule.
pub const CONSISTENT_FRAMES: u32 = 8;
/// File beside the viewer's log where `kwin-viewer --stream` keeps its
/// counters: frames, bytes, size, mode, rate, and the last second's rates and
/// capture-to-present latency.
pub const COUNTERS_FILE: &str = "viewer-stream.json";
/// Weights of one output sample of the scaler sum to this.
const WEIGHT_ONE: u16 = 256;
const BYTES_PER_PIXEL: usize = 4;
const RAW_BYTES_PER_PIXEL: usize = 3;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StreamError {
    #[error("frame buffer is smaller than its dimensions")]
    FrameTooSmall,
    #[error("dimensions {0}x{1} cannot be scaled to {2}x{3}")]
    Dimensions(u32, u32, u32, u32),
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
    },
    Title {
        title: String,
    },
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

/// The fixed head of every binary message from the sender, little-endian:
/// kind (1 raw, 2 DSC, 3 PPS), a zero byte, width, height and rate (u16
/// each), sequence (u32), capture time (u64) and sender time (u32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub payload: Payload,
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

/// Bits per second of a stream of `pixels`-pixel pictures at [`FRAME_RATE`].
pub fn stream_bits_per_second(pixels: u64, bpp_x16: u16) -> u64 {
    pixels
        .saturating_mul(u64::from(bpp_x16))
        .saturating_mul(FRAME_RATE)
        / 16
}

/// Whether raw pictures of this size fit the budget at [`FRAME_RATE`].
pub fn raw_fits(width: u32, height: u32, budget: Option<u64>) -> bool {
    let pixels = u64::from(width) * u64::from(height);
    budget.is_none_or(|budget| stream_bits_per_second(pixels, RAW_BPP_X16) <= budget)
}

/// The bit budget of a frame socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkBudget {
    /// Bits per second the stream may fill; None is unlimited.
    pub bits_per_second: Option<u64>,
    /// Where the number came from, for the log and the viewer.
    pub link: String,
}

/// The interface a socket's local address belongs to.
struct Interface {
    name: String,
    loopback: bool,
    /// Contents of its `speed` file, when readable.
    speed: Option<String>,
}

fn budget_of(override_mbps: Option<u64>, interface: Option<&Interface>) -> LinkBudget {
    let usable = |mbps: u64| Some(mbps.saturating_mul(1_000_000) / 100 * LINK_USABLE_PERCENT);
    let name = interface.map_or("unknown interface", |interface| interface.name.as_str());
    if let Some(mbps) = override_mbps {
        return LinkBudget {
            bits_per_second: usable(mbps),
            link: format!("{name}, --link-mbps {mbps}"),
        };
    }
    let Some(interface) = interface else {
        return LinkBudget {
            bits_per_second: usable(UNKNOWN_LINK_MBPS),
            link: format!("{name}, assumed {UNKNOWN_LINK_MBPS} Mb/s"),
        };
    };
    if interface.loopback {
        return LinkBudget {
            bits_per_second: None,
            link: format!("{name}, loopback, unlimited"),
        };
    }
    let speed = interface
        .speed
        .as_deref()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|mbps| *mbps > 0);
    match speed {
        Some(mbps) => LinkBudget {
            bits_per_second: usable(mbps),
            link: format!("{name}, {mbps} Mb/s"),
        },
        None => LinkBudget {
            bits_per_second: usable(UNKNOWN_LINK_MBPS),
            link: format!("{name} reports no speed, assumed {UNKNOWN_LINK_MBPS} Mb/s"),
        },
    }
}

/// The budget for a frame socket whose local address is `local`: the speed of
/// the interface that holds the address (the one that routes to the peer)
/// times [`LINK_USABLE_PERCENT`]; unlimited on loopback; `override_mbps`
/// replaces the interface's speed.
pub fn link_budget(local: std::net::IpAddr, override_mbps: Option<u64>) -> LinkBudget {
    let local = local.to_canonical();
    let interface = nix::ifaddrs::getifaddrs().ok().and_then(|addresses| {
        addresses
            .filter(|entry| {
                let address = entry.address.as_ref();
                let v4 = address
                    .and_then(|address| address.as_sockaddr_in())
                    .map(|address| std::net::IpAddr::V4(address.ip()));
                let v6 = address
                    .and_then(|address| address.as_sockaddr_in6())
                    .map(|address| std::net::IpAddr::V6(address.ip()).to_canonical());
                v4.or(v6) == Some(local)
            })
            .map(|entry| Interface {
                loopback: entry
                    .flags
                    .contains(nix::net::if_::InterfaceFlags::IFF_LOOPBACK),
                speed: std::fs::read_to_string(format!(
                    "/sys/class/net/{}/speed",
                    entry.interface_name
                ))
                .ok(),
                name: entry.interface_name,
            })
            .next()
    });
    budget_of(override_mbps, interface.as_ref())
}

/// DSC rate for the pictures of one viewer size. See the module text for the rule.
#[derive(Clone, Debug)]
pub struct RateControl {
    bpp_x16: u16,
    slow: u32,
    fast: u32,
}

impl Default for RateControl {
    fn default() -> Self {
        Self {
            bpp_x16: dsc::DEFAULT_BPP_X16,
            slow: 0,
            fast: 0,
        }
    }
}

impl RateControl {
    /// Rate for the next picture, in 1/16 bit per pixel.
    pub fn bpp_x16(&self) -> u16 {
        self.bpp_x16
    }

    /// Accounts for one sent DSC picture of `pixels` pixels that took `time`
    /// to scale, encode and send. Returns whether the rate changed.
    pub fn after_frame(
        &mut self,
        pixels: u64,
        time: Duration,
        complexity_permille: u32,
        budget: Option<u64>,
    ) -> bool {
        let complexity = u16::try_from(complexity_permille.min(1000)).unwrap_or(1000);
        self.slow = if time > FRAME_PERIOD {
            self.slow + 1
        } else {
            0
        };
        self.fast = if time * 2 < FRAME_PERIOD {
            self.fast + 1
        } else {
            0
        };
        // The highest rate the budget carries at FRAME_RATE.
        let ceiling = budget.map_or(dsc::MAX_BPP_X16, |budget| {
            let rate = budget.saturating_mul(16) / pixels.max(1).saturating_mul(FRAME_RATE);
            u16::try_from(rate)
                .unwrap_or(dsc::MAX_BPP_X16)
                .clamp(dsc::MIN_BPP_X16, dsc::MAX_BPP_X16)
        });
        let over_budget =
            budget.is_some_and(|budget| stream_bits_per_second(pixels, self.bpp_x16) > budget);
        let slow = self.slow >= CONSISTENT_FRAMES;
        let next = if slow || over_budget {
            let step = 4 + 12 * (1000 - complexity) / 1000;
            let lowered = self.bpp_x16.saturating_sub(step).max(dsc::MIN_BPP_X16);
            if slow { lowered } else { lowered.max(ceiling) }
        } else if self.fast >= CONSISTENT_FRAMES {
            (self.bpp_x16 + 16 * complexity / 1000).min(ceiling)
        } else {
            self.bpp_x16
        };
        if next == self.bpp_x16 {
            return false;
        }
        self.bpp_x16 = next;
        self.slow = 0;
        self.fast = 0;
        true
    }
}

/// Runs `work(first_row, rows)` over `out` cut into bands of whole rows, on up
/// to `threads` threads.
fn in_bands(
    out: &mut [u8],
    row_bytes: usize,
    threads: usize,
    work: impl Fn(usize, &mut [u8]) + Sync,
) {
    let rows = out.len() / row_bytes.max(1);
    let threads = threads.clamp(1, rows.max(1));
    if threads == 1 {
        work(0, out);
        return;
    }
    let band_rows = rows.div_ceil(threads);
    std::thread::scope(|scope| {
        for (band, chunk) in out.chunks_mut(band_rows * row_bytes).enumerate() {
            let work = &work;
            scope.spawn(move || work(band * band_rows, chunk));
        }
    });
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

/// Area (box) downscale of 4-byte pixels from one fixed size to another.
pub struct Scaler {
    source: (usize, usize),
    target: (usize, usize),
    columns: Axis,
    rows: Axis,
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
        })
    }

    /// Scales `source` (rows `stride` bytes apart) into `out`, which becomes
    /// tightly packed rows of the target size. The fourth byte of each pixel
    /// is averaged like the others.
    pub fn scale(
        &self,
        source: &[u8],
        stride: usize,
        out: &mut Vec<u8>,
        threads: usize,
    ) -> Result<(), StreamError> {
        let source_row = self.source.0 * BYTES_PER_PIXEL;
        if stride < source_row || source.len() < (self.source.1 - 1) * stride + source_row {
            return Err(StreamError::FrameTooSmall);
        }
        let target_row = self.target.0 * BYTES_PER_PIXEL;
        out.clear();
        out.resize(target_row * self.target.1, 0);
        in_bands(out, target_row, threads, |first_row, band| {
            // One source-width row: the rows this output row covers, averaged.
            let mut sums = vec![0u16; source_row];
            let mut mean = vec![0u8; source_row];
            for (row, out_row) in band.chunks_exact_mut(target_row).enumerate() {
                let Some(tap) = self.rows.taps.get(first_row + row) else {
                    return;
                };
                sums.fill(0);
                let weights = self.rows.weights.get(tap.weights.clone()).unwrap_or(&[]);
                for (offset, weight) in weights.iter().enumerate() {
                    let start = (tap.first + offset) * stride;
                    let Some(line) = source.get(start..start + source_row) else {
                        return;
                    };
                    for (sum, sample) in sums.iter_mut().zip(line) {
                        *sum += weight * u16::from(*sample);
                    }
                }
                for (byte, sum) in mean.iter_mut().zip(&sums) {
                    *byte = (sum + WEIGHT_ONE / 2).to_le_bytes()[1];
                }
                for (pixel, tap) in out_row
                    .chunks_exact_mut(BYTES_PER_PIXEL)
                    .zip(&self.columns.taps)
                {
                    let weights = self.columns.weights.get(tap.weights.clone()).unwrap_or(&[]);
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
        });
        Ok(())
    }
}

/// Appends the picture's pixels to `out` as 3 bytes each (red, green, blue),
/// dropping the padding byte of `BGRx` rows `stride` bytes apart.
pub fn pack_rgb(
    source: &[u8],
    stride: usize,
    size: (u32, u32),
    out: &mut Vec<u8>,
    threads: usize,
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
    let start = out.len();
    out.resize(start + packed_row * height, 0);
    let Some(packed) = out.get_mut(start..) else {
        return Err(StreamError::FrameTooSmall);
    };
    in_bands(packed, packed_row, threads, |first_row, band| {
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
    });
    Ok(())
}

/// Expands a raw payload (3 bytes per pixel) to `RGBA` with full alpha.
pub fn unpack_rgb(rgb: &[u8], width: usize, out: &mut Vec<u8>, threads: usize) {
    let pixels = rgb.len() / RAW_BYTES_PER_PIXEL;
    out.clear();
    out.resize(pixels * BYTES_PER_PIXEL, 255);
    let row = width.max(1) * BYTES_PER_PIXEL;
    in_bands(out, row, threads, |first_row, band| {
        let at = first_row * width.max(1) * RAW_BYTES_PER_PIXEL;
        let Some(source) = rgb.get(at..) else { return };
        for (rgba, rgb) in band
            .chunks_exact_mut(BYTES_PER_PIXEL)
            .zip(source.chunks_exact(RAW_BYTES_PER_PIXEL))
        {
            rgba[0] = rgb[0];
            rgba[1] = rgb[1];
            rgba[2] = rgb[2];
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        CONSISTENT_FRAMES, FRAME_PERIOD, HEADER_BYTES, Header, Interface, LINK_USABLE_PERCENT,
        Notice, Payload, RAW_BPP_X16, RateControl, Scaler, StreamError, UNKNOWN_LINK_MBPS,
        budget_of, fit_size, pack_rgb, raw_fits, stream_bits_per_second, unpack_rgb,
    };
    use crate::dsc;
    use std::time::Duration;

    #[test]
    fn a_header_survives_the_wire_and_foreign_bytes_do_not_parse() {
        let header = Header {
            payload: Payload::Dsc,
            width: 1280,
            height: 720,
            bpp_x16: 128,
            sequence: 7,
            captured_micros: 1_762_732_800_123_456,
            sender_micros: 31_000,
        };
        let mut message = header.encode().to_vec();
        message.extend_from_slice(&[9, 8, 7]);
        assert_eq!(Header::decode(&message), Some((header, &[9u8, 8, 7][..])));
        assert_eq!(Header::decode(&message[..HEADER_BYTES - 1]), None);
        message[0] = 0;
        assert_eq!(Header::decode(&message), None);
        let empty = Header { width: 0, ..header };
        assert_eq!(Header::decode(&empty.encode()), None);
        let raw = Header {
            payload: Payload::Raw,
            bpp_x16: RAW_BPP_X16,
            ..header
        };
        assert_eq!(raw.raw_len(), 1280 * 720 * 3);
    }

    #[test]
    fn notices_are_tagged_json() -> Result<(), serde_json::Error> {
        let hello = Notice::Hello {
            width: 1920,
            height: 1080,
            title: "kwin-viewer s1".to_owned(),
            budget_bits_per_second: None,
            link: "lo, loopback, unlimited".to_owned(),
        };
        let text = serde_json::to_string(&hello)?;
        assert!(text.contains("\"type\":\"hello\""), "{text}");
        assert_eq!(serde_json::from_str::<Notice>(&text)?, hello);
        let title: Notice =
            serde_json::from_str("{\"type\":\"title\",\"title\":\"Enter the code\"}")?;
        assert_eq!(
            title,
            Notice::Title {
                title: "Enter the code".to_owned()
            }
        );
        Ok(())
    }

    #[test]
    fn pictures_are_never_larger_than_the_session() {
        assert_eq!(fit_size((1920, 1080), (1280, 720)), (1280, 720));
        assert_eq!(fit_size((1920, 1080), (3840, 2160)), (1920, 1080));
        assert_eq!(fit_size((1920, 1080), (2560, 700)), (1920, 700));
        assert_eq!(fit_size((1920, 1080), (0, 0)), (1, 1));
    }

    #[test]
    fn raw_goes_out_only_when_sixty_pictures_a_second_fit() {
        // The issue's table: 2.99 Gb/s, 1.33 Gb/s and 0.75 Gb/s raw.
        assert_eq!(
            stream_bits_per_second(1920 * 1080, RAW_BPP_X16),
            2_985_984_000
        );
        assert_eq!(
            stream_bits_per_second(1280 * 720, dsc::DEFAULT_BPP_X16),
            442_368_000
        );
        let gigabit = Some(1000 * 1_000_000 / 100 * LINK_USABLE_PERCENT);
        assert!(!raw_fits(1920, 1080, gigabit));
        assert!(!raw_fits(1280, 720, gigabit));
        assert!(raw_fits(960, 540, gigabit));
        assert!(raw_fits(1920, 1080, None));
        assert!(raw_fits(1920, 1080, Some(2_985_984_000)));
        assert!(!raw_fits(1920, 1080, Some(2_985_983_999)));
    }

    #[test]
    fn the_budget_is_the_link_speed_unless_loopback_or_overridden() {
        let wired = Interface {
            name: "enp10s0".to_owned(),
            loopback: false,
            speed: Some("1000\n".to_owned()),
        };
        assert_eq!(
            budget_of(None, Some(&wired)).bits_per_second,
            Some(900_000_000)
        );
        let loopback = Interface {
            name: "lo".to_owned(),
            loopback: true,
            speed: None,
        };
        assert_eq!(budget_of(None, Some(&loopback)).bits_per_second, None);
        assert_eq!(
            budget_of(Some(100), Some(&loopback)).bits_per_second,
            Some(90_000_000)
        );
        // Wi-Fi and down links report nothing or -1.
        let assumed = Some(UNKNOWN_LINK_MBPS * 900_000);
        for speed in [None, Some("-1\n".to_owned()), Some(String::new())] {
            let wireless = Interface {
                name: "wlan0".to_owned(),
                loopback: false,
                speed,
            };
            assert_eq!(budget_of(None, Some(&wireless)).bits_per_second, assumed);
        }
        assert_eq!(budget_of(None, None).bits_per_second, assumed);
        assert!(
            budget_of(Some(250), Some(&wired))
                .link
                .contains("--link-mbps 250")
        );
    }

    const PIXELS_720P: u64 = 1280 * 720;

    #[test]
    fn a_rate_over_the_budget_falls_to_what_the_budget_carries() {
        // 400 Mb/s link: 360 Mb/s usable carries 6.5 bpp at 1280x720.
        let budget = Some(360_000_000);
        let mut rate = RateControl::default();
        assert_eq!(rate.bpp_x16(), dsc::DEFAULT_BPP_X16);
        let quick = Duration::from_millis(1);
        assert!(rate.after_frame(PIXELS_720P, quick, 0, budget));
        assert_eq!(rate.bpp_x16(), 112, "a simple picture gives up a whole bit");
        assert!(rate.after_frame(PIXELS_720P, quick, 0, budget));
        assert_eq!(rate.bpp_x16(), 104, "no lower than the budget asks");
        for _ in 0..3 * CONSISTENT_FRAMES {
            assert!(!rate.after_frame(PIXELS_720P, quick, 1000, budget));
        }
        assert_eq!(
            rate.bpp_x16(),
            104,
            "headroom never lifts it over the budget"
        );
    }

    #[test]
    fn consistently_slow_pictures_lower_the_rate_and_a_complex_one_gives_up_less() {
        let slow = FRAME_PERIOD + Duration::from_millis(1);
        let mut rate = RateControl::default();
        for _ in 1..CONSISTENT_FRAMES {
            assert!(!rate.after_frame(PIXELS_720P, slow, 1000, None));
        }
        assert!(rate.after_frame(PIXELS_720P, slow, 1000, None));
        assert_eq!(
            rate.bpp_x16(),
            124,
            "a complex picture gives up a quarter bit"
        );
        // One picture on time starts the count again.
        for _ in 1..CONSISTENT_FRAMES {
            assert!(!rate.after_frame(PIXELS_720P, slow, 0, None));
        }
        assert!(!rate.after_frame(PIXELS_720P, FRAME_PERIOD, 0, None));
        for _ in 1..CONSISTENT_FRAMES {
            assert!(!rate.after_frame(PIXELS_720P, slow, 0, None));
        }
        assert!(rate.after_frame(PIXELS_720P, slow, 0, None));
        assert_eq!(rate.bpp_x16(), 108);
        for _ in 0..20 * CONSISTENT_FRAMES {
            rate.after_frame(PIXELS_720P, slow, 0, None);
        }
        assert_eq!(rate.bpp_x16(), dsc::MIN_BPP_X16);
    }

    #[test]
    fn headroom_raises_the_rate_for_pictures_that_were_quantized() {
        let quick = FRAME_PERIOD / 4;
        let mut rate = RateControl::default();
        for _ in 0..3 * CONSISTENT_FRAMES {
            assert!(
                !rate.after_frame(PIXELS_720P, quick, 0, None),
                "exact pictures gain nothing"
            );
        }
        for _ in 1..CONSISTENT_FRAMES {
            assert!(!rate.after_frame(PIXELS_720P, FRAME_PERIOD / 4 * 3, 500, None));
        }
        for _ in 1..CONSISTENT_FRAMES {
            assert!(!rate.after_frame(PIXELS_720P, quick, 500, None));
        }
        assert!(rate.after_frame(PIXELS_720P, quick, 500, None));
        assert_eq!(rate.bpp_x16(), 136);
        for _ in 0..20 * CONSISTENT_FRAMES {
            rate.after_frame(PIXELS_720P, quick, 1000, None);
        }
        assert_eq!(rate.bpp_x16(), dsc::MAX_BPP_X16);
    }

    fn solid(width: usize, height: usize, stride: usize, pixel: [u8; 4]) -> Vec<u8> {
        let mut data = vec![0xEEu8; stride * height];
        for row in data.chunks_exact_mut(stride) {
            for target in row[..width * 4].chunks_exact_mut(4) {
                target.copy_from_slice(&pixel);
            }
        }
        data
    }

    #[test]
    fn scaling_averages_the_area_each_output_pixel_covers() -> Result<(), StreamError> {
        // 4x2 to 2x1: each output pixel is the mean of a 2x2 block.
        let source: Vec<u8> = [
            [0u8, 0, 0, 0],
            [100, 40, 8, 0],
            [200, 200, 200, 0],
            [200, 200, 200, 0],
            [0, 0, 0, 0],
            [100, 40, 8, 0],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
        ]
        .concat();
        let mut out = Vec::new();
        Scaler::new((4, 2), (2, 1))?.scale(&source, 16, &mut out, 1)?;
        assert_eq!(out, [50, 20, 4, 0, 100, 100, 100, 0]);
        // 3 to 2 columns: weights 2/3 and 1/3.
        let source: Vec<u8> = [[0u8, 0, 0, 0], [90, 90, 90, 0], [30, 30, 30, 0]].concat();
        Scaler::new((3, 1), (2, 1))?.scale(&source, 12, &mut out, 1)?;
        assert_eq!(out, [30, 30, 30, 0, 50, 50, 50, 0]);
        Ok(())
    }

    #[test]
    fn scaling_keeps_flat_colour_exact_and_ignores_row_padding_and_threads()
    -> Result<(), StreamError> {
        let pixel = [17, 130, 255, 255];
        let source = solid(1920, 1080, 1920 * 4 + 64, pixel);
        for target in [(1280, 720), (1277, 713), (1920, 540), (1, 1), (1920, 1080)] {
            let scaler = Scaler::new((1920, 1080), target)?;
            let (mut one, mut many) = (Vec::new(), Vec::new());
            scaler.scale(&source, 1920 * 4 + 64, &mut one, 1)?;
            scaler.scale(&source, 1920 * 4 + 64, &mut many, 7)?;
            assert_eq!(
                one.len(),
                usize::try_from(target.0 * target.1 * 4).unwrap_or(0)
            );
            assert!(
                one.chunks_exact(4).all(|scaled| scaled == pixel),
                "{target:?}"
            );
            assert_eq!(one, many, "{target:?}");
        }
        Ok(())
    }

    #[test]
    fn scaling_a_gradient_matches_single_threaded_output_and_never_upscales()
    -> Result<(), StreamError> {
        let (width, height) = (640usize, 360usize);
        let mut source = vec![0u8; width * height * 4];
        for (index, byte) in source.iter_mut().enumerate() {
            *byte = u8::try_from((index * 31 + index / 7) % 251).unwrap_or(0);
        }
        let scaler = Scaler::new((640, 360), (417, 233))?;
        let (mut one, mut many) = (Vec::new(), Vec::new());
        scaler.scale(&source, width * 4, &mut one, 1)?;
        scaler.scale(&source, width * 4, &mut many, 5)?;
        assert_eq!(one, many);
        assert!(Scaler::new((640, 360), (641, 360)).is_err());
        assert!(Scaler::new((640, 360), (0, 10)).is_err());
        assert_eq!(
            scaler.scale(&source[..source.len() - 1], width * 4, &mut one, 1),
            Err(StreamError::FrameTooSmall)
        );
        Ok(())
    }

    #[test]
    fn raw_pixels_drop_the_padding_byte_and_come_back_as_rgba() -> Result<(), StreamError> {
        let (width, height, stride) = (5usize, 3usize, 5 * 4 + 8);
        let mut source = vec![0xEEu8; stride * height];
        let mut expected = Vec::new();
        for (row, line) in source.chunks_exact_mut(stride).enumerate() {
            for (column, pixel) in line[..width * 4].chunks_exact_mut(4).enumerate() {
                let value = u8::try_from(row * 16 + column).unwrap_or(0);
                pixel.copy_from_slice(&[value, value + 100, value + 200, 0]);
                expected.extend_from_slice(&[value + 200, value + 100, value, 255]);
            }
        }
        for threads in [1, 4] {
            let mut wire = vec![1, 2];
            pack_rgb(&source, stride, (5, 3), &mut wire, threads)?;
            assert_eq!(wire.len(), 2 + width * height * 3);
            let mut rgba = Vec::new();
            unpack_rgb(&wire[2..], width, &mut rgba, threads);
            assert_eq!(rgba, expected);
        }
        let mut wire = Vec::new();
        assert_eq!(
            pack_rgb(&source[..10], stride, (5, 3), &mut wire, 1),
            Err(StreamError::FrameTooSmall)
        );
        Ok(())
    }
}
