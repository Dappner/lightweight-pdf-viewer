//! Page tiles without colour, kept on the GPU as one byte a pixel instead of four.
//!
//! Most tiles of text pages are grey: every pixel has R = G = B (`render_spike`'s gray mode
//! counts them). egui only takes RGBA textures, so grey tiles get their own single-channel
//! (R8) wgpu textures, drawn with a paint callback whose shader is egui's own reading the one
//! channel as grey. The pixels on screen are exactly those egui draws for the same tile as
//! an RGBA texture (see the tests). On M-series Macs GPU memory is system memory, so this is
//! a quarter of the memory for those tiles, and lets the texture budget keep more of them.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui::{Color32, Painter, Rect, TextureHandle, TextureId, pos2};
use eframe::epaint::{Mesh, Vertex};
use egui_wgpu::{CallbackResources, CallbackTrait, RenderState, ScreenDescriptor, wgpu};

/// Whether grey tiles are drawn: headless (tests, benchmarks) nothing draws them, so their
/// pixels are not kept.
static GPU: AtomicBool = AtomicBool::new(false);

const UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// A grey tile: its pixels until first drawn, then its GPU texture. Dropping it frees both.
pub struct GreyTexture {
    size: [u32; 2],
    pixels: Mutex<Option<Vec<u8>>>,
    gpu: OnceLock<(wgpu::Texture, wgpu::BindGroup)>,
}

impl GreyTexture {
    pub fn new(size: [usize; 2], pixels: Vec<u8>) -> Self {
        let pixels = GPU.load(Ordering::Relaxed).then_some(pixels);
        Self { size: size.map(|v| v as u32), pixels: Mutex::new(pixels), gpu: OnceLock::new() }
    }
}

/// A cached tile's or thumbnail's texture. Dropping it frees it.
pub enum Texture {
    Egui(TextureHandle),
    Grey(Arc<GreyTexture>),
}

impl Texture {
    pub fn image(&self) -> Image {
        match self {
            Texture::Egui(handle) => Image::Egui(handle.id()),
            Texture::Grey(grey) => Image::Grey(grey.clone()),
        }
    }
}

/// A cached tile or thumbnail, ready to paint.
#[derive(Clone)]
pub enum Image {
    Egui(TextureId),
    Grey(Arc<GreyTexture>),
}

/// Paints `image` stretched over `rect`, as `Painter::image` with a white tint does.
pub fn paint(painter: &Painter, image: Image, rect: Rect) {
    paint_part(painter, image, rect, UV, Color32::WHITE);
}

/// Paints the `uv` part of `image` over `rect`, its pixels multiplied by `tint`, as
/// `Painter::image` does.
pub fn paint_part(painter: &Painter, image: Image, rect: Rect, uv: Rect, tint: Color32) {
    match image {
        Image::Egui(id) => painter.image(id, rect, uv, tint),
        Image::Grey(texture) => {
            let tile = Tile { texture, rect, uv, tint, slot: AtomicU32::new(0) };
            painter.add(egui_wgpu::Callback::new_paint_callback(rect, tile))
        }
    };
}

/// The pixels as one byte each, if they are all opaque grey. Only exact grey: anything else
/// would draw differently.
pub fn to_grey(pixels: &[Color32]) -> Option<Vec<u8>> {
    // Whole pixels as u32s, which vectorizes: opaque grey is 0xFF_vv_vv_vv.
    let words: &[u32] = bytemuck::cast_slice(pixels);
    let grey = words.iter().fold(true, |grey, &w| grey & (w == ((w & 0xFF) * 0x01_01_01) | 0xFF_00_00_00));
    grey.then(|| words.iter().map(|&w| w as u8).collect())
}

/// Sets up drawing grey tiles in the app's renderer. eframe's defaults are assumed for what
/// the pipeline must match: no multisampling and no depth buffer.
pub fn install(state: &RenderState, dithering: bool) {
    let resources = Resources::new(&state.device, state.target_format, dithering);
    state.renderer.write().callback_resources.insert(resources);
    GPU.store(true, Ordering::Relaxed);
}

/// egui's shader uniforms (`Locals` in egui.wgsl).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Locals {
    screen_size_in_points: [f32; 2],
    dithering: u32,
    predictable_texture_filtering: u32,
}

/// Kept in the renderer's callback resources.
struct Resources {
    pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    dithering: bool,
    locals: wgpu::Buffer,
    locals_group: wgpu::BindGroup,
    /// Six vertices for each grey tile in the frame being prepared.
    vertices: Vec<Vertex>,
    vertex_buffer: wgpu::Buffer,
    /// Whether `vertices` went to the GPU: the next `prepare` starts a new frame.
    uploaded: bool,
}

