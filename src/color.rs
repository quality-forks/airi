//! Color management and HDR.
//!
//! On HDR outputs niri composites in an extended SDR "blend space": BT.709 primaries, a pure 2.2
//! power curve (mirrored through the origin for negative values), and 1.0 meaning the reference
//! white of the output. SDR clients, niri's own UI and all existing shaders produce values in this
//! space without any changes, so SDR content blends exactly the same as on an SDR output. Values
//! outside of [0, 1] carry wide gamut and HDR information.
//!
//! Every frame of an HDR output is rendered into a floating point buffer in the blend space, and
//! then a final pass encodes it to BT.2100 PQ for scanout. Surfaces with a non-default image
//! description are converted into the blend space as they are drawn, see [`ColorTransform`].

use std::cell::{Cell, RefCell};
use std::sync::Mutex;

use glam::{DMat3, DVec3, Mat3, Vec3};
use smithay::output::Output;

/// Exponent of the blend space transfer function.
pub const BLEND_GAMMA: f64 = 2.2;

/// Luminance of the reference white used when nothing else is known, in cd/m².
///
/// Suggested by Report ITU-R BT.2408.
pub const DEFAULT_REFERENCE_LUMINANCE: f64 = 203.;

/// Peak luminance assumed for HDR content without metadata when it has to be tone mapped.
const ASSUMED_CONTENT_PEAK_LUMINANCE: f64 = 1000.;

/// Fraction of the target headroom where tone mapping starts compressing highlights.
const TONE_MAPPING_KNEE: f64 = 0.75;

/// CIE 1931 xy chromaticity coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
    pub x: f64,
    pub y: f64,
}

impl Chromaticity {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Converts to XYZ with Y = 1.
    fn to_xyz(self) -> DVec3 {
        DVec3::new(self.x / self.y, 1., (1. - self.x - self.y) / self.y)
    }

    fn approx_eq(self, other: Self) -> bool {
        (self.x - other.x).abs() < 1e-3 && (self.y - other.y).abs() < 1e-3
    }
}

pub const D65: Chromaticity = Chromaticity::new(0.3127, 0.3290);
const ILLUMINANT_C: Chromaticity = Chromaticity::new(0.310, 0.316);

/// Color primaries and white point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Primaries {
    pub red: Chromaticity,
    pub green: Chromaticity,
    pub blue: Chromaticity,
    pub white: Chromaticity,
}

impl Primaries {
    pub const BT709: Self = Self::new([0.64, 0.33], [0.30, 0.60], [0.15, 0.06], D65);
    pub const BT2020: Self = Self::new([0.708, 0.292], [0.170, 0.797], [0.131, 0.046], D65);

    const fn new(red: [f64; 2], green: [f64; 2], blue: [f64; 2], white: Chromaticity) -> Self {
        Self {
            red: Chromaticity::new(red[0], red[1]),
            green: Chromaticity::new(green[0], green[1]),
            blue: Chromaticity::new(blue[0], blue[1]),
            white,
        }
    }

    /// Matrix converting linear RGB in these primaries to CIE XYZ.
    pub fn to_xyz(&self) -> DMat3 {
        let p = DMat3::from_cols(self.red.to_xyz(), self.green.to_xyz(), self.blue.to_xyz());
        let s = p.inverse() * self.white.to_xyz();
        p * DMat3::from_diagonal(s)
    }

    /// Whether these primaries describe a usable color space.
    pub fn is_valid(&self) -> bool {
        let points = [self.red, self.green, self.blue, self.white];
        if !points
            .iter()
            .all(|c| c.x.is_finite() && c.y.is_finite() && c.y > 0.)
        {
            return false;
        }

        let p = DMat3::from_cols(self.red.to_xyz(), self.green.to_xyz(), self.blue.to_xyz());
        let det = p.determinant();
        det.is_finite() && det.abs() > 1e-9 && self.to_xyz().is_finite()
    }

    pub fn approx_eq(&self, other: &Self) -> bool {
        self.red.approx_eq(other.red)
            && self.green.approx_eq(other.green)
            && self.blue.approx_eq(other.blue)
            && self.white.approx_eq(other.white)
    }
}

