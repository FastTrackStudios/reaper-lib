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
    Adapter, Device, Extent3d, Features, Instance, MemoryHints, Queue, RequestAdapterError,
    Surface, SurfaceConfiguration, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages,
};

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("Failed to create WGPU surface: {0}")]
    SurfaceCreation(#[from] wgpu::CreateSurfaceError),

    #[error("No compatible GPU adapter found: {0}")]
    NoAdapter(#[from] RequestAdapterError),

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

/// GPU state for Vello rendering to a WGPU surface.
pub struct GpuState {
    pub device: Device,
    pub queue: Queue,
    pub surface: Surface<'static>,
    pub surface_config: SurfaceConfiguration,
    pub vello_renderer: VelloRenderer,
    intermediate_texture: wgpu::Texture,
    texture_blitter: TextureBlitter,
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
                trace: wgpu::Trace::default(),
                experimental_features: Default::default(),
            }))?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
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

        tracing::debug!(
            ?alpha_mode,
            available = ?surface_caps.alpha_modes,
            "Surface alpha mode"
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
            surface,
            surface_config,
            vello_renderer,
            intermediate_texture,
            texture_blitter,
            adapter,
        })
    }

    /// Resize the surface.
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);

        if self.surface_config.width != width || self.surface_config.height != height {
            self.surface_config.width = width;
            self.surface_config.height = height;
            self.surface.configure(&self.device, &self.surface_config);
            self.intermediate_texture = create_intermediate_texture(&self.device, width, height);
        }
    }

    /// Render a Vello scene to the surface.
    pub fn render(&mut self, scene: &Scene) -> Result<(), GpuError> {
        let surface_texture = self.surface.get_current_texture()?;

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

    pub fn size(&self) -> (u32, u32) {
        (self.surface_config.width, self.surface_config.height)
    }
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
            push_constant_ranges: &[],
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
            multiview: None,
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
