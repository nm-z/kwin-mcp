//! Software VESA Display Stream Compression (DSC 1.1 / 1.2a) codec for
//! 8 bits-per-component RGB 4:4:4 pictures at a constant bit rate.
//!
//! The algorithm follows the VESA DSC C reference model (DSC_model_20211213,
//! v1.67): RGB to YCoCg-R, slices, MMAP / block / midpoint prediction, the
//! indexed colour history, delta-size unit VLC, substream multiplexing with
//! 48-bit mux words and the rate-control buffer model. The picture parameter
//! set uses the standard 128-byte layout.
//!
//! Every encoded picture of one configuration has the same length:
//! `chunk_size * slice_height` bytes per slice, slices of one row interleaved
//! line by line (chunk by chunk), as the DSC transport sends them.
//!
//! Speed comes from four places, none of which changes a byte of output:
//!
//! - Slices are coded on a pool of worker threads that lives as long as any
//!   [`Encoder`] or [`Decoder`] of the process (`Pool`), and every codec
//!   object keeps its per-thread buffers between pictures.
//! - Line buffers hold whole pixels, so the three components of a group are
//!   predicted and quantized side by side in 16-bit lanes (`Lanes`); the
//!   colour history search and the colour conversion are `Kernels` with SSE2
//!   and AVX2 implementations next to the portable one.
//! - The slice loops are compiled once per instruction set (`Level`) and the
//!   copy for the running CPU is chosen at run time.
//! - Work that follows the picture data (bit writing, the block-prediction
//!   search, clamping) is written without branches.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;

const NUM_BUF_RANGES: usize = 15;
const PAD_LEFT: usize = 5;
const PAD_RIGHT: usize = 10;
const ICH_SIZE: usize = 32;
const ICH_ABOVE: usize = 7;
const ICH_RESERVED: usize = ICH_SIZE - ICH_ABOVE;
const ICH_BITS: i32 = 5;
const ICH_LAMBDA: i32 = 4;
const MUX_WORD_BYTES: usize = 6;
const MUX_WORD_BITS: i32 = 48;
const MAX_SE_SIZE: i32 = 36;
const RC_MODEL_SIZE: i64 = 8192;
const OFFSET_FRACTIONAL_BITS: u32 = 11;
const OVERFLOW_AVOID_THRESHOLD: i64 = -172;
/// Block prediction needs an edge among the last this many pixels; with the
/// three-pixel groups that is an edge inside the group itself.
const BP_EDGE_COUNT: usize = 3;
const BP_EDGE_STRENGTH: i16 = 32;
const FLATNESS_DET_THRESH: i32 = 2;
const SOMEWHAT_FLAT_QP_THRESH: i32 = 7;
const SOMEWHAT_FLAT_QP_DELTA: i32 = 4;
const VERY_FLAT_QP: i32 = 1;
const MAX_QP: i32 = 15;
/// Size in bytes of a serialized picture parameter set.
pub const PPS_SIZE: usize = 128;
/// Smallest supported rate, in 1/16 bit per pixel (6 bpp).
pub const MIN_BPP_X16: u16 = 96;
/// Largest supported rate, in 1/16 bit per pixel (12 bpp).
pub const MAX_BPP_X16: u16 = 192;
/// Default rate, in 1/16 bit per pixel (8 bpp).
pub const DEFAULT_BPP_X16: u16 = 128;

/// Bit depth of Y, Co and Cg for 8 bpc RGB input.
const DEPTH: [i32; 3] = [8, 9, 9];
const QLEVEL_LUMA: [i32; 16] = [0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 5, 6, 7];
const QLEVEL_CHROMA: [i32; 16] = [0, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 8, 8, 8];
const RC_BUF_THRESH: [u16; 14] = [
    896, 1792, 2688, 3584, 4480, 5376, 6272, 6720, 7168, 7616, 7744, 7872, 8000, 8064,
];
const RC_BUF_THRESH_6BPP: [u16; 14] = [
    896, 1792, 2688, 3584, 4480, 5376, 6272, 6720, 7168, 7616, 7744, 7872, 7936, 8000,
];

/// Recommended parameters for one integer rate (C model `rc_8bpc_*bpp.cfg`,
/// identical to `rc_parameters_1_2_444` in the Linux `drm_dsc_helper.c`).
struct RcPreset {
    bpp_x16: i64,
    initial_xmit_delay: i64,
    initial_offset: i64,
    flatness_max_qp: u8,
    min_qp: [u8; 15],
    max_qp: [u8; 15],
    offset: [i8; 15],
}

const RC_PRESETS: [RcPreset; 4] = [
    RcPreset {
        bpp_x16: 96,
        initial_xmit_delay: 768,
        initial_offset: 6144,
        flatness_max_qp: 13,
        min_qp: [0, 1, 3, 4, 5, 5, 6, 6, 7, 8, 9, 10, 10, 11, 13],
        max_qp: [4, 6, 8, 8, 9, 9, 9, 10, 11, 12, 12, 12, 12, 12, 14],
        offset: [
            0, -2, -2, -4, -6, -6, -6, -8, -8, -10, -10, -12, -12, -12, -12,
        ],
    },
    RcPreset {
        bpp_x16: 128,
        initial_xmit_delay: 512,
        initial_offset: 6144,
        flatness_max_qp: 12,
        min_qp: [0, 0, 1, 1, 3, 3, 3, 3, 3, 3, 5, 5, 5, 9, 12],
        max_qp: [4, 4, 5, 6, 7, 7, 7, 8, 9, 10, 10, 11, 11, 12, 13],
        offset: [
            2, 0, 0, -2, -4, -6, -8, -8, -8, -10, -10, -12, -12, -12, -12,
        ],
    },
    RcPreset {
        bpp_x16: 160,
        initial_xmit_delay: 410,
        initial_offset: 5632,
        flatness_max_qp: 12,
        min_qp: [0, 0, 1, 2, 3, 3, 3, 3, 3, 3, 5, 5, 5, 7, 11],
        max_qp: [3, 4, 5, 6, 7, 7, 7, 8, 9, 9, 10, 10, 11, 11, 12],
        offset: [
            2, 0, 0, -2, -4, -6, -8, -8, -8, -10, -10, -10, -12, -12, -12,
        ],
    },
    RcPreset {
        bpp_x16: 192,
        initial_xmit_delay: 341,
        initial_offset: 2048,
        flatness_max_qp: 12,
        min_qp: [0, 0, 1, 1, 3, 3, 3, 3, 3, 3, 5, 5, 5, 7, 10],
        max_qp: [2, 4, 5, 6, 7, 7, 7, 8, 8, 9, 9, 9, 9, 10, 11],
        offset: [
            2, 0, 0, -2, -4, -6, -8, -8, -8, -10, -10, -12, -12, -12, -12,
        ],
    },
];

/// `minqp444_8b` of the C model's `rc_tables.h`, columns for 6..=12 bpp in
/// half-bpp steps (used for rates between the integer presets).
const MIN_QP_444_8B: [[u8; 13]; 15] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1],
    [3, 3, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1],
    [5, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3],
    [5, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3],
    [5, 5, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3],
    [5, 5, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3],
    [5, 5, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3],
    [6, 5, 5, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3],
    [6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5],
    [6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5],
    [6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5],
    [9, 9, 9, 9, 8, 8, 7, 7, 7, 7, 7, 7, 7],
    [14, 14, 13, 13, 12, 12, 12, 12, 11, 11, 10, 10, 10],
];
/// `maxqp444_8b` of the C model's `rc_tables.h`, same columns.
const MAX_QP_444_8B: [[u8; 13]; 15] = [
    [4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 2, 2, 2],
    [6, 6, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 3],
    [8, 7, 7, 6, 5, 5, 5, 5, 5, 5, 5, 5, 4],
    [8, 8, 7, 7, 6, 6, 6, 6, 6, 6, 6, 6, 5],
    [9, 8, 8, 7, 7, 7, 7, 7, 7, 7, 7, 7, 6],
    [9, 8, 8, 8, 7, 7, 7, 7, 7, 7, 7, 7, 6],
    [9, 9, 8, 8, 7, 7, 7, 7, 7, 7, 7, 7, 7],
    [10, 10, 9, 9, 8, 8, 8, 8, 8, 8, 8, 8, 8],
    [11, 11, 10, 10, 9, 9, 9, 9, 9, 9, 8, 8, 8],
    [12, 11, 11, 10, 10, 10, 9, 9, 9, 9, 9, 9, 9],
    [12, 12, 11, 11, 10, 10, 10, 10, 10, 10, 9, 9, 9],
    [12, 12, 12, 11, 11, 11, 10, 10, 10, 10, 9, 9, 9],
    [12, 12, 12, 12, 11, 11, 11, 11, 11, 10, 10, 9, 9],
    [13, 13, 13, 13, 12, 12, 11, 11, 11, 11, 10, 10, 10],
    [15, 15, 14, 14, 13, 13, 13, 13, 12, 12, 11, 11, 11],
];
const OFFSET_UNDER_6BPP: [i8; 15] = [
    0, -2, -2, -4, -6, -6, -8, -8, -8, -10, -10, -12, -12, -12, -12,
];
const OFFSET_UNDER_8BPP: [i8; 15] = [
    2, 0, 0, -2, -4, -6, -8, -8, -8, -10, -10, -10, -12, -12, -12,
];

/// Errors reported by the DSC encoder and decoder.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DscError {
    #[error("invalid DSC configuration: {0}")]
    Config(&'static str),
    #[error("unsupported picture parameter set: {0}")]
    UnsupportedPps(&'static str),
    #[error("frame buffer is smaller than the configured picture")]
    FrameTooSmall,
    #[error("bitstream is {got} bytes, the picture parameter set needs {want}")]
    BitstreamLength { got: usize, want: usize },
    #[error("rate control failure: {0}")]
    RateControl(&'static str),
    #[error("a codec worker thread panicked")]
    Worker,
}

/// Byte order of the 32-bit pixels handed to the encoder or wanted from the
/// decoder. The fourth byte is ignored on input and written as 255 on output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Blue, green, red, padding (PipeWire `BGRx`).
    Bgrx,
    /// Red, green, blue, padding (PipeWire `RGBx`).
    Rgbx,
}

/// A borrowed picture: rows of 4-byte pixels, `stride` bytes apart.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub format: PixelFormat,
}

/// Encoder configuration; [`Pps::from_config`] derives every other parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    /// Slice width in pixels; `ceil(width / slice_width)` slices per line.
    pub slice_width: u32,
    /// Slice height in lines; a picture whose height is not a multiple is
    /// padded at the bottom as the standard prescribes.
    pub slice_height: u32,
    /// Target rate in 1/16 bit per pixel, `MIN_BPP_X16..=MAX_BPP_X16`.
    pub bits_per_pixel_x16: u16,
    pub block_pred_enable: bool,
    /// 1 for DSC 1.1 behaviour, 2 for DSC 1.2a.
    pub version_minor: u8,
    /// Bits per component kept in the previous-line buffer (9 keeps YCoCg exact).
    pub linebuf_depth: u8,
}

impl Config {
    /// Default configuration for a picture: 8 bpp, DSC 1.2a, block prediction
    /// on, four slices per line and a slice height near 108 lines that divides
    /// the picture height when such a divisor exists. Narrow or short pictures
    /// get fewer, larger slices until the rate-control parameters are valid
    /// for the whole 6..=12 bpp range.
    pub fn new(width: u32, height: u32) -> Result<Self, DscError> {
        if width == 0 || height == 0 {
            return Err(DscError::Config("picture dimensions must be nonzero"));
        }
        let mut heights = vec![default_slice_height(height)];
        if height > 108 && !heights.contains(&height) {
            heights.push(height);
        }
        let mut last = DscError::Config("no valid slice layout");
        for slice_height in heights {
            for slices in [4u32, 2, 1] {
                let config = Self {
                    width,
                    height,
                    slice_width: width.div_ceil(slices),
                    slice_height,
                    bits_per_pixel_x16: DEFAULT_BPP_X16,
                    block_pred_enable: true,
                    version_minor: 2,
                    linebuf_depth: 9,
                };
                let mut valid = Ok(());
                for bpp in [MIN_BPP_X16, DEFAULT_BPP_X16, MAX_BPP_X16] {
                    let probe = Self {
                        bits_per_pixel_x16: bpp,
                        ..config.clone()
                    };
                    if let Err(error) = Pps::from_config(&probe) {
                        valid = Err(error);
                        break;
                    }
                }
                match valid {
                    Ok(()) => return Ok(config),
                    Err(error) => last = error,
                }
            }
        }
        Err(last)
    }
}

fn default_slice_height(height: u32) -> u32 {
    if height <= 108 {
        return height;
    }
    let mut best = 108;
    let mut best_distance = u32::MAX;
    for candidate in 54..=216u32 {
        if height.is_multiple_of(candidate) {
            let distance = candidate.abs_diff(108);
            if distance < best_distance {
                best = candidate;
                best_distance = distance;
            }
        }
    }
    best
}

/// One rate-control range of the picture parameter set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RcRange {
    pub min_qp: u8,
    pub max_qp: u8,
    pub bpg_offset: i8,
}

/// DSC picture parameter set for 8 bpc RGB 4:4:4 (fields as in the standard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pps {
    pub dsc_version_minor: u8,
    pub pps_identifier: u8,
    pub bits_per_component: u8,
    pub linebuf_depth: u8,
    pub block_pred_enable: bool,
    pub convert_rgb: bool,
    pub simple_422: bool,
    pub vbr_enable: bool,
    pub bits_per_pixel_x16: u16,
    pub pic_height: u16,
    pub pic_width: u16,
    pub slice_height: u16,
    pub slice_width: u16,
    pub chunk_size: u16,
    pub initial_xmit_delay: u16,
    pub initial_dec_delay: u16,
    pub initial_scale_value: u8,
    pub scale_increment_interval: u16,
    pub scale_decrement_interval: u16,
    pub first_line_bpg_offset: u8,
    pub nfl_bpg_offset: u16,
    pub slice_bpg_offset: u16,
    pub initial_offset: u16,
    pub final_offset: u16,
    pub flatness_min_qp: u8,
    pub flatness_max_qp: u8,
    pub rc_model_size: u16,
    pub rc_edge_factor: u8,
    pub rc_quant_incr_limit0: u8,
    pub rc_quant_incr_limit1: u8,
    pub rc_tgt_offset_hi: u8,
    pub rc_tgt_offset_lo: u8,
    pub rc_buf_thresh: [u16; 14],
    pub rc_range_parameters: [RcRange; NUM_BUF_RANGES],
    pub native_420: bool,
    pub native_422: bool,
    pub second_line_bpg_offset: u8,
    pub nsl_bpg_offset: u16,
    pub second_line_offset_adj: u16,
}

/// Quantization shift of Y, Co and Cg for every quantization parameter.
const QLEVELS: [[i32; 3]; 16] = {
    let mut levels = [[0; 3]; 16];
    let mut qp = 0;
    while qp < 16 {
        levels[qp] = [QLEVEL_LUMA[qp], QLEVEL_CHROMA[qp], QLEVEL_CHROMA[qp]];
        qp += 1;
    }
    levels
};

/// Quantization shift of Y, Co and Cg at `qp`.
#[inline(always)]
fn qp_levels(qp: i32) -> [i32; 3] {
    QLEVELS[usize::try_from(qp.clamp(0, MAX_QP)).unwrap_or(0) % QLEVELS.len()]
}

#[inline(always)]
fn qlevel(component: usize, qp: i32) -> i32 {
    qp_levels(qp)[component.min(2)]
}

fn ceil_div(numerator: i64, denominator: i64) -> i64 {
    (numerator + denominator - 1).div_euclid(denominator)
}

fn narrow<T: TryFrom<i64>>(value: i64, what: &'static str) -> Result<T, DscError> {
    T::try_from(value).map_err(|_| DscError::Config(what))
}

/// The C model's `check_qp_for_overflow`: whether the last range's maximum QP
/// cannot guarantee that the rate-control model drains in the worst case.
fn max_qp_may_overflow(max_qp: i32, slice_width: i64, bpp: i64, slice_bpg: i64, nfl: i64) -> bool {
    let full_groups = slice_width / 3;
    let padding = if slice_width % 3 == 0 {
        0
    } else {
        3 - slice_width % 3
    };
    let mut mpp_group = 1i64;
    let mut zero_group = 1i64;
    for (component, depth) in DEPTH.iter().enumerate() {
        let size = i64::from(depth - qlevel(component, max_qp));
        mpp_group += size * 4;
        zero_group += 1 + (size - 1) * 3;
    }
    let max_bits = mpp_group + zero_group;
    let budget =
        2 * (3 * bpp * 128 * full_groups - (slice_bpg + nfl) * full_groups - bpp * 128 * padding);
    max_bits * 2048 * full_groups > budget
}

impl Pps {
    /// Derive the picture parameter set the way the C model does: the
    /// recommended tables at 6, 8, 10 and 12 bpp, the model's generated
    /// (`rc_tables.h`) parameters in between, then `compute_rc_parameters`.
    pub fn from_config(config: &Config) -> Result<Self, DscError> {
        let bpp = i64::from(config.bits_per_pixel_x16);
        if !(i64::from(MIN_BPP_X16)..=i64::from(MAX_BPP_X16)).contains(&bpp) {
            return Err(DscError::Config("bits per pixel must be within 6..=12"));
        }
        if config.width == 0 || config.height == 0 || config.width > 65535 || config.height > 65535
        {
            return Err(DscError::Config(
                "picture dimensions must be within 1..=65535",
            ));
        }
        if config.slice_width < 7 || config.slice_width > config.width {
            return Err(DscError::Config(
                "slice width must be within 7..=picture width",
            ));
        }
        if config.slice_height == 0 || config.slice_height > 65535 {
            return Err(DscError::Config("slice height must be within 1..=65535"));
        }
        if config.version_minor != 1 && config.version_minor != 2 {
            return Err(DscError::Config("DSC minor version must be 1 or 2"));
        }
        let linebuf_max = if config.version_minor == 1 { 13 } else { 16 };
        if config.linebuf_depth < 8 || config.linebuf_depth > linebuf_max {
            return Err(DscError::Config("line buffer depth out of range"));
        }
        let slice_width = i64::from(config.slice_width);
        let slice_height = i64::from(config.slice_height);

        let mut ranges = [RcRange::default(); NUM_BUF_RANGES];
        let mut thresh = RC_BUF_THRESH;
        let mut flatness_max_qp = 12u8;
        let initial_offset;
        let mut xmit_delay;
        if let Some(preset) = RC_PRESETS.iter().position(|preset| preset.bpp_x16 == bpp) {
            let preset = &RC_PRESETS[preset];
            initial_offset = preset.initial_offset;
            xmit_delay = preset.initial_xmit_delay;
            flatness_max_qp = preset.flatness_max_qp;
            if bpp == 96 {
                thresh = RC_BUF_THRESH_6BPP;
            }
            for (index, range) in ranges.iter_mut().enumerate() {
                *range = RcRange {
                    min_qp: preset.min_qp[index],
                    max_qp: preset.max_qp[index],
                    bpg_offset: preset.offset[index],
                };
            }
        } else {
            initial_offset = if bpp >= 160 {
                5632 - (bpp - 160) * 112
            } else if bpp >= 128 {
                6144 - (bpp - 128) * 16
            } else {
                6144
            };
            xmit_delay = (2 * 65536 + bpp) / (2 * bpp);
            let padding = if slice_width % 3 == 0 {
                0
            } else {
                3 - slice_width % 3
            };
            let padding_pixels = padding * (xmit_delay / slice_width);
            if 3 * bpp >= 16 * ((xmit_delay + 2) / 3) * 3 && (xmit_delay + padding_pixels) % 3 == 1
            {
                xmit_delay += 1;
            }
            let column = usize::try_from((bpp - 96) / 8).unwrap_or(0).min(12);
            for (index, range) in ranges.iter_mut().enumerate() {
                let under6 = i64::from(OFFSET_UNDER_6BPP[index]);
                let under8 = i64::from(OFFSET_UNDER_8BPP[index]);
                let offset = if bpp <= 128 {
                    under6 + ((bpp - 96) * (under8 - under6) + 16) / 32
                } else {
                    under8
                };
                *range = RcRange {
                    min_qp: MIN_QP_444_8B[index][column],
                    max_qp: MAX_QP_444_8B[index][column],
                    bpg_offset: narrow(offset, "range offset")?,
                };
            }
        }

        // 3 * bpc + 2 bits per pixel uncompressed in YCoCg, three pixels per group.
        let uncompressed_bpg = 78i64;
        let mut first_line_bpg_offset = if slice_height >= 8 {
            12 + 9 * (slice_height - 8).min(34) / 100
        } else {
            2 * (slice_height - 1)
        };
        let first_line_max = (uncompressed_bpg * 16 - 3 * bpp).div_euclid(16);
        if first_line_bpg_offset > first_line_max {
            first_line_bpg_offset = first_line_max;
        }
        if !(0..=31).contains(&first_line_bpg_offset) {
            return Err(DscError::Config("first_line_bpg_offset out of range"));
        }
        let groups_per_line = (slice_width + 2) / 3;
        let chunk_size = ceil_div(slice_width * bpp, 128);
        let mut extra_mux_bits = 3 * (i64::from(MUX_WORD_BITS) + i64::from(MAX_SE_SIZE) - 2);
        let slice_bits = 8 * chunk_size * slice_height;
        while extra_mux_bits > 0 && (slice_bits - extra_mux_bits) % i64::from(MUX_WORD_BITS) != 0 {
            extra_mux_bits -= 1;
        }
        let mut initial_scale = 8 * RC_MODEL_SIZE / (RC_MODEL_SIZE - initial_offset);
        if groups_per_line < initial_scale - 8 {
            initial_scale = groups_per_line + 8;
        }
        if initial_scale > 63 {
            return Err(DscError::Config("initial_scale_value out of range"));
        }
        let scale_decrement_interval = if initial_scale > 8 {
            groups_per_line / (initial_scale - 8)
        } else {
            4095
        };
        if scale_decrement_interval > 4095 {
            return Err(DscError::Config("scale_decrement_interval out of range"));
        }
        let final_offset = RC_MODEL_SIZE - ((xmit_delay * bpp + 8) >> 4) + extra_mux_bits;
        if !(0..RC_MODEL_SIZE).contains(&final_offset) {
            return Err(DscError::Config("final_offset must be below rc_model_size"));
        }
        let final_scale = 8 * RC_MODEL_SIZE / (RC_MODEL_SIZE - final_offset);
        let nfl_bpg_offset = if slice_height > 1 {
            ceil_div(
                first_line_bpg_offset << OFFSET_FRACTIONAL_BITS,
                slice_height - 1,
            )
        } else {
            0
        };
        if nfl_bpg_offset > 65535 {
            return Err(DscError::Config(
                "nfl_bpg_offset is too large for this slice height",
            ));
        }
        let groups_total = groups_per_line * slice_height;
        let slice_bpg_offset = ceil_div(
            (RC_MODEL_SIZE - initial_offset + extra_mux_bits) << OFFSET_FRACTIONAL_BITS,
            groups_total,
        );
        if slice_bpg_offset > 65535 {
            return Err(DscError::Config(
                "slice_bpg_offset out of range (slice too small)",
            ));
        }
        if slice_height == 1 {
            if first_line_bpg_offset > 0 {
                return Err(DscError::Config(
                    "first_line_bpg_offset must be 0 for one-line slices",
                ));
            }
        } else if 3 * bpp * 128 - (slice_bpg_offset + nfl_bpg_offset) < 16 * 2048 {
            return Err(DscError::Config(
                "bits per pixel left for non-first lines is too low (slice too small)",
            ));
        }
        let scale_increment_interval = if final_scale > 9 {
            (final_offset << OFFSET_FRACTIONAL_BITS)
                / ((final_scale - 9) * (nfl_bpg_offset + slice_bpg_offset))
        } else {
            0
        };
        if scale_increment_interval > 65535 {
            return Err(DscError::Config(
                "scale_increment_interval is too large for this slice height",
            ));
        }
        let rbs_min = RC_MODEL_SIZE - initial_offset
            + ceil_div(xmit_delay * bpp, 16)
            + groups_per_line * first_line_bpg_offset;
        let hrd_delay = ceil_div(rbs_min * 16, bpp);
        let initial_dec_delay = hrd_delay - xmit_delay;
        if !(0..=65535).contains(&initial_dec_delay) {
            return Err(DscError::Config("initial_dec_delay out of range"));
        }
        // GENERATE_RC_PARAMETERS = 2 behaviour: raise the last range's maximum
        // QP until the worst case cannot overflow the rate buffer.
        while max_qp_may_overflow(
            i32::from(ranges[NUM_BUF_RANGES - 1].max_qp),
            slice_width,
            bpp,
            slice_bpg_offset,
            nfl_bpg_offset,
        ) {
            if i32::from(ranges[NUM_BUF_RANGES - 1].max_qp) >= MAX_QP {
                return Err(DscError::Config(
                    "slice too narrow for a safe rate-control setup",
                ));
            }
            ranges[NUM_BUF_RANGES - 1].max_qp += 1;
        }

        Ok(Self {
            dsc_version_minor: config.version_minor,
            pps_identifier: 0,
            bits_per_component: 8,
            linebuf_depth: config.linebuf_depth,
            block_pred_enable: config.block_pred_enable,
            convert_rgb: true,
            simple_422: false,
            vbr_enable: false,
            bits_per_pixel_x16: config.bits_per_pixel_x16,
            pic_height: narrow(i64::from(config.height), "pic_height")?,
            pic_width: narrow(i64::from(config.width), "pic_width")?,
            slice_height: narrow(slice_height, "slice_height")?,
            slice_width: narrow(slice_width, "slice_width")?,
            chunk_size: narrow(chunk_size, "chunk_size")?,
            initial_xmit_delay: narrow(xmit_delay, "initial_xmit_delay")?,
            initial_dec_delay: narrow(initial_dec_delay, "initial_dec_delay")?,
            initial_scale_value: narrow(initial_scale, "initial_scale_value")?,
            scale_increment_interval: narrow(scale_increment_interval, "scale_increment_interval")?,
            scale_decrement_interval: narrow(scale_decrement_interval, "scale_decrement_interval")?,
            first_line_bpg_offset: narrow(first_line_bpg_offset, "first_line_bpg_offset")?,
            nfl_bpg_offset: narrow(nfl_bpg_offset, "nfl_bpg_offset")?,
            slice_bpg_offset: narrow(slice_bpg_offset, "slice_bpg_offset")?,
            initial_offset: narrow(initial_offset, "initial_offset")?,
            final_offset: narrow(final_offset, "final_offset")?,
            flatness_min_qp: 3,
            flatness_max_qp,
            rc_model_size: narrow(RC_MODEL_SIZE, "rc_model_size")?,
            rc_edge_factor: 6,
            rc_quant_incr_limit0: 11,
            rc_quant_incr_limit1: 11,
            rc_tgt_offset_hi: 3,
            rc_tgt_offset_lo: 3,
            rc_buf_thresh: thresh,
            rc_range_parameters: ranges,
            native_420: false,
            native_422: false,
            second_line_bpg_offset: 0,
            nsl_bpg_offset: 0,
            second_line_offset_adj: 0,
        })
    }

    /// Number of slices in one slice row.
    pub fn slices_per_line(&self) -> usize {
        usize::from(self.pic_width).div_ceil(usize::from(self.slice_width).max(1))
    }

    /// Number of slice rows in the picture.
    pub fn slice_rows(&self) -> usize {
        usize::from(self.pic_height).div_ceil(usize::from(self.slice_height).max(1))
    }

    /// Exact length in bytes of every encoded picture with these parameters.
    pub fn frame_len(&self) -> usize {
        usize::from(self.chunk_size)
            * usize::from(self.slice_height)
            * self.slices_per_line()
            * self.slice_rows()
    }

