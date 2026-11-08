//! wgpu setup, video (YUV -> RGB) rendering and egui painting.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use egui_wgpu::wgpu;
use gstreamer as gst;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use winit::window::Window;

const SHADER: &str = r#"
struct Params {
    r: vec4<f32>,
    g: vec4<f32>,
    b: vec4<f32>,
    fmt: vec4<f32>,
};
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var out: VsOut;
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv).r;
    let su = textureSample(tex_u, samp, in.uv);
    let sv = textureSample(tex_v, samp, in.uv).r;
    let nv12 = params.fmt.x < 0.5;
    let u = su.r;
    let v = select(sv, su.g, nv12);
    let yuv = vec4<f32>(y, u, v, 1.0);
    let rgb = vec3<f32>(dot(params.r, yuv), dot(params.g, yuv), dot(params.b, yuv));
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlaneLayout {
    Nv12,
    I420,
}

struct VideoTextures {
    width: u32,
    height: u32,
    layout: PlaneLayout,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    textures: Option<VideoTextures>,
    matrix_key: Option<(i32, bool, PlaneLayout)>,
}

/// Rows of a matrix mapping (Y, U, V, 1) to (R, G, B).
fn yuv_matrix(matrix: gst_video::VideoColorMatrix, full_range: bool) -> [[f32; 4]; 3] {
    let (kr, kb) = match matrix {
        gst_video::VideoColorMatrix::Bt601 | gst_video::VideoColorMatrix::Smpte240m => (0.299, 0.114),
        gst_video::VideoColorMatrix::Bt2020 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let rv = 2.0 * (1.0 - kr);
    let gu = -2.0 * kb * (1.0 - kb) / kg;
    let gv = -2.0 * kr * (1.0 - kr) / kg;
    let bu = 2.0 * (1.0 - kb);
    let (ys, yo, cs) = if full_range { (1.0, 0.0, 1.0) } else { (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0) };
    let co = 128.0 / 255.0;
    [
        [ys, 0.0, rv * cs, -ys * yo - rv * cs * co],
        [ys, gu * cs, gv * cs, -ys * yo - (gu + gv) * cs * co],
        [ys, bu * cs, 0.0, -ys * yo - bu * cs * co],
    ]
}

impl VideoRenderer {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                multisampled: false,
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv_pipeline_layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv_params"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { pipeline, layout, sampler, uniform, textures: None, matrix_key: None }
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.textures.as_ref().map(|t| (t.width, t.height))
    }

    pub fn clear(&mut self) {
        self.textures = None;
        self.matrix_key = None;
    }

    fn ensure_textures(&mut self, device: &wgpu::Device, w: u32, h: u32, layout: PlaneLayout) {
        if self.textures.as_ref().is_some_and(|t| t.width == w && t.height == h && t.layout == layout) {
            return;
        }
        let tex = |label, width, height, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y = tex("video_y", w, h, wgpu::TextureFormat::R8Unorm);
        let (u, v) = match layout {
            PlaneLayout::Nv12 => (
                tex("video_uv", cw, ch, wgpu::TextureFormat::Rg8Unorm),
                tex("video_unused", 1, 1, wgpu::TextureFormat::R8Unorm),
            ),
            PlaneLayout::I420 => (
                tex("video_u", cw, ch, wgpu::TextureFormat::R8Unorm),
                tex("video_v", cw, ch, wgpu::TextureFormat::R8Unorm),
            ),
        };
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let (vy, vu, vv) = (view(&y), view(&u), view(&v));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv_bind_group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&vy) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&vu) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&vv) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        log::info!("Video textures: {w}x{h} {layout:?}");
        self.textures = Some(VideoTextures { width: w, height: h, layout, y, u, v, bind_group });
    }

    /// Upload a decoded frame (NV12 or I420 in system memory).
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, sample: &gst::Sample) -> Result<()> {
        let caps = sample.caps().ok_or_else(|| anyhow!("sample without caps"))?;
        let info = gst_video::VideoInfo::from_caps(caps).context("video caps")?;
        let layout = match info.format() {
            gst_video::VideoFormat::Nv12 => PlaneLayout::Nv12,
            gst_video::VideoFormat::I420 => PlaneLayout::I420,
            f => return Err(anyhow!("unsupported video format {f:?}")),
        };
        let buffer = sample.buffer().ok_or_else(|| anyhow!("sample without buffer"))?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|_| anyhow!("cannot map video frame"))?;
        let (w, h) = (info.width(), info.height());
        self.ensure_textures(device, w, h, layout);

        let colorimetry = info.colorimetry();
        let full = colorimetry.range() == gst_video::VideoColorRange::Range0_255;
        let key = (colorimetry.matrix().into_glib_i32(), full, layout);
        if self.matrix_key != Some(key) {
            self.matrix_key = Some(key);
            let m = yuv_matrix(colorimetry.matrix(), full);
            let fmt = if layout == PlaneLayout::Nv12 { 0.0f32 } else { 1.0 };
            let mut data = [0f32; 16];
            data[0..4].copy_from_slice(&m[0]);
            data[4..8].copy_from_slice(&m[1]);
            data[8..12].copy_from_slice(&m[2]);
            data[12] = fmt;
            queue.write_buffer(&self.uniform, 0, bytemuck_cast(&data));
        }

        let t = self.textures.as_ref().unwrap();
        let strides = frame.plane_stride();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let write = |tex: &wgpu::Texture, plane: u32, width: u32, height: u32| -> Result<()> {
            let data = frame.plane_data(plane).map_err(|_| anyhow!("missing plane {plane}"))?;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(strides[plane as usize] as u32),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
            Ok(())
        };
        write(&t.y, 0, w, h)?;
        write(&t.u, 1, cw, ch)?;
        if layout == PlaneLayout::I420 {
            write(&t.v, 2, cw, ch)?;
        }
        Ok(())
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, viewport: [f32; 4]) {
        let Some(t) = &self.textures else { return };
        pass.set_viewport(viewport[0], viewport[1], viewport[2], viewport[3], 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &t.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn bytemuck_cast(data: &[f32; 16]) -> &[u8] {
    // SAFETY: f32 has no padding and any bit pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}

trait IntoGlibI32 {
    fn into_glib_i32(self) -> i32;
}
impl IntoGlibI32 for gst_video::VideoColorMatrix {
    fn into_glib_i32(self) -> i32 {
        use gst::glib::translate::IntoGlib;
        self.into_glib()
    }
}

/// Where to draw a video of `video` size inside `window` keeping aspect ratio (x, y, w, h).
pub fn letterbox(window: (u32, u32), video: (u32, u32)) -> [f32; 4] {
    let (ww, wh) = (window.0.max(1) as f32, window.1.max(1) as f32);
    let (vw, vh) = (video.0.max(1) as f32, video.1.max(1) as f32);
    let scale = (ww / vw).min(wh / vh);
    let (w, h) = ((vw * scale).round(), (vh * scale).round());
    [((ww - w) / 2.0).floor(), ((wh - h) / 2.0).floor(), w, h]
}

pub struct EguiFrame {
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures: egui::TexturesDelta,
    pub pixels_per_point: f32,
}

pub struct Gfx {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    modes: Vec<wgpu::PresentMode>,
    egui: egui_wgpu::Renderer,
    pub video: VideoRenderer,
    pub adapter_name: String,
}

impl Gfx {
    pub fn new(window: Arc<Window>) -> Result<Gfx> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window.clone()).context("create surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no suitable graphics adapter")?;
        let adapter_name = format!("{} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        log::info!("Graphics adapter: {adapter_name}");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pc-connector"),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("create graphics device")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        // Video and egui both output gamma-encoded values, so render to a non-sRGB view.
        let view_format = format.remove_srgb_suffix();
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow!("surface not supported by adapter"))?;
        config.format = format;
        config.view_formats = vec![view_format];
        config.desired_maximum_frame_latency = 1;
        config.present_mode = wgpu::PresentMode::Fifo;
        surface.configure(&device, &config);

        let egui = egui_wgpu::Renderer::new(&device, view_format, egui_wgpu::RendererOptions::default());
        let video = VideoRenderer::new(&device, view_format);
        Ok(Gfx { surface, device, queue, config, view_format, modes: caps.present_modes, egui, video, adapter_name })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// vsync=false: lowest latency (mailbox if available, else immediate/tearing).
    pub fn set_vsync(&mut self, vsync: bool) {
        use wgpu::PresentMode::*;
        let wanted: &[wgpu::PresentMode] = if vsync { &[Fifo] } else { &[Mailbox, Immediate, Fifo] };
        let mode = wanted.iter().copied().find(|m| self.modes.contains(m)).unwrap_or(Fifo);
        if mode != self.config.present_mode {
            log::info!("Present mode: {mode:?}");
            self.config.present_mode = mode;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn upload_frame(&mut self, sample: &gst::Sample) {
        if let Err(e) = self.video.upload(&self.device, &self.queue, sample) {
            log::warn!("Frame upload failed: {e:#}");
        }
    }

    /// Draw the video (letterboxed) and/or an egui frame, then present.
    pub fn render(&mut self, show_video: bool, mut egui: Option<EguiFrame>) {
        // Texture updates must be applied even if this frame can't be presented,
        // otherwise egui's font atlas would get out of sync.
        if let Some(e) = &mut egui {
            for (id, deltas) in &e.textures.set {
                for delta in deltas.iter() {
                    self.egui.update_texture(&self.device, &self.queue, *id, delta);
                }
            }
        }
        self.draw_and_present(show_video, egui.as_ref());
        if let Some(e) = &mut egui {
            for id in &e.textures.free {
                self.egui.free_texture(id);
            }
            e.textures.clear();
        }
    }

    fn draw_and_present(&mut self, show_video: bool, egui: Option<&EguiFrame>) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                t
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor { format: Some(self.view_format), ..Default::default() });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: egui.map(|e| e.pixels_per_point).unwrap_or(1.0),
        };
        if let Some(e) = egui {
            let cmds = self.egui.update_buffers(&self.device, &self.queue, &mut encoder, &e.primitives, &screen);
            if !cmds.is_empty() {
                self.queue.submit(cmds);
            }
        }
        {
            let clear =
                if show_video { wgpu::Color::BLACK } else { wgpu::Color { r: 0.035, g: 0.04, b: 0.05, a: 1.0 } };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if show_video && let Some(vs) = self.video.video_size() {
                let vp = letterbox((self.config.width, self.config.height), vs);
                self.video.draw(&mut pass, vp);
                pass.set_viewport(0.0, 0.0, self.config.width as f32, self.config.height as f32, 0.0, 1.0);
            }
            if let Some(e) = egui {
                let mut pass = pass.forget_lifetime();
                self.egui.render(&mut pass, &e.primitives, &screen);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
    }

    pub fn max_texture_side(&self) -> usize {
        self.device.limits().max_texture_dimension_2d as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &[[f32; 4]; 3], yuv: [f32; 3]) -> [f32; 3] {
        let v = [yuv[0], yuv[1], yuv[2], 1.0];
        let dot = |r: &[f32; 4]| r.iter().zip(v.iter()).map(|(a, b)| a * b).sum::<f32>();
        [dot(&m[0]), dot(&m[1]), dot(&m[2])]
    }

    #[test]
    fn limited_range_bt709() {
        let m = yuv_matrix(gst_video::VideoColorMatrix::Bt709, false);
        let black = apply(&m, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let white = apply(&m, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        for c in black {
            assert!(c.abs() < 1e-3, "{black:?}");
        }
        for c in white {
            assert!((c - 1.0).abs() < 1e-3, "{white:?}");
        }
        // Pure red in BT.709 limited range: Y=63, Cb=102, Cr=240
        let red = apply(&m, [63.0 / 255.0, 102.0 / 255.0, 240.0 / 255.0]);
        assert!(red[0] > 0.97 && red[1] < 0.03 && red[2] < 0.03, "{red:?}");
    }

    #[test]
    fn letterboxing() {
        assert_eq!(letterbox((1920, 1080), (1920, 1080)), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(letterbox((2880, 1920), (1920, 1080)), [0.0, 150.0, 2880.0, 1620.0]);
        assert_eq!(letterbox((1000, 1000), (1920, 1080)), [0.0, 218.0, 1000.0, 563.0]);
    }
}
