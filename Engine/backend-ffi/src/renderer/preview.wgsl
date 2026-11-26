// Hanabi preview layer shader: transform + colour grading + effects in ONE pass.
//
// Every parameter arrives in `Params` (built by params.rs from clip.props;
// the same uniforms and shader are used by preview and final export). All "px" values are in 1080p
// REFERENCE pixels, so a preview at any resolution matches the 1080p export.
// All sampling uses textureSampleLevel so loops/branches never break uniformity.

struct Params {
    size:  vec4<f32>, // src_w, src_h, target_w, target_h
    geo0:  vec4<f32>, // scale_x, scale_y, rotation_rad, opacity
    geo1:  vec4<f32>, // pos_x, pos_y (fractions of target), mirror(0/1), time_sec
    crop:  vec4<f32>, // left, right, top, bottom (fractions of source)
    transition: vec4<f32>, // axis_x, axis_y, progress, kind
    distort: vec4<f32>, // top-left, top-right, bottom-right, bottom-left
    light: vec4<f32>, // brightness(+), contrast(x), saturation(x), vibrance
    bal_s: vec4<f32>, // colour balance shadows    rgb
    bal_m: vec4<f32>, // colour balance midtones   rgb
    bal_h: vec4<f32>, // colour balance highlights rgb
    hsl:   vec4<f32>, // hue_deg, sat(-1..1), lightness, range_index(0=all,1..8)
    fx0:   vec4<f32>, // sharpen_amount, sharpen_radius_px, blur_value, blur_mode(0 off,1 gauss,2 motion,3 zoom)
    fx1:   vec4<f32>, // blur_angle_rad, vignette_amount(-1..1), vignette_mid, vignette_feather
    fx2:   vec4<f32>, // vignette_aspect, reserved, reserved, chroma_px
    fx3:   vec4<f32>, // chroma_angle_rad, reserved, reserved, reserved
    fx4:   vec4<f32>, // pixelate_px(0=off), posterize_levels(0=off), posterize_blend, unused
    fx5:   vec4<f32>, // reserved, reserved, scan_spacing_px(0=off), scan_amount
    fx6:   vec4<f32>, // reserved, reserved, halftone_size_px(0=off), halftone_contrast
    fx7:   vec4<f32>, // halftone_angle_rad, burn_amount, burn_warmth, burn_spread
    fx8:   vec4<f32>, // burn_flicker, leak_amount, leak_warmth, leak_position
    fx9:   vec4<f32>, // sine_wave_amount, sine_wave_frequency, swirl_radians, swirl_radius
};

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@group(0) @binding(2) var<uniform> u: Params;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0)
    );
    var out: VertexOut;
    let p = positions[index];
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

const PI: f32 = 3.14159265;
const GOLDEN: f32 = 2.39996323;

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn sat3(c: vec3<f32>) -> vec3<f32> {
    return clamp(c, vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(1.0, 1.0, 1.0));
}

fn fetch_sample(uv: vec2<f32>) -> vec4<f32> {
    let c = clamp(uv, vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0));
    return textureSampleLevel(source_texture, source_sampler, c, 0.0);
}

fn fetch(uv: vec2<f32>) -> vec3<f32> {
    return fetch_sample(uv).rgb;
}

// Gaussian-weighted Vogel-disk blur. `ruv` = source-uv per reference pixel.
fn blur_gauss(uv: vec2<f32>, sigma: f32, ruv: vec2<f32>) -> vec3<f32> {
    var acc = fetch(uv);
    var wsum = 1.0;
    let reach = sigma * 2.5;
    for (var i = 0; i < 20; i = i + 1) {
        let fi = f32(i);
        let r = sqrt((fi + 0.5) / 20.0) * reach;
        let a = fi * GOLDEN;
        let w = exp(-0.5 * (r * r) / (sigma * sigma));
        acc = acc + fetch(uv + vec2<f32>(cos(a), sin(a)) * r * ruv) * w;
        wsum = wsum + w;
    }
    return acc / wsum;
}

fn blur_motion(uv: vec2<f32>, len_px: f32, angle: f32, ruv: vec2<f32>) -> vec3<f32> {
    let dir = vec2<f32>(cos(angle), sin(angle)) * ruv;
    var acc = vec3<f32>(0.0, 0.0, 0.0);
    for (var i = 0; i < 16; i = i + 1) {
        let t = (f32(i) / 15.0 - 0.5) * len_px;
        acc = acc + fetch(uv + dir * t);
    }
    return acc / 16.0;
}

