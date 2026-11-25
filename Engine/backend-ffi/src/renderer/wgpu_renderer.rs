use std::collections::HashMap;
use std::mem::size_of;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, OnceLock};

use super::params::LayerUniforms;

const SHADER: &str = include_str!("preview.wgsl");
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Evicted source textures kept around for reuse (same size => no driver
/// allocation). Small on purpose so memory stays bounded.
const TEXTURE_POOL_MAX: usize = 2;
/// Previous source frames kept beyond the ones the current composite uses.
/// A fixed large cache could eat hundreds of MiB at 1080p with many layers.
const SOURCE_CACHE_EXTRA: usize = 4;
/// Initial number of layers the shared uniform buffer can hold (grows by x2).
const INITIAL_LAYER_CAPACITY: usize = 16;

struct SourceResources {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// texture + sampler + the shared (dynamic-offset) uniform buffer. Built
    /// once per texture, so playback no longer creates a bind group per frame.
    bind_group: wgpu::BindGroup,
    /// Composite tick this source was last used on (for LRU eviction).
    last_used: u64,
}

struct TargetResources {
    width: u32,
    height: u32,
    padded_row: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
}

/// Per-layer part of the frame signature. Source frames are identified by
/// `source_key` (same key => same pixels, which the source cache relies on).
struct LayerSig {
    source_key: String,
    width: u32,
    height: u32,
}

/// Everything that determines the composited pixels for one frame.
struct FrameSig {
    tw: u32,
    th: u32,
    layers: Vec<LayerSig>,
    /// Strided uniform bytes for all layers, exactly as uploaded to the GPU.
    uniforms: Vec<u8>,
}

impl FrameSig {
    fn matches(&self, layers: &[GpuLayerFrame<'_>], uniforms: &[u8], tw: u32, th: u32) -> bool {
        self.tw == tw
            && self.th == th
            && self.layers.len() == layers.len()
            && self.uniforms == uniforms
            && self.layers.iter().zip(layers).all(|(s, l)| {
                s.width == l.width && s.height == l.height && s.source_key == l.source_key
            })
    }

    /// Overwrite in place so steady-state playback does no allocation here.
    fn update(&mut self, layers: &[GpuLayerFrame<'_>], uniforms: &[u8], tw: u32, th: u32) {
        self.tw = tw;
        self.th = th;
        self.layers.truncate(layers.len());
        for (i, l) in layers.iter().enumerate() {
            match self.layers.get_mut(i) {
                Some(s) => {
                    s.source_key.clear();
                    s.source_key.push_str(l.source_key);
                    s.width = l.width;
                    s.height = l.height;
                }
                None => self.layers.push(LayerSig {
                    source_key: l.source_key.to_owned(),
                    width: l.width,
                    height: l.height,
                }),
            }
        }
        self.uniforms.clear();
        self.uniforms.extend_from_slice(uniforms);
    }
}

struct LastFrame {
    sig: FrameSig,
    sig_valid: bool,
    /// Only filled once the same signature has been requested twice (paused
    /// playhead, held frame). Normal playback never pays for this copy.
    output: Vec<u8>,
    output_valid: bool,
}

struct RendererState {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    // Source pixels are cached independently from clip/layer uniforms. Multiple
    // layers can therefore share one texture upload while retaining separate
    // transforms, opacity, keyframes and blend order.
    sources: HashMap<String, SourceResources>,
    // Recently evicted source textures, reused for new frames of the same size.
    texture_pool: Vec<SourceResources>,
    target: Option<TargetResources>,
    last: LastFrame,
    // One uniform buffer for every layer of a frame. Layer `i` lives at
    // `i * uniform_stride` and is selected with a dynamic offset, so there is
    // no per-layer buffer, no per-layer map, and one write_buffer per frame.
    uniform_buffer: wgpu::Buffer,
    uniform_capacity: usize,
    uniform_stride: usize,
    uniform_scratch: Vec<u8>,
    tick: u64,
}

fn create_uniform_buffer(device: &wgpu::Device, stride: usize, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Hanabi shared layer uniforms"),
        size: (stride * capacity) as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    view: &wgpu::TextureView,
    uniforms: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Hanabi source bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: uniforms,
                    offset: 0,
                    size: NonZeroU64::new(size_of::<LayerUniforms>() as u64),
                }),
            },
        ],
    })
}

