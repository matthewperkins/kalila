//! GPU continuous-trace renderer (egui-wgpu paint callback).
//!
//! CPU work per frame: one memcpy of the new raw int16 samples into a wgpu staging write.
//! Filtering, decimation and geometry all happen on the GPU. In triggered mode the CPU only
//! adds trigger positions (ring indices); the GPU cuts the sweeps from its own ring of
//! filtered samples.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use std::sync::Arc;

pub const MAX_SECTIONS: usize = 4;
/// Ring of filtered samples per channel for triggered sweeps (≈1.1 s at 30 kHz).
pub const RING: u32 = 1 << 15;
/// Triggers cut per frame at most (uniform array size in traces.wgsl).
pub const MAX_CUTS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IngestU {
    nch: u32,
    n_new: u32,
    window: u32,
    cols: u32,
    pos0: u32,
    nsec: u32,
    ring0: u32,
    ring: u32,
    car: u32,
    _p: [u32; 3],
    coef: [[f32; 4]; 8],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CutU {
    nch: u32,
    cols: u32,
    ring: u32,
    span: u32,
    n_trig: u32,
    _p: [u32; 3],
    trig: [[u32; 4]; MAX_CUTS],
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
    kmax: u32,
    nslots: u32,
    newest: u32,
    filled: u32,
}

/// Geometry/config that requires reallocation or a reset when changed.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceConfig {
    pub nch: usize,
    pub cols: u32,
    pub window: u32,
    pub sections: Vec<[f32; 5]>, // b0 b1 b2 a1 a2
    /// Sweep slots; 0 = free-running (no ring, no sweep store).
    pub nslots: u32,
}

/// Triggered-mode state for one frame.
#[derive(Clone, Default)]
pub struct SweepFrame {
    /// Ring position of the first new sample.
    pub ring0: u32,
    /// Window length (pre + post) in samples.
    pub span: u32,
    /// New sweeps to cut this frame: (slot, ring index of window start).
    pub cuts: Vec<(u32, u32)>,
    pub newest: u32,
    pub filled: u32,
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
    /// Channels per row (instances per row).
    pub kmax: u32,
    pub uv_per_row: f32,
    pub row_h_px: f32,
    pub thickness_px: f32,
    pub colors: Option<Arc<Vec<[f32; 4]>>>,
    pub order: Option<Arc<Vec<u32>>>,
    /// Per-channel (gain, filter 0/1, units_per_row or 0 for global µV/row, center); see traces.wgsl.
    pub chinfo: Option<Arc<Vec<[f32; 4]>>>,
    /// Per-row (first index into `order`, channel count).
    pub rowtab: Option<Arc<Vec<[u32; 2]>>>,
    pub sweep: SweepFrame,
    /// Apply the display CAR (chinfo flag bits 1/2).
    pub car: bool,
}

pub struct TraceGpu {
    filt_pipe: wgpu::ComputePipeline,
    carmean_pipe: wgpu::ComputePipeline,
    fold_pipe: wgpu::ComputePipeline,
    cut_pipe: wgpu::ComputePipeline,
    render_pipe: wgpu::RenderPipeline,
    ingest_bgl: wgpu::BindGroupLayout,
    cut_bgl: wgpu::BindGroupLayout,
    render_bgl: wgpu::BindGroupLayout,
    ingest_u: wgpu::Buffer,
    cut_u: wgpu::Buffer,
    render_u: wgpu::Buffer,
    res: Option<Resources>,
    /// Last per-channel tables from the app, rewritten after a reallocation (which would
    /// otherwise silently reset them to defaults).
    last_colors: Option<Arc<Vec<[f32; 4]>>>,
    last_order: Option<Arc<Vec<u32>>>,
    last_chinfo: Option<Arc<Vec<[f32; 4]>>>,
    last_rowtab: Option<Arc<Vec<[u32; 2]>>>,
}

struct Resources {
    config: TraceConfig,
    raw: wgpu::Buffer,
    raw_cap: u64,
    state: wgpu::Buffer,
    cols: wgpu::Buffer,
    colors: wgpu::Buffer,
    order: wgpu::Buffer,
    chinfo: wgpu::Buffer,
    rowtab: wgpu::Buffer,
    ingest_bg: wgpu::BindGroup,
    cut_bg: wgpu::BindGroup,
    render_bg: wgpu::BindGroup,
    instances: u32,
    vertex_count: u32,
}

fn storage(binding: u32, read_only: bool, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: vis, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }, count: None }
}
fn uniform(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: vis, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }
}
fn entry(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: b.as_entire_binding() }
}

