//! Immediate samplers: the console's four sampler words (the GPU's public
//! `SQ_IMG_SAMP_WORD0-3` layout) as a D3D12 static sampler.
//!
//! Word 0: address modes x, y, z (bits 0-8, 3 each), log2 of the maximum
//! anisotropy (9-11), comparison function (12-14), reduction (29-30: 0
//! filtering, 1 minimum, 2 maximum). Word 1: minimum and maximum LOD
//! (0-11, 12-23, unsigned 4.8; a maximum below the minimum is raised to it,
//! since a host sampler needs `MaxLOD >= MinLOD`). Word 2: LOD bias (0-13, signed 5.8; D3D12
//! takes -16 to 15.99, so the rest of the field's range is clamped),
//! magnification and minification filters (20-21, 22-23: point, linear,
//! anisotropic point, anisotropic linear), mip filter (26-27: none, point,
//! linear). Word 3: border colour (30-31: transparent black, opaque black,
//! opaque white, custom).
//!
//! Comparison is the sampling instruction's, not the sampler's: the console
//! encodes a comparison function in every sampler (D3D's default, less or
//! equal, when unused).

/// `D3D12_MIP_LOD_BIAS_MIN` and `D3D12_MIP_LOD_BIAS_MAX`: a root signature
/// with a static sampler outside them is refused.
const MIP_LOD_BIAS: (f32, f32) = (-16.0, 15.99);

/// A static sampler in D3D12 terms; the integers are the `D3D12_FILTER`,
/// `D3D12_TEXTURE_ADDRESS_MODE`, `D3D12_COMPARISON_FUNC` and
/// `D3D12_STATIC_BORDER_COLOR` values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticSampler {
    pub filter: u32,
    /// U, V, W.
    pub address: [u32; 3],
    pub mip_lod_bias: f32,
    pub max_anisotropy: u32,
    pub comparison: u32,
    pub border: u32,
    pub min_lod: f32,
    pub max_lod: f32,
}

// The floats come from fixed-point fields: never NaN.
impl Eq for StaticSampler {}

impl StaticSampler {
    /// Decodes the four sampler words.
    pub fn from_words(w: [u32; 4]) -> Self {
        let bits = |word: u32, at: u32, n: u32| word >> at & ((1 << n) - 1);
        // wrap, mirror, clamp, mirror once; the half-border modes have no
        // D3D form and become their nearest.
        let address = |m: u32| [1, 2, 3, 5, 3, 5, 4, 5][m as usize];
        let (mag, min, mip) = (bits(w[2], 20, 2), bits(w[2], 22, 2), bits(w[2], 26, 2));
        let reduction = match bits(w[0], 29, 2) {
            1 => 2,
            2 => 3,
            _ => 0,
        };
        let filter = if mag >= 2 || min >= 2 {
            0x55 | reduction << 7
        } else {
            (min & 1) << 4 | (mag & 1) << 2 | u32::from(mip == 2) | reduction << 7
        };
        let lod = |v: u32| v as f32 / 256.0;
        // Without a mip filter the base level is sampled. The fields are
        // independent, so the maximum can sit below the minimum; the hardware
        // clamp then yields the minimum, which an equal pair also gives, and
        // a host sampler with `MaxLOD < MinLOD` is invalid (Vulkan's
        // VUID-VkSamplerCreateInfo-maxLod-01973 under vkd3d-proton).
        let (min_lod, max_lod) = if mip == 0 {
            (0.0, 0.0)
        } else {
            let min = lod(bits(w[1], 0, 12));
            (min, lod(bits(w[1], 12, 12)).max(min))
        };
        StaticSampler {
            filter,
            address: [0, 3, 6].map(|at| address(bits(w[0], at, 3))),
            mip_lod_bias: (((bits(w[2], 0, 14) << 18) as i32 >> 18) as f32 / 256.0)
                .clamp(MIP_LOD_BIAS.0, MIP_LOD_BIAS.1),
            max_anisotropy: 1 << bits(w[0], 9, 3).min(4),
            comparison: bits(w[0], 12, 3) + 1,
            // shortcut: a custom border colour (type 3, a table entry) becomes
            // transparent black; upgrade when a title uses one.
            border: bits(w[3], 30, 2) % 3,
            min_lod,
            max_lod,
        }
    }

