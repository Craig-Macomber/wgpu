extern crate wgpu_hal as hal;

use hal::{Adapter as _, CommandBuffer as _, Device as _, Instance as _, Queue as _, Surface as _};

use std::{borrow::Borrow, iter, mem, num::NonZeroU32, ptr, time::Instant};

const MAX_BUNNIES: usize = 1 << 20;
const BUNNY_SIZE: f32 = 0.15 * 256.0;
const GRAVITY: f32 = -9.8 * 100.0;
const MAX_VELOCITY: f32 = 750.0;

#[repr(C)]
#[derive(Clone, Copy)]
struct Globals {
    mvp: [[f32; 4]; 4],
    size: [f32; 2],
    pad: [f32; 2],
}

#[repr(C, align(256))]
#[derive(Clone, Copy)]
struct Locals {
    position: [f32; 2],
    velocity: [f32; 2],
    color: u32,
    _pad: u32,
}

struct Example<A: hal::Api> {
    instance: A::Instance,
    surface: A::Surface,
    surface_format: wgt::TextureFormat,
    device: A::Device,
    queue: A::Queue,
    global_group: A::BindGroup,
    local_group: A::BindGroup,
    pipeline_layout: A::PipelineLayout,
    pipeline: A::RenderPipeline,
    bunnies: Vec<Locals>,
    local_buffer: A::Buffer,
    extent: [u32; 2],
    start: Instant,
}

impl<A: hal::Api> Example<A> {
    fn init(window: &winit::window::Window) -> Result<Self, hal::InstanceError> {
        let instance = unsafe { A::Instance::init()? };
        let mut surface = unsafe { instance.create_surface(window).unwrap() };
        let hal::OpenDevice { device, mut queue } = unsafe {
            let adapters = instance.enumerate_adapters();
            let exposed = &adapters[0];
            println!(
                "Surface caps: {:?}",
                exposed.adapter.surface_capabilities(&surface)
            );
            exposed.adapter.open(wgt::Features::empty()).unwrap()
        };

        let window_size: (u32, u32) = window.inner_size().into();
        let surface_config = hal::SurfaceConfiguration {
            swap_chain_size: 2,
            present_mode: wgt::PresentMode::Fifo,
            composite_alpha_mode: hal::CompositeAlphaMode::Opaque,
            format: wgt::TextureFormat::Rgba8UnormSrgb,
            extent: wgt::Extent3d {
                width: window_size.0,
                height: window_size.1,
                depth_or_array_layers: 1,
            },
            usage: hal::TextureUse::COLOR_TARGET,
        };
        unsafe {
            surface.configure(&device, &surface_config).unwrap();
        };

        let naga_shader = {
            let shader_file = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples")
                .join("bunnymark")
                .join("shader.wgsl");
            let source = std::fs::read_to_string(shader_file).unwrap();
            let module = naga::front::wgsl::Parser::new().parse(&source).unwrap();
            let info = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::empty(),
            )
            .validate(&module)
            .unwrap();
            hal::NagaShader { module, info }
        };
        let shader_desc = hal::ShaderModuleDescriptor { label: None };
        let shader = unsafe {
            match device.create_shader_module(&shader_desc, naga_shader) {
                Ok(shader) => shader,
                Err((error, _shader)) => panic!("{}", error),
            }
        };

