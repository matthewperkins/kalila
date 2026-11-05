// GPU trace pipeline.
//
// ingest (compute): one invocation per channel walks the newly uploaded raw int16 samples
//   (interleaved exactly as received from the board), runs the display IIR cascade and folds
//   the result into per-pixel-column (min, max) pairs of a sweeping time window.
// vs/fs (render): one instance per visible channel row, 6 vertices per column, building a
//   1-px-or-taller bar from the column's min/max joined to its neighbour (gap-free trace).
//
// Per-channel `chinfo` = (gain, filter, units_per_row, center):
//   gain          physical units per int16 LSB (µV for electrodes, V for AUX/ADC)
//   filter        1 = run the display filter, 0 = always raw (AUX/ADC)
//   units_per_row 0 = use the global µV/row; otherwise this channel's own row range
//   center        value drawn at the row's centre line

struct Ingest {
    nch: u32,
    n_new: u32,
    window: u32,
    cols: u32,
    pos0: u32,
    nsec: u32,
    _unused: f32,
    _pad: u32,
    coef: array<vec4<f32>, 8>, // per section: (b0, b1, b2, 0), (a1, a2, 0, 0)
};

@group(0) @binding(0) var<uniform> P: Ingest;
@group(0) @binding(1) var<storage, read> raw: array<u32>;
@group(0) @binding(2) var<storage, read_write> state: array<f32>; // 12 per channel
@group(0) @binding(3) var<storage, read_write> cols: array<vec2<f32>>;
@group(0) @binding(4) var<storage, read> chinfo: array<vec4<f32>>;

fn sample(i: u32, ch: u32) -> f32 {
    let idx = i * P.nch + ch;
    let w = raw[idx >> 1u];
    let h = select(w & 0xffffu, w >> 16u, (idx & 1u) == 1u);
    let s = bitcast<i32>(h << 16u) >> 16u; // sign-extend int16
    return f32(s) * chinfo[ch].x;
}

@compute @workgroup_size(64)
fn ingest(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ch = gid.x;
    if (ch >= P.nch) {
        return;
    }
    let sb = ch * 12u;
    var z: array<f32, 8>;
    for (var k = 0u; k < 8u; k++) {
        z[k] = state[sb + k];
    }
    var mn = state[sb + 8u];
    var mx = state[sb + 9u];
    var cur = bitcast<u32>(state[sb + 10u]);
    let fw = f32(P.window);
    let fc = f32(P.cols);
    let nsec = select(0u, P.nsec, chinfo[ch].y > 0.5);
    for (var i = 0u; i < P.n_new; i++) {
        var x = sample(i, ch);
        for (var s = 0u; s < nsec; s++) {
            let b = P.coef[2u * s];
            let a = P.coef[2u * s + 1u];
            let y = b.x * x + z[2u * s];
            z[2u * s] = b.y * x - a.x * y + z[2u * s + 1u];
            z[2u * s + 1u] = b.z * x - a.y * y;
            x = y;
        }
        let pos = (P.pos0 + i) % P.window;
        let col = min(u32(f32(pos) * fc / fw), P.cols - 1u);
        if (col != cur) {
            mn = x;
            mx = x;
            cur = col;
        } else {
            mn = min(mn, x);
            mx = max(mx, x);
        }
        cols[ch * P.cols + col] = vec2<f32>(mn, mx);
    }
    for (var k = 0u; k < 8u; k++) {
        state[sb + k] = z[k];
    }
    state[sb + 8u] = mn;
    state[sb + 9u] = mx;
    state[sb + 10u] = bitcast<f32>(cur);
}

struct Render {
    vp: vec2<f32>,
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
};

@group(0) @binding(0) var<uniform> R: Render;
@group(0) @binding(1) var<storage, read> rcols: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> colors: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> order: array<u32>; // row -> channel
@group(0) @binding(4) var<storage, read> rinfo: array<vec4<f32>>; // = chinfo

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn empty(m: vec2<f32>) -> bool {
    return m.x > m.y; // columns are cleared to (+big, -big)
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) inst: u32) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(-2.0, -2.0, 0.0, 1.0); // degenerate default (clipped)
    o.color = vec4<f32>(0.0);
    let row = inst;
    let ch = order[R.first_row + row];
    let col = vi / 6u;
    let corner = vi % 6u;
    let base = ch * R.cols;
    let m = rcols[base + col];
    if (empty(m)) {
        return o;
    }
    // Hide stale data just ahead of the sweep cursor.
    let d = (col + R.cols - R.cursor) % R.cols;
    if (d >= 1u && d <= R.gap) {
        return o;
    }
    var lo = m.x;
    var hi = m.y;
    if (col > 0u) {
        let p = rcols[base + col - 1u];
        let dp = (col - 1u + R.cols - R.cursor) % R.cols;
        if (!empty(p) && !(dp >= 1u && dp <= R.gap)) {
            lo = min(lo, p.y);
            hi = max(hi, p.x);
        }
    }
    let ci = rinfo[ch];
    let upr = select(R.uv_per_row, ci.z, ci.z > 0.0);
    let yc = (f32(row) + 0.5) * R.row_h;
    let k = R.row_h / upr;
    let half = 0.5 * R.thickness;
    let yt = yc - (hi - ci.w) * k - half;
    let yb = yc - (lo - ci.w) * k + half;
    let x0 = f32(col) * R.vp.x / f32(R.cols);
    let x1 = f32(col + 1u) * R.vp.x / f32(R.cols);
    var px = vec2<f32>(0.0);
    switch corner {
        case 0u: { px = vec2<f32>(x0, yt); }
        case 1u: { px = vec2<f32>(x1, yt); }
        case 2u: { px = vec2<f32>(x0, yb); }
        case 3u: { px = vec2<f32>(x1, yt); }
        case 4u: { px = vec2<f32>(x1, yb); }
        default: { px = vec2<f32>(x0, yb); }
    }
    o.pos = vec4<f32>(px.x / R.vp.x * 2.0 - 1.0, 1.0 - px.y / R.vp.y * 2.0, 0.0, 1.0);
    o.color = colors[ch];
    return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4<f32> {
    return i.color;
}