impl RendererState {
    fn create_source(&self, width: u32, height: u32) -> SourceResources {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Hanabi shared preview source texture"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let bind_group = make_bind_group(&self.device, &self.bind_layout, &self.sampler, &view, &self.uniform_buffer);
        SourceResources { width, height, texture, view, bind_group, last_used: 0 }
    }

    /// Grow the shared uniform buffer if needed. Bind groups reference the
    /// buffer, so every existing one (cached and pooled) is rebuilt. Rare.
    fn ensure_uniform_capacity(&mut self, layers: usize) {
        if layers <= self.uniform_capacity {
            return;
        }
        let capacity = layers.next_power_of_two();
        let buffer = create_uniform_buffer(&self.device, self.uniform_stride, capacity);
        {
            let Self { device, bind_layout, sampler, sources, texture_pool, .. } = &mut *self;
            for s in sources.values_mut().chain(texture_pool.iter_mut()) {
                s.bind_group = make_bind_group(device, bind_layout, sampler, &s.view, &buffer);
            }
        }
        self.uniform_buffer = buffer;
        self.uniform_capacity = capacity;
    }
}

// The result (including a failure) is cached: on machines where no adapter
// exists we report the error cheaply instead of re-probing the GPU every frame.
static GPU: OnceLock<Result<Mutex<RendererState>, String>> = OnceLock::new();

fn state() -> Result<&'static Mutex<RendererState>, String> {
    GPU.get_or_init(|| init_state().map(Mutex::new)).as_ref().map_err(|e| e.clone())
}

fn init_state() -> Result<RendererState, String> {
    pollster::block_on(async {
        let instance = wgpu::Instance::default();
        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }).await.ok_or_else(|| "wgpu could not find a usable GPU adapter".to_string())?;
        let info = adapter.get_info();
        eprintln!("wgpu adapter: {} | backend: {:?} | type: {:?} | driver: {}",
            info.name, info.backend, info.device_type, info.driver);
        let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("Hanabi preview device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
        }, None).await.map_err(|e| format!("wgpu device creation failed: {e}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Hanabi real-time video compositor shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Hanabi preview bind group layout"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Hanabi preview pipeline layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Hanabi GPU preview compositor"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: "vs_main", buffers: &[] },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Hanabi preview linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // Dynamic uniform offsets must be multiples of the device alignment.
        let align = (device.limits().min_uniform_buffer_offset_alignment as usize).max(1);
        let uniform_stride = size_of::<LayerUniforms>().div_ceil(align) * align;
        let uniform_buffer = create_uniform_buffer(&device, uniform_stride, INITIAL_LAYER_CAPACITY);

        Ok::<_, String>(RendererState {
            device: Arc::new(device),
            queue: Arc::new(queue),
            pipeline,
            bind_layout,
            sampler,
            sources: HashMap::new(),
            texture_pool: Vec::new(),
            target: None,
            last: LastFrame {
                sig: FrameSig { tw: 0, th: 0, layers: Vec::new(), uniforms: Vec::new() },
                sig_valid: false,
                output: Vec::new(),
                output_valid: false,
            },
            uniform_buffer,
            uniform_capacity: INITIAL_LAYER_CAPACITY,
            uniform_stride,
            uniform_scratch: Vec::new(),
            tick: 0,
        })
    })
}