impl TraceGpu {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("traces"), source: wgpu::ShaderSource::Wgsl(include_str!("traces.wgsl").into()) });
        let c = wgpu::ShaderStages::COMPUTE;
        let v = wgpu::ShaderStages::VERTEX;
        let ingest_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ingest"),
            entries: &[uniform(0, c), storage(1, true, c), storage(2, false, c), storage(3, false, c), storage(4, true, c), storage(5, false, c), storage(6, false, c), storage(7, false, c)],
        });
        let cut_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("cut"), entries: &[uniform(0, c), storage(1, true, c), storage(2, false, c)] });
        let render_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trace-render"),
            entries: &[uniform(0, v), storage(1, true, v), storage(2, true, v), storage(3, true, v), storage(4, true, v), storage(5, true, v), storage(6, true, v)],
        });
        let layout = |bgl: &wgpu::BindGroupLayout| device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(bgl)], immediate_size: 0 });
        let compute = |bgl: &wgpu::BindGroupLayout, ep: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(ep), layout: Some(&layout(bgl)), module: &module, entry_point: Some(ep), compilation_options: Default::default(), cache: None })
        };
        let filt_pipe = compute(&ingest_bgl, "filt");
        let carmean_pipe = compute(&ingest_bgl, "carmean");
        let fold_pipe = compute(&ingest_bgl, "fold");
        let cut_pipe = compute(&cut_bgl, "cut");
        let render_pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("traces"),
            layout: Some(&layout(&render_bgl)),
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
            cut_u: mk_u(std::mem::size_of::<CutU>() as u64, "cut-u"),
            render_u: mk_u(std::mem::size_of::<RenderU>() as u64, "render-u"),
            filt_pipe,
            carmean_pipe,
            fold_pipe,
            cut_pipe,
            render_pipe,
            ingest_bgl,
            cut_bgl,
            render_bgl,
            res: None,
            last_colors: None,
            last_order: None,
            last_chinfo: None,
            last_rowtab: None,
        }
    }

    fn ensure(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &TraceFrame) {
        let need_raw = ((f.n_new as u64 * f.config.nch as u64 * 2 + 3) & !3).max(4);
        let realloc = match &self.res {
            None => true,
            Some(r) => r.config.nch != f.config.nch || r.config.cols != f.config.cols || r.config.nslots != f.config.nslots || r.raw_cap < need_raw,
        };
        let reset = realloc || f.reset || self.res.as_ref().is_some_and(|r| r.config != f.config);
        if realloc {
            let nch = f.config.nch.max(1) as u64;
            let cols = f.config.cols as u64;
            let raw_cap = need_raw.max(nch * 2 * 4096).next_power_of_two();
            let buf = |label, size: u64| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(16), usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            let trig = f.config.nslots > 0;
            let raw = buf("raw", raw_cap);
            let state = buf("state", nch * 12 * 4);
            let colbuf = buf("cols", nch * cols * 8);
            let colors = buf("colors", nch * 16);
            let order = buf("order", nch * 4);
            let chinfo = buf("chinfo", nch * 16);
            let rowtab = buf("rowtab", nch * 8);
            // Triggered mode only; 16-byte placeholders otherwise (bind groups need a buffer).
            let fring = buf("fring", if trig { nch * RING as u64 * 4 } else { 0 });
            let scols = buf("sweeps", if trig { f.config.nslots as u64 * nch * cols * 8 } else { 0 });
            // Filtered samples (f32, sized like `raw` holds i16) and the per-sample CAR.
            let fbuf = buf("filtered", raw_cap * 2);
            let refv = buf("car", raw_cap / (2 * nch) * 4 + 4);
            let ingest_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.ingest_bgl,
                entries: &[entry(0, &self.ingest_u), entry(1, &raw), entry(2, &state), entry(3, &colbuf), entry(4, &chinfo), entry(5, &fring), entry(6, &fbuf), entry(7, &refv)],
            });
            let cut_bg = device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &self.cut_bgl, entries: &[entry(0, &self.cut_u), entry(1, &fring), entry(2, &scols)] });
            let render_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.render_bgl,
                entries: &[entry(0, &self.render_u), entry(1, &colbuf), entry(2, &colors), entry(3, &order), entry(4, &chinfo), entry(5, &rowtab), entry(6, &scols)],
            });
            // Defaults until the app supplies its tables: grey, identity order, all electrodes,
            // one channel per row.
            let n = nch as usize;
            queue.write_buffer(&colors, 0, bytemuck::cast_slice(&vec![[0.8f32, 0.8, 0.8, 1.0]; n]));
            queue.write_buffer(&order, 0, bytemuck::cast_slice(&(0..n as u32).collect::<Vec<_>>()));
            queue.write_buffer(&chinfo, 0, bytemuck::cast_slice(&vec![[engine::rhythm::ELECTRODE_UV_PER_BIT, 1.0, 0.0, 0.0]; n]));
            queue.write_buffer(&rowtab, 0, bytemuck::cast_slice(&(0..n as u32).map(|i| [i, 1]).collect::<Vec<_>>()));
            // The ring and sweep buffers are owned by the bind groups.
            self.res = Some(Resources { config: f.config.clone(), raw, raw_cap, state, cols: colbuf, colors, order, chinfo, rowtab, ingest_bg, cut_bg, render_bg, instances: 0, vertex_count: 0 });
        }
        let r = self.res.as_mut().unwrap();
        if reset {
            r.config = f.config.clone();
            let nch = f.config.nch.max(1);
            let mut st = vec![0f32; nch * 12];
            for c in 0..nch {
                st[c * 12 + 10] = f32::from_bits(u32::MAX);
            }
            queue.write_buffer(&r.state, 0, bytemuck::cast_slice(&st));
            let empty: Vec<[f32; 2]> = vec![[1e30, -1e30]; nch * f.config.cols as usize];
            queue.write_buffer(&r.cols, 0, bytemuck::cast_slice(&empty));
            // Sweep slots are hidden via `filled` (the app restarts it at 0), not cleared.
        }
        macro_rules! table {
            ($field:ident, $last:ident) => {
                if f.$field.is_some() {
                    self.$last = f.$field.clone();
                }
                if let Some(t) = f.$field.as_ref().or(if realloc { self.$last.as_ref() } else { None }) {
                    queue.write_buffer(&r.$field, 0, bytemuck::cast_slice(&t[..t.len().min(f.config.nch)]));
                }
            };
        }
        table!(colors, last_colors);
        table!(order, last_order);
        table!(chinfo, last_chinfo);
        table!(rowtab, last_rowtab);
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
        let trig = f.config.nslots > 0;
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
            let u = IngestU { nch: f.config.nch as u32, n_new: f.n_new, window: f.config.window, cols: f.config.cols, pos0: f.pos0, nsec: nsec as u32, ring0: f.sweep.ring0, ring: if trig { RING } else { 0 }, car: f.car as u32, _p: [0; 3], coef };
            queue.write_buffer(&g.ingest_u, 0, bytemuck::bytes_of(&u));
            // Each dispatch is its own synchronisation scope: filt → carmean → fold.
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("ingest"), timestamp_writes: None });
            cp.set_bind_group(0, &r.ingest_bg, &[]);
            cp.set_pipeline(&g.filt_pipe);
            cp.dispatch_workgroups((f.config.nch as u32).div_ceil(64), 1, 1);
            if f.car {
                cp.set_pipeline(&g.carmean_pipe);
                cp.dispatch_workgroups(f.n_new.div_ceil(64), 1, 1);
            }
            cp.set_pipeline(&g.fold_pipe);
            cp.dispatch_workgroups((f.config.nch as u32).div_ceil(64), 1, 1);
        }
        if trig && !f.sweep.cuts.is_empty() && f.sweep.span > 0 {
            let mut t = [[0u32; 4]; MAX_CUTS];
            let n = f.sweep.cuts.len().min(MAX_CUTS);
            for (i, &(slot, start)) in f.sweep.cuts.iter().take(n).enumerate() {
                t[i] = [slot, start, 0, 0];
            }
            let u = CutU { nch: f.config.nch as u32, cols: f.config.cols, ring: RING, span: f.sweep.span, n_trig: n as u32, _p: [0; 3], trig: t };
            queue.write_buffer(&g.cut_u, 0, bytemuck::bytes_of(&u));
            // A separate pass orders the cut after this frame's ingest.
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("cut"), timestamp_writes: None });
            cp.set_pipeline(&g.cut_pipe);
            cp.set_bind_group(0, &r.cut_bg, &[]);
            cp.dispatch_workgroups((f.config.nch as u32).div_ceil(64), n as u32, 1);
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
            kmax: f.kmax.max(1),
            nslots: f.config.nslots,
            newest: f.sweep.newest,
            filled: f.sweep.filled,
        };
        queue.write_buffer(&g.render_u, 0, bytemuck::bytes_of(&ru));
        r.instances = f.nrows * f.kmax.max(1) * f.config.nslots.max(1);
        r.vertex_count = f.config.cols * 6;
        vec![]
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, rp: &mut wgpu::RenderPass<'static>, res: &egui_wgpu::CallbackResources) {
        let Some(g) = res.get::<TraceGpu>() else { return };
        let Some(r) = &g.res else { return };
        if r.instances == 0 {
            return;
        }
        rp.set_pipeline(&g.render_pipe);
        rp.set_bind_group(0, &r.render_bg, &[]);
        rp.draw(0..r.vertex_count, 0..r.instances);
    }
}
