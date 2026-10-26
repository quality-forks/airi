//! `wp_color_management_v1`: image descriptions for surfaces and outputs.

use std::sync::Mutex;

use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1, wp_color_management_surface_feedback_v1,
    wp_color_management_surface_v1, wp_color_manager_v1, wp_image_description_creator_icc_v1,
    wp_image_description_creator_params_v1, wp_image_description_info_v1,
    wp_image_description_reference_v1, wp_image_description_v1,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak,
};
use smithay::wayland::compositor::{self, Cacheable};
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use wp_color_management_output_v1::WpColorManagementOutputV1;
use wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1;
use wp_color_management_surface_v1::WpColorManagementSurfaceV1;
use wp_color_manager_v1::WpColorManagerV1;
use wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1;
use wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1;
use wp_image_description_info_v1::WpImageDescriptionInfoV1;
use wp_image_description_reference_v1::WpImageDescriptionReferenceV1;
use wp_image_description_v1::WpImageDescriptionV1;

use crate::color::{
    self, Chromaticity, ImageDescription, Luminances, NamedPrimaries, Primaries, RenderIntent,
    TransferFunction,
};
use crate::protocols::EmptyData;

const VERSION: u32 = 3;

pub struct ColorManagementState {
    outputs: Vec<WpColorManagementOutputV1>,
    feedbacks: Vec<WpColorManagementSurfaceFeedbackV1>,
    /// Information objects waiting for their events.
    ///
    /// `done` is a destructor event, so it can't be sent from the request creating the object:
    /// the resource would be gone before wayland-backend gets to set its data.
    pending_info: Vec<(WpImageDescriptionInfoV1, ImageDescription)>,
}

pub trait ColorManagementHandler {
    fn color_management_state(&mut self) -> &mut ColorManagementState;
    /// The image description the compositor would like the surface to use.
    fn preferred_image_description(&mut self, surface: &WlSurface) -> ImageDescription;
}

/// Double-buffered image description of a surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct SurfaceColorState {
    pub description: Option<(ImageDescription, RenderIntent)>,
}

impl Cacheable for SurfaceColorState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Returns the image description and render intent of the surface's current state.
pub fn surface_image_description(
    states: &compositor::SurfaceData,
) -> Option<(ImageDescription, RenderIntent)> {
    states
        .cached_state
        .get::<SurfaceColorState>()
        .current()
        .description
}

/// Marks surfaces that have a `wp_color_management_surface_v1`.
#[derive(Default)]
struct HasColorManagementSurface(Mutex<bool>);

pub struct OutputData {
    output: WeakOutput,
}

pub struct SurfaceData {
    surface: Weak<WlSurface>,
}

pub struct FeedbackData {
    surface: Weak<WlSurface>,
    /// Identity of the last preferred image description sent or assumed by the client.
    preferred: Mutex<u64>,
}

#[derive(Debug, Default)]
pub struct ParamsData(Mutex<ParamsBuilder>);

#[derive(Debug, Default)]
struct ParamsBuilder {
    tf: Option<TransferFunction>,
    primaries: Option<(Primaries, Option<NamedPrimaries>)>,
    luminances: Option<Luminances>,
    target_primaries: Option<Primaries>,
    target_luminance: Option<(f64, f64)>,
    max_cll: Option<f64>,
    max_fall: Option<f64>,
}

// Image description objects are few, short-lived, and never moved around.
#[allow(clippy::large_enum_variant)]
pub enum ImageDescriptionData {
    Ready {
        description: ImageDescription,
        /// Whether `get_information` is allowed.
        info: bool,
    },
    Failed,
}

