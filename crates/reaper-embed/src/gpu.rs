//! GPU management for WGPU + Vello rendering.
//!
//! Provides `GpuState` which owns a WGPU device, surface, and Vello renderer.
//! Renders Vello scenes to an intermediate `Rgba8Unorm` texture (required by
//! Vello's compute shaders), then blits to the surface via a fullscreen-quad
//! WGSL shader for format conversion.

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle,
};
use thiserror::Error;
use vello::{AaSupport, RenderParams, Renderer as VelloRenderer, RendererOptions, Scene};
use wgpu::{
    Adapter, Device, Extent3d, Features, Instance, MemoryHints, Queue, Surface,
    SurfaceConfiguration, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("Failed to create WGPU surface: {0}")]
    SurfaceCreation(#[from] wgpu::CreateSurfaceError),

    #[error("No compatible GPU adapter found: {0}")]
    NoAdapter(#[from] wgpu::RequestAdapterError),

    #[error("Failed to request GPU device: {0}")]
    DeviceRequest(#[from] wgpu::RequestDeviceError),

    #[error("Failed to create Vello renderer: {0}")]
    VelloRenderer(#[from] vello::Error),

    #[error("Surface texture error: {0}")]
    SurfaceTexture(#[from] wgpu::SurfaceError),

    #[error("Invalid window handle")]
    InvalidWindowHandle,
}

/// Wrapper for raw window handles implementing `HasWindowHandle + HasDisplayHandle`.
struct RawHandleWrapper {
    window_handle: RawWindowHandle,
    display_handle: RawDisplayHandle,
}

// SAFETY: REAPER extensions run single-threaded on the main thread.
unsafe impl Send for RawHandleWrapper {}
unsafe impl Sync for RawHandleWrapper {}

impl HasWindowHandle for RawHandleWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(self.window_handle) })
    }
}

impl HasDisplayHandle for RawHandleWrapper {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(self.display_handle) })
    }
}

/// GPU state for Vello rendering.
///
/// Supports two modes:
/// - **Surface mode** (default): direct wgpu present to a window surface.
///   Zero-copy, GPU-only path.
/// - **Offscreen mode**: renders to a BGRA8 texture whose bits can be copied
///   back to CPU via [`GpuState::read_pixels`]. Used on Linux docked panels
///   where SWELL owns the HWND and we must blit via WM_PAINT +
///   `StretchBltFromMem` (matches reaimgui's gdk_opengl.cpp:86-260).
///
/// Field order matters: Rust drops fields top-to-bottom. Surface and textures
/// must be dropped before Device to avoid wgpu validation errors.
pub struct GpuState {
    // Rendering resources (dropped first)
    pub vello_renderer: VelloRenderer,
    intermediate_texture: wgpu::Texture,
    texture_blitter: TextureBlitter,
    /// BGRA8 present target for offscreen mode; None in surface mode.
    offscreen_target: Option<wgpu::Texture>,
    /// Staging buffer for GPU→CPU readback in offscreen mode; None in surface mode.
    readback_buffer: Option<wgpu::Buffer>,
    /// Padded bytes-per-row for the readback buffer (wgpu requires 256-byte alignment).
    readback_row_pitch: u32,
    pub surface_config: SurfaceConfiguration,
    /// Present surface; None in offscreen mode.
    // Surface dropped before device (wgpu requirement)
    pub surface: Option<Surface<'static>>,
    // Core GPU handles (dropped last)
    pub queue: Queue,
    pub device: Device,
    #[allow(dead_code)]
    adapter: Adapter,
}

