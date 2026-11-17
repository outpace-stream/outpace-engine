// GPU version of render.rs: fractal + life + logo, written straight to NV12 (BT.709, limited
// range). One invocation shades a 4x2 pixel block, so each luma row and the interleaved chroma
// row are written as whole u32 words.

struct Params {
    size: vec4<u32>,      // width, height, cell, gap
    grid: vec4<u32>,      // life cols, life rows, sprite stride, sprite rows
    sprite: vec4<f32>,    // sprite origin x, origin y (top-left including margin), flash, -
    julia: vec4<f32>,     // cx, cy, palette phase, -
    plane: vec4<f32>,     // scale*sin(rot), scale*cos(rot), -, - (complex units per pixel)
    primary: vec4<f32>,   // logo ring colour, 0..255
    secondary: vec4<f32>, // logo sweep colour, 0..255
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> lut: array<vec4<f32>>;     // 1024-entry palette
@group(0) @binding(2) var<storage, read> sprite: array<vec4<f32>>;  // alpha, tone, shadow, -
@group(0) @binding(3) var<storage, read> cells: array<vec4<f32>>;   // r, g, b, glow
@group(0) @binding(4) var<storage, read_write> out: array<u32>;     // NV12

// Keep in sync with render.rs.
const MAX_ITER: u32 = 64u;
const LUT_SIZE: f32 = 1024.0;
const INTERIOR: vec3<f32> = vec3<f32>(9.0, 5.0, 20.0);
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;

fn julia(z0: vec2<f32>) -> vec3<f32> {
    var z = z0;
    var r2 = dot(z, z);
    var i = 0u;
    loop {
        if (i >= MAX_ITER || r2 > 256.0) { break; }
        z = vec2<f32>(z.x * z.x - z.y * z.y + p.julia.x, 2.0 * z.x * z.y + p.julia.y);
        r2 = dot(z, z);
        i = i + 1u;
    }
    if (i == MAX_ITER) {
        return INTERIOR;
    }
    let mu = max(f32(i) + 1.0 - log2(log(r2) * 0.5), 0.0);
    let idx = u32(fract(mu * 0.045 + p.julia.z) * LUT_SIZE) & 1023u;
    let g = min(mu / 16.0, 1.0);
    let glow = 0.24 + 0.76 * g * g * (3.0 - 2.0 * g);
    return lut[idx].rgb * glow;
}

fn shade(x: u32, y: u32) -> vec3<f32> {
    let w = f32(p.size.x);
    let h = f32(p.size.y);
    let q = vec2<f32>(f32(x) + 0.5 - w * 0.5, f32(y) + 0.5 - h * 0.5);
    let z = vec2<f32>(q.x * p.plane.y - q.y * p.plane.x, q.x * p.plane.x + q.y * p.plane.y);
    var rgb = julia(z);

    // Life cells, additive, with a gap between cells.
    let cell = p.size.z;
    let gap = p.size.w;
    let wx = x % cell;
    let wy = y % cell;
    if (wx >= gap && wx < cell - gap && wy >= gap && wy < cell - gap) {
        let cx = x / cell;
        let cy = y / cell;
        if (cx < p.grid.x && cy < p.grid.y) {
            let c = cells[cy * p.grid.x + cx];
            if (c.w > 0.003) {
                rgb = min(rgb + (c.rgb * 0.75 + vec3<f32>(40.0)) * c.w * 0.6, vec3<f32>(255.0));
            }
        }
    }

    // Logo with drop shadow; a corner hit turns the shadow into a glow.
    let lx = i32(x) - i32(p.sprite.x);
    let ly = i32(y) - i32(p.sprite.y);
    if (lx >= 0 && ly >= 0 && lx < i32(p.grid.z) && ly < i32(p.grid.w)) {
        let s = sprite[u32(ly) * p.grid.z + u32(lx)];
        let flash = p.sprite.z;
        var v = rgb * (1.0 - 0.6 * s.z * (1.0 - flash));
        v = v + p.primary.rgb * s.z * flash * 0.9;
        var logo = mix(p.primary.rgb, p.secondary.rgb, s.y);
        logo = logo + (vec3<f32>(255.0) - logo) * flash * 0.55;
        rgb = mix(v, logo, s.x);
    }
    return rgb;
}

fn luma(c: vec3<f32>) -> u32 {
    let y = KR * c.r + (1.0 - KR - KB) * c.g + KB * c.b;
    return u32(clamp(round(16.0 + y * (219.0 / 255.0)), 16.0, 235.0));
}

fn chroma(c: vec3<f32>) -> vec2<u32> {
    let y = KR * c.r + (1.0 - KR - KB) * c.g + KB * c.b;
    let cb = 128.0 + (c.b - y) / (2.0 * (1.0 - KB)) * (224.0 / 255.0);
    let cr = 128.0 + (c.r - y) / (2.0 * (1.0 - KR)) * (224.0 / 255.0);
    return vec2<u32>(u32(clamp(round(cb), 16.0, 240.0)), u32(clamp(round(cr), 16.0, 240.0)));
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let w = p.size.x;
    let h = p.size.y;
    if (id.x >= w / 4u || id.y >= h / 2u) {
        return;
    }
    let x0 = id.x * 4u;
    let y0 = id.y * 2u;
    var top = 0u;
    var bottom = 0u;
    var uv = 0u;
    for (var k = 0u; k < 2u; k = k + 1u) {
        let x = x0 + 2u * k;
        let a = shade(x, y0);
        let b = shade(x + 1u, y0);
        let c = shade(x, y0 + 1u);
        let d = shade(x + 1u, y0 + 1u);
        top = top | (luma(a) << (16u * k)) | (luma(b) << (16u * k + 8u));
        bottom = bottom | (luma(c) << (16u * k)) | (luma(d) << (16u * k + 8u));
        let cbcr = chroma((a + b + c + d) * 0.25);
        uv = uv | (cbcr.x << (16u * k)) | (cbcr.y << (16u * k + 8u));
    }
    out[(y0 * w + x0) / 4u] = top;
    out[((y0 + 1u) * w + x0) / 4u] = bottom;
    out[(w * h + id.y * w + x0) / 4u] = uv;
}