impl ColorManagementState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, EmptyData>,
        D: ColorManagementHandler,
        D: 'static,
    {
        display.create_global::<D, WpColorManagerV1, _>(VERSION, EmptyData);

        Self {
            outputs: Vec::new(),
            feedbacks: Vec::new(),
            pending_info: Vec::new(),
        }
    }

    /// Notifies clients that the image description of the output changed.
    pub fn output_changed(&mut self, output: &Output) {
        self.outputs.retain(|obj| obj.is_alive());

        for obj in &self.outputs {
            let data = obj.data::<OutputData>().unwrap();
            if data.output.upgrade().as_ref() != Some(output) {
                continue;
            }

            obj.image_description_changed();

            let Some(client) = obj.client() else {
                continue;
            };
            for wl_output in output.client_outputs(&client) {
                if wl_output.version() >= 2 {
                    wl_output.done();
                }
            }
        }
    }

    /// Sends preferred image description changes to clients.
    pub fn refresh_preferred(&mut self, mut preferred: impl FnMut(&WlSurface) -> ImageDescription) {
        for (info, description) in self.pending_info.drain(..) {
            if info.is_alive() {
                send_information(&info, &description);
            }
        }

        self.feedbacks.retain(|obj| obj.is_alive());

        for obj in &self.feedbacks {
            let data = obj.data::<FeedbackData>().unwrap();
            let Ok(surface) = data.surface.upgrade() else {
                continue;
            };

            let id = color::identity(&preferred(&surface));
            let mut last = data.preferred.lock().unwrap();
            if *last != id {
                *last = id;
                send_preferred_changed(obj, id);
            }
        }
    }
}

fn send_preferred_changed(obj: &WpColorManagementSurfaceFeedbackV1, id: u64) {
    if obj.version() >= 2 {
        obj.preferred_changed2((id >> 32) as u32, id as u32);
    } else {
        obj.preferred_changed(id as u32);
    }
}

fn send_ready(obj: &WpImageDescriptionV1, description: &ImageDescription) {
    let id = color::identity(description);
    if obj.version() >= 2 {
        obj.ready2((id >> 32) as u32, id as u32);
    } else {
        obj.ready(id as u32);
    }
}

fn init_description<D>(
    data_init: &mut DataInit<'_, D>,
    id: New<WpImageDescriptionV1>,
    description: ImageDescription,
    info: bool,
) where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData> + 'static,
{
    let obj = data_init.init(id, ImageDescriptionData::Ready { description, info });
    send_ready(&obj, &description);
}

fn init_failed<D>(
    data_init: &mut DataInit<'_, D>,
    id: New<WpImageDescriptionV1>,
    cause: wp_image_description_v1::Cause,
    msg: &str,
) where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData> + 'static,
{
    let obj = data_init.init(id, ImageDescriptionData::Failed);
    obj.failed(cause, msg.to_owned());
}

fn supported_tfs(version: u32) -> Vec<wp_color_manager_v1::TransferFunction> {
    use wp_color_manager_v1::TransferFunction as Tf;

    let mut tfs = vec![
        Tf::Bt1886,
        Tf::Gamma22,
        Tf::Gamma28,
        Tf::ExtLinear,
        Tf::St2084Pq,
        Tf::Hlg,
    ];
    if version >= 2 {
        tfs.push(Tf::CompoundPower24);
    } else {
        tfs.push(Tf::Srgb);
        tfs.push(Tf::ExtSrgb);
    }
    tfs
}

fn tf_from_protocol(
    tf: wp_color_manager_v1::TransferFunction,
    version: u32,
) -> Option<TransferFunction> {
    use wp_color_manager_v1::TransferFunction as Tf;

    if !supported_tfs(version).contains(&tf) {
        return None;
    }

    Some(match tf {
        Tf::Bt1886 => TransferFunction::Bt1886,
        Tf::Gamma22 => TransferFunction::Gamma22,
        Tf::Gamma28 => TransferFunction::Gamma28,
        Tf::ExtLinear => TransferFunction::ExtLinear,
        Tf::Srgb => TransferFunction::Srgb,
        Tf::ExtSrgb => TransferFunction::ExtSrgb,
        Tf::St2084Pq => TransferFunction::St2084Pq,
        Tf::Hlg => TransferFunction::Hlg,
        Tf::CompoundPower24 => TransferFunction::CompoundPower24,
        _ => return None,
    })
}

fn tf_to_protocol(tf: TransferFunction) -> Option<wp_color_manager_v1::TransferFunction> {
    use wp_color_manager_v1::TransferFunction as Tf;

    Some(match tf {
        TransferFunction::Bt1886 => Tf::Bt1886,
        TransferFunction::Gamma22 => Tf::Gamma22,
        TransferFunction::Gamma28 => Tf::Gamma28,
        TransferFunction::ExtLinear => Tf::ExtLinear,
        TransferFunction::Srgb => Tf::Srgb,
        TransferFunction::ExtSrgb => Tf::ExtSrgb,
        TransferFunction::St2084Pq => Tf::St2084Pq,
        TransferFunction::Hlg => Tf::Hlg,
        TransferFunction::CompoundPower24 => Tf::CompoundPower24,
        TransferFunction::Power(_) => return None,
    })
}