impl GpuState {
    /// Create GPU state for a window.
    ///
    /// # Safety
    /// The window must remain valid for the lifetime of the returned `GpuState`.
    pub fn new<W>(window: &W, width: u32, height: u32) -> Result<Self, GpuError>
    where
        W: HasWindowHandle + HasDisplayHandle,
    {
        let window_handle = window
            .window_handle()
            .map_err(|_| GpuError::InvalidWindowHandle)?
            .as_raw();
        let display_handle = window
            .display_handle()
            .map_err(|_| GpuError::InvalidWindowHandle)?
            .as_raw();

        let handle_wrapper = RawHandleWrapper {
            window_handle,
            display_handle,
        };

        let instance = Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or_default(),
            flags: wgpu::InstanceFlags::from_build_config().with_env(),
            backend_options: wgpu::BackendOptions::from_env_or_default(),
            ..Default::default()
        });

        let surface = instance.create_surface(handle_wrapper)?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        let features = adapter.features() & Features::empty();
        let limits = adapter.limits();

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("reaper-embed"),
                required_features: features,
                required_limits: limits,
                memory_hints: MemoryHints::Performance,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::default(),
            }))?;

        let surface_caps = surface.get_capabilities(&adapter);
        // Prefer non-sRGB format: Vello outputs sRGB values into Rgba8Unorm,
        // so using an sRGB surface would double-apply gamma correction.
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        // Prefer PreMultiplied alpha for transparency support
        let alpha_mode = surface_caps
            .alpha_modes
            .iter()
            .find(|m| **m == wgpu::CompositeAlphaMode::PreMultiplied)
            .or_else(|| {
                surface_caps
                    .alpha_modes
                    .iter()
                    .find(|m| **m == wgpu::CompositeAlphaMode::PostMultiplied)
            })
            .copied()
            .unwrap_or(surface_caps.alpha_modes[0]);

        tracing::info!(
            ?surface_format,
            ?alpha_mode,
            available_formats = ?surface_caps.formats,
            available_alpha = ?surface_caps.alpha_modes,
            "GPU surface config"
        );

        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST,
            format: surface_format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let vello_renderer = VelloRenderer::new(
            &device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: AaSupport::area_only(),
                num_init_threads: std::num::NonZeroUsize::new(1),
                pipeline_cache: None,
            },
        )?;

        let intermediate_texture =
            create_intermediate_texture(&device, width.max(1), height.max(1));
        let texture_blitter = TextureBlitter::new(&device, surface_format);

        Ok(Self {
            device,
            queue,
            surface: Some(surface),
            surface_config,
            vello_renderer,
            intermediate_texture,
            texture_blitter,
            offscreen_target: None,
            readback_buffer: None,
            readback_row_pitch: 0,
            adapter,
        })
    }

    /// Create a GPU state that renders offscreen (no window surface).
    ///
    /// The caller reads back rendered pixels via [`read_pixels`] and is
    /// responsible for blitting them to a window (e.g., via SWELL
    /// `StretchBltFromMem` under `WM_PAINT`). Output format is BGRA8 to
    /// match LICE's pixel layout on Linux/macOS SWELL.
    pub fn new_offscreen(width: u32, height: u32) -> Result<Self, GpuError> {
        let instance = Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or_default(),
            flags: wgpu::InstanceFlags::from_build_config().with_env(),
            backend_options: wgpu::BackendOptions::from_env_or_default(),
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;

        let features = adapter.features() & Features::empty();
        let limits = adapter.limits();

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("reaper-embed-offscreen"),
                required_features: features,
                required_limits: limits,
                memory_hints: MemoryHints::Performance,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::default(),
            }))?;

        let present_format = TextureFormat::Bgra8Unorm;
        let vello_renderer = VelloRenderer::new(
            &device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: AaSupport::area_only(),
                num_init_threads: std::num::NonZeroUsize::new(1),
                pipeline_cache: None,
            },
        )?;

        let width = width.max(1);
        let height = height.max(1);
        let intermediate_texture = create_intermediate_texture(&device, width, height);
        let texture_blitter = TextureBlitter::new(&device, present_format);
        let (offscreen_target, readback_buffer, row_pitch) =
            create_offscreen_resources(&device, width, height, present_format);

        // Surface config shape is preserved so shared code can read width/height.
        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST,
            format: present_format,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };

        tracing::info!(width, height, "Offscreen GPU state created");

        Ok(Self {
            device,
            queue,
            surface: None,
            surface_config,
            vello_renderer,
            intermediate_texture,
            texture_blitter,
            offscreen_target: Some(offscreen_target),
            readback_buffer: Some(readback_buffer),
            readback_row_pitch: row_pitch,
            adapter,
        })
    }

    /// Resize the present target (surface or offscreen).
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);

        if self.surface_config.width == width && self.surface_config.height == height {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.intermediate_texture = create_intermediate_texture(&self.device, width, height);

        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.surface_config);
        }
        if self.offscreen_target.is_some() {
            let (tex, buf, row_pitch) =
                create_offscreen_resources(&self.device, width, height, self.surface_config.format);
            self.offscreen_target = Some(tex);
            self.readback_buffer = Some(buf);
            self.readback_row_pitch = row_pitch;
        }
    }

    /// Render a Vello scene to the surface (panics in offscreen mode — use
    /// [`render_offscreen`] instead).
    pub fn render(&mut self, scene: &Scene) -> Result<(), GpuError> {
        let surface = self
            .surface
            .as_ref()
            .expect("render() requires surface mode; use render_offscreen");
        let surface_texture = surface.get_current_texture()?;

        let intermediate_view = self
            .intermediate_texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let render_params = RenderParams {
            base_color: vello::peniko::Color::TRANSPARENT,
            width: self.surface_config.width,
            height: self.surface_config.height,
            antialiasing_method: vello::AaConfig::Area,
        };

        // Render Vello scene → intermediate Rgba8Unorm texture
        self.vello_renderer.render_to_texture(
            &self.device,
            &self.queue,
            scene,
            &intermediate_view,
            &render_params,
        )?;

        // Blit intermediate → surface (format conversion)
        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        self.texture_blitter
            .blit(&self.device, &self.queue, &intermediate_view, &surface_view);

        surface_texture.present();
        Ok(())
    }

    /// Render a Vello scene to the offscreen BGRA8 target (panics in surface mode).
    ///
    /// After this returns, [`read_pixels`] can be used to copy the rendered
    /// bytes back to a CPU buffer suitable for `StretchBltFromMem`.
    pub fn render_offscreen(&mut self, scene: &Scene) -> Result<(), GpuError> {
        let target = self
            .offscreen_target
            .as_ref()
            .expect("render_offscreen() requires offscreen mode; use render");

        let intermediate_view = self
            .intermediate_texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let render_params = RenderParams {
            base_color: vello::peniko::Color::TRANSPARENT,
            width: self.surface_config.width,
            height: self.surface_config.height,
            antialiasing_method: vello::AaConfig::Area,
        };

        // Render Vello scene → intermediate Rgba8Unorm texture.
        self.vello_renderer.render_to_texture(
            &self.device,
            &self.queue,
            scene,
            &intermediate_view,
            &render_params,
        )?;

        // Blit intermediate → BGRA8 offscreen target (format conversion).
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        self.texture_blitter
            .blit(&self.device, &self.queue, &intermediate_view, &target_view);
        Ok(())
    }

    /// Copy the offscreen target into `out` as tightly-packed BGRA8 rows.
    ///
    /// `out` is resized to `width * height * 4` bytes. Blocks until the GPU
    /// copy completes (synchronous mapping). Returns an error in surface mode.
    pub fn read_pixels(&self, out: &mut Vec<u8>) -> Result<(), GpuError> {
        let target = self
            .offscreen_target
            .as_ref()
            .ok_or(GpuError::InvalidWindowHandle)?;
        let buffer = self
            .readback_buffer
            .as_ref()
            .ok_or(GpuError::InvalidWindowHandle)?;

        let width = self.surface_config.width;
        let height = self.surface_config.height;
        let row_pitch = self.readback_row_pitch;

        // Copy texture → padded buffer
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Offscreen Readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_pitch),
                    rows_per_image: Some(height),
                },
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        // Map and copy the unpadded rows into `out`.
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        rx.recv().ok().and_then(|r| r.ok());

        let tight_stride = (width * 4) as usize;
        out.clear();
        out.reserve(tight_stride * height as usize);
        {
            let mapped = slice.get_mapped_range();
            for y in 0..height as usize {
                let start = y * row_pitch as usize;
                out.extend_from_slice(&mapped[start..start + tight_stride]);
            }
        }
        buffer.unmap();
        Ok(())
    }

    /// True if this GpuState was created with `new_offscreen`.
    pub fn is_offscreen(&self) -> bool {
        self.surface.is_none()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.surface_config.width, self.surface_config.height)
    }
}

