//! Compositing of HDR outputs, see [`crate::color`].

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::{
    Element, Id, Kind, RenderElement, RenderElementStates, UnderlyingStorage,
};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::{
    CommitCounter, DamageBag, DamageSet, DamageSnapshot, OpaqueRegions,
};
use smithay::backend::renderer::{Color32F, ContextId, Frame as _, Renderer as _, Texture as _};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Size, Transform};

use super::renderer::{AsGlesFrame as _, NiriRenderer};
use super::shaders::{hdr_encode_uniforms, Shaders};
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::color::{self, ColorTarget, HdrEncode, HdrParams};

/// Floating point buffer that an HDR output is composited into.
#[derive(Debug)]
pub struct HdrBuffer {
    id: Id,
    inner: Option<Inner>,
}

#[derive(Debug)]
struct Inner {
    texture: GlesTexture,
    renderer_context_id: ContextId<GlesTexture>,
    scale: Scale<f64>,
    params: HdrParams,
    damage: OutputDamageTracker,
    outer_damage: DamageBag<i32, Buffer>,
}

/// The composited blend space, encoded to PQ as it's drawn.
#[derive(Debug, Clone)]
pub struct HdrEncodeRenderElement {
    id: Id,
    texture: GlesTexture,
    renderer_context_id: ContextId<GlesTexture>,
    damage: DamageSnapshot<i32, Buffer>,
    encode: HdrEncode,
}

impl HdrBuffer {
    pub fn new() -> Self {
        Self {
            id: Id::new(),
            inner: None,
        }
    }

    /// Whether the renderer can composite HDR outputs.
    pub fn is_supported<R: NiriRenderer>(renderer: &mut R) -> bool {
        if Shaders::get(renderer).hdr_encode.is_none() {
            return false;
        }

        // Binding checks that the texture is color-renderable.
        let texture: Result<GlesTexture, _> =
            renderer.create_buffer(Fourcc::Abgr16161616f, Size::from((1, 1)));
        texture.is_ok_and(|mut texture| renderer.bind(&mut texture).is_ok())
    }

    /// Renders `elements` into the blend space.
    ///
    /// `size` is the output size in physical pixels, after the output transform. The output
    /// transform is then applied when drawing the returned element.
    pub fn render<R: NiriRenderer>(
        &mut self,
        renderer: &mut R,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
        params: HdrParams,
        elements: &[impl RenderElement<R>],
    ) -> anyhow::Result<(HdrEncodeRenderElement, SyncPoint, RenderElementStates)> {
        let _span = tracy_client::span!("HdrBuffer::render");

        let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        let context_id = renderer.as_gles_renderer().context_id();

        let recreate = match &mut self.inner {
            None => true,
            Some(inner) => {
                inner.texture.size() != buffer_size
                    || inner.renderer_context_id != context_id
                    || !inner.texture.is_unique_reference()
            }
        };

        if recreate {
            self.inner = None;

            let texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr16161616f, buffer_size)
                .context("error creating floating point texture")?;

            self.inner = Some(Inner {
                texture,
                renderer_context_id: context_id,
                scale,
                params,
                damage: OutputDamageTracker::new(size, scale, Transform::Normal),
                outer_damage: DamageBag::default(),
            });
        }

        let inner = self.inner.as_mut().unwrap();

        // Element appearance depends on the parameters, so redraw everything when they change.
        if inner.scale != scale || inner.params != params {
            inner.scale = scale;
            inner.params = params;
            inner.damage = OutputDamageTracker::new(size, scale, Transform::Normal);
            inner.outer_damage = DamageBag::default();
        }

        let target = ColorTarget::Hdr {
            headroom: params.headroom(),
        };
        let res = color::with_target(target, || {
            let mut texture = inner.texture.clone();
            let mut fb = renderer.bind(&mut texture)?;
            inner
                .damage
                .render_output(renderer, &mut fb, 1, elements, Color32F::BLACK)
                .context("error rendering")
        })?;

        if let Some(damage) = res.damage {
            // Rendering to a texture with Normal transform: Physical is the same as Buffer.
            let size = size.to_logical(1);
            let damage = damage
                .iter()
                .map(|rect| rect.to_logical(1).to_buffer(1, Transform::Normal, &size));
            inner.outer_damage.add(damage);
        }

        let elem = HdrEncodeRenderElement {
            id: self.id.clone(),
            texture: inner.texture.clone(),
            renderer_context_id: inner.renderer_context_id.clone(),
            damage: inner.outer_damage.snapshot(),
            encode: params.encode(),
        };

        Ok((elem, res.sync, res.states))
    }
}

impl Default for HdrBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Element for HdrEncodeRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.damage.current_commit()
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let size = self.texture.size();
        Rectangle::from_size((size.w, size.h).into())
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size()).to_f64()
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self.damage.damage_since(commit) {
            // Buffer and Physical coincide here: Normal transform and scale 1.
            Some(damage) => damage
                .into_iter()
                .map(|rect| {
                    Rectangle::new(
                        (rect.loc.x, rect.loc.y).into(),
                        (rect.size.w, rect.size.h).into(),
                    )
                })
                .collect(),
            None => DamageSet::from_slice(&[self.geometry(Scale::from(1.))]),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::from_slice(&[self.geometry(scale)])
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for HdrEncodeRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        if frame.context_id() != self.renderer_context_id {
            warn!("trying to render texture from different renderer");
            return Ok(());
        }

        let Some(program) = Shaders::get_from_frame(frame).hdr_encode.clone() else {
            // Without the shader the result would be wrong anyway, draw something at least.
            return frame.render_texture_from_to(
                &self.texture,
                src,
                dst,
                damage,
                opaque_regions,
                Transform::Normal,
                1.,
                None,
                &[],
            );
        };

        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.,
            Some(&program),
            &hdr_encode_uniforms(&self.encode),
        )
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for HdrEncodeRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        RenderElement::<GlesRenderer>::draw(
            self,
            frame.as_gles_frame(),
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
