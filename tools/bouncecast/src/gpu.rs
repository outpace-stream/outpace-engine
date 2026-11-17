//! GPU renderer (feature `gpu`): the same scene as `render.rs`, drawn by a wgpu compute shader
//! (`scene.wgsl`) that writes NV12 directly. The palette and logo sprite are uploaded once; each
//! frame uploads 112 bytes of parameters plus the life grid, dispatches one pass and reads back
//! the finished frame. The CPU does no per-pixel work.

use crate::life::Life;
use crate::logo::Sprite;
use crate::render::{cell_gap, palette, Fractal, Overlay};
use std::sync::mpsc;

const SHADER: &str = include_str!("scene.wgsl");
const PARAMS_BYTES: u64 = 7 * 16;

pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params: wgpu::Buffer,
    cells: wgpu::Buffer,
    output: wgpu::Buffer,
    staging: wgpu::Buffer,
    width: u32,
    height: u32,
    cell: u32,
    cols: u32,
    rows: u32,
    sprite_stride: u32,
    sprite_rows: u32,
    sprite_margin: u32,
    cell_scratch: Vec<u8>,
    pub adapter: String,
}

impl GpuRenderer {
    /// `width` must be a multiple of 4 and `height` even.
    pub fn new(width: u32, height: u32, sprite: &Sprite, life: &Life) -> Result<Self, String> {
        if !width.is_multiple_of(4) || !height.is_multiple_of(2) {
            return Err("the GPU renderer needs a width divisible by 4 and an even height".into());
        }
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| format!("no Vulkan GPU available: {e}"))?;
        let info = adapter.get_info();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| format!("GPU device: {e}"))?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("scene"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        let storage = |label, data: &[u8]| {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: data.len() as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&buffer, 0, data);
            buffer
        };
        let lut: Vec<u8> = palette()
            .iter()
            .flat_map(|c| f32s([c[0], c[1], c[2], 0.0]))
            .collect();
        let sprite_px: Vec<u8> = (0..sprite.alpha.len())
            .flat_map(|i| f32s([sprite.alpha[i], sprite.tone[i], sprite.shadow[i], 0.0]))
            .collect();
        let lut = storage("palette", &lut);
        let sprite_buf = storage("sprite", &sprite_px);
        let cells = storage("cells", &vec![0u8; life.cols * life.rows * 16]);
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: PARAMS_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame_bytes = crate::render::frame_len(width, height) as u64;
        let output = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12"),
            size: frame_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: frame_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                entry(0, &params),
                entry(1, &lut),
                entry(2, &sprite_buf),
                entry(3, &cells),
                entry(4, &output),
            ],
        });
        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group,
            params,
            cells,
            output,
            staging,
            width,
            height,
            cell: life.cell as u32,
            cols: life.cols as u32,
            rows: life.rows as u32,
            sprite_stride: sprite.stride,
            sprite_rows: sprite.rows,
            sprite_margin: sprite.margin,
            cell_scratch: Vec::with_capacity(life.cols * life.rows * 16),
            adapter: format!("{} ({:?})", info.name, info.backend),
        })
    }

    /// Renders one NV12 frame into `out` (`frame_len` bytes).
    pub fn render(&mut self, t: f32, ov: &Overlay, out: &mut [u8]) -> Result<(), String> {
        let f = Fractal::at(t);
        let scale = f.half_height / (self.height as f32 / 2.0);
        let (sin, cos) = (f.rotation.sin() * scale, f.rotation.cos() * scale);
        let sprite_x = ov.x.round() - self.sprite_margin as f32;
        let sprite_y = ov.y.round() - self.sprite_margin as f32;
        let (pr, sc) = (ov.tint.primary, ov.tint.secondary);
        let mut params = Vec::with_capacity(PARAMS_BYTES as usize);
        params.extend(u32s([
            self.width,
            self.height,
            self.cell,
            cell_gap(self.cell as usize) as u32,
        ]));
        params.extend(u32s([
            self.cols,
            self.rows,
            self.sprite_stride,
            self.sprite_rows,
        ]));
        params.extend(f32s([sprite_x, sprite_y, ov.flash, 0.0]));
        params.extend(f32s([f.cx, f.cy, f.phase, 0.0]));
        params.extend(f32s([sin, cos, 0.0, 0.0]));
        params.extend(f32s([pr[0], pr[1], pr[2], 0.0]));
        params.extend(f32s([sc[0], sc[1], sc[2], 0.0]));
        self.queue.write_buffer(&self.params, 0, &params);

        self.cell_scratch.clear();
        let life = ov.life;
        for (c, g) in life.color.iter().zip(&life.glow) {
            self.cell_scratch.extend(f32s([c[0], c[1], c[2], *g]));
        }
        self.queue.write_buffer(&self.cells, 0, &self.cell_scratch);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("scene"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(
                (self.width / 4).div_ceil(8),
                (self.height / 2).div_ceil(8),
                1,
            );
        }
        encoder.copy_buffer_to_buffer(&self.output, 0, &self.staging, 0, self.output.size());
        self.queue.submit([encoder.finish()]);

        let (tx, rx) = mpsc::channel();
        self.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("GPU poll: {e}"))?;
        rx.recv()
            .map_err(|_| "GPU readback dropped".to_string())?
            .map_err(|e| format!("GPU readback: {e}"))?;
        {
            let view = self
                .staging
                .slice(..)
                .get_mapped_range()
                .map_err(|e| format!("GPU readback: {e}"))?;
            out.copy_from_slice(&view);
        }
        self.staging.unmap();
        Ok(())
    }
}

fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn f32s<const N: usize>(v: [f32; N]) -> impl Iterator<Item = u8> {
    v.into_iter().flat_map(f32::to_le_bytes)
}

fn u32s<const N: usize>(v: [u32; N]) -> impl Iterator<Item = u8> {
    v.into_iter().flat_map(u32::to_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounce::BRAND;
    use crate::render::{frame_len, Renderer};

    /// The GPU frame must show the same scene as the CPU frame. The CPU draws the fractal at
    /// half resolution, so compare mean luma error rather than exact bytes.
    #[test]
    fn gpu_frame_matches_cpu_frame() {
        let (w, h) = (640u32, 360u32);
        let sprite = Sprite::new(80).unwrap();
        let life = Life::new(w, h, 60, 9, BRAND.primary);
        let mut gpu = match GpuRenderer::new(w, h, &sprite, &life) {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("skipping gpu_frame_matches_cpu_frame: {e}");
                return;
            }
        };
        let overlay = Overlay {
            life: &life,
            sprite: &sprite,
            x: 200.0,
            y: 120.0,
            tint: BRAND,
            flash: 0.0,
        };
        let mut nv12 = vec![0; frame_len(w, h)];
        let mut i420 = vec![0; frame_len(w, h)];
        gpu.render(42.0, &overlay, &mut nv12).unwrap();
        Renderer::new(w, h, life.cell, 2).render(42.0, &overlay, &mut i420);

        let luma = (w * h) as usize;
        let mad = |a: &[u8], b: &[u8]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| x.abs_diff(*y) as f64)
                .sum::<f64>()
                / a.len() as f64
        };
        let y_err = mad(&nv12[..luma], &i420[..luma]);
        // De-interleave NV12 chroma to compare with planar I420.
        let (u, v): (Vec<u8>, Vec<u8>) = nv12[luma..].chunks(2).map(|p| (p[0], p[1])).unzip();
        let q = luma / 4;
        let c_err = (mad(&u, &i420[luma..luma + q]) + mad(&v, &i420[luma + q..])) / 2.0;
        assert!(y_err < 4.0, "luma differs by {y_err:.2} on average");
        assert!(c_err < 3.0, "chroma differs by {c_err:.2} on average");
        // The logo box carries the brand orange in both.
        let at = |f: &[u8], x: usize, y: usize| f[y * w as usize + x];
        let inside = (120..120 + sprite.height as usize)
            .flat_map(|y| (200..200 + sprite.width as usize).map(move |x| (x, y)));
        assert!(inside.clone().any(|(x, y)| at(&nv12, x, y) > 120));
    }
}