    /// Serialize into the standard 128-byte PPS layout.
    pub fn to_bytes(&self) -> [u8; PPS_SIZE] {
        let mut writer = BitWriter::default();
        let flag = |value: bool| i32::from(value);
        writer.put(1, 4);
        writer.put(i32::from(self.dsc_version_minor), 4);
        writer.put(i32::from(self.pps_identifier), 8);
        writer.put(0, 8);
        writer.put(i32::from(self.bits_per_component & 15), 4);
        writer.put(i32::from(self.linebuf_depth & 15), 4);
        writer.put(0, 2);
        writer.put(flag(self.block_pred_enable), 1);
        writer.put(flag(self.convert_rgb), 1);
        writer.put(flag(self.simple_422), 1);
        writer.put(flag(self.vbr_enable), 1);
        writer.put(i32::from(self.bits_per_pixel_x16), 10);
        writer.put(i32::from(self.pic_height), 16);
        writer.put(i32::from(self.pic_width), 16);
        writer.put(i32::from(self.slice_height), 16);
        writer.put(i32::from(self.slice_width), 16);
        writer.put(i32::from(self.chunk_size), 16);
        writer.put(0, 6);
        writer.put(i32::from(self.initial_xmit_delay), 10);
        writer.put(i32::from(self.initial_dec_delay), 16);
        writer.put(0, 10);
        writer.put(i32::from(self.initial_scale_value), 6);
        writer.put(i32::from(self.scale_increment_interval), 16);
        writer.put(0, 4);
        writer.put(i32::from(self.scale_decrement_interval), 12);
        writer.put(0, 11);
        writer.put(i32::from(self.first_line_bpg_offset), 5);
        writer.put(i32::from(self.nfl_bpg_offset), 16);
        writer.put(i32::from(self.slice_bpg_offset), 16);
        writer.put(i32::from(self.initial_offset), 16);
        writer.put(i32::from(self.final_offset), 16);
        writer.put(0, 3);
        writer.put(i32::from(self.flatness_min_qp), 5);
        writer.put(0, 3);
        writer.put(i32::from(self.flatness_max_qp), 5);
        writer.put(i32::from(self.rc_model_size), 16);
        writer.put(0, 4);
        writer.put(i32::from(self.rc_edge_factor), 4);
        writer.put(0, 3);
        writer.put(i32::from(self.rc_quant_incr_limit0), 5);
        writer.put(0, 3);
        writer.put(i32::from(self.rc_quant_incr_limit1), 5);
        writer.put(i32::from(self.rc_tgt_offset_hi), 4);
        writer.put(i32::from(self.rc_tgt_offset_lo), 4);
        for thresh in self.rc_buf_thresh {
            writer.put(i32::from(thresh >> 6), 8);
        }
        for range in self.rc_range_parameters {
            writer.put(i32::from(range.min_qp), 5);
            writer.put(i32::from(range.max_qp), 5);
            writer.put(i32::from(range.bpg_offset), 6);
        }
        if self.dsc_version_minor == 2 {
            writer.put(0, 6);
            writer.put(flag(self.native_420), 1);
            writer.put(flag(self.native_422), 1);
            writer.put(0, 3);
            writer.put(i32::from(self.second_line_bpg_offset), 5);
            writer.put(i32::from(self.nsl_bpg_offset), 16);
            writer.put(i32::from(self.second_line_offset_adj), 16);
        }
        let mut bytes = [0u8; PPS_SIZE];
        for (target, source) in bytes.iter_mut().zip(writer.written().iter()) {
            *target = *source;
        }
        bytes
    }

    /// Parse the standard PPS layout (at least 94 bytes; 128 on the wire) and
    /// reject anything this codec does not implement.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DscError> {
        if bytes.len() < 94 {
            return Err(DscError::UnsupportedPps(
                "picture parameter set is shorter than 94 bytes",
            ));
        }
        let mut reader = PpsReader { bytes, position: 0 };
        if reader.get(4) != 1 {
            return Err(DscError::UnsupportedPps("dsc_version_major is not 1"));
        }
        let dsc_version_minor = reader.byte(4);
        let pps_identifier = reader.byte(8);
        reader.get(8);
        let bits_per_component = reader.byte(4);
        let linebuf_depth = match reader.byte(4) {
            0 => 16,
            depth => depth,
        };
        reader.get(2);
        let block_pred_enable = reader.get(1) == 1;
        let convert_rgb = reader.get(1) == 1;
        let simple_422 = reader.get(1) == 1;
        let vbr_enable = reader.get(1) == 1;
        let bits_per_pixel_x16 = reader.get(10);
        let pic_height = reader.get(16);
        let pic_width = reader.get(16);
        let slice_height = reader.get(16);
        let slice_width = reader.get(16);
        let chunk_size = reader.get(16);
        reader.get(6);
        let initial_xmit_delay = reader.get(10);
        let initial_dec_delay = reader.get(16);
        reader.get(10);
        let initial_scale_value = reader.byte(6);
        let scale_increment_interval = reader.get(16);
        reader.get(4);
        let scale_decrement_interval = reader.get(12);
        reader.get(11);
        let first_line_bpg_offset = reader.byte(5);
        let nfl_bpg_offset = reader.get(16);
        let slice_bpg_offset = reader.get(16);
        let initial_offset = reader.get(16);
        let final_offset = reader.get(16);
        reader.get(3);
        let flatness_min_qp = reader.byte(5);
        reader.get(3);
        let flatness_max_qp = reader.byte(5);
        let rc_model_size = reader.get(16);
        reader.get(4);
        let rc_edge_factor = reader.byte(4);
        reader.get(3);
        let rc_quant_incr_limit0 = reader.byte(5);
        reader.get(3);
        let rc_quant_incr_limit1 = reader.byte(5);
        let rc_tgt_offset_hi = reader.byte(4);
        let rc_tgt_offset_lo = reader.byte(4);
        let mut rc_buf_thresh = [0u16; 14];
        for thresh in &mut rc_buf_thresh {
            *thresh = reader.get(8) << 6;
        }
        let mut rc_range_parameters = [RcRange::default(); NUM_BUF_RANGES];
        for range in &mut rc_range_parameters {
            range.min_qp = reader.byte(5);
            range.max_qp = reader.byte(5);
            let raw = i8::try_from(reader.get(6)).unwrap_or(0);
            range.bpg_offset = if raw >= 32 { raw - 64 } else { raw };
        }
        let mut pps = Self {
            dsc_version_minor,
            pps_identifier,
            bits_per_component,
            linebuf_depth,
            block_pred_enable,
            convert_rgb,
            simple_422,
            vbr_enable,
            bits_per_pixel_x16,
            pic_height,
            pic_width,
            slice_height,
            slice_width,
            chunk_size,
            initial_xmit_delay,
            initial_dec_delay,
            initial_scale_value,
            scale_increment_interval,
            scale_decrement_interval,
            first_line_bpg_offset,
            nfl_bpg_offset,
            slice_bpg_offset,
            initial_offset,
            final_offset,
            flatness_min_qp,
            flatness_max_qp,
            rc_model_size,
            rc_edge_factor,
            rc_quant_incr_limit0,
            rc_quant_incr_limit1,
            rc_tgt_offset_hi,
            rc_tgt_offset_lo,
            rc_buf_thresh,
            rc_range_parameters,
            native_420: false,
            native_422: false,
            second_line_bpg_offset: 0,
            nsl_bpg_offset: 0,
            second_line_offset_adj: 0,
        };
        if dsc_version_minor == 2 {
            reader.get(6);
            pps.native_420 = reader.get(1) == 1;
            pps.native_422 = reader.get(1) == 1;
            reader.get(3);
            pps.second_line_bpg_offset = reader.byte(5);
            pps.nsl_bpg_offset = reader.get(16);
            pps.second_line_offset_adj = reader.get(16);
        }
        Params::from_pps(&pps)?;
        Ok(pps)
    }
}

struct PpsReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl PpsReader<'_> {
    fn get(&mut self, bits: usize) -> u16 {
        let mut value = 0u16;
        for _ in 0..bits {
            let byte = self.bytes.get(self.position / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - self.position % 8)) & 1;
            value = (value << 1) | u16::from(bit);
            self.position += 1;
        }
        value
    }

    fn byte(&mut self, bits: usize) -> u8 {
        u8::try_from(self.get(bits.min(8))).unwrap_or(0)
    }
}

/// MSB-first bit accumulator (one per substream, and for the PPS). Every
/// write stores the bits that are not yet part of a whole byte, left-aligned
/// and padded with zero bits, as eight bytes at the write position: no
/// branch depends on the data, and the written bytes are complete at any
/// time.
#[derive(Default)]
struct BitWriter {
    /// The bytes written so far, then scratch space.
    bytes: Vec<u8>,
    /// Whole bytes written.
    position: usize,
    accumulator: u64,
    /// Low bits of the accumulator after the last whole byte (fewer than 8
    /// between writes).
    pending: u32,
}

impl BitWriter {
    /// Start over, with room for `capacity` bytes before the buffer grows.
    fn clear(&mut self, capacity: usize) {
        if self.bytes.len() < capacity + 8 {
            self.bytes.resize(capacity + 8, 0);
        }
        self.position = 0;
        self.accumulator = 0;
        self.pending = 0;
    }

    /// Append the low `bits` bits of `value` (two's complement for negatives);
    /// a width outside 1..=32 writes nothing.
    #[inline(always)]
    fn put(&mut self, value: i32, bits: i32) {
        if let Ok(bits @ 1..=32) = u32::try_from(bits) {
            self.put_raw(u64::from(u32::from_le_bytes(value.to_le_bytes())), bits);
        }
    }

    /// Append the low `bits` (at most 56) bits of `raw`.
    #[inline(always)]
    fn put_raw(&mut self, raw: u64, bits: u32) {
        if bits == 0 || bits > 56 {
            return;
        }
        self.accumulator = (self.accumulator << bits) | (raw & ((1u64 << bits) - 1));
        // Fewer than 8 bits were pending, so at most 63 are now.
        self.pending += bits;
        let aligned = (self.accumulator << (64 - self.pending)).to_be_bytes();
        match self.bytes.get_mut(self.position..self.position + 8) {
            Some(target) => target.copy_from_slice(&aligned),
            None => self.grow(aligned),
        }
        self.position += usize::try_from(self.pending / 8).unwrap_or(0);
        self.pending %= 8;
    }

    #[cold]
    fn grow(&mut self, aligned: [u8; 8]) {
        self.bytes.resize(self.position * 2 + 64, 0);
        if let Some(target) = self.bytes.get_mut(self.position..self.position + 8) {
            target.copy_from_slice(&aligned);
        }
    }

    /// The bytes written so far, the last partial byte padded with zero bits.
    fn written(&self) -> &[u8] {
        let length = self.position + usize::from(self.pending > 0);
        self.bytes.get(..length).unwrap_or(&[])
    }
}

/// `value` limited to `low..=high` the way the C model does it: `high` wins
/// when the limits cross. The limits follow the picture data, so this is
/// written without branches.
#[inline(always)]
fn clamp_c(value: i32, low: i32, high: i32) -> i32 {
    let limited = value.max(low);
    let over = -i32::from(value > high);
    (high & over) | (limited & !over)
}

/// Number of significant bits (the C model's `ceil_log2`).
#[inline(always)]
fn bit_length(value: i32) -> i32 {
    i32::try_from(32 - value.leading_zeros()).unwrap_or(0)
}

/// Bits needed to code a quantized residual in two's complement.
#[inline(always)]
fn residual_size(value: i32) -> i32 {
    // `value ^ (value >> 31)` is `value` when positive, `!value` otherwise;
    // every value but zero needs a sign bit on top.
    bit_length(value ^ (value >> 31)) + i32::from(value != 0)
}

/// Divide by `1 << qlevel`, rounding half toward zero, keeping the sign.
#[inline(always)]
fn quantize(error: i32, qlevel: i32) -> i32 {
    let offset = ((1 << qlevel) >> 1) - i32::from(qlevel > 0);
    let sign = error >> 31;
    let magnitude = (error ^ sign) - sign;
    (((magnitude + offset) >> qlevel) ^ sign) - sign
}

#[inline(always)]
fn sample(value: i32) -> i16 {
    i16::try_from(value).unwrap_or(0)
}

#[inline(always)]
fn index(value: i32) -> usize {
    usize::try_from(value).unwrap_or(0)
}

#[inline(always)]
fn mid_value(component: usize) -> i32 {
    1 << (DEPTH[component] - 1)
}

/// The low `count` bits set.
#[inline(always)]
fn low_bits(count: usize) -> u32 {
    if count >= 32 {
        u32::MAX
    } else {
        (1u32 << count) - 1
    }
}

struct Zeros<const N: usize>;

impl<const N: usize> Zeros<N> {
    const SAMPLES: [i16; N] = [0; N];
}

/// The `N` samples of `line` from `at` on. A window outside the line reads as
/// zeros; the line buffers are sized so that this does not happen, and the
/// hot loops stay free of panics either way.
#[inline(always)]
fn window<const N: usize>(line: &[i16], at: usize) -> &[i16; N] {
    line.get(at..)
        .and_then(|rest| rest.first_chunk::<N>())
        .unwrap_or(&Zeros::<N>::SAMPLES)
}

/// One pixel of a line buffer: its Y, Co and Cg samples and a spare lane that
/// stays zero. Keeping the components of a pixel together lets one load fetch
/// all three for the lane arithmetic of [`Lanes`].
type Pixel = [i16; 4];

struct ZeroPixels<const N: usize>;

impl<const N: usize> ZeroPixels<N> {
    const PIXELS: [Pixel; N] = [[0; 4]; N];
}

/// The `N` pixels of `line` from `at` on; zeros outside the line, as
/// [`window`] does it.
#[inline(always)]
fn pixels<const N: usize>(line: &[Pixel], at: usize) -> &[Pixel; N] {
    line.get(at..)
        .and_then(|rest| rest.first_chunk::<N>())
        .unwrap_or(&ZeroPixels::<N>::PIXELS)
}

/// The indexed colour history, one row per component (Y, Co, Cg). Samples are
/// within 0..=511.
type IchTable = [[i16; ICH_SIZE]; 3];

/// Sixteen candidate pixels for a history search, one row per component:
/// half of the history, or the previous line around the group (whose first
/// seven samples are history entries 25..=31 from the second line on).
type IchRows<'a> = [&'a [i16; ICH_ROW]; 3];
const ICH_ROW: usize = ICH_SIZE / 2;

/// Search key of a history entry: the weighted difference above the entry
/// number, so that the lowest key is the first entry with the lowest
/// difference. No entry has the key `ICH_NO_KEY`.
const ICH_KEY_SHIFT: u32 = 5;
const ICH_NO_KEY: u32 = 0xFFFF;

/// The two halves of the history as search rows.
#[inline(always)]
fn ich_halves(ich: &IchTable) -> [IchRows<'_>; 2] {
    [
        [window(&ich[0], 0), window(&ich[1], 0), window(&ich[2], 0)],
        [
            window(&ich[0], ICH_ROW),
            window(&ich[1], ICH_ROW),
            window(&ich[2], ICH_ROW),
        ],
    ]
}

/// The first history entry whose bit in `valid` is set and that is within
/// `tolerance` of `pixel`. The second half is only searched when the first
/// has no such entry.
#[inline(always)]
fn ich_table_first<K: Kernels>(
    kernels: K,
    ich: &IchTable,
    pixel: [i16; 3],
    tolerance: [i16; 3],
    valid: u32,
) -> Option<usize> {
    let [low, high] = ich_halves(ich);
    kernels.ich_first(low, pixel, tolerance, valid).or_else(|| {
        kernels
            .ich_first(high, pixel, tolerance, valid >> ICH_ROW)
            .map(|entry| entry + ICH_ROW)
    })
}

/// Weighted absolute difference the history search minimizes.
#[inline(always)]
fn ich_weighted_sad(entry: [i32; 3], pixel: [i16; 3]) -> i32 {
    2 * (entry[0] - i32::from(pixel[0])).abs()
        + (entry[1] - i32::from(pixel[1])).abs()
        + (entry[2] - i32::from(pixel[2])).abs()
}

/// Insert `pixel` at the front of the history, moving the entries before
/// `location` back by one (the entry at `location` is replaced).
#[inline(always)]
fn ich_insert(ich: &mut IchTable, location: usize, pixel: [i16; 3]) {
    for (history, value) in ich.iter_mut().zip(pixel) {
        if location == 0 {
            history[0] = value;
            continue;
        }
        let old = *history;
        let mut moved = [value; ICH_SIZE];
        moved[1..].copy_from_slice(&old[..ICH_SIZE - 1]);
        for (slot, ((target, moved), old)) in history
            .iter_mut()
            .zip(moved.iter())
            .zip(old.iter())
            .enumerate()
        {
            // A mask instead of a choice, so that this is vector code.
            let take = -i16::from(slot <= location);
            *target = (*moved & take) | (*old & !take);
        }
    }
}

/// Insert three pixels one after the other without looking for duplicates:
/// the first `valid` entries (at most `reserved - 3`) move back by three.
#[inline(always)]
fn ich_insert_group(ich: &mut IchTable, valid: usize, reserved: usize, pixels: &[Pixel; 3]) {
    let filled = (valid + 3).min(reserved);
    for (component, history) in ich.iter_mut().enumerate() {
        let old = *history;
        let mut moved = [0i16; ICH_SIZE];
        moved[0] = pixels[2][component];
        moved[1] = pixels[1][component];
        moved[2] = pixels[0][component];
        moved[3..].copy_from_slice(&old[..ICH_SIZE - 3]);
        for (slot, ((target, moved), old)) in history
            .iter_mut()
            .zip(moved.iter())
            .zip(old.iter())
            .enumerate()
        {
            let take = -i16::from(slot < filled);
            *target = (*moved & take) | (*old & !take);
        }
    }
}

#[inline(always)]
fn rgb_to_ycocg(red: u8, green: u8, blue: u8) -> Pixel {
    let (r, g, b) = (i16::from(red), i16::from(green), i16::from(blue));
    let co = r - b;
    let t = b + (co >> 1);
    let cg = g - t;
    let y = t + (cg >> 1);
    [y, co + 256, cg + 256, 0]
}

#[inline(always)]
fn ycocg_to_rgb(pixel: Pixel) -> [u8; 3] {
    let [y, co, cg, _] = pixel.map(i32::from);
    let (co, cg) = (co - 256, cg - 256);
    let t = y - (cg >> 1);
    let g = cg + t;
    let b = t - (co >> 1);
    let r = co + b;
    let byte = |value: i32| value.clamp(0, 255).to_le_bytes()[0];
    [byte(r), byte(g), byte(b)]
}

/// Four 16-bit lanes: the Y, Co and Cg samples of one pixel, or one value per
/// component (lane 3 is spare). The three components of a group are coded
/// with the same steps, so they are computed side by side. All arithmetic
/// wraps: every implementation gives the same lanes for every input.
trait Lanes: Copy {
    fn new(lanes: [i16; 4]) -> Self;
    fn splat(value: i16) -> Self;
    fn get(self) -> [i16; 4];
    fn add(self, other: Self) -> Self;
    fn sub(self, other: Self) -> Self;
    /// The low 16 bits of the products.
    fn mul(self, other: Self) -> Self;
    fn min(self, other: Self) -> Self;
    fn max(self, other: Self) -> Self;
    fn xor(self, other: Self) -> Self;
    /// Arithmetic shift right of every lane by `count` (at most 15).
    fn shr(self, count: u32) -> Self;
    /// Lane 0 (luma) of `self` with lanes 1..=3 of `chroma`.
    fn luma_or(self, chroma: Self) -> Self;

    /// `self` limited to `low..=high`; `low` must not exceed `high`.
    #[inline(always)]
    fn clamp(self, low: Self, high: Self) -> Self {
        self.min(high).max(low)
    }

    #[inline(always)]
    fn abs(self) -> Self {
        self.max(Self::splat(0).sub(self))
    }
}

/// [`Lanes`] in portable code, the reference for the other implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PortableLanes([i16; 4]);

impl PortableLanes {
    #[inline(always)]
    fn zip(self, other: Self, operation: impl Fn(i16, i16) -> i16) -> Self {
        Self(std::array::from_fn(|lane| {
            operation(self.0[lane], other.0[lane])
        }))
    }
}

impl Lanes for PortableLanes {
    #[inline(always)]
    fn new(lanes: [i16; 4]) -> Self {
        Self(lanes)
    }

    #[inline(always)]
    fn splat(value: i16) -> Self {
        Self([value; 4])
    }

    #[inline(always)]
    fn get(self) -> [i16; 4] {
        self.0
    }

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        self.zip(other, i16::wrapping_add)
    }

    #[inline(always)]
    fn sub(self, other: Self) -> Self {
        self.zip(other, i16::wrapping_sub)
    }

    #[inline(always)]
    fn mul(self, other: Self) -> Self {
        self.zip(other, i16::wrapping_mul)
    }

    #[inline(always)]
    fn min(self, other: Self) -> Self {
        self.zip(other, Ord::min)
    }

    #[inline(always)]
    fn max(self, other: Self) -> Self {
        self.zip(other, Ord::max)
    }

    #[inline(always)]
    fn xor(self, other: Self) -> Self {
        self.zip(other, |a, b| a ^ b)
    }

    #[inline(always)]
    fn shr(self, count: u32) -> Self {
        Self(self.0.map(|lane| lane >> count.min(15)))
    }

    #[inline(always)]
    fn luma_or(self, chroma: Self) -> Self {
        Self([self.0[0], chroma.0[1], chroma.0[2], chroma.0[3]])
    }
}

/// Per-component constants of one quantization parameter, as lanes.
#[derive(Clone, Copy)]
struct QuantLanes<V> {
    /// Quantization shift of luma and of both chroma components.
    shift: [u32; 2],
    /// `1 << shift`.
    scale: V,
    /// Rounding offset of the quantizer.
    offset: V,
    /// Most that MAP prediction lets the filter change a previous-line sample.
    half: V,
    neg_half: V,
}

impl<V: Lanes> QuantLanes<V> {
    fn new(qp: i32) -> Self {
        let levels = [qlevel(0, qp), qlevel(1, qp), qlevel(2, qp), qlevel(2, qp)];
        let half = levels.map(|level| sample((1 << level) / 2));
        Self {
            shift: [
                u32::try_from(levels[0]).unwrap_or(0),
                u32::try_from(levels[1]).unwrap_or(0),
            ],
            scale: V::new(levels.map(|level| sample(1 << level))),
            offset: V::new(levels.map(|level| sample(((1 << level) >> 1) - i32::from(level > 0)))),
            half: V::new(half),
            neg_half: V::new(half.map(|value| -value)),
        }
    }

    /// The constants of every quantization parameter, indexed by it.
    fn table() -> [Self; 16] {
        std::array::from_fn(|qp| Self::new(i32::try_from(qp).unwrap_or(0)))
    }

    /// [`quantize`] of every lane with its component's shift.
    #[inline(always)]
    fn quantize(&self, error: V) -> V {
        let sign = error.shr(15);
        let rounded = error.xor(sign).sub(sign).add(self.offset);
        let shifted = rounded
            .shr(self.shift[0])
            .luma_or(rounded.shr(self.shift[1]));
        shifted.xor(sign).sub(sign)
    }
}

/// Largest sample value per component, as lanes (the spare lane stays zero).
#[inline(always)]
fn max_values<V: Lanes>() -> V {
    V::new([
        sample((1 << DEPTH[0]) - 1),
        sample((1 << DEPTH[1]) - 1),
        sample((1 << DEPTH[2]) - 1),
        0,
    ])
}

/// Everything needed to predict the three pixels of a group, one lane per
/// component.
#[derive(Clone, Copy)]
enum Pred<V> {
    /// The left pixel and the blended previous-line pixels c, b, d, e.
    Map {
        a: V,
        c: V,
        b: V,
        d: V,
        e: V,
    },
    Left {
        a: V,
    },
    Block([V; 3]),
}

/// Predict and reconstruct the three pixels of a group, all components at
/// once. `residual` gives the quantized residuals of a position from its
/// prediction (the encoder quantizes, the decoder returns what it read);
/// later predictions depend on earlier residuals. Returns the residuals and
/// the reconstructed pixels.
#[inline(always)]
fn code_units<V: Lanes>(
    pred: Pred<V>,
    scale: V,
    mut residual: impl FnMut(usize, V) -> V,
) -> ([V; 3], [V; 3]) {
    let zero = V::splat(0);
    let max_value = max_values::<V>();
    let rebuild =
        |prediction: V, quantized: V| prediction.add(quantized.mul(scale)).clamp(zero, max_value);
    let predictions;
    let residuals;
    match pred {
        Pred::Map { a, c, b, d, e } => {
            let slope = a.sub(c);
            let (low, high) = (a.min(b), a.max(b));
            let p0 = slope.add(b).clamp(low, high);
            let q0 = residual(0, p0);
            let (low, high) = (low.min(d), high.max(d));
            let p1 = slope.add(d).add(q0.mul(scale)).clamp(low, high);
            let q1 = residual(1, p1);
            let (low, high) = (low.min(e), high.max(e));
            let p2 = slope.add(e).add(q0.add(q1).mul(scale)).clamp(low, high);
            let q2 = residual(2, p2);
            predictions = [p0, p1, p2];
            residuals = [q0, q1, q2];
        }
        Pred::Left { a } => {
            let q0 = residual(0, a);
            let p1 = a.add(q0.mul(scale)).clamp(zero, max_value);
            let q1 = residual(1, p1);
            let p2 = a.add(q0.add(q1).mul(scale)).clamp(zero, max_value);
            let q2 = residual(2, p2);
            predictions = [a, p1, p2];
            residuals = [q0, q1, q2];
        }
        Pred::Block(block) => {
            predictions = block;
            residuals = [
                residual(0, block[0]),
                residual(1, block[1]),
                residual(2, block[2]),
            ];
        }
    }
    let rebuilt = [
        rebuild(predictions[0], residuals[0]),
        rebuild(predictions[1], residuals[1]),
        rebuild(predictions[2], residuals[2]),
    ];
    (residuals, rebuilt)
}

/// The data-parallel inner loops of the codec. Every implementation gives the
/// same results as [`Scalar`]; the slice coders are generic over this trait,
/// so each implementation gets its own copy of the whole slice loop.
trait Kernels: Copy {
    /// Lane arithmetic for the three components of a pixel.
    type Lanes: Lanes;

    /// The first candidate `n` (of 16) of `rows` whose bit in `valid` is set
    /// and that is within `tolerance` of `pixel` in every component.
    fn ich_first(
        self,
        rows: IchRows<'_>,
        pixel: [i16; 3],
        tolerance: [i16; 3],
        valid: u32,
    ) -> Option<usize>;

    /// The lowest search key `sad << ICH_KEY_SHIFT | (first + n)` among the
    /// candidates `n` of `rows` whose bit in `valid` is set, where `sad` is
    /// the weighted absolute difference from `pixel`; `ICH_NO_KEY` when
    /// `valid` has none of the low 16 bits set. `first + 15` is at most 31
    /// and samples are within 0..=511, so keys fit 16 bits.
    fn ich_best(self, rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32;

    /// Convert leading 4-byte pixels of `row` to Y, Co and Cg samples and
    /// return how many pixels were converted; the caller converts the rest.
    fn to_ycocg(self, row: &[u8], red_first: bool, line: &mut [Pixel]) -> usize;

    /// Convert leading Y, Co and Cg samples to 4-byte pixels in `row` and
    /// return how many pixels were written; the caller converts the rest.
    fn to_rgb(self, line: &[Pixel], red_first: bool, row: &mut [u8]) -> usize;
}

/// Portable implementation of [`Kernels`], the reference for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scalar;

impl Kernels for Scalar {
    type Lanes = PortableLanes;

    #[inline(always)]
    fn ich_first(
        self,
        rows: IchRows<'_>,
        pixel: [i16; 3],
        tolerance: [i16; 3],
        valid: u32,
    ) -> Option<usize> {
        // Only the valid candidates are visited, lowest first.
        let mut remaining = valid & low_bits(ICH_ROW);
        while remaining != 0 {
            let entry = usize::try_from(remaining.trailing_zeros()).unwrap_or(0) % ICH_ROW;
            if (rows[0][entry] - pixel[0]).abs() <= tolerance[0]
                && (rows[1][entry] - pixel[1]).abs() <= tolerance[1]
                && (rows[2][entry] - pixel[2]).abs() <= tolerance[2]
            {
                return Some(entry);
            }
            remaining &= remaining - 1;
        }
        None
    }

    #[inline(always)]
    fn ich_best(self, rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32 {
        let mut lowest = ICH_NO_KEY;
        let mut remaining = valid & low_bits(ICH_ROW);
        while remaining != 0 {
            let entry = usize::try_from(remaining.trailing_zeros()).unwrap_or(0) % ICH_ROW;
            let candidate = [rows[0][entry], rows[1][entry], rows[2][entry]].map(i32::from);
            let sad = u32::try_from(ich_weighted_sad(candidate, pixel)).unwrap_or(0);
            let number = u32::try_from(first + entry).unwrap_or(0);
            lowest = lowest.min((sad << ICH_KEY_SHIFT) | number);
            remaining &= remaining - 1;
        }
        lowest
    }

    #[inline(always)]
    fn to_ycocg(self, _row: &[u8], _red_first: bool, _line: &mut [Pixel]) -> usize {
        0
    }

    #[inline(always)]
    fn to_rgb(self, _line: &[Pixel], _red_first: bool, _row: &mut [u8]) -> usize {
        0
    }
}

/// Convert one row of 4-byte pixels to Y, Co and Cg samples; the shorter of
/// the two slices sets the pixel count.
#[inline(always)]
fn row_to_ycocg<K: Kernels>(kernels: K, row: &[u8], red_first: bool, line: &mut [Pixel]) {
    let done = kernels.to_ycocg(row, red_first, line);
    let (red, blue) = if red_first { (0, 2) } else { (2, 0) };
    let sources = row.chunks_exact(4).skip(done);
    for (target, source) in line.iter_mut().skip(done).zip(sources) {
        *target = rgb_to_ycocg(source[red], source[1], source[blue]);
    }
}

/// Convert Y, Co and Cg samples to one row of 4-byte pixels (padding byte
/// 255); the shorter of the two slices sets the pixel count.
#[inline(always)]
fn row_to_rgb<K: Kernels>(kernels: K, line: &[Pixel], red_first: bool, row: &mut [u8]) {
    let done = kernels.to_rgb(line, red_first, row);
    let (red, blue) = if red_first { (0, 2) } else { (2, 0) };
    let targets = row.chunks_exact_mut(4).skip(done);
    for (target, source) in targets.zip(line.iter().skip(done)) {
        let [r, g, b] = ycocg_to_rgb(*source);
        target[red] = r;
        target[1] = g;
        target[blue] = b;
        target[3] = 255;
    }
}

/// SSE2 and AVX2 implementations of [`Kernels`].
#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{IchRows, Kernels, Lanes, Pixel};
    use std::arch::x86_64::{
        __m128i, __m256i, _mm_add_epi16, _mm_and_si128, _mm_andnot_si128, _mm_cmpeq_epi16,
        _mm_cmpgt_epi16, _mm_cvtsi32_si128, _mm_extract_epi16, _mm_loadl_epi64, _mm_loadu_si128,
        _mm_max_epi16, _mm_min_epi16, _mm_min_epu16, _mm_minpos_epu16, _mm_movemask_epi8,
        _mm_mullo_epi16, _mm_or_si128, _mm_packs_epi16, _mm_packs_epi32, _mm_packus_epi16,
        _mm_set1_epi16, _mm_set1_epi32, _mm_setr_epi16, _mm_setzero_si128, _mm_slli_epi16,
        _mm_sra_epi16, _mm_srai_epi16, _mm_srli_epi32, _mm_srli_si128, _mm_storel_epi64,
        _mm_storeu_si128, _mm_sub_epi16, _mm_unpackhi_epi16, _mm_unpackhi_epi32,
        _mm_unpackhi_epi64, _mm_unpacklo_epi8, _mm_unpacklo_epi16, _mm_unpacklo_epi32,
        _mm_unpacklo_epi64, _mm_xor_si128, _mm256_abs_epi16, _mm256_add_epi16, _mm256_and_si256,
        _mm256_andnot_si256, _mm256_castsi256_si128, _mm256_cmpeq_epi16, _mm256_cmpgt_epi16,
        _mm256_extracti128_si256, _mm256_loadu_si256, _mm256_or_si256, _mm256_set1_epi16,
        _mm256_setr_epi16, _mm256_setzero_si256, _mm256_slli_epi16, _mm256_sub_epi16,
    };