    /// The same sampler for a comparison instruction (`image_sample_c*`).
    /// `D3D12_FILTER_REDUCTION_TYPE` (bits 7-8) holds one of filtering,
    /// comparison, minimum or maximum, so comparison replaces the reduction
    /// the sampler words give: setting bit 7 alone would turn minimum (2)
    /// into maximum (3) and leave maximum without comparison, a filter the
    /// shader's comparison sampler declaration does not match.
    pub fn comparison(self) -> Self {
        StaticSampler {
            filter: self.filter & !(3 << 7) | 1 << 7,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_compiled_samplers_decode() {
        // Words the console compiler wrote for static samplers of known
        // descriptions.
        let point = StaticSampler::from_words([0x3088, 0x0054_0100, 0x1400_0080, 0x8000_0000]);
        assert_eq!(
            point,
            StaticSampler {
                filter: 0,
                address: [1, 2, 3],
                mip_lod_bias: 0.5,
                max_anisotropy: 1,
                comparison: 4,
                border: 2,
                min_lod: 1.0,
                max_lod: 5.25,
            }
        );
        let aniso = StaticSampler::from_words([0x369E, 0x00FF_F000, 0x18F0_3E00, 0x8000_0000]);
        assert_eq!(
            (aniso.filter, aniso.address, aniso.max_anisotropy),
            (0x55, [4, 5, 3], 8)
        );
        assert_eq!((aniso.mip_lod_bias, aniso.max_lod), (-2.0, 4095.0 / 256.0));
        let compare = StaticSampler::from_words([0x3006, 0x00FF_F000, 0x1450_0000, 0x4000_0000]);
        assert_eq!(
            (compare.filter, compare.address, compare.border),
            (0x14, [4, 1, 1], 1)
        );
        let minimum =
            StaticSampler::from_words([0x2000_3003, 0x00FF_F000, 0x1850_0000, 0x8000_0000]);
        assert_eq!((minimum.filter, minimum.address[0]), (0x115, 5));
        let mag_mip = StaticSampler::from_words([0x3002, 0, 0x1810_0000, 0x8000_0000]);
        assert_eq!((mag_mip.filter, mag_mip.max_lod), (0x05, 0.0));
        let max_aniso =
            StaticSampler::from_words([0x4000_3800, 0x00FF_F000, 0x18F0_0F00, 0x8000_0000]);
        assert_eq!(
            (
                max_aniso.filter,
                max_aniso.max_anisotropy,
                max_aniso.mip_lod_bias
            ),
            (0x1D5, 16, 15.0)
        );
    }

    /// The bias field reaches -32 to 31.99; D3D12 refuses a static sampler
    /// past -16 or 15.99, so those ends are clamped and the rest kept.
    #[test]
    fn lod_bias_stays_in_the_host_range() {
        let bias = |field: u32| StaticSampler::from_words([0, 0, field, 0]).mip_lod_bias;
        assert_eq!(bias(0x0FFF), 15.99, "15.996 clamps");
        assert_eq!(bias(0x1FFF), 15.99, "31.996 clamps");
        assert_eq!(bias(0x2000), -16.0, "-32 clamps");
        assert_eq!(bias(0x3000), -16.0, "-16 is kept");
        assert_eq!(bias(0x0E80), 14.5, "in range is kept");
        assert_eq!(bias(0x3F80), -0.5, "negative in range is kept");
    }

    /// Comparison takes the reduction field whatever the sampler words
    /// asked for; the filter bits beside it are kept.
    #[test]
    fn comparison_replaces_the_reduction() {
        let filter = |w0: u32, w2: u32| {
            StaticSampler::from_words([w0, 0, w2, 0])
                .comparison()
                .filter
        };
        // Point, linear min/mag with linear mip, anisotropic.
        assert_eq!(filter(0, 0), 0x80);
        assert_eq!(filter(0, 0x0850_0000), 0x95);
        assert_eq!(filter(0, 0x00F0_0000), 0xD5);
        // Minimum (reduction 1) and maximum (2) reductions become comparison.
        assert_eq!(filter(0x2000_0000, 0x0850_0000), 0x95, "minimum");
        assert_eq!(filter(0x4000_0000, 0x0850_0000), 0x95, "maximum");
        assert_eq!(filter(0x4000_0000, 0x00F0_0000), 0xD5, "maximum aniso");
        // Everything else is unchanged.
        let s = StaticSampler::from_words([0x2000_3088, 0x0054_0100, 0x1400_0080, 0x8000_0000]);
        assert_eq!(
            StaticSampler {
                filter: s.filter,
                ..s.comparison()
            },
            s
        );
    }

    /// The LOD fields are independent; a maximum below the minimum is raised
    /// to it so the host sampler stays valid, and an ordered pair is kept.
    #[test]
    fn max_lod_is_never_below_min_lod() {
        // Linear mip filter, so the LOD fields are read.
        let lods = |w1: u32| {
            let s = StaticSampler::from_words([0, w1, 0x0800_0000, 0]);
            (s.min_lod, s.max_lod)
        };
        assert_eq!(lods(0x0010_0400), (4.0, 4.0), "max 1 below min 4");
        assert_eq!(lods(0x0000_0FFF), (4095.0 / 256.0, 4095.0 / 256.0));
        assert_eq!(lods(0x0040_0400), (4.0, 4.0), "equal is kept");
        assert_eq!(lods(0x0054_0100), (1.0, 5.25), "ordered is kept");
        // Without a mip filter only the base level is sampled.
        let base = StaticSampler::from_words([0, 0x0010_0400, 0, 0]);
        assert_eq!((base.min_lod, base.max_lod), (0.0, 0.0));
    }
}