fn primaries_from_protocol(p: wp_color_manager_v1::Primaries) -> Option<NamedPrimaries> {
    use wp_color_manager_v1::Primaries as P;

    Some(match p {
        P::Srgb => NamedPrimaries::Srgb,
        P::PalM => NamedPrimaries::PalM,
        P::Pal => NamedPrimaries::Pal,
        P::Ntsc => NamedPrimaries::Ntsc,
        P::GenericFilm => NamedPrimaries::GenericFilm,
        P::Bt2020 => NamedPrimaries::Bt2020,
        P::Cie1931Xyz => NamedPrimaries::Cie1931Xyz,
        P::DciP3 => NamedPrimaries::DciP3,
        P::DisplayP3 => NamedPrimaries::DisplayP3,
        P::AdobeRgb => NamedPrimaries::AdobeRgb,
        _ => return None,
    })
}

fn primaries_to_protocol(p: NamedPrimaries) -> wp_color_manager_v1::Primaries {
    use wp_color_manager_v1::Primaries as P;

    match p {
        NamedPrimaries::Srgb => P::Srgb,
        NamedPrimaries::PalM => P::PalM,
        NamedPrimaries::Pal => P::Pal,
        NamedPrimaries::Ntsc => P::Ntsc,
        NamedPrimaries::GenericFilm => P::GenericFilm,
        NamedPrimaries::Bt2020 => P::Bt2020,
        NamedPrimaries::Cie1931Xyz => P::Cie1931Xyz,
        NamedPrimaries::DciP3 => P::DciP3,
        NamedPrimaries::DisplayP3 => P::DisplayP3,
        NamedPrimaries::AdobeRgb => P::AdobeRgb,
    }
}

#[allow(clippy::too_many_arguments)]
fn primaries_from_args(
    r_x: i32,
    r_y: i32,
    g_x: i32,
    g_y: i32,
    b_x: i32,
    b_y: i32,
    w_x: i32,
    w_y: i32,
) -> Primaries {
    let c = |x: i32, y: i32| Chromaticity::new(f64::from(x) / 1e6, f64::from(y) / 1e6);
    Primaries {
        red: c(r_x, r_y),
        green: c(g_x, g_y),
        blue: c(b_x, b_y),
        white: c(w_x, w_y),
    }
}

fn primaries_to_args(p: &Primaries) -> [i32; 8] {
    let c = |v: f64| (v * 1e6).round() as i32;
    [
        c(p.red.x),
        c(p.red.y),
        c(p.green.x),
        c(p.green.y),
        c(p.blue.x),
        c(p.blue.y),
        c(p.white.x),
        c(p.white.y),
    ]
}

impl<D> GlobalDispatch2<WpColorManagerV1, D> for EmptyData
where
    D: Dispatch<WpColorManagerV1, EmptyData>,
    D: 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_manager_v1::{Feature, RenderIntent as Ri};

        let manager = data_init.init(resource, EmptyData);
        let version = manager.version();

        manager.supported_intent(Ri::Perceptual);
        manager.supported_intent(Ri::Relative);

        let mut features = vec![
            Feature::Parametric,
            Feature::SetPrimaries,
            Feature::SetTfPower,
            Feature::SetLuminances,
            Feature::SetMasteringDisplayPrimaries,
            Feature::ExtendedTargetVolume,
            Feature::WindowsScrgb,
        ];
        if version >= 3 {
            features.push(Feature::WindowsBt2100);
        }
        for feature in features {
            manager.supported_feature(feature);
        }

        for tf in supported_tfs(version) {
            manager.supported_tf_named(tf);
        }

        for primaries in NamedPrimaries::ALL {
            manager.supported_primaries_named(primaries_to_protocol(primaries));
        }

        manager.done();
    }
}

