//! GPU spike-waveform persistence renderer.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use engine::dsp::{SPIKE_LEN, Spike};
use std::sync::Arc;

pub const SLOTS: usize = 48;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct U {
    vp: [f32; 2],
    cell: [f32; 2],
    grid_cols: u32,
    first_ch: u32,
    nslots: u32,
    len: u32,
    uv_range: f32,
    now: f32,
    tau: f32,
    thickness: f32,
}

pub struct SpikeFrame {
    pub nch: usize,
    pub reset: bool,
    /// New spikes this frame with their slot and time stamp.
    pub new: Vec<(usize, usize, f32, Arc<Spike>)>,
    pub vp_px: [f32; 2],
    pub cell_px: [f32; 2],
    pub grid_cols: u32,
    pub first_ch: u32,
    pub ncells: u32,
    pub uv_range: f32,
    pub now: f32,
    pub tau: f32,
    pub thickness: f32,
    pub colors: Option<Arc<Vec<[f32; 4]>>>,
}

pub struct SpikeGpu {
    pipe: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    ubuf: wgpu::Buffer,
    res: Option<(usize, wgpu::Buffer, wgpu::Buffer, wgpu::Buffer, wgpu::BindGroup)>,
    draw_instances: u32,
}

fn entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::VERTEX, ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None }, count: None }
}

impl SpikeGpu {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("spikes"), source: wgpu::ShaderSource::Wgsl(include_str!("spikes.wgsl").into()) });
        let ro = wgpu::BufferBindingType::Storage { read_only: true };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("spikes"), entries: &[entry(0, wgpu::BufferBindingType::Uniform), entry(1, ro), entry(2, ro), entry(3, ro)] });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&bgl)], immediate_size: 0 });
        let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("spikes"),
            layout: Some(&layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                // Additive-ish blending gives an oscilloscope persistence look.
                targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("spikes-u"), size: std::mem::size_of::<U>() as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Self { pipe, bgl, ubuf, res: None, draw_instances: 0 }
    }
}

pub struct SpikeCallback(pub SpikeFrame);

impl egui_wgpu::CallbackTrait for SpikeCallback {
    fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue, _sd: &egui_wgpu::ScreenDescriptor, _enc: &mut wgpu::CommandEncoder, res: &mut egui_wgpu::CallbackResources) -> Vec<wgpu::CommandBuffer> {
        let Some(g) = res.get_mut::<SpikeGpu>() else { return vec![] };
        let f = &self.0;
        if f.nch == 0 {
            g.draw_instances = 0;
            return vec![];
        }
        if f.reset || g.res.as_ref().is_none_or(|r| r.0 != f.nch) {
            let st = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
            let mk = |label, size: u64| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage: st, mapped_at_creation: false });
            let wave = mk("wave", (f.nch * SLOTS * SPIKE_LEN * 4) as u64);
            let stamp = mk("stamp", (f.nch * SLOTS * 4) as u64);
            let colors = mk("spike-colors", (f.nch * 16) as u64);
            queue.write_buffer(&stamp, 0, bytemuck::cast_slice(&vec![-1.0f32; f.nch * SLOTS]));
            queue.write_buffer(&colors, 0, bytemuck::cast_slice(&vec![[0.9f32, 0.9, 0.9, 0.6]; f.nch]));
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &g.bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: g.ubuf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wave.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: stamp.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: colors.as_entire_binding() },
                ],
            });
            g.res = Some((f.nch, wave, stamp, colors, bg));
        }
        let (_, wave, stamp, colors, _) = g.res.as_ref().unwrap();
        if let Some(c) = &f.colors {
            queue.write_buffer(colors, 0, bytemuck::cast_slice(&c[..c.len().min(f.nch)]));
        }
        for (ch, slot, t, sp) in &f.new {
            if *ch >= f.nch || sp.waveform.len() != SPIKE_LEN {
                continue;
            }
            let i = ch * SLOTS + slot;
            queue.write_buffer(wave, (i * SPIKE_LEN * 4) as u64, bytemuck::cast_slice(&sp.waveform));
            queue.write_buffer(stamp, (i * 4) as u64, bytemuck::bytes_of(t));
        }
        let u = U { vp: f.vp_px, cell: f.cell_px, grid_cols: f.grid_cols.max(1), first_ch: f.first_ch, nslots: SLOTS as u32, len: SPIKE_LEN as u32, uv_range: f.uv_range.max(1.0), now: f.now, tau: f.tau, thickness: f.thickness };
        queue.write_buffer(&g.ubuf, 0, bytemuck::bytes_of(&u));
        g.draw_instances = f.ncells.min(f.nch as u32 - f.first_ch.min(f.nch as u32)) * SLOTS as u32;
        vec![]
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, rp: &mut wgpu::RenderPass<'static>, res: &egui_wgpu::CallbackResources) {
        let Some(g) = res.get::<SpikeGpu>() else { return };
        let Some((_, _, _, _, bg)) = &g.res else { return };
        if g.draw_instances == 0 {
            return;
        }
        rp.set_pipeline(&g.pipe);
        rp.set_bind_group(0, bg, &[]);
        rp.draw(0..(SPIKE_LEN as u32 - 1) * 6, 0..g.draw_instances);
    }
}