/// Matrix converting linear RGB from `src` primaries to `dst` primaries.
///
/// Uses Bradford chromatic adaptation when the white points differ, since the viewer is assumed
/// to be adapted to the white point of the display.
pub fn conversion_matrix(src: &Primaries, dst: &Primaries) -> DMat3 {
    dst.to_xyz().inverse() * chromatic_adaptation(src.white, dst.white) * src.to_xyz()
}

fn chromatic_adaptation(src: Chromaticity, dst: Chromaticity) -> DMat3 {
    if src.approx_eq(dst) {
        return DMat3::IDENTITY;
    }

    let bradford = DMat3::from_cols(
        DVec3::new(0.8951, 0.2664, -0.1614),
        DVec3::new(-0.7502, 1.7135, 0.0367),
        DVec3::new(0.0389, -0.0685, 1.0296),
    )
    .transpose();

    let src_lms = bradford * src.to_xyz();
    let dst_lms = bradford * dst.to_xyz();
    bradford.inverse() * DMat3::from_diagonal(dst_lms / src_lms) * bradford
}

/// Named color primaries from the color management protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedPrimaries {
    Srgb,
    PalM,
    Pal,
    Ntsc,
    GenericFilm,
    Bt2020,
    Cie1931Xyz,
    DciP3,
    DisplayP3,
    AdobeRgb,
}

impl NamedPrimaries {
    pub const ALL: [Self; 10] = [
        Self::Srgb,
        Self::PalM,
        Self::Pal,
        Self::Ntsc,
        Self::GenericFilm,
        Self::Bt2020,
        Self::Cie1931Xyz,
        Self::DciP3,
        Self::DisplayP3,
        Self::AdobeRgb,
    ];

    pub fn primaries(self) -> Primaries {
        match self {
            Self::Srgb => Primaries::BT709,
            Self::PalM => Primaries::new([0.67, 0.33], [0.21, 0.71], [0.14, 0.08], ILLUMINANT_C),
            Self::Pal => Primaries::new([0.64, 0.33], [0.29, 0.60], [0.15, 0.06], D65),
            Self::Ntsc => Primaries::new([0.630, 0.340], [0.310, 0.595], [0.155, 0.070], D65),
            Self::GenericFilm => {
                Primaries::new([0.681, 0.319], [0.243, 0.692], [0.145, 0.049], ILLUMINANT_C)
            }
            Self::Bt2020 => Primaries::BT2020,
            Self::Cie1931Xyz => Primaries::new(
                [1., 0.],
                [0., 1.],
                [0., 0.],
                Chromaticity::new(1. / 3., 1. / 3.),
            ),
            Self::DciP3 => Primaries::new(
                [0.680, 0.320],
                [0.265, 0.690],
                [0.150, 0.060],
                Chromaticity::new(0.314, 0.351),
            ),
            Self::DisplayP3 => Primaries::new([0.680, 0.320], [0.265, 0.690], [0.150, 0.060], D65),
            Self::AdobeRgb => Primaries::new([0.64, 0.33], [0.21, 0.71], [0.15, 0.06], D65),
        }
    }
}

/// Transfer characteristic of an image description.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransferFunction {
    Bt1886,
    Gamma22,
    Gamma28,
    ExtLinear,
    /// Deprecated ambiguous sRGB; treated as what SDR outputs display, i.e. gamma 2.2.
    Srgb,
    ExtSrgb,
    St2084Pq,
    Hlg,
    CompoundPower24,
    /// Pure power curve with the given exponent.
    Power(f64),
}

impl TransferFunction {
    /// Default luminances implied by the transfer function.
    pub fn default_luminances(self) -> Luminances {
        match self {
            Self::St2084Pq => Luminances {
                min: 0.005,
                max: 10000.,
                reference: DEFAULT_REFERENCE_LUMINANCE,
            },
            Self::Hlg => Luminances {
                min: 0.005,
                max: 1000.,
                reference: DEFAULT_REFERENCE_LUMINANCE,
            },
            Self::Bt1886 => Luminances {
                min: 0.01,
                max: 100.,
                reference: 100.,
            },
            _ => Luminances {
                min: 0.2,
                max: 80.,
                reference: 80.,
            },
        }
    }
}

