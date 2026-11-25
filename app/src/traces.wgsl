// GPU trace pipeline.
//
// Per frame, on the newly uploaded raw int16 samples (interleaved exactly as received):
// filt (compute): one invocation per channel runs the display IIR cascade into `F`.
// carmean (compute): one invocation per sample averages the CAR reference channels into
//   `refv` (display-only common average reference).
// fold (compute): one invocation per channel subtracts the reference where flagged, keeps
//   the result in the triggered-mode ring (`fring`) and folds it into per-pixel-column
//   (min, max) pairs of the sweeping time window.
// cut (compute): triggered mode. One invocation per (channel, new trigger) folds the ring
//   window [trigger - pre, trigger + post) into the trigger's sweep slot (`scols`).
// vs/fs (render): one instance per (visible row, channel in the row[, sweep slot]), 6 vertices
//   per column, building a 1-px-or-taller bar from the column's min/max joined to its
//   neighbour (gap-free trace). Older sweeps fade.
//
// Per-channel `chinfo` = (gain, flags, units_per_row, center):
//   gain          physical units per int16 LSB (µV for electrodes, V for AUX/ADC)
//   flags         bit 3: display filter (0 raw, 1 the view's band; AUX/ADC raw)
//                 bit 1: part of the CAR reference; bit 2: subtract the CAR
//   units_per_row 0 = use the global µV/row; otherwise this channel's own row range
//   center        value drawn at the row's centre line
// `rowtab[row]` = (first index into `order`, channel count): several channels can share a row.

struct Ingest {
    nch: u32,
    n_new: u32,
    window: u32,
    cols: u32,
    pos0: u32,
    ring0: u32, // ring position of the first new sample
    ring: u32,  // ring length in samples; 0 = no ring (free-running)
    car: u32,   // 1 = apply the CAR (flags bits 1/2)
    fsec: vec2<u32>,            // per filter (0 raw, 1 band): first section | count << 8
    _p0: u32,
    _p1: u32,
    coef: array<vec4<f32>, 16>, // per section: (b0, b1, b2, 0), (a1, a2, 0, 0)
};

// Biquad sections one filter may use; two state values each. Mirrors `MAX_FILTER_SECTIONS` in
// traces.rs (keep equal); the state array below holds 2 * this.
const MAX_FILTER_SECTIONS: u32 = 4u;

@group(0) @binding(0) var<uniform> P: Ingest;
@group(0) @binding(1) var<storage, read> raw: array<u32>;
@group(0) @binding(2) var<storage, read_write> state: array<f32>; // 12 per channel
@group(0) @binding(3) var<storage, read_write> cols: array<vec2<f32>>;
@group(0) @binding(4) var<storage, read> chinfo: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read_write> fring: array<f32>;
@group(0) @binding(6) var<storage, read_write> F: array<f32>;    // filtered, n_new × nch
@group(0) @binding(7) var<storage, read_write> refv: array<f32>; // CAR, n_new

fn flags(ch: u32) -> u32 {
    return u32(chinfo[ch].y + 0.5);
}

fn sample(i: u32, ch: u32) -> f32 {
    let idx = i * P.nch + ch;
    let w = raw[idx >> 1u];
    let h = select(w & 0xffffu, w >> 16u, (idx & 1u) == 1u);
    let s = bitcast<i32>(h << 16u) >> 16u; // sign-extend int16
    return f32(s) * chinfo[ch].x;
}