        let global_bgl_desc = hal::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgt::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgt::ShaderStage::VERTEX,
                    ty: wgt::BindingType::Buffer {
                        ty: wgt::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgt::BufferSize::new(mem::size_of::<Globals>() as _),
                    },
                    count: None,
                },
                wgt::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgt::ShaderStage::FRAGMENT,
                    ty: wgt::BindingType::Texture {
                        sample_type: wgt::TextureSampleType::Float { filterable: true },
                        view_dimension: wgt::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgt::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgt::ShaderStage::FRAGMENT,
                    ty: wgt::BindingType::Sampler {
                        filtering: true,
                        comparison: false,
                    },
                    count: None,
                },
            ],
        };

        let global_bind_group_layout =
            unsafe { device.create_bind_group_layout(&global_bgl_desc).unwrap() };

        let local_bgl_desc = hal::BindGroupLayoutDescriptor {
            entries: &[wgt::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgt::ShaderStage::VERTEX,
                ty: wgt::BindingType::Buffer {
                    ty: wgt::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgt::BufferSize::new(mem::size_of::<Locals>() as _),
                },
                count: None,
            }],
            label: None,
        };
        let local_bind_group_layout =
            unsafe { device.create_bind_group_layout(&local_bgl_desc).unwrap() };

        let pipeline_layout_desc = hal::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&global_bind_group_layout, &local_bind_group_layout],
            push_constant_ranges: &[],
        };
        let pipeline_layout = unsafe {
            device
                .create_pipeline_layout(&pipeline_layout_desc)
                .unwrap()
        };

        let pipeline_desc = hal::RenderPipelineDescriptor {
            label: None,
            layout: &pipeline_layout,
            vertex_stage: hal::ProgrammableStage {
                module: &shader,
                entry_point: "vs_main",
            },
            vertex_buffers: &[],
            fragment_stage: Some(hal::ProgrammableStage {
                module: &shader,
                entry_point: "fs_main",
            }),
            primitive: wgt::PrimitiveState {
                topology: wgt::PrimitiveTopology::TriangleStrip,
                ..wgt::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgt::MultisampleState::default(),
            color_targets: &[wgt::ColorTargetState {
                format: surface_config.format,
                blend: Some(wgt::BlendState::ALPHA_BLENDING),
                write_mask: wgt::ColorWrite::default(),
            }],
        };
        let pipeline = unsafe { device.create_render_pipeline(&pipeline_desc).unwrap() };

        let texture_data = vec![0xFFu8; 3];

        let staging_buffer_desc = hal::BufferDescriptor {
            label: Some("stage"),
            size: texture_data.len() as wgt::BufferAddress,
            usage: hal::BufferUse::MAP_WRITE | hal::BufferUse::COPY_SRC,
            memory_flags: hal::MemoryFlag::TRANSIENT,
        };
        let staging_buffer = unsafe { device.create_buffer(&staging_buffer_desc).unwrap() };
        unsafe {
            let _is_coherent = true; //TODO
            let ptr = device
                .map_buffer(&staging_buffer, 0..staging_buffer_desc.size)
                .unwrap();
            ptr::copy_nonoverlapping(texture_data.as_ptr(), ptr.as_ptr(), texture_data.len());
            device.unmap_buffer(&staging_buffer).unwrap();
        }

        let texture_desc = hal::TextureDescriptor {
            label: None,
            size: wgt::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: wgt::TextureFormat::Rgba8UnormSrgb,
            usage: hal::TextureUse::COPY_DST | hal::TextureUse::SAMPLED,
            memory_flags: hal::MemoryFlag::empty(),
        };
        let texture = unsafe { device.create_texture(&texture_desc).unwrap() };

        let init_cmd_desc = hal::CommandBufferDescriptor {
            label: Some("init"),
        };
        let mut init_cmd = unsafe { device.create_command_buffer(&init_cmd_desc).unwrap() };
        {
            let buffer_barrier = hal::BufferBarrier {
                buffer: &staging_buffer,
                usage: hal::BufferUse::empty()..hal::BufferUse::COPY_SRC,
            };
            let texture_barrier1 = hal::TextureBarrier {
                texture: &texture,
                range: wgt::ImageSubresourceRange::default(),
                usage: hal::TextureUse::UNINITIALIZED..hal::TextureUse::COPY_DST,
            };
            let texture_barrier2 = hal::TextureBarrier {
                texture: &texture,
                range: wgt::ImageSubresourceRange::default(),
                usage: hal::TextureUse::COPY_DST..hal::TextureUse::SAMPLED,
            };
            let copy = hal::BufferTextureCopy {
                buffer_layout: wgt::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: NonZeroU32::new(4),
                    rows_per_image: None,
                },
                texture_base: hal::TextureCopyBase {
                    origin: wgt::Origin3d::ZERO,
                    mip_level: 0,
                    aspect: hal::FormatAspect::COLOR,
                },
                size: texture_desc.size,
            };
            unsafe {
                init_cmd.transition_buffers(iter::once(buffer_barrier));
                init_cmd.transition_textures(iter::once(texture_barrier1));
                init_cmd.copy_buffer_to_texture(&staging_buffer, &texture, iter::once(copy));
                init_cmd.transition_textures(iter::once(texture_barrier2));
            }
        }

        let sampler_desc = hal::SamplerDescriptor {
            label: None,
            address_modes: [wgt::AddressMode::ClampToEdge; 3],
            mag_filter: wgt::FilterMode::Linear,
            min_filter: wgt::FilterMode::Nearest,
            mipmap_filter: wgt::FilterMode::Nearest,
            lod_clamp: None,
            compare: None,
            anisotropy_clamp: None,
            border_color: None,
        };
        let sampler = unsafe { device.create_sampler(&sampler_desc).unwrap() };

        let globals = Globals {
            // cgmath::ortho() projection
            mvp: [
                [2.0 / window_size.0 as f32, 0.0, 0.0, 0.0],
                [0.0, 2.0 / window_size.1 as f32, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [-1.0, -1.0, 0.0, 1.0],
            ],
            size: [BUNNY_SIZE; 2],
            pad: [0.0; 2],
        };

        let global_buffer_desc = hal::BufferDescriptor {
            label: Some("global"),
            size: mem::size_of::<Globals>() as wgt::BufferAddress,
            usage: hal::BufferUse::MAP_WRITE | hal::BufferUse::UNIFORM,
            memory_flags: hal::MemoryFlag::empty(),
        };
        let global_buffer = unsafe {
            let buffer = device.create_buffer(&global_buffer_desc).unwrap();
            let _is_coherent = true; //TODO
            let ptr = device
                .map_buffer(&buffer, 0..global_buffer_desc.size)
                .unwrap();
            ptr::copy_nonoverlapping(
                &globals as *const Globals as *const u8,
                ptr.as_ptr(),
                mem::size_of::<Globals>(),
            );
            device.unmap_buffer(&buffer).unwrap();
            buffer
        };

        let local_buffer_desc = hal::BufferDescriptor {
            label: Some("local"),
            size: (MAX_BUNNIES as wgt::BufferAddress) * wgt::BIND_BUFFER_ALIGNMENT,
            usage: hal::BufferUse::MAP_WRITE | hal::BufferUse::UNIFORM,
            memory_flags: hal::MemoryFlag::empty(),
        };
        let local_buffer = unsafe { device.create_buffer(&local_buffer_desc).unwrap() };

        let view_desc = hal::TextureViewDescriptor {
            label: None,
            format: texture_desc.format,
            dimension: wgt::TextureViewDimension::D2,
            range: wgt::ImageSubresourceRange::default(),
        };
        let view = unsafe { device.create_texture_view(&texture, &view_desc).unwrap() };

        let global_group = {
            let global_buffer_binding = hal::BufferBinding {
                buffer: &global_buffer,
                offset: 0,
                size: None,
            };
            let global_group_desc = hal::BindGroupDescriptor {
                label: Some("global"),
                layout: &global_bind_group_layout,
                entries: &[
                    hal::BindGroupEntry {
                        binding: 0,
                        resource: hal::BindingResource::Buffers(
                            iter::once(global_buffer_binding).collect(),
                        ),
                    },
                    hal::BindGroupEntry {
                        binding: 1,
                        resource: hal::BindingResource::TextureViews(
                            iter::once(&view).collect(),
                            hal::TextureUse::SAMPLED,
                        ),
                    },
                    hal::BindGroupEntry {
                        binding: 2,
                        resource: hal::BindingResource::Sampler(&sampler),
                    },
                ],
            };
            unsafe { device.create_bind_group(&global_group_desc).unwrap() }
        };

        let local_group = {
            let local_buffer_binding = hal::BufferBinding {
                buffer: &local_buffer,
                offset: 0,
                size: wgt::BufferSize::new(mem::size_of::<Locals>() as _),
            };
            let local_group_desc = hal::BindGroupDescriptor {
                label: Some("local"),
                layout: &local_bind_group_layout,
                entries: &[hal::BindGroupEntry {
                    binding: 0,
                    resource: hal::BindingResource::Buffers(
                        iter::once(local_buffer_binding).collect(),
                    ),
                }],
            };
            unsafe { device.create_bind_group(&local_group_desc).unwrap() }
        };

        unsafe {
            let fence = device.create_fence().unwrap();
            init_cmd.finish();
            queue
                .submit(iter::once(init_cmd), Some((&fence, 1)))
                .unwrap();
            device.wait(&fence, 1, !0).unwrap();
            device.destroy_fence(fence);
            device.destroy_buffer(staging_buffer);
        }

        Ok(Example {
            instance,
            surface,
            surface_format: surface_config.format,
            device,
            queue,
            pipeline_layout,
            pipeline,
            global_group,
            local_group,
            bunnies: Vec::new(),
            local_buffer,
            extent: [window_size.0, window_size.1],
            start: Instant::now(),
        })
    }

    fn update(&mut self, event: winit::event::WindowEvent) {
        if let winit::event::WindowEvent::KeyboardInput {
            input:
                winit::event::KeyboardInput {
                    virtual_keycode: Some(winit::event::VirtualKeyCode::Space),
                    state: winit::event::ElementState::Pressed,
                    ..
                },
            ..
        } = event
        {
            let spawn_count = 64 + self.bunnies.len() / 2;
            let elapsed = self.start.elapsed();
            let color = elapsed.as_nanos() as u32;
            println!(
                "Spawning {} bunnies, total at {}",
                spawn_count,
                self.bunnies.len() + spawn_count
            );
            for _ in 0..spawn_count {
                let random = (elapsed.as_nanos() & 0xFF) as f32 / 255.0;
                let speed = random * MAX_VELOCITY - (MAX_VELOCITY * 0.5);
                self.bunnies.push(Locals {
                    position: [0.0, 0.5 * (self.extent[1] as f32)],
                    velocity: [speed, 0.0],
                    color,
                    _pad: 0,
                });
            }
        }
    }

    fn render(&mut self) {
        let delta = 0.01;
        for bunny in self.bunnies.iter_mut() {
            bunny.position[0] += bunny.velocity[0] * delta;
            bunny.position[1] += bunny.velocity[1] * delta;
            bunny.velocity[1] += GRAVITY * delta;
            if (bunny.velocity[0] > 0.0
                && bunny.position[0] + 0.5 * BUNNY_SIZE > self.extent[0] as f32)
                || (bunny.velocity[0] < 0.0 && bunny.position[0] - 0.5 * BUNNY_SIZE < 0.0)
            {
                bunny.velocity[0] *= -1.0;
            }
            if bunny.velocity[1] < 0.0 && bunny.position[1] < 0.5 * BUNNY_SIZE {
                bunny.velocity[1] *= -1.0;
            }
        }

        unsafe {
            let _is_coherent = true; //TODO
            let size = self.bunnies.len() * wgt::BIND_BUFFER_ALIGNMENT as usize;
            let ptr = self
                .device
                .map_buffer(&self.local_buffer, 0..size as wgt::BufferAddress)
                .unwrap();
            ptr::copy_nonoverlapping(self.bunnies.as_ptr() as *const u8, ptr.as_ptr(), size);
            self.device.unmap_buffer(&self.local_buffer).unwrap();
        }

        let mut cmd_buf = unsafe {
            self.device
                .create_command_buffer(&hal::CommandBufferDescriptor {
                    label: Some("frame"),
                })
                .unwrap()
        };

        let surface_tex = unsafe { self.surface.acquire_texture(!0).unwrap().unwrap().texture };
        let surface_view_desc = hal::TextureViewDescriptor {
            label: None,
            format: self.surface_format,
            dimension: wgt::TextureViewDimension::D2,
            range: wgt::ImageSubresourceRange::default(),
        };
        let surface_tex_view = unsafe {
            self.device
                .create_texture_view(surface_tex.borrow(), &surface_view_desc)
                .unwrap()
        };
        let pass_desc = hal::RenderPassDescriptor {
            label: None,
            color_attachments: &[hal::ColorAttachment {
                target: hal::Attachment {
                    view: &surface_tex_view,
                    usage: hal::TextureUse::COLOR_TARGET,
                    boundary_usage: hal::TextureUse::UNINITIALIZED..hal::TextureUse::empty(),
                },
                resolve_target: None,
                ops: hal::AttachmentOp::STORE,
                clear_value: wgt::Color {
                    r: 0.1,
                    g: 0.2,
                    b: 0.3,
                    a: 1.0,
                },
            }],
            depth_stencil_attachment: None,
        };
        unsafe {
            cmd_buf.begin_render_pass(&pass_desc);
            cmd_buf.set_render_pipeline(&self.pipeline);
            cmd_buf.set_bind_group(&self.pipeline_layout, 0, &self.global_group, &[]);
        }

        for i in 0..self.bunnies.len() {
            let offset =
                (i as wgt::DynamicOffset) * (wgt::BIND_BUFFER_ALIGNMENT as wgt::DynamicOffset);
            unsafe {
                cmd_buf.set_bind_group(&self.pipeline_layout, 1, &self.local_group, &[offset]);
                cmd_buf.draw(0, 4, 0, 1);
            }
        }

        unsafe {
            cmd_buf.finish();
            self.queue.submit(iter::once(cmd_buf), None).unwrap();
            self.queue.present(&mut self.surface, surface_tex).unwrap();
        }
    }
}

fn main() {
    let event_loop = winit::event_loop::EventLoop::new();
    let window = winit::window::WindowBuilder::new()
        .with_title("hal-bunnymark")
        .build(&event_loop)
        .unwrap();

    #[cfg(feature = "metal")]
    let mut example = Example::init::<hal::api::Metal>(&window);
}