fn blur_zoom(uv: vec2<f32>, strength: f32) -> vec3<f32> {
    let c = vec2<f32>(0.5, 0.5);
    var acc = vec3<f32>(0.0, 0.0, 0.0);
    for (var i = 0; i < 16; i = i + 1) {
        let t = f32(i) / 15.0;
        acc = acc + fetch(c + (uv - c) * (1.0 - strength * t));
    }
    return acc / 16.0;
}

fn hue_rotate(c: vec3<f32>, deg: f32) -> vec3<f32> {
    let a = radians(deg);
    let cs = cos(a);
    let sn = sin(a);
    let r = dot(c, vec3<f32>(0.213 + cs * 0.787 - sn * 0.213, 0.715 - cs * 0.715 - sn * 0.715, 0.072 - cs * 0.072 + sn * 0.928));
    let g = dot(c, vec3<f32>(0.213 - cs * 0.213 + sn * 0.143, 0.715 + cs * 0.285 + sn * 0.140, 0.072 - cs * 0.072 - sn * 0.283));
    let b = dot(c, vec3<f32>(0.213 - cs * 0.213 - sn * 0.787, 0.715 - cs * 0.715 + sn * 0.715, 0.072 + cs * 0.928 + sn * 0.072));
    return vec3<f32>(r, g, b);
}

