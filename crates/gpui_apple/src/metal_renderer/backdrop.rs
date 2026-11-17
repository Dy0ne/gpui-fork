//! Backdrop layers (`Styled::backdrop`) and GPU timing for `MetalRenderer`.
//!
//! Fork-only. Everything lives here rather than in `metal_renderer.rs`, which is upstream, so
//! upstream changes to the renderer merge against a handful of hooks instead of this code:
//! the `backdrop` field, `begin_backdrop_frame`/`end_backdrop_frame` in `render_frame`, and
//! `backdrop_textures`, `quads_below_layer`, `draw_backdrop_layers`, `resume_scene_pass` and
//! `finish_backdrop_frame` in `draw_primitives_to_texture`.
//!
//! Upstream draws a frame in one render pass: every opaque quad first, front to back with
//! depth writes, then the batches in paint order, depth-tested against those quads. A backdrop
//! layer samples what is behind it, so a frame with layers is split into one pass per run of
//! layers. Each pass draws only the opaque quads painted in its part of the frame; drawing
//! them all up front would let a layer blur quads that are painted on top of it. A later pass
//! is entirely above an earlier one, so its depth buffer starts cleared and never has to be
//! stored (upstream keeps it memoryless on Apple GPUs).

use super::{GlobalInputIndex, InstanceBindings, MetalRenderer};
use crate::gpu_timer::{self, GpuTimer};
use anyhow::{Context as _, Result};
use gpui::{
    BackdropLayer, Bounds, Corners, DevicePixels, GpuFrameStats, Rgba, ScaledBackdropEffect,
    ScaledPixels, Scene, Size, quad_depth_scale, size,
};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLCommandBuffer, MTLCommandEncoder, MTLDevice, MTLLibrary,
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLScissorRect,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage, MTLViewport,
};
use std::{mem, ops::Range, ptr};

/// How many of `scene.quads` are painted below `scene.backdrop_layers[layer]`, or all of them
/// if there is no such layer: where the frame's quads split between the render pass before
/// that layer and the one after it.
pub(super) fn quads_below_layer(scene: &Scene, layer: usize) -> usize {
    scene
        .backdrop_layers
        .get(layer)
        .map_or(scene.quads.len(), |layer| {
            // Both are sorted by draw order, and `Scene::batches` puts a layer after the quads
            // that share its order.
            scene
                .quads
                .partition_point(|quad| quad.order <= layer.order)
        })
}

pub(super) struct BackdropRenderer {
    downsample_pipeline_state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    blur_pipeline_state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    composite_pipeline_state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    copy_pipeline_state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// Created the first time a frame has backdrop layers, and resized with the viewport.
    textures: Option<BackdropTextures>,
    gpu_timer: GpuTimer,
}

impl BackdropRenderer {
    pub(super) fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        library: &ProtocolObject<dyn MTLLibrary>,
    ) -> Result<Self> {
        Ok(Self {
            downsample_pipeline_state: build_backdrop_pipeline_state(
                device,
                library,
                "backdrop_downsample",
                "backdrop_fullscreen_vertex",
                "backdrop_downsample_fragment",
                false,
            )?,
            blur_pipeline_state: build_backdrop_pipeline_state(
                device,
                library,
                "backdrop_blur",
                "backdrop_fullscreen_vertex",
                "backdrop_blur_fragment",
                false,
            )?,
            composite_pipeline_state: build_backdrop_pipeline_state(
                device,
                library,
                "backdrop_composite",
                "backdrop_composite_vertex",
                "backdrop_composite_fragment",
                true,
            )?,
            copy_pipeline_state: build_backdrop_pipeline_state(
                device,
                library,
                "backdrop_copy",
                "backdrop_fullscreen_vertex",
                "backdrop_copy_fragment",
                false,
            )?,
            textures: None,
            gpu_timer: GpuTimer::default(),
        })
    }
}

impl MetalRenderer {
    /// GPU timing of the last completed frame; the first call turns timing on.
    pub fn gpu_frame_stats(&mut self) -> Option<GpuFrameStats> {
        self.backdrop.gpu_timer.stats(&self.device)
    }

    pub(super) fn begin_backdrop_frame(&mut self, scene: &Scene) {
        self.backdrop
            .gpu_timer
            .begin_frame(&self.device, scene.backdrop_layers.len());
    }

