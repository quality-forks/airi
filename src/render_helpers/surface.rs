use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{
    import_surface, CommitCounter, DamageSet, OpaqueRegions, RendererSurfaceStateUserData,
};
use smithay::backend::renderer::{
    Bind as _, Color32F, Frame as _, Offscreen as _, Renderer, Texture as _,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};

use super::renderer::{AsGlesFrame as _, NiriRenderer};
use super::shaders::{color_transform_uniforms, Shaders};
use super::texture::TextureBuffer;
use super::BakedBuffer;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::color::{self, ColorTarget, ColorTransform, ImageDescription, RenderIntent};
use crate::protocols::color_management::surface_image_description;

/// Surface render element that converts its contents into the blend space.
///
/// Surfaces without an image description are drawn as is.
#[derive(Debug)]
pub struct SurfaceRenderElement<R: NiriRenderer> {
    inner: WaylandSurfaceRenderElement<R>,
    color: Option<(ImageDescription, RenderIntent)>,
}

impl<R: NiriRenderer> SurfaceRenderElement<R> {
    pub fn new(
        inner: WaylandSurfaceRenderElement<R>,
        color: Option<(ImageDescription, RenderIntent)>,
    ) -> Self {
        let color = color.filter(|(desc, _)| !desc.is_blend_identity());
        Self { inner, color }
    }

    pub fn inner(&self) -> &WaylandSurfaceRenderElement<R> {
        &self.inner
    }

    /// Returns the conversion for the target currently being drawn into.
    pub fn color_transform(&self) -> Option<ColorTransform> {
        let (desc, intent) = self.color?;
        desc.transform_to_blend(intent, color::current_target())
    }
}

impl<R: NiriRenderer> Element for SurfaceRenderElement<R> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for SurfaceRenderElement<GlesRenderer> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let program = self
            .color_transform()
            .and_then(|t| Some((Shaders::get_from_frame(frame).color_surface.clone()?, t)));
        let Some((program, transform)) = program else {
            return RenderElement::<GlesRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
        };

        frame.override_default_tex_program(program, color_transform_uniforms(&transform));
        let res = RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        frame.clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Scanning out would skip the conversion.
        if self.color.is_some() {
            return None;
        }
        self.inner.underlying_storage(renderer)
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for SurfaceRenderElement<TtyRenderer<'render>> {
    fn draw(
        &self,
        frame: &mut TtyFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let program = self.color_transform().and_then(|t| {
            let program = Shaders::get_from_frame(frame.as_gles_frame())
                .color_surface
                .clone()?;
            Some((program, t))
        });
        let Some((program, transform)) = program else {
            return RenderElement::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
        };

        frame
            .as_gles_frame()
            .override_default_tex_program(program, color_transform_uniforms(&transform));
        let res = RenderElement::draw(&self.inner, frame, src, dst, damage, opaque_regions, cache);
        frame.as_gles_frame().clear_tex_program_override();
        res
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        // Scanning out would skip the conversion.
        if self.color.is_some() {
            return None;
        }
        self.inner.underlying_storage(renderer)
    }
}

/// Renders elements from a surface tree as textures into `storage`.
pub fn render_snapshot_from_surface_tree(
    renderer: &mut GlesRenderer,
    surface: &WlSurface,
    location: Point<f64, Logical>,
    storage: &mut Vec<BakedBuffer<TextureBuffer<GlesTexture>>>,
) {
    let _span = tracy_client::span!("render_snapshot_from_surface_tree");

    with_surface_tree_downward(
        surface,
        location,
        |_, states, location| {
            let mut location = *location;
            let data = states.data_map.get::<RendererSurfaceStateUserData>();

            if let Some(data) = data {
                let data = &*data.lock().unwrap();

                if let Some(view) = data.view() {
                    location += view.offset.to_f64();
                    TraversalAction::DoChildren(location)
                } else {
                    TraversalAction::SkipChildren
                }
            } else {
                TraversalAction::SkipChildren
            }
        },
        |_, states, location| {
            let mut location = *location;
            let data = states.data_map.get::<RendererSurfaceStateUserData>();

            if let Some(data) = data {
                let Some(view) = data.lock().unwrap().view() else {
                    return;
                };
                location += view.offset.to_f64();

                if let Err(err) = import_surface(renderer, states) {
                    warn!("failed to import surface: {err:?}");
                    return;
                }

                let data = data.lock().unwrap();
                let Some(texture) = data.texture(renderer.context_id()) else {
                    return;
                };

                // Snapshots outlive the surface state, so bake the conversion in.
                let texture = match surface_image_description(states) {
                    Some((desc, intent)) => convert_texture(renderer, texture, desc, intent)
                        .unwrap_or_else(|err| {
                            warn!("error converting surface texture: {err:?}");
                            texture.clone()
                        }),
                    None => texture.clone(),
                };

                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    f64::from(data.buffer_scale()),
                    data.buffer_transform(),
                    Vec::new(),
                );

                let baked = BakedBuffer {
                    buffer,
                    location,
                    src: Some(view.src),
                    dst: Some(view.dst),
                };

                storage.push(baked);
            }
        },
        |_, _, _| true,
    );
}

