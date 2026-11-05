//! GPU continuous-trace renderer (egui-wgpu paint callback).
//!
//! CPU work per frame: one memcpy of the new raw int16 samples into a wgpu staging write.
//! Filtering, decimation and geometry all happen on the GPU.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use std::sync::Arc;

pub const MAX_SECTIONS: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IngestU {
    nch: u32,
    n_new: u32,
    window: u32,
    cols: u32,
    pos0: u32,
    nsec: u32,
    _unused: f32,
    _pad: u32,
    coef: [[f32; 4]; 8],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RenderU {
    vp: [f32; 2],
    cols: u32,
    nrows: u32,
    first_row: u32,
    nch: u32,
    cursor: u32,
    gap: u32,
    uv_per_row: f32,
    row_h: f32,
    thickness: f32,
    _pad: f32,
}

/// Geometry/config that requires reallocation or a reset when changed.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceConfig {
    pub nch: usize,
    pub cols: u32,
    pub window: u32,
    pub sections: Vec<[f32; 5]>, // b0 b1 b2 a1 a2
}

/// Everything the GPU needs for one frame.
pub struct TraceFrame {
    pub config: TraceConfig,
    pub reset: bool,
    /// Raw interleaved int16 samples (n_new × nch).
    pub upload: Arc<Vec<i16>>,
    pub n_new: u32,
    /// Window position of the first new sample.
    pub pos0: u32,
    pub cursor_col: u32,
    pub first_row: u32,
    pub nrows: u32,
    pub uv_per_row: f32,
    pub row_h_px: f32,
    pub thickness_px: f32,
    pub colors: Option<Arc<Vec<[f32; 4]>>>,
    pub order: Option<Arc<Vec<u32>>>,
    /// Per-channel (gain, filter 0/1, units_per_row or 0 for global µV/row, center); see traces.wgsl.
    pub chinfo: Option<Arc<Vec<[f32; 4]>>>,
}

pub struct TraceGpu {
    ingest_pipe: wgpu::ComputePipeline,
    render_pipe: wgpu::RenderPipeline,
    ingest_bgl: wgpu::BindGroupLayout,
    render_bgl: wgpu::BindGroupLayout,
    ingest_u: wgpu::Buffer,
    render_u: wgpu::Buffer,
    /// Filter/column state, referenced by the ingest bind group; kept for resets.
    state: Option<wgpu::Buffer>,
    res: Option<Resources>,
    /// Last per-channel tables from the app, rewritten after a reallocation (which would
    /// otherwise silently reset them to defaults).
    last_colors: Option<Arc<Vec<[f32; 4]>>>,
    last_order: Option<Arc<Vec<u32>>>,
    last_chinfo: Option<Arc<Vec<[f32; 4]>>>,
}

struct Resources {
    config: TraceConfig,
    raw: wgpu::Buffer,
    raw_cap: u64,
    cols: wgpu::Buffer,
    colors: wgpu::Buffer,
    order: wgpu::Buffer,
    chinfo: wgpu::Buffer,
    ingest_bg: wgpu::BindGroup,
    render_bg: wgpu::BindGroup,
    nrows_drawn: u32,
    vertex_count: u32,
}

fn storage(binding: u32, read_only: bool, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: vis, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }, count: None }
}
fn uniform(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: vis, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }
}

