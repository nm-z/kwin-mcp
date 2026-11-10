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
const BP_EDGE_COUNT: u32 = 3;
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

fn qlevel(component: usize, qp: i32) -> i32 {
    let index = usize::try_from(qp.clamp(0, MAX_QP)).unwrap_or(0);
    if component == 0 {
        QLEVEL_LUMA[index]
    } else {
        QLEVEL_CHROMA[index]
    }
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
        writer.flush();
        let mut bytes = [0u8; PPS_SIZE];
        for (target, source) in bytes.iter_mut().zip(writer.bytes.iter()) {
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

/// MSB-first bit accumulator (one per substream, and for the PPS).
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    accumulator: u64,
    pending: u32,
}

impl BitWriter {
    fn clear(&mut self) {
        self.bytes.clear();
        self.accumulator = 0;
        self.pending = 0;
    }

    /// Append the low `bits` bits of `value` (two's complement for negatives).
    fn put(&mut self, value: i32, bits: i32) {
        let Ok(bits) = u32::try_from(bits) else {
            return;
        };
        if bits == 0 || bits > 32 {
            return;
        }
        let raw = u64::from(u32::from_le_bytes(value.to_le_bytes()));
        self.accumulator = (self.accumulator << bits) | (raw & ((1u64 << bits) - 1));
        self.pending += bits;
        while self.pending >= 8 {
            self.pending -= 8;
            self.bytes
                .push((self.accumulator >> self.pending).to_le_bytes()[0]);
        }
    }

    /// Pad the last partial byte with zero bits.
    fn flush(&mut self) {
        if self.pending > 0 {
            let byte = (self.accumulator << (8 - self.pending)).to_le_bytes()[0];
            self.bytes.push(byte);
            self.pending = 0;
        }
    }
}

fn clamp_c(value: i32, low: i32, high: i32) -> i32 {
    if value > high {
        high
    } else if value < low {
        low
    } else {
        value
    }
}

/// Number of significant bits (the C model's `ceil_log2`).
fn bit_length(value: i32) -> i32 {
    i32::try_from(32 - value.leading_zeros()).unwrap_or(0)
}

/// Bits needed to code a quantized residual in two's complement.
fn residual_size(value: i32) -> i32 {
    if value == 0 {
        0
    } else if value > 0 {
        bit_length(value) + 1
    } else {
        bit_length(!value) + 1
    }
}

fn quantize(error: i32, qlevel: i32) -> i32 {
    let offset = if qlevel == 0 {
        0
    } else {
        (1 << (qlevel - 1)) - 1
    };
    if error > 0 {
        (error + offset) >> qlevel
    } else {
        -((offset - error) >> qlevel)
    }
}

fn sample(value: i32) -> i16 {
    i16::try_from(value).unwrap_or(0)
}

fn index(value: i32) -> usize {
    usize::try_from(value).unwrap_or(0)
}

fn mid_value(component: usize) -> i32 {
    1 << (DEPTH[component] - 1)
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

/// Everything needed to predict the three samples of one unit.
struct PredCtx {
    mode: PredMode,
    left: i32,
    /// Blended previous-line samples c, b, d, e.
    blend: [i32; 4],
    block: [i32; 3],
    max_value: i32,
    quant_divisor: i32,
}

impl PredCtx {
    fn predict(&self, position: usize, residuals: &[i32; 3]) -> i32 {
        let a = self.left;
        let [c, b, d, e] = self.blend;
        match self.mode {
            PredMode::Map => match position {
                0 => clamp_c(a + b - c, a.min(b), a.max(b)),
                1 => clamp_c(
                    a + d - c + residuals[0] * self.quant_divisor,
                    a.min(b).min(d),
                    a.max(b).max(d),
                ),
                _ => clamp_c(
                    a + e - c + (residuals[0] + residuals[1]) * self.quant_divisor,
                    a.min(b).min(d.min(e)),
                    a.max(b).max(d.max(e)),
                ),
            },
            PredMode::Left => match position {
                0 => a,
                1 => clamp_c(a + residuals[0] * self.quant_divisor, 0, self.max_value),
                _ => clamp_c(
                    a + (residuals[0] + residuals[1]) * self.quant_divisor,
                    0,
                    self.max_value,
                ),
            },
            PredMode::Block => self.block[position.min(2)],
        }
    }
}

/// Block-prediction candidates that take part in the search: candidate 0 (the
/// left neighbour) is the reference, 2..=9 are vectors -3..=-10.
const BP_CANDIDATES: [usize; 9] = [0, 2, 3, 4, 5, 6, 7, 8, 9];

/// Slice state shared by the encoder and the decoder.
struct Core {
    cur: [Vec<i16>; 3],
    prev: [Vec<i16>; 3],
    /// Per group of the previous line: 0 for MAP, else the block vector length.
    pred: Vec<u8>,
    bp_acc: Vec<u16>,
    bp_group: [Vec<u16>; 9],
    ich: [[i16; ICH_SIZE]; 3],
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

impl Core {
    fn new(params: &Params) -> Self {
        let line = params.line_len();
        let groups = params.slice_w.div_ceil(3);
        let mut core = Self {
            cur: [vec![0; line], vec![0; line], vec![0; line]],
            prev: [vec![0; line], vec![0; line], vec![0; line]],
            pred: vec![0; groups],
            bp_acc: vec![0; groups * 3],
            bp_group: std::array::from_fn(|_| vec![0; groups]),
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

    /// Start-of-slice state.
    fn reset(&mut self, params: &Params) {
        for component in 0..3 {
            let mid = sample(mid_value(component));
            self.cur[component].fill(mid);
            self.prev[component].fill(mid);
            self.ich[component] = [mid; ICH_SIZE];
        }
        self.pred.fill(0);
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
    fn ich_group_start(&mut self, params: &Params, group_start: usize, previous_group_ich: bool) {
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
        for offset in 0..3 {
            let x = group_start - 3 + offset + PAD_LEFT;
            let pixel = [self.cur[0][x], self.cur[1][x], self.cur[2][x]];
            let valid = self.ich_valid.min(reserved);
            let mut location = if valid < reserved {
                valid
            } else {
                reserved - 1
            };
            if previous_group_ich {
                for entry in 0..valid {
                    if self.ich[0][entry] == pixel[0]
                        && self.ich[1][entry] == pixel[1]
                        && self.ich[2][entry] == pixel[2]
                    {
                        location = entry;
                        break;
                    }
                }
            }
            for (history, value) in self.ich.iter_mut().zip(pixel) {
                history.copy_within(0..location, 1);
                history[0] = value;
            }
            if location == valid {
                self.ich_valid = valid + 1;
            }
        }
    }

    /// Column (with padding) of history entry 25 for the group at `group_start`.
    fn ich_above_base(params: &Params, group_start: usize) -> usize {
        let low = ICH_ABOVE / 2;
        let high = params.slice_w - 1 - ICH_ABOVE / 2;
        let centre = group_start + 1;
        let centre = if centre > high { high } else { centre.max(low) };
        centre - ICH_ABOVE / 2 + PAD_LEFT
    }

    fn ich_pixel(&self, params: &Params, entry: usize, group_start: usize) -> [i32; 3] {
        let entry = entry % ICH_SIZE;
        if self.v_pos > 0 && entry >= ICH_RESERVED {
            let x = Self::ich_above_base(params, group_start) + entry - ICH_RESERVED;
            [
                i32::from(self.prev[0][x]),
                i32::from(self.prev[1][x]),
                i32::from(self.prev[2][x]),
            ]
        } else {
            [
                i32::from(self.ich[0][entry]),
                i32::from(self.ich[1][entry]),
                i32::from(self.ich[2][entry]),
            ]
        }
    }

    /// Predictor of the group and, for block prediction, the vector length.
    fn pred_mode(&self, group_start: usize) -> (PredMode, usize) {
        if self.v_pos == 0 {
            return (PredMode::Left, 0);
        }
        match self.pred.get(group_start / 3).copied().unwrap_or(0) {
            0 => (PredMode::Map, 0),
            vector => (PredMode::Block, usize::from(vector)),
        }
    }

    fn pred_ctx(
        &self,
        mode: (PredMode, usize),
        component: usize,
        group_start: usize,
        qlevel: i32,
    ) -> PredCtx {
        let base = group_start + PAD_LEFT;
        let cur = &self.cur[component];
        let left = i32::from(cur[base - 1]);
        let quant_divisor = 1 << qlevel;
        let mut ctx = PredCtx {
            mode: mode.0,
            left,
            blend: [0; 4],
            block: [0; 3],
            max_value: (1 << DEPTH[component]) - 1,
            quant_divisor,
        };
        match mode.0 {
            PredMode::Map => {
                let prev = &self.prev[component];
                let at = |x: usize| i32::from(prev[x]);
                let half = quant_divisor / 2;
                let blend = |x: usize| {
                    let filtered = (at(x - 1) + 2 * at(x) + at(x + 1) + 2) >> 2;
                    at(x) + clamp_c(filtered - at(x), -half, half)
                };
                ctx.blend = [
                    blend(base - 1),
                    blend(base),
                    blend(base + 1),
                    blend(base + 2),
                ];
                if group_start == 0 {
                    ctx.blend[0] = left;
                }
            }
            PredMode::Left => {}
            PredMode::Block => {
                for (offset, value) in ctx.block.iter_mut().enumerate() {
                    *value = i32::from(cur[(base + offset).saturating_sub(mode.1)]);
                }
            }
        }
        ctx
    }

    fn midpoint_pred(&self, component: usize, qlevel: i32) -> i32 {
        mid_value(component) + self.left_recon[component] % (1 << qlevel)
    }

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

    /// Store the group's last reconstructed pixel and run the rate control.
    fn finish_group(&mut self, params: &Params, group_start: usize, pixels: usize) {
        let x = (group_start + 2).min(params.slice_w - 1) + PAD_LEFT;
        for component in 0..3 {
            self.left_recon[component] = i32::from(self.cur[component][x]);
        }
        let (scale, bpg_offset) = self.calc_fullness_offset(params);
        self.group_count += 1;
        self.rate_control(
            params,
            scale,
            bpg_offset,
            i32::try_from(pixels).unwrap_or(3),
        );
    }

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

    fn rate_control(&mut self, params: &Params, scale: i64, bpg_offset: i64, group_size: i32) {
        let prev_qp = self.st_qp;
        let prev2_qp = self.prev_qp;
        let rc_size_group_prev = self.rc_size_group;
        let midpoints = self.midpoint.iter().filter(|selected| **selected).count();
        for _ in 0..group_size {
            self.pixel_count += 1;
            if self.pixel_count >= params.xmit_delay {
                self.remove_bits(params);
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
            if !self.ich_selected && midpoints >= 3 {
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
    /// buffer and search block-prediction vectors for the next line.
    fn end_line(&mut self, params: &Params) {
        let width = params.slice_w;
        for component in 0..3 {
            let shift = params.linebuf_shift[component];
            let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
            let prev = &mut self.prev[component];
            let cur = &self.cur[component];
            for (target, source) in prev[PAD_LEFT..PAD_LEFT + width]
                .iter_mut()
                .zip(cur[PAD_LEFT..PAD_LEFT + width].iter())
            {
                let stored = ((i32::from(*source) + round) >> shift).min(params.linebuf_max);
                *target = sample(stored << shift);
            }
            let first = prev[PAD_LEFT];
            let last = prev[PAD_LEFT + width - 1];
            prev[..PAD_LEFT].fill(first);
            prev[PAD_LEFT + width..].fill(last);
        }
        if params.bp_enable {
            self.block_pred_search(params);
        }
        self.v_pos += 1;
        self.ich_valid = self.ich_valid.min(ICH_RESERVED);
    }

    fn block_pred_search(&mut self, params: &Params) {
        let groups = params.slice_w / 3;
        let pixels = groups * 3;
        if groups == 0 {
            return;
        }
        for (slot, candidate) in BP_CANDIDATES.iter().enumerate() {
            let acc = &mut self.bp_acc[..pixels];
            acc.fill(0);
            for component in 0..3 {
                let line = &self.prev[component];
                let shift = DEPTH[component] - 7;
                let mid = sample(mid_value(component));
                let head = (candidate + 1).min(pixels);
                for (sum, value) in acc[..head].iter_mut().zip(line[PAD_LEFT..].iter()) {
                    *sum += ((*value - mid).unsigned_abs() >> shift).min(63);
                }
                if pixels > head {
                    let current = &line[PAD_LEFT + head..PAD_LEFT + pixels];
                    let start = PAD_LEFT + head - 1 - candidate;
                    let reference = &line[start..start + pixels - head];
                    for ((sum, value), predictor) in acc[head..]
                        .iter_mut()
                        .zip(current.iter())
                        .zip(reference.iter())
                    {
                        *sum += ((*value - *predictor).unsigned_abs() >> shift).min(63);
                    }
                }
            }
            for (group, sum) in self.bp_group[slot][..groups].iter_mut().enumerate() {
                *sum = (acc[group * 3] + acc[group * 3 + 1] + acc[group * 3 + 2]).min(511);
            }
        }
        let mut bp_count = 0u32;
        let mut last_edge_count = 10u32;
        for group in 0..groups {
            for h in group * 3..group * 3 + 3 {
                let mut edge = false;
                for component in 0..3 {
                    let line = &self.prev[component];
                    let left = if h > 0 {
                        line[PAD_LEFT + h - 1]
                    } else {
                        sample(mid_value(component))
                    };
                    if (line[PAD_LEFT + h] - left).abs() > BP_EDGE_STRENGTH {
                        edge = true;
                    }
                }
                last_edge_count = if edge { 0 } else { last_edge_count + 1 };
            }
            let sad = |slot: usize| {
                let sums = &self.bp_group[slot];
                let mut total = sums[group];
                if group >= 1 {
                    total += sums[group - 1];
                }
                if group >= 2 {
                    total += sums[group - 2];
                }
                total >> 3
            };
            let mut min_err = sad(0);
            let mut best = 0u8;
            for (slot, candidate) in BP_CANDIDATES.iter().enumerate().skip(1) {
                let err = sad(slot);
                if min_err > err {
                    min_err = err;
                    best = u8::try_from(candidate + 1).unwrap_or(0);
                }
            }
            if group * 3 + 2 >= 9 {
                bp_count = if best != 0 { bp_count + 1 } else { 0 };
            }
            self.pred[group] = if bp_count >= 3 && last_edge_count < BP_EDGE_COUNT {
                best
            } else {
                0
            };
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

fn rgb_to_ycocg(red: u8, green: u8, blue: u8) -> [i16; 3] {
    let (r, g, b) = (i16::from(red), i16::from(green), i16::from(blue));
    let co = r - b;
    let t = b + (co >> 1);
    let cg = g - t;
    let y = t + (cg >> 1);
    [y, co + 256, cg + 256]
}

fn ycocg_to_rgb(y: i16, co: i16, cg: i16) -> [u8; 3] {
    let (y, co, cg) = (i32::from(y), i32::from(co) - 256, i32::from(cg) - 256);
    let t = y - (cg >> 1);
    let g = cg + t;
    let b = t - (co >> 1);
    let r = co + b;
    let byte = |value: i32| value.clamp(0, 255).to_le_bytes()[0];
    [byte(r), byte(g), byte(b)]
}

/// One slice's share of the output picture: its chunk of every slice line.
struct EncodeJob<'a> {
    xstart: usize,
    ystart: usize,
    chunks: Vec<&'a mut [u8]>,
}

struct SliceEncoder {
    core: Core,
    orig: [Vec<i16>; 3],
    substreams: [BitWriter; 3],
    se_sizes: Vec<[u8; 3]>,
    prev_flatness_type: i32,
    muxed: Vec<u8>,
}

impl SliceEncoder {
    fn new(params: &Params) -> Self {
        let line = params.line_len();
        Self {
            core: Core::new(params),
            orig: [vec![0; line], vec![0; line], vec![0; line]],
            substreams: [
                BitWriter::default(),
                BitWriter::default(),
                BitWriter::default(),
            ],
            se_sizes: Vec::with_capacity(params.slice_w.div_ceil(3) * params.slice_h),
            prev_flatness_type: 0,
            muxed: vec![0; params.slice_bytes()],
        }
    }

    fn populate_orig(
        &mut self,
        params: &Params,
        frame: &Frame<'_>,
        xstart: usize,
        y: usize,
    ) -> Result<(), DscError> {
        let width = params.slice_w + PAD_RIGHT;
        if y >= params.pic_h {
            for component in 0..3 {
                self.orig[component][PAD_LEFT..PAD_LEFT + width].fill(sample(mid_value(component)));
            }
            return Ok(());
        }
        let end = params.pic_w.min(xstart + params.slice_w);
        let row_start = y * frame.stride;
        let row = frame
            .data
            .get(row_start + xstart * 4..row_start + end * 4)
            .ok_or(DscError::FrameTooSmall)?;
        let (red, blue) = match frame.format {
            PixelFormat::Bgrx => (2, 0),
            PixelFormat::Rgbx => (0, 2),
        };
        let [luma, orange, green] = &mut self.orig;
        let targets = luma[PAD_LEFT..]
            .iter_mut()
            .zip(orange[PAD_LEFT..].iter_mut())
            .zip(green[PAD_LEFT..].iter_mut());
        for (((y_out, co_out), cg_out), pixel) in targets.zip(row.chunks_exact(4)) {
            let [y_value, co, cg] = rgb_to_ycocg(pixel[red], pixel[1], pixel[blue]);
            *y_out = y_value;
            *co_out = co;
            *cg_out = cg;
        }
        let real = end - xstart;
        for line in &mut self.orig {
            let last = line[PAD_LEFT + real - 1];
            line[PAD_LEFT + real..PAD_LEFT + width].fill(last);
        }
        Ok(())
    }

    /// Flatness class of the original pixels after position `h`: 0 not flat,
    /// 1 somewhat flat, 2 very flat.
    fn orig_flatness(&self, params: &Params, h: usize) -> i32 {
        if h + 1 >= params.slice_w {
            return 0;
        }
        let qp = (self.core.primary_qp - SOMEWHAT_FLAT_QP_DELTA).max(0);
        for (first, last) in [(0usize, 4usize), (1, 7)] {
            if first == 1 && h + 2 >= params.slice_w {
                return 0;
            }
            let mut somewhat_flat = true;
            let mut very_flat = true;
            for component in 0..3 {
                let window = &self.orig[component][PAD_LEFT + h + first..PAD_LEFT + h + last];
                let mut low = i16::MAX;
                let mut high = i16::MIN;
                for value in window {
                    low = low.min(*value);
                    high = high.max(*value);
                }
                let spread = i32::from(high - low);
                if spread > FLATNESS_DET_THRESH.max(1 << qlevel(component, qp)) {
                    somewhat_flat = false;
                }
                if spread > FLATNESS_DET_THRESH {
                    very_flat = false;
                }
            }
            if very_flat {
                return 2;
            }
            if somewhat_flat {
                return 1;
            }
        }
        0
    }

    fn encode_slice(
        &mut self,
        params: &Params,
        frame: &Frame<'_>,
        job: &mut EncodeJob<'_>,
    ) -> Result<FrameStats, DscError> {
        let mut stats = FrameStats::default();
        self.core.reset(params);
        for writer in &mut self.substreams {
            writer.clear();
        }
        self.se_sizes.clear();
        self.prev_flatness_type = 0;
        for line in 0..params.slice_h {
            self.populate_orig(params, frame, job.xstart, job.ystart + line)?;
            let mut group_start = 0;
            while group_start < params.slice_w {
                let pixels = (params.slice_w - group_start).min(3);
                self.encode_group(params, group_start, pixels, &mut stats)?;
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

    fn encode_group(
        &mut self,
        params: &Params,
        group_start: usize,
        pixels: usize,
        stats: &mut FrameStats,
    ) -> Result<(), DscError> {
        self.core.primary_qp = self.core.prev_qp;
        let qp = self.core.primary_qp;
        let prev_ich = self.core.ich_selected;
        self.core.ich_group_start(params, group_start, prev_ich);
        let mode = self.core.pred_mode(group_start);
        let last_h = group_start + pixels - 1;

        // Predictive coding of every unit, both with the regular predictor
        // and with midpoint prediction.
        let mut residuals = [[0i32; 3]; 3];
        let mut mid_residuals = [[0i32; 3]; 3];
        let mut recon = [[0i32; 3]; 3];
        let mut mid_recon = [[0i32; 3]; 3];
        let mut max_error = [0i32; 3];
        let mut max_mid_error = [0i32; 3];
        let mut qlevels = [0i32; 3];
        for unit in 0..3 {
            let level = qlevel(unit, qp);
            qlevels[unit] = level;
            let ctx = self.core.pred_ctx(mode, unit, group_start, level);
            let max_residual = DEPTH[unit] - level;
            let mid_pred = self.core.midpoint_pred(unit, level);
            for position in 0..pixels {
                let actual = i32::from(self.orig[unit][PAD_LEFT + group_start + position]);
                let pred = ctx.predict(position, &residuals[unit]);
                let quantized = quantize(actual - pred, level);
                residuals[unit][position] = quantized;
                let mut mid_quantized = quantize(actual - mid_pred, level);
                while residual_size(mid_quantized) > max_residual {
                    mid_quantized += if mid_quantized > 0 { -1 } else { 1 };
                }
                mid_residuals[unit][position] = mid_quantized;
                let value = clamp_c(pred + quantized * ctx.quant_divisor, 0, ctx.max_value);
                recon[unit][position] = value;
                max_error[unit] = max_error[unit].max((actual - value).abs());
                let mid = clamp_c(
                    mid_pred + mid_quantized * ctx.quant_divisor,
                    0,
                    ctx.max_value,
                );
                mid_recon[unit][position] = mid;
                max_mid_error[unit] = max_mid_error[unit].max((actual - mid).abs());
            }
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

        // Indexed colour history search.
        let mut ich_possible = !force_mpp && !(group_start == 0 && self.core.v_pos == 0);
        let mut ich_lookup = [0usize; 3];
        let mut ich_pixels = [[0i32; 3]; 3];
        let mut max_ich_error = [0i32; 3];
        if ich_possible {
            let mut entries = self.core.ich;
            let mut valid = if self.core.ich_valid >= ICH_SIZE {
                u32::MAX
            } else {
                (1u32 << self.core.ich_valid) - 1
            };
            if self.core.v_pos > 0 {
                valid = (valid & ((1 << ICH_RESERVED) - 1)) | !((1u32 << ICH_RESERVED) - 1);
                let base = Core::ich_above_base(params, group_start);
                for component in 0..3 {
                    entries[component][ICH_RESERVED..]
                        .copy_from_slice(&self.core.prev[component][base..base + ICH_ABOVE]);
                }
            }
            let modified_qp = (qp + 2).min(MAX_QP);
            let max_qerr: [i32; 3] =
                std::array::from_fn(|component| (1 << qlevel(component, modified_qp)) / 2);
            for position in 0..pixels {
                let x = PAD_LEFT + group_start + position;
                let orig = [
                    i32::from(self.orig[0][x]),
                    i32::from(self.orig[1][x]),
                    i32::from(self.orig[2][x]),
                ];
                let diffs = |entry: usize| {
                    [
                        (i32::from(entries[0][entry]) - orig[0]).abs(),
                        (i32::from(entries[1][entry]) - orig[1]).abs(),
                        (i32::from(entries[2][entry]) - orig[2]).abs(),
                    ]
                };
                let mut hit = None;
                for entry in 0..ICH_SIZE {
                    if (valid >> entry) & 1 == 0 {
                        continue;
                    }
                    let d = diffs(entry);
                    if d[0] <= max_qerr[0] && d[1] <= max_qerr[1] && d[2] <= max_qerr[2] {
                        hit = Some((entry, 2 * d[0] + d[1] + d[2]));
                        break;
                    }
                }
                let Some((hit_entry, hit_sad)) = hit else {
                    ich_possible = false;
                    break;
                };
                // The first entry with the lowest weighted SAD; an exact hit
                // is that entry already.
                let mut best = hit_entry;
                if hit_sad != 0 {
                    let mut lowest = i32::MAX;
                    for entry in 0..ICH_SIZE {
                        if (valid >> entry) & 1 == 0 {
                            continue;
                        }
                        let d = diffs(entry);
                        let sad = 2 * d[0] + d[1] + d[2];
                        if lowest > sad {
                            lowest = sad;
                            best = entry;
                        }
                    }
                }
                ich_lookup[position] = best;
                let d = diffs(best);
                for component in 0..3 {
                    ich_pixels[position][component] = i32::from(entries[component][best]);
                    max_ich_error[component] = max_ich_error[component].max(d[component]);
                }
            }
            let last_lookup = ich_lookup[pixels - 1];
            for lookup in ich_lookup.iter_mut().skip(pixels) {
                *lookup = last_lookup;
            }
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
        let mut adjusted = [0i32; 3];
        let mut max_size = [0i32; 3];
        let mut required = [[0i32; 3]; 3];
        let mut midpoint = [false; 3];
        for unit in 0..3 {
            adjusted[unit] = self.core.adjusted_predicted_size(unit);
            for position in 0..3 {
                required[unit][position] = residual_size(residuals[unit][position]);
                max_size[unit] = max_size[unit].max(required[unit][position]);
            }
            let limit = DEPTH[unit] - qlevels[unit];
            if force_mpp || max_size[unit] >= limit {
                max_size[unit] = limit;
                required[unit] = [limit; 3];
                midpoint[unit] = true;
            }
        }
        let escape_size = DEPTH[0] + 1 - qlevels[0];
        let mut use_ich = false;
        if ich_possible {
            let mut ich_bits = if prev_ich {
                1
            } else {
                escape_size - adjusted[0]
            };
            ich_bits += ICH_BITS * 3;
            let mut log_err_p = 0;
            let mut log_err_ich = 0;
            let mut p_bits = 0;
            for unit in 0..3 {
                let error = if midpoint[unit] {
                    max_mid_error[unit]
                } else {
                    max_error[unit]
                };
                log_err_p += bit_length(error);
                log_err_ich += bit_length(max_ich_error[unit]);
                if !params.v2 && unit == 0 {
                    log_err_p *= 2;
                    log_err_ich *= 2;
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
            let ich_cost = ich_bits + ICH_LAMBDA * log_err_ich;
            let need_lower_error = !params.v2 || self.orig_flatness(params, group_start + 2) == 2;
            use_ich = ich_cost < p_cost && (!need_lower_error || log_err_ich <= log_err_p);
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
                if prefix == max_prefix {
                    self.substreams[unit].put(0, max_prefix);
                    bits[unit] += max_prefix;
                } else {
                    self.substreams[unit].put(1, prefix + 1);
                    bits[unit] += prefix + 1;
                }
                let values = if midpoint[unit] {
                    &mid_residuals[unit]
                } else {
                    &residuals[unit]
                };
                for value in values {
                    self.substreams[unit].put(*value, size);
                }
                bits[unit] += 3 * size;
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
        self.se_sizes
            .push(bits.map(|size| u8::try_from(size).unwrap_or(u8::MAX)));
        let coded = bits[0] + bits[1] + bits[2];
        core.buffer_fullness += i64::from(coded);
        core.coded_group_size = coded;
        core.prev_primary_qp = core.primary_qp;
        stats.groups += 1;
        stats.qp_sum += u64::try_from(qp).unwrap_or(0);
        stats.stuffed_groups += u64::from(force_mpp);

        // Final reconstruction of the group.
        for position in 0..pixels {
            let x = PAD_LEFT + group_start + position;
            for unit in 0..3 {
                let value = if use_ich {
                    ich_pixels[position][unit]
                } else if midpoint[unit] {
                    mid_recon[unit][position]
                } else {
                    recon[unit][position]
                };
                core.cur[unit][x] = sample(value);
            }
        }
        core.finish_group(params, group_start, pixels);
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
        for writer in &mut self.substreams {
            writer.flush();
        }
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
                    let bytes = &self.substreams[stream].bytes;
                    for (offset, byte) in target.iter_mut().enumerate() {
                        *byte = bytes.get(read[stream] + offset).copied().unwrap_or(0);
                    }
                    read[stream] += MUX_WORD_BYTES;
                    position += MUX_WORD_BYTES;
                    fullness[stream] += MUX_WORD_BITS;
                }
                fullness[stream] -= i32::from(sizes[stream]);
            }
        }
        self.muxed[position..].fill(0);
        Ok(())
    }
}

/// The decoder's substream demultiplexer: three funnel shifters fed with
/// 48-bit mux words.
struct Demux<'a> {
    data: &'a [u8],
    position: usize,
    shifter: [u128; 3],
    fullness: [i32; 3],
    consumed: i32,
}

impl Demux<'_> {
    fn refill(&mut self) {
        for stream in 0..3 {
            if self.fullness[stream] < MAX_SE_SIZE {
                let mut word = 0u128;
                for offset in 0..MUX_WORD_BYTES {
                    let byte = self.data.get(self.position + offset).copied().unwrap_or(0);
                    word = (word << 8) | u128::from(byte);
                }
                self.position += MUX_WORD_BYTES;
                self.shifter[stream] = (self.shifter[stream] << MUX_WORD_BITS) | word;
                self.fullness[stream] += MUX_WORD_BITS;
            }
        }
    }

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

    fn get_signed(&mut self, stream: usize, bits: i32) -> i32 {
        let value = self.get(stream, bits);
        if bits > 0 && value >> (bits - 1) != 0 {
            value - (1 << bits)
        } else {
            value
        }
    }
}

/// One slice's share of the decoded picture: its segment of every row.
struct DecodeJob<'a> {
    chunks: Vec<&'a [u8]>,
    rows: Vec<&'a mut [u8]>,
}

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

    fn decode_slice(&mut self, params: &Params, job: &mut DecodeJob<'_>, format: PixelFormat) {
        self.data.clear();
        for chunk in &job.chunks {
            self.data.extend_from_slice(chunk);
        }
        self.core.reset(params);
        let mut demux = Demux {
            data: &self.data,
            position: 0,
            shifter: [0; 3],
            fullness: [0; 3],
            consumed: 0,
        };
        let (red, blue) = match format {
            PixelFormat::Bgrx => (2, 0),
            PixelFormat::Rgbx => (0, 2),
        };
        for line in 0..params.slice_h {
            let mut group_start = 0;
            while group_start < params.slice_w {
                let pixels = (params.slice_w - group_start).min(3);
                Self::decode_group(&mut self.core, params, &mut demux, group_start, pixels);
                group_start += 3;
            }
            if let Some(row) = job.rows.get_mut(line) {
                let core = &self.core;
                let samples = core.cur[0][PAD_LEFT..]
                    .iter()
                    .zip(core.cur[1][PAD_LEFT..].iter())
                    .zip(core.cur[2][PAD_LEFT..].iter());
                for (pixel, ((y, co), cg)) in row.chunks_exact_mut(4).zip(samples) {
                    let [r, g, b] = ycocg_to_rgb(*y, *co, *cg);
                    pixel[red] = r;
                    pixel[1] = g;
                    pixel[blue] = b;
                    pixel[3] = 255;
                }
            }
            self.core.end_line(params);
        }
    }

    fn decode_group(
        core: &mut Core,
        params: &Params,
        demux: &mut Demux<'_>,
        group_start: usize,
        pixels: usize,
    ) {
        core.primary_qp = core.prev_qp;
        let qp = core.primary_qp;
        let prev_ich = core.ich_selected;
        core.ich_group_start(params, group_start, prev_ich);

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
            let mut prefix = 0;
            while prefix < max_prefix && demux.get(unit, 1) == 0 {
                prefix += 1;
            }
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
            let mut required = [0i32; 3];
            let mut max_size = 0;
            for position in 0..3 {
                residuals[unit][position] = demux.get_signed(unit, size);
                required[position] = residual_size(residuals[unit][position]);
                max_size = max_size.max(required[position]);
            }
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
        if core.ich_selected {
            for (position, lookup) in ich_lookup.iter().enumerate().take(pixels) {
                let pixel = core.ich_pixel(params, *lookup, group_start);
                for unit in 0..3 {
                    core.cur[unit][PAD_LEFT + group_start + position] = sample(pixel[unit]);
                }
            }
        } else {
            let mode = core.pred_mode(group_start);
            for unit in 0..3 {
                let level = qlevel(unit, qp);
                let ctx = core.pred_ctx(mode, unit, group_start, level);
                let mid_pred = core.midpoint_pred(unit, level);
                for position in 0..pixels {
                    let pred = if core.midpoint[unit] {
                        mid_pred
                    } else {
                        ctx.predict(position, &residuals[unit])
                    };
                    let value = clamp_c(
                        pred + residuals[unit][position] * ctx.quant_divisor,
                        0,
                        ctx.max_value,
                    );
                    core.cur[unit][PAD_LEFT + group_start + position] = sample(value);
                }
            }
        }
        core.flatness_qp_adjust(params, group_start + pixels - 1);
        core.finish_group(params, group_start, pixels);
    }
}

fn check_frame(params: &Params, len: usize, stride: usize) -> Result<(), DscError> {
    let row = params.pic_w * 4;
    if stride < row || len < (params.pic_h - 1) * stride + row {
        return Err(DscError::FrameTooSmall);
    }
    Ok(())
}

/// Run `work` over `jobs` on up to `threads` scoped threads, one worker state
/// per thread, and merge the results.
fn run_jobs<J, W, R>(
    jobs: Vec<J>,
    threads: usize,
    make_worker: impl Fn() -> W + Sync,
    work: impl Fn(&mut W, &mut J) -> Result<R, DscError> + Sync,
    mut merge: impl FnMut(R),
) -> Result<(), DscError>
where
    J: Send,
    R: Send,
{
    let threads = threads.clamp(1, jobs.len().max(1));
    if threads == 1 {
        let mut worker = make_worker();
        for mut job in jobs {
            merge(work(&mut worker, &mut job)?);
        }
        return Ok(());
    }
    let mut buckets: Vec<Vec<J>> = (0..threads).map(|_| Vec::new()).collect();
    for (number, job) in jobs.into_iter().enumerate() {
        buckets[number % threads].push(job);
    }
    let results = thread::scope(|scope| {
        let handles: Vec<_> = buckets
            .into_iter()
            .map(|bucket| {
                let make_worker = &make_worker;
                let work = &work;
                scope.spawn(move || {
                    let mut worker = make_worker();
                    let mut results = Vec::with_capacity(bucket.len());
                    for mut job in bucket {
                        results.push(work(&mut worker, &mut job)?);
                    }
                    Ok::<_, DscError>(results)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| DscError::Worker))
            .collect::<Vec<_>>()
    });
    for result in results {
        for item in result?? {
            merge(item);
        }
    }
    Ok(())
}

/// Fixed-rate DSC encoder for one picture size.
#[derive(Clone, Debug)]
pub struct Encoder {
    config: Config,
    pps: Pps,
    params: Params,
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, DscError> {
        let pps = Pps::from_config(&config)?;
        let params = Params::from_pps(&pps)?;
        Ok(Self {
            config,
            pps,
            params,
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
        *self = Self::new(config)?;
        Ok(())
    }

    /// Encode one picture on the calling thread. `out` is resized to
    /// [`Encoder::frame_len`].
    pub fn encode(&self, frame: &Frame<'_>, out: &mut Vec<u8>) -> Result<FrameStats, DscError> {
        self.encode_parallel(frame, out, 1)
    }

    /// Encode one picture with its slices spread over up to `threads` threads.
    /// The output is identical to [`Encoder::encode`].
    pub fn encode_parallel(
        &self,
        frame: &Frame<'_>,
        out: &mut Vec<u8>,
        threads: usize,
    ) -> Result<FrameStats, DscError> {
        let params = &self.params;
        check_frame(params, frame.data.len(), frame.stride)?;
        out.clear();
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
        let mut stats = FrameStats::default();
        run_jobs(
            jobs,
            threads,
            || SliceEncoder::new(params),
            |worker, job| worker.encode_slice(params, frame, job),
            |slice_stats| stats.add(&slice_stats),
        )?;
        Ok(stats)
    }
}

/// DSC decoder for one picture parameter set.
#[derive(Clone, Debug)]
pub struct Decoder {
    pps: Pps,
    params: Params,
}

impl Decoder {
    pub fn new(pps: Pps) -> Result<Self, DscError> {
        let params = Params::from_pps(&pps)?;
        Ok(Self { pps, params })
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

    /// Decode one picture with its slices spread over up to `threads` threads.
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
        let bands = out.chunks_mut(stride * params.slice_h);
        for (band, data) in bands.zip(bitstream.chunks(row_bytes)) {
            let mut slices: Vec<DecodeJob<'_>> = (0..params.slices_per_line)
                .map(|_| DecodeJob {
                    chunks: Vec::with_capacity(params.slice_h),
                    rows: Vec::with_capacity(params.slice_h),
                })
                .collect();
            for (number, chunk) in data.chunks(params.chunk_bytes).enumerate() {
                slices[number % params.slices_per_line].chunks.push(chunk);
            }
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
        run_jobs(
            jobs,
            threads,
            || SliceDecoder::new(params),
            |worker, job| {
                worker.decode_slice(params, job, format);
                Ok(())
            },
            |()| {},
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, DEFAULT_BPP_X16, Decoder, DscError, Encoder, Frame, FrameStats, MAX_BPP_X16,
        MIN_BPP_X16, PPS_SIZE, PixelFormat, Pps,
    };
    use std::error::Error;
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

    /// Prints encode and decode time per 1920x1080 picture: `cargo test
    /// --release --lib dsc_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark; run in release mode"]
    fn dsc_timing() -> TestResult {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        let encoder = Encoder::new(Config::new(1920, 1080)?)?;
        let decoder = Decoder::new(encoder.pps().clone())?;
        println!("1920x1080, 8 bpp, 40 slices of 480x108, {threads} threads for the parallel runs");
        for pattern in PATTERNS {
            let image = picture(pattern, 1920, 1080);
            let frame = Frame {
                data: &image,
                stride: 1920 * 4,
                format: PixelFormat::Bgrx,
            };
            let mut bitstream = Vec::new();
            let mut decoded = vec![0u8; image.len()];
            let time = |runs: u32, work: &mut dyn FnMut() -> Result<(), DscError>| {
                let mut best = f64::INFINITY;
                for _ in 0..runs {
                    let start = Instant::now();
                    work()?;
                    best = best.min(start.elapsed().as_secs_f64() * 1000.0);
                }
                Ok::<f64, DscError>(best)
            };
            let encode_single = time(5, &mut || {
                encoder.encode(&frame, &mut bitstream).map(|_| ())
            })?;
            let encode_parallel = time(10, &mut || {
                encoder
                    .encode_parallel(&frame, &mut bitstream, threads)
                    .map(|_| ())
            })?;
            let stream = bitstream.clone();
            let decode_single = time(5, &mut || {
                decoder.decode(&stream, &mut decoded, 1920 * 4, PixelFormat::Bgrx)
            })?;
            let decode_parallel = time(10, &mut || {
                decoder.decode_parallel(&stream, &mut decoded, 1920 * 4, PixelFormat::Bgrx, threads)
            })?;
            println!(
                "{pattern:?}: encode {encode_single:.1} ms single, {encode_parallel:.1} ms parallel; decode {decode_single:.1} ms single, {decode_parallel:.1} ms parallel"
            );
        }
        Ok(())
    }
}