/// Primary color volume luminance range and reference white, in cd/m².
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Luminances {
    pub min: f64,
    pub max: f64,
    pub reference: f64,
}

/// Rendering intent requested for a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderIntent {
    /// Tone and gamut map content that doesn't fit the target.
    #[default]
    Perceptual,
    /// Map reference white to reference white and clip everything else.
    Relative,
}

/// Colorimetric description of surface or output contents.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageDescription {
    pub tf: TransferFunction,
    pub primaries: Primaries,
    /// Set if the primaries were given by name.
    pub named_primaries: Option<NamedPrimaries>,
    pub luminances: Luminances,
    /// Mastering display primaries; `None` means same as `primaries`.
    pub target_primaries: Option<Primaries>,
    /// Mastering display luminance range (min, max); `None` means same as `luminances`.
    pub target_luminance: Option<(f64, f64)>,
    pub max_cll: Option<f64>,
    pub max_fall: Option<f64>,
}

impl ImageDescription {
    /// The description of untagged content and of SDR outputs.
    pub fn srgb() -> Self {
        Self::named(TransferFunction::Gamma22, NamedPrimaries::Srgb)
    }

    pub fn named(tf: TransferFunction, primaries: NamedPrimaries) -> Self {
        Self {
            tf,
            primaries: primaries.primaries(),
            named_primaries: Some(primaries),
            luminances: tf.default_luminances(),
            target_primaries: None,
            target_luminance: None,
            max_cll: None,
            max_fall: None,
        }
    }

    /// Windows-scRGB from the color management protocol.
    pub fn windows_scrgb() -> Self {
        Self {
            luminances: Luminances {
                min: 0.,
                max: 80.,
                reference: DEFAULT_REFERENCE_LUMINANCE,
            },
            ..Self::named(TransferFunction::ExtLinear, NamedPrimaries::Srgb)
        }
    }

    /// Windows-BT.2100 from the color management protocol.
    pub fn windows_bt2100() -> Self {
        Self {
            luminances: Luminances {
                min: 0.,
                max: 10000.,
                reference: DEFAULT_REFERENCE_LUMINANCE,
            },
            ..Self::named(TransferFunction::St2084Pq, NamedPrimaries::Bt2020)
        }
    }

    /// The description of an output driven in BT.2100 PQ mode.
    pub fn hdr_output(params: &HdrParams) -> Self {
        Self {
            luminances: Luminances {
                min: params.min_luminance,
                max: 10000. + params.min_luminance,
                reference: params.reference_luminance,
            },
            target_primaries: Some(params.display_primaries),
            target_luminance: Some((params.min_luminance, params.peak_luminance)),
            max_cll: Some(params.peak_luminance),
            max_fall: Some(params.max_fall_luminance),
            ..Self::named(TransferFunction::St2084Pq, NamedPrimaries::Bt2020)
        }
    }

    pub fn effective_target_primaries(&self) -> Primaries {
        self.target_primaries.unwrap_or(self.primaries)
    }

    pub fn effective_target_luminance(&self) -> (f64, f64) {
        self.target_luminance
            .unwrap_or((self.luminances.min, self.luminances.max))
    }

    /// Whether the content is already in the SDR blend space.
    ///
    /// Such content is drawn without any conversion, exactly like on an SDR output.
    pub fn is_blend_identity(&self) -> bool {
        matches!(self.tf, TransferFunction::Gamma22 | TransferFunction::Srgb)
            && self.primaries.approx_eq(&Primaries::BT709)
            && (self.luminances.max / self.luminances.reference - 1.).abs() < 1e-3
    }

    /// Relative luminance of encoded value 1.0 (or of PQ 10000 cd/m²).
    fn luminance_scale(&self) -> f64 {
        match self.tf {
            TransferFunction::St2084Pq => 10000. / self.luminances.reference,
            _ => self.luminances.max / self.luminances.reference,
        }
    }