    pub(super) fn end_backdrop_frame(
        &mut self,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) {
        self.backdrop
            .gpu_timer
            .end_frame(&self.device, command_buffer);
    }

    /// Offscreen targets for `scene`, if it has backdrop layers. Those sample what was rendered
    /// before them, and a drawable can't be sampled, so such frames render into the returned
    /// `scene` texture and `finish_backdrop_frame` copies it to the drawable.
    pub(super) fn backdrop_textures(
        &mut self,
        scene: &Scene,
        viewport_size: Size<DevicePixels>,
    ) -> Result<Option<BackdropTextures>> {
        let (width, height) = (viewport_size.width.0, viewport_size.height.0);
        // Zero-sized texture creation aborts.
        if scene.backdrop_layers.is_empty() || width <= 0 || height <= 0 {
            return Ok(None);
        }
        let up_to_date = self.backdrop.textures.as_ref().is_some_and(|textures| {
            textures.scene.width() == width as usize && textures.scene.height() == height as usize
        });
        if !up_to_date {
            let new_texture = |width: i32, height: i32| {
                let descriptor = MTLTextureDescriptor::new();
                // SAFETY: The dimensions are at least 1 and this descriptor creates a 2D texture.
                unsafe {
                    descriptor.setWidth(width.max(1) as usize);
                    descriptor.setHeight(height.max(1) as usize);
                }
                descriptor.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
                descriptor.setStorageMode(MTLStorageMode::Private);
                descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
                self.device
                    .newTextureWithDescriptor(&descriptor)
                    .context("allocating backdrop texture")
            };
            self.backdrop.textures = Some(BackdropTextures {
                scene: new_texture(width, height)?,
                ping: new_texture(width / 2, height / 2)?,
                pong: new_texture(width / 2, height / 2)?,
            });
        }
        Ok(self.backdrop.textures.clone())
    }

    /// Draws `layers` into `textures.scene`. The caller has ended its render pass on it.
    pub(super) fn draw_backdrop_layers(
        &self,
        layers: &[BackdropLayer],
        textures: Option<&BackdropTextures>,
        viewport_size: Size<DevicePixels>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<()> {
        let Some(textures) = textures else {
            return Ok(());
        };
        for layer in layers {
            self.draw_backdrop_layer(layer, textures, viewport_size, command_buffer)?;
        }
        Ok(())
    }

    /// Continues rendering the scene into `target` after `draw_backdrop_layers`, in a new pass
    /// like the one upstream's `draw_primitives_to_texture` starts the frame with, except that
    /// it keeps `target` and draws only `opaque_quads`: those painted above the layers just
    /// drawn and below the next ones.
    pub(super) fn resume_scene_pass(
        &self,
        scene: &Scene,
        opaque_quads: Range<usize>,
        instance_bindings: &InstanceBindings,
        target: &ProtocolObject<dyn MTLTexture>,
        depth_texture: &ProtocolObject<dyn MTLTexture>,
        viewport_size: Size<DevicePixels>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>> {
        let descriptor = MTLRenderPassDescriptor::new();
        // SAFETY: Index zero is within the render pass's color attachment array.
        let color_attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        color_attachment.setTexture(Some(target));
        color_attachment.setLoadAction(MTLLoadAction::Load);
        color_attachment.setStoreAction(MTLStoreAction::Store);
        // Everything in this pass is above everything in the earlier ones, so it needs none of
        // their depth.
        let depth_attachment = descriptor.depthAttachment();
        depth_attachment.setTexture(Some(depth_texture));
        depth_attachment.setLoadAction(MTLLoadAction::Clear);
        depth_attachment.setClearDepth(0.0);
        depth_attachment.setStoreAction(MTLStoreAction::DontCare);

        let command_encoder = command_buffer
            .renderCommandEncoderWithDescriptor(&descriptor)
            .context("creating render command encoder after backdrop layers")?;
        command_encoder.setViewport(MTLViewport {
            originX: 0.0,
            originY: 0.0,
            width: i32::from(viewport_size.width) as f64,
            height: i32::from(viewport_size.height) as f64,
            znear: 0.0,
            zfar: 1.0,
        });
        let depth_scale = quad_depth_scale(scene.quads.len());
        // SAFETY: Metal copies both values, which match every vertex shader's
        // `GlobalInputIndex_ViewportSize` and `GlobalInputIndex_DepthScale` arguments.
        unsafe {
            command_encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(&viewport_size).cast(),
                mem::size_of_val(&viewport_size),
                GlobalInputIndex::ViewportSize as usize,
            );
            command_encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(&depth_scale).cast(),
                mem::size_of_val(&depth_scale),
                GlobalInputIndex::DepthScale as usize,
            );
        }
        self.draw_opaque_quads(opaque_quads, instance_bindings, &command_encoder);
        command_encoder.setDepthStencilState(Some(&self.depth_test_state));
        Ok(command_encoder)
    }