impl<D> Dispatch2<WpColorManagerV1, D> for EmptyData
where
    D: Dispatch<WpColorManagementOutputV1, OutputData>,
    D: Dispatch<WpColorManagementSurfaceV1, SurfaceData>,
    D: Dispatch<WpColorManagementSurfaceFeedbackV1, FeedbackData>,
    D: Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData>,
    D: Dispatch<WpImageDescriptionCreatorIccV1, EmptyData>,
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpColorManagerV1,
        request: <WpColorManagerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_manager_v1::{Error, Request};

        match request {
            Request::Destroy => (),
            Request::GetOutput { id, output } => {
                let output = Output::from_resource(&output)
                    .map(|output| output.downgrade())
                    .unwrap_or_default();
                let obj = data_init.init(id, OutputData { output });
                state.color_management_state().outputs.push(obj);
            }
            Request::GetSurface { id, surface } => {
                let exists = compositor::with_states(&surface, |states| {
                    let marker = states
                        .data_map
                        .get_or_insert_threadsafe(HasColorManagementSurface::default);
                    let mut exists = marker.0.lock().unwrap();
                    std::mem::replace(&mut *exists, true)
                });
                if exists {
                    resource.post_error(
                        Error::SurfaceExists,
                        "the surface already has a color management surface",
                    );
                    return;
                }

                data_init.init(
                    id,
                    SurfaceData {
                        surface: surface.downgrade(),
                    },
                );
            }
            Request::GetSurfaceFeedback { id, surface } => {
                // The client knows nothing yet, so the first change event is not needed. It will
                // ask with get_preferred.
                let preferred = color::identity(&state.preferred_image_description(&surface));
                let obj = data_init.init(
                    id,
                    FeedbackData {
                        surface: surface.downgrade(),
                        preferred: Mutex::new(preferred),
                    },
                );
                state.color_management_state().feedbacks.push(obj);
            }
            Request::CreateIccCreator { obj } => {
                data_init.init(obj, EmptyData);
                resource.post_error(Error::UnsupportedFeature, "ICC profiles are not supported");
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(obj, ParamsData::default());
            }
            Request::CreateWindowsScrgb { image_description } => {
                init_description(
                    data_init,
                    image_description,
                    ImageDescription::windows_scrgb(),
                    false,
                );
            }
            Request::CreateWindowsBt2100 { image_description } => {
                init_description(
                    data_init,
                    image_description,
                    ImageDescription::windows_bt2100(),
                    false,
                );
            }
            Request::GetImageDescription {
                image_description, ..
            } => {
                // No protocol that niri implements creates image description references.
                init_failed(
                    data_init,
                    image_description,
                    wp_image_description_v1::Cause::Unsupported,
                    "unknown image description reference",
                );
            }
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<WpColorManagementOutputV1, D> for OutputData
where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: <WpColorManagementOutputV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_output_v1::Request;

        match request {
            Request::Destroy => (),
            Request::GetImageDescription { image_description } => match self.output.upgrade() {
                Some(output) => init_description(
                    data_init,
                    image_description,
                    color::output_image_description(&output),
                    true,
                ),
                None => init_failed(
                    data_init,
                    image_description,
                    wp_image_description_v1::Cause::NoOutput,
                    "the output no longer exists",
                ),
            },
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<WpColorManagementSurfaceV1, D> for SurfaceData
where
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        resource: &WpColorManagementSurfaceV1,
        request: <WpColorManagementSurfaceV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_surface_v1::{Error, Request};

        let set_pending = |surface: &WlSurface, value| {
            compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceColorState>()
                    .pending()
                    .description = value;
            });
        };

        match request {
            Request::Destroy => {
                if let Ok(surface) = self.surface.upgrade() {
                    set_pending(&surface, None);
                }
            }
            Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let Ok(surface) = self.surface.upgrade() else {
                    resource.post_error(Error::Inert, "the surface was destroyed");
                    return;
                };

                let intent = match render_intent {
                    WEnum::Value(wp_color_manager_v1::RenderIntent::Perceptual) => {
                        RenderIntent::Perceptual
                    }
                    WEnum::Value(wp_color_manager_v1::RenderIntent::Relative) => {
                        RenderIntent::Relative
                    }
                    _ => {
                        resource.post_error(Error::RenderIntent, "unsupported render intent");
                        return;
                    }
                };

                let description = match image_description.data::<ImageDescriptionData>() {
                    Some(ImageDescriptionData::Ready { description, .. }) => *description,
                    _ => {
                        resource.post_error(
                            Error::ImageDescription,
                            "the image description is not ready",
                        );
                        return;
                    }
                };

                set_pending(&surface, Some((description, intent)));
            }
            Request::UnsetImageDescription => {
                let Ok(surface) = self.surface.upgrade() else {
                    resource.post_error(Error::Inert, "the surface was destroyed");
                    return;
                };
                set_pending(&surface, None);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        _state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _resource: &WpColorManagementSurfaceV1,
    ) {
        if let Ok(surface) = self.surface.upgrade() {
            compositor::with_states(&surface, |states| {
                if let Some(marker) = states.data_map.get::<HasColorManagementSurface>() {
                    *marker.0.lock().unwrap() = false;
                }
            });
        }
    }
}