    /// Peak of the content relative to its reference white, if known.
    fn content_peak(&self) -> Option<f64> {
        let peak = match (self.max_cll, self.target_luminance) {
            (Some(max_cll), _) if max_cll > 0. => max_cll,
            (_, Some((_, max))) => max,
            // Without metadata, absolute HDR content says nothing about its actual peak.
            _ if self.tf == TransferFunction::St2084Pq => return None,
            _ if self.tf == TransferFunction::ExtLinear => return None,
            _ => self.luminances.max,
        };
        Some(peak / self.luminances.reference)
    }

    /// Computes the conversion of this content into the blend space of `target`.
    ///
    /// Returns `None` when no conversion is needed.
    pub fn transform_to_blend(
        &self,
        intent: RenderIntent,
        target: ColorTarget,
    ) -> Option<ColorTransform> {
        if self.is_blend_identity() {
            return None;
        }

        let (tf, tf_param) = match self.tf {
            TransferFunction::ExtLinear => (TF_LINEAR, 1.),
            TransferFunction::Gamma22 | TransferFunction::Srgb => (TF_POWER, 2.2),
            TransferFunction::Gamma28 => (TF_POWER, 2.8),
            TransferFunction::Bt1886 => (TF_POWER, 2.4),
            TransferFunction::Power(exp) => (TF_POWER, exp),
            TransferFunction::ExtSrgb | TransferFunction::CompoundPower24 => (TF_SRGB, 0.),
            TransferFunction::St2084Pq => (TF_PQ, 0.),
            TransferFunction::Hlg => {
                // HLG system gamma for the nominal peak luminance, BT.2100 note 5f.
                let peak = self.luminances.max.max(1.);
                (TF_HLG, 1.2 + 0.42 * (peak / 1000.).log10() - 1.)
            }
        };

        let to_xyz = self.primaries.to_xyz();
        let luma = to_xyz.row(1);
        let matrix = conversion_matrix(&self.primaries, &Primaries::BT709);

        let headroom = target.headroom();
        let tone_mapping = if intent == RenderIntent::Perceptual {
            let peak = match self.content_peak() {
                Some(peak) => Some(peak),
                // Unknown peak: only compress it if it can't fit at all.
                None if target == ColorTarget::Sdr => {
                    Some(ASSUMED_CONTENT_PEAK_LUMINANCE / self.luminances.reference)
                }
                None => None,
            };
            peak.and_then(|peak| ToneMapping::new(peak, headroom))
        } else {
            None
        };

        Some(ColorTransform {
            tf,
            tf_param: tf_param as f32,
            luminance_scale: self.luminance_scale() as f32,
            luma: luma.as_vec3(),
            matrix: matrix.as_mat3(),
            tone_mapping,
            max: headroom as f32,
        })
    }
}

/// Parameters of an output driven in HDR mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrParams {
    /// Luminance that SDR white and HDR reference white are shown at.
    pub reference_luminance: f64,
    pub peak_luminance: f64,
    pub min_luminance: f64,
    pub max_fall_luminance: f64,
    pub display_primaries: Primaries,
}

impl HdrParams {
    /// Whether the connector signalling (HDR metadata) is the same for both.
    pub fn signalling_eq(&self, other: &Self) -> bool {
        Self {
            reference_luminance: other.reference_luminance,
            ..*self
        } == *other
    }

    /// How much brighter than reference white the output can go.
    pub fn headroom(&self) -> f64 {
        (self.peak_luminance / self.reference_luminance).max(1.)
    }

    /// Uniforms for the final blend space to PQ encoding pass.
    pub fn encode(&self) -> HdrEncode {
        HdrEncode {
            matrix: conversion_matrix(&Primaries::BT709, &Primaries::BT2020).as_mat3(),
            reference: (self.reference_luminance / 10000.) as f32,
            max: (self.peak_luminance.max(self.reference_luminance) / 10000.) as f32,
        }
    }
}

/// HDR state of an output, stored in its user data.
#[derive(Debug, Default)]
struct OutputHdrState(RefCell<Option<HdrParams>>);

/// Returns the parameters if the output is currently driven in HDR mode.
pub fn output_hdr(output: &Output) -> Option<HdrParams> {
    output
        .user_data()
        .get::<OutputHdrState>()
        .and_then(|state| *state.0.borrow())
}