impl TraceGpu {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("traces"), source: wgpu::ShaderSource::Wgsl(include_str!("traces.wgsl").into()) });
        let c = wgpu::ShaderStages::COMPUTE;
        let v = wgpu::ShaderStages::VERTEX;
        let ingest_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("ingest"), entries: &[uniform(0, c), storage(1, true, c), storage(2, false, c), storage(3, false, c), storage(4, true, c)] });
        let render_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("trace-render"), entries: &[uniform(0, v), storage(1, true, v), storage(2, true, v), storage(3, true, v), storage(4, true, v)] });
        let il = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&ingest_bgl)], immediate_size: 0 });
        let rl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&render_bgl)], immediate_size: 0 });
        let ingest_pipe = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("ingest"), layout: Some(&il), module: &module, entry_point: Some("ingest"), compilation_options: Default::default(), cache: None });
        let render_pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("traces"),
            layout: Some(&rl),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState { module: &module, entry_point: Some("fs"), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })] }),
            multiview_mask: None,
            cache: None,
        });
        let mk_u = |size: u64, label| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Self {
            ingest_u: mk_u(std::mem::size_of::<IngestU>() as u64, "ingest-u"),
            render_u: mk_u(std::mem::size_of::<RenderU>() as u64, "render-u"),
            ingest_pipe,
            render_pipe,
            ingest_bgl,
            render_bgl,
            state: None,
            res: None,
            last_colors: None,
            last_order: None,
            last_chinfo: None,
        }
    }

    fn ensure(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &TraceFrame) {
        let need_raw = ((f.n_new as u64 * f.config.nch as u64 * 2 + 3) & !3).max(4);
        let realloc = match &self.res {
            None => true,
            Some(r) => r.config.nch != f.config.nch || r.config.cols != f.config.cols || r.raw_cap < need_raw,
        };
        let reset = realloc || f.reset || self.res.as_ref().is_some_and(|r| r.config != f.config);
        if realloc {
            let nch = f.config.nch.max(1) as u64;
            let raw_cap = need_raw.max(nch * 2 * 4096).next_power_of_two();
            let buf = |label, size: u64, usage| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage, mapped_at_creation: false });
            let st = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
            let raw = buf("raw", raw_cap, st);
            let state = buf("state", nch * 12 * 4, st);
            let cols = buf("cols", nch * f.config.cols as u64 * 8, st);
            let colors = buf("colors", nch * 16, st);
            let order = buf("order", nch * 4, st);
            let chinfo = buf("chinfo", nch * 16, st);
            let ingest_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.ingest_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.ingest_u.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: raw.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: state.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: cols.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 4, resource: chinfo.as_entire_binding() },
                ],
            });
            let render_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.render_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.render_u.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: cols.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: colors.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: order.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 4, resource: chinfo.as_entire_binding() },
                ],
            });
            // Default colours/order until the app supplies them.
            let default_colors: Vec<[f32; 4]> = vec![[0.8, 0.8, 0.8, 1.0]; nch as usize];
            queue.write_buffer(&colors, 0, bytemuck::cast_slice(&default_colors));
            let default_order: Vec<u32> = (0..nch as u32).collect();
            queue.write_buffer(&order, 0, bytemuck::cast_slice(&default_order));
            // Default: everything is an electrode until the app supplies per-channel info.
            let default_info: Vec<[f32; 4]> = vec![[engine::rhythm::ELECTRODE_UV_PER_BIT, 1.0, 0.0, 0.0]; nch as usize];
            queue.write_buffer(&chinfo, 0, bytemuck::cast_slice(&default_info));
            self.res = Some(Resources { config: f.config.clone(), raw, raw_cap, cols, colors, order, chinfo, ingest_bg, render_bg, nrows_drawn: 0, vertex_count: 0 });
            // Keep `state` alive through the bind group; store the handle for resets.
            self.state = Some(state);
        }
        let r = self.res.as_mut().unwrap();
        if reset {
            r.config = f.config.clone();
            let nch = f.config.nch.max(1);
            let mut st = vec![0f32; nch * 12];
            for c in 0..nch {
                st[c * 12 + 10] = f32::from_bits(u32::MAX);
            }
            queue.write_buffer(self.state.as_ref().unwrap(), 0, bytemuck::cast_slice(&st));
            let empty: Vec<[f32; 2]> = vec![[1e30, -1e30]; nch * f.config.cols as usize];
            queue.write_buffer(&r.cols, 0, bytemuck::cast_slice(&empty));
        }
        if f.colors.is_some() {
            self.last_colors = f.colors.clone();
        }
        if f.order.is_some() {
            self.last_order = f.order.clone();
        }
        if f.chinfo.is_some() {
            self.last_chinfo = f.chinfo.clone();
        }
        let n = f.config.nch;
        if let Some(c) = f.colors.as_ref().or(if realloc { self.last_colors.as_ref() } else { None }) {
            queue.write_buffer(&r.colors, 0, bytemuck::cast_slice(&c[..c.len().min(n)]));
        }
        if let Some(o) = f.order.as_ref().or(if realloc { self.last_order.as_ref() } else { None }) {
            queue.write_buffer(&r.order, 0, bytemuck::cast_slice(&o[..o.len().min(n)]));
        }
        if let Some(c) = f.chinfo.as_ref().or(if realloc { self.last_chinfo.as_ref() } else { None }) {
            queue.write_buffer(&r.chinfo, 0, bytemuck::cast_slice(&c[..c.len().min(n)]));
        }
    }
}

