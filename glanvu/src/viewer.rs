// SPDX-License-Identifier: Apache-2.0

//! Phase 1 viewer: winit window + wgpu textured-quad pipeline + glyphon path overlay.
//!
//! Folder navigation and prefetch live in `nav::FolderNav`; this module owns only the GPU state
//! (`Gpu`), the view/transform state (`ViewState`), and the winit event loop (`App`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache as GlyphCache, Color, FontSystem, Metrics, Resolution,
    Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use arboard::Clipboard;
use glanvu_core::DecodedImage;

use glanvu_viewer_core::explorer::{
    ExplorerState, FONT as EXPLORER_FONT, LINE_H as EXPLORER_LINE_H, PANEL_W,
};
use glanvu_viewer_core::grid::{GridState, CELL_H, CELL_W, GAP, SEL_OUTSET};
use glanvu_viewer_core::nav::{locate, FolderNav};
use glanvu_viewer_core::thumb::{ThumbnailCache, THUMB_H, THUMB_W};

/// Maximum tile uniform buffers for the grid renderer (pool pre-allocated at GPU init).
/// Each visible tile needs up to 3 draw slots (bg + selection ring + thumbnail).
const TILE_POOL: usize = 384;

/// How long the path overlay stays visible after an action.
const OVERLAY_DURATION: Duration = Duration::from_millis(2000);

/// Whether to print timing diagnostics (set `GLANVU_PERF=1`). Off by default so runs stay quiet.
fn perf_logging() -> bool {
    std::env::var_os("GLANVU_PERF").is_some()
}

// ---------------------------------------------------------------------------
// GPU shaders
// ---------------------------------------------------------------------------

const WATERMARK_BYTES: &[u8] = include_bytes!("../../assets/AppIcon.png");

/// Version label shown under the watermark in the empty state.
const VERSION_LABEL: &str = concat!("v", env!("CARGO_PKG_VERSION"));

const KOFI_URL: &str = "https://ko-fi.com/juanyque";
const GITHUB_SPONSORS_URL: &str = "https://github.com/sponsors/juanyque";
/// Donate line, rendered blue and centered. The two link labels are roughly balanced in width so
/// a click split at the line's horizontal center cleanly separates Ko-fi (left) from Sponsors (right).
const DONATE_LINE: &str = "\u{2665}  Ko-fi Support  \u{00b7}  GitHub Sponsors";
/// About overlay body (white). The donate line is rendered separately (blue, centered) below it.
const ABOUT_HEAD: &str = concat!(
    "Glanvu  v",
    env!("CARGO_PKG_VERSION"),
    "\n\nFast, keyboard-driven, cross-platform image viewer\nand converter.\n\nApache-2.0 License",
);

/// A clickable donate-link zone (physical px), computed during render so the click handler does no
/// geometry of its own. A click inside `[x0,x1] × [y0,y1]` opens Ko-fi if `cx < split_x`, else Sponsors.
#[derive(Clone, Copy)]
struct DonateHit {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    split_x: f32,
}

const SHADER: &str = r#"
struct Uniforms { mvp: mat4x4<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.clip = u.mvp * vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

// ---------------------------------------------------------------------------
// Vertex / uniform types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    mvp: [[f32; 4]; 4],
}

const VERTICES: [Vertex; 4] = [
    Vertex {
        pos: [-0.5, 0.5],
        uv: [0.0, 0.0],
    },
    Vertex {
        pos: [-0.5, -0.5],
        uv: [0.0, 1.0],
    },
    Vertex {
        pos: [0.5, -0.5],
        uv: [1.0, 1.0],
    },
    Vertex {
        pos: [0.5, 0.5],
        uv: [1.0, 0.0],
    },
];
const INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

// ---------------------------------------------------------------------------
// Transform helpers
// ---------------------------------------------------------------------------

/// Pan/zoom/rotation state, independent of the GPU.
struct ViewState {
    fit: bool,
    zoom: f32,
    pan: (f32, f32),
    quarter_turns: u32,
}

impl ViewState {
    fn fit() -> Self {
        ViewState {
            fit: true,
            zoom: 1.0,
            pan: (0.0, 0.0),
            quarter_turns: 0,
        }
    }
}

/// MVP matrix mapping the unit quad to clip space (image transform, y-up center origin).
fn mvp(img: (u32, u32), win: (f32, f32), st: &ViewState) -> [[f32; 4]; 4] {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (win_w, win_h) = (win.0.max(1.0), win.1.max(1.0));
    let (bw, bh) = if st.quarter_turns % 2 == 1 {
        (ih, iw)
    } else {
        (iw, ih)
    };
    let base = if st.fit {
        (win_w / bw).min(win_h / bh)
    } else {
        1.0
    };
    let scale = base * st.zoom;
    let model = Mat4::from_scale(Vec3::new(iw * scale, ih * scale, 1.0));
    let rot = Mat4::from_rotation_z(st.quarter_turns as f32 * std::f32::consts::FRAC_PI_2);
    let trans = Mat4::from_translation(Vec3::new(st.pan.0, st.pan.1, 0.0));
    let proj = Mat4::from_scale(Vec3::new(2.0 / win_w, 2.0 / win_h, 1.0));
    (proj * trans * rot * model).to_cols_array_2d()
}

/// Ideal initial window size (logical px) for an image: capped to 1600x1000, scaled to 90%,
/// never smaller than 320x240. Used both when creating the window and when resizing to a newly
/// opened image (Open With / drop from the empty state).
fn ideal_window_size(iw: u32, ih: u32) -> (f32, f32) {
    let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
    let s = (1600.0 / iw).min(1000.0 / ih).min(1.0) * 0.9;
    ((iw * s).max(320.0), (ih * s).max(240.0))
}

/// MVP for a screen-space rectangle (top-left origin, y-down pixel coords).
fn rect_mvp(w: f32, h: f32, sx: f32, sy: f32, win_w: f32, win_h: f32) -> [[f32; 4]; 4] {
    let (win_w, win_h) = (win_w.max(1.0), win_h.max(1.0));
    let cx = sx + w / 2.0 - win_w / 2.0;
    let cy = win_h / 2.0 - (sy + h / 2.0);
    let proj = Mat4::from_scale(Vec3::new(2.0 / win_w, 2.0 / win_h, 1.0));
    (proj * Mat4::from_translation(Vec3::new(cx, cy, 0.0)) * Mat4::from_scale(Vec3::new(w, h, 1.0)))
        .to_cols_array_2d()
}

// ---------------------------------------------------------------------------
// GPU helpers
// ---------------------------------------------------------------------------

fn build_texture_bind(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    srgb: bool,
    image: &DecodedImage,
) -> wgpu::BindGroup {
    let format = if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let extent = wgpu::Extent3d {
        width: image.width.max(1),
        height: image.height.max(1),
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glanvu texture"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * image.width.max(1)),
            rows_per_image: Some(image.height.max(1)),
        },
        extent,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("texture bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn make_uniform_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glanvu uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_uniform_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buf: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("uniform bind"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buf.as_entire_binding(),
        }],
    })
}

// ---------------------------------------------------------------------------
// Gpu struct
// ---------------------------------------------------------------------------

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    uniform_bind: wgpu::BindGroup,
    texture_bind: wgpu::BindGroup,
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    // Overlay: dark backing box + glyphon text.
    box_uniform_buf: wgpu::Buffer,
    box_uniform_bind: wgpu::BindGroup,
    box_texture_bind: wgpu::BindGroup,
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_buffer: TextBuffer,
    overlay_text: String,
    overlay_line_h: f32,
    // Centered status overlay (slideshow on/off, etc.) — separate buf/text to avoid conflicts.
    status_uniform_buf: wgpu::Buffer,
    status_uniform_bind: wgpu::BindGroup,
    status_text_buf: TextBuffer,
    status_overlay_text: String,
    // Centered help overlay (two-column keyboard cheatsheet, toggled with H).
    // `help_keys_buf` holds the left column (keys + title); `help_desc_buf` the right column
    // (descriptions). Two buffers so the columns align by real font metrics, not by padding spaces.
    help_uniform_buf: wgpu::Buffer,
    help_uniform_bind: wgpu::BindGroup,
    help_keys_buf: TextBuffer,
    help_desc_buf: TextBuffer,
    help_layout_scale: f32,
    help_line_h: f32,
    // Confirmation overlay (D key: set/unset default app). Single-column centered text.
    confirm_uniform_buf: wgpu::Buffer,
    confirm_uniform_bind: wgpu::BindGroup,
    confirm_text_buf: TextBuffer,
    confirm_layout_cache: String,
    confirm_line_h: f32,
    // Grid renderer: pre-allocated pool of per-tile uniform bufs + bind groups.
    tile_bufs: Vec<wgpu::Buffer>,
    tile_binds: Vec<wgpu::BindGroup>,
    // Solid-color textures for grid UI elements.
    cell_bg_bind: wgpu::BindGroup,     // dark cell background
    sel_bind: wgpu::BindGroup,         // selection ring (blue)
    placeholder_bind: wgpu::BindGroup, // thumbnail not yet ready (mid-grey)
    // Date overlay (bottom-right, mirrors the path overlay on the left).
    date_uniform_buf: wgpu::Buffer,
    date_uniform_bind: wgpu::BindGroup,
    date_text_buf: TextBuffer,
    date_overlay_text: String,
    date_overlay_line_h: f32,
    // Watermark logo shown in the empty state (app icon, low opacity, centered).
    watermark_bind: wgpu::BindGroup,
    watermark_uniform_buf: wgpu::Buffer,
    watermark_uniform_bind: wgpu::BindGroup,
    // Version label under the watermark (empty state). Rebuilt only when the DPI scale changes.
    version_text_buf: TextBuffer,
    version_layout_scale: f32,
    // Donate text ("♥ Ko-fi Support · GitHub Sponsors") — reused in empty state, help & about.
    donate_text_buf: TextBuffer,
    donate_layout_scale: f32,
    /// Clickable donate-link zones from the last render (≥0; empty state and an overlay can coexist).
    donate_hits: Vec<DonateHit>,
    // About overlay (A key): centered box with version, license, and donate links.
    about_uniform_buf: wgpu::Buffer,
    about_uniform_bind: wgpu::BindGroup,
    about_text_buf: TextBuffer,
    about_layout_scale: f32,
    about_line_h: f32,
    // GPU-uploaded thumbnails keyed by path.
    thumb_binds: std::collections::HashMap<PathBuf, wgpu::BindGroup>,
    // Explorer panel: per-item text buffers (index 0 = header, 1..n = entries) + solid textures.
    explorer_item_bufs: Vec<TextBuffer>,
    explorer_text_cache: String,
    explorer_layout_scale: f32,
    /// Intrinsic panel width (widest label + padding, in physical px). Depends only on text +
    /// scale, so it is cached in the rebuild block and re-clamped against the live window each frame.
    explorer_intrinsic_w: f32,
    /// Panel width after clamping the intrinsic width to the current window (recomputed per frame).
    explorer_computed_panel_w: f32,
    explorer_bg_bind: wgpu::BindGroup,
    explorer_header_bind: wgpu::BindGroup,
}