    /// Copies a frame rendered offscreen for its backdrop layers to `texture`.
    pub(super) fn finish_backdrop_frame(
        &self,
        textures: Option<&BackdropTextures>,
        texture: &ProtocolObject<dyn MTLTexture>,
        viewport_size: Size<DevicePixels>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<()> {
        let Some(textures) = textures else {
            return Ok(());
        };
        self.run_backdrop_pass(
            command_buffer,
            &self.backdrop.copy_pipeline_state,
            texture,
            &textures.scene,
            viewport_size,
            None,
            &BackdropParams::default(),
            false,
        )
    }

    /// Blurs the scene behind `layer` at half resolution into `textures.ping`, then draws it
    /// back into the scene through the layer's effect.
    fn draw_backdrop_layer(
        &self,
        layer: &BackdropLayer,
        textures: &BackdropTextures,
        viewport_size: Size<DevicePixels>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<()> {
        let half_size: Size<DevicePixels> = size(
            (textures.ping.width() as i32).into(),
            (textures.ping.height() as i32).into(),
        );
        let mut params = BackdropParams {
            bounds: layer.bounds,
            content_mask: layer.content_mask.bounds,
            corner_radii: layer.corner_radii,
            opacity: layer.opacity,
            ..BackdropParams::default()
        };
        // How far outside its bounds the composite samples the backdrop.
        let mut reach = 0.;
        if let ScaledBackdropEffect::LiquidGlass {
            refraction,
            rim,
            magnify,
            aberration,
            tint,
            rim_tone,
        } = layer.effect
        {
            let tint = Rgba::from(tint);
            params.effect = 1.;
            params.refraction = refraction.0;
            params.rim = rim.0;
            params.magnify = magnify;
            params.aberration = aberration;
            params.tint = [tint.r, tint.g, tint.b, tint.a];
            params.rim_tone = rim_tone;
            let half_extent = layer.bounds.size.width.0.max(layer.bounds.size.height.0) * 0.5;
            let minified = if magnify < 1. {
                (1. / magnify.max(0.01) - 1.) * half_extent
            } else {
                0.
            };
            reach = refraction.0 * (1. + aberration.abs()) + minified;
        }

        // Only the region the composite reads needs blurring; the extra 3 sigma keeps the
        // blur from pulling in texels left over from earlier layers at the region's edge.
        let region = layer
            .bounds
            .dilate(ScaledPixels(reach + 3. * layer.blur.0 + 2.));
        let Some(scissor) = half_resolution_scissor(region, half_size) else {
            return Ok(());
        };

        self.run_backdrop_pass(
            command_buffer,
            &self.backdrop.downsample_pipeline_state,
            &textures.ping,
            &textures.scene,
            half_size,
            Some(scissor),
            &params,
            false,
        )?;

        // Sigma is halved because the blur runs at half resolution.
        let sigma = layer.blur.0 * 0.5;
        if sigma > 0. {
            // Span 3 sigma; past 32 taps, spread them out rather than truncating the kernel.
            let ideal_taps = (3. * sigma).ceil();
            params.blur_sigma = sigma;
            params.blur_tap_count = ideal_taps.clamp(1., 32.);
            params.blur_tap_step = (ideal_taps / params.blur_tap_count).max(1.);
            params.blur_direction = [1. / i32::from(half_size.width) as f32, 0.];
            self.run_backdrop_pass(
                command_buffer,
                &self.backdrop.blur_pipeline_state,
                &textures.pong,
                &textures.ping,
                half_size,
                Some(scissor),
                &params,
                false,
            )?;
            params.blur_direction = [0., 1. / i32::from(half_size.height) as f32];
            self.run_backdrop_pass(
                command_buffer,
                &self.backdrop.blur_pipeline_state,
                &textures.ping,
                &textures.pong,
                half_size,
                Some(scissor),
                &params,
                false,
            )?;
        }

        self.run_backdrop_pass(
            command_buffer,
            &self.backdrop.composite_pipeline_state,
            &textures.scene,
            &textures.ping,
            viewport_size,
            None,
            &params,
            true,
        )
    }

    fn run_backdrop_pass(
        &self,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        pipeline_state: &ProtocolObject<dyn MTLRenderPipelineState>,
        target: &ProtocolObject<dyn MTLTexture>,
        source: &ProtocolObject<dyn MTLTexture>,
        target_size: Size<DevicePixels>,
        scissor: Option<MTLScissorRect>,
        params: &BackdropParams,
        preserve_target: bool,
    ) -> Result<()> {
        // Passes that overwrite their target skip loading or clearing it. The final copy covers the
        // whole target; the downsample and blurs draw only their scissored region, and the
        // composite reads only inside it (the region includes 3 sigma of margin), so what is
        // outside never shows. Measured about 14% off the blur passes.
        let load_action = if preserve_target {
            MTLLoadAction::Load
        } else {
            MTLLoadAction::DontCare
        };
        let pass = if ptr::eq(pipeline_state, &*self.backdrop.copy_pipeline_state) {
            gpu_timer::Pass::BackdropCopy
        } else if ptr::eq(pipeline_state, &*self.backdrop.composite_pipeline_state) {
            gpu_timer::Pass::BackdropComposite
        } else {
            gpu_timer::Pass::BackdropBlur
        };
        let encoder = gpu_timer::new_timed_command_encoder(
            &self.backdrop.gpu_timer,
            pass,
            command_buffer,
            target,
            target_size,
            load_action,
        )?;
        if let Some(scissor) = scissor {
            encoder.setScissorRect(scissor);
        }
        encoder.setRenderPipelineState(pipeline_state);
        // SAFETY: The copied params and size, and source texture match the backdrop shaders'
        // slots; `BackdropParams` mirrors the shader struct byte for byte.
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(params).cast(),
                mem::size_of::<BackdropParams>(),
                BackdropInputIndex::Params as usize,
            );
            encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(&target_size).cast(),
                mem::size_of_val(&target_size),
                BackdropInputIndex::ViewportSize as usize,
            );
            encoder.setFragmentBytes_length_atIndex(
                ptr::NonNull::from(params).cast(),
                mem::size_of::<BackdropParams>(),
                BackdropInputIndex::Params as usize,
            );
            encoder.setFragmentTexture_atIndex(Some(source), 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        }
        encoder.endEncoding();
        Ok(())
    }
}