pub fn push_elements_from_surface_tree<R>(
    renderer: &mut R,
    surface: &WlSurface,
    // Fractional scale expects surface buffers to be aligned to physical pixels.
    location: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
    kind: Kind,
    push: &mut dyn FnMut(SurfaceRenderElement<R>),
) where
    R: NiriRenderer,
{
    let _span = tracy_client::span!("push_elements_from_surface_tree");

    let location = location.to_f64();

    with_surface_tree_downward(
        surface,
        location,
        |_, states, location| {
            let mut location = *location;
            let data = states.data_map.get::<RendererSurfaceStateUserData>();

            if let Some(data) = data {
                if let Some(view) = data.lock().unwrap().view() {
                    location += view.offset.to_f64().to_physical(scale);
                    TraversalAction::DoChildren(location)
                } else {
                    TraversalAction::SkipChildren
                }
            } else {
                TraversalAction::SkipChildren
            }
        },
        |surface, states, location| {
            let mut location = *location;
            let data = states.data_map.get::<RendererSurfaceStateUserData>();

            if let Some(data) = data {
                let has_view = if let Some(view) = data.lock().unwrap().view() {
                    location += view.offset.to_f64().to_physical(scale);
                    true
                } else {
                    false
                };

                if has_view {
                    match WaylandSurfaceRenderElement::from_surface(
                        renderer, surface, states, location, alpha, kind,
                    ) {
                        Ok(Some(elem)) => {
                            let color = surface_image_description(states);
                            push(SurfaceRenderElement::new(elem, color));
                        }
                        Ok(None) => {} // surface is not mapped
                        Err(err) => {
                            warn!("failed to import surface: {}", err);
                        }
                    };
                }
            }
        },
        |_, _, _| true,
    );
}

/// Converts a surface texture into the blend space, keeping the full dynamic range.
fn convert_texture(
    renderer: &mut GlesRenderer,
    texture: &GlesTexture,
    desc: ImageDescription,
    intent: RenderIntent,
) -> anyhow::Result<GlesTexture> {
    let target = ColorTarget::Hdr { headroom: 10000. };
    let Some(transform) = desc.transform_to_blend(intent, target) else {
        return Ok(texture.clone());
    };

    let program = Shaders::get(renderer)
        .color_surface
        .clone()
        .context("missing color surface shader")?;

    let buffer_size = texture.size();
    let size = buffer_size.to_logical(1, Transform::Normal).to_physical(1);
    let rect = Rectangle::from_size(size);

    let mut converted: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr16161616f, buffer_size)
        .context("error creating texture")?;
    {
        let mut target = renderer.bind(&mut converted)?;
        let mut frame = renderer.render(&mut target, size, Transform::Normal)?;
        frame.clear(Color32F::TRANSPARENT, &[rect])?;
        frame.render_texture_from_to(
            texture,
            Rectangle::from_size(buffer_size).to_f64(),
            rect,
            &[rect],
            &[],
            Transform::Normal,
            1.,
            Some(&program),
            &color_transform_uniforms(&transform),
        )?;
        let _sync = frame.finish()?;
    }

    Ok(converted)
}