impl Resources {
    /// The same pipeline, bindings and sampler as egui-wgpu's `Renderer` makes, but for an R8
    /// texture.
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, dithering: bool) -> Self {
        let module = device.create_shader_module(wgpu::include_wgsl!("grey.wgsl"));
        let locals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grey tile locals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grey tile texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let locals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grey tile locals"),
            size: size_of::<Locals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let locals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("grey tile locals"),
            layout: &locals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: locals.as_entire_binding() }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grey tile"),
            bind_group_layouts: &[&locals_layout, &texture_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grey tile"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(if format.is_srgb() {
                    "fs_main_linear_framebuffer"
                } else {
                    "fs_main_gamma_framebuffer"
                }),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::OneMinusDstAlpha,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        // egui's sampler for `TextureOptions::LINEAR`.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("grey tile"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Self {
            pipeline,
            texture_layout,
            sampler,
            dithering,
            locals,
            locals_group,
            vertices: Vec::new(),
            vertex_buffer: vertex_buffer(device, 0),
            uploaded: true,
        }
    }

    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        grey: &GreyTexture,
    ) -> (wgpu::Texture, wgpu::BindGroup) {
        let [width, height] = grey.size;
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("grey tile"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        if let Some(pixels) = grey.pixels.lock().unwrap().take() {
            let layout =
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width), rows_per_image: Some(height) };
            queue.write_texture(texture.as_image_copy(), &pixels, layout, size);
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("grey tile"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        (texture, group)
    }
}