/// Create the offscreen BGRA8 render target + padded staging buffer for readback.
fn create_offscreen_resources(
    device: &Device,
    width: u32,
    height: u32,
    format: TextureFormat,
) -> (wgpu::Texture, wgpu::Buffer, u32) {
    // wgpu requires 256-byte-aligned bytes-per-row for texture→buffer copies.
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let row_pitch = (unpadded + align - 1) / align * align;

    let target = device.create_texture(&TextureDescriptor {
        label: Some("Offscreen Present Target"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Offscreen Readback Buffer"),
        size: (row_pitch as u64) * (height as u64),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    (target, buffer, row_pitch)
}

fn create_intermediate_texture(device: &Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&TextureDescriptor {
        label: Some("Vello Intermediate Texture"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::STORAGE_BINDING
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Fullscreen-quad blit pipeline for copying between texture formats.
struct TextureBlitter {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
}

impl TextureBlitter {
    fn new(device: &Device, target_format: TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Texture Blit Shader"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Blit Bind Group Layout"),
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Blit Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Blit Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Blit Sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            sampler,
            bind_group_layout,
        }
    }

    fn blit(
        &self,
        device: &Device,
        queue: &Queue,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
    ) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Blit Bind Group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Blit Encoder"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Blit Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            render_pass.draw(0..6, 0..1);
        }

        queue.submit(std::iter::once(encoder.finish()));
    }
}

const BLIT_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );

    var tex_coords = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 0.0),
    );

    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.tex_coord = tex_coords[vertex_index];
    return output;
}

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, input.tex_coord);
}
"#;