pub fn set_output_hdr(output: &Output, params: Option<HdrParams>) {
    let data = output.user_data();
    data.insert_if_missing(OutputHdrState::default);
    *data.get::<OutputHdrState>().unwrap().0.borrow_mut() = params;
}

/// Returns the image description of the output contents.
pub fn output_image_description(output: &Output) -> ImageDescription {
    match output_hdr(output) {
        Some(params) => ImageDescription::hdr_output(&params),
        None => ImageDescription::srgb(),
    }
}

/// Uniforms for encoding the blend space into BT.2100 PQ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrEncode {
    /// BT.709 to BT.2020 primaries.
    pub matrix: Mat3,
    /// Reference white in PQ normalized luminance (1.0 = 10000 cd/m²).
    pub reference: f32,
    /// Maximum output in PQ normalized luminance.
    pub max: f32,
}

/// What the content is being drawn into.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ColorTarget {
    /// An SDR framebuffer: blend space values outside of [0, 1] are lost.
    #[default]
    Sdr,
    /// The floating point blend space buffer of an HDR output.
    Hdr {
        /// Peak luminance relative to reference white.
        headroom: f64,
    },
}

impl ColorTarget {
    pub fn headroom(self) -> f64 {
        match self {
            ColorTarget::Sdr => 1.,
            ColorTarget::Hdr { headroom } => headroom,
        }
    }
}

thread_local! {
    static CURRENT_TARGET: Cell<ColorTarget> = const { Cell::new(ColorTarget::Sdr) };
}

/// Returns the target that elements are currently being drawn into.
pub fn current_target() -> ColorTarget {
    CURRENT_TARGET.get()
}

/// Runs `f` with elements drawing into `target`.
pub fn with_target<T>(target: ColorTarget, f: impl FnOnce() -> T) -> T {
    let prev = CURRENT_TARGET.replace(target);
    let rv = f();
    CURRENT_TARGET.set(prev);
    rv
}

pub const TF_LINEAR: f32 = 0.;
pub const TF_POWER: f32 = 1.;
pub const TF_SRGB: f32 = 2.;
pub const TF_PQ: f32 = 3.;
pub const TF_HLG: f32 = 4.;

/// Highlight compression from a content peak down to a target peak.
///
/// Linear up to a knee, then `f(e) = e / (1 + a e)` over the excess, which has unit slope at the
/// knee and reaches the target peak exactly at the content peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneMapping {
    pub knee: f32,
    pub a: f32,
}

impl ToneMapping {
    fn new(peak: f64, target: f64) -> Option<Self> {
        if peak <= target * 1.001 {
            return None;
        }

        let knee = target * TONE_MAPPING_KNEE;
        let a = 1. / (target - knee) - 1. / (peak - knee);
        Some(Self {
            knee: knee as f32,
            a: a as f32,
        })
    }
}

/// Conversion of surface contents into the blend space, applied in shaders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTransform {
    /// One of the `TF_*` constants.
    pub tf: f32,
    /// Exponent for `TF_POWER`, system gamma minus one for `TF_HLG`.
    pub tf_param: f32,
    /// Multiplier from the normalized transfer function output to reference white relative.
    pub luminance_scale: f32,
    /// Luminance coefficients of the source primaries, for the HLG OOTF.
    pub luma: Vec3,
    /// Source primaries to BT.709.
    pub matrix: Mat3,
    pub tone_mapping: Option<ToneMapping>,
    /// Maximum relative luminance of the target.
    pub max: f32,
}

/// Returns the image description identity, the same for equal descriptions.
///
/// Identities are never reused.
pub fn identity(desc: &ImageDescription) -> u64 {
    static REGISTRY: Mutex<Vec<(ImageDescription, u64)>> = Mutex::new(Vec::new());

    let mut registry = REGISTRY.lock().unwrap();
    if let Some((_, id)) = registry.iter().find(|(d, _)| d == desc) {
        return *id;
    }

    let id = registry.len() as u64 + 1;
    registry.push((*desc, id));
    id
}