fn vertex_buffer(device: &wgpu::Device, vertices: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("grey tile vertices"),
        size: (vertices.max(6 * 64) * size_of::<Vertex>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// One grey tile, or part of one, painted this frame.
struct Tile {
    texture: Arc<GreyTexture>,
    rect: Rect,
    uv: Rect,
    tint: Color32,
    /// Where its vertices start in this frame's vertex buffer, set in `prepare`.
    slot: AtomicU32,
}

impl CallbackTrait for Tile {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<Resources>() else { return Vec::new() };
        if std::mem::take(&mut r.uploaded) {
            // The first tile of a frame (of any window: they are prepared and drawn in turn).
            r.vertices.clear();
            let ppp = screen.pixels_per_point;
            let locals = Locals {
                // As egui-wgpu computes it.
                screen_size_in_points: [screen.size_in_pixels[0] as f32 / ppp, screen.size_in_pixels[1] as f32 / ppp],
                dithering: u32::from(r.dithering),
                predictable_texture_filtering: 0,
            };
            queue.write_buffer(&r.locals, 0, bytemuck::bytes_of(&locals));
        }
        self.texture.gpu.get_or_init(|| r.upload(device, queue, &self.texture));
        self.slot.store(r.vertices.len() as u32, Ordering::Relaxed);
        // egui's own quad for an image, unindexed.
        let mut mesh = Mesh::default();
        mesh.add_rect_with_uv(self.rect, self.uv, self.tint);
        r.vertices.extend(mesh.indices.iter().map(|&i| mesh.vertices[i as usize]));
        Vec::new()
    }

    fn finish_prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<Resources>() else { return Vec::new() };
        if !r.uploaded {
            let bytes: &[u8] = bytemuck::cast_slice(&r.vertices);
            if r.vertex_buffer.size() < bytes.len() as u64 {
                r.vertex_buffer = vertex_buffer(device, r.vertices.len().next_power_of_two());
            }
            queue.write_buffer(&r.vertex_buffer, 0, bytes);
            r.uploaded = true;
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: eframe::epaint::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let (Some(r), Some((_, group))) = (resources.get::<Resources>(), self.texture.gpu.get()) else { return };
        // egui sets the viewport to the callback's rect rounded to whole pixels; its own meshes
        // are drawn in the whole screen's, which places them exactly.
        let [w, h] = info.screen_size_px;
        pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
        pass.set_pipeline(&r.pipeline);
        pass.set_bind_group(0, &r.locals_group, &[]);
        pass.set_bind_group(1, group, &[]);
        pass.set_vertex_buffer(0, r.vertex_buffer.slice(..));
        let start = self.slot.load(Ordering::Relaxed);
        pass.draw(start..start + 6, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{self, ColorImage, Context, RawInput, TextureOptions, vec2};

    use super::*;

    struct Gpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
    }

    fn gpu() -> Option<Gpu> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).ok()?;
        Some(Gpu { device, queue })
    }

    /// Draws `paint` into a `size` pixel `format` target through egui-wgpu, as eframe does,
    /// and reads the pixels back.
    fn draw(gpu: &Gpu, format: wgpu::TextureFormat, size: [u32; 2], ppp: f32, paint: impl FnMut(&Context)) -> Vec<u8> {
        let Gpu { device, queue } = gpu;
        let mut renderer = egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions::default());
        renderer.callback_resources.insert(Resources::new(device, format, true));
        let ctx = Context::default();
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size[0] as f32, size[1] as f32) / ppp)),
            ..Default::default()
        };
        ctx.set_pixels_per_point(ppp);
        let mut paint = paint;
        let output = ctx.run(input, |ctx| paint(ctx));
        let primitives = ctx.tessellate(output.shapes, ppp);
        for (id, delta) in &output.textures_delta.set {
            renderer.update_texture(device, queue, *id, delta);
        }
        let screen = ScreenDescriptor { size_in_pixels: size, pixels_per_point: ppp };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        let mut buffers = renderer.update_buffers(device, queue, &mut encoder, &primitives, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.2, g: 0.3, b: 0.4, a: 1.0 }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            renderer.render(&mut pass, &primitives, &screen);
        }
        let row = (size[0] * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * size[1]) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None },
            },
            target.size(),
        );
        buffers.push(encoder.finish());
        queue.submit(buffers);
        readback.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = readback.slice(..).get_mapped_range();
        data.chunks(row as usize).flat_map(|r| &r[..(size[0] * 4) as usize]).copied().collect()
    }

    #[test]
    fn only_opaque_grey_is_grey() {
        let grey: Vec<Color32> = (0..=255).map(Color32::from_gray).collect();
        assert_eq!(to_grey(&grey), Some((0..=255).collect()));
        for odd in [
            Color32::from_rgb(10, 10, 11),
            Color32::from_rgb(10, 11, 10),
            Color32::from_rgba_premultiplied(10, 10, 10, 254),
        ] {
            let mut pixels = grey.clone();
            pixels[100] = odd;
            assert_eq!(to_grey(&pixels), None, "{odd:?}");
        }
    }

    /// A grey tile drawn as R8 through the callback puts the same pixels on screen as egui
    /// drawing it as RGBA: at 1:1, off the pixel grid, scaled up and down (mid-zoom), and
    /// clipped. Noise is the hardest case for filtering to agree on.
    #[test]
    fn grey_tiles_draw_exactly_as_rgba() {
        let Some(gpu) = gpu() else {
            eprintln!("no GPU adapter: skipped");
            return;
        };
        GPU.store(true, Ordering::Relaxed);
        let [tw, th] = [300usize, 200];
        let mut seed = 12345u32;
        let grey: Vec<u8> = (0..tw * th)
            .map(|i| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                // Mostly paper white with text-like dark runs, plus noise everywhere.
                if (i / tw) % 12 < 5 && i % 7 != 0 { (seed >> 24) as u8 / 3 } else { 255 - (seed >> 28) as u8 }
            })
            .collect();
        let rgba: Vec<Color32> = grey.iter().map(|&v| Color32::from_gray(v)).collect();
        assert_eq!(to_grey(&rgba).as_deref(), Some(&grey[..]));

        let ppp = 2.0;
        let rects = [
            Rect::from_min_size(pos2(10.0, 10.0), vec2(tw as f32, th as f32) / ppp),
            Rect::from_min_size(pos2(170.3, 12.77), vec2(tw as f32, th as f32) / ppp),
            Rect::from_min_size(pos2(5.25, 120.6), vec2(tw as f32, th as f32) / ppp * 1.37),
            Rect::from_min_size(pos2(230.1, 130.4), vec2(tw as f32, th as f32) / ppp * 0.61),
        ];
        let clip = Rect::from_min_max(pos2(0.0, 0.0), pos2(390.0, 230.0));
        for format in [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Bgra8UnormSrgb] {
            let size = [800, 500];
            let egui = draw(&gpu, format, size, ppp, |ctx| {
                let texture = ctx.load_texture("t", ColorImage::new([tw, th], rgba.clone()), TextureOptions::LINEAR);
                let painter = ctx.layer_painter(egui::LayerId::background()).with_clip_rect(clip);
                for rect in rects {
                    paint(&painter, Image::Egui(texture.id()), rect);
                }
            });
            let grey_texture = Arc::new(GreyTexture::new([tw, th], grey.clone()));
            let ours = draw(&gpu, format, size, ppp, |ctx| {
                let painter = ctx.layer_painter(egui::LayerId::background()).with_clip_rect(clip);
                for rect in rects {
                    paint(&painter, Image::Grey(grey_texture.clone()), rect);
                }
            });
            let background = [egui[0], egui[1], egui[2]];
            let drawn = egui.chunks(4).filter(|p| p[..3] != background).count();
            assert!(drawn > 100_000, "{format:?}: only {drawn} pixels drawn");
            let differing = egui.chunks(4).zip(ours.chunks(4)).filter(|(a, b)| a != b).count();
            assert_eq!(differing, 0, "{format:?}: {differing} of {drawn} drawn pixels differ");
        }
    }
}