    /// SSE2 kernels. SSE2 is part of the x86-64 baseline; the assertion below
    /// rejects a build that turns it off.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Sse2;

    const _: () = assert!(cfg!(target_feature = "sse2"));

    /// The SSE2 kernels inside slice loops compiled for SSE4.2-era CPUs. A
    /// value exists only after [`Sse42::detect`] saw the features on the
    /// running CPU, so holding one proves they are present.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Sse42(());

    impl Sse42 {
        /// The features the SSE4.2 copies of the slice loops are compiled for.
        pub(super) fn detect() -> Option<Self> {
            let present = std::arch::is_x86_feature_detected!("sse4.2")
                && std::arch::is_x86_feature_detected!("ssse3")
                && std::arch::is_x86_feature_detected!("popcnt");
            present.then_some(Self(()))
        }
    }

    /// AVX2 kernels. A value exists only after [`Avx2::detect`] saw the
    /// features on the running CPU, so holding one proves they are present.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Avx2(());

    impl Avx2 {
        /// The features the AVX2 copies of the slice loops are compiled for.
        pub(super) fn detect() -> Option<Self> {
            let present = std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("bmi1")
                && std::arch::is_x86_feature_detected!("bmi2")
                && std::arch::is_x86_feature_detected!("lzcnt")
                && std::arch::is_x86_feature_detected!("popcnt");
            present.then_some(Self(()))
        }
    }

    /// [`Lanes`] in the low half of an SSE2 register.
    #[derive(Clone, Copy)]
    pub(super) struct WideLanes(__m128i);