pub struct TraceCallback(pub TraceFrame);

impl egui_wgpu::CallbackTrait for TraceCallback {
    fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue, _sd: &egui_wgpu::ScreenDescriptor, enc: &mut wgpu::CommandEncoder, res: &mut egui_wgpu::CallbackResources) -> Vec<wgpu::CommandBuffer> {
        let Some(g) = res.get_mut::<TraceGpu>() else { return vec![] };
        let f = &self.0;
        if f.config.nch == 0 || f.config.cols == 0 || f.config.window == 0 {
            return vec![];
        }
        g.ensure(device, queue, f);
        let r = g.res.as_mut().unwrap();
        if f.n_new > 0 {
            let bytes: &[u8] = bytemuck::cast_slice(&f.upload[..f.n_new as usize * f.config.nch]);
            // write_buffer requires a 4-byte multiple.
            if bytes.len() % 4 == 0 {
                queue.write_buffer(&r.raw, 0, bytes);
            } else {
                let mut v = bytes.to_vec();
                v.extend_from_slice(&[0, 0]);
                queue.write_buffer(&r.raw, 0, &v);
            }
            let mut coef = [[0f32; 4]; 8];
            let nsec = f.config.sections.len().min(MAX_SECTIONS);
            for (i, s) in f.config.sections.iter().take(nsec).enumerate() {
                coef[2 * i] = [s[0], s[1], s[2], 0.0];
                coef[2 * i + 1] = [s[3], s[4], 0.0, 0.0];
            }
            let u = IngestU { nch: f.config.nch as u32, n_new: f.n_new, window: f.config.window, cols: f.config.cols, pos0: f.pos0, nsec: nsec as u32, _unused: 0.0, _pad: 0, coef };
            queue.write_buffer(&g.ingest_u, 0, bytemuck::bytes_of(&u));
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("ingest"), timestamp_writes: None });
            cp.set_pipeline(&g.ingest_pipe);
            cp.set_bind_group(0, &r.ingest_bg, &[]);
            cp.dispatch_workgroups((f.config.nch as u32).div_ceil(64), 1, 1);
        }
        let ru = RenderU {
            // Callback viewport in physical pixels: width = one column per pixel.
            vp: [f.config.cols as f32, f.row_h_px * f.nrows as f32],
            cols: f.config.cols,
            nrows: f.nrows,
            first_row: f.first_row,
            nch: f.config.nch as u32,
            cursor: f.cursor_col,
            gap: (f.config.cols / 80).max(2),
            uv_per_row: f.uv_per_row,
            row_h: f.row_h_px,
            thickness: f.thickness_px,
            _pad: 0.0,
        };
        queue.write_buffer(&g.render_u, 0, bytemuck::bytes_of(&ru));
        r.nrows_drawn = f.nrows.min(f.config.nch as u32 - f.first_row.min(f.config.nch as u32));
        r.vertex_count = f.config.cols * 6;
        vec![]
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, rp: &mut wgpu::RenderPass<'static>, res: &egui_wgpu::CallbackResources) {
        let Some(g) = res.get::<TraceGpu>() else { return };
        let Some(r) = &g.res else { return };
        if r.nrows_drawn == 0 {
            return;
        }
        rp.set_pipeline(&g.render_pipe);
        rp.set_bind_group(0, &r.render_bg, &[]);
        rp.draw(0..r.vertex_count, 0..r.nrows_drawn);
    }
}