impl Gpu {
    async fn new(window: Arc<Window>, image: &DecodedImage) -> Gpu {
        let t0 = Instant::now();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("no suitable GPU adapter");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("glanvu device"),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await
            .expect("request device");

        let size = window.inner_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface not supported by this adapter");
        surface.configure(&device, &config);
        let srgb = config.format.is_srgb();

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glanvu sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniform layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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

        let uniform_buf = make_uniform_buffer(&device);
        let uniform_bind = make_uniform_bind(&device, &uniform_layout, &uniform_buf);
        let texture_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, image);

        let box_uniform_buf = make_uniform_buffer(&device);
        let box_uniform_bind = make_uniform_bind(&device, &uniform_layout, &box_uniform_buf);
        let black = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 170],
        };
        let box_texture_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &black);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glanvu shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glanvu pipeline layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glanvu pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glanvu vertices"),
            contents: bytemuck::cast_slice(&VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glanvu indices"),
            contents: bytemuck::cast_slice(&INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let glyph_cache = GlyphCache::new(&device);
        let viewport = Viewport::new(&device, &glyph_cache);
        let mut atlas = TextAtlas::new(&device, &queue, &glyph_cache, config.format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let text_buffer = TextBuffer::new(&mut font_system, Metrics::new(14.0, 18.0));
        let status_text_buf = TextBuffer::new(&mut font_system, Metrics::new(20.0, 26.0));

        if perf_logging() {
            eprintln!(
                "glanvu: gpu init in {:.1} ms (one-time)",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }

        // Grid: pool of per-tile uniform buffers + bind groups.
        let mut tile_bufs = Vec::with_capacity(TILE_POOL);
        let mut tile_binds = Vec::with_capacity(TILE_POOL);
        let ul = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tile uniform layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        for _ in 0..TILE_POOL {
            let buf = make_uniform_buffer(&device);
            let bind = make_uniform_bind(&device, &ul, &buf);
            tile_bufs.push(buf);
            tile_binds.push(bind);
        }
        // Solid-color textures: dark bg, blue selection, mid-grey placeholder.
        let cell_bg = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![35, 35, 37, 255],
        };
        let cell_bg_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &cell_bg);
        let sel = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![60, 120, 220, 230],
        };
        let sel_bind = build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &sel);
        let ph = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![70, 70, 72, 255],
        };
        let placeholder_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &ph);

        // Status overlay (centered slideshow message) — needs its own uniform buf/bind.
        let status_uniform_buf = make_uniform_buffer(&device);
        let explorer_bg_bind = {
            // Near-opaque dark bg: 248/255 ≈ 97% opacity.
            let bg = DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![18, 20, 26, 248],
            };
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &bg)
        };
        let explorer_header_bind = {
            // Solid dark-blue header bar for the current-directory label.
            let bg = DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![35, 65, 145, 255],
            };
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &bg)
        };
        let uniform_layout_solo =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("solo uniform layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let status_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &status_uniform_buf);
        let help_uniform_buf = make_uniform_buffer(&device);
        let help_uniform_bind = make_uniform_bind(&device, &uniform_layout_solo, &help_uniform_buf);
        let help_keys_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_desc_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let confirm_uniform_buf = make_uniform_buffer(&device);
        let confirm_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &confirm_uniform_buf);
        let confirm_text_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let donate_text_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 17.0));
        let about_uniform_buf = make_uniform_buffer(&device);
        let about_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &about_uniform_buf);
        let about_text_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));

        // Date overlay (bottom-right counterpart to the path overlay).
        let date_uniform_buf = make_uniform_buffer(&device);
        let date_uniform_bind = make_uniform_bind(&device, &uniform_layout_solo, &date_uniform_buf);
        let date_text_buf = TextBuffer::new(&mut font_system, Metrics::new(14.0, 18.0));

        // Watermark: decode app icon, bake ~22% opacity, upload as texture.
        let watermark_bind = {
            let mut img =
                glanvu_core::decode_bytes(WATERMARK_BYTES).unwrap_or_else(|_| DecodedImage {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 0, 0, 0],
                });
            for px in img.rgba.chunks_mut(4) {
                px[3] = (px[3] as f32 * 0.22) as u8;
            }
            build_texture_bind(&device, &queue, &texture_layout, &sampler, false, &img)
        };
        let watermark_uniform_buf = make_uniform_buffer(&device);
        let watermark_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &watermark_uniform_buf);

        // Version label buffer (text + size filled in by `layout_version` on first empty-state draw).
        let version_text_buf = TextBuffer::new(&mut font_system, Metrics::new(14.0, 18.0));

        Gpu {
            surface,
            device,
            queue,
            config,
            pipeline,
            texture_layout,
            sampler,
            uniform_buf,
            uniform_bind,
            texture_bind,
            vertex_buf,
            index_buf,
            box_uniform_buf,
            box_uniform_bind,
            box_texture_bind,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            text_buffer,
            overlay_text: String::new(),
            overlay_line_h: 18.0,
            status_uniform_buf,
            status_uniform_bind,
            status_text_buf,
            status_overlay_text: String::new(),
            help_uniform_buf,
            help_uniform_bind,
            help_keys_buf,
            help_desc_buf,
            help_layout_scale: -1.0,
            help_line_h: 22.0,
            confirm_uniform_buf,
            confirm_uniform_bind,
            confirm_text_buf,
            confirm_layout_cache: String::new(),
            confirm_line_h: 22.0,
            tile_bufs,
            tile_binds,
            cell_bg_bind,
            sel_bind,
            placeholder_bind,
            date_uniform_buf,
            date_uniform_bind,
            date_text_buf,
            date_overlay_text: String::new(),
            date_overlay_line_h: 18.0,
            watermark_bind,
            watermark_uniform_buf,
            watermark_uniform_bind,
            version_text_buf,
            version_layout_scale: -1.0,
            donate_text_buf,
            donate_layout_scale: -1.0,
            donate_hits: Vec::new(),
            about_uniform_buf,
            about_uniform_bind,
            about_text_buf,
            about_layout_scale: -1.0,
            about_line_h: 22.0,
            thumb_binds: std::collections::HashMap::new(),
            explorer_item_bufs: Vec::new(),
            explorer_text_cache: String::new(),
            explorer_layout_scale: -1.0,
            explorer_intrinsic_w: PANEL_W,
            explorer_computed_panel_w: PANEL_W,
            explorer_bg_bind,
            explorer_header_bind,
        }
    }

    fn set_image(&mut self, image: &DecodedImage) {
        self.texture_bind = build_texture_bind(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            self.config.format.is_srgb(),
            image,
        );
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width > 0 && size.height > 0 {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn layout_overlay(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.overlay_text != text {
            self.overlay_text = text.to_string();
            let font = (13.0 * scale).clamp(12.0, 30.0);
            self.overlay_line_h = font * 1.35;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.overlay_line_h),
            );
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.text_buffer = buf;
        }
        let pad = 8.0 * scale;
        let margin = 12.0 * scale;
        let text_w = self
            .text_buffer
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let bw = (text_w + 2.0 * pad)
            .min(win_w - 2.0 * margin)
            .max(2.0 * pad);
        let bh = self.overlay_line_h + 2.0 * pad;
        let bx = margin;
        let by = (win_h - margin - bh).max(0.0);
        (bx, by, bw, bh, bx + pad, by + pad)
    }

    /// Lay out the date overlay (bottom-right, mirrors the path overlay).
    fn layout_date_overlay(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.date_overlay_text != text {
            self.date_overlay_text = text.to_string();
            let font = (13.0 * scale).clamp(12.0, 30.0);
            self.date_overlay_line_h = font * 1.35;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.date_overlay_line_h),
            );
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.date_text_buf = buf;
        }
        let pad = 8.0 * scale;
        let margin = 12.0 * scale;
        let text_w = self
            .date_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let bw = (text_w + 2.0 * pad)
            .min(win_w - 2.0 * margin)
            .max(2.0 * pad);
        let bh = self.date_overlay_line_h + 2.0 * pad;
        let bx = (win_w - margin - bw).max(0.0); // right-aligned
        let by = (win_h - margin - bh).max(0.0);
        (bx, by, bw, bh, bx + pad, by + pad)
    }

    /// Lay out the version label centered under the watermark (empty state). No background box.
    /// Returns `(text_x, text_y)` in physical pixels. The buffer is rebuilt only when `scale`
    /// changes; the label text itself is static.
    fn layout_version(&mut self, scale: f32, win_w: f32, win_h: f32) -> (f32, f32) {
        if (self.version_layout_scale - scale).abs() > f32::EPSILON {
            self.version_layout_scale = scale;
            let font = (14.0 * scale).clamp(12.0, 28.0);
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, font * 1.3));
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                VERSION_LABEL,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.version_text_buf = buf;
        }
        let text_w = self
            .version_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        // Watermark is a centered square sized to 40% of the shorter window dimension.
        let logo_size = (win_w.min(win_h) * 0.40).max(64.0);
        let logo_bottom = ((win_h - logo_size) / 2.0).round() + logo_size;
        let left = ((win_w - text_w) / 2.0).max(0.0);
        let top = logo_bottom + 16.0 * scale;
        (left, top)
    }

    /// Lay out the donate line ("♥ Ko-fi · GitHub Sponsors") centered below the version label.
    /// Rebuilds the buffer only when `scale` changes. Returns `(text_x, text_y)`.
    fn layout_donate(&mut self, scale: f32, win_w: f32, win_h: f32) -> (f32, f32) {
        if (self.donate_layout_scale - scale).abs() > f32::EPSILON {
            self.donate_layout_scale = scale;
            let font = (13.0 * scale).clamp(11.0, 26.0);
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, font * 1.3));
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                DONATE_LINE,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.donate_text_buf = buf;
        }
        let text_w = self
            .donate_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let version_font = (14.0 * scale).clamp(12.0, 28.0);
        let version_lh = version_font * 1.3;
        let logo_size = (win_w.min(win_h) * 0.40).max(64.0);
        let logo_bottom = ((win_h - logo_size) / 2.0).round() + logo_size;
        let version_y = logo_bottom + 16.0 * scale;
        let top = version_y + version_lh + 6.0 * scale;
        let left = ((win_w - text_w) / 2.0).max(0.0);
        (left, top)
    }

    /// Number of text lines in the About body (head), used for box height and donate placement.
    const ABOUT_HEAD_LINES: usize = {
        // Count '\n' + 1. `const`-friendly manual count.
        let bytes = ABOUT_HEAD.as_bytes();
        let mut n = 1usize;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                n += 1;
            }
            i += 1;
        }
        n
    };

    /// Lay out the About overlay (A key). The body (head) is white; the donate line is rendered
    /// separately (blue, centered) below it. Returns `(box_x, box_y, box_w, box_h, text_x, text_y)`.
    fn layout_about(&mut self, scale: f32, win_w: f32, win_h: f32) -> (f32, f32, f32, f32, f32, f32) {
        if (self.about_layout_scale - scale).abs() > f32::EPSILON {
            self.about_layout_scale = scale;
            let font = (15.0 * scale).clamp(13.0, 30.0);
            self.about_line_h = font * 1.5;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.about_line_h),
            );
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                ABOUT_HEAD,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.about_text_buf = buf;
        }
        let pad_h = 40.0 * scale;
        let pad_v = 28.0 * scale;
        let head_w = self
            .about_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let donate_w = self
            .donate_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let text_w = head_w.max(donate_w);
        // Rows: head lines + one blank gap + one donate row.
        let rows = (Self::ABOUT_HEAD_LINES + 2) as f32;
        let bw = (text_w + 2.0 * pad_h).min(win_w - 40.0);
        let bh = (rows * self.about_line_h + 2.0 * pad_v).min(win_h - 40.0);
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = ((win_h - bh) / 2.0).max(0.0);
        (bx, by, bw, bh, bx + pad_h, by + pad_v)
    }

    /// Lay out the centered status overlay (slideshow message, etc.).
    /// Returns `(box_x, box_y, box_w, box_h, text_x, text_y)` in physical pixels.
    fn layout_status(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.status_overlay_text != text {
            self.status_overlay_text = text.to_string();
            let font = (20.0 * scale).clamp(16.0, 40.0);
            let lh = font * 1.35;
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, lh));
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.status_text_buf = buf;
        }
        let pad_h = 12.0 * scale;
        let pad_v = 10.0 * scale;
        let text_w = self
            .status_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let lh = self.status_text_buf.metrics().line_height;
        let bw = (text_w + 2.0 * pad_h).max(2.0 * pad_h);
        let bh = lh + 2.0 * pad_v;
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = win_h * 0.38;
        (bx, by, bw, bh, bx + pad_h, by + pad_v)
    }

    /// Lay out the centered multi-line help overlay. Returns box + text rect (physical px).
    /// Lay out the two-column help overlay. Returns
    /// `(box_x, box_y, box_w, box_h, keys_x, text_y, desc_x)`.
    ///
    /// The left column is keys + title, the right column is descriptions. Both are aligned by real
    /// font metrics: `desc_x` is `keys_x` plus the measured width of the widest key plus a gap.
    fn layout_help(
        &mut self,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32, f32) {
        // Title occupies line 0, blank line 1, then HELP_ROWS from line 2. Blank (key,desc) pairs
        // become blank lines in both columns so the two buffers stay vertically aligned.
        if (self.help_layout_scale - scale).abs() > f32::EPSILON {
            self.help_layout_scale = scale;
            let font = (15.0 * scale).clamp(13.0, 30.0);
            self.help_line_h = font * 1.5;

            let mut keys = String::from(HELP_TITLE);
            keys.push_str("\n\n");
            let mut desc = String::from("\n\n");
            for (i, (k, d)) in HELP_ROWS.iter().enumerate() {
                if i > 0 {
                    keys.push('\n');
                    desc.push('\n');
                }
                keys.push_str(k);
                desc.push_str(d);
            }

            let mut make = |s: &str| {
                let mut buf =
                    TextBuffer::new(&mut self.font_system, Metrics::new(font, self.help_line_h));
                buf.set_wrap(&mut self.font_system, Wrap::None);
                buf.set_size(&mut self.font_system, None, None);
                buf.set_text(
                    &mut self.font_system,
                    s,
                    &Attrs::new(),
                    Shaping::Basic,
                    None,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                buf
            };
            self.help_keys_buf = make(&keys);
            self.help_desc_buf = make(&desc);
        }

        let pad_h = 28.0 * scale;
        let pad_v = 22.0 * scale;
        let gap = 36.0 * scale;
        let keys_w = self
            .help_keys_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let desc_w = self
            .help_desc_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        // Rows = title + blank + HELP_ROWS, plus one extra row at the bottom for the donate footer
        // (kept INSIDE the dark box).
        let lines = (HELP_ROWS.len() + 2) as f32;
        let bw = (keys_w + gap + desc_w + 2.0 * pad_h).min(win_w - 40.0);
        let bh = ((lines + 1.0) * self.help_line_h + 2.0 * pad_v).min(win_h - 40.0);
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = ((win_h - bh) / 2.0).max(0.0);
        let keys_x = bx + pad_h;
        (bx, by, bw, bh, keys_x, by + pad_v, keys_x + keys_w + gap)
    }

    /// Lay out the confirmation overlay (D key). Single-column centered text.
    /// Returns `(box_x, box_y, box_w, box_h, text_x, text_y)`.
    fn layout_confirm(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.confirm_layout_cache != text {
            self.confirm_layout_cache = text.to_string();
            let font = (15.0 * scale).clamp(13.0, 30.0);
            self.confirm_line_h = font * 1.5;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.confirm_line_h),
            );
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.confirm_text_buf = buf;
        }
        let pad_h = 36.0 * scale;
        let pad_v = 24.0 * scale;
        let text_w = self
            .confirm_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let lines = text.lines().count().max(1) as f32;
        let bw = (text_w + 2.0 * pad_h).min(win_w - 40.0);
        let bh = (lines * self.confirm_line_h + 2.0 * pad_v).min(win_h - 40.0);
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = ((win_h - bh) / 2.0).max(0.0);
        (bx, by, bw, bh, bx + pad_h, by + pad_v)
    }

    /// Render one frame in single-image mode.
    ///
    /// All text overlays (path overlay, status message, explorer panel) are prepared in ONE
    /// `text_renderer.prepare()` call so no overlay replaces another. All box draws happen before
    /// the single `text_renderer.render()` call at the end of the pass, so the pipeline is only
    /// switched once from ours to glyphon's.
    ///
    /// The raw-pointer trick in the prepare section borrows `text_buffer`, `status_text_buf`, and
    /// `explorer_text_buf` (read-only) while `font_system`, `atlas`, and `swash_cache` are borrowed
    /// mutably — all distinct fields, so it is sound. The `unsafe_code = deny` workspace lint is
    /// selectively lifted here because this is the canonical solution to the Rust "can't borrow
    /// disjoint fields via &mut self" limitation in this specific GPU context.
    #[allow(unsafe_code)]
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        uniforms: Uniforms,
        overlay: Option<&str>,
        date_overlay: Option<&str>,
        status: Option<&str>,
        explorer: Option<&ExplorerState>,
        help: bool,
        confirm: Option<&str>,
        about: bool,
        scale: f32,
        logo: bool,
    ) -> bool {
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        let (win_w, win_h) = (self.config.width as f32, self.config.height as f32);

        // Donate-link hit zones are recomputed from scratch each frame.
        self.donate_hits.clear();

        // ---- Collect positions for box draws and text areas ----

        // Path overlay (bottom-left).
        let mut show_overlay_box = false;
        let mut overlay_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = overlay {
            overlay_coords = self.layout_overlay(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = overlay_coords;
            self.queue.write_buffer(
                &self.box_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_overlay_box = true;
        }

        // Date overlay (bottom-right).
        let mut show_date_box = false;
        let mut date_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = date_overlay {
            date_coords = self.layout_date_overlay(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = date_coords;
            self.queue.write_buffer(
                &self.date_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_date_box = true;
        }

        // Status message (centered).
        let mut show_status_box = false;
        let mut status_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = status {
            status_coords = self.layout_status(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = status_coords;
            self.queue.write_buffer(
                &self.status_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_status_box = true;
        }

        // Donate line buffer (blue, centered) — shared by the empty state, help footer, and About.
        let donate_pos = self.layout_donate(scale, win_w, win_h);
        let donate_text_w = self
            .donate_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let don_lh = (13.0 * scale).clamp(11.0, 26.0) * 1.3;
        // Centered horizontal origin (window center == any centered box center).
        let donate_left = ((win_w - donate_text_w) / 2.0).max(0.0);
        // Push a clickable hit zone (left half → Ko-fi, right half → Sponsors) for a donate line at `top`.
        macro_rules! push_donate_hit {
            ($top:expr) => {{
                let top = $top;
                self.donate_hits.push(DonateHit {
                    x0: donate_left,
                    y0: top,
                    x1: donate_left + donate_text_w,
                    y1: top + don_lh,
                    split_x: donate_left + donate_text_w / 2.0,
                });
            }};
        }

        // Donate footer for help overlay — declared here, positioned inside the help block below.
        let mut show_help_donate = false;
        let mut help_donate_coords = (0.0_f32, 0.0_f32);

        // Help overlay (centered, two columns).
        let mut show_help_box = false;
        let mut help_coords = (
            0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32,
        );
        if help {
            help_coords = self.layout_help(scale, win_w, win_h);
            let (bx, by, bw, bh, _, ty, _) = help_coords;
            self.queue.write_buffer(
                &self.help_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_help_box = true;
            // Donate footer: centered, in the extra row reserved INSIDE the box (one help_line_h
            // below the last help row). `ty` is the text top (by + pad_v); the help text occupies
            // `lines` rows, so the donate row starts at ty + lines * help_line_h.
            let lines = (HELP_ROWS.len() + 2) as f32;
            let donate_top = ty + lines * self.help_line_h + (self.help_line_h - don_lh) / 2.0;
            help_donate_coords = (donate_left, donate_top);
            // Only show help donate if not obscured by the about overlay.
            show_help_donate = !about;
            if show_help_donate {
                push_donate_hit!(donate_top);
            }
        }

        // Confirmation overlay (D key: set/unset default app).
        let mut show_confirm_box = false;
        let mut confirm_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = confirm {
            confirm_coords = self.layout_confirm(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = confirm_coords;
            self.queue.write_buffer(
                &self.confirm_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_confirm_box = true;
        }

        // Watermark logo (empty state only): centered square, 40% of the shorter dimension.
        let mut show_version = false;
        let mut version_coords = (0.0_f32, 0.0_f32);
        if logo {
            let size = (win_w.min(win_h) * 0.40).max(64.0);
            let sx = ((win_w - size) / 2.0).round();
            let sy = ((win_h - size) / 2.0).round();
            self.queue.write_buffer(
                &self.watermark_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(size, size, sx, sy, win_w, win_h),
                }),
            );
            version_coords = self.layout_version(scale, win_w, win_h);
            show_version = true;
        }

        // Donate text (empty state, below version label).
        let show_donate = logo;
        let donate_coords = donate_pos; // position computed above via layout_donate
        if show_donate {
            push_donate_hit!(donate_coords.1);
        }

        // About overlay box (A key). The donate line is rendered separately (blue, centered) in the
        // row below the head + one blank gap row.
        let mut show_about_box = false;
        let mut about_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        let mut about_donate_coords = (0.0_f32, 0.0_f32);
        if about {
            about_coords = self.layout_about(scale, win_w, win_h);
            let (bx, by, bw, bh, _, ty) = about_coords;
            self.queue.write_buffer(
                &self.about_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_about_box = true;
            let about_donate_top = ty
                + (Self::ABOUT_HEAD_LINES as f32 + 1.0) * self.about_line_h
                + (self.about_line_h - don_lh) / 2.0;
            about_donate_coords = (donate_left, about_donate_top);
            push_donate_hit!(about_donate_top);
        }

        // Explorer panel (left side).
        let mut show_explorer = false;
        let explorer_sel_y;
        let mut explorer_sel_visible = false;
        let mut explorer_panel_w = self.explorer_computed_panel_w; // current best estimate
        if let Some(exp) = explorer {
            // 1. Rebuild TextBuffers first (Wrap::None → layout_runs gives actual line widths).
            let text = exp.panel_text();
            if self.explorer_text_cache != text
                || (self.explorer_layout_scale - scale).abs() > f32::EPSILON
            {
                self.explorer_text_cache = text;
                self.explorer_layout_scale = scale;
                self.explorer_item_bufs.clear();
                let mut make_buf = |label: &str| {
                    let mut buf = TextBuffer::new(
                        &mut self.font_system,
                        Metrics::new(EXPLORER_FONT * scale, EXPLORER_LINE_H * scale),
                    );
                    buf.set_wrap(&mut self.font_system, Wrap::None);
                    buf.set_size(&mut self.font_system, None, None);
                    buf.set_text(
                        &mut self.font_system,
                        label,
                        &Attrs::new(),
                        Shaping::Basic,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    buf
                };
                // Index 0: header.
                let hbuf = make_buf(&format!("{}/", exp.dir_label()));
                self.explorer_item_bufs.push(hbuf);
                // Index 1..n: entries.
                for entry in exp.entries() {
                    let buf = make_buf(&entry.label);
                    self.explorer_item_bufs.push(buf);
                }
                // Intrinsic width from real font metrics (no guesswork). max_lw is already in
                // physical pixels (Metrics are scaled), so the padding must be scaled too.
                let max_lw = self
                    .explorer_item_bufs
                    .iter()
                    .flat_map(|b| b.layout_runs())
                    .map(|r| r.line_w)
                    .fold(0.0_f32, f32::max);
                self.explorer_intrinsic_w = max_lw + 20.0 * scale;
            }
            // Clamp the cached intrinsic width to the live window EVERY frame: at least 180 logical
            // px, at most 70% of the window. Done outside the rebuild block so a stale window size
            // captured during a monitor switch (ScaleFactorChanged fires before Resized) self-
            // corrects on the next frame instead of freezing a wrong width.
            self.explorer_computed_panel_w =
                self.explorer_intrinsic_w.clamp(180.0 * scale, win_w * 0.70);
            explorer_panel_w = self.explorer_computed_panel_w;

            // 2. Write geometry uniforms with the actual panel width.
            // Panel background (tile_bufs[0]).
            self.queue.write_buffer(
                &self.tile_bufs[0],
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(explorer_panel_w, win_h, 0.0, 0.0, win_w, win_h),
                }),
            );
            // Header blue bar (tile_bufs[2]).
            self.queue.write_buffer(
                &self.tile_bufs[2],
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(
                        explorer_panel_w,
                        (EXPLORER_LINE_H + 8.0) * scale,
                        0.0,
                        0.0,
                        win_w,
                        win_h,
                    ),
                }),
            );
            // Selection highlight (tile_bufs[1]).
            explorer_sel_y = exp.entry_y(exp.sel) * scale;
            explorer_sel_visible = explorer_sel_y + EXPLORER_LINE_H * scale > 0.0
                && explorer_sel_y < win_h
                && !exp.entries().is_empty();
            if explorer_sel_visible {
                self.queue.write_buffer(
                    &self.tile_bufs[1],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(
                            explorer_panel_w - 4.0 * scale,
                            EXPLORER_LINE_H * scale,
                            2.0 * scale,
                            explorer_sel_y,
                            win_w,
                            win_h,
                        ),
                    }),
                );
            }
            show_explorer = true;
        }

        // ---- ONE combined text prepare() call ----
        let need_text = show_overlay_box
            || show_date_box
            || show_status_box
            || show_explorer
            || show_help_box
            || show_confirm_box
            || show_version
            || show_donate
            || show_help_donate
            || show_about_box;
        let text_ok = if need_text {
            self.viewport.update(
                &self.queue,
                Resolution {
                    width: self.config.width,
                    height: self.config.height,
                },
            );
            // Build TextArea list using field-level borrows to satisfy the borrow checker.
            let (o_left, o_top, o_bx, o_by, o_bw, o_bh);
            let (s_left, s_top, s_bx, s_by, s_bw, s_bh);
            if show_overlay_box {
                let (bx, by, bw, bh, tx, ty) = overlay_coords;
                (o_left, o_top, o_bx, o_by, o_bw, o_bh) = (tx, ty, bx, by, bw, bh);
            } else {
                (o_left, o_top, o_bx, o_by, o_bw, o_bh) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            }
            if show_status_box {
                let (bx, by, bw, bh, tx, ty) = status_coords;
                (s_left, s_top, s_bx, s_by, s_bw, s_bh) = (tx, ty, bx, by, bw, bh);
            } else {
                (s_left, s_top, s_bx, s_by, s_bw, s_bh) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            }
            // Borrow text buffers via raw pointers before the mutable borrows below.
            // SAFETY: each pointer targets a distinct field from font_system/atlas/swash_cache.
            let p_buf = &self.text_buffer as *const TextBuffer;
            let d_buf = &self.date_text_buf as *const TextBuffer;
            let s_buf = &self.status_text_buf as *const TextBuffer;
            let hk_buf = &self.help_keys_buf as *const TextBuffer;
            let hd_buf = &self.help_desc_buf as *const TextBuffer;
            let c_buf = &self.confirm_text_buf as *const TextBuffer;
            let v_buf = &self.version_text_buf as *const TextBuffer;
            let donate_ptr = &self.donate_text_buf as *const TextBuffer;
            let about_ptr = &self.about_text_buf as *const TextBuffer;
            // explorer_item_bufs is a Vec; get raw pointers to each element.
            let e_ptrs: Vec<*const TextBuffer> = self
                .explorer_item_bufs
                .iter()
                .map(|b| b as *const _)
                .collect();
            let mut areas: Vec<TextArea> = Vec::with_capacity(
                show_overlay_box as usize + show_status_box as usize + e_ptrs.len(),
            );
            if show_overlay_box {
                areas.push(TextArea {
                    buffer: unsafe { &*p_buf },
                    left: o_left,
                    top: o_top,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: o_bx as i32,
                        top: o_by as i32,
                        right: (o_bx + o_bw) as i32,
                        bottom: (o_by + o_bh) as i32,
                    },
                    default_color: Color::rgb(240, 240, 240),
                    custom_glyphs: &[],
                });
            }
            if show_status_box {
                areas.push(TextArea {
                    buffer: unsafe { &*s_buf },
                    left: s_left,
                    top: s_top,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: s_bx as i32,
                        top: s_by as i32,
                        right: (s_bx + s_bw) as i32,
                        bottom: (s_by + s_bh) as i32,
                    },
                    default_color: Color::rgb(255, 255, 255),
                    custom_glyphs: &[],
                });
            }
            if show_date_box {
                let (bx, by, bw, bh, tx, ty) = date_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*d_buf },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: bx as i32,
                        top: by as i32,
                        right: (bx + bw) as i32,
                        bottom: (by + bh) as i32,
                    },
                    default_color: Color::rgb(200, 200, 200),
                    custom_glyphs: &[],
                });
            }
            if show_help_box {
                let (bx, by, bw, bh, keys_x, ty, desc_x) = help_coords;
                let bounds = TextBounds {
                    left: bx as i32,
                    top: by as i32,
                    right: (bx + bw) as i32,
                    bottom: (by + bh) as i32,
                };
                // Left column: keys + title (brighter). Right column: descriptions (dimmer).
                areas.push(TextArea {
                    buffer: unsafe { &*hk_buf },
                    left: keys_x,
                    top: ty,
                    scale: 1.0,
                    bounds,
                    default_color: Color::rgb(245, 245, 250),
                    custom_glyphs: &[],
                });
                areas.push(TextArea {
                    buffer: unsafe { &*hd_buf },
                    left: desc_x,
                    top: ty,
                    scale: 1.0,
                    bounds,
                    default_color: Color::rgb(180, 185, 195),
                    custom_glyphs: &[],
                });
            }
            if show_confirm_box {
                let (bx, by, bw, bh, tx, ty) = confirm_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*c_buf },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: bx as i32,
                        top: by as i32,
                        right: (bx + bw) as i32,
                        bottom: (by + bh) as i32,
                    },
                    default_color: Color::rgb(240, 240, 245),
                    custom_glyphs: &[],
                });
            }
            if show_version {
                let (tx, ty) = version_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*v_buf },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(130, 130, 140),
                    custom_glyphs: &[],
                });
            }
            if show_explorer {
                // Index 0 = header (white), 1..n = entries with per-kind colors.
                // Colors: parent/dir = soft blue, current image = amber, other images = light grey.
                let panel_right = explorer_panel_w as i32;
                let explorer = explorer.unwrap(); // show_explorer is true, so explorer is Some
                for (idx, ptr) in e_ptrs.iter().enumerate() {
                    let color = if idx == 0 {
                        // Header
                        Color::rgb(255, 255, 255)
                    } else {
                        let entry = &explorer.entries()[idx - 1];
                        match entry.kind {
                            glanvu_viewer_core::explorer::EntryKind::Parent
                            | glanvu_viewer_core::explorer::EntryKind::Dir => {
                                Color::rgb(100, 165, 255)
                            }
                            glanvu_viewer_core::explorer::EntryKind::Image => {
                                if entry.label.starts_with("> ") {
                                    Color::rgb(255, 220, 80) // current image: amber
                                } else {
                                    Color::rgb(200, 200, 205) // other images: light grey
                                }
                            }
                        }
                    };
                    // y position: 0 = header at top, 1..n = entries below EXPLORER_LINE_H+8.
                    // All coordinates are in physical pixels (Metrics are scaled).
                    let top = if idx == 0 {
                        8.0 * scale // header padding within the header bar
                    } else {
                        explorer.entry_y(idx - 1) * scale
                    };
                    if idx > 0 && (top + EXPLORER_LINE_H * scale < 0.0 || top > win_h) {
                        continue; // not visible
                    }
                    areas.push(TextArea {
                        buffer: unsafe { &**ptr },
                        left: 8.0 * scale,
                        top,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: 0,
                            top: if idx == 0 {
                                0
                            } else {
                                ((EXPLORER_LINE_H + 8.0) * scale) as i32
                            },
                            right: panel_right,
                            bottom: win_h as i32,
                        },
                        default_color: color,
                        custom_glyphs: &[],
                    });
                }
            }
            if show_donate {
                let (tx, ty) = donate_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*donate_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(130, 190, 255),
                    custom_glyphs: &[],
                });
            }
            if show_help_donate {
                let (tx, ty) = help_donate_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*donate_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(160, 205, 255),
                    custom_glyphs: &[],
                });
            }
            if show_about_box {
                let (_, _, _, _, tx, ty) = about_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*about_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(240, 240, 245),
                    custom_glyphs: &[],
                });
                // Donate line: blue, centered, in the row below the About body.
                let (dtx, dty) = about_donate_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*donate_ptr },
                    left: dtx,
                    top: dty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(130, 190, 255),
                    custom_glyphs: &[],
                });
            }
            self.text_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    &mut self.font_system,
                    &mut self.atlas,
                    &self.viewport,
                    areas,
                    &mut self.swash_cache,
                )
                .is_ok()
        } else {
            false
        };

        // ---- Acquire surface and record draw commands ----
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glanvu encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glanvu pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.07,
                            g: 0.07,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
            pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint16);

            // 1. Image.
            pass.set_bind_group(0, &self.uniform_bind, &[]);
            pass.set_bind_group(1, &self.texture_bind, &[]);
            pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);

            // 1b. Watermark logo (empty state only).
            if logo {
                pass.set_bind_group(0, &self.watermark_uniform_bind, &[]);
                pass.set_bind_group(1, &self.watermark_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 2. Explorer panel bg + header bar + selection (drawn on top of image, under text).
            if show_explorer {
                // Panel background.
                pass.set_bind_group(0, &self.tile_binds[0], &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                // Header blue bar.
                pass.set_bind_group(0, &self.tile_binds[2], &[]);
                pass.set_bind_group(1, &self.explorer_header_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                // Selection highlight.
                if explorer_sel_visible {
                    pass.set_bind_group(0, &self.tile_binds[1], &[]);
                    pass.set_bind_group(1, &self.sel_bind, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }
            }

            // 3. Path overlay box (bottom-left) + date overlay box (bottom-right).
            if show_overlay_box {
                pass.set_bind_group(0, &self.box_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
            if show_date_box {
                pass.set_bind_group(0, &self.date_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 4. Status box.
            if show_status_box {
                pass.set_bind_group(0, &self.status_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 5. Help box.
            if show_help_box {
                pass.set_bind_group(0, &self.help_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // near-opaque dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 6. Confirmation overlay box (D key).
            if show_confirm_box {
                pass.set_bind_group(0, &self.confirm_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // same dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 6b. About overlay box (A key).
            if show_about_box {
                pass.set_bind_group(0, &self.about_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // same dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 7. ONE glyphon render call for ALL text (explorer + overlay + status + help + confirm).
            if text_ok {
                let _ = self
                    .text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        if text_ok {
            self.atlas.trim();
        }
        true
    }

    // --- Grid renderer ------------------------------------------------------

    /// Upload a thumbnail to the GPU and store the bind group keyed by path.
    pub fn upload_thumb(&mut self, path: PathBuf, img: &DecodedImage) {
        let bind = build_texture_bind(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            self.config.format.is_srgb(),
            img,
        );
        self.thumb_binds.insert(path, bind);
    }

    /// Render the thumbnail grid. Returns whether a frame was presented.
    pub fn render_grid(&mut self, paths: &[PathBuf], grid: &GridState) -> bool {
        let (win_w, win_h) = (self.config.width as f32, self.config.height as f32);
        let n = paths.len();

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Collect visible tiles and write all transforms before the render pass.
        let visible: Vec<usize> = (0..n)
            .filter(|&i| grid.is_visible(i, win_w, win_h))
            .collect();

        let mut slot = 0usize;
        // For each visible tile we write up to 3 transforms (bg, sel ring, thumb/placeholder).
        // Slot assignment per tile: bg=slot, sel=slot+1 (only if selected), thumb=slot+2.
        struct TileSlots {
            bg: usize,
            sel: Option<usize>,
            content: usize,
        }
        let mut tile_slots: Vec<TileSlots> = Vec::with_capacity(visible.len());

        for &i in &visible {
            let (cx, cy) = grid.cell_origin(i, win_w);
            // bg quad
            if slot < TILE_POOL {
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(CELL_W, CELL_H, cx, cy, win_w, win_h),
                    }),
                );
            }
            let bg_slot = slot;
            slot += 1;
            // selection ring (slightly larger quad, only for selected tile)
            let sel_slot = if i == grid.sel && slot < TILE_POOL {
                let out = SEL_OUTSET;
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(
                            CELL_W + out * 2.0,
                            CELL_H + out * 2.0,
                            cx - out,
                            cy - out,
                            win_w,
                            win_h,
                        ),
                    }),
                );
                let s = slot;
                slot += 1;
                Some(s)
            } else {
                None
            };
            // thumbnail or placeholder quad (centered within cell)
            let content_slot = slot;
            if slot < TILE_POOL {
                let path = &paths[i];
                let (tw, th) = self
                    .thumb_binds
                    .get(path)
                    .map(|_| (THUMB_W as f32, THUMB_H as f32))
                    .unwrap_or((CELL_W, CELL_H));
                let tx = cx + (CELL_W - tw) / 2.0;
                let ty = cy + (CELL_H - th) / 2.0;
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(tw, th, tx, ty, win_w, win_h),
                    }),
                );
                slot += 1;
            }
            tile_slots.push(TileSlots {
                bg: bg_slot,
                sel: sel_slot,
                content: content_slot,
            });
        }

        // Single render pass: clear + draw all tiles.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glanvu grid encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glanvu grid pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.10,
                            g: 0.10,
                            b: 0.11,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
            pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint16);

            for (idx, &i) in visible.iter().enumerate() {
                let ts = &tile_slots[idx];
                if ts.bg >= TILE_POOL {
                    continue;
                }

                // 1. Cell background.
                pass.set_bind_group(0, &self.tile_binds[ts.bg], &[]);
                pass.set_bind_group(1, &self.cell_bg_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);

                // 2. Selection ring (selected tile only).
                if let Some(ss) = ts.sel {
                    pass.set_bind_group(0, &self.tile_binds[ss], &[]);
                    pass.set_bind_group(1, &self.sel_bind, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }

                // 3. Thumbnail or placeholder.
                if ts.content < TILE_POOL {
                    let path = &paths[i];
                    let tex = self.thumb_binds.get(path).unwrap_or(&self.placeholder_bind);
                    pass.set_bind_group(0, &self.tile_binds[ts.content], &[]);
                    pass.set_bind_group(1, tex, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        true
    }
}

// ---------------------------------------------------------------------------
// App (winit event handler)
// ---------------------------------------------------------------------------

/// Viewer display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Waiting for the user to drop or pick a file (launched without arguments).
    Empty,
    /// Single-image view (Phase 1 default).
    Single,
    /// Thumbnail grid (Phase 2).
    Grid,
}

struct App {
    start: Instant,
    nav: FolderNav,
    img_size: (u32, u32),
    state: ViewState,
    mode: ViewMode,
    grid: GridState,
    thumbs: ThumbnailCache,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    first_frame: bool,
    cursor: PhysicalPosition<f64>,
    dragging: bool,
    overlay_until: Option<Instant>,
    /// When `Some(t)`, the slideshow advances at time `t`.
    slideshow_next: Option<Instant>,
    slideshow_interval: Duration,
    /// Short centered status message (e.g. "Slideshow on") and when to hide it.
    status_text: String,
    status_until: Option<Instant>,
    /// Directory explorer panel; `Some` when open.
    explorer: Option<ExplorerState>,
    /// Whether the centered keyboard-help overlay is shown.
    help_visible: bool,
    /// Whether the About overlay is shown (A key).
    about_visible: bool,
    /// Pending default-app confirmation. `Some(true)` = set; `Some(false)` = unset.
    confirm_assoc: Option<bool>,
    /// Last grid click: (timestamp, cell_index) for double-click detection.
    last_grid_click: Option<(Instant, usize)>,
    /// Current sort order.
    sort_mode: glanvu_viewer_core::nav::SortMode,
    /// Cached mtime string for the date overlay (updated when the path overlay fires).
    date_text: String,
}

/// Static keyboard cheatsheet shown by the help overlay (H).
/// Text for the D-key confirmation overlay. `set` = true → set as default; false → unset.
fn confirm_overlay_text(set: bool) -> &'static str {
    if set {
        "Set Glanvu as default?\n\njpg  jpeg  png  gif\nbmp  tif   tiff webp\n\nEnter = confirm   Esc = cancel"
    } else {
        "Restore previous defaults?\n\nGlanvu won't open images\nby default anymore.\n\nEnter = confirm   Esc = cancel"
    }
}

const HELP_TITLE: &str = "Glanvu — keyboard shortcuts";

/// Two-column cheatsheet rows: `(keys, description)`. An empty pair renders as a blank
/// separator line in both columns.
const HELP_ROWS: &[(&str, &str)] = &[
    ("Arrows", "previous · next image"),
    ("Home / End", "first · last image"),
    ("Tab / G", "thumbnail grid"),
    ("Enter", "directory explorer"),
    ("S", "slideshow"),
    ("", ""),
    ("+ / − / wheel", "zoom in · out"),
    ("drag", "pan"),
    ("0", "fit to window"),
    ("1", "actual size (1:1)"),
    ("R", "rotate 90°"),
    ("Space / F / F11", "fullscreen"),
    ("", ""),
    ("C", "copy image to clipboard"),
    ("Shift+C", "copy file path to clipboard"),
    ("O", "toggle sort order (name / date)"),
    ("", ""),
    ("D", "set Glanvu as default app"),
    ("U", "restore previous defaults"),
    ("A", "about Glanvu"),
    ("H / ?", "show / hide this help"),
    ("Esc / Q", "close · quit"),
];

fn mtime_string(path: &std::path::PathBuf) -> Option<String> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let dt = chrono::DateTime::<chrono::Local>::from(mtime);
    Some(dt.format("%Y-%m-%d  %H:%M").to_string())
}

impl App {
    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn flash_overlay(&mut self) {
        self.overlay_until = Some(Instant::now() + OVERLAY_DURATION);
        self.date_text = self
            .nav
            .current_path()
            .and_then(mtime_string)
            .unwrap_or_default();
    }

    fn toggle_slideshow(&mut self) {
        if self.slideshow_next.is_some() {
            self.slideshow_next = None;
            self.show_status("Slideshow stop");
        } else {
            self.slideshow_next = Some(Instant::now() + self.slideshow_interval);
            self.show_status("Slideshow start");
        }
    }

    fn show_status(&mut self, text: &str) {
        self.status_text = text.to_string();
        self.status_until = Some(Instant::now() + Duration::from_secs(1));
    }

    fn copy_image_to_clipboard(&mut self) {
        let Some(img) = self.nav.current_image() else {
            return;
        };
        let data = arboard::ImageData {
            width: img.width as usize,
            height: img.height as usize,
            bytes: std::borrow::Cow::Borrowed(&img.rgba),
        };
        match Clipboard::new().and_then(|mut cb| cb.set_image(data)) {
            Ok(()) => self.show_status("Image copied"),
            Err(_) => self.show_status("Copy failed"),
        }
        self.redraw();
    }

    fn copy_path_to_clipboard(&mut self) {
        let Some(path) = self.nav.current_path() else {
            return;
        };
        let text = path.display().to_string();
        match Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => self.show_status("Path copied"),
            Err(_) => self.show_status("Copy failed"),
        }
        self.redraw();
    }

    fn stop_slideshow(&mut self) {
        self.slideshow_next = None;
    }

    fn open_explorer(&mut self) {
        if let Some(path) = self.nav.current_path() {
            let mut exp = ExplorerState::for_path(&path.clone());
            let (_, win_h) = self.win_size();
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor() as f32)
                .unwrap_or(1.0);
            exp.scroll_to_sel(win_h / scale);
            self.explorer = Some(exp);
        }
    }

    fn close_explorer(&mut self) {
        self.explorer = None;
    }

    /// Resize the window to the ideal size for the current image (used when opening a file from
    /// the empty state, so "Open With" gets a properly-sized window like `glanvu <file>` does).
    fn fit_window_to_image(&self) {
        if let Some(w) = &self.window {
            let (ww, wh) = ideal_window_size(self.img_size.0, self.img_size.1);
            let _ = w.request_inner_size(LogicalSize::new(ww, wh));
        }
    }

    /// Open a file received from the OS (Finder "Open With", drag-and-drop, Apple Events).
    /// Reloads the folder playlist if the file is in a different directory.
    fn open_file_path(&mut self, path: &std::path::Path) {
        if !glanvu_core::is_supported_path(path) {
            return;
        }
        let was_empty = self.mode == ViewMode::Empty;
        self.mode = ViewMode::Single; // leave Empty state
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        // Check if the file is already in the current playlist.
        if let Some(idx) = self.nav.paths.iter().position(|p| p == path) {
            if let Some(res) = self.nav.show_index(idx) {
                self.apply_nav_result(res);
            }
        } else {
            // Different folder — build a new playlist.
            let images = glanvu_core::list_images(&dir);
            let idx = images.iter().position(|p| p == path).unwrap_or(0);
            match glanvu_core::decode_path(path) {
                Ok(img) => {
                    self.nav = FolderNav::new(images, idx, path.to_path_buf(), img);
                    if let Some(res) = self.nav.show_index(idx) {
                        self.apply_nav_result(res);
                    }
                    self.close_explorer();
                }
                Err(e) => {
                    eprintln!("glanvu: cannot open {}: {e}", path.display());
                    return;
                }
            }
        }

        // Opening from the empty state (Open With / drop on launch): size the window to the image,
        // matching `glanvu <file>` behavior. Skip on normal navigation (keeps the user's window size).
        if was_empty {
            self.fit_window_to_image();
        }
    }

    fn enter_grid(&mut self) {
        self.stop_slideshow();
        self.mode = ViewMode::Grid;
        self.grid.sel = self.nav.index;
        let (win_w, win_h) = self.win_size();
        self.grid.scroll_to_sel(self.nav.paths.len(), win_w, win_h);
        // Request thumbnails for all paths (worker handles dedup).
        for p in &self.nav.paths.clone() {
            self.thumbs.request(p);
        }
        self.update_title_grid();
        // Paint immediately so the grid appears without waiting for a user action
        // (same fix as the single-image blank-first-frame issue on macOS).
        self.draw();
        self.redraw();
    }

    fn enter_single(&mut self, idx: usize) {
        self.mode = ViewMode::Single;
        if let Some(res) = self.nav.show_index(idx) {
            self.apply_nav_result(res);
        }
    }

    fn win_size(&self) -> (f32, f32) {
        self.window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (s.width as f32, s.height as f32)
            })
            .unwrap_or((800.0, 600.0))
    }

    fn update_title_grid(&self) {
        if let Some(w) = &self.window {
            w.set_title(&format!("Glanvu — {} images", self.nav.paths.len()));
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = &self.window {
            w.set_fullscreen(if w.fullscreen().is_some() {
                None
            } else {
                Some(Fullscreen::Borderless(None))
            });
        }
    }

    fn update_title(&self, index: usize, total: usize, path: &Path) {
        if let Some(w) = &self.window {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            w.set_title(&format!("{name} ({}/{total}) - Glanvu", index + 1));
        }
    }

    fn apply_nav_result(&mut self, res: glanvu_viewer_core::nav::ShowResult) {
        self.img_size = res.img_size;
        if let (Some(gpu), Some(img)) = (self.gpu.as_mut(), self.nav.current_image()) {
            gpu.set_image(img);
        }
        self.state = ViewState::fit();
        self.update_title(res.index, res.total, &res.path);
        self.flash_overlay();

        if perf_logging() {
            let name = res
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kind = if res.cache_hit {
                "prefetched"
            } else {
                "decoded"
            };
            eprintln!(
                "glanvu: switch to {name} in {:.1} ms ({kind})",
                res.elapsed_ms
            );
        }
        self.draw();
    }

    fn draw(&mut self) {
        // Drain any newly generated thumbnails and upload them to the GPU.
        // We clone the images to avoid the borrow-checker conflict between `self.thumbs`
        // and `self.gpu`. Thumbnails are small (THUMB_W×THUMB_H×4 bytes ≈ 75 KB); cloning
        // is acceptable here and happens at most once per thumbnail per session.
        if self.thumbs.drain() {
            let to_upload: Vec<(PathBuf, DecodedImage)> = self
                .nav
                .paths
                .iter()
                .filter(|p| {
                    self.thumbs.get(p).is_some()
                        && !self
                            .gpu
                            .as_ref()
                            .map(|g| g.thumb_binds.contains_key(*p))
                            .unwrap_or(false)
                })
                .filter_map(|p| self.thumbs.get(p).map(|img| (p.clone(), img.clone())))
                .collect();
            if let Some(gpu) = self.gpu.as_mut() {
                for (path, img) in to_upload {
                    gpu.upload_thumb(path, &img);
                }
            }
        }

        // Empty state: dark window with a centred prompt.
        if self.mode == ViewMode::Empty {
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor() as f32)
                .unwrap_or(1.0);
            let Some(gpu) = self.gpu.as_mut() else { return };
            let win = (gpu.config.width as f32, gpu.config.height as f32);
            // Re-use the status overlay as a centred hint text (no timer — always visible).
            let hint = "Drop an image here  ·  Press Enter to open";
            let confirm_text = self.confirm_assoc.map(confirm_overlay_text);
            let _ = gpu.render(
                Uniforms {
                    mvp: mvp((1, 1), win, &self.state),
                },
                None,
                None,
                Some(hint),
                None,
                self.help_visible,
                confirm_text,
                self.about_visible,
                scale,
                true,
            );
            return;
        }

        if self.mode == ViewMode::Grid {
            let Some(gpu) = self.gpu.as_mut() else { return };
            let paths = self.nav.paths.clone(); // clone needed to split borrow from gpu
            let presented = gpu.render_grid(&paths, &self.grid);
            if !presented {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            return;
        }

        // Single-image mode (original path).
        let now = Instant::now();
        let overlay_active = matches!(self.overlay_until, Some(t) if now < t);
        let overlay = if overlay_active {
            self.nav.current_path().map(|p| p.display().to_string())
        } else {
            None
        };
        let date_overlay = if overlay_active && !self.date_text.is_empty() {
            Some(self.date_text.as_str())
        } else {
            None
        };
        let status = match self.status_until {
            Some(t) if now < t => Some(self.status_text.as_str()),
            _ => None,
        };
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        let Some(gpu) = self.gpu.as_mut() else { return };
        let win = (gpu.config.width as f32, gpu.config.height as f32);
        let explorer_ref = self.explorer.as_ref();
        let confirm_text = self.confirm_assoc.map(confirm_overlay_text);
        let presented = gpu.render(
            Uniforms {
                mvp: mvp(self.img_size, win, &self.state),
            },
            overlay.as_deref(),
            date_overlay,
            status,
            explorer_ref,
            self.help_visible,
            confirm_text,
            self.about_visible,
            scale,
            false,
        );
        if presented {
            if !self.first_frame {
                self.first_frame = true;
                if perf_logging() {
                    let ms = self.start.elapsed().as_secs_f64() * 1000.0;
                    eprintln!("glanvu: first frame in {ms:.1} ms (cold: incl. one-time gpu init)");
                }
            }
        } else if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let (ww, wh) = ideal_window_size(self.img_size.0, self.img_size.1);
        let attrs = Window::default_attributes()
            .with_title("Glanvu")
            .with_inner_size(LogicalSize::new(ww, wh));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let gpu = {
            let img = self
                .nav
                .current_image()
                .expect("current image must exist before resume");
            pollster::block_on(Gpu::new(window.clone(), img))
        };
        self.window = Some(window);
        self.gpu = Some(gpu);
        if let Some(current) = self.nav.current_path() {
            self.update_title(self.nav.index, self.nav.paths.len(), &current.clone());
        }
        self.flash_overlay();
        self.nav.prune_and_prefetch();
        self.draw();
        self.redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size);
                }
                self.redraw();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // If the about overlay is up, any keypress dismisses it.
                if self.about_visible {
                    self.about_visible = false;
                    self.redraw();
                    return;
                }

                // If the help overlay is up, the next keypress dismisses it.
                if self.help_visible {
                    self.help_visible = false;
                    self.redraw();
                    return;
                }

                // If the confirm overlay is up, Enter confirms, Esc cancels, anything else is ignored.
                if let Some(set) = self.confirm_assoc {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Enter) => {
                            self.confirm_assoc = None;
                            self.show_status("Working…");
                            self.redraw();
                            std::thread::spawn(move || {
                                let msg = if set {
                                    crate::associate::set_default_blocking()
                                } else {
                                    crate::associate::unset_default_blocking()
                                };
                                if let Ok(mut r) = crate::associate::ASSOC_RESULT.lock() {
                                    *r = Some(msg);
                                }
                            });
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.confirm_assoc = None;
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Empty mode: Esc/Q to quit, Enter to open a file, H for help.
                if self.mode == ViewMode::Empty {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Escape)
                        | Key::Character("q")
                        | Key::Character("Q") => event_loop.exit(),
                        Key::Character("h") | Key::Character("H") | Key::Character("?") => {
                            self.help_visible = true;
                            self.redraw();
                        }
                        Key::Character("d") | Key::Character("D") => {
                            self.confirm_assoc = Some(true);
                            self.redraw();
                        }
                        Key::Character("u") | Key::Character("U") => {
                            self.confirm_assoc = Some(false);
                            self.redraw();
                        }
                        Key::Named(NamedKey::Enter) => {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_title("Open image — Glanvu")
                                .add_filter(
                                    "Images",
                                    &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp"],
                                )
                                .add_filter("All files", &["*"])
                                .pick_file()
                            {
                                self.open_file_path(&path);
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // Tab / G toggles between single-image and grid mode.
                let is_grid_toggle = event.logical_key == Key::Named(NamedKey::Tab)
                    || matches!(
                        event.logical_key.as_ref(),
                        Key::Character("g") | Key::Character("G")
                    );
                if is_grid_toggle {
                    match self.mode {
                        ViewMode::Single => self.enter_grid(),
                        ViewMode::Grid => self.enter_single(self.grid.sel),
                        ViewMode::Empty => {} // handled above
                    }
                    return;
                }
                // In grid mode, arrows navigate tiles and Enter opens the selection.
                if self.mode == ViewMode::Grid {
                    let (win_w, win_h) = self.win_size();
                    let n = self.nav.paths.len();
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Escape)
                        | Key::Character("q")
                        | Key::Character("Q") => {
                            event_loop.exit();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let sel = self.grid.sel;
                            self.enter_single(sel);
                        }
                        // S from grid: open selected image and start slideshow from there.
                        Key::Character("s") | Key::Character("S") => {
                            let sel = self.grid.sel;
                            self.enter_single(sel);
                            self.toggle_slideshow();
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            self.grid.move_sel(1, 0, n, win_w);
                            self.grid.scroll_to_sel(n, win_w, win_h);
                            self.redraw();
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            self.grid.move_sel(-1, 0, n, win_w);
                            self.grid.scroll_to_sel(n, win_w, win_h);
                            self.redraw();
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            self.grid.move_sel(0, 1, n, win_w);
                            self.grid.scroll_to_sel(n, win_w, win_h);
                            self.redraw();
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.grid.move_sel(0, -1, n, win_w);
                            self.grid.scroll_to_sel(n, win_w, win_h);
                            self.redraw();
                        }
                        Key::Named(NamedKey::Home) => {
                            self.grid.sel = 0;
                            self.grid.scroll_to_sel(n, win_w, win_h);
                            self.redraw();
                        }
                        Key::Named(NamedKey::End) => {
                            if n > 0 {
                                self.grid.sel = n - 1;
                            }
                            self.grid.scroll_to_sel(n, win_w, win_h);
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Explorer panel: intercept keys when open.
                if self.explorer.is_some() {
                    let (_, win_h) = self.win_size();
                    let scale = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor() as f32)
                        .unwrap_or(1.0);
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter)
                            if matches!(
                                event.logical_key.as_ref(),
                                Key::Named(NamedKey::Escape)
                            ) =>
                        {
                            self.close_explorer();
                            self.redraw();
                        }
                        Key::Named(NamedKey::Enter) => {
                            if let Some(exp) = &self.explorer {
                                match exp.open_sel() {
                                    glanvu_viewer_core::explorer::OpenResult::OpenImage(path) => {
                                        self.close_explorer();
                                        let nav_dir = path
                                            .parent()
                                            .filter(|p| !p.as_os_str().is_empty())
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or_else(|| PathBuf::from("."));
                                        if nav_dir
                                            == self
                                                .nav
                                                .paths
                                                .first()
                                                .and_then(|p| p.parent())
                                                .map(|p| p.to_path_buf())
                                                .unwrap_or_default()
                                        {
                                            // Same folder — just find & show the image.
                                            if let Some(idx) =
                                                self.nav.paths.iter().position(|p| p == &path)
                                            {
                                                if let Some(res) = self.nav.show_index(idx) {
                                                    self.apply_nav_result(res);
                                                }
                                            }
                                        } else {
                                            // Different folder — reload playlist and open.
                                            let images = glanvu_core::list_images(&nav_dir);
                                            if let Some(idx) =
                                                images.iter().position(|p| p == &path)
                                            {
                                                if let Ok(img) = glanvu_core::decode_path(&path) {
                                                    let new_nav =
                                                        glanvu_viewer_core::nav::FolderNav::new(
                                                            images, idx, path, img,
                                                        );
                                                    self.nav = new_nav;
                                                    if let Some(res) =
                                                        self.nav.show_index(self.nav.index)
                                                    {
                                                        self.apply_nav_result(res);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    glanvu_viewer_core::explorer::OpenResult::NavigateDir(dir) => {
                                        if let Some(exp) = self.explorer.as_mut() {
                                            exp.set_dir(dir);
                                            exp.scroll_to_sel(win_h / scale);
                                            self.redraw();
                                        }
                                    }
                                    glanvu_viewer_core::explorer::OpenResult::Nothing => {}
                                }
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(exp) = self.explorer.as_mut() {
                                exp.move_sel(1);
                                exp.scroll_to_sel(win_h / scale);
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(exp) = self.explorer.as_mut() {
                                exp.move_sel(-1);
                                exp.scroll_to_sel(win_h / scale);
                                self.redraw();
                            }
                        }
                        _ => {} // other keys fall through to single-mode handler
                    }
                    return;
                }

                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character("q") | Key::Character("Q") => event_loop.exit(),
                    Key::Character("h") | Key::Character("H") | Key::Character("?") => {
                        self.help_visible = true;
                        self.about_visible = false;
                        self.redraw();
                    }
                    Key::Character("a") | Key::Character("A") => {
                        self.about_visible = !self.about_visible;
                        self.help_visible = false;
                        self.redraw();
                    }
                    Key::Character("d") | Key::Character("D") => {
                        self.confirm_assoc = Some(true);
                        self.redraw();
                    }
                    Key::Character("u") | Key::Character("U") => {
                        self.confirm_assoc = Some(false);
                        self.redraw();
                    }
                    Key::Named(NamedKey::Space)
                    | Key::Named(NamedKey::F11)
                    | Key::Character("f")
                    | Key::Character("F") => {
                        self.toggle_fullscreen();
                        self.flash_overlay();
                        self.redraw();
                    }
                    Key::Character("r") | Key::Character("R") => {
                        self.state.quarter_turns = (self.state.quarter_turns + 1) % 4;
                        self.redraw();
                    }
                    Key::Character("0") => {
                        self.state = ViewState::fit();
                        self.redraw();
                    }
                    Key::Character("1") => {
                        self.state.fit = false;
                        self.state.zoom = 1.0;
                        self.state.pan = (0.0, 0.0);
                        self.redraw();
                    }
                    Key::Character("+") | Key::Character("=") => {
                        self.state.zoom *= 1.25;
                        self.redraw();
                    }
                    Key::Character("-") | Key::Character("_") => {
                        self.state.zoom /= 1.25;
                        self.redraw();
                    }
                    Key::Character("s") | Key::Character("S") => {
                        self.toggle_slideshow();
                        self.redraw();
                    }
                    // Enter opens / closes the directory explorer.
                    Key::Named(NamedKey::Enter) => {
                        self.open_explorer();
                        self.redraw();
                    }
                    // Manual navigation stops the slideshow (user took control).
                    Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::ArrowDown) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.next() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::ArrowUp) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.prev() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Named(NamedKey::Home) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.first() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Named(NamedKey::End) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.last() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Character("c") => self.copy_image_to_clipboard(),
                    Key::Character("C") => self.copy_path_to_clipboard(),
                    Key::Character("o") | Key::Character("O") => {
                        self.sort_mode = self.sort_mode.next();
                        self.nav.resort(self.sort_mode);
                        self.show_status(self.sort_mode.label());
                        self.redraw();
                    }
                    _ => {}
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                if dy != 0.0 {
                    if self.mode == ViewMode::Grid {
                        let (win_w, win_h) = self.win_size();
                        let n = self.nav.paths.len();
                        self.grid.scroll_y -= dy * (CELL_H + GAP) * 0.5;
                        let max_s = (GridState::total_height(n, win_w) - win_h).max(0.0);
                        self.grid.scroll_y = self.grid.scroll_y.clamp(0.0, max_s);
                    } else {
                        self.state.zoom *= 1.1_f32.powf(dy);
                    }
                    self.redraw();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    if self.mode == ViewMode::Grid && state == ElementState::Pressed {
                        let (win_w, _) = self.win_size();
                        let n = self.nav.paths.len();
                        if let Some(idx) =
                            self.grid
                                .hit_test(self.cursor.x as f32, self.cursor.y as f32, win_w, n)
                        {
                            let now = Instant::now();
                            let is_double = self.last_grid_click.is_some_and(|(t, prev)| {
                                prev == idx && now.duration_since(t).as_millis() < 400
                            });
                            if is_double {
                                self.last_grid_click = None;
                                self.enter_single(idx);
                            } else {
                                self.last_grid_click = Some((now, idx));
                                self.grid.sel = idx;
                                self.redraw();
                            }
                        }
                    }
                    // Explorer click: select or open entry.
                    // Use GPU-computed width (actual font metrics) for accurate click detection.
                    let exp_panel_w = self
                        .gpu
                        .as_ref()
                        .map(|g| g.explorer_computed_panel_w)
                        .unwrap_or(PANEL_W);
                    if self.mode == ViewMode::Single
                        && self.explorer.is_some()
                        && state == ElementState::Pressed
                        && (self.cursor.x as f32) < exp_panel_w
                    {
                        let my = self.cursor.y as f32;
                        let (_, win_h) = self.win_size();
                        let scale = self
                            .window
                            .as_ref()
                            .map(|w| w.scale_factor() as f32)
                            .unwrap_or(1.0);
                        let hit = self.explorer.as_ref().and_then(|e| e.hit_entry(my / scale));
                        if let Some(idx) = hit {
                            if self.explorer.as_ref().map(|e| e.sel) == Some(idx) {
                                // Double-tap: open (same entry selected twice).
                                let action = self.explorer.as_ref().map(|e| e.open_sel());
                                match action {
                                    Some(
                                        glanvu_viewer_core::explorer::OpenResult::NavigateDir(dir),
                                    ) => {
                                        if let Some(exp) = self.explorer.as_mut() {
                                            exp.set_dir(dir);
                                            exp.scroll_to_sel(win_h / scale);
                                        }
                                    }
                                    Some(glanvu_viewer_core::explorer::OpenResult::OpenImage(
                                        path,
                                    )) => {
                                        self.close_explorer();
                                        if let Some(pos) =
                                            self.nav.paths.iter().position(|p| p == &path)
                                        {
                                            if let Some(res) = self.nav.show_index(pos) {
                                                self.apply_nav_result(res);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                // First tap: just select.
                                if let Some(exp) = self.explorer.as_mut() {
                                    exp.sel = idx;
                                }
                            }
                            self.redraw();
                        }
                        return; // don't start drag when clicking in the panel
                    }
                    self.dragging = state == ElementState::Pressed;
                }
                // Donation link click detection. Hit zones are computed during render (exact text
                // metrics), so here we just test the cursor against them — no geometry duplication.
                if button == MouseButton::Left && state == ElementState::Pressed {
                    let cx = self.cursor.x as f32;
                    let cy = self.cursor.y as f32;
                    if let Some(gpu) = self.gpu.as_ref() {
                        for hit in &gpu.donate_hits {
                            if cx >= hit.x0 && cx < hit.x1 && cy >= hit.y0 && cy < hit.y1 {
                                let url = if cx < hit.split_x {
                                    KOFI_URL
                                } else {
                                    GITHUB_SPONSORS_URL
                                };
                                let _ = open::that(url);
                                break;
                            }
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging && self.mode == ViewMode::Single {
                    self.state.pan.0 += (position.x - self.cursor.x) as f32;
                    self.state.pan.1 -= (position.y - self.cursor.y) as f32;
                    self.redraw();
                }
                self.cursor = position;
            }

            WindowEvent::RedrawRequested => self.draw(),

            // macOS "Open With" / Finder sends files via Apple Events → winit → DroppedFile.
            // Also handles literal drag-and-drop onto the window.
            WindowEvent::DroppedFile(path) => {
                self.open_file_path(&path.clone());
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        // macOS: drain any files queued by applicationOpenFile: (Finder "Open With").
        #[cfg(target_os = "macos")]
        {
            let pending: Vec<PathBuf> = crate::macos_open::PENDING_OPEN_PATHS
                .lock()
                .map(|mut v| v.drain(..).collect())
                .unwrap_or_default();
            for path in pending {
                self.open_file_path(&path);
            }
        }

        // Drain result from background set-default / unset-default task.
        if let Ok(mut slot) = crate::associate::ASSOC_RESULT.lock() {
            if let Some(msg) = slot.take() {
                self.show_status(&msg);
                self.redraw();
            }
        }

        // Grid thumbnail polling: redraw immediately so draw() drains the worker.
        if self.mode == ViewMode::Grid && self.nav.paths.iter().any(|p| self.thumbs.is_pending(p)) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(80)));
            return;
        }

        // Slideshow: advance when the timer fires.
        if let Some(t) = self.slideshow_next {
            if now >= t {
                if let Some(res) = self.nav.next() {
                    self.apply_nav_result(res);
                }
                self.slideshow_next = Some(now + self.slideshow_interval);
            }
        }

        // Expire the overlay timer.
        if let Some(t) = self.overlay_until {
            if now >= t {
                self.overlay_until = None;
                self.redraw();
            }
        }

        // Schedule the next wake-up at the earliest pending deadline.
        // Expire the status overlay timer.
        if let Some(t) = self.status_until {
            if Instant::now() >= t {
                self.status_until = None;
                self.redraw();
            }
        }

        let deadline = [self.overlay_until, self.slideshow_next, self.status_until]
            .into_iter()
            .flatten()
            .min();
        match deadline {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(path: &str) -> ExitCode {
    let start = Instant::now();
    let first_path = PathBuf::from(path);

    let first_image = match glanvu_core::decode_path(&first_path) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("glanvu: cannot open {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let img_size = (first_image.width, first_image.height);

    let dir = first_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut paths = glanvu_core::list_images(&dir);
    let index = locate(&paths, &first_path).unwrap_or_else(|| {
        paths = vec![first_path.clone()];
        0
    });

    let nav = FolderNav::new(paths, index, first_path, first_image);

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("glanvu: cannot start event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    // macOS: patch the (now-registered) WinitApplicationDelegate to handle "Open With" before the
    // app runs, so the kAEOpenDocuments Apple Event delivered during launch is handled (queued).
    #[cfg(target_os = "macos")]
    crate::macos_open::install();

    let mut app = App {
        start,
        nav,
        img_size,
        state: ViewState::fit(),
        mode: ViewMode::Single,
        grid: GridState::new(index),
        thumbs: ThumbnailCache::new(),
        window: None,
        gpu: None,
        first_frame: false,
        cursor: PhysicalPosition::new(0.0, 0.0),
        dragging: false,
        overlay_until: None,
        slideshow_next: None,
        slideshow_interval: Duration::from_secs(3),
        status_text: String::new(),
        status_until: None,
        explorer: None,
        help_visible: false,
        about_visible: false,
        confirm_assoc: None,
        last_grid_click: None,
        sort_mode: glanvu_viewer_core::nav::SortMode::default(),
        date_text: String::new(),
    };

    match event_loop.run_app(&mut app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glanvu: viewer error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Launch the viewer without an initial file.
///
/// Used when the app is opened without arguments (double-click on .app, Dock, Spotlight).
/// A dark window appears with a "drop or press Enter" prompt; any file received via
/// `DroppedFile` (macOS Apple Events from "Open With") or by pressing Enter opens normally.
pub fn run_empty() -> ExitCode {
    let start = Instant::now();

    // Placeholder 1×1 black image — replaced as soon as a real file arrives.
    let placeholder = DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![18, 18, 20, 255],
    };
    let placeholder_path = PathBuf::from("(none)");
    let nav = FolderNav::new(
        vec![placeholder_path.clone()],
        0,
        placeholder_path,
        placeholder,
    );

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("glanvu: cannot start event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    // macOS: patch the (now-registered) WinitApplicationDelegate to handle "Open With" before the
    // app runs, so the kAEOpenDocuments Apple Event delivered during launch is handled (queued).
    #[cfg(target_os = "macos")]
    crate::macos_open::install();

    let mut app = App {
        start,
        nav,
        img_size: (1, 1),
        state: ViewState::fit(),
        mode: ViewMode::Empty, // special empty state
        grid: GridState::new(0),
        thumbs: ThumbnailCache::new(),
        window: None,
        gpu: None,
        first_frame: false,
        cursor: PhysicalPosition::new(0.0, 0.0),
        dragging: false,
        overlay_until: None,
        slideshow_next: None,
        slideshow_interval: Duration::from_secs(3),
        status_text: String::new(),
        status_until: None,
        explorer: None,
        help_visible: false,
        about_visible: false,
        confirm_assoc: None,
        last_grid_click: None,
        sort_mode: glanvu_viewer_core::nav::SortMode::default(),
        date_text: String::new(),
    };

    match event_loop.run_app(&mut app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glanvu: viewer error: {e}");
            ExitCode::FAILURE
        }
    }
}