pub struct GpuLayerFrame<'a> {
    /// Layer identity. No longer needed for GPU resources (uniforms are slotted
    /// by draw order), kept so existing callers keep compiling.
    pub key: &'a str,
    /// Identity of the decoded source frame, shared by duplicate clip layers.
    pub source_key: &'a str,
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub uniforms: LayerUniforms,
}

pub struct GpuPreviewRenderer;
impl GpuPreviewRenderer {
    pub fn composite_rgba(layers: &[GpuLayerFrame<'_>], tw: u32, th: u32) -> Result<Vec<u8>, String> {
        let mut output = Vec::new();
        Self::composite_rgba_into(layers, tw, th, &mut output)?;
        Ok(output)
    }

    /// Same as `composite_rgba`, but writes into a caller-owned buffer so the
    /// per-frame allocation (8 MB at 1080p) can be reused. `out` is replaced
    /// with exactly `tw * th * 4` RGBA bytes on success.
    pub fn composite_rgba_into(layers: &[GpuLayerFrame<'_>], tw: u32, th: u32, out: &mut Vec<u8>) -> Result<(), String> {
        if tw == 0 || th == 0 { return Err("GPU preview dimensions must be nonzero".into()); }
        for layer in layers {
            if layer.width == 0 || layer.height == 0 { return Err("GPU layer dimensions must be nonzero".into()); }
            let expected = layer.width as usize * layer.height as usize * 4;
            if layer.pixels.len() != expected {
                return Err(format!("Decoded layer is {} bytes; expected {expected}", layer.pixels.len()));
            }
        }
        let mut guard = state()?.lock().map_err(|_| "wgpu renderer lock failed")?;
        // Reborrow once so disjoint fields can be borrowed independently.
        let r = &mut *guard;

        // Return an error instead of a wgpu validation panic on oversized input.
        let max_dim = r.device.limits().max_texture_dimension_2d;
        if tw > max_dim || th > max_dim || layers.iter().any(|l| l.width > max_dim || l.height > max_dim) {
            return Err(format!("GPU texture dimension exceeds device limit of {max_dim}"));
        }

        // Pack every layer's uniforms at its dynamic-offset slot. Padding bytes
        // stay zero, so this buffer doubles as the uniform part of the frame
        // signature.
        r.ensure_uniform_capacity(layers.len());
        let stride = r.uniform_stride;
        let ubytes = size_of::<LayerUniforms>();
        r.uniform_scratch.clear();
        r.uniform_scratch.resize(stride * layers.len(), 0);
        for (i, layer) in layers.iter().enumerate() {
            let u = layer.uniforms.with_sizes(layer.width, layer.height, tw, th);
            r.uniform_scratch[i * stride..i * stride + ubytes].copy_from_slice(bytemuck::bytes_of(&u));
        }

        // Identical inputs to the previous frame (paused playhead, held frame):
        // the pixels are guaranteed identical, so skip the GPU round trip.
        let same_sig = r.last.sig_valid && r.last.sig.matches(layers, &r.uniform_scratch, tw, th);
        if same_sig && r.last.output_valid {
            out.clear();
            out.extend_from_slice(&r.last.output);
            return Ok(());
        }

        if r.target.as_ref().is_none_or(|t| t.width != tw || t.height != th) {
            let texture = r.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Hanabi persistent preview target"),
                size: wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let padded_row = (tw * 4 + align - 1) & !(align - 1);
            let readback = r.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Hanabi persistent preview readback"),
                size: padded_row as u64 * th as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            r.target = Some(TargetResources { width: tw, height: th, padded_row, texture, view, readback });
        }

        // Upload layer uniforms: one write for the whole frame.
        if !r.uniform_scratch.is_empty() {
            r.queue.write_buffer(&r.uniform_buffer, 0, &r.uniform_scratch);
        }

        r.tick += 1;
        let tick = r.tick;
        let mut active_sources = 0usize;

        for layer in layers {
            // A source-frame texture is uploaded once, regardless of how many
            // timeline clips reference it. An existing key is already populated
            // with the same RGBA (dimensions are checked defensively).
            let cached = match r.sources.get_mut(layer.source_key) {
                Some(s) if s.width == layer.width && s.height == layer.height => {
                    if s.last_used != tick {
                        s.last_used = tick;
                        active_sources += 1;
                    }
                    true
                }
                _ => false,
            };
            if cached { continue; }

            // Prefer recycling an evicted texture of the same size: avoids a
            // driver allocation per frame. write_texture overwrites every
            // texel, so the result is identical to a fresh texture. Its bind
            // group stays valid too (same view, same shared uniform buffer).
            let pooled_index = r.texture_pool.iter()
                .position(|s| s.width == layer.width && s.height == layer.height);
            let mut source = match pooled_index {
                Some(index) => r.texture_pool.swap_remove(index),
                None => r.create_source(layer.width, layer.height),
            };
            source.last_used = tick;
            r.queue.write_texture(
                wgpu::ImageCopyTexture { texture: &source.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                layer.pixels,
                wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(layer.width * 4), rows_per_image: Some(layer.height) },
                wgpu::Extent3d { width: layer.width, height: layer.height, depth_or_array_layers: 1 },
            );
            r.sources.insert(layer.source_key.to_owned(), source);
            active_sources += 1;
        }

        // Evict the least-recently-used inactive sources beyond the working set
        // (previously the victims were arbitrary HashMap order, which could
        // drop the newest frame and keep a stale one). Never evicts a source
        // used by this composite.
        while r.sources.len() > active_sources + SOURCE_CACHE_EXTRA {
            let oldest = r.sources.iter()
                .filter(|(_, s)| s.last_used != tick)
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| k.clone());
            let Some(oldest) = oldest else { break };
            if let Some(source) = r.sources.remove(&oldest) {
                if r.texture_pool.len() < TEXTURE_POOL_MAX {
                    r.texture_pool.push(source);
                }
            }
        }

        let target = r.target.as_ref().expect("persistent target initialized");
        let mut encoder = r.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Hanabi persistent render encoder") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Hanabi GPU composite pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&r.pipeline);
            for (i, layer) in layers.iter().enumerate() {
                if let Some(source) = r.sources.get(layer.source_key) {
                    pass.set_bind_group(0, &source.bind_group, &[(i * stride) as u32]);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::ImageCopyBuffer { buffer: &target.readback, layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(target.padded_row), rows_per_image: Some(th) } },
            wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
        );
        r.queue.submit(Some(encoder.finish()));

        // Readback is still required by the current Raylib/OpenGL preview surface.
        // It runs on the worker thread, never the UI thread; target and staging buffer persist.
        let slice = target.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| { let _ = tx.send(result); });
        r.device.poll(wgpu::Maintain::Wait);
        rx.recv().map_err(|e| format!("GPU readback callback failed: {e}"))?
            .map_err(|e| format!("GPU readback mapping failed: {e}"))?;
        let mapped = slice.get_mapped_range();
        let row_bytes = tw as usize * 4;
        let padded_row = target.padded_row as usize;
        let total = row_bytes * th as usize;
        out.clear();
        out.reserve(total);
        if padded_row == row_bytes {
            // No row padding (width * 4 already aligned): one contiguous copy.
            out.extend_from_slice(&mapped[..total]);
        } else {
            for y in 0..th as usize {
                out.extend_from_slice(&mapped[y * padded_row..y * padded_row + row_bytes]);
            }
        }
        drop(mapped);
        target.readback.unmap();

        if same_sig {
            // Second request for identical inputs: remember the pixels so any
            // further repeats are served without touching the GPU. Reuses the
            // cached Vec's allocation.
            r.last.output.clear();
            r.last.output.extend_from_slice(out);
            r.last.output_valid = true;
        } else {
            r.last.sig.update(layers, &r.uniform_scratch, tw, th);
            r.last.sig_valid = true;
            r.last.output_valid = false;
        }
        Ok(())
    }
}