impl<D> Dispatch2<WpColorManagementSurfaceFeedbackV1, D> for FeedbackData
where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpColorManagementSurfaceFeedbackV1,
        request: <WpColorManagementSurfaceFeedbackV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_color_management_surface_feedback_v1::{Error, Request};

        match request {
            Request::Destroy => (),
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => {
                let Ok(surface) = self.surface.upgrade() else {
                    resource.post_error(Error::Inert, "the surface was destroyed");
                    return;
                };

                let description = state.preferred_image_description(&surface);
                *self.preferred.lock().unwrap() = color::identity(&description);
                init_description(data_init, image_description, description, true);
            }
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<WpImageDescriptionCreatorIccV1, D> for EmptyData
where
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &WpImageDescriptionCreatorIccV1,
        _request: <WpImageDescriptionCreatorIccV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // Creating this object is already a protocol error.
    }
}

impl<D> Dispatch2<WpImageDescriptionCreatorParamsV1, D> for ParamsData
where
    D: Dispatch<WpImageDescriptionV1, ImageDescriptionData>,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionCreatorParamsV1,
        request: <WpImageDescriptionCreatorParamsV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};

        let mut params = self.0.lock().unwrap();

        macro_rules! set_once {
            ($field:expr, $value:expr) => {{
                if $field.is_some() {
                    resource.post_error(Error::AlreadySet, "property already set");
                    return;
                }
                $field = Some($value);
            }};
        }

        match request {
            Request::Create { image_description } => {
                let (Some(tf), Some((primaries, named_primaries))) = (params.tf, params.primaries)
                else {
                    resource.post_error(
                        Error::IncompleteSet,
                        "transfer function and primaries are required",
                    );
                    return;
                };

                if let (Some(max_cll), Some(max_fall)) = (params.max_cll, params.max_fall) {
                    if max_fall > max_cll {
                        resource.post_error(
                            Error::InvalidLuminance,
                            "max_fall must not be greater than max_cll",
                        );
                        return;
                    }
                }

                let mut luminances = params.luminances.unwrap_or_else(|| tf.default_luminances());
                if tf == TransferFunction::St2084Pq {
                    luminances.max = luminances.min + 10000.;
                }

                let description = ImageDescription {
                    tf,
                    primaries,
                    named_primaries,
                    luminances,
                    target_primaries: params.target_primaries,
                    target_luminance: params.target_luminance,
                    max_cll: params.max_cll,
                    max_fall: params.max_fall,
                };

                let valid =
                    primaries.is_valid() && params.target_primaries.is_none_or(|p| p.is_valid());
                if valid {
                    init_description(data_init, image_description, description, false);
                } else {
                    init_failed(
                        data_init,
                        image_description,
                        wp_image_description_v1::Cause::Unsupported,
                        "degenerate primaries",
                    );
                }
            }
            Request::SetTfNamed { tf } => {
                let tf = match tf {
                    WEnum::Value(tf) => tf_from_protocol(tf, resource.version()),
                    WEnum::Unknown(_) => None,
                };
                let Some(tf) = tf else {
                    resource.post_error(Error::InvalidTf, "unsupported transfer function");
                    return;
                };
                set_once!(params.tf, tf);
            }
            Request::SetTfPower { eexp } => {
                if !(10000..=100000).contains(&eexp) {
                    resource.post_error(Error::InvalidTf, "exponent must be within [1, 10]");
                    return;
                }
                set_once!(params.tf, TransferFunction::Power(f64::from(eexp) / 10000.));
            }
            Request::SetPrimariesNamed { primaries } => {
                let primaries = match primaries {
                    WEnum::Value(p) => primaries_from_protocol(p),
                    WEnum::Unknown(_) => None,
                };
                let Some(primaries) = primaries else {
                    resource.post_error(Error::InvalidPrimariesNamed, "unsupported primaries");
                    return;
                };
                set_once!(params.primaries, (primaries.primaries(), Some(primaries)));
            }
            Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                let primaries = primaries_from_args(r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y);
                set_once!(params.primaries, (primaries, None));
            }
            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                let min = f64::from(min_lum) / 10000.;
                let max = f64::from(max_lum);
                let reference = f64::from(reference_lum);
                if max <= min || reference <= min {
                    resource.post_error(
                        Error::InvalidLuminance,
                        "max and reference luminance must be greater than min",
                    );
                    return;
                }
                set_once!(
                    params.luminances,
                    Luminances {
                        min,
                        max,
                        reference
                    }
                );
            }
            Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                let primaries = primaries_from_args(r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y);
                set_once!(params.target_primaries, primaries);
            }
            Request::SetMasteringLuminance { min_lum, max_lum } => {
                let min = f64::from(min_lum) / 10000.;
                let max = f64::from(max_lum);
                if max <= min {
                    resource.post_error(
                        Error::InvalidLuminance,
                        "max luminance must be greater than min",
                    );
                    return;
                }
                set_once!(params.target_luminance, (min, max));
            }
            Request::SetMaxCll { max_cll } => {
                set_once!(params.max_cll, f64::from(max_cll));
            }
            Request::SetMaxFall { max_fall } => {
                set_once!(params.max_fall, f64::from(max_fall));
            }
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<WpImageDescriptionV1, D> for ImageDescriptionData
where
    D: Dispatch<WpImageDescriptionInfoV1, EmptyData>,
    D: ColorManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionV1,
        request: <WpImageDescriptionV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use wp_image_description_v1::{Error, Request};

        match request {
            Request::Destroy => (),
            Request::GetInformation { information } => {
                let description = match self {
                    ImageDescriptionData::Ready {
                        description,
                        info: true,
                    } => description,
                    ImageDescriptionData::Ready { info: false, .. } => {
                        resource.post_error(Error::NoInformation, "get_information not allowed");
                        return;
                    }
                    ImageDescriptionData::Failed => {
                        resource.post_error(Error::NotReady, "the image description is not ready");
                        return;
                    }
                };

                let info = data_init.init(information, EmptyData);
                state
                    .color_management_state()
                    .pending_info
                    .push((info, *description));
            }
            _ => unreachable!(),
        }
    }
}

