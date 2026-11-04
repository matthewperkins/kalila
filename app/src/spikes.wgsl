// Spike waveform persistence display: each stored waveform is an instance; each segment
// of the waveform is a screen-space thick-line quad (6 vertices). Alpha decays with age.

struct U {
    vp: vec2<f32>,
    cell: vec2<f32>,
    grid_cols: u32,
    first_ch: u32,
    nslots: u32,
    len: u32,
    uv_range: f32,
    now: f32,
    tau: f32,
    thickness: f32,
};

@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var<storage, read> wave: array<f32>;   // [ch][slot][len]
@group(0) @binding(2) var<storage, read> stamp: array<f32>;  // [ch][slot], <0 = empty
@group(0) @binding(3) var<storage, read> colors: array<vec4<f32>>;

struct O {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) inst: u32) -> O {
    var o: O;
    o.pos = vec4<f32>(-2.0, -2.0, 0.0, 1.0);
    o.color = vec4<f32>(0.0);
    let cell_i = inst / u.nslots;
    let slot = inst % u.nslots;
    let ch = u.first_ch + cell_i;
    let st = stamp[ch * u.nslots + slot];
    if (st < 0.0) {
        return o;
    }
    let age = u.now - st;
    let a = exp(-age / u.tau);
    if (a < 0.02) {
        return o;
    }
    let seg = vi / 6u;
    let corner = vi % 6u;
    let base = (ch * u.nslots + slot) * u.len;
    let origin = vec2<f32>(f32(cell_i % u.grid_cols) * u.cell.x, f32(cell_i / u.grid_cols) * u.cell.y);
    let sx = (u.cell.x - 8.0) / f32(u.len - 1u);
    let k = (u.cell.y * 0.5 - 4.0) / u.uv_range;
    let p0 = origin + vec2<f32>(4.0 + f32(seg) * sx, u.cell.y * 0.5 - wave[base + seg] * k);
    let p1 = origin + vec2<f32>(4.0 + f32(seg + 1u) * sx, u.cell.y * 0.5 - wave[base + seg + 1u] * k);
    let dir = normalize(p1 - p0 + vec2<f32>(1e-6, 0.0));
    let n = vec2<f32>(-dir.y, dir.x) * (0.5 * u.thickness);
    var p = p0;
    switch corner {
        case 0u: { p = p0 + n; }
        case 1u: { p = p1 + n; }
        case 2u: { p = p0 - n; }
        case 3u: { p = p1 + n; }
        case 4u: { p = p1 - n; }
        default: { p = p0 - n; }
    }
    // Clip to the cell vertically.
    p.y = clamp(p.y, origin.y, origin.y + u.cell.y);
    o.pos = vec4<f32>(p.x / u.vp.x * 2.0 - 1.0, 1.0 - p.y / u.vp.y * 2.0, 0.0, 1.0);
    let c = colors[ch];
    o.color = vec4<f32>(c.rgb, c.a * a);
    return o;
}

@fragment
fn fs(i: O) -> @location(0) vec4<f32> {
    return i.color;
}