@compute @workgroup_size(64)
fn filt(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ch = gid.x;
    if (ch >= P.nch) {
        return;
    }
    let sb = ch * 12u;
    var z: array<f32, 2u * MAX_FILTER_SECTIONS>;
    for (var k = 0u; k < 2u * MAX_FILTER_SECTIONS; k++) {
        z[k] = state[sb + k];
    }
    // This channel's filter: a run of sections in the shared bank (at most MAX_FILTER_SECTIONS, so 2 * that many states).
    let fs = P.fsec[(flags(ch) >> 3u) & 1u];
    let s0 = fs & 0xffu;
    let nsec = min(fs >> 8u, MAX_FILTER_SECTIONS);
    for (var i = 0u; i < P.n_new; i++) {
        var x = sample(i, ch);
        for (var s = 0u; s < nsec; s++) {
            let b = P.coef[2u * (s0 + s)];
            let a = P.coef[2u * (s0 + s) + 1u];
            let y = b.x * x + z[2u * s];
            z[2u * s] = b.y * x - a.x * y + z[2u * s + 1u];
            z[2u * s + 1u] = b.z * x - a.y * y;
            x = y;
        }
        F[i * P.nch + ch] = x;
    }
    for (var k = 0u; k < 2u * MAX_FILTER_SECTIONS; k++) {
        state[sb + k] = z[k];
    }
}

@compute @workgroup_size(64)
fn carmean(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= P.n_new) {
        return;
    }
    var s = 0.0;
    var n = 0u;
    for (var ch = 0u; ch < P.nch; ch++) {
        if ((flags(ch) & 2u) != 0u) {
            s += F[i * P.nch + ch];
            n += 1u;
        }
    }
    refv[i] = select(0.0, s / f32(max(n, 1u)), n > 0u);
}