fn send_information(info: &WpImageDescriptionInfoV1, desc: &ImageDescription) {
    let [r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y] = primaries_to_args(&desc.primaries);
    info.primaries(r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y);
    if let Some(named) = desc.named_primaries {
        info.primaries_named(primaries_to_protocol(named));
    }

    match tf_to_protocol(desc.tf) {
        Some(tf) => info.tf_named(tf),
        None => {
            if let TransferFunction::Power(exp) = desc.tf {
                info.tf_power((exp * 10000.).round() as u32);
            }
        }
    }

    let lum = desc.luminances;
    info.luminances(
        (lum.min * 10000.).round() as u32,
        lum.max.round() as u32,
        lum.reference.round() as u32,
    );

    let [r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y] =
        primaries_to_args(&desc.effective_target_primaries());
    info.target_primaries(r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y);

    let (min, max) = desc.effective_target_luminance();
    info.target_luminance((min * 10000.).round() as u32, max.round() as u32);

    if let Some(max_cll) = desc.max_cll {
        info.target_max_cll(max_cll.round() as u32);
    }
    if let Some(max_fall) = desc.max_fall {
        info.target_max_fall(max_fall.round() as u32);
    }

    info.done();
}

impl<D> Dispatch2<WpImageDescriptionInfoV1, D> for EmptyData
where
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: <WpImageDescriptionInfoV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // No requests.
    }
}

impl<D> Dispatch2<WpImageDescriptionReferenceV1, D> for EmptyData
where
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &WpImageDescriptionReferenceV1,
        _request: <WpImageDescriptionReferenceV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // Only destroy.
    }
}