/// Like upstream's `build_pipeline_state`, but for the backdrop passes, which have no depth
/// attachment. Only the composite blends: the passes that copy, downsample and blur whole
/// textures must replace pixels, not blend over them.
fn build_backdrop_pipeline_state(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    blending: bool,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>> {
    let vertex_fn = library
        .newFunctionWithName(&NSString::from_str(vertex_fn_name))
        .with_context(|| format!("locating vertex function {vertex_fn_name}"))?;
    let fragment_fn = library
        .newFunctionWithName(&NSString::from_str(fragment_fn_name))
        .with_context(|| format!("locating fragment function {fragment_fn_name}"))?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setLabel(Some(&NSString::from_str(label)));
    descriptor.setVertexFunction(Some(vertex_fn.as_ref()));
    descriptor.setFragmentFunction(Some(fragment_fn.as_ref()));
    // SAFETY: Index zero is within the pipeline's color attachment array.
    let color_attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    color_attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    color_attachment.setBlendingEnabled(blending);
    color_attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    color_attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    color_attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    color_attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    color_attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .with_context(|| format!("creating {label} render pipeline"))
}

/// The half-resolution pixels covering `region` (in full-resolution device pixels), or `None`
/// if it's entirely off screen.
fn half_resolution_scissor(
    region: Bounds<ScaledPixels>,
    half_size: Size<DevicePixels>,
) -> Option<MTLScissorRect> {
    let (width, height) = (
        i32::from(half_size.width) as f32,
        i32::from(half_size.height) as f32,
    );
    let left = (region.origin.x.0 * 0.5).floor().clamp(0., width);
    let top = (region.origin.y.0 * 0.5).floor().clamp(0., height);
    let right = (region.right().0 * 0.5).ceil().clamp(0., width);
    let bottom = (region.bottom().0 * 0.5).ceil().clamp(0., height);
    (right > left && bottom > top).then(|| MTLScissorRect {
        x: left as usize,
        y: top as usize,
        width: (right - left) as usize,
        height: (bottom - top) as usize,
    })
}

/// Offscreen targets for frames with backdrop layers. The scene renders into `scene`, and each
/// layer downsamples and blurs it through `ping` and `pong` at half resolution.
#[derive(Clone)]
pub(super) struct BackdropTextures {
    pub(super) scene: Retained<ProtocolObject<dyn MTLTexture>>,
    ping: Retained<ProtocolObject<dyn MTLTexture>>,
    pong: Retained<ProtocolObject<dyn MTLTexture>>,
}

/// Mirrors `BackdropParams` in `backdrop.metal`, which reads it byte for byte through
/// `setVertexBytes`/`setFragmentBytes`, so field order and sizes must match. Every field
/// after `blur_direction` is 4 bytes, which keeps both layouts free of padding.
#[repr(C)]
#[derive(Clone, Copy)]
struct BackdropParams {
    bounds: Bounds<ScaledPixels>,
    content_mask: Bounds<ScaledPixels>,
    corner_radii: Corners<ScaledPixels>,
    blur_direction: [f32; 2],
    blur_sigma: f32,
    blur_tap_count: f32,
    blur_tap_step: f32,
    opacity: f32,
    /// 0 draws the blurred backdrop as is (frost), 1 through the liquid glass shader.
    effect: f32,
    refraction: f32,
    rim: f32,
    magnify: f32,
    aberration: f32,
    tint: [f32; 4],
    rim_tone: f32,
}

impl Default for BackdropParams {
    fn default() -> Self {
        Self {
            bounds: Bounds::default(),
            content_mask: Bounds::default(),
            corner_radii: Corners::default(),
            blur_direction: [0., 0.],
            blur_sigma: 0.,
            blur_tap_count: 0.,
            blur_tap_step: 1.,
            opacity: 1.,
            effect: 0.,
            refraction: 0.,
            rim: 0.,
            magnify: 1.,
            aberration: 0.,
            tint: [0.; 4],
            rim_tone: 0.,
        }
    }
}

/// Buffer slots of the backdrop shaders in `backdrop.metal`.
#[repr(C)]
enum BackdropInputIndex {
    Params = 1,
    ViewportSize = 2,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metal_renderer::InstanceBufferPool;
    use gpui::{ContentMask, Quad, bounds, point};
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn square(origin: f32, size: f32) -> Bounds<ScaledPixels> {
        bounds(
            point(ScaledPixels(origin), ScaledPixels(origin)),
            gpui::size(ScaledPixels(size), ScaledPixels(size)),
        )
    }

    fn quad(bounds: Bounds<ScaledPixels>, color: gpui::Hsla) -> Quad {
        Quad {
            bounds,
            content_mask: ContentMask { bounds },
            background: color.into(),
            ..Quad::default()
        }
    }

    /// Opaque quads are drawn ahead of the batches, and a layer must not blur the ones painted
    /// on top of it.
    #[test]
    fn backdrop_layer_blurs_only_what_is_below_it() -> Result<()> {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(square(0., 64.), gpui::red()));
        let layer_bounds = square(8., 48.);
        scene.insert_primitive(BackdropLayer {
            order: 0,
            bounds: layer_bounds,
            content_mask: ContentMask {
                bounds: layer_bounds,
            },
            corner_radii: Corners::default(),
            blur: ScaledPixels(4.),
            effect: ScaledBackdropEffect::Frost,
            opacity: 1.,
        });
        scene.insert_primitive(quad(square(24., 16.), gpui::blue()));
        scene.finish();
        assert_eq!(quads_below_layer(&scene, 0), 1);
        assert_eq!(quads_below_layer(&scene, 1), 2);

        let mut renderer =
            MetalRenderer::new_headless(Arc::new(Mutex::new(InstanceBufferPool::default())))?;
        let image =
            renderer.render_scene_to_image(&scene, size(DevicePixels(64), DevicePixels(64)))?;

        // On the layer, two pixels from the blue quad: well within the blur's reach of it.
        assert_eq!(image.get_pixel(21, 32).0, [255, 0, 0, 255]);
        // The quad itself is drawn over the layer, and the scene outside the layer is untouched.
        assert_eq!(image.get_pixel(32, 32).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(2, 2).0, [255, 0, 0, 255]);
        Ok(())
    }
}