@compute @workgroup_size(64)
fn fold(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ch = gid.x;
    if (ch >= P.nch) {
        return;
    }
    let sb = ch * 12u;
    var mn = state[sb + 8u];
    var mx = state[sb + 9u];
    var cur = bitcast<u32>(state[sb + 10u]);
    let fw = f32(P.window);
    let fc = f32(P.cols);
    let sub = P.car != 0u && (flags(ch) & 4u) != 0u;
    for (var i = 0u; i < P.n_new; i++) {
        var x = F[i * P.nch + ch];
        if (sub) {
            x -= refv[i];
        }
        if (P.ring > 0u) {
            fring[ch * P.ring + (P.ring0 + i) % P.ring] = x;
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
    state[sb + 8u] = mn;
    state[sb + 9u] = mx;
    state[sb + 10u] = bitcast<f32>(cur);
}

struct Cut {
    nch: u32,
    cols: u32,
    ring: u32,
    span: u32, // pre + post, samples
    n_trig: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
    trig: array<vec4<u32>, 16>, // (slot, ring index of window start, 0, 0)
};

@group(0) @binding(0) var<uniform> C: Cut;
@group(0) @binding(1) var<storage, read> cring: array<f32>;
@group(0) @binding(2) var<storage, read_write> scols: array<vec2<f32>>;

@compute @workgroup_size(64)
fn cut(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ch = gid.x;
    let t = gid.y;
    if (ch >= C.nch || t >= C.n_trig) {
        return;
    }
    let slot = C.trig[t].x;
    let start = C.trig[t].y;
    let base = (slot * C.nch + ch) * C.cols;
    let rb = ch * C.ring;
    if (C.span < C.cols) {
        // Fewer samples than columns (millisecond windows): interpolate linearly between
        // samples, so a column's value is the trace at that column's centre.
        for (var c = 0u; c < C.cols; c++) {
            let pos = max((f32(c) + 0.5) * f32(C.span) / f32(C.cols) - 0.5, 0.0);
            let i0 = min(u32(pos), C.span - 1u);
            let i1 = min(i0 + 1u, C.span - 1u);
            let f = pos - f32(i0);
            let v = mix(cring[rb + (start + i0) % C.ring], cring[rb + (start + i1) % C.ring], f);
            scols[base + c] = vec2<f32>(v, v);
        }
        return;
    }
    for (var c = 0u; c < C.cols; c++) {
        let s0 = c * C.span / C.cols;
        let s1 = min(max(s0 + 1u, (c + 1u) * C.span / C.cols), C.span);
        var mn = 1e30;
        var mx = -1e30;
        for (var s = s0; s < s1; s++) {
            let v = cring[rb + (start + s) % C.ring];
            mn = min(mn, v);
            mx = max(mx, v);
        }
        scols[base + c] = vec2<f32>(mn, mx);
    }
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
    x_shift: f32, // review: pan offset of a stale image, in columns
    kmax: u32,   // channels per row (instances per row per slot)
    nslots: u32, // 0 = free-running; else sweep slots
    newest: u32, // slot of the newest sweep
    filled: u32, // sweeps available (≤ nslots)
    x_scale: f32, // review: zoom of a stale image (columns drawn per data column)
    rot: u32,     // review: data column drawn at column 0
    _p1: u32,
    _p2: u32,
};

@group(0) @binding(0) var<uniform> R: Render;
@group(0) @binding(1) var<storage, read> rcols: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> colors: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> order: array<u32>; // order index -> channel
@group(0) @binding(4) var<storage, read> rinfo: array<vec4<f32>>; // = chinfo
@group(0) @binding(5) var<storage, read> rowtab: array<vec2<u32>>; // row -> (first order index, count)
@group(0) @binding(6) var<storage, read> rscols: array<vec2<f32>>; // = scols

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn empty(m: vec2<f32>) -> bool {
    return m.x > m.y; // columns are cleared to (+big, -big)
}

fn colv(trig: bool, idx: u32) -> vec2<f32> {
    if (trig) {
        return rscols[idx];
    }
    return rcols[idx];
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) inst: u32) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(-2.0, -2.0, 0.0, 1.0); // degenerate default (clipped)
    o.color = vec4<f32>(0.0);
    let trig = R.nslots > 0u;
    var slot = 0u;
    var rest = inst;
    if (trig) {
        slot = inst % R.nslots;
        rest = inst / R.nslots;
    }
    let row = rest / R.kmax;
    let sub = rest % R.kmax;
    let rt = rowtab[R.first_row + row];
    if (sub >= rt.y) {
        return o;
    }
    let ch = order[rt.x + sub];
    var alpha = 1.0;
    if (trig) {
        let age = (R.newest + R.nslots - slot) % R.nslots;
        if (age >= R.filled) {
            return o;
        }
        // Newest sweep at full strength; older ones fade towards 0.12.
        alpha = select(0.12 + 0.6 * (1.0 - f32(age) / f32(max(R.filled, 1u))), 1.0, age == 0u);
    }
    let col = vi / 6u;
    let corner = vi % 6u;
    let base = select(ch * R.cols, (slot * R.nch + ch) * R.cols, trig);
    // Review images may be rotated: the data column at `col` is (col + rot) % cols.
    let dc = (col + R.rot) % R.cols;
    let m = colv(trig, base + dc);
    if (empty(m)) {
        return o;
    }
    // Free-running: hide stale data just ahead of the sweep cursor.
    let d = (dc + R.cols - R.cursor) % R.cols;
    if (!trig && d >= 1u && d <= R.gap) {
        return o;
    }
    var lo = m.x;
    var hi = m.y;
    if (col > 0u) {
        let dcp = (dc + R.cols - 1u) % R.cols;
        let p = colv(trig, base + dcp);
        let dp = (dcp + R.cols - R.cursor) % R.cols;
        if (!empty(p) && (trig || !(dp >= 1u && dp <= R.gap))) {
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
    let x0 = (f32(col) * R.x_scale + R.x_shift) * R.vp.x / f32(R.cols);
    let x1 = (f32(col + 1u) * R.x_scale + R.x_shift) * R.vp.x / f32(R.cols);
    if (x1 < 0.0 || x0 > R.vp.x) {
        return o;
    }
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
    let c = colors[ch];
    o.color = vec4<f32>(c.rgb, c.a * alpha);
    return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4<f32> {
    return i.color;
}