// Centre hue (degrees) for hsl range index 1..8: r, r+y, y, g, c, b, b+m, m+r
fn range_center(i: f32) -> f32 {
    if (i < 1.5) { return 0.0; }
    if (i < 2.5) { return 30.0; }
    if (i < 3.5) { return 60.0; }
    if (i < 4.5) { return 120.0; }
    if (i < 5.5) { return 180.0; }
    if (i < 6.5) { return 240.0; }
    if (i < 7.5) { return 270.0; }
    return 330.0;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let tgt = u.size.zw;
    let k = tgt.y / 1080.0; // target px per 1080p reference px

    // ---------------------------------------------------------------- geometry
    // Crop removes source-edge pixels; it must not shrink the source aspect
    // and then scale the remaining image back up (the old behaviour zoomed in).
    let src_aspect = u.size.x / u.size.y;
    let tgt_aspect = tgt.x / tgt.y;
    var fit = vec2<f32>(1.0, 1.0);
    if (src_aspect > tgt_aspect) {
        fit.y = tgt_aspect / src_aspect;
    } else {
        fit.x = src_aspect / tgt_aspect;
    }
    let base_px = fit * tgt;                               // fitted size, before user scale
    let layer_px = base_px * vec2<f32>(u.geo0.x, u.geo0.y); // after user scale

    // target pixel -> layer space: undo position, then rotation, then scale
    var p = (in.uv - vec2<f32>(0.5, 0.5)) * tgt - vec2<f32>(u.geo1.x, u.geo1.y) * tgt;
    let transition_progress = clamp(u.transition.z, 0.0, 1.0);
    let transition_kind = i32(u.transition.w + 0.5);
    // Zoom and spin are distinct spatial reveals rather than directional slides.
    if (transition_kind == 6) {
        p = p / mix(0.32, 1.0, transition_progress);
    } else if (transition_kind == 7) {
        let spin_scale = mix(0.48, 1.0, transition_progress);
        let spin_angle = (1.0 - transition_progress) * 1.35 * PI;
        let spin_cs = cos(spin_angle);
        let spin_sn = sin(spin_angle);
        p = vec2<f32>(spin_cs * p.x + spin_sn * p.y, -spin_sn * p.x + spin_cs * p.y) / spin_scale;
    }
    if (transition_kind == 4) {
        let slide_axis = normalize(u.transition.xy + vec2<f32>(0.0001, 0.0001));
        p = p + slide_axis * (1.0 - transition_progress) * max(tgt.x, tgt.y) * 1.25;
    }
    let cs = cos(u.geo0.z);
    let sn = sin(u.geo0.z);
    p = vec2<f32>(cs * p.x + sn * p.y, -sn * p.x + cs * p.y);
    var l = p / layer_px + vec2<f32>(0.5, 0.5);

    // anti-aliased layer edge
    let edge_px = min(min(l.x, 1.0 - l.x) * layer_px.x, min(l.y, 1.0 - l.y) * layer_px.y);
    let coverage = clamp(edge_px + 0.5, 0.0, 1.0);
    if (coverage <= 0.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    let lin = l; // layer coords before any warp (used for vignette / overlays)

    if (u.geo1.z > 0.5) { l.x = 1.0 - l.x; } // mirror

    // Corner displacement uses the same four transform.distort values as export.
    let distort_top = mix(u.distort.x, u.distort.y, l.x);
    let distort_bottom = mix(u.distort.w, u.distort.z, l.x);
    l.y = l.y + mix(distort_top, distort_bottom, l.y) * 0.001;

    // Hanabi sine-wave warp. Amount is normalized and the phase follows the
    // clip-local preview time, so paused frames and exported frames match.
    if (u.fx9.x > 0.0001) {
        let wave_phase = l.x * u.fx9.y * PI * 2.0 + u.geo1.w * 3.0;
        l.y = l.y + sin(wave_phase) * u.fx9.x * 0.075;
    }

    // Hanabi swirl warp: twist the inside of the selected radius and feather
    // the edge so it does not create a hard circular discontinuity.
    if (u.fx9.z > 0.0001) {
        let center = vec2<f32>(0.5, 0.5);
        let d = l - center;
        let radius = length(d);
        let edge_fade = 1.0 - smoothstep(u.fx9.w, 1.0, radius);
        let angle = u.fx9.z * edge_fade * max(0.0, 1.0 - radius);
        let cs = cos(angle);
        let sn = sin(angle);
        l = center + vec2<f32>(cs * d.x - sn * d.y, sn * d.x + cs * d.y);
    }

    // pixelate (block size in reference px)
    let ref_px = base_px / k; // layer size in reference px
    if (u.fx4.x > 1.0) {
        l = (floor(l * ref_px / u.fx4.x) + vec2<f32>(0.5, 0.5)) * u.fx4.x / ref_px;
    }

    // The crop is a transparent cut-out over the original, unchanged source
    // geometry. This keeps subject scale/position stable while crop values move.
    let crop_right = 1.0 - u.crop.y;
    let crop_bottom = 1.0 - u.crop.w;
    let crop_edge_px = min(min((l.x - u.crop.x) * u.size.x, (crop_right - l.x) * u.size.x),
                           min((l.y - u.crop.z) * u.size.y, (crop_bottom - l.y) * u.size.y));
    let crop_coverage = clamp(crop_edge_px + 0.5, 0.0, 1.0);
    if (crop_coverage <= 0.0) { return vec4<f32>(0.0, 0.0, 0.0, 0.0); }
    let suv = l;
    let ruv = k / base_px; // source-uv per reference pixel

    // ------------------------------------------------------- sampling / blurs
    var col = fetch(suv);

    let blur_mode = i32(u.fx0.w + 0.5);
    var sigma = 0.0;
    if (blur_mode == 1) { sigma = u.fx0.z; }
    if (transition_kind == 8) {
        let transition_blur = (1.0 - transition_progress) * 8.0;
        if (transition_blur > 0.25) {
            col = blur_gauss(suv, transition_blur, ruv);
        }
    } else if (sigma > 0.25) {
        col = blur_gauss(suv, sigma, ruv);
    } else if (blur_mode == 2) {
        col = blur_motion(suv, u.fx0.z, u.fx1.x, ruv);
    } else if (blur_mode == 3) {
        col = blur_zoom(suv, u.fx0.z);
    }

    // chromatic aberration + glitch (RGB split)
    var shift = vec2<f32>(0.0, 0.0);
    if (u.fx2.w > 0.01) {
        shift = vec2<f32>(cos(u.fx3.x), sin(u.fx3.x)) * u.fx2.w;
    }
    if (abs(shift.x) + abs(shift.y) > 0.01) {
        let off = shift * ruv;
        col.r = fetch(suv + off).r;
        col.b = fetch(suv - off).b;
    }

    // sharpen (unsharp mask)
    if (u.fx0.x > 0.001) {
        let rad = max(u.fx0.y, 1.0);
        var bl = fetch(suv + vec2<f32>(rad, 0.0) * ruv);
        bl = bl + fetch(suv - vec2<f32>(rad, 0.0) * ruv);
        bl = bl + fetch(suv + vec2<f32>(0.0, rad) * ruv);
        bl = bl + fetch(suv - vec2<f32>(0.0, rad) * ruv);
        bl = bl * 0.25;
        col = col + (col - bl) * u.fx0.x;
    }

    // ---------------------------------------------------------- colour grading
    // exposure/brightness + contrast + saturation (ffmpeg eq)
    col = (col - vec3<f32>(0.5, 0.5, 0.5)) * u.light.y + vec3<f32>(0.5, 0.5, 0.5) + vec3<f32>(u.light.x, u.light.x, u.light.x);
    let lm = luma(col);
    col = sat3(vec3<f32>(lm, lm, lm) + (col - vec3<f32>(lm, lm, lm)) * u.light.z);

    // vibrance: boosts low-saturation pixels more
    if (abs(u.light.w) > 0.001) {
        let mx = max(col.r, max(col.g, col.b));
        let mn = min(col.r, min(col.g, col.b));
        let boost = 1.0 + u.light.w * (1.0 - (mx - mn));
        let lv = luma(col);
        col = sat3(vec3<f32>(lv, lv, lv) + (col - vec3<f32>(lv, lv, lv)) * boost);
    }

    // HSL for the selected range
    if (abs(u.hsl.x) > 0.001 || abs(u.hsl.y) > 0.001 || abs(u.hsl.z) > 0.001) {
        var w = 1.0;
        if (u.hsl.w > 0.5) {
            let mx = max(col.r, max(col.g, col.b));
            let mn = min(col.r, min(col.g, col.b));
            var hh = degrees(atan2(1.7320508 * (col.g - col.b), 2.0 * col.r - col.g - col.b));
            if (hh < 0.0) { hh = hh + 360.0; }
            var d = abs(hh - range_center(u.hsl.w));
            d = min(d, 360.0 - d);
            w = (1.0 - smoothstep(15.0, 45.0, d)) * smoothstep(0.02, 0.15, mx - mn);
        }
        let hr = hue_rotate(col, u.hsl.x * w);
        let lh = luma(hr);
        var res = vec3<f32>(lh, lh, lh) + (hr - vec3<f32>(lh, lh, lh)) * (1.0 + u.hsl.y * w);
        res = res + vec3<f32>(u.hsl.z * w, u.hsl.z * w, u.hsl.z * w);
        col = sat3(res);
    }

    // shadows / midtones / highlights colour balance (wheels, temperature, presets)
    let lb = luma(col);
    let w_s = clamp((0.333 - lb) * 4.0 + 0.5, 0.0, 1.0) * 0.7;
    let w_m = clamp((lb - 0.333) * 4.0 + 0.5, 0.0, 1.0) * clamp((1.0 - lb - 0.333) * 4.0 + 0.5, 0.0, 1.0) * 0.7;
    let w_h = clamp((lb + 0.333 - 1.0) * 4.0 + 0.5, 0.0, 1.0) * 0.7;
    col = sat3(col + u.bal_s.xyz * w_s + u.bal_m.xyz * w_m + u.bal_h.xyz * w_h);

    // ---------------------------------------------------------------- effects
    // vignette (negative = lightens the edges)
    if (abs(u.fx1.y) > 0.001) {
        let pv = (lin - vec2<f32>(0.5, 0.5)) * vec2<f32>(u.fx2.x, 1.0) * 2.0;
        let dv = length(pv) * 0.7071;
        let m = smoothstep(u.fx1.z * 0.9, u.fx1.z * 0.9 + max(u.fx1.w, 0.01) * 0.8, dv);
        if (u.fx1.y > 0.0) {
            col = col * (1.0 - u.fx1.y * m);
        } else {
            col = col + (vec3<f32>(1.0, 1.0, 1.0) - col) * (-u.fx1.y) * m;
        }
    }

    // posterize
    if (u.fx4.y > 1.5) {
        let steps = u.fx4.y - 1.0;
        let q = floor(col * steps + vec3<f32>(0.5, 0.5, 0.5)) / steps;
        col = mix(col, q, u.fx4.z);
    }

    // halftone (dots darken the image)
    if (u.fx6.z > 0.5) {
        let xy = lin * ref_px;
        let sz = u.fx6.z;
        let ca = cos(u.fx7.x);
        let sa = sin(u.fx7.x);
        let a = xy.x * ca + xy.y * sa + sz * 0.5;
        let b = -xy.x * sa + xy.y * ca + sz * 0.5;
        let px = (a - sz * floor(a / sz)) - sz * 0.5;
        let py = (b - sz * floor(b / sz)) - sz * 0.5;
        let radius = (1.0 - luma(col)) * sz * 0.5 * u.fx6.w;
        if (length(vec2<f32>(px, py)) < radius) {
            col = vec3<f32>(0.0, 0.0, 0.0);
        }
    }

    // scanlines
    if (u.fx5.z > 0.5) {
        let row = floor(lin.y * ref_px.y);
        let rr = row - u.fx5.z * floor(row / u.fx5.z);
        if (rr < 1.0) {
            col = col * (1.0 - u.fx5.w);
        }
    }

    // film burn (edge glow)
    if (u.fx7.y > 0.0001) {
        let spread = max(u.fx7.w, 0.01);
        let e = max(0.0, (abs(lin.x - 0.5) * 2.0 - (1.0 - spread)) / spread) * (1.0 + sin(u.geo1.w * 8.0 * PI) * u.fx8.x);
        col = sat3(col + vec3<f32>(u.fx7.y * u.fx7.z, u.fx7.y * 0.28, u.fx7.y * 0.06) * e);
    }

    // light leak
    if (u.fx8.y > 0.0001) {
        let radius = clamp(0.14 + u.fx8.z * 0.38, 0.14, 0.52);
        let hot = max(0.0, 1.0 - abs(lin.x - u.fx8.w) / radius);
        col = sat3(col + vec3<f32>(u.fx8.y, u.fx8.y * (0.25 + u.fx8.z * 0.2), u.fx8.y * (1.0 - u.fx8.z) * 0.25) * hot);
    }

    // Incoming transition masks are evaluated on the GPU with the same
    // direction/progress metadata the timeline supplies to the old compositor.
    var transition_alpha = 1.0;
    if (transition_kind == 1 || transition_kind == 6 || transition_kind == 7 || transition_kind == 8) {
        transition_alpha = transition_progress;
    } else if (transition_kind == 5) {
        // Dip to black: hold the incoming shot hidden until the outgoing shot
        // has completed its fade to black, then reveal it from black.
        transition_alpha = smoothstep(0.5, 1.0, transition_progress);
    } else if (transition_kind == 2) {
        let axis = normalize(u.transition.xy + vec2<f32>(0.0001, 0.0001));
        let wipe_coord = clamp(0.5 + dot(l - vec2<f32>(0.5, 0.5), axis) * 0.70710678, 0.0, 1.0);
        transition_alpha = 1.0 - smoothstep(transition_progress - 0.015, transition_progress + 0.015, wipe_coord);
    } else if (transition_kind == 3) {
        // The normalized corner distance is 1.0. Expanding beyond that value
        // ensures the iris reaches every corner instead of remaining clipped.
        let radius = transition_progress * 1.05;
        let radial_distance = distance(l, vec2<f32>(0.5, 0.5)) * 1.41421356;
        transition_alpha = select(
            0.0,
            1.0 - smoothstep(radius - 0.015, radius + 0.015, radial_distance),
            transition_progress > 0.0
        );
    } else if (transition_kind == 9) {
        let checker_cell = floor(l * vec2<f32>(12.0, 8.0));
        let checker_index = checker_cell.x + checker_cell.y;
        let reveal_at = select(0.72, 0.28, (checker_index - 2.0 * floor(checker_index / 2.0)) < 1.0);
        transition_alpha = smoothstep(reveal_at - 0.035, reveal_at + 0.035, transition_progress);
    }
    if (transition_kind == 10) {
        // Outgoing half of dip-to-black: stay opaque so the track below cannot
        // show through while the outgoing picture is being darkened.
        let blacken = smoothstep(0.0, 0.5, transition_progress);
        col = col * (1.0 - blacken);
    }
    // Preserve source alpha (notably transparent PNG layers) as well as the
    // geometric, crop, opacity and transition coverage.
    let source_alpha = fetch_sample(suv).a;
    let alpha = source_alpha * coverage * crop_coverage * clamp(u.geo0.w, 0.0, 1.0) * transition_alpha;
    return vec4<f32>(col, alpha);
}