    // SAFETY (every block in this impl): the functions called need SSE2,
    // which is enabled for the whole build (asserted above); the loads and
    // stores go through references to exactly the eight bytes they touch and
    // have no alignment requirement.
    impl Lanes for WideLanes {
        #[inline(always)]
        fn new(lanes: [i16; 4]) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_loadl_epi64(std::ptr::from_ref(&lanes).cast()) })
        }

        #[inline(always)]
        fn splat(value: i16) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_set1_epi16(value) })
        }

        #[inline(always)]
        fn get(self) -> [i16; 4] {
            let mut lanes = [0i16; 4];
            // SAFETY: see the impl.
            unsafe { _mm_storel_epi64(std::ptr::from_mut(&mut lanes).cast(), self.0) };
            lanes
        }

        #[inline(always)]
        fn add(self, other: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_add_epi16(self.0, other.0) })
        }

        #[inline(always)]
        fn sub(self, other: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_sub_epi16(self.0, other.0) })
        }

        #[inline(always)]
        fn mul(self, other: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_mullo_epi16(self.0, other.0) })
        }

        #[inline(always)]
        fn min(self, other: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_min_epi16(self.0, other.0) })
        }

        #[inline(always)]
        fn max(self, other: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_max_epi16(self.0, other.0) })
        }

        #[inline(always)]
        fn xor(self, other: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe { _mm_xor_si128(self.0, other.0) })
        }

        #[inline(always)]
        fn shr(self, count: u32) -> Self {
            let count = i32::try_from(count.min(15)).unwrap_or(15);
            // SAFETY: see the impl.
            Self(unsafe { _mm_sra_epi16(self.0, _mm_cvtsi32_si128(count)) })
        }

        #[inline(always)]
        fn luma_or(self, chroma: Self) -> Self {
            // SAFETY: see the impl.
            Self(unsafe {
                let luma = _mm_setr_epi16(-1, 0, 0, 0, 0, 0, 0, 0);
                _mm_or_si128(
                    _mm_and_si128(luma, self.0),
                    _mm_andnot_si128(luma, chroma.0),
                )
            })
        }
    }

    /// The lowest set bit of a 16-candidate hit mask.
    fn first_hit(hits: u32) -> Option<usize> {
        (hits != 0).then(|| usize::try_from(hits.trailing_zeros()).unwrap_or(0))
    }

    fn mask_bits(mask: i32) -> u32 {
        u32::from_le_bytes(mask.to_le_bytes())
    }

    /// Bits `shift..shift + 16` of `valid` as a 16-bit lane.
    fn valid_lanes(valid: u32, shift: usize) -> i16 {
        let [low, high, _, _] = (valid >> shift).to_le_bytes();
        i16::from_le_bytes([low, high])
    }

    fn lane(value: usize) -> i16 {
        i16::try_from(value).unwrap_or(0)
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn load(values: &[i16; 8]) -> __m128i {
        // SAFETY: `values` is a valid reference to 16 bytes and the load has
        // no alignment requirement.
        unsafe { _mm_loadu_si128(std::ptr::from_ref(values).cast()) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn load_pixels(values: &[Pixel; 2]) -> __m128i {
        // SAFETY: `values` is a valid reference to 16 bytes and the load has
        // no alignment requirement.
        unsafe { _mm_loadu_si128(std::ptr::from_ref(values).cast()) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn store_pixels(target: &mut [Pixel; 2], value: __m128i) {
        // SAFETY: `target` is a valid, exclusive reference to 16 bytes and
        // the store has no alignment requirement.
        unsafe { _mm_storeu_si128(std::ptr::from_mut(target).cast(), value) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn load_bytes(values: &[u8; 16]) -> __m128i {
        // SAFETY: `values` is a valid reference to 16 bytes and the load has
        // no alignment requirement.
        unsafe { _mm_loadu_si128(std::ptr::from_ref(values).cast()) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn store_bytes(target: &mut [u8; 16], value: __m128i) {
        // SAFETY: `target` is a valid, exclusive reference to 16 bytes and
        // the store has no alignment requirement.
        unsafe { _mm_storeu_si128(std::ptr::from_mut(target).cast(), value) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn magnitude(value: __m128i) -> __m128i {
        _mm_max_epi16(value, _mm_sub_epi16(_mm_setzero_si128(), value))
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn ich_hits_sse2(rows: IchRows<'_>, pixel: [i16; 3], tolerance: [i16; 3]) -> u32 {
        let mut over = [_mm_setzero_si128(); 2];
        for component in 0..3 {
            let target = _mm_set1_epi16(pixel[component]);
            let limit = _mm_set1_epi16(tolerance[component]);
            let (blocks, _) = rows[component].as_chunks::<8>();
            for (over, block) in over.iter_mut().zip(blocks) {
                let distance = magnitude(_mm_sub_epi16(load(block), target));
                *over = _mm_or_si128(*over, _mm_cmpgt_epi16(distance, limit));
            }
        }
        !mask_bits(_mm_movemask_epi8(_mm_packs_epi16(over[0], over[1]))) & 0xFFFF
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn ich_best_sse2(rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32 {
        // Each lane holds the key of its candidate, 0xFFFF when the candidate
        // is not valid. SSE2 only has a signed 16-bit minimum, hence the
        // sign-bit flip around it.
        let flip = _mm_set1_epi16(i16::MIN);
        let ones = _mm_set1_epi16(-1);
        let selectors = _mm_setr_epi16(1, 2, 4, 8, 16, 32, 64, 128);
        let numbers = _mm_setr_epi16(0, 1, 2, 3, 4, 5, 6, 7);
        let mut lowest = _mm_set1_epi16(i16::MAX);
        for block in 0..2 {
            let mut sad = _mm_setzero_si128();
            for component in 0..3 {
                let (blocks, _) = rows[component].as_chunks::<8>();
                let target = _mm_set1_epi16(pixel[component]);
                let distance = magnitude(_mm_sub_epi16(load(&blocks[block]), target));
                let weighted = if component == 0 {
                    _mm_slli_epi16::<1>(distance)
                } else {
                    distance
                };
                sad = _mm_add_epi16(sad, weighted);
            }
            let number = _mm_add_epi16(numbers, _mm_set1_epi16(lane(first + block * 8)));
            let key = _mm_or_si128(_mm_slli_epi16::<5>(sad), number);
            let lanes = _mm_set1_epi16(valid_lanes(valid, block * 8));
            let usable = _mm_cmpeq_epi16(_mm_and_si128(lanes, selectors), selectors);
            let key = _mm_or_si128(key, _mm_andnot_si128(usable, ones));
            lowest = _mm_min_epi16(lowest, _mm_xor_si128(key, flip));
        }
        let lowest = _mm_min_epi16(lowest, _mm_srli_si128::<8>(lowest));
        let lowest = _mm_min_epi16(lowest, _mm_srli_si128::<4>(lowest));
        let lowest = _mm_min_epi16(lowest, _mm_srli_si128::<2>(lowest));
        mask_bits(_mm_extract_epi16::<0>(_mm_xor_si128(lowest, flip))) & 0xFFFF
    }

    /// Eight pixels at a time.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn to_ycocg_sse2(row: &[u8], red_first: bool, line: &mut [Pixel]) -> usize {
        let (sources, _) = row.as_chunks::<32>();
        let (targets, _) = line.as_chunks_mut::<8>();
        let byte = _mm_set1_epi32(0xFF);
        let bias = _mm_set1_epi16(256);
        let zero = _mm_setzero_si128();
        let mut done = 0;
        for (source, target) in sources.iter().zip(targets.iter_mut()) {
            let (halves, _) = source.as_chunks::<16>();
            let low = load_bytes(&halves[0]);
            let high = load_bytes(&halves[1]);
            let first = _mm_packs_epi32(_mm_and_si128(low, byte), _mm_and_si128(high, byte));
            let g = _mm_packs_epi32(
                _mm_and_si128(_mm_srli_epi32::<8>(low), byte),
                _mm_and_si128(_mm_srli_epi32::<8>(high), byte),
            );
            let third = _mm_packs_epi32(
                _mm_and_si128(_mm_srli_epi32::<16>(low), byte),
                _mm_and_si128(_mm_srli_epi32::<16>(high), byte),
            );
            let (r, b) = if red_first {
                (first, third)
            } else {
                (third, first)
            };
            let co = _mm_sub_epi16(r, b);
            let t = _mm_add_epi16(b, _mm_srai_epi16::<1>(co));
            let cg = _mm_sub_epi16(g, t);
            let y = _mm_add_epi16(t, _mm_srai_epi16::<1>(cg));
            let co = _mm_add_epi16(co, bias);
            let cg = _mm_add_epi16(cg, bias);
            // Interleave the three planes (and zeros) into pixels.
            let front_low = _mm_unpacklo_epi16(y, co);
            let front_high = _mm_unpackhi_epi16(y, co);
            let back_low = _mm_unpacklo_epi16(cg, zero);
            let back_high = _mm_unpackhi_epi16(cg, zero);
            let (pairs, _) = target.as_chunks_mut::<2>();
            if let [first, second, third, fourth] = pairs {
                store_pixels(first, _mm_unpacklo_epi32(front_low, back_low));
                store_pixels(second, _mm_unpackhi_epi32(front_low, back_low));
                store_pixels(third, _mm_unpacklo_epi32(front_high, back_high));
                store_pixels(fourth, _mm_unpackhi_epi32(front_high, back_high));
            }
            done += 8;
        }
        done
    }

    /// The Y, Co and Cg planes of four pixels: Y in the low half of the
    /// first register, Co in its high half, Cg in the low half of the second.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn planes_of_four(first: &[Pixel; 2], second: &[Pixel; 2]) -> (__m128i, __m128i) {
        let first = load_pixels(first);
        let second = load_pixels(second);
        let even = _mm_unpacklo_epi16(first, second);
        let odd = _mm_unpackhi_epi16(first, second);
        (_mm_unpacklo_epi16(even, odd), _mm_unpackhi_epi16(even, odd))
    }

    /// Eight pixels at a time.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn to_rgb_sse2(line: &[Pixel], red_first: bool, row: &mut [u8]) -> usize {
        let (sources, _) = line.as_chunks::<8>();
        let (targets, _) = row.as_chunks_mut::<32>();
        let bias = _mm_set1_epi16(256);
        let opaque = _mm_set1_epi16(-1);
        let mut done = 0;
        for (source, target) in sources.iter().zip(targets.iter_mut()) {
            let (pairs, _) = source.as_chunks::<2>();
            let [first, second, third, fourth] = pairs else {
                break;
            };
            let (front_low, back_low) = planes_of_four(first, second);
            let (front_high, back_high) = planes_of_four(third, fourth);
            let y = _mm_unpacklo_epi64(front_low, front_high);
            let co = _mm_sub_epi16(_mm_unpackhi_epi64(front_low, front_high), bias);
            let cg = _mm_sub_epi16(_mm_unpacklo_epi64(back_low, back_high), bias);
            let t = _mm_sub_epi16(y, _mm_srai_epi16::<1>(cg));
            let g = _mm_add_epi16(cg, t);
            let b = _mm_sub_epi16(t, _mm_srai_epi16::<1>(co));
            let r = _mm_add_epi16(co, b);
            let (first, third) = if red_first { (r, b) } else { (b, r) };
            // Saturating packs clamp to 0..=255.
            let first = _mm_packus_epi16(first, first);
            let g = _mm_packus_epi16(g, g);
            let third = _mm_packus_epi16(third, third);
            let front = _mm_unpacklo_epi8(first, g);
            let back = _mm_unpacklo_epi8(third, opaque);
            let (halves, _) = target.as_chunks_mut::<16>();
            if let [low, high] = halves {
                store_bytes(low, _mm_unpacklo_epi16(front, back));
                store_bytes(high, _mm_unpackhi_epi16(front, back));
            }
            done += 8;
        }
        done
    }

    impl Kernels for Sse2 {
        type Lanes = WideLanes;

        #[inline(always)]
        fn ich_first(
            self,
            rows: IchRows<'_>,
            pixel: [i16; 3],
            tolerance: [i16; 3],
            valid: u32,
        ) -> Option<usize> {
            // SAFETY: SSE2 is enabled for the whole build (asserted above).
            first_hit(unsafe { ich_hits_sse2(rows, pixel, tolerance) } & valid)
        }

        #[inline(always)]
        fn ich_best(self, rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32 {
            // SAFETY: SSE2 is enabled for the whole build (asserted above).
            unsafe { ich_best_sse2(rows, pixel, valid, first) }
        }

        #[inline(always)]
        fn to_ycocg(self, row: &[u8], red_first: bool, line: &mut [Pixel]) -> usize {
            // SAFETY: SSE2 is enabled for the whole build (asserted above).
            unsafe { to_ycocg_sse2(row, red_first, line) }
        }

        #[inline(always)]
        fn to_rgb(self, line: &[Pixel], red_first: bool, row: &mut [u8]) -> usize {
            // SAFETY: SSE2 is enabled for the whole build (asserted above).
            unsafe { to_rgb_sse2(line, red_first, row) }
        }
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    fn load_wide(values: &[i16; 16]) -> __m256i {
        // SAFETY: `values` is a valid reference to 32 bytes and the load has
        // no alignment requirement.
        unsafe { _mm256_loadu_si256(std::ptr::from_ref(values).cast()) }
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    fn ich_hits_avx2(rows: IchRows<'_>, pixel: [i16; 3], tolerance: [i16; 3]) -> u32 {
        let mut over = _mm256_setzero_si256();
        for component in 0..3 {
            let target = _mm256_set1_epi16(pixel[component]);
            let limit = _mm256_set1_epi16(tolerance[component]);
            let distance = _mm256_abs_epi16(_mm256_sub_epi16(load_wide(rows[component]), target));
            over = _mm256_or_si256(over, _mm256_cmpgt_epi16(distance, limit));
        }
        let packed = _mm_packs_epi16(
            _mm256_castsi256_si128(over),
            _mm256_extracti128_si256::<1>(over),
        );
        !mask_bits(_mm_movemask_epi8(packed)) & 0xFFFF
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    fn ich_best_avx2(rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32 {
        // Each lane holds the key of its candidate, 0xFFFF when the candidate
        // is not valid.
        let ones = _mm256_set1_epi16(-1);
        let selectors = _mm256_setr_epi16(
            1,
            2,
            4,
            8,
            16,
            32,
            64,
            128,
            256,
            512,
            1024,
            2048,
            4096,
            8192,
            16384,
            i16::MIN,
        );
        let numbers = _mm256_setr_epi16(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let mut sad = _mm256_setzero_si256();
        for component in 0..3 {
            let target = _mm256_set1_epi16(pixel[component]);
            let distance = _mm256_abs_epi16(_mm256_sub_epi16(load_wide(rows[component]), target));
            let weighted = if component == 0 {
                _mm256_slli_epi16::<1>(distance)
            } else {
                distance
            };
            sad = _mm256_add_epi16(sad, weighted);
        }
        let number = _mm256_add_epi16(numbers, _mm256_set1_epi16(lane(first)));
        let key = _mm256_or_si256(_mm256_slli_epi16::<5>(sad), number);
        let lanes = _mm256_set1_epi16(valid_lanes(valid, 0));
        let usable = _mm256_cmpeq_epi16(_mm256_and_si256(lanes, selectors), selectors);
        let key = _mm256_or_si256(key, _mm256_andnot_si256(usable, ones));
        let folded = _mm_min_epu16(
            _mm256_castsi256_si128(key),
            _mm256_extracti128_si256::<1>(key),
        );
        mask_bits(_mm_extract_epi16::<0>(_mm_minpos_epu16(folded))) & 0xFFFF
    }

    /// The kernels are the SSE2 ones; what differs is the slice loop around
    /// them, which the compiler may write with SSSE3 and SSE4 instructions.
    impl Kernels for Sse42 {
        type Lanes = WideLanes;

        #[inline(always)]
        fn ich_first(
            self,
            rows: IchRows<'_>,
            pixel: [i16; 3],
            tolerance: [i16; 3],
            valid: u32,
        ) -> Option<usize> {
            Sse2.ich_first(rows, pixel, tolerance, valid)
        }

        #[inline(always)]
        fn ich_best(self, rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32 {
            Sse2.ich_best(rows, pixel, valid, first)
        }

        #[inline(always)]
        fn to_ycocg(self, row: &[u8], red_first: bool, line: &mut [Pixel]) -> usize {
            Sse2.to_ycocg(row, red_first, line)
        }

        #[inline(always)]
        fn to_rgb(self, line: &[Pixel], red_first: bool, row: &mut [u8]) -> usize {
            Sse2.to_rgb(line, red_first, row)
        }
    }

    /// The history searches use AVX2 (sixteen candidates per register). The
    /// lane arithmetic and the colour conversion are the SSE2 code: inside
    /// the slice loops compiled for AVX2 it is encoded with VEX prefixes, and
    /// wider registers would not make them noticeably faster.
    impl Kernels for Avx2 {
        type Lanes = WideLanes;

        #[inline(always)]
        fn ich_first(
            self,
            rows: IchRows<'_>,
            pixel: [i16; 3],
            tolerance: [i16; 3],
            valid: u32,
        ) -> Option<usize> {
            // SAFETY: an `Avx2` value proves the CPU has AVX2 (see `detect`).
            first_hit(unsafe { ich_hits_avx2(rows, pixel, tolerance) } & valid)
        }

        #[inline(always)]
        fn ich_best(self, rows: IchRows<'_>, pixel: [i16; 3], valid: u32, first: usize) -> u32 {
            // SAFETY: an `Avx2` value proves the CPU has AVX2 (see `detect`).
            unsafe { ich_best_avx2(rows, pixel, valid, first) }
        }

        #[inline(always)]
        fn to_ycocg(self, row: &[u8], red_first: bool, line: &mut [Pixel]) -> usize {
            Sse2.to_ycocg(row, red_first, line)
        }

        #[inline(always)]
        fn to_rgb(self, line: &[Pixel], red_first: bool, row: &mut [u8]) -> usize {
            Sse2.to_rgb(line, red_first, row)
        }
    }
}

/// Which copy of the slice loops runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    /// Portable code only.
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse2,
    /// SSE2 kernels in slice loops compiled for SSE4.2, SSSE3 and POPCNT.
    #[cfg(target_arch = "x86_64")]
    Sse42(x86::Sse42),
    /// AVX2 kernels, and the slice loops compiled for AVX2, BMI and LZCNT.
    #[cfg(target_arch = "x86_64")]
    Avx2(x86::Avx2),
}

impl Level {
    /// The fastest level the running CPU supports. `KWIN_DSC_SIMD` set to
    /// `scalar`, `sse2` or `sse4.2` selects a lower one (every level gives
    /// the same bytes); other values, and levels the CPU lacks, are ignored.
    fn detect() -> Self {
        let wanted = std::env::var("KWIN_DSC_SIMD").unwrap_or_default();
        Self::choose(&wanted)
    }

    fn choose(wanted: &str) -> Self {
        if wanted == "scalar" {
            return Self::Scalar;
        }
        #[cfg(target_arch = "x86_64")]
        {
            if wanted == "sse2" {
                return Self::Sse2;
            }
            match (x86::Avx2::detect(), x86::Sse42::detect()) {
                (Some(token), _) if wanted != "sse4.2" => Self::Avx2(token),
                (_, Some(token)) => Self::Sse42(token),
                (_, None) => Self::Sse2,
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Self::Scalar
        }
    }
}

/// Validated, widened view of a picture parameter set.
#[derive(Clone, Debug)]
struct Params {
    v2: bool,
    bp_enable: bool,
    pic_w: usize,
    pic_h: usize,
    slice_w: usize,
    slice_h: usize,
    slices_per_line: usize,
    slice_rows: usize,
    chunk_bytes: usize,
    multi_slice: bool,
    bpp: i32,
    xmit_delay: i32,
    initial_scale: i64,
    scale_increment_interval: i64,
    scale_decrement_interval: i64,
    first_line_bpg_offset: i64,
    nfl_bpg_offset: i64,
    slice_bpg_offset: i64,
    initial_offset: i64,
    final_offset: i64,
    flatness_min_qp: i32,
    flatness_max_qp: i32,
    model_size: i64,
    edge_factor: i32,
    incr_limit0: i32,
    incr_limit1: i32,
    tgt_hi: i32,
    tgt_lo: i32,
    thresh: [i64; 14],
    range_min: [i32; NUM_BUF_RANGES],
    range_max: [i32; NUM_BUF_RANGES],
    range_offset: [i32; NUM_BUF_RANGES],
    linebuf_shift: [i32; 3],
    linebuf_max: i32,
    rcb_bits: i64,
}

impl Params {
    fn from_pps(pps: &Pps) -> Result<Self, DscError> {
        let unsupported = |what| Err(DscError::UnsupportedPps(what));
        if pps.dsc_version_minor != 1 && pps.dsc_version_minor != 2 {
            return unsupported("dsc_version_minor must be 1 or 2");
        }
        if pps.bits_per_component != 8 {
            return unsupported("only 8 bits per component is implemented");
        }
        if !pps.convert_rgb || pps.simple_422 || pps.native_420 || pps.native_422 {
            return unsupported("only RGB 4:4:4 is implemented");
        }
        if pps.vbr_enable {
            return unsupported("VBR is not implemented");
        }
        if pps.second_line_bpg_offset != 0
            || pps.nsl_bpg_offset != 0
            || pps.second_line_offset_adj != 0
        {
            return unsupported("second-line offsets are only defined for 4:2:0");
        }
        if !(MIN_BPP_X16..=MAX_BPP_X16).contains(&pps.bits_per_pixel_x16) {
            return unsupported("bits per pixel must be within 6..=12");
        }
        if pps.pic_width == 0 || pps.pic_height == 0 || pps.slice_height == 0 {
            return unsupported("picture and slice dimensions must be nonzero");
        }
        if pps.slice_width < 7 || pps.slice_width > pps.pic_width {
            return unsupported("slice width must be within 7..=picture width");
        }
        if !(8..=16).contains(&pps.linebuf_depth) {
            return unsupported("line buffer depth out of range");
        }
        let min_chunk =
            (usize::from(pps.slice_width) * usize::from(pps.bits_per_pixel_x16)).div_ceil(128);
        if usize::from(pps.chunk_size) < min_chunk {
            return unsupported("chunk_size is too small for the slice width and rate");
        }
        if pps.rc_model_size == 0 {
            return unsupported("rc_model_size must be nonzero");
        }
        let mut range_min = [0; NUM_BUF_RANGES];
        let mut range_max = [0; NUM_BUF_RANGES];
        let mut range_offset = [0; NUM_BUF_RANGES];
        for (slot, range) in pps.rc_range_parameters.iter().enumerate() {
            if i32::from(range.min_qp) > MAX_QP || i32::from(range.max_qp) > MAX_QP {
                return unsupported("range QP above 15");
            }
            range_min[slot] = i32::from(range.min_qp);
            range_max[slot] = i32::from(range.max_qp);
            range_offset[slot] = i32::from(range.bpg_offset);
        }
        let mut thresh = [0i64; 14];
        for (target, source) in thresh.iter_mut().zip(pps.rc_buf_thresh.iter()) {
            *target = i64::from(*source);
        }
        let linebuf_depth = i32::from(pps.linebuf_depth);
        let mut linebuf_shift = [0; 3];
        for (shift, depth) in linebuf_shift.iter_mut().zip(DEPTH.iter()) {
            *shift = (depth - linebuf_depth).max(0);
        }
        let bpp = i64::from(pps.bits_per_pixel_x16);
        let hrd_delay = i64::from(pps.initial_dec_delay) + i64::from(pps.initial_xmit_delay);
        Ok(Self {
            v2: pps.dsc_version_minor == 2,
            bp_enable: pps.block_pred_enable,
            pic_w: usize::from(pps.pic_width),
            pic_h: usize::from(pps.pic_height),
            slice_w: usize::from(pps.slice_width),
            slice_h: usize::from(pps.slice_height),
            slices_per_line: pps.slices_per_line(),
            slice_rows: pps.slice_rows(),
            chunk_bytes: usize::from(pps.chunk_size),
            multi_slice: pps.slice_width != pps.pic_width,
            bpp: i32::from(pps.bits_per_pixel_x16),
            xmit_delay: i32::from(pps.initial_xmit_delay),
            initial_scale: i64::from(pps.initial_scale_value),
            scale_increment_interval: i64::from(pps.scale_increment_interval),
            scale_decrement_interval: i64::from(pps.scale_decrement_interval),
            first_line_bpg_offset: i64::from(pps.first_line_bpg_offset),
            nfl_bpg_offset: i64::from(pps.nfl_bpg_offset),
            slice_bpg_offset: i64::from(pps.slice_bpg_offset),
            initial_offset: i64::from(pps.initial_offset),
            final_offset: i64::from(pps.final_offset),
            flatness_min_qp: i32::from(pps.flatness_min_qp),
            flatness_max_qp: i32::from(pps.flatness_max_qp),
            model_size: i64::from(pps.rc_model_size),
            edge_factor: i32::from(pps.rc_edge_factor),
            incr_limit0: i32::from(pps.rc_quant_incr_limit0),
            incr_limit1: i32::from(pps.rc_quant_incr_limit1),
            tgt_hi: i32::from(pps.rc_tgt_offset_hi),
            tgt_lo: i32::from(pps.rc_tgt_offset_lo),
            thresh,
            range_min,
            range_max,
            range_offset,
            linebuf_shift,
            linebuf_max: (1 << linebuf_depth) - 1,
            rcb_bits: ceil_div(hrd_delay * bpp, 16),
        })
    }

    fn slice_bytes(&self) -> usize {
        self.chunk_bytes * self.slice_h
    }

    fn line_len(&self) -> usize {
        self.slice_w + PAD_LEFT + PAD_RIGHT
    }

    #[inline(always)]
    fn flatness_info_sent(&self, qp: i32) -> bool {
        qp >= self.flatness_min_qp && qp <= self.flatness_max_qp
    }
}

#[derive(Clone, Copy)]
enum PredMode {
    Map,
    Left,
    Block,
}

/// Block-prediction candidates that take part in the search: candidate 0 (the
/// left neighbour) is the reference, 2..=9 are vectors -3..=-10.
const BP_CANDIDATES: [usize; 9] = [0, 2, 3, 4, 5, 6, 7, 8, 9];
/// Block vector length of each search slot (0 for the reference), indexed by
/// the low `BP_KEY_SHIFT` bits of a search key.
const BP_VECTORS: [u8; 16] = [0, 3, 4, 5, 6, 7, 8, 9, 10, 0, 0, 0, 0, 0, 0, 0];
const BP_KEY_SHIFT: u32 = 4;

/// Slice state shared by the encoder and the decoder.
struct Core {
    /// The line being reconstructed and the line above it as the line buffer
    /// keeps it, both with `PAD_LEFT` and `PAD_RIGHT` pixels around them.
    cur: Vec<Pixel>,
    prev: Vec<Pixel>,
    /// Per sample of the previous line: the 1-2-1 filtered value minus the
    /// sample, which MAP prediction clamps and adds back.
    blend: Vec<Pixel>,
    /// The previous line again, one plane per component, for the searches
    /// that compare many pixels of one component.
    planes: [Vec<i16>; 3],
    /// Per group of the previous line: 0 for MAP, else the block vector length.
    pred: Vec<u8>,
    /// Block-prediction search scratch, one entry per pixel of the line.
    bp_acc: Vec<i16>,
    bp_sum: Vec<i16>,
    bp_min: Vec<i16>,
    bp_edge: Vec<i16>,
    ich: IchTable,
    ich_valid: usize,
    st_qp: i32,
    prev_qp: i32,
    primary_qp: i32,
    prev_primary_qp: i32,
    predicted_size: [i32; 3],
    rc_size_unit: [i32; 3],
    rc_size_group: i32,
    coded_group_size: i32,
    buffer_fullness: i64,
    bpg_frac_accum: i32,
    num_bits_chunk: i32,
    chunk_pixel_times: usize,
    rc_xform_offset: i64,
    throttle_frac: i64,
    current_scale: i64,
    scale_adjust_counter: i64,
    scale_increment_start: bool,
    rc_offset_clamp: bool,
    prev_range: usize,
    pixel_count: i32,
    prev_pixel_count: i32,
    left_recon: [i32; 3],
    ich_selected: bool,
    first_flat: i32,
    prev_first_flat: i32,
    flatness_type: i32,
    orig_is_flat: bool,
    bit_save_mode: i32,
    mpp_state: i32,
    group_count: i32,
    v_pos: usize,
    midpoint: [bool; 3],
    model_overflow: bool,
    buffer_overflow: bool,
}

/// The pixel every line buffer holds at the start of a slice.
fn mid_pixel() -> Pixel {
    [
        sample(mid_value(0)),
        sample(mid_value(1)),
        sample(mid_value(2)),
        0,
    ]
}

impl Core {
    fn new(params: &Params) -> Self {
        let line = params.line_len();
        let groups = params.slice_w.div_ceil(3);
        let mut core = Self {
            cur: vec![[0; 4]; line],
            prev: vec![[0; 4]; line],
            blend: vec![[0; 4]; line],
            planes: [vec![0; line], vec![0; line], vec![0; line]],
            pred: vec![0; groups],
            bp_acc: vec![0; line],
            bp_sum: vec![0; line],
            bp_min: vec![0; line],
            bp_edge: vec![0; line],
            ich: [[0; ICH_SIZE]; 3],
            ich_valid: 0,
            st_qp: 0,
            prev_qp: 0,
            primary_qp: 0,
            prev_primary_qp: 0,
            predicted_size: [0; 3],
            rc_size_unit: [0; 3],
            rc_size_group: 0,
            coded_group_size: 0,
            buffer_fullness: 0,
            bpg_frac_accum: 0,
            num_bits_chunk: 0,
            chunk_pixel_times: 0,
            rc_xform_offset: 0,
            throttle_frac: 0,
            current_scale: 0,
            scale_adjust_counter: 0,
            scale_increment_start: false,
            rc_offset_clamp: false,
            prev_range: 0,
            pixel_count: 0,
            prev_pixel_count: 0,
            left_recon: [0; 3],
            ich_selected: false,
            first_flat: -1,
            prev_first_flat: -1,
            flatness_type: 0,
            orig_is_flat: false,
            bit_save_mode: 0,
            mpp_state: 0,
            group_count: 0,
            v_pos: 0,
            midpoint: [false; 3],
            model_overflow: false,
            buffer_overflow: false,
        };
        core.reset(params);
        core
    }

    /// Whether the buffers have the sizes `params` needs.
    fn fits(&self, params: &Params) -> bool {
        self.cur.len() == params.line_len()
    }

    /// Start-of-slice state.
    fn reset(&mut self, params: &Params) {
        let mid = mid_pixel();
        self.cur.fill(mid);
        self.prev.fill(mid);
        self.blend.fill([0; 4]);
        for ((plane, history), value) in self.planes.iter_mut().zip(self.ich.iter_mut()).zip(mid) {
            plane.fill(value);
            *history = [value; ICH_SIZE];
        }
        self.pred.fill(0);
        self.bp_sum.fill(0);
        self.ich_valid = 0;
        self.st_qp = 0;
        self.prev_qp = 0;
        self.primary_qp = 0;
        self.prev_primary_qp = 0;
        self.predicted_size = [0; 3];
        self.rc_size_unit = [0; 3];
        self.rc_size_group = 0;
        self.coded_group_size = 0;
        self.buffer_fullness = 0;
        self.bpg_frac_accum = 0;
        self.num_bits_chunk = 0;
        self.chunk_pixel_times = 0;
        self.rc_xform_offset = params.initial_offset;
        self.throttle_frac = 0;
        self.current_scale = 0;
        self.scale_adjust_counter = 0;
        self.scale_increment_start = false;
        self.rc_offset_clamp = false;
        self.prev_range = 0;
        self.pixel_count = 0;
        self.prev_pixel_count = 0;
        self.left_recon = [0; 3];
        self.ich_selected = false;
        self.first_flat = -1;
        self.prev_first_flat = -1;
        self.flatness_type = 0;
        self.orig_is_flat = false;
        self.bit_save_mode = 0;
        self.mpp_state = 0;
        self.group_count = 0;
        self.v_pos = 0;
        self.midpoint = [false; 3];
        self.model_overflow = false;
        self.buffer_overflow = false;
    }

    /// Feed the previous group's reconstructed pixels into the colour history.
    #[inline(always)]
    fn ich_group_start<K: Kernels>(
        &mut self,
        kernels: K,
        params: &Params,
        group_start: usize,
        previous_group_ich: bool,
    ) {
        if group_start == 0 && (self.v_pos == 0 || params.multi_slice) {
            self.ich_valid = 0;
        }
        if group_start < 3 {
            return;
        }
        let reserved = if self.v_pos == 0 {
            ICH_SIZE
        } else {
            ICH_RESERVED
        };
        let group: &[Pixel; 3] = pixels(&self.cur, group_start - 3 + PAD_LEFT);
        if !previous_group_ich {
            // No duplicate search: the three pixels go to the front at once.
            let valid = self.ich_valid.min(reserved);
            ich_insert_group(&mut self.ich, valid, reserved, group);
            if valid < reserved {
                self.ich_valid = (valid + 3).min(reserved);
            }
            return;
        }
        for pixel in group {
            let pixel = [pixel[0], pixel[1], pixel[2]];
            let valid = self.ich_valid.min(reserved);
            let newest = [self.ich[0][0], self.ich[1][0], self.ich[2][0]];
            if valid > 0 && newest == pixel {
                // The pixel is the newest entry already: nothing moves.
                continue;
            }
            let mut location = if valid < reserved {
                valid
            } else {
                reserved - 1
            };
            let same = ich_table_first(kernels, &self.ich, pixel, [0; 3], low_bits(valid));
            if let Some(same) = same {
                location = same;
            }
            ich_insert(&mut self.ich, location, pixel);
            if location == valid {
                self.ich_valid = valid + 1;
            }
        }
    }

    /// Column (with padding) of history entry 25 for the group at `group_start`.
    #[inline(always)]
    fn ich_above_base(params: &Params, group_start: usize) -> usize {
        let low = ICH_ABOVE / 2;
        let high = params.slice_w - 1 - ICH_ABOVE / 2;
        let centre = group_start + 1;
        let centre = if centre > high { high } else { centre.max(low) };
        centre - ICH_ABOVE / 2 + PAD_LEFT
    }

    #[inline(always)]
    fn ich_pixel(&self, params: &Params, entry: usize, group_start: usize) -> [i32; 3] {
        let entry = entry % ICH_SIZE;
        if self.v_pos > 0 && entry >= ICH_RESERVED {
            let x = Self::ich_above_base(params, group_start) + entry - ICH_RESERVED;
            let [y, co, cg, _] = pixels::<1>(&self.prev, x)[0].map(i32::from);
            [y, co, cg]
        } else {
            [
                i32::from(self.ich[0][entry]),
                i32::from(self.ich[1][entry]),
                i32::from(self.ich[2][entry]),
            ]
        }
    }

    /// Predictor of the group and, for block prediction, the vector length.
    #[inline(always)]
    fn pred_mode(&self, group_start: usize) -> (PredMode, usize) {
        if self.v_pos == 0 {
            return (PredMode::Left, 0);
        }
        match self.pred.get(group_start / 3).copied().unwrap_or(0) {
            0 => (PredMode::Map, 0),
            vector => (PredMode::Block, usize::from(vector)),
        }
    }

    /// Predictor inputs of the group at `group_start`, all components.
    #[inline(always)]
    fn predictor<V: Lanes>(
        &self,
        mode: (PredMode, usize),
        group_start: usize,
        quant: &QuantLanes<V>,
    ) -> Pred<V> {
        let base = group_start + PAD_LEFT;
        let a = V::new(pixels::<1>(&self.cur, base - 1)[0]);
        match mode.0 {
            PredMode::Map => {
                let previous: &[Pixel; 4] = pixels(&self.prev, base - 1);
                let filter: &[Pixel; 4] = pixels(&self.blend, base - 1);
                let blend = |slot: usize| {
                    V::new(previous[slot])
                        .add(V::new(filter[slot]).clamp(quant.neg_half, quant.half))
                };
                Pred::Map {
                    a,
                    c: if group_start == 0 { a } else { blend(0) },
                    b: blend(1),
                    d: blend(2),
                    e: blend(3),
                }
            }
            PredMode::Left => Pred::Left { a },
            PredMode::Block => {
                let reference = |offset: usize| {
                    V::new(pixels::<1>(&self.cur, (base + offset).saturating_sub(mode.1))[0])
                };
                Pred::Block([reference(0), reference(1), reference(2)])
            }
        }
    }

    #[inline(always)]
    fn midpoint_pred(&self, component: usize, qlevel: i32) -> i32 {
        // Reconstructed samples are never negative, so the mask is the
        // remainder of the division by `1 << qlevel`.
        mid_value(component) + (self.left_recon[component] & ((1 << qlevel) - 1))
    }

    #[inline(always)]
    fn adjusted_predicted_size(&self, component: usize) -> i32 {
        let new = qlevel(component, self.primary_qp);
        let old = qlevel(component, self.prev_primary_qp);
        clamp_c(
            self.predicted_size[component] + old - new,
            0,
            DEPTH[component] - new - 1,
        )
    }

    /// QP change at a flat group (after the encoder's flatness decision or the
    /// decoder's flatness syntax), for the last pixel `last_h` of the group.
    #[inline(always)]
    fn flatness_qp_adjust(&mut self, params: &Params, last_h: usize) {
        if params.v2 && last_h >= params.slice_w - 1 {
            self.orig_is_flat = true;
            self.flatness_type = 1;
        }
        if self.orig_is_flat && self.primary_qp < params.range_max[NUM_BUF_RANGES - 1] {
            if self.flatness_type == 0 || self.primary_qp < SOMEWHAT_FLAT_QP_THRESH {
                self.st_qp = (self.st_qp - SOMEWHAT_FLAT_QP_DELTA).max(0);
                if params.v2 {
                    self.prev_qp = (self.prev_qp - SOMEWHAT_FLAT_QP_DELTA).max(0);
                }
            } else {
                self.st_qp = VERY_FLAT_QP;
                if params.v2 {
                    self.prev_qp = VERY_FLAT_QP;
                }
            }
        }
    }

    /// Store the group's reconstructed pixels (the first `pixels` of
    /// `group`) and run the rate control.
    #[inline(always)]
    fn finish_group(
        &mut self,
        params: &Params,
        group_start: usize,
        pixels: usize,
        group: &[Pixel; 3],
    ) {
        let pixels = pixels.clamp(1, 3);
        let base = group_start + PAD_LEFT;
        // Fixed-size stores: a copy of variable length would be a call.
        if let Some(target) = self.cur.get_mut(base..base + 3)
            && pixels == 3
        {
            target[0] = group[0];
            target[1] = group[1];
            target[2] = group[2];
        } else if let Some(target) = self.cur.get_mut(base..base + pixels) {
            for (target, pixel) in target.iter_mut().zip(group) {
                *target = *pixel;
            }
        }
        let [y, co, cg, _] = group[pixels - 1].map(i32::from);
        self.left_recon = [y, co, cg];
        let (scale, bpg_offset) = self.calc_fullness_offset(params);
        self.group_count += 1;
        self.rate_control(
            params,
            scale,
            bpg_offset,
            i32::try_from(pixels).unwrap_or(3),
        );
    }

    #[inline(always)]
    fn calc_fullness_offset(&mut self, params: &Params) -> (i64, i64) {
        if self.group_count == 0 {
            self.current_scale = params.initial_scale;
            self.scale_adjust_counter = 1;
        } else if self.v_pos == 0 && self.current_scale > 8 {
            self.scale_adjust_counter += 1;
            if self.scale_adjust_counter >= params.scale_decrement_interval {
                self.scale_adjust_counter = 0;
                self.current_scale -= 1;
            }
        } else if self.scale_increment_start {
            self.scale_adjust_counter += 1;
            if self.scale_adjust_counter >= params.scale_increment_interval {
                self.scale_adjust_counter = 0;
                self.current_scale += 1;
            }
        }
        let (mut target, mut increment) = if self.v_pos == 0 {
            (
                params.first_line_bpg_offset,
                -(params.first_line_bpg_offset << OFFSET_FRACTIONAL_BITS),
            )
        } else {
            (
                -(params.nfl_bpg_offset >> OFFSET_FRACTIONAL_BITS),
                params.nfl_bpg_offset,
            )
        };
        if self.pixel_count < params.xmit_delay {
            let pixels = if self.pixel_count == 0 {
                3
            } else {
                self.pixel_count - self.prev_pixel_count
            };
            let pixels = pixels.min(params.xmit_delay - self.pixel_count);
            increment -= i64::from(params.bpp * pixels) << (OFFSET_FRACTIONAL_BITS - 4);
        } else if params.scale_increment_interval != 0
            && !self.scale_increment_start
            && self.v_pos > 0
            && self.rc_xform_offset > 0
        {
            self.current_scale = 9;
            self.scale_adjust_counter = 0;
            self.scale_increment_start = true;
        }
        self.prev_pixel_count = self.pixel_count;
        target -= params.slice_bpg_offset >> OFFSET_FRACTIONAL_BITS;
        increment += params.slice_bpg_offset;
        self.throttle_frac += increment;
        self.rc_xform_offset += self.throttle_frac >> OFFSET_FRACTIONAL_BITS;
        self.throttle_frac &= (1 << OFFSET_FRACTIONAL_BITS) - 1;
        if self.rc_xform_offset < params.final_offset {
            self.rc_offset_clamp = true;
        }
        if self.rc_offset_clamp {
            self.rc_xform_offset = self.rc_xform_offset.min(params.final_offset);
        }
        (self.current_scale, target)
    }

    /// Remove one pixel time of bits from the rate buffer model.
    #[inline(always)]
    fn remove_bits(&mut self, params: &Params) {
        self.bpg_frac_accum += params.bpp & 15;
        let bits = (params.bpp >> 4) + (self.bpg_frac_accum >> 4);
        self.buffer_fullness -= i64::from(bits);
        self.num_bits_chunk += bits;
        self.bpg_frac_accum &= 15;
        self.chunk_pixel_times += 1;
        if self.chunk_pixel_times >= params.slice_w {
            let chunk_bits = i64::try_from(params.chunk_bytes * 8).unwrap_or(0);
            self.buffer_fullness -= chunk_bits - i64::from(self.num_bits_chunk);
            self.bpg_frac_accum = 0;
            self.num_bits_chunk = 0;
            self.chunk_pixel_times = 0;
        }
    }

    #[inline(always)]
    fn rate_control(&mut self, params: &Params, scale: i64, bpg_offset: i64, group_size: i32) {
        let prev_qp = self.st_qp;
        let prev2_qp = self.prev_qp;
        let rc_size_group_prev = self.rc_size_group;
        let pixel_times = usize::try_from(group_size).unwrap_or(0);
        if self.pixel_count + 1 >= params.xmit_delay
            && self.chunk_pixel_times + pixel_times < params.slice_w
        {
            // Every pixel time of the group removes bits and none ends a
            // chunk, so the per-pixel steps of `remove_bits` add up to this.
            let fraction = self.bpg_frac_accum + group_size * (params.bpp & 15);
            let bits = group_size * (params.bpp >> 4) + (fraction >> 4);
            self.buffer_fullness -= i64::from(bits);
            self.num_bits_chunk += bits;
            self.bpg_frac_accum = fraction & 15;
            self.chunk_pixel_times += pixel_times;
            self.pixel_count += group_size;
        } else {
            for _ in 0..group_size {
                self.pixel_count += 1;
                if self.pixel_count >= params.xmit_delay {
                    self.remove_bits(params);
                }
            }
        }
        let rc_size_group: i32 = self.rc_size_unit.iter().sum();
        let throttle = self.rc_xform_offset - params.model_size;
        let model_fullness = (scale * (self.buffer_fullness + throttle)) >> 3;
        let overflow_avoid = self.buffer_fullness + throttle > OVERFLOW_AVOID_THRESHOLD;
        let mut range = NUM_BUF_RANGES - 1;
        while range > 0 {
            if model_fullness > params.thresh[range - 1] - params.model_size {
                break;
            }
            range -= 1;
        }
        if model_fullness > 0 {
            self.model_overflow = true;
        }
        let selected = self.prev_range;
        self.prev_range = range;

        let bpg = (params.bpp * group_size + 8) >> 4;
        let target_wide = i64::from(bpg) + i64::from(params.range_offset[selected]) + bpg_offset;
        let target = i32::try_from(target_wide.max(0)).unwrap_or(i32::MAX);
        let mut min_qp = params.range_min[selected];
        let mut max_qp = params.range_max[selected];
        let target_minus = (target - params.tgt_lo).max(0);
        let target_plus = (target + params.tgt_hi).max(0);
        let increment = (self.coded_group_size - target) >> 1;

        let activity = self.prev_qp
            + self.predicted_size[0]
            + self.predicted_size[1].max(self.predicted_size[2]);
        let bit_save_thresh = DEPTH[0] + DEPTH[1] - 2;
        if params.v2 && self.v_pos > 0 && self.first_flat == -1 {
            if !self.ich_selected && self.midpoint == [true; 3] {
                self.mpp_state += 1;
                if self.mpp_state >= 2 {
                    self.bit_save_mode = 2;
                }
            } else if !self.ich_selected && activity >= bit_save_thresh {
                // Keep the current bit-save mode.
            } else if self.ich_selected {
                self.bit_save_mode = self.bit_save_mode.max(1);
            } else {
                self.mpp_state = 0;
                self.bit_save_mode = 0;
            }
        } else {
            self.bit_save_mode = 0;
            self.mpp_state = 0;
        }

        let mut st_qp;
        if params.v2 && self.buffer_fullness < 192 {
            st_qp = min_qp;
        } else if self.bit_save_mode != 0 {
            max_qp = (max_qp + 1).min(MAX_QP);
            st_qp = if self.bit_save_mode == 1 {
                prev_qp
            } else {
                prev_qp + 2
            };
        } else if rc_size_group == 3 {
            if params.v2 {
                min_qp = (min_qp - 4).max(0);
                st_qp = prev_qp - 1;
            } else {
                st_qp = (min_qp / 2).max(prev_qp - 1);
            }
        } else if (!params.v2
            && self.coded_group_size < target_minus
            && rc_size_group < target_minus)
            || (params.v2 && rc_size_group < target_minus)
        {
            st_qp = if params.v2 {
                prev_qp - 1
            } else {
                min_qp.max(prev_qp - 1)
            };
        } else if self.buffer_fullness >= 64 && self.coded_group_size > target_plus {
            let cur_qp = prev_qp.max(min_qp);
            let raised = if params.v2 {
                cur_qp + increment
            } else {
                max_qp.min(cur_qp + increment)
            };
            let calm = rc_size_group * 2 < rc_size_group_prev * params.edge_factor;
            st_qp = if prev2_qp == cur_qp {
                if calm { raised } else { cur_qp }
            } else if prev2_qp < cur_qp {
                if calm && cur_qp < params.incr_limit0 {
                    raised
                } else {
                    cur_qp
                }
            } else if cur_qp < params.incr_limit1 {
                raised
            } else {
                cur_qp
            };
        } else {
            st_qp = prev_qp;
        }
        if params.v2 {
            st_qp = clamp_c(st_qp, min_qp, max_qp);
        }
        if overflow_avoid {
            st_qp = params.range_max[NUM_BUF_RANGES - 1];
        }
        self.rc_size_group = rc_size_group;
        self.st_qp = st_qp.clamp(0, MAX_QP);
        self.prev_qp = prev_qp;
        if self.buffer_fullness > params.rcb_bits {
            self.buffer_overflow = true;
        }
    }

    /// End of a slice line: move the line into the (depth-limited) line
    /// buffer, prepare the MAP filter and search block-prediction vectors for
    /// the next line.
    #[inline(always)]
    fn end_line(&mut self, params: &Params) {
        let width = params.slice_w;
        let (Some(target), Some(source)) = (
            self.prev.get_mut(PAD_LEFT..PAD_LEFT + width),
            self.cur.get(PAD_LEFT..PAD_LEFT + width),
        ) else {
            return;
        };
        let largest = (1 << DEPTH[1].max(DEPTH[2]).max(DEPTH[0])) - 1;
        if params.linebuf_shift == [0; 3] && params.linebuf_max >= largest {
            // The line buffer keeps every bit of the samples.
            target.copy_from_slice(source);
        } else {
            for (target, source) in target.iter_mut().zip(source.iter()) {
                for component in 0..3 {
                    let shift = params.linebuf_shift[component];
                    let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
                    let stored =
                        ((i32::from(source[component]) + round) >> shift).min(params.linebuf_max);
                    target[component] = sample(stored << shift);
                }
            }
        }
        let first = target.first().copied().unwrap_or_default();
        let last = target.last().copied().unwrap_or_default();
        self.prev[..PAD_LEFT].fill(first);
        self.prev[PAD_LEFT + width..].fill(last);
        // Pixels PAD_LEFT - 1 ..= PAD_LEFT + width + 2 are the ones MAP
        // prediction blends; the filter is the same for every lane.
        let count = (width + 4) * 4;
        let from = (PAD_LEFT - 1) * 4;
        let samples = self.prev.as_flattened();
        if let (Some(filter), Some(left), Some(centre), Some(right)) = (
            self.blend.as_flattened_mut().get_mut(from..from + count),
            samples.get(from - 4..from - 4 + count),
            samples.get(from..from + count),
            samples.get(from + 4..from + 4 + count),
        ) {
            let neighbours = left.iter().zip(centre.iter()).zip(right.iter());
            for (filter, ((left, centre), right)) in filter.iter_mut().zip(neighbours) {
                *filter = ((*left + 2 * *centre + *right + 2) >> 2) - *centre;
            }
        }
        let [luma, orange, green] = &mut self.planes;
        let planes = luma.iter_mut().zip(orange.iter_mut()).zip(green.iter_mut());
        for (pixel, ((y, co), cg)) in self.prev.iter().zip(planes) {
            *y = pixel[0];
            *co = pixel[1];
            *cg = pixel[2];
        }
        if params.bp_enable {
            self.block_pred_search(params);
        }
        self.v_pos += 1;
        self.ich_valid = self.ich_valid.min(ICH_RESERVED);
    }

    /// Choose, per group of the next line, MAP prediction or a block vector
    /// from the line just stored. A group is block-predicted only when it
    /// holds an edge and it and the two groups before it have a vector that
    /// beats the left neighbour, so only the span of groups from four before
    /// the first edge to the last edge is searched.
    #[inline(always)]
    fn block_pred_search(&mut self, params: &Params) {
        let groups = params.slice_w / 3;
        let pixels = groups * 3;
        let (Some(pred), Some(edge)) =
            (self.pred.get_mut(..groups), self.bp_edge.get_mut(..pixels))
        else {
            return;
        };
        if groups == 0 {
            return;
        }
        edge.fill(0);
        for component in 0..3 {
            let Some(current) = self.planes[component].get(PAD_LEFT..PAD_LEFT + pixels) else {
                return;
            };
            let mid = sample(mid_value(component));
            edge[0] |= i16::from((current[0] - mid).abs() > BP_EDGE_STRENGTH);
            let pairs = current[1..].iter().zip(current.iter());
            for (edge, (value, left)) in edge[1..].iter_mut().zip(pairs) {
                *edge |= i16::from((*value - *left).abs() > BP_EDGE_STRENGTH);
            }
        }
        pred.fill(0);
        let has_edge = |flags: &[i16]| flags.iter().any(|flag| *flag != 0);
        let mut edge_groups = edge.chunks_exact(BP_EDGE_COUNT);
        let Some(first_edge) = edge_groups.position(has_edge) else {
            return;
        };
        // `rposition` counts from the front of what `position` left over.
        let last_edge = edge_groups
            .rposition(has_edge)
            .map_or(first_edge, |after| first_edge + 1 + after);
        // Vectors are compared for the groups from `start` on; their sums
        // reach two groups further back.
        let start = first_edge.saturating_sub(2);
        let low = first_edge.saturating_sub(4) * 3;
        let high = (last_edge + 1) * 3;
        let from = start * 3;
        for (slot, candidate) in BP_CANDIDATES.iter().enumerate() {
            let Some(acc) = self.bp_acc.get_mut(low..high) else {
                return;
            };
            acc.fill(0);
            // Pixels up to `candidate` have no reference in the line and are
            // compared with the mid value.
            let head = (candidate + 1).clamp(low, high);
            for (component, line) in self.planes.iter().enumerate() {
                let shift = DEPTH[component] - 7;
                let mid = sample(mid_value(component));
                if let Some(current) = line.get(PAD_LEFT + low..PAD_LEFT + head) {
                    for (sum, value) in acc.iter_mut().zip(current.iter()) {
                        *sum += ((*value - mid).abs() >> shift).min(63);
                    }
                }
                if high > head
                    && let (Some(sums), Some(current), Some(reference)) = (
                        acc.get_mut(head - low..),
                        line.get(PAD_LEFT + head..PAD_LEFT + high),
                        line.get(PAD_LEFT + head - 1 - candidate..PAD_LEFT + high - 1 - candidate),
                    )
                {
                    let pairs = current.iter().zip(reference.iter());
                    for (sum, (value, predictor)) in sums.iter_mut().zip(pairs) {
                        *sum += ((*value - *predictor).abs() >> shift).min(63);
                    }
                }
            }
            // `bp_sum[x + 6]` is the capped error of the three pixels from
            // `x` on; the six entries before pixel 0 stay zero.
            if let (Some(sums), Some(second), Some(third)) = (
                self.bp_sum.get_mut(low + 6..high + 4),
                acc.get(1..),
                acc.get(2..),
            ) {
                let triples = acc.iter().zip(second.iter()).zip(third.iter());
                for (sum, ((a, b), c)) in sums.iter_mut().zip(triples) {
                    *sum = (*a + *b + *c).min(511);
                }
            }
            // The error of a group and the two groups before it, for a group
            // starting at any pixel `x` (only multiples of three are used).
            // The lowest key `error << BP_KEY_SHIFT | slot` is the first
            // candidate with the lowest error; a plain minimum, which the
            // compiler turns into vector code, keeps it.
            let (Some(lowest), Some(two_back), Some(one_back), Some(own)) = (
                self.bp_min.get_mut(from..high - 2),
                self.bp_sum.get(from..),
                self.bp_sum.get(from + 3..),
                self.bp_sum.get(from + 6..),
            ) else {
                return;
            };
            let number = i16::try_from(slot).unwrap_or(0);
            let errors = two_back.iter().zip(one_back.iter()).zip(own.iter());
            if slot == 0 {
                for (lowest, ((a, b), c)) in lowest.iter_mut().zip(errors) {
                    *lowest = ((*a + *b + *c) >> 3) << BP_KEY_SHIFT;
                }
            } else {
                for (lowest, ((a, b), c)) in lowest.iter_mut().zip(errors) {
                    let key = (((*a + *b + *c) >> 3) << BP_KEY_SHIFT) | number;
                    *lowest = (*lowest).min(key);
                }
            }
        }
        let mut bp_count = 0u32;
        for (group, vector) in pred.iter_mut().enumerate().take(last_edge + 1).skip(start) {
            let x = group * 3;
            let key = self.bp_min.get(x).copied().unwrap_or(0);
            let best = BP_VECTORS[usize::try_from(key).unwrap_or(0) % BP_VECTORS.len()];
            let counted = if best != 0 { bp_count + 1 } else { 0 };
            bp_count = if group >= 3 { counted } else { bp_count };
            let flags: &[i16; BP_EDGE_COUNT] = window(edge, x);
            let has_edge = (flags[0] | flags[1] | flags[2]) != 0;
            *vector = if bp_count >= 3 && has_edge { best } else { 0 };
        }
    }
}

/// Counters gathered while encoding one picture; they describe how hard the
/// picture was to code at the current rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Coded groups (three pixels each).
    pub groups: u64,
    /// Sum of the quantization parameter (0..=15) over all groups.
    pub qp_sum: u64,
    /// Groups coded from the indexed colour history.
    pub ich_groups: u64,
    /// Predictively coded groups whose residuals were all zero.
    pub zero_residual_groups: u64,
    /// Groups reproduced exactly from zero residuals or the colour history.
    pub flat_groups: u64,
    /// Groups where midpoint prediction was forced to keep the constant-rate
    /// buffer from underflowing, that is, the picture needed fewer bits.
    pub stuffed_groups: u64,
}

impl FrameStats {
    fn add(&mut self, other: &Self) {
        self.groups += other.groups;
        self.qp_sum += other.qp_sum;
        self.ich_groups += other.ich_groups;
        self.zero_residual_groups += other.zero_residual_groups;
        self.flat_groups += other.flat_groups;
        self.stuffed_groups += other.stuffed_groups;
    }

    fn permille(&self, part: u64, scale: u64) -> u32 {
        let total = self.groups.max(1) * scale;
        u32::try_from(part * 1000 / total).unwrap_or(1000)
    }

    /// Mean quantization parameter times 16 (0 = lossless, 240 = coarsest).
    pub fn mean_qp_x16(&self) -> u32 {
        u32::try_from(self.qp_sum * 16 / self.groups.max(1)).unwrap_or(u32::MAX)
    }

    /// Complexity at the current rate: mean QP as a share of the maximum, in
    /// thousandths. 0 means the picture was coded without quantization.
    pub fn complexity_permille(&self) -> u32 {
        self.permille(self.qp_sum, 15)
    }

    /// Share of groups that were flat or repeated recent colours (zero
    /// residuals or colour-history hits), in thousandths.
    pub fn flatness_permille(&self) -> u32 {
        self.permille(self.flat_groups, 1)
    }

    /// Share of groups that had to be padded to hold the constant rate, in
    /// thousandths; a high value means a lower rate would cost no quality.
    pub fn stuffing_permille(&self) -> u32 {
        self.permille(self.stuffed_groups, 1)
    }
}

/// One slice's share of the output picture: its chunk of every slice line.
struct EncodeJob<'a> {
    xstart: usize,
    ystart: usize,
    chunks: Vec<&'a mut [u8]>,
}

/// Low `size` bits of three residuals, first residual in the highest bits.
#[inline(always)]
fn pack_residuals(values: [i32; 3], size: u32) -> u32 {
    let mask = (1u32 << size) - 1;
    let raw = values.map(|value| u32::from_le_bytes(value.to_le_bytes()) & mask);
    (raw[0] << (2 * size)) | (raw[1] << size) | raw[2]
}

/// Everything one thread needs to encode slices; kept between pictures.
struct SliceEncoder {
    core: Core,
    orig: Vec<Pixel>,
    /// Per pixel position `h` of the line and per component, the spread
    /// (highest minus lowest) of the original samples the flatness test
    /// looks at: of samples `h..h + 4` and of samples `h + 1..h + 7`.
    near_spread: Vec<Pixel>,
    far_spread: Vec<Pixel>,
    scan_low: Vec<Pixel>,
    scan_high: Vec<Pixel>,
    substreams: [BitWriter; 3],
    /// Per group: the bits it wrote to each substream, eight bits each.
    se_sizes: Vec<u32>,
    prev_flatness_type: i32,
    muxed: Vec<u8>,
}

impl SliceEncoder {
    fn new(params: &Params) -> Self {
        let line = || vec![[0; 4]; params.line_len()];
        Self {
            core: Core::new(params),
            orig: line(),
            near_spread: line(),
            far_spread: line(),
            scan_low: line(),
            scan_high: line(),
            substreams: [
                BitWriter::default(),
                BitWriter::default(),
                BitWriter::default(),
            ],
            se_sizes: Vec::new(),
            prev_flatness_type: 0,
            muxed: Vec::new(),
        }
    }

    /// Start-of-slice state; buffers keep their allocations between slices
    /// and are only rebuilt when the slice geometry changed.
    fn start(&mut self, params: &Params) {
        if !self.core.fits(params) {
            *self = Self::new(params);
        }
        self.core.reset(params);
        let groups = params.slice_w.div_ceil(3) * params.slice_h;
        for writer in &mut self.substreams {
            writer.clear(params.slice_bytes());
        }
        self.se_sizes.clear();
        self.se_sizes.reserve(groups);
        self.prev_flatness_type = 0;
        self.muxed.resize(params.slice_bytes(), 0);
    }

    #[inline(always)]
    fn populate_orig<K: Kernels>(
        &mut self,
        kernels: K,
        params: &Params,
        frame: &Frame<'_>,
        xstart: usize,
        y: usize,
    ) -> Result<(), DscError> {
        let width = params.slice_w + PAD_RIGHT;
        let Some(line) = self.orig.get_mut(PAD_LEFT..PAD_LEFT + width) else {
            return Ok(());
        };
        if y >= params.pic_h {
            line.fill(mid_pixel());
            return Ok(());
        }
        let end = params.pic_w.min(xstart + params.slice_w);
        let row_start = y * frame.stride;
        let row = frame
            .data
            .get(row_start + xstart * 4..row_start + end * 4)
            .ok_or(DscError::FrameTooSmall)?;
        let real = (end - xstart).clamp(1, width);
        let (picture, padding) = line.split_at_mut(real);
        row_to_ycocg(kernels, row, frame.format == PixelFormat::Rgbx, picture);
        padding.fill(picture[real - 1]);
        Ok(())
    }

    /// Fill `near_spread` and `far_spread` for the line in `orig`. The steps
    /// are the same for every lane, so the loops run over the samples.
    #[inline(always)]
    fn scan_flatness(&mut self, params: &Params) {
        let width = params.slice_w * 4;
        // Lowest and highest of four pixels from every position on; the
        // six-pixel window is two of those, two pixels apart.
        let count = width + 3 * 4;
        let from = PAD_LEFT * 4;
        let samples = self.orig.as_flattened();
        let (Some(low), Some(high)) = (
            self.scan_low.as_flattened_mut().get_mut(..count),
            self.scan_high.as_flattened_mut().get_mut(..count),
        ) else {
            return;
        };
        let (Some(first), Some(second), Some(third), Some(fourth)) = (
            samples.get(from..from + count),
            samples.get(from + 4..from + 4 + count),
            samples.get(from + 8..from + 8 + count),
            samples.get(from + 12..from + 12 + count),
        ) else {
            return;
        };
        let windows = first
            .iter()
            .zip(second.iter())
            .zip(third.iter())
            .zip(fourth.iter());
        for ((low, high), (((a, b), c), d)) in low.iter_mut().zip(high.iter_mut()).zip(windows) {
            *low = (*a).min(*b).min((*c).min(*d));
            *high = (*a).max(*b).max((*c).max(*d));
        }
        let (Some(near), Some(far)) = (
            self.near_spread.as_flattened_mut().get_mut(..width),
            self.far_spread.as_flattened_mut().get_mut(..width),
        ) else {
            return;
        };
        for ((near, low), high) in near.iter_mut().zip(low.iter()).zip(high.iter()) {
            *near = *high - *low;
        }
        let lows = low[4..].iter().zip(low[12..].iter());
        let highs = high[4..].iter().zip(high[12..].iter());
        for (far, ((low_a, low_b), (high_a, high_b))) in far.iter_mut().zip(lows.zip(highs)) {
            *far = (*high_a).max(*high_b) - (*low_a).min(*low_b);
        }
    }

    /// Flatness class of the original pixels after position `h`: 0 not flat,
    /// 1 somewhat flat, 2 very flat.
    #[inline(always)]
    fn orig_flatness(&self, params: &Params, h: usize) -> i32 {
        if h + 1 >= params.slice_w {
            return 0;
        }
        let qp = (self.core.primary_qp - SOMEWHAT_FLAT_QP_DELTA).max(0);
        let luma_limit = FLATNESS_DET_THRESH.max(1 << qlevel(0, qp));
        let chroma_limit = FLATNESS_DET_THRESH.max(1 << qlevel(1, qp));
        for (window, spreads) in [&self.near_spread, &self.far_spread].iter().enumerate() {
            if window == 1 && h + 2 >= params.slice_w {
                return 0;
            }
            let [luma, orange, green, _] = pixels::<1>(spreads, h)[0].map(i32::from);
            let chroma = orange.max(green);
            if luma <= FLATNESS_DET_THRESH && chroma <= FLATNESS_DET_THRESH {
                return 2;
            }
            if luma <= luma_limit && chroma <= chroma_limit {
                return 1;
            }
        }
        0
    }

    fn encode_slice(
        &mut self,
        level: Level,
        params: &Params,
        frame: &Frame<'_>,
        job: &mut EncodeJob<'_>,
    ) -> Result<FrameStats, DscError> {
        match level {
            Level::Scalar => self.encode_slice_with(Scalar, params, frame, job),
            #[cfg(target_arch = "x86_64")]
            Level::Sse2 => self.encode_slice_with(x86::Sse2, params, frame, job),
            #[cfg(target_arch = "x86_64")]
            Level::Sse42(token) => {
                // SAFETY: the token proves that the CPU has every feature
                // `encode_slice_sse42` is compiled for (`x86::Sse42::detect`).
                unsafe { self.encode_slice_sse42(token, params, frame, job) }
            }
            #[cfg(target_arch = "x86_64")]
            Level::Avx2(token) => {
                // SAFETY: the token proves that the CPU has every feature
                // `encode_slice_avx2` is compiled for (`x86::Avx2::detect`).
                unsafe { self.encode_slice_avx2(token, params, frame, job) }
            }
        }
    }

    /// The slice loop compiled for SSE4.2-era CPUs (no AVX2): the automatic
    /// vector code and the lowering of the SSE2 kernels get absolute values,
    /// unsigned minima and blends as single instructions.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse4.2,ssse3,popcnt")]
    fn encode_slice_sse42(
        &mut self,
        token: x86::Sse42,
        params: &Params,
        frame: &Frame<'_>,
        job: &mut EncodeJob<'_>,
    ) -> Result<FrameStats, DscError> {
        self.encode_slice_with(token, params, frame, job)
    }

    /// The slice loop compiled for AVX2-era CPUs: shorter shifts and bit
    /// counts, wider automatic vectors, and the AVX2 kernels inlined.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    fn encode_slice_avx2(
        &mut self,
        token: x86::Avx2,
        params: &Params,
        frame: &Frame<'_>,
        job: &mut EncodeJob<'_>,
    ) -> Result<FrameStats, DscError> {
        self.encode_slice_with(token, params, frame, job)
    }

    #[inline(always)]
    fn encode_slice_with<K: Kernels>(
        &mut self,
        kernels: K,
        params: &Params,
        frame: &Frame<'_>,
        job: &mut EncodeJob<'_>,
    ) -> Result<FrameStats, DscError> {
        let mut stats = FrameStats::default();
        let quant = QuantLanes::<K::Lanes>::table();
        self.start(params);
        for line in 0..params.slice_h {
            self.populate_orig(kernels, params, frame, job.xstart, job.ystart + line)?;
            self.scan_flatness(params);
            let mut group_start = 0;
            while group_start < params.slice_w {
                let pixels = (params.slice_w - group_start).min(3);
                self.encode_group(kernels, params, &quant, group_start, pixels, &mut stats)?;
                group_start += 3;
            }
            self.core.end_line(params);
        }
        let leftover = (i64::from(params.xmit_delay) * i64::from(params.bpp)) >> 4;
        if self.core.buffer_fullness > leftover {
            return Err(DscError::RateControl(
                "too many bits left in the rate buffer at the slice end",
            ));
        }
        self.multiplex()?;
        for (chunk, source) in job
            .chunks
            .iter_mut()
            .zip(self.muxed.chunks_exact(params.chunk_bytes))
        {
            chunk.copy_from_slice(source);
        }
        Ok(stats)
    }

    #[inline(always)]
    fn encode_group<K: Kernels>(
        &mut self,
        kernels: K,
        params: &Params,
        quant: &[QuantLanes<K::Lanes>; 16],
        group_start: usize,
        pixels: usize,
        stats: &mut FrameStats,
    ) -> Result<(), DscError> {
        self.core.primary_qp = self.core.prev_qp;
        let qp = self.core.primary_qp;
        let prev_ich = self.core.ich_selected;
        self.core
            .ich_group_start(kernels, params, group_start, prev_ich);
        let mode = self.core.pred_mode(group_start);
        let pixels = pixels.clamp(1, 3);
        let last_h = group_start + pixels - 1;
        let qlevels = qp_levels(qp);
        let quant = &quant[index(qp) % 16];
        let originals: &[Pixel; 3] = self::pixels(&self.orig, group_start + PAD_LEFT);

        // Underflow prevention: force midpoint prediction when the rate
        // buffer is about to run dry.
        let max_bits_per_group = (3 * params.bpp + 15) >> 4;
        let chunk_bits = i32::try_from(params.chunk_bytes * 8).unwrap_or(i32::MAX);
        let fractional =
            (i64::from(params.bpp) * i64::try_from(params.slice_w).unwrap_or(0)) & 15 != 0;
        let chunk_end = self.core.num_bits_chunk + max_bits_per_group + 8;
        let low_water = i64::from(max_bits_per_group - 3);
        let force_mpp = if (fractional && chunk_end == chunk_bits) || chunk_end > chunk_bits {
            self.core.buffer_fullness - 8 < low_water
        } else {
            self.core.pixel_count >= params.xmit_delay && self.core.buffer_fullness < low_water
        };

        // Predictive coding with the regular predictor, the three components
        // side by side in lanes.
        let actual = originals.map(<K::Lanes as Lanes>::new);
        let pred = self.core.predictor(mode, group_start, quant);
        let (quantized, rebuilt) = code_units(pred, quant.scale, |position, prediction| {
            quant.quantize(actual[position].sub(prediction))
        });
        let errors = [
            actual[0].sub(rebuilt[0]).abs().get(),
            actual[1].sub(rebuilt[1]).abs().get(),
            actual[2].sub(rebuilt[2]).abs().get(),
        ];
        let quantized = quantized.map(Lanes::get);
        let rebuilt = rebuilt.map(Lanes::get);

        // Per component: residual sizes, and midpoint prediction where that
        // is what will be sent. Positions past the slice edge code a zero
        // residual and have no error.
        let live = [true, pixels > 1, pixels > 2];
        let pick = |position: usize, value: i32| if live[position] { value } else { 0 };
        let mut residuals = [[0i32; 3]; 3];
        let mut recon = [[0i16; 3]; 3];
        let mut max_error = [0i32; 3];
        let mut required = [[0i32; 3]; 3];
        let mut max_size = [0i32; 3];
        let mut midpoint = [false; 3];
        for unit in 0..3 {
            let level = qlevels[unit];
            let [mut q0, mut q1, mut q2] = [
                i32::from(quantized[0][unit]),
                pick(1, i32::from(quantized[1][unit])),
                pick(2, i32::from(quantized[2][unit])),
            ];
            let [mut r0, mut r1, mut r2] = [rebuilt[0][unit], rebuilt[1][unit], rebuilt[2][unit]];
            let mut error = i32::from(errors[0][unit])
                .max(pick(1, i32::from(errors[1][unit])))
                .max(pick(2, i32::from(errors[2][unit])));
            let mut sizes = [residual_size(q0), residual_size(q1), residual_size(q2)];
            let mut size = sizes[0].max(sizes[1]).max(sizes[2]);
            let limit = DEPTH[unit] - level;
            let use_midpoint = force_mpp || size >= limit;
            if use_midpoint {
                // Midpoint prediction; residuals are limited to `limit` bits.
                let max_value = (1 << DEPTH[unit]) - 1;
                let mid_pred = self.core.midpoint_pred(unit, level);
                let bound = 1 << (limit - 1).max(0);
                let code = |position: usize| {
                    let wanted = i32::from(originals[position][unit]);
                    let residual = clamp_c(quantize(wanted - mid_pred, level), -bound, bound - 1);
                    let value = clamp_c(mid_pred + (residual << level), 0, max_value);
                    (
                        pick(position, residual),
                        sample(value),
                        pick(position, (wanted - value).abs()),
                    )
                };
                let (first, second, third) = (code(0), code(1), code(2));
                [q0, q1, q2] = [first.0, second.0, third.0];
                [r0, r1, r2] = [first.1, second.1, third.1];
                error = first.2.max(second.2).max(third.2);
                size = limit;
                sizes = [limit; 3];
            }
            residuals[unit] = [q0, q1, q2];
            recon[unit] = [r0, r1, r2];
            max_error[unit] = error;
            required[unit] = sizes;
            max_size[unit] = size;
            midpoint[unit] = use_midpoint;
        }

        // Flatness decision for the next supergroup and QP adjustment.
        let group_in_supergroup = self.core.group_count % 4;
        let info_sent = params.flatness_info_sent(qp);
        if info_sent && group_in_supergroup == 3 {
            let mut prev_is_flat = self.core.first_flat >= 0;
            self.core.prev_first_flat = -1;
            for ahead in 0..4i32 {
                let flatness = self.orig_flatness(params, last_h + (index(ahead) + 1) * 3);
                if !prev_is_flat && flatness != 0 {
                    self.core.prev_first_flat = ahead;
                    self.prev_flatness_type = flatness - 1;
                    break;
                }
                prev_is_flat = flatness != 0;
            }
        } else if !info_sent && group_in_supergroup == 3 {
            self.core.prev_first_flat = -1;
        } else if group_in_supergroup == 0 {
            self.core.first_flat = self.core.prev_first_flat;
            self.core.flatness_type = self.prev_flatness_type;
        }
        self.core.orig_is_flat =
            self.core.first_flat >= 0 && group_in_supergroup == self.core.first_flat;
        self.core.flatness_qp_adjust(params, last_h);

        // Cost of the predictive coding, which the colour history has to beat.
        let adjusted = [
            self.core.adjusted_predicted_size(0),
            self.core.adjusted_predicted_size(1),
            self.core.adjusted_predicted_size(2),
        ];
        let escape_size = DEPTH[0] + 1 - qlevels[0];
        let mut ich_bits = if prev_ich {
            1
        } else {
            escape_size - adjusted[0]
        };
        ich_bits += ICH_BITS * 3;
        let mut log_err_p = 0;
        let mut p_bits = 0;
        for unit in 0..3 {
            log_err_p += bit_length(max_error[unit]);
            if !params.v2 && unit == 0 {
                log_err_p *= 2;
            }
            let limit = DEPTH[unit] - qlevels[unit];
            p_bits += if max_size[unit] < adjusted[unit] {
                1 + 3 * adjusted[unit]
            } else if max_size[unit] == limit && unit != 0 {
                max_size[unit] - adjusted[unit] + 3 * max_size[unit]
            } else {
                1 + max_size[unit] - adjusted[unit] + 3 * max_size[unit]
            };
        }
        if max_size[0] < DEPTH[0] - qlevels[0] && prev_ich {
            p_bits += 1;
        }
        let p_cost = p_bits + ICH_LAMBDA * log_err_p;

        // Indexed colour history search. Its cost is at least `ich_bits`, so
        // a group that predicts for less is not searched at all.
        let mut ich_possible =
            !force_mpp && !(group_start == 0 && self.core.v_pos == 0) && ich_bits < p_cost;
        let mut ich_lookup = [0usize; 3];
        let mut ich_recon = [[0i16; 3]; 3];
        let mut max_ich_error = [0i32; 3];
        if ich_possible {
            let core = &self.core;
            // On the first line all 32 entries are history. From the second
            // line on the history has 25 entries and entries 25..=31 are the
            // seven pixels above the group, searched where they are in the
            // previous line.
            let above_base = Core::ich_above_base(params, group_start);
            let above: IchRows<'_> = [
                window(&core.planes[0], above_base),
                window(&core.planes[1], above_base),
                window(&core.planes[2], above_base),
            ];
            let (history, above_valid) = if core.v_pos > 0 {
                (
                    low_bits(core.ich_valid.min(ICH_RESERVED)),
                    low_bits(ICH_ABOVE),
                )
            } else {
                (low_bits(core.ich_valid), 0)
            };
            let [low, high] = ich_halves(&core.ich);
            let modified_qp = (qp + 2).min(MAX_QP);
            let tolerance: [i16; 3] =
                std::array::from_fn(|unit| sample((1 << qlevel(unit, modified_qp)) / 2));
            for position in 0..pixels {
                let [y, co, cg, _] = originals[position];
                let pixel = [y, co, cg];
                // The first entry within the tolerance, in entry order:
                // history, then the pixels above.
                let hit =
                    ich_table_first(kernels, &core.ich, pixel, tolerance, history).or_else(|| {
                        kernels
                            .ich_first(above, pixel, tolerance, above_valid)
                            .map(|entry| entry + ICH_RESERVED)
                    });
                let Some(mut best) = hit else {
                    ich_possible = false;
                    break;
                };
                // The entry to use is the first with the lowest weighted
                // difference; an exact first hit is that entry already.
                let mut entry = core.ich_pixel(params, best, group_start);
                if ich_weighted_sad(entry, pixel) != 0 {
                    let mut key = kernels
                        .ich_best(low, pixel, history, 0)
                        .min(kernels.ich_best(high, pixel, history >> ICH_ROW, ICH_ROW));
                    if above_valid != 0 {
                        key = key.min(kernels.ich_best(above, pixel, above_valid, ICH_RESERVED));
                    }
                    best = usize::try_from(key).unwrap_or(0) % ICH_SIZE;
                    entry = core.ich_pixel(params, best, group_start);
                }
                ich_lookup[position] = best;
                for unit in 0..3 {
                    ich_recon[unit][position] = sample(entry[unit]);
                    max_ich_error[unit] =
                        max_ich_error[unit].max((entry[unit] - i32::from(pixel[unit])).abs());
                }
            }
            let last_lookup = ich_lookup[pixels - 1];
            for lookup in ich_lookup.iter_mut().skip(pixels) {
                *lookup = last_lookup;
            }
        }
        let mut use_ich = false;
        if ich_possible {
            let mut log_err_ich = 0;
            for (unit, error) in max_ich_error.iter().enumerate() {
                log_err_ich += bit_length(*error);
                if !params.v2 && unit == 0 {
                    log_err_ich *= 2;
                }
            }
            let ich_cost = ich_bits + ICH_LAMBDA * log_err_ich;
            use_ich = ich_cost < p_cost
                && (log_err_ich <= log_err_p
                    || (params.v2 && self.orig_flatness(params, group_start + 2) != 2));
        }

        // Entropy coding.
        let mut bits = [0i32; 3];
        if group_in_supergroup == 3 && info_sent {
            self.substreams[0].put(i32::from(self.core.prev_first_flat >= 0), 1);
            bits[0] += 1;
        }
        if group_in_supergroup == 0 && self.core.first_flat >= 0 {
            if qp >= SOMEWHAT_FLAT_QP_THRESH {
                self.substreams[0].put(self.core.flatness_type, 1);
                bits[0] += 1;
            } else {
                self.core.flatness_type = 0;
            }
            self.substreams[0].put(self.core.first_flat, 2);
            bits[0] += 2;
        }
        let core = &mut self.core;
        if use_ich {
            if prev_ich {
                self.substreams[0].put(1, 1);
                bits[0] += 1;
            } else {
                let zeros = escape_size - adjusted[0];
                self.substreams[0].put(0, zeros);
                bits[0] += zeros;
            }
            for unit in 0..3 {
                self.substreams[unit].put(i32::try_from(ich_lookup[unit]).unwrap_or(0), ICH_BITS);
                bits[unit] += ICH_BITS;
            }
            core.rc_size_unit = [ICH_BITS * 3 + 1, 0, 0];
            core.midpoint = [false; 3];
            stats.ich_groups += 1;
            stats.flat_groups += u64::from(max_ich_error == [0; 3]);
        } else {
            for unit in 0..3 {
                let luma = i32::from(unit == 0);
                let mut prefix = (max_size[unit] - adjusted[unit]).max(0);
                let size = max_size[unit].max(adjusted[unit]);
                if unit == 0 && prev_ich {
                    prefix += 1;
                }
                let max_prefix = DEPTH[unit] - qlevels[unit] + luma - adjusted[unit];
                let writer = &mut self.substreams[unit];
                // `prefix` zero bits, then a one bit unless the prefix has
                // its greatest length.
                let terminated = i32::from(prefix != max_prefix);
                let prefix_bits = prefix + terminated;
                match (u32::try_from(prefix_bits), u32::try_from(size)) {
                    // The prefix and the three residuals in one write.
                    (Ok(head @ 0..=16), Ok(width @ 0..=10)) => {
                        let packed = (u64::from(terminated != 0) << (3 * width))
                            | u64::from(pack_residuals(residuals[unit], width));
                        writer.put_raw(packed, head + 3 * width);
                    }
                    (Ok(_) | Err(_), Ok(_) | Err(_)) => {
                        writer.put(terminated, prefix_bits);
                        for value in residuals[unit] {
                            writer.put(value, size);
                        }
                    }
                }
                bits[unit] += prefix_bits + 3 * size;
                core.rc_size_unit[unit] = max_size[unit] * 3 + 1;
                core.predicted_size[unit] =
                    (2 + required[unit][0] + required[unit][1] + 2 * required[unit][2]) >> 2;
            }
            core.midpoint = midpoint;
            if core.rc_size_unit == [1, 1, 1] {
                stats.zero_residual_groups += 1;
                stats.flat_groups += u64::from(max_error == [0; 3]);
            }
        }
        core.ich_selected = use_ich;
        let size_byte = |size: i32| u32::try_from(size).map_or(255, |size| size.min(255));
        self.se_sizes
            .push(size_byte(bits[0]) | (size_byte(bits[1]) << 8) | (size_byte(bits[2]) << 16));
        let coded = bits[0] + bits[1] + bits[2];
        core.buffer_fullness += i64::from(coded);
        core.coded_group_size = coded;
        core.prev_primary_qp = core.primary_qp;
        stats.groups += 1;
        stats.qp_sum += u64::try_from(qp).unwrap_or(0);
        stats.stuffed_groups += u64::from(force_mpp);

        // Final reconstruction of the group.
        let chosen = if use_ich { &ich_recon } else { &recon };
        let group: [Pixel; 3] = std::array::from_fn(|position| {
            [
                chosen[0][position],
                chosen[1][position],
                chosen[2][position],
                0,
            ]
        });
        core.finish_group(params, group_start, pixels, &group);
        if core.model_overflow {
            return Err(DscError::RateControl("the rate-control model overflowed"));
        }
        if core.buffer_overflow {
            return Err(DscError::RateControl("the rate buffer overflowed"));
        }
        if core.buffer_fullness < 0 {
            return Err(DscError::RateControl("the rate buffer underflowed"));
        }
        Ok(())
    }

    /// Substream multiplexing: emit 48-bit mux words in the order a decoder's
    /// three funnel shifters will request them.
    fn multiplex(&mut self) -> Result<(), DscError> {
        let mut fullness = [0i32; 3];
        let mut read = [0usize; 3];
        let mut position = 0usize;
        for sizes in &self.se_sizes {
            for stream in 0..3 {
                if fullness[stream] < MAX_SE_SIZE {
                    let target = self
                        .muxed
                        .get_mut(position..position + MUX_WORD_BYTES)
                        .ok_or(DscError::RateControl(
                            "the slice does not fit its constant-rate budget",
                        ))?;
                    let bytes = self.substreams[stream].written();
                    let start = read[stream];
                    if let Some(source) = bytes.get(start..start + MUX_WORD_BYTES) {
                        target.copy_from_slice(source);
                    } else {
                        // The last word of a substream is padded with zeros.
                        for (offset, byte) in target.iter_mut().enumerate() {
                            *byte = bytes.get(start + offset).copied().unwrap_or(0);
                        }
                    }
                    read[stream] += MUX_WORD_BYTES;
                    position += MUX_WORD_BYTES;
                    fullness[stream] += MUX_WORD_BITS;
                }
                fullness[stream] -= i32::from(sizes.to_le_bytes()[stream]);
            }
        }
        self.muxed[position..].fill(0);
        Ok(())
    }
}

/// The decoder's substream demultiplexer: three funnel shifters fed with
/// 48-bit mux words.
#[derive(Clone)]
struct Demux<'a> {
    data: &'a [u8],
    position: usize,
    shifter: [u128; 3],
    fullness: [i32; 3],
    consumed: i32,
}

impl Demux<'_> {
    #[inline(always)]
    fn refill(&mut self) {
        for stream in 0..3 {
            if self.fullness[stream] < MAX_SE_SIZE {
                let whole = self
                    .data
                    .get(self.position..)
                    .and_then(|rest| rest.first_chunk::<MUX_WORD_BYTES>());
                let word = if let Some([a, b, c, d, e, f]) = whole {
                    u64::from_be_bytes([0, 0, *a, *b, *c, *d, *e, *f])
                } else {
                    // Past the end of the slice the words read as zeros.
                    let mut word = 0u64;
                    for offset in 0..MUX_WORD_BYTES {
                        let byte = self.data.get(self.position + offset).copied().unwrap_or(0);
                        word = (word << 8) | u64::from(byte);
                    }
                    word
                };
                self.position += MUX_WORD_BYTES;
                self.shifter[stream] = (self.shifter[stream] << MUX_WORD_BITS) | u128::from(word);
                self.fullness[stream] += MUX_WORD_BITS;
            }
        }
    }

    /// The low 64 bits of the funnel shifter above its lowest `below` bits.
    #[inline(always)]
    fn peek(&self, stream: usize, below: i32) -> u64 {
        let shifted = self.shifter[stream] >> below.clamp(0, 127);
        u64::try_from(shifted & u128::from(u64::MAX)).unwrap_or(0)
    }

    #[inline(always)]
    fn get(&mut self, stream: usize, bits: i32) -> i32 {
        if bits <= 0 {
            return 0;
        }
        self.consumed += bits;
        if self.fullness[stream] < bits {
            self.fullness[stream] = 0;
            return 0;
        }
        self.fullness[stream] -= bits;
        let value = (self.shifter[stream] >> self.fullness[stream]) & ((1u128 << bits) - 1);
        i32::try_from(value).unwrap_or(0)
    }

    #[inline(always)]
    fn get_signed(&mut self, stream: usize, bits: i32) -> i32 {
        let value = self.get(stream, bits);
        if bits > 0 && value >> (bits - 1) != 0 {
            value - (1 << bits)
        } else {
            value
        }
    }

    /// Three `bits`-wide signed values, as three [`Demux::get_signed`] calls
    /// read them.
    #[inline(always)]
    fn get_signed3(&mut self, stream: usize, bits: i32) -> [i32; 3] {
        if bits <= 0 {
            return [0; 3];
        }
        if bits > 10 || self.fullness[stream] < 3 * bits {
            return [
                self.get_signed(stream, bits),
                self.get_signed(stream, bits),
                self.get_signed(stream, bits),
            ];
        }
        self.consumed += 3 * bits;
        self.fullness[stream] -= 3 * bits;
        let all = self.peek(stream, self.fullness[stream]);
        let mask = (1u64 << bits) - 1;
        let sign = 1i32 << (bits - 1);
        let value = |shift: i32| {
            let raw = i32::try_from((all >> shift) & mask).unwrap_or(0);
            (raw ^ sign) - sign
        };
        [value(2 * bits), value(bits), value(0)]
    }

    /// The unary size prefix: the number of zero bits before a one bit, at
    /// most `max_prefix` (then no one bit follows). Reads what bit-by-bit
    /// [`Demux::get`] calls would read.
    #[inline(always)]
    fn prefix(&mut self, stream: usize, max_prefix: i32) -> i32 {
        if max_prefix <= 0 {
            return 0;
        }
        if max_prefix > 32 {
            let mut prefix = 0;
            while prefix < max_prefix && self.get(stream, 1) == 0 {
                prefix += 1;
            }
            return prefix;
        }
        let available = self.fullness[stream].max(0);
        let looked = max_prefix.min(available);
        let bits = self.peek(stream, available - looked) & ((1u64 << looked) - 1);
        if bits == 0 {
            // No one bit in reach: an exhausted shifter reads as zeros.
            self.consumed += max_prefix;
            self.fullness[stream] = available - looked;
            return max_prefix;
        }
        let length = i32::try_from(u64::BITS - bits.leading_zeros()).unwrap_or(0);
        let zeros = looked - length;
        self.consumed += zeros + 1;
        self.fullness[stream] = available - zeros - 1;
        zeros
    }
}

/// One slice's share of the decoded picture: the bitstream of its slice row,
/// its position in that row and its segment of every picture row.
struct DecodeJob<'a> {
    band: &'a [u8],
    slice: usize,
    rows: Vec<&'a mut [u8]>,
}

/// Everything one thread needs to decode slices; kept between pictures.
struct SliceDecoder {
    core: Core,
    data: Vec<u8>,
}

impl SliceDecoder {
    fn new(params: &Params) -> Self {
        Self {
            core: Core::new(params),
            data: Vec::with_capacity(params.slice_bytes()),
        }
    }

    fn decode_slice(
        &mut self,
        level: Level,
        params: &Params,
        job: &mut DecodeJob<'_>,
        format: PixelFormat,
    ) {
        match level {
            Level::Scalar => self.decode_slice_with(Scalar, params, job, format),
            #[cfg(target_arch = "x86_64")]
            Level::Sse2 => self.decode_slice_with(x86::Sse2, params, job, format),
            #[cfg(target_arch = "x86_64")]
            Level::Sse42(token) => {
                // SAFETY: the token proves that the CPU has every feature
                // `decode_slice_sse42` is compiled for (`x86::Sse42::detect`).
                unsafe { self.decode_slice_sse42(token, params, job, format) }
            }
            #[cfg(target_arch = "x86_64")]
            Level::Avx2(token) => {
                // SAFETY: the token proves that the CPU has every feature
                // `decode_slice_avx2` is compiled for (`x86::Avx2::detect`).
                unsafe { self.decode_slice_avx2(token, params, job, format) }
            }
        }
    }

    /// The slice loop compiled for SSE4.2-era CPUs, see
    /// [`SliceEncoder::encode_slice_sse42`].
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse4.2,ssse3,popcnt")]
    fn decode_slice_sse42(
        &mut self,
        token: x86::Sse42,
        params: &Params,
        job: &mut DecodeJob<'_>,
        format: PixelFormat,
    ) {
        self.decode_slice_with(token, params, job, format);
    }

    /// The slice loop compiled for AVX2-era CPUs, see
    /// [`SliceEncoder::encode_slice_avx2`].
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,popcnt")]
    fn decode_slice_avx2(
        &mut self,
        token: x86::Avx2,
        params: &Params,
        job: &mut DecodeJob<'_>,
        format: PixelFormat,
    ) {
        self.decode_slice_with(token, params, job, format);
    }

    #[inline(always)]
    fn decode_slice_with<K: Kernels>(
        &mut self,
        kernels: K,
        params: &Params,
        job: &mut DecodeJob<'_>,
        format: PixelFormat,
    ) {
        if !self.core.fits(params) {
            *self = Self::new(params);
        }
        // The slice's chunks are interleaved with those of its row.
        self.data.clear();
        let chunks = job
            .band
            .chunks(params.chunk_bytes.max(1))
            .skip(job.slice)
            .step_by(params.slices_per_line.max(1));
        for chunk in chunks {
            self.data.extend_from_slice(chunk);
        }
        self.core.reset(params);
        let quant = QuantLanes::<K::Lanes>::table();
        let mut demux = Demux {
            data: &self.data,
            position: 0,
            shifter: [0; 3],
            fullness: [0; 3],
            consumed: 0,
        };
        let red_first = format == PixelFormat::Rgbx;
        for line in 0..params.slice_h {
            let mut group_start = 0;
            while group_start < params.slice_w {
                let count = (params.slice_w - group_start).min(3);
                Self::decode_group(
                    &mut self.core,
                    kernels,
                    params,
                    &quant,
                    &mut demux,
                    group_start,
                    count,
                );
                group_start += 3;
            }
            if let Some(row) = job.rows.get_mut(line) {
                let count = (row.len() / 4).min(params.slice_w);
                if let Some(decoded) = self.core.cur.get(PAD_LEFT..PAD_LEFT + count) {
                    row_to_rgb(kernels, decoded, red_first, row);
                }
            }
            self.core.end_line(params);
        }
    }

    #[inline(always)]
    fn decode_group<K: Kernels>(
        core: &mut Core,
        kernels: K,
        params: &Params,
        quant: &[QuantLanes<K::Lanes>; 16],
        demux: &mut Demux<'_>,
        group_start: usize,
        count: usize,
    ) {
        core.primary_qp = core.prev_qp;
        let qp = core.primary_qp;
        let prev_ich = core.ich_selected;
        core.ich_group_start(kernels, params, group_start, prev_ich);
        let count = count.clamp(1, 3);

        // Entropy decoding.
        demux.refill();
        let consumed_before = demux.consumed;
        core.ich_selected = false;
        let group_in_supergroup = core.group_count % 4;
        let mut residuals = [[0i32; 3]; 3];
        let mut ich_lookup = [0usize; 3];
        for unit in 0..3 {
            let level = qlevel(unit, qp);
            let adjusted = core.adjusted_predicted_size(unit);
            if unit == 0 {
                if group_in_supergroup == 3 {
                    core.prev_first_flat = if params.flatness_info_sent(qp) && demux.get(0, 1) != 0
                    {
                        0
                    } else {
                        -1
                    };
                }
                if group_in_supergroup == 0 {
                    if core.prev_first_flat >= 0 {
                        core.flatness_type = 0;
                        if qp >= SOMEWHAT_FLAT_QP_THRESH {
                            core.flatness_type = demux.get(0, 1);
                        }
                        core.first_flat = demux.get(0, 2);
                    } else {
                        core.first_flat = -1;
                    }
                }
            }
            if core.ich_selected {
                ich_lookup[unit] = index(demux.get(unit, ICH_BITS));
                continue;
            }
            let limit = DEPTH[unit] - level;
            let max_prefix = limit + i32::from(unit == 0) - adjusted;
            let prefix = demux.prefix(unit, max_prefix);
            let size = if unit == 0 && prev_ich {
                adjusted + prefix - 1
            } else {
                adjusted + prefix
            };
            if unit == 0 {
                let use_ich = if prev_ich { prefix == 0 } else { size > limit };
                if use_ich {
                    core.ich_selected = true;
                    core.rc_size_unit = [ICH_BITS * 3 + 1, 0, 0];
                    ich_lookup[0] = index(demux.get(0, ICH_BITS));
                    continue;
                }
            }
            let size = size.clamp(0, limit);
            let midpoint = size == limit;
            core.midpoint[unit] = midpoint;
            residuals[unit] = demux.get_signed3(unit, size);
            let mut required = residuals[unit].map(residual_size);
            let mut max_size = required[0].max(required[1]).max(required[2]);
            if midpoint {
                max_size = size;
                required = [size; 3];
            }
            core.rc_size_unit[unit] = max_size * 3 + 1;
            core.predicted_size[unit] = (2 + required[0] + required[1] + 2 * required[2]) >> 2;
        }
        core.prev_primary_qp = core.primary_qp;
        core.coded_group_size = demux.consumed - consumed_before;
        core.buffer_fullness += i64::from(core.coded_group_size);
        core.orig_is_flat = group_in_supergroup == core.first_flat;

        // Reconstruction.
        let mut group = [[0i16; 4]; 3];
        if core.ich_selected {
            for (pixel, lookup) in group.iter_mut().zip(ich_lookup).take(count) {
                let [y, co, cg] = core.ich_pixel(params, lookup, group_start);
                *pixel = [sample(y), sample(co), sample(cg), 0];
            }
        } else {
            // The three components side by side in lanes; a component with
            // midpoint prediction is then redone on its own.
            let quant = &quant[index(qp) % 16];
            let mode = core.pred_mode(group_start);
            let pred = core.predictor::<K::Lanes>(mode, group_start, quant);
            let read: [K::Lanes; 3] = std::array::from_fn(|position| {
                Lanes::new([
                    sample(residuals[0][position]),
                    sample(residuals[1][position]),
                    sample(residuals[2][position]),
                    0,
                ])
            });
            let (_, rebuilt) = code_units(pred, quant.scale, |position, _| read[position % 3]);
            group = rebuilt.map(Lanes::get);
            for unit in 0..3 {
                if core.midpoint[unit] {
                    let level = qlevel(unit, qp);
                    let max_value = (1 << DEPTH[unit]) - 1;
                    let mid_pred = core.midpoint_pred(unit, level);
                    for (pixel, residual) in group.iter_mut().zip(residuals[unit]) {
                        pixel[unit] = sample(clamp_c(mid_pred + (residual << level), 0, max_value));
                    }
                }
            }
        }
        core.flatness_qp_adjust(params, group_start + count - 1);
        core.finish_group(params, group_start, count, &group);
    }
}

/// Whether a buffer of `len` bytes holds the picture in rows `stride` apart.
fn check_frame(params: &Params, len: usize, stride: usize) -> Result<(), DscError> {
    let row = params.pic_w * 4;
    let needed = params
        .pic_h
        .saturating_sub(1)
        .checked_mul(stride)
        .and_then(|rows| rows.checked_add(row));
    match needed {
        Some(needed) if stride >= row && len >= needed => Ok(()),
        Some(_) | None => Err(DscError::FrameTooSmall),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A borrowed `Fn() + Sync` closure with its type and lifetime erased, so
/// that pool threads, which outlive any one call, can run it.
#[derive(Clone, Copy)]
struct TaskRef {
    data: *const (),
    call: unsafe fn(*const ()),
}

// SAFETY: `data` points to a closure that is `Sync` (`TaskRef::new`), so
// calling it through a shared reference from another thread is sound while
// the closure is alive; `Pool::run` keeps it alive (see there).
unsafe impl Send for TaskRef {}

impl TaskRef {
    fn new<F: Fn() + Sync>(task: &F) -> Self {
        /// # Safety
        /// `data` must point to a live `F`.
        unsafe fn call<F: Fn() + Sync>(data: *const ()) {
            // SAFETY: the caller guarantees that `data` points to a live `F`.
            let task = unsafe { &*data.cast::<F>() };
            task();
        }
        Self {
            data: std::ptr::from_ref(task).cast(),
            call: call::<F>,
        }
    }
}

/// One `Pool::run` call that pool threads may still join.
struct Batch {
    id: u64,
    task: TaskRef,
    /// Pool threads that may still join.
    wanted: usize,
    /// Pool threads inside the task right now.
    active: usize,
    panicked: bool,
}

#[derive(Default)]
struct PoolState {
    batches: Vec<Batch>,
    next_id: u64,
    shutdown: bool,
}

#[derive(Default)]
struct PoolShared {
    state: Mutex<PoolState>,
    /// Signalled when a batch arrives or the pool shuts down.
    work: Condvar,
    /// Signalled when a pool thread leaves a task.
    done: Condvar,
}

/// Persistent worker threads for the slices of a picture. Threads start the
/// first time they are needed and sleep between pictures; dropping the pool
/// stops and joins them.
struct Pool {
    shared: Arc<PoolShared>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    /// Most threads the pool will ever start.
    limit: usize,
}

impl Pool {
    fn new(limit: usize) -> Self {
        Self {
            shared: Arc::new(PoolShared::default()),
            workers: Mutex::new(Vec::new()),
            limit,
        }
    }

    /// The pool every encoder and decoder of the process shares: one thread
    /// per available core, counting the calling thread. It lives as long as
    /// any of its users.
    fn shared() -> Arc<Self> {
        static SHARED: Mutex<Weak<Pool>> = Mutex::new(Weak::new());
        let mut slot = lock(&SHARED);
        if let Some(pool) = slot.upgrade() {
            return pool;
        }
        let cores = thread::available_parallelism().map_or(1, usize::from);
        let pool = Arc::new(Self::new(cores.saturating_sub(1)));
        *slot = Arc::downgrade(&pool);
        pool
    }

    /// Make sure `count` threads exist (fewer if the system refuses to start
    /// one) and return how many do.
    fn grow(&self, count: usize) -> usize {
        let mut workers = lock(&self.workers);
        while workers.len() < count.min(self.limit) {
            let shared = Arc::clone(&self.shared);
            let spawned = thread::Builder::new()
                .name("dsc-slice".to_owned())
                .spawn(move || Self::work(&shared));
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
        workers.len()
    }

    /// Body of a pool thread: join batches until the pool shuts down.
    fn work(shared: &PoolShared) {
        let mut state = lock(&shared.state);
        loop {
            if state.shutdown {
                return;
            }
            let mut claimed = None;
            for batch in &mut state.batches {
                if batch.wanted > 0 {
                    batch.wanted -= 1;
                    batch.active += 1;
                    claimed = Some((batch.id, batch.task));
                    break;
                }
            }
            let Some((id, task)) = claimed else {
                state = shared
                    .work
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            };
            drop(state);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: this thread is counted in the batch's `active`, and
                // `Pool::run` does not return or unwind (`BatchGuard`) before
                // `active` is zero again, so the closure `task` points to is
                // alive for the whole call.
                unsafe { (task.call)(task.data) }
            }));
            state = lock(&shared.state);
            for batch in &mut state.batches {
                if batch.id == id {
                    batch.active -= 1;
                    batch.panicked |= outcome.is_err();
                }
            }
            shared.done.notify_all();
        }
    }

    /// Run `task` on the calling thread and on up to `helpers` pool threads
    /// at once, and return when all of them have left it. `task` is expected
    /// to take jobs from a shared queue until none are left. An error means a
    /// pool thread panicked inside `task`.
    fn run<F: Fn() + Sync>(&self, helpers: usize, task: &F) -> Result<(), DscError> {
        let helpers = helpers.min(self.grow(helpers));
        if helpers == 0 {
            task();
            return Ok(());
        }
        // The guard exists before the batch is visible to pool threads, so
        // however this function is left, it first waits for them.
        let guard = {
            let mut state = lock(&self.shared.state);
            state.next_id += 1;
            let guard = BatchGuard {
                shared: &self.shared,
                id: state.next_id,
            };
            state.batches.push(Batch {
                id: guard.id,
                task: TaskRef::new(task),
                wanted: helpers,
                active: 0,
                panicked: false,
            });
            guard
        };
        self.shared.work.notify_all();
        task();
        if guard.close() {
            return Err(DscError::Worker);
        }
        Ok(())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        lock(&self.shared.state).shutdown = true;
        self.shared.work.notify_all();
        for handle in lock(&self.workers).drain(..) {
            // A pool thread only ends by returning from `Pool::work`.
            drop(handle.join());
        }
    }
}

/// Withdraws a batch from the pool and waits for the pool threads inside it.
struct BatchGuard<'a> {
    shared: &'a PoolShared,
    id: u64,
}

impl BatchGuard<'_> {
    /// Stop pool threads from joining the batch, wait until those inside
    /// have left, and report whether one of them panicked. Later calls
    /// report `false`.
    fn close(&self) -> bool {
        let mut state = lock(&self.shared.state);
        loop {
            let Some(at) = state.batches.iter().position(|batch| batch.id == self.id) else {
                return false;
            };
            state.batches[at].wanted = 0;
            if state.batches[at].active == 0 {
                return state.batches.swap_remove(at).panicked;
            }
            state = self
                .shared
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for BatchGuard<'_> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Per-thread coder states that are idle between pictures.
struct Spares<W>(Mutex<Vec<W>>);

impl<W> Spares<W> {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    fn take(&self) -> Option<W> {
        lock(&self.0).pop()
    }

    fn give(&self, worker: W) {
        lock(&self.0).push(worker);
    }
}

/// Run `work` over `jobs` on the calling thread and up to `threads - 1` pool
/// threads, each with a coder state from `spares` (or a new one), and merge
/// the results. Threads take the next job when they finish one, so slices
/// that are quick to code do not leave a thread idle. With one thread the
/// jobs run in order on the calling thread.
fn run_jobs<J, W, R>(
    pool: &Pool,
    spares: &Spares<W>,
    threads: usize,
    jobs: Vec<J>,
    make_worker: impl Fn() -> W + Sync,
    work: impl Fn(&mut W, &mut J) -> Result<R, DscError> + Sync,
    mut merge: impl FnMut(R),
) -> Result<(), DscError>
where
    J: Send,
    W: Send,
    R: Send,
{
    let count = jobs.len();
    let helpers = threads.clamp(1, count.max(1)) - 1;
    let queue = Mutex::new(jobs.into_iter().enumerate());
    let results: Mutex<Vec<R>> = Mutex::new(Vec::with_capacity(count));
    // The failed job with the lowest number, as a single thread reports it.
    let failure: Mutex<Option<(usize, DscError)>> = Mutex::new(None);
    let failed = AtomicBool::new(false);
    let task = || {
        // A thread that arrives after the last job was taken needs no state.
        let mut worker: Option<W> = None;
        while !failed.load(Ordering::Relaxed) {
            let next = lock(&queue).next();
            let Some((number, mut job)) = next else {
                break;
            };
            let state = worker.get_or_insert_with(|| spares.take().unwrap_or_else(&make_worker));
            match work(state, &mut job) {
                Ok(result) => lock(&results).push(result),
                Err(error) => {
                    failed.store(true, Ordering::Relaxed);
                    let mut failure = lock(&failure);
                    if failure.as_ref().is_none_or(|(first, _)| number < *first) {
                        *failure = Some((number, error));
                    }
                }
            }
        }
        if let Some(worker) = worker {
            spares.give(worker);
        }
    };
    pool.run(helpers, &task)?;
    if let Some((_, error)) = failure.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(error);
    }
    for result in results.into_inner().unwrap_or_else(PoisonError::into_inner) {
        merge(result);
    }
    Ok(())
}

/// Fixed-rate DSC encoder for one picture size. It keeps its working buffers
/// between pictures, and all encoders and decoders of a process share one
/// pool of worker threads that stops when the last of them is dropped.
pub struct Encoder {
    config: Config,
    pps: Pps,
    params: Params,
    level: Level,
    pool: Arc<Pool>,
    spares: Spares<SliceEncoder>,
}

impl Clone for Encoder {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            pps: self.pps.clone(),
            params: self.params.clone(),
            level: self.level,
            pool: Arc::clone(&self.pool),
            spares: Spares::new(),
        }
    }
}

impl fmt::Debug for Encoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Encoder")
            .field("config", &self.config)
            .field("pps", &self.pps)
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, DscError> {
        let pps = Pps::from_config(&config)?;
        let params = Params::from_pps(&pps)?;
        Ok(Self {
            config,
            pps,
            params,
            level: Level::detect(),
            pool: Pool::shared(),
            spares: Spares::new(),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Parameters the decoder needs for pictures encoded from now on.
    pub fn pps(&self) -> &Pps {
        &self.pps
    }

    /// Exact length in bytes of every picture encoded at the current rate.
    pub fn frame_len(&self) -> usize {
        self.pps.frame_len()
    }

    /// Change the rate (in 1/16 bit per pixel) for the following pictures.
    /// The picture parameter set changes with it; on error nothing changes.
    pub fn set_bits_per_pixel_x16(&mut self, bits_per_pixel_x16: u16) -> Result<(), DscError> {
        let config = Config {
            bits_per_pixel_x16,
            ..self.config.clone()
        };
        let pps = Pps::from_config(&config)?;
        let params = Params::from_pps(&pps)?;
        self.config = config;
        self.pps = pps;
        self.params = params;
        Ok(())
    }

    /// Encode one picture on the calling thread. `out` is resized to
    /// [`Encoder::frame_len`].
    pub fn encode(&self, frame: &Frame<'_>, out: &mut Vec<u8>) -> Result<FrameStats, DscError> {
        self.encode_parallel(frame, out, 1)
    }

    /// Encode one picture with its slices spread over the calling thread and
    /// up to `threads - 1` threads of the shared pool (never more threads
    /// than cores). The output is identical to [`Encoder::encode`].
    pub fn encode_parallel(
        &self,
        frame: &Frame<'_>,
        out: &mut Vec<u8>,
        threads: usize,
    ) -> Result<FrameStats, DscError> {
        let params = &self.params;
        check_frame(params, frame.data.len(), frame.stride)?;
        // Every byte is written below, so a buffer of the right length is
        // reused as it is.
        out.resize(self.frame_len(), 0);
        let row_bytes = params.slice_bytes() * params.slices_per_line;
        let mut jobs = Vec::with_capacity(params.slices_per_line * params.slice_rows);
        for (row, band) in out.chunks_mut(row_bytes).enumerate() {
            let mut slices: Vec<EncodeJob<'_>> = (0..params.slices_per_line)
                .map(|slice| EncodeJob {
                    xstart: slice * params.slice_w,
                    ystart: row * params.slice_h,
                    chunks: Vec::with_capacity(params.slice_h),
                })
                .collect();
            for (number, chunk) in band.chunks_mut(params.chunk_bytes).enumerate() {
                slices[number % params.slices_per_line].chunks.push(chunk);
            }
            jobs.extend(slices);
        }
        let level = self.level;
        let mut stats = FrameStats::default();
        run_jobs(
            &self.pool,
            &self.spares,
            threads,
            jobs,
            || SliceEncoder::new(params),
            |worker, job| worker.encode_slice(level, params, frame, job),
            |slice_stats| stats.add(&slice_stats),
        )?;
        Ok(stats)
    }
}

/// DSC decoder for one picture parameter set. Like [`Encoder`] it keeps its
/// working buffers between pictures and uses the shared pool of threads.
pub struct Decoder {
    pps: Pps,
    params: Params,
    level: Level,
    pool: Arc<Pool>,
    spares: Spares<SliceDecoder>,
}

impl Clone for Decoder {
    fn clone(&self) -> Self {
        Self {
            pps: self.pps.clone(),
            params: self.params.clone(),
            level: self.level,
            pool: Arc::clone(&self.pool),
            spares: Spares::new(),
        }
    }
}

impl fmt::Debug for Decoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Decoder")
            .field("pps", &self.pps)
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

impl Decoder {
    pub fn new(pps: Pps) -> Result<Self, DscError> {
        let params = Params::from_pps(&pps)?;
        Ok(Self {
            pps,
            params,
            level: Level::detect(),
            pool: Pool::shared(),
            spares: Spares::new(),
        })
    }

    pub fn pps(&self) -> &Pps {
        &self.pps
    }

    /// Length in bytes of one encoded picture.
    pub fn frame_len(&self) -> usize {
        self.pps.frame_len()
    }

    /// Decode one picture into `out` (rows of 4-byte pixels, `stride` apart).
    pub fn decode(
        &self,
        bitstream: &[u8],
        out: &mut [u8],
        stride: usize,
        format: PixelFormat,
    ) -> Result<(), DscError> {
        self.decode_parallel(bitstream, out, stride, format, 1)
    }

    /// Decode one picture with its slices spread over the calling thread and
    /// up to `threads - 1` threads of the shared pool (never more threads
    /// than cores).
    pub fn decode_parallel(
        &self,
        bitstream: &[u8],
        out: &mut [u8],
        stride: usize,
        format: PixelFormat,
        threads: usize,
    ) -> Result<(), DscError> {
        let params = &self.params;
        check_frame(params, out.len(), stride)?;
        if bitstream.len() != self.frame_len() {
            return Err(DscError::BitstreamLength {
                got: bitstream.len(),
                want: self.frame_len(),
            });
        }
        let row_bytes = params.slice_bytes() * params.slices_per_line;
        let mut jobs = Vec::with_capacity(params.slices_per_line * params.slice_rows);
        let bands = out.chunks_mut(stride.saturating_mul(params.slice_h));
        for (band, data) in bands.zip(bitstream.chunks(row_bytes)) {
            let mut slices: Vec<DecodeJob<'_>> = (0..params.slices_per_line)
                .map(|slice| DecodeJob {
                    band: data,
                    slice,
                    rows: Vec::with_capacity(params.slice_h),
                })
                .collect();
            for row in band.chunks_mut(stride) {
                let Some(visible) = row.get_mut(..params.pic_w * 4) else {
                    continue;
                };
                for (slice, segment) in visible.chunks_mut(params.slice_w * 4).enumerate() {
                    slices[slice].rows.push(segment);
                }
            }
            jobs.extend(slices);
        }
        let level = self.level;
        run_jobs(
            &self.pool,
            &self.spares,
            threads,
            jobs,
            || SliceDecoder::new(params),
            |worker, job| {
                worker.decode_slice(level, params, job, format);
                Ok(())
            },
            |()| {},
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BitWriter, Config, DEFAULT_BPP_X16, Decoder, Demux, DscError, Encoder, Frame, FrameStats,
        Kernels, Lanes, Level, MAX_BPP_X16, MIN_BPP_X16, PPS_SIZE, Pixel, PixelFormat, Pool,
        PortableLanes, Pps, QuantLanes, Scalar, lock, qlevel, quantize, row_to_rgb, row_to_ycocg,
    };
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::Instant;

    type TestResult = Result<(), Box<dyn Error>>;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() >> 33).unwrap_or(0) % bound.max(1)
        }

        fn byte(&mut self) -> u8 {
            self.next().to_le_bytes()[3]
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Pattern {
        Flat,
        Text,
        Gradient,
        Noise,
        Ui,
    }

    const PATTERNS: [Pattern; 5] = [
        Pattern::Flat,
        Pattern::Text,
        Pattern::Gradient,
        Pattern::Noise,
        Pattern::Ui,
    ];

    fn byte(value: usize) -> u8 {
        u8::try_from(value.min(255)).unwrap_or(u8::MAX)
    }

    fn put(image: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3]) {
        let at = (y * width + x) * 4;
        if let Some(pixel) = image.get_mut(at..at + 4) {
            pixel.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0xA5]);
        }
    }

    fn fill(image: &mut [u8], width: usize, rect: [usize; 4], rgb: [u8; 3]) {
        let height = image.len() / (width * 4);
        for y in rect[1]..rect[3].min(height) {
            for x in rect[0]..rect[2].min(width) {
                put(image, width, x, y, rgb);
            }
        }
    }

    /// Window chrome made of flat fills: panels, a title bar, buttons.
    fn draw_flat(image: &mut [u8], width: usize, rect: [usize; 4], rng: &mut Rng) {
        fill(image, width, rect, [239, 240, 241]);
        let [x0, y0, x1, y1] = rect;
        fill(image, width, [x0, y0, x1, (y0 + 28).min(y1)], [49, 54, 59]);
        fill(
            image,
            width,
            [x0, y0 + 28, (x0 + 180).min(x1), y1],
            [252, 252, 252],
        );
        let palette = [
            [61, 174, 233],
            [218, 68, 83],
            [39, 174, 96],
            [246, 116, 0],
            [255, 255, 255],
            [35, 38, 41],
        ];
        let spans = [(x1 - x0).max(2), (y1 - y0).max(2)];
        for _ in 0..(spans[0] * spans[1] / 9000).max(4) {
            let x = x0 + rng.below(spans[0]);
            let y = y0 + rng.below(spans[1]);
            let w = 12 + rng.below(140);
            let h = 6 + rng.below(40);
            let colour = palette[rng.below(palette.len())];
            fill(
                image,
                width,
                [x, y, (x + w).min(x1), (y + h).min(y1)],
                colour,
            );
        }
    }

    /// Rows of 5x9 pseudo-glyphs with one-pixel strokes and a few grey
    /// anti-aliasing shades, dark on light and light on dark.
    fn draw_text(image: &mut [u8], width: usize, rect: [usize; 4], rng: &mut Rng) {
        let [x0, y0, x1, y1] = rect;
        let middle = y0 + (y1 - y0) / 2;
        fill(image, width, [x0, y0, x1, middle], [255, 255, 255]);
        fill(image, width, [x0, middle, x1, y1], [30, 31, 41]);
        let mut y = y0 + 4;
        while y + 12 < y1 {
            let dark_on_light = y + 12 < middle;
            let (ink, soft) = if dark_on_light {
                ([0u8, 0, 0], [[96u8, 96, 96], [160, 160, 160]])
            } else {
                ([248u8, 248, 242], [[139u8, 141, 150], [80, 82, 95]])
            };
            let skip = y < middle && y + 12 >= middle;
            let mut x = x0 + 6;
            while x + 7 < x1 && !skip {
                if rng.below(6) != 0 {
                    for gy in 0..9 {
                        let row = rng.next();
                        for gx in 0..5 {
                            match (row >> (gx * 3)) & 7 {
                                0..=2 => put(image, width, x + gx, y + gy, ink),
                                3 => put(image, width, x + gx, y + gy, soft[gy % 2]),
                                _ => {}
                            }
                        }
                    }
                }
                x += 7;
            }
            y += 14;
        }
    }

    fn draw_gradient(image: &mut [u8], width: usize, rect: [usize; 4]) {
        let [x0, y0, x1, y1] = rect;
        let (w, h) = (x1.saturating_sub(x0).max(2), y1.saturating_sub(y0).max(2));
        for y in y0..y1 {
            for x in x0..x1 {
                let (dx, dy) = (x - x0, y - y0);
                let rgb = [
                    byte(dx * 255 / (w - 1)),
                    byte(dy * 255 / (h - 1)),
                    byte((dx + dy) * 255 / (w + h - 2)),
                ];
                put(image, width, x, y, rgb);
            }
        }
    }

    fn draw_noise(image: &mut [u8], width: usize, rect: [usize; 4], rng: &mut Rng) {
        for y in rect[1]..rect[3] {
            for x in rect[0]..rect[2] {
                put(image, width, x, y, [rng.byte(), rng.byte(), rng.byte()]);
            }
        }
    }

    /// A BGRx test picture (the padding byte is deliberately not zero).
    fn picture(pattern: Pattern, width: usize, height: usize) -> Vec<u8> {
        let mut image = vec![0u8; width * height * 4];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ u64::try_from(width * 31 + height).unwrap_or(1));
        let all = [0, 0, width, height];
        match pattern {
            Pattern::Flat => draw_flat(&mut image, width, all, &mut rng),
            Pattern::Text => draw_text(&mut image, width, all, &mut rng),
            Pattern::Gradient => draw_gradient(&mut image, width, all),
            Pattern::Noise => draw_noise(&mut image, width, all, &mut rng),
            Pattern::Ui => {
                let (half_w, half_h) = (width / 2, height / 2);
                draw_flat(&mut image, width, all, &mut rng);
                draw_text(
                    &mut image,
                    width,
                    [half_w / 4, 40, half_w, half_h + half_h / 2],
                    &mut rng,
                );
                draw_gradient(&mut image, width, [half_w + 8, 40, width - 8, half_h]);
                let thumb = [half_w + 8, half_h + 8, half_w + 8 + half_w / 3, height - 8];
                draw_noise(&mut image, width, thumb, &mut rng);
            }
        }
        image
    }

    struct Quality {
        psnr_db: f64,
        max_error: u8,
    }

    fn quality(original: &[u8], decoded: &[u8]) -> Quality {
        let mut squared = 0u64;
        let mut samples = 0u32;
        let mut max_error = 0u8;
        for (a, b) in original.chunks_exact(4).zip(decoded.chunks_exact(4)) {
            for channel in 0..3 {
                let error = a[channel].abs_diff(b[channel]);
                max_error = max_error.max(error);
                squared += u64::from(error) * u64::from(error);
                samples += 1;
            }
        }
        let squared =
            f64::from(u32::try_from(squared.min(u64::from(u32::MAX))).unwrap_or(u32::MAX));
        let mse = squared / f64::from(samples.max(1));
        let psnr_db = if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        };
        Quality { psnr_db, max_error }
    }

    struct Roundtrip {
        quality: Quality,
        stats: FrameStats,
        bytes: usize,
    }

    fn roundtrip(config: &Config, image: &[u8]) -> Result<Roundtrip, DscError> {
        let width = usize::try_from(config.width).unwrap_or(0);
        let encoder = Encoder::new(config.clone())?;
        let frame = Frame {
            data: image,
            stride: width * 4,
            format: PixelFormat::Bgrx,
        };
        let mut bitstream = Vec::new();
        let stats = encoder.encode(&frame, &mut bitstream)?;
        assert_eq!(bitstream.len(), encoder.frame_len());
        let mut parallel = Vec::new();
        let parallel_stats = encoder.encode_parallel(&frame, &mut parallel, 4)?;
        assert_eq!(
            bitstream, parallel,
            "parallel slices must give the same bitstream"
        );
        assert_eq!(stats, parallel_stats);

        let pps = Pps::from_bytes(&encoder.pps().to_bytes())?;
        assert_eq!(&pps, encoder.pps());
        let decoder = Decoder::new(pps)?;
        let mut decoded = vec![0u8; image.len()];
        decoder.decode(&bitstream, &mut decoded, width * 4, PixelFormat::Bgrx)?;
        let mut decoded_parallel = vec![0u8; image.len()];
        decoder.decode_parallel(
            &bitstream,
            &mut decoded_parallel,
            width * 4,
            PixelFormat::Bgrx,
            4,
        )?;
        assert_eq!(decoded, decoded_parallel);
        Ok(Roundtrip {
            quality: quality(image, &decoded),
            stats,
            bytes: bitstream.len(),
        })
    }

    fn report(name: &str, config: &Config, pattern: Pattern, result: &Roundtrip) {
        println!(
            "{name} {}x{} {:?} bpp={}/16: {} bytes, PSNR {:.2} dB, max error {}, mean QP {}/16, flat {} permille, stuffing {} permille",
            config.width,
            config.height,
            pattern,
            config.bits_per_pixel_x16,
            result.bytes,
            result.quality.psnr_db,
            result.quality.max_error,
            result.stats.mean_qp_x16(),
            result.stats.flatness_permille(),
            result.stats.stuffing_permille(),
        );
    }

    fn size(value: u32) -> usize {
        usize::try_from(value).unwrap_or(0)
    }

    #[test]
    fn roundtrip_1080p_is_fixed_rate_and_accurate() -> TestResult {
        let config = Config::new(1920, 1080)?;
        assert_eq!((config.slice_width, config.slice_height), (480, 108));
        let expected = 1920 * 1080;
        for pattern in PATTERNS {
            let image = picture(pattern, 1920, 1080);
            let result = roundtrip(&config, &image)?;
            report("1080p", &config, pattern, &result);
            assert_eq!(result.bytes, expected, "8 bpp is one byte per pixel");
            let (min_psnr, max_error) = match pattern {
                Pattern::Flat => (80.0, 2),
                Pattern::Text => (80.0, 3),
                Pattern::Gradient => (75.0, 2),
                Pattern::Ui => (33.0, 255),
                Pattern::Noise => (19.0, 255),
            };
            assert!(result.quality.psnr_db >= min_psnr, "{pattern:?} PSNR");
            assert!(
                result.quality.max_error <= max_error,
                "{pattern:?} max error"
            );
        }
        Ok(())
    }

    #[test]
    fn roundtrip_odd_sizes() -> TestResult {
        for (width, height) in [(1366u32, 768u32), (641, 479), (333, 217), (101, 67)] {
            let config = Config::new(width, height)?;
            let mut lengths = Vec::new();
            for pattern in PATTERNS {
                let image = picture(pattern, size(width), size(height));
                let result = roundtrip(&config, &image)?;
                report("odd", &config, pattern, &result);
                lengths.push(result.bytes);
                if pattern == Pattern::Flat {
                    assert!(result.quality.max_error <= 3, "flat max error");
                    assert!(result.quality.psnr_db >= 70.0, "flat PSNR");
                }
            }
            lengths.dedup();
            assert_eq!(
                lengths.len(),
                1,
                "every picture of one size has the same length"
            );
        }
        Ok(())
    }

    #[test]
    fn rate_is_settable_in_sixteenths_and_length_follows() -> TestResult {
        let (width, height) = (640u32, 360u32);
        let mut encoder = Encoder::new(Config::new(width, height)?)?;
        let images = [
            picture(Pattern::Ui, 640, 360),
            picture(Pattern::Noise, 640, 360),
        ];
        let mut previous_psnr = 0.0;
        for bpp in [
            MIN_BPP_X16,
            104,
            119,
            DEFAULT_BPP_X16,
            137,
            160,
            171,
            MAX_BPP_X16,
        ] {
            encoder.set_bits_per_pixel_x16(bpp)?;
            let slice_width = size(encoder.config().slice_width);
            let chunk = (slice_width * usize::from(bpp)).div_ceil(128);
            let slices = 640usize.div_ceil(slice_width);
            let slice_height = size(encoder.config().slice_height);
            let expected = chunk * slices * slice_height * 360usize.div_ceil(slice_height);
            assert_eq!(encoder.frame_len(), expected);
            for image in &images {
                let result = roundtrip(encoder.config(), image)?;
                assert_eq!(result.bytes, expected);
            }
            let result = roundtrip(encoder.config(), &images[0])?;
            report("rate", encoder.config(), Pattern::Ui, &result);
            assert!(
                result.quality.psnr_db + 1.5 >= previous_psnr,
                "quality grows with the rate"
            );
            previous_psnr = result.quality.psnr_db;
        }
        assert!(encoder.set_bits_per_pixel_x16(MIN_BPP_X16 - 1).is_err());
        assert!(encoder.set_bits_per_pixel_x16(MAX_BPP_X16 + 1).is_err());
        assert_eq!(encoder.config().bits_per_pixel_x16, MAX_BPP_X16);
        Ok(())
    }

    #[test]
    fn stats_separate_flat_from_busy_content() -> TestResult {
        let config = Config::new(640, 360)?;
        let flat = roundtrip(&config, &picture(Pattern::Flat, 640, 360))?;
        let noise = roundtrip(&config, &picture(Pattern::Noise, 640, 360))?;
        for stats in [&flat.stats, &noise.stats] {
            println!(
                "stats: complexity {} flat {} stuffing {} permille",
                stats.complexity_permille(),
                stats.flatness_permille(),
                stats.stuffing_permille()
            );
        }
        assert!(flat.stats.complexity_permille() < 100);
        assert!(flat.stats.flatness_permille() > 500);
        assert!(flat.stats.stuffing_permille() > noise.stats.stuffing_permille());
        assert!(noise.stats.complexity_permille() > 400);
        assert!(noise.stats.flatness_permille() < 100);
        assert_eq!(flat.stats.groups, noise.stats.groups);
        Ok(())
    }

    #[test]
    fn dsc_1_1_mode_and_options_roundtrip() -> TestResult {
        let image = picture(Pattern::Ui, 640, 360);
        for (version_minor, block_pred_enable, linebuf_depth) in [
            (1u8, true, 9u8),
            (1, false, 8),
            (2, false, 9),
            (2, true, 8),
            (2, true, 16),
        ] {
            let config = Config {
                version_minor,
                block_pred_enable,
                linebuf_depth,
                ..Config::new(640, 360)?
            };
            let result = roundtrip(&config, &image)?;
            report("options", &config, Pattern::Ui, &result);
            assert!(result.quality.psnr_db >= 30.0);
        }
        Ok(())
    }

    #[test]
    fn rgbx_input_and_strided_buffers() -> TestResult {
        let (width, height) = (320usize, 120usize);
        let bgrx = picture(Pattern::Ui, width, height);
        let stride = width * 4 + 12;
        let mut rgbx = vec![0x5Au8; stride * height];
        for (row, source) in rgbx
            .chunks_exact_mut(stride)
            .zip(bgrx.chunks_exact(width * 4))
        {
            for (target, pixel) in row.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
                target.copy_from_slice(&[pixel[2], pixel[1], pixel[0], 0x11]);
            }
        }
        let encoder = Encoder::new(Config::new(320, 120)?)?;
        let mut from_bgrx = Vec::new();
        encoder.encode(
            &Frame {
                data: &bgrx,
                stride: width * 4,
                format: PixelFormat::Bgrx,
            },
            &mut from_bgrx,
        )?;
        let mut from_rgbx = Vec::new();
        encoder.encode(
            &Frame {
                data: &rgbx,
                stride,
                format: PixelFormat::Rgbx,
            },
            &mut from_rgbx,
        )?;
        assert_eq!(
            from_bgrx, from_rgbx,
            "byte order and stride do not change the bitstream"
        );

        let decoder = Decoder::new(encoder.pps().clone())?;
        let mut out = vec![0u8; stride * height];
        decoder.decode(&from_rgbx, &mut out, stride, PixelFormat::Rgbx)?;
        let mut plain = vec![0u8; width * height * 4];
        decoder.decode(&from_bgrx, &mut plain, width * 4, PixelFormat::Bgrx)?;
        for (row, expected) in out.chunks_exact(stride).zip(plain.chunks_exact(width * 4)) {
            for (pixel, wanted) in row.chunks_exact(4).zip(expected.chunks_exact(4)) {
                assert_eq!(pixel, [wanted[2], wanted[1], wanted[0], 255]);
            }
        }
        Ok(())
    }

    #[test]
    fn bad_input_is_rejected() -> TestResult {
        let encoder = Encoder::new(Config::new(320, 120)?)?;
        let short = vec![0u8; 320 * 119 * 4];
        let mut out = Vec::new();
        let frame = Frame {
            data: &short,
            stride: 320 * 4,
            format: PixelFormat::Bgrx,
        };
        assert_eq!(
            encoder.encode(&frame, &mut out),
            Err(DscError::FrameTooSmall)
        );
        let decoder = Decoder::new(encoder.pps().clone())?;
        let mut picture = vec![0u8; 320 * 120 * 4];
        let wrong = vec![0u8; encoder.frame_len() - 1];
        assert!(matches!(
            decoder.decode(&wrong, &mut picture, 320 * 4, PixelFormat::Bgrx),
            Err(DscError::BitstreamLength { .. })
        ));
        // A corrupt bitstream decodes to some picture without failing.
        let mut rng = Rng(7);
        let garbage: Vec<u8> = (0..encoder.frame_len()).map(|_| rng.byte()).collect();
        decoder.decode(&garbage, &mut picture, 320 * 4, PixelFormat::Bgrx)?;
        assert!(Config::new(0, 10).is_err());
        assert!(Config::new(6, 100).is_err(), "too narrow for a slice");
        let mut pps = encoder.pps().to_bytes();
        pps[3] = 0xA9;
        assert!(matches!(
            Pps::from_bytes(&pps),
            Err(DscError::UnsupportedPps(_))
        ));
        assert!(Pps::from_bytes(&pps[..40]).is_err());
        Ok(())
    }

    /// The 1080p default at 8 bpp against the values the C reference model
    /// derives (`rc_8bpc_8bpp.cfg`, SLICE_WIDTH 480, SLICE_HEIGHT 108).
    #[test]
    fn pps_matches_reference_model_for_1080p() -> TestResult {
        let pps = Pps::from_config(&Config::new(1920, 1080)?)?;
        // Bytes 4..98 of the .dsc file the C model writes for this setup
        // (rc_8bpc_8bpp.cfg, SLICE_WIDTH 480, SLICE_HEIGHT 108, LINE_BUFFER_BPC 9).
        let reference = concat!(
            "12000089308004380780006c01e001e00200022c002008d90006000f0120",
            "0110180010f0030c2000060b0b330e1c2a38465462697077797b7d7e0102",
            "0100094009be19fc19fa19f81a381a781ab62ab62af42af44b3463740000",
            "00000000",
        );
        let bytes = pps.to_bytes();
        let hex: String = bytes[..94]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(hex, reference);
        assert_eq!(bytes[94..], [0u8; PPS_SIZE - 94]);
        assert_eq!((pps.chunk_size, pps.initial_dec_delay), (480, 556));
        let ranges: Vec<(u8, u8, i8)> = pps
            .rc_range_parameters
            .iter()
            .map(|range| (range.min_qp, range.max_qp, range.bpg_offset))
            .collect();
        // rc_parameters_1_2_444, 8 bpp / 8 bpc, in Linux drm_dsc_helper.c.
        let linux = [
            (0, 4, 2),
            (0, 4, 0),
            (1, 5, 0),
            (1, 6, -2),
            (3, 7, -4),
            (3, 7, -6),
            (3, 7, -8),
            (3, 8, -8),
            (3, 9, -8),
            (3, 10, -10),
            (5, 10, -10),
            (5, 11, -12),
            (5, 11, -12),
            (9, 12, -12),
            (12, 13, -12),
        ];
        assert_eq!(ranges, linux);
        Ok(())
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xCBF2_9CE4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    struct Vector {
        name: &'static str,
        pattern: Pattern,
        width: u32,
        height: u32,
        slice_width: u32,
        slice_height: u32,
        bpp_x16: u16,
        version_minor: u8,
        block_pred_enable: bool,
        linebuf_depth: u8,
        session: Option<&'static str>,
    }

    /// Real 1920x1080 session screenshots in this repository.
    const SESSION_FRAMES: [(&str, &str); 2] = [
        ("session_before", "docs/screenshot-metadata-365/before.png"),
        (
            "session_terminal",
            "docs/verification/issue476/terminal-fast-after-return.png",
        ),
    ];

    /// Decode a repository PNG screenshot into BGRx.
    fn session_frame(path: &str) -> Result<Vec<u8>, Box<dyn Error>> {
        let file = std::fs::File::open(format!("{}/{path}", env!("CARGO_MANIFEST_DIR")))?;
        let mut reader = png::Decoder::new(file).read_info()?;
        let mut pixels = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels)?;
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        pixels.truncate(info.buffer_size());
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Ok(pixels)
    }

    #[test]
    fn roundtrip_real_session_frames() -> TestResult {
        let config = Config::new(1920, 1080)?;
        for (name, path) in SESSION_FRAMES {
            let image = session_frame(path)?;
            let result = roundtrip(&config, &image)?;
            report(name, &config, Pattern::Ui, &result);
            assert_eq!(result.bytes, 1920 * 1080);
            assert!(result.quality.psnr_db >= 40.0, "{name} PSNR");
        }
        Ok(())
    }

    impl Vector {
        fn config(&self) -> Config {
            Config {
                width: self.width,
                height: self.height,
                slice_width: self.slice_width,
                slice_height: self.slice_height,
                bits_per_pixel_x16: self.bpp_x16,
                block_pred_enable: self.block_pred_enable,
                version_minor: self.version_minor,
                linebuf_depth: self.linebuf_depth,
            }
        }

        fn encode(&self) -> Result<(Vec<u8>, Vec<u8>, Encoder), Box<dyn Error>> {
            let image = match self.session {
                Some(path) => session_frame(path)?,
                None => picture(self.pattern, size(self.width), size(self.height)),
            };
            let encoder = Encoder::new(self.config())?;
            let frame = Frame {
                data: &image,
                stride: size(self.width) * 4,
                format: PixelFormat::Bgrx,
            };
            let mut bitstream = Vec::new();
            encoder.encode(&frame, &mut bitstream)?;
            Ok((image, bitstream, encoder))
        }
    }

    /// Name, pattern, [width, height, slice width, slice height], rate in
    /// 1/16 bpp, DSC minor version, block prediction, line buffer depth.
    type Row = (&'static str, Pattern, [u32; 4], u16, u8, bool, u8);

    /// Configurations compared byte for byte with the C reference model.
    const ROWS: [Row; 22] = [
        (
            "ui_v2_8bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            128,
            2,
            true,
            9,
        ),
        (
            "ui_v1_8bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            128,
            1,
            true,
            9,
        ),
        (
            "text_v2_8bpp",
            Pattern::Text,
            [640, 360, 160, 120],
            128,
            2,
            true,
            9,
        ),
        (
            "text_v1_8bpp",
            Pattern::Text,
            [640, 360, 160, 120],
            128,
            1,
            true,
            9,
        ),
        (
            "flat_v2_8bpp",
            Pattern::Flat,
            [640, 360, 320, 180],
            128,
            2,
            true,
            9,
        ),
        (
            "flat_v1_8bpp",
            Pattern::Flat,
            [640, 360, 320, 180],
            128,
            1,
            true,
            9,
        ),
        (
            "gradient_v2_8bpp",
            Pattern::Gradient,
            [640, 360, 640, 360],
            128,
            2,
            true,
            9,
        ),
        (
            "gradient_v1_8bpp",
            Pattern::Gradient,
            [640, 360, 640, 360],
            128,
            1,
            true,
            9,
        ),
        (
            "noise_v2_8bpp",
            Pattern::Noise,
            [640, 360, 320, 90],
            128,
            2,
            true,
            9,
        ),
        (
            "noise_v1_8bpp",
            Pattern::Noise,
            [640, 360, 320, 90],
            128,
            1,
            true,
            9,
        ),
        (
            "ui_v2_6bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            96,
            2,
            true,
            9,
        ),
        (
            "noise_v2_6bpp",
            Pattern::Noise,
            [640, 360, 320, 90],
            96,
            2,
            true,
            9,
        ),
        (
            "ui_v2_10bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            160,
            2,
            true,
            9,
        ),
        (
            "ui_v2_12bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            192,
            2,
            true,
            9,
        ),
        (
            "noise_v1_12bpp",
            Pattern::Noise,
            [640, 360, 320, 90],
            192,
            1,
            true,
            9,
        ),
        (
            "ui_v2_7_4375bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            119,
            2,
            true,
            9,
        ),
        (
            "ui_v2_9_0625bpp",
            Pattern::Ui,
            [640, 360, 320, 90],
            145,
            2,
            true,
            9,
        ),
        (
            "noise_v2_10_6875bpp",
            Pattern::Noise,
            [640, 360, 320, 90],
            171,
            2,
            true,
            9,
        ),
        (
            "ui_v2_nobp_lb8",
            Pattern::Ui,
            [640, 360, 320, 90],
            128,
            2,
            false,
            8,
        ),
        (
            "ui_v1_lb8",
            Pattern::Ui,
            [640, 360, 320, 90],
            128,
            1,
            true,
            8,
        ),
        (
            "ui_v2_odd",
            Pattern::Ui,
            [641, 479, 161, 100],
            128,
            2,
            true,
            9,
        ),
        (
            "text_v2_odd",
            Pattern::Text,
            [333, 217, 167, 108],
            128,
            2,
            true,
            9,
        ),
    ];

    /// FNV-1a of each row's bitstream, recorded from runs in which the
    /// bitstream and the decoded picture were identical to the output of the
    /// VESA C reference model v1.67 (DSC_model_20211213) on the same input.
    const REFERENCE_HASHES: [u64; 24] = [
        0xa162_24c0_aea6_69bf,
        0x7b89_aeb7_12a0_b22c,
        0x5f66_d79f_af9b_bf51,
        0x3c5d_5137_3127_c79c,
        0x7e52_b3fd_8c27_7702,
        0x23a2_0d57_829f_ca72,
        0x18fc_9037_a541_6df7,
        0xc3cf_aedc_3b8d_f35e,
        0xa1c6_d82e_5397_9297,
        0x45d2_de47_43d1_6466,
        0x8b3e_85b1_d02e_1adf,
        0x6418_1691_dd64_3c3a,
        0xd23e_e7a5_3bc0_4468,
        0x7b89_e2c2_6473_ac04,
        0xe4f2_e9b2_dc4c_58cf,
        0x0acf_a1d7_daf8_a36d,
        0x82ba_8e21_13d4_f86c,
        0xff34_6866_7305_8fb6,
        0x40c2_03ff_35f8_c6c5,
        0x6209_d180_3522_eb5f,
        0xbaac_d888_595d_79d1,
        0x8ca9_ee47_efa4_2a56,
        0x7fa0_bd06_ce22_ab6f,
        0x5c47_4b4d_c37d_3374,
    ];

    fn vectors() -> Vec<Vector> {
        ROWS.iter()
            .map(|row| Vector {
                name: row.0,
                pattern: row.1,
                width: row.2[0],
                height: row.2[1],
                slice_width: row.2[2],
                slice_height: row.2[3],
                bpp_x16: row.3,
                version_minor: row.4,
                block_pred_enable: row.5,
                linebuf_depth: row.6,
                session: None,
            })
            .chain(SESSION_FRAMES.iter().map(|(name, path)| Vector {
                name,
                pattern: Pattern::Ui,
                width: 1920,
                height: 1080,
                slice_width: 480,
                slice_height: 108,
                bpp_x16: DEFAULT_BPP_X16,
                version_minor: 2,
                block_pred_enable: true,
                linebuf_depth: 9,
                session: Some(path),
            }))
            .collect()
    }

    #[test]
    fn bitstreams_match_reference_model() -> TestResult {
        let all = vectors();
        assert_eq!(all.len(), REFERENCE_HASHES.len());
        for (item, expected) in all.iter().zip(REFERENCE_HASHES) {
            let (_, bitstream, _) = item.encode()?;
            assert_eq!(
                fnv1a(&bitstream),
                expected,
                "{} differs from the C model",
                item.name
            );
        }
        Ok(())
    }

    /// Writes every vector as a PPM picture, this encoder's `.dsc` file (the C
    /// model's container: "DSCF", the PPS, the picture), this decoder's output
    /// and a C model configuration, so the reference model can be run on the
    /// same input: `KWIN_DSC_DUMP_DIR=dir KWIN_DSC_RC_DIR=model-dir cargo test
    /// --lib dsc_dump_vectors -- --ignored`.
    #[test]
    #[ignore = "writes comparison files for the C reference model"]
    fn dsc_dump_vectors() -> TestResult {
        let directory = std::env::var("KWIN_DSC_DUMP_DIR")?;
        let rc_directory = std::env::var("KWIN_DSC_RC_DIR")?;
        for item in &vectors() {
            let (image, bitstream, encoder) = item.encode()?;
            let (width, height) = (size(item.width), size(item.height));
            let ppm = |pixels: &[u8]| {
                let mut file = format!("P6\n{width} {height}\n255\n").into_bytes();
                for pixel in pixels.chunks_exact(4) {
                    file.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                }
                file
            };
            std::fs::write(format!("{directory}/{}.ppm", item.name), ppm(&image))?;
            let mut file = b"DSCF".to_vec();
            file.extend_from_slice(&encoder.pps().to_bytes());
            file.extend_from_slice(&bitstream);
            std::fs::write(format!("{directory}/{}.rust.dsc", item.name), file)?;
            let decoder = Decoder::new(encoder.pps().clone())?;
            let mut decoded = vec![0u8; image.len()];
            decoder.decode(&bitstream, &mut decoded, width * 4, PixelFormat::Bgrx)?;
            std::fs::write(format!("{directory}/{}.rust.ppm", item.name), ppm(&decoded))?;
            std::fs::write(
                format!("{directory}/{}.list", item.name),
                format!("{directory}/{}.ppm\n", item.name),
            )?;
            let rate = if item.bpp_x16.is_multiple_of(32) {
                format!(
                    "INCLUDE {rc_directory}/rc_8bpc_{}bpp.cfg\n",
                    item.bpp_x16 / 16
                )
            } else {
                let fraction = u32::from(item.bpp_x16 % 16) * 625;
                format!(
                    "GENERATE_RC_PARAMETERS 2\nBITS_PER_COMPONENT 8\nBITS_PER_PIXEL {}.{fraction:04}\n",
                    item.bpp_x16 / 16
                )
            };
            let cfg = format!(
                "DSC_VERSION_MINOR {}\nFUNCTION 0\nDSC_FILE_OUTPUT 1\nPPM_FILE_OUTPUT 1\nDPX_FILE_OUTPUT 0\nSRC_LIST {directory}/{}.list\nOUT_DIR {directory}/c\nSLICE_WIDTH {}\nSLICE_HEIGHT {}\nBLOCK_PRED_ENABLE {}\nVBR_ENABLE 0\nLINE_BUFFER_BPC {}\nUSE_YUV_INPUT 0\n{rate}",
                item.version_minor,
                item.name,
                item.slice_width,
                item.slice_height,
                u8::from(item.block_pred_enable),
                item.linebuf_depth,
            );
            std::fs::write(format!("{directory}/{}.cfg", item.name), cfg)?;
            println!("{} {:016x}", item.name, fnv1a(&bitstream));
        }
        Ok(())
    }

    /// Every code level this CPU can run, by the name `KWIN_DSC_SIMD` uses.
    fn levels() -> Vec<(&'static str, Level)> {
        #[cfg(target_arch = "x86_64")]
        let simd = {
            let mut levels = vec![("sse2", Level::Sse2)];
            if let Some(token) = super::x86::Sse42::detect() {
                levels.push(("sse4.2", Level::Sse42(token)));
            }
            if let Some(token) = super::x86::Avx2::detect() {
                levels.push(("avx2", Level::Avx2(token)));
            }
            levels
        };
        #[cfg(not(target_arch = "x86_64"))]
        let simd = Vec::new();
        std::iter::once(("scalar", Level::Scalar))
            .chain(simd)
            .collect()
    }

    fn load_average() -> String {
        std::fs::read_to_string("/proc/loadavg")
            .map_or_else(|_| "unknown".to_owned(), |text| text.trim().to_owned())
    }

    /// Prints encode and decode time per 1920x1080 and 1280x720 picture, on
    /// one thread and on the pool, for every code level of this CPU: `cargo
    /// test --release --lib dsc_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark; run in release mode"]
    fn dsc_timing() -> TestResult {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        println!(
            "8 bpp, best of several runs, {threads} threads for the pooled runs, load average {}",
            load_average()
        );
        let time = |runs: u32, work: &mut dyn FnMut() -> Result<(), DscError>| {
            let mut best = f64::INFINITY;
            for _ in 0..runs {
                let start = Instant::now();
                work()?;
                best = best.min(start.elapsed().as_secs_f64() * 1000.0);
            }
            Ok::<f64, DscError>(best)
        };
        for (name, level) in levels() {
            for (width, height) in [(1920u32, 1080u32), (1280, 720)] {
                let mut encoder = Encoder::new(Config::new(width, height)?)?;
                encoder.level = level;
                let mut decoder = Decoder::new(encoder.pps().clone())?;
                decoder.level = level;
                let pps = encoder.pps();
                println!(
                    "{name} {width}x{height}: {} slices of {}x{}",
                    pps.slices_per_line() * pps.slice_rows(),
                    pps.slice_width,
                    pps.slice_height
                );
                let stride = size(width) * 4;
                for pattern in PATTERNS {
                    let image = picture(pattern, size(width), size(height));
                    let frame = Frame {
                        data: &image,
                        stride,
                        format: PixelFormat::Bgrx,
                    };
                    let mut bitstream = Vec::new();
                    let mut decoded = vec![0u8; image.len()];
                    let encode_single = time(5, &mut || {
                        encoder.encode(&frame, &mut bitstream).map(|_| ())
                    })?;
                    let encode_pooled = time(15, &mut || {
                        encoder
                            .encode_parallel(&frame, &mut bitstream, threads)
                            .map(|_| ())
                    })?;
                    let stream = bitstream.clone();
                    let decode_single = time(5, &mut || {
                        decoder.decode(&stream, &mut decoded, stride, PixelFormat::Bgrx)
                    })?;
                    let decode_pooled = time(15, &mut || {
                        decoder.decode_parallel(
                            &stream,
                            &mut decoded,
                            stride,
                            PixelFormat::Bgrx,
                            threads,
                        )
                    })?;
                    println!(
                        "  {pattern:?}: encode {encode_single:.1} ms single, {encode_pooled:.1} ms pooled; decode {decode_single:.1} ms single, {decode_pooled:.1} ms pooled"
                    );
                }
            }
        }
        println!("load average at the end {}", load_average());
        Ok(())
    }

    #[test]
    fn level_choice_follows_the_request() {
        assert_eq!(Level::choose("scalar"), Level::Scalar);
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(Level::choose("sse2"), Level::Sse2);
            let middle = Level::choose("sse4.2");
            assert_eq!(
                matches!(middle, Level::Sse42(_)),
                super::x86::Sse42::detect().is_some()
            );
            let best = Level::choose("");
            match super::x86::Avx2::detect() {
                Some(token) => assert_eq!(best, Level::Avx2(token)),
                None => assert_eq!(best, middle),
            }
            assert_eq!(Level::choose("something else"), best);
        }
    }

    fn random_lanes(rng: &mut Rng) -> [i16; 4] {
        std::array::from_fn(|_| {
            let value = i16::from_le_bytes([rng.byte(), rng.byte()]);
            // Mostly values of the size the codec works with, sometimes any.
            if rng.below(4) == 0 {
                value
            } else {
                value % 1024
            }
        })
    }

    /// The result of every lane operation for one set of operands.
    fn lane_results<V: Lanes>(operands: [[i16; 4]; 3], count: u32) -> [[i16; 4]; 13] {
        let [a, b, c] = operands.map(V::new);
        let (low, high) = (b.min(c), b.max(c));
        [
            a.get(),
            a.add(b).get(),
            a.sub(b).get(),
            a.mul(b).get(),
            a.min(b).get(),
            a.max(b).get(),
            a.xor(b).get(),
            a.shr(count).get(),
            a.luma_or(b).get(),
            a.clamp(low, high).get(),
            a.abs().get(),
            V::splat(operands[0][0]).get(),
            QuantLanes::<V>::new(i32::try_from(count).unwrap_or(0))
                .quantize(a)
                .get(),
        ]
    }

    /// The lane arithmetic the group coder is written in: the portable
    /// implementation against plain integer code, and the SSE2 one against
    /// the portable one, lane for lane.
    #[test]
    fn lane_arithmetic_matches_scalar_code() {
        let mut rng = Rng(21);
        for _ in 0..50_000 {
            let operands = [
                random_lanes(&mut rng),
                random_lanes(&mut rng),
                random_lanes(&mut rng),
            ];
            let count = u32::try_from(rng.below(16)).unwrap_or(0);
            let portable = lane_results::<PortableLanes>(operands, count);
            let [a, b, _] = operands;
            for lane in 0..4 {
                assert_eq!(portable[1][lane], a[lane].wrapping_add(b[lane]));
                assert_eq!(portable[3][lane], a[lane].wrapping_mul(b[lane]));
                assert_eq!(portable[7][lane], a[lane] >> count);
            }
            #[cfg(target_arch = "x86_64")]
            assert_eq!(
                portable,
                lane_results::<super::x86::WideLanes>(operands, count),
                "{operands:?} {count}"
            );
        }
        // The lane quantizer is `quantize` with each component's shift.
        for qp in 0..16 {
            for error in -1100i16..=1100 {
                let lanes = QuantLanes::<PortableLanes>::new(qp)
                    .quantize(PortableLanes::new([error; 4]))
                    .get();
                for (component, lane) in lanes.iter().enumerate().take(3) {
                    let expected = quantize(i32::from(error), qlevel(component, qp));
                    assert_eq!(i32::from(*lane), expected, "{qp} {error}");
                }
            }
        }
    }

    fn random_colour(rng: &mut Rng) -> [i16; 3] {
        let sample = |rng: &mut Rng, bound: usize| i16::try_from(rng.below(bound)).unwrap_or(0);
        [sample(rng, 256), sample(rng, 512), sample(rng, 512)]
    }

    /// The history searches and the colour conversion of `kernels` against
    /// the portable implementation.
    fn check_kernels<K: Kernels>(kernels: K, rng: &mut Rng) {
        for _ in 0..20_000 {
            // A few colours repeat, so that exact hits and ties are common.
            let palette = [random_colour(rng), random_colour(rng), random_colour(rng)];
            let mut rows = [[0i16; 16]; 3];
            for entry in 0..16 {
                let colour = if rng.below(3) == 0 {
                    random_colour(rng)
                } else {
                    palette[rng.below(3)]
                };
                for (row, value) in rows.iter_mut().zip(colour) {
                    row[entry] = value;
                }
            }
            let mut pixel = palette[rng.below(3)];
            if rng.below(2) == 0 {
                for value in &mut pixel {
                    *value = (*value + i16::try_from(rng.below(9)).unwrap_or(0) - 4).clamp(0, 255);
                }
            }
            let tolerance = [0usize; 3].map(|_| {
                let wide = rng.below(3) == 0;
                i16::try_from(rng.below(if wide { 130 } else { 4 })).unwrap_or(0)
            });
            let (first, usable) = [(0usize, u32::MAX), (16, u32::MAX), (25, 0x7F)][rng.below(3)];
            let valid = match rng.below(4) {
                0 => 0,
                1 => usable,
                _ => u32::try_from(rng.next() >> 32).unwrap_or(0) & usable,
            };
            let candidates = [&rows[0], &rows[1], &rows[2]];
            assert_eq!(
                kernels.ich_first(candidates, pixel, tolerance, valid),
                Scalar.ich_first(candidates, pixel, tolerance, valid),
                "{rows:?} {pixel:?} {tolerance:?} {valid:#x}"
            );
            assert_eq!(
                kernels.ich_first(candidates, pixel, tolerance, u32::MAX),
                Scalar.ich_first(candidates, pixel, tolerance, u32::MAX),
                "{rows:?} {pixel:?} {tolerance:?}"
            );
            assert_eq!(
                kernels.ich_best(candidates, pixel, valid, first),
                Scalar.ich_best(candidates, pixel, valid, first),
                "{rows:?} {pixel:?} {valid:#x} {first}"
            );
        }
        for length in 0..70usize {
            for red_first in [false, true] {
                let row: Vec<u8> = (0..length * 4).map(|_| rng.byte()).collect();
                let mut line = vec![[7i16; 4]; length];
                let mut expected = vec![[9i16; 4]; length];
                row_to_ycocg(kernels, &row, red_first, &mut line);
                row_to_ycocg(Scalar, &row, red_first, &mut expected);
                assert_eq!(line, expected, "{length} pixels to YCoCg");
                // Converting back gives the row again, padding byte 255.
                let mut back = vec![1u8; length * 4];
                row_to_rgb(kernels, &line, red_first, &mut back);
                for (pixel, original) in back.chunks_exact(4).zip(row.chunks_exact(4)) {
                    assert_eq!(pixel, [original[0], original[1], original[2], 255]);
                }
                // Any reconstructed samples, also ones no picture produces.
                let line: Vec<Pixel> = (0..length)
                    .map(|_| {
                        let [y, co, cg] = random_colour(rng);
                        [y, co, cg, 0]
                    })
                    .collect();
                let mut pixels = vec![3u8; length * 4];
                let mut expected = vec![5u8; length * 4];
                row_to_rgb(kernels, &line, red_first, &mut pixels);
                row_to_rgb(Scalar, &line, red_first, &mut expected);
                assert_eq!(pixels, expected, "{length} pixels to RGB");
            }
        }
    }

    /// The SSE2 and AVX2 kernels (the `unsafe` code of the codec) against the
    /// scalar kernels on random input.
    #[test]
    fn simd_kernels_match_scalar_kernels() {
        let mut rng = Rng(22);
        check_kernels(Scalar, &mut rng);
        #[cfg(target_arch = "x86_64")]
        {
            check_kernels(super::x86::Sse2, &mut rng);
            if let Some(token) = super::x86::Sse42::detect() {
                check_kernels(token, &mut rng);
            }
            if let Some(token) = super::x86::Avx2::detect() {
                check_kernels(token, &mut rng);
            }
        }
    }

    /// Whole pictures through the scalar, SSE2 and AVX2 copies of the slice
    /// loops: the same bitstream, statistics and decoded picture, also from
    /// a corrupt bitstream.
    #[test]
    fn every_level_gives_the_same_bytes() -> TestResult {
        let all = levels();
        // Width, height, rate, DSC minor version, block prediction, line
        // buffer depth, RGBx input.
        let cases = [
            (640u32, 360u32, 128u16, 2u8, true, 9u8, false),
            (641, 479, 96, 2, true, 9, false),
            (333, 217, 192, 1, true, 8, true),
            (320, 120, 137, 2, false, 9, false),
            (101, 67, 128, 2, true, 16, true),
            (1366, 768, 160, 1, true, 9, false),
        ];
        for (width, height, bits_per_pixel_x16, version_minor, block_pred_enable, depth, rgbx) in
            cases
        {
            let config = Config {
                bits_per_pixel_x16,
                version_minor,
                block_pred_enable,
                linebuf_depth: depth,
                ..Config::new(width, height)?
            };
            let (format, stride) = if rgbx {
                (PixelFormat::Rgbx, size(width) * 4 + 8)
            } else {
                (PixelFormat::Bgrx, size(width) * 4)
            };
            for pattern in PATTERNS {
                let image = picture(pattern, size(width), size(height));
                let mut data = vec![0x42u8; stride * size(height)];
                let rows = data.chunks_exact_mut(stride);
                for (row, source) in rows.zip(image.chunks_exact(size(width) * 4)) {
                    row[..source.len()].copy_from_slice(source);
                }
                let frame = Frame {
                    data: &data,
                    stride,
                    format,
                };
                let mut reference = None;
                for (name, level) in &all {
                    let mut encoder = Encoder::new(config.clone())?;
                    encoder.level = *level;
                    let mut bitstream = Vec::new();
                    let stats = encoder.encode(&frame, &mut bitstream)?;
                    let mut pooled = Vec::new();
                    assert_eq!(encoder.encode_parallel(&frame, &mut pooled, 3)?, stats);
                    assert_eq!(bitstream, pooled, "{name} pooled");
                    let mut decoder = Decoder::new(encoder.pps().clone())?;
                    decoder.level = *level;
                    let mut decoded = vec![0u8; data.len()];
                    decoder.decode_parallel(&bitstream, &mut decoded, stride, format, 3)?;
                    let mut corrupt = bitstream.clone();
                    let mut rng = Rng(u64::from(width) * 7919 + u64::from(bits_per_pixel_x16));
                    for _ in 0..64 {
                        let at = rng.below(corrupt.len());
                        corrupt[at] ^= rng.byte();
                    }
                    let mut garbled = vec![0u8; data.len()];
                    decoder.decode(&corrupt, &mut garbled, stride, format)?;
                    let result = (bitstream, stats, decoded, garbled);
                    match &reference {
                        None => reference = Some(result),
                        Some(expected) => {
                            assert!(
                                *expected == result,
                                "{name} differs from scalar: {width}x{height} {pattern:?}"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The pool runs a closure that borrows from the caller's stack on its
    /// threads, survives a panicking closure, and its threads end when it is
    /// dropped.
    #[test]
    fn pool_runs_borrowed_work_and_shuts_down() {
        let pool = Pool::new(3);
        let shared = Arc::clone(&pool.shared);
        assert_eq!(
            Arc::strong_count(&shared),
            2,
            "no thread before the first use"
        );
        let numbers: Vec<u64> = (1..=10_000).collect();
        for _ in 0..50 {
            let next = AtomicUsize::new(0);
            let sum = AtomicU64::new(0);
            let task = || {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(number) = numbers.get(at) else {
                        break;
                    };
                    sum.fetch_add(*number, Ordering::Relaxed);
                }
            };
            assert_eq!(pool.run(3, &task), Ok(()));
            assert_eq!(sum.load(Ordering::Relaxed), 50_005_000);
        }
        let started = lock(&pool.workers).len();
        assert_eq!(started, 3, "three helper threads were asked for");
        assert_eq!(Arc::strong_count(&shared), 2 + started);
        assert_eq!(pool.run(0, &|| {}), Ok(()), "no helper wanted");

        // A helper thread panics inside the closure: the call reports it
        // once every thread has left the closure, and the pool keeps working.
        let helper_ran = AtomicBool::new(false);
        let task = || {
            if std::thread::current().name() == Some("dsc-slice") {
                helper_ran.store(true, Ordering::SeqCst);
                std::panic::resume_unwind(Box::new("test panic"));
            }
            let start = Instant::now();
            while !helper_ran.load(Ordering::SeqCst) && start.elapsed().as_secs() < 20 {
                std::thread::yield_now();
            }
        };
        assert_eq!(pool.run(3, &task), Err(DscError::Worker));
        assert!(helper_ran.load(Ordering::SeqCst));
        let calls = AtomicUsize::new(0);
        let task = || {
            calls.fetch_add(1, Ordering::SeqCst);
        };
        assert_eq!(pool.run(3, &task), Ok(()));
        assert!((1..=4).contains(&calls.load(Ordering::SeqCst)));
        assert_eq!(lock(&pool.shared.state).batches.len(), 0);

        drop(pool);
        assert_eq!(Arc::strong_count(&shared), 1, "every thread has ended");
    }

    /// A panic of the calling thread inside the closure still waits for the
    /// helper threads before the borrowed closure goes away.
    #[test]
    fn pool_waits_for_helpers_when_the_caller_panics() {
        let pool = Pool::new(2);
        let inside = AtomicUsize::new(0);
        let left = AtomicUsize::new(0);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let task = || {
                if std::thread::current().name() == Some("dsc-slice") {
                    inside.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    left.fetch_add(1, Ordering::SeqCst);
                } else {
                    let start = Instant::now();
                    while inside.load(Ordering::SeqCst) == 0 && start.elapsed().as_secs() < 20 {
                        std::thread::yield_now();
                    }
                    std::panic::resume_unwind(Box::new("test panic"));
                }
            };
            pool.run(2, &task)
        }));
        assert!(outcome.is_err(), "the caller's panic continues");
        let inside = inside.load(Ordering::SeqCst);
        assert!(inside >= 1);
        assert_eq!(
            left.load(Ordering::SeqCst),
            inside,
            "helpers were waited for"
        );
        assert_eq!(lock(&pool.shared.state).batches.len(), 0);
        assert_eq!(pool.run(2, &|| {}), Ok(()));
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// One encoder and one decoder for many pictures: the kept state and the
    /// shared pool do not leak from one picture, rate or thread into another.
    #[test]
    fn codec_objects_are_reused_across_pictures_rates_and_threads() -> TestResult {
        assert_send_sync::<Encoder>();
        assert_send_sync::<Decoder>();
        let (width, height) = (640usize, 360usize);
        let stride = width * 4;
        let images: Vec<Vec<u8>> = PATTERNS
            .iter()
            .map(|pattern| picture(*pattern, width, height))
            .collect();
        let mut encoder = Encoder::new(Config::new(640, 360)?)?;
        for bits_per_pixel_x16 in [DEFAULT_BPP_X16, MIN_BPP_X16, 171, MAX_BPP_X16, 104] {
            encoder.set_bits_per_pixel_x16(bits_per_pixel_x16)?;
            let decoder = Decoder::new(encoder.pps().clone())?;
            for (number, image) in images.iter().enumerate() {
                let frame = Frame {
                    data: image,
                    stride,
                    format: PixelFormat::Bgrx,
                };
                // A new encoder on one thread is the reference.
                let fresh = Encoder::new(encoder.config().clone())?;
                let mut expected = Vec::new();
                let expected_stats = fresh.encode(&frame, &mut expected)?;
                let mut bitstream = vec![0xAAu8; number * 1000];
                let threads = 1 + number % 5;
                let stats = encoder.encode_parallel(&frame, &mut bitstream, threads)?;
                assert_eq!(stats, expected_stats);
                assert_eq!(
                    bitstream, expected,
                    "picture {number} at {bits_per_pixel_x16}"
                );
                let mut expected_picture = vec![0u8; image.len()];
                Decoder::new(fresh.pps().clone())?.decode(
                    &expected,
                    &mut expected_picture,
                    stride,
                    PixelFormat::Bgrx,
                )?;
                let mut decoded = vec![0x55u8; image.len()];
                decoder.decode_parallel(&bitstream, &mut decoded, stride, PixelFormat::Bgrx, 4)?;
                assert_eq!(decoded, expected_picture);
                let copy = decoder.clone();
                decoded.fill(0);
                copy.decode(&bitstream, &mut decoded, stride, PixelFormat::Bgrx)?;
                assert_eq!(decoded, expected_picture);
            }
        }
        assert!(
            !lock(&encoder.spares.0).is_empty(),
            "the encoder keeps its slice state between pictures"
        );
        assert!(Arc::ptr_eq(&encoder.pool, &encoder.clone().pool));

        // Several threads use one encoder and one decoder at the same time.
        encoder.set_bits_per_pixel_x16(DEFAULT_BPP_X16)?;
        let decoder = Decoder::new(encoder.pps().clone())?;
        let frame = Frame {
            data: &images[4],
            stride,
            format: PixelFormat::Bgrx,
        };
        let mut expected = Vec::new();
        encoder.encode(&frame, &mut expected)?;
        let mut expected_picture = vec![0u8; images[4].len()];
        decoder.decode(&expected, &mut expected_picture, stride, PixelFormat::Bgrx)?;
        let outcomes: Vec<Result<bool, DscError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|thread| {
                    let (encoder, decoder) = (&encoder, &decoder);
                    let (expected, expected_picture) = (&expected, &expected_picture);
                    scope.spawn(move || {
                        let mut same = true;
                        for round in 0..6 {
                            let mut bitstream = Vec::new();
                            encoder.encode_parallel(
                                &frame,
                                &mut bitstream,
                                1 + (thread + round) % 4,
                            )?;
                            same &= bitstream == *expected;
                            let mut decoded = vec![0u8; expected_picture.len()];
                            decoder.decode_parallel(
                                &bitstream,
                                &mut decoded,
                                stride,
                                PixelFormat::Bgrx,
                                1 + round % 3,
                            )?;
                            same &= decoded == *expected_picture;
                        }
                        Ok(same)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or(Err(DscError::Worker)))
                .collect()
        });
        assert!(
            outcomes.iter().all(|outcome| *outcome == Ok(true)),
            "{outcomes:?}"
        );
        Ok(())
    }

    /// Small, narrow and odd pictures and slices: every call returns (a
    /// result or an error) and every code level agrees.
    #[test]
    fn small_and_odd_sizes_never_panic() -> TestResult {
        let all = levels();
        let mut rng = Rng(31);
        let mut configs = Vec::new();
        for width in (1u32..=48).chain([63, 64, 65, 100, 127, 255, 333]) {
            for height in (1u32..=12).chain([16, 24, 31, 32, 48, 64, 100, 108, 109, 217]) {
                if let Ok(config) = Config::new(width, height) {
                    configs.push(config);
                }
            }
        }
        // Slices that do not divide the picture, one-line slices, slices of
        // the smallest width.
        for (width, height, slice_width, slice_height) in [
            (50u32, 20u32, 7u32, 3u32),
            (50, 20, 13, 20),
            (33, 9, 33, 1),
            (640, 100, 7, 100),
            (97, 53, 31, 17),
            (200, 30, 199, 29),
            (64, 64, 8, 64),
        ] {
            for bits_per_pixel_x16 in [MIN_BPP_X16, 133, MAX_BPP_X16] {
                configs.push(Config {
                    width,
                    height,
                    slice_width,
                    slice_height,
                    bits_per_pixel_x16,
                    block_pred_enable: true,
                    version_minor: 2,
                    linebuf_depth: 9,
                });
            }
        }
        let (mut coded, mut refused) = (0, 0);
        for config in configs {
            let (width, height) = (size(config.width), size(config.height));
            let noisy = rng.below(2) == 0;
            let image: Vec<u8> = (0..width * height * 4)
                .map(|at| {
                    if noisy {
                        rng.byte()
                    } else {
                        byte(at / 4 % 7 * 40)
                    }
                })
                .collect();
            let frame = Frame {
                data: &image,
                stride: width * 4,
                format: PixelFormat::Bgrx,
            };
            let mut reference = None;
            for (name, level) in &all {
                let mut encoder = match Encoder::new(config.clone()) {
                    Ok(encoder) => encoder,
                    Err(DscError::Config(_)) => {
                        refused += 1;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                encoder.level = *level;
                let mut bitstream = Vec::new();
                let outcome = encoder.encode_parallel(&frame, &mut bitstream, 3);
                let mut decoder = Decoder::new(Pps::from_bytes(&encoder.pps().to_bytes())?)?;
                decoder.level = *level;
                if outcome.is_err() {
                    // A slice this small can overflow its rate buffer; the
                    // decoder still has to cope with whatever arrives.
                    bitstream = (0..encoder.frame_len()).map(|_| rng.byte()).collect();
                }
                let mut decoded = vec![0u8; image.len()];
                decoder.decode_parallel(
                    &bitstream,
                    &mut decoded,
                    width * 4,
                    PixelFormat::Bgrx,
                    3,
                )?;
                // Random bytes differ per level; only real pictures compare.
                let result = if outcome.is_ok() {
                    coded += 1;
                    (outcome, bitstream, decoded.clone())
                } else {
                    (outcome, Vec::new(), Vec::new())
                };
                match &reference {
                    None => reference = Some(result),
                    Some(expected) => assert!(*expected == result, "{name} {config:?}"),
                }
                // Buffers of the wrong size are errors, not panics.
                let mut short = vec![0u8; image.len() - 1];
                let stream = vec![0u8; encoder.frame_len()];
                assert_eq!(
                    decoder.decode(&stream, &mut short, width * 4, PixelFormat::Bgrx),
                    Err(DscError::FrameTooSmall)
                );
                assert_eq!(
                    decoder.decode(&stream, &mut decoded, width * 4 - 1, PixelFormat::Bgrx),
                    Err(DscError::FrameTooSmall)
                );
                let cut = Frame {
                    data: &image[1..],
                    ..frame
                };
                assert_eq!(
                    encoder.encode(&cut, &mut Vec::new()),
                    Err(DscError::FrameTooSmall)
                );
            }
        }
        println!("{coded} pictures coded, {refused} configurations refused");
        assert!(coded >= 100 * all.len(), "{coded}");
        Ok(())
    }

    #[test]
    fn bit_writer_matches_bit_by_bit_output() {
        let mut rng = Rng(41);
        for round in 0..300 {
            let mut writer = BitWriter::default();
            if round % 2 == 0 {
                writer.clear(rng.below(64));
            }
            let mut bits = Vec::new();
            for _ in 0..rng.below(400) {
                // A width of 0 or above the limit (56 bits for raw words,
                // 32 for signed values) writes nothing.
                let value = rng.next();
                let (width, limit) = if rng.below(2) == 0 {
                    let width = u32::try_from(rng.below(58)).unwrap_or(0);
                    writer.put_raw(value, width);
                    (width, 56)
                } else {
                    let width = u32::try_from(rng.below(34)).unwrap_or(0);
                    let [a, b, c, d, ..] = value.to_le_bytes();
                    let signed = i32::from_le_bytes([a, b, c, d]);
                    writer.put(signed, i32::try_from(width).unwrap_or(0));
                    writer.put(signed, -3);
                    (width, 32)
                };
                if (1..=limit).contains(&width) {
                    bits.extend((0..width).rev().map(|bit| (value >> bit) & 1 == 1));
                }
            }
            let mut expected = vec![0u8; bits.len().div_ceil(8)];
            for (number, bit) in bits.iter().enumerate() {
                if *bit {
                    expected[number / 8] |= 0x80 >> (number % 8);
                }
            }
            assert_eq!(writer.written(), expected);
        }
    }

    /// The decoder's prefix and three-residual reads against the single-bit
    /// reads they replace, also when the funnel shifter runs dry.
    #[test]
    fn demux_fast_paths_read_what_single_reads_read() {
        let mut rng = Rng(51);
        for _ in 0..2000 {
            let sparse = rng.below(2) == 0;
            let data: Vec<u8> = (0..rng.below(48))
                .map(|_| {
                    if sparse && rng.below(3) != 0 {
                        0
                    } else {
                        rng.byte()
                    }
                })
                .collect();
            let mut fast = Demux {
                data: &data,
                position: 0,
                shifter: [0; 3],
                fullness: [0; 3],
                consumed: 0,
            };
            for _ in 0..40 {
                if rng.below(3) == 0 {
                    fast.refill();
                }
                let stream = rng.below(3);
                let mut slow = fast.clone();
                if rng.below(2) == 0 {
                    let max_prefix = i32::try_from(rng.below(15)).unwrap_or(0) - 1;
                    let mut expected = 0;
                    while expected < max_prefix && slow.get(stream, 1) == 0 {
                        expected += 1;
                    }
                    assert_eq!(fast.prefix(stream, max_prefix), expected);
                } else {
                    let bits = i32::try_from(rng.below(13)).unwrap_or(0) - 1;
                    let expected = [
                        slow.get_signed(stream, bits),
                        slow.get_signed(stream, bits),
                        slow.get_signed(stream, bits),
                    ];
                    assert_eq!(fast.get_signed3(stream, bits), expected);
                }
                assert_eq!(
                    (fast.fullness, fast.consumed, fast.position),
                    (slow.fullness, slow.consumed, slow.position)
                );
            }
        }
    }
}
