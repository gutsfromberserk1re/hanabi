//! Maps a clip's `props` JSON onto the shared WGPU shader uniform block.
//!
//! The property tree is the one the C inspector writes (`props.transform.*`,
//! `props.colors.*`, `props.effects.*`). Preview and export both build these
//! same uniforms and pass them through the same WGPU shader, so visual controls
//! are mapped here once. `filters.rs` remains responsible for audio filters;
//! active preview and final-export rendering do not use FFmpeg video filters.
//!
//! Layout rule: every field is a `[f32; 4]` so the Rust struct has exactly the
//! same std140 layout as `Params` in preview.wgsl (no padding surprises).
use serde_json::Value;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LayerUniforms {
    pub size: [f32; 4],  // src_w, src_h, target_w, target_h (filled by the renderer)
    pub geo0: [f32; 4],  // scale_x, scale_y, rotation_rad, opacity
    pub geo1: [f32; 4],  // pos_x, pos_y (fractions of target), mirror, time_sec
    pub crop: [f32; 4],  // left, right, top, bottom
    pub transition: [f32; 4], // axis_x, axis_y, progress, kind (0 none, 1 crossfade, 2 wipe, 3 iris, 4 slide, 5 dip-in, 6 zoom, 7 spin, 8 blur, 9 checker, 10 dip-out)
    pub distort: [f32; 4], // top-left, top-right, bottom-right, bottom-left
    pub light: [f32; 4], // brightness, contrast, saturation, vibrance
    pub bal_s: [f32; 4],
    pub bal_m: [f32; 4],
    pub bal_h: [f32; 4],
    pub hsl: [f32; 4],   // hue_deg, sat, lightness, range_index
    pub fx0: [f32; 4],   // sharpen_amount, sharpen_radius_px, blur_value, blur_mode
    pub fx1: [f32; 4],   // blur_angle, vignette_amount, vignette_mid, vignette_feather
    pub fx2: [f32; 4],   // vignette_aspect, reserved, reserved, chroma_px
    pub fx3: [f32; 4],   // chroma_angle, reserved, reserved, reserved
    pub fx4: [f32; 4],   // pixelate_px, posterize_levels, posterize_blend, unused
    pub fx5: [f32; 4],   // reserved, reserved, scan_spacing, scan_amount
    pub fx6: [f32; 4],   // reserved, reserved, halftone_size, halftone_contrast
    pub fx7: [f32; 4],   // halftone_angle, burn_amount, burn_warmth, burn_spread
    pub fx8: [f32; 4],   // burn_flicker, leak_amount, leak_warmth, leak_position
    pub fx9: [f32; 4],   // sine_wave_amount, sine_wave_frequency, swirl_radians, swirl_radius
}

fn n(value: &Value, key: &str, fallback: f64) -> f64 {
    value[key].as_f64().filter(|v| v.is_finite()).unwrap_or(fallback)
}
fn on(effects: &Value, key: &str) -> bool {
    effects[key]["enabled"].as_bool().unwrap_or(false)
}
fn flag(value: bool) -> f32 {
    if value { 1.0 } else { 0.0 }
}

const KEYFRAME_NUMERIC_PATHS: &[&str] = &[
    "transform.position.x", "transform.position.y", "transform.scale.w", "transform.scale.h",
    "transform.rotation", "transform.opacity",
    "colors.light.exposure", "colors.light.brightness", "colors.light.contrast",
    "colors.light.highlights", "colors.light.shadows", "colors.light.whites", "colors.light.blacks",
    "colors.color.temperature", "colors.color.vibrance", "colors.color.saturation",
    "colors.detail.sharpness", "colors.detail.clarity", "colors.detail.vignette",
    "effects.chromatic.amount", "effects.chromatic.angle",
    "effects.wave.amount", "effects.wave.frequency",
    "effects.swirl.amount", "effects.swirl.radius",
    "effects.blur.amount", "effects.pixelate.size",
];

const KEYFRAME_BOOL_PATHS: &[&str] = &[
    "effects.chromatic.enabled", "effects.wave.enabled", "effects.swirl.enabled",
    "effects.blur.enabled", "effects.pixelate.enabled",
];

fn value_at_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut value = root;
    for part in path.split('.') {
        value = value.get(part)?;
    }
    Some(value)
}

fn set_value_at_path(root: &mut Value, path: &str, value: Value) {
    let parts: Vec<_> = path.split('.').collect();
    let mut cursor = root;
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !cursor[*part].is_object() { cursor[*part] = serde_json::json!({}); }
        cursor = &mut cursor[*part];
    }
    if let Some(last) = parts.last() { cursor[*last] = value; }
}

fn legacy_keyframe_field(path: &str) -> Option<&'static str> {
    match path {
        "transform.position.x" => Some("x"),
        "transform.position.y" => Some("y"),
        "transform.scale.w" => Some("scaleW"),
        "transform.scale.h" => Some("scaleH"),
        "transform.rotation" => Some("rotation"),
        "transform.opacity" => Some("opacity"),
        _ => None,
    }
}

fn keyframe_value<'a>(frame: &'a Value, path: &str) -> Option<&'a Value> {
    let values = &frame["values"];
    let snapshot = values.get("props").unwrap_or(values);
    value_at_path(snapshot, path).or_else(|| {
        let legacy = legacy_keyframe_field(path)?;
        values.get(legacy)
    })
}

fn resolve_keyframed_props(props: &Value, keyframes: &Value, time: f64) -> Value {
    let Some(frames) = keyframes.as_array().filter(|items| !items.is_empty()) else { return props.clone(); };
    let mut resolved = if props.is_object() { props.clone() } else { serde_json::json!({}) };

    for path in KEYFRAME_NUMERIC_PATHS {
        let mut points: Vec<(f64, f64)> = frames.iter().filter_map(|frame| {
            let at = frame["time"].as_f64().filter(|v| v.is_finite())?;
            let mut value = keyframe_value(frame, path)?.as_f64().filter(|v| v.is_finite())?;
            // Compatibility with older projects where opacity was stored as 0..1.
            if *path == "transform.opacity" && value > 0.0 && value <= 1.0 { value *= 100.0; }
            Some((at, value))
        }).collect();
        if points.is_empty() { continue; }
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points.dedup_by(|a, b| {
            if (a.0 - b.0).abs() < 1e-7 { a.1 = b.1; true } else { false }
        });
        let value = if time <= points[0].0 {
            points[0].1
        } else if time >= points[points.len() - 1].0 {
            points[points.len() - 1].1
        } else {
            let right = points.partition_point(|point| point.0 < time).min(points.len() - 1);
            let (t0, v0) = points[right - 1];
            let (t1, v1) = points[right];
            let amount = ((time - t0) / (t1 - t0).max(1e-9)).clamp(0.0, 1.0);
            v0 + (v1 - v0) * amount
        };
        set_value_at_path(&mut resolved, path, serde_json::json!(value));
    }

    // Effect enable switches are discrete/step-keyframed; numeric effect values
    // above interpolate smoothly. This keeps an effect from blending booleans.
    for path in KEYFRAME_BOOL_PATHS {
        let mut points: Vec<(f64, bool)> = frames.iter().filter_map(|frame| {
            let at = frame["time"].as_f64().filter(|v| v.is_finite())?;
            let value = keyframe_value(frame, path)?.as_bool()?;
            Some((at, value))
        }).collect();
        if points.is_empty() { continue; }
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        let value = points.iter().take_while(|point| point.0 <= time).last()
            .map(|point| point.1).unwrap_or(points[0].1);
        set_value_at_path(&mut resolved, path, serde_json::json!(value));
    }
    resolved
}

impl LayerUniforms {
    /// A layer that renders exactly like the old fit-and-blend shader.
    pub fn identity() -> Self {
        Self {
            size: [1.0, 1.0, 1.0, 1.0],
            geo0: [1.0, 1.0, 0.0, 1.0],
            geo1: [0.0, 0.0, 0.0, 0.0],
            crop: [0.0, 0.0, 0.0, 0.0],
            transition: [1.0, 0.0, 1.0, 0.0],
            distort: [0.0; 4],
            light: [0.0, 1.0, 1.0, 0.0],
            bal_s: [0.0; 4],
            bal_m: [0.0; 4],
            bal_h: [0.0; 4],
            hsl: [0.0, 0.0, 0.0, 0.0],
            fx0: [0.0, 2.5, 0.0, 0.0],
            fx1: [0.0, 0.0, 0.55, 0.5],
            fx2: [1.0, 0.0, 1.0, 0.0],
            fx3: [0.0, 0.0, 0.0, 1.0],
            fx4: [0.0, 0.0, 1.0, 0.0],
            fx5: [0.0, 1.0, 0.0, 0.0],
            fx6: [0.0, 1.0, 0.0, 0.5],
            fx7: [0.0, 0.0, 0.0, 0.5],
            fx8: [0.0, 0.0, 0.0, 0.0],
            fx9: [0.0, 1.0, 0.0, 0.75],
        }
    }

    pub fn opacity(&self) -> f32 {
        self.geo0[3]
    }

    pub fn with_sizes(mut self, src_w: u32, src_h: u32, target_w: u32, target_h: u32) -> Self {
        self.size = [src_w as f32, src_h as f32, target_w as f32, target_h as f32];
        self
    }

    /// `props` is `clip.extra["props"]` (or `Value::Null`); `time` is timeline seconds.
    pub fn from_props(props: &Value, time: f64) -> Self {
        Self::from_props_with_transition(props, time, None)
    }

    /// Resolves transform keyframes before mapping the normal property tree to
    /// uniforms. Keyframe times are absolute timeline seconds and values use
    /// x, y, scaleW, scaleH, rotation (degrees), and opacity (percent).
    pub fn from_props_with_keyframes(props: &Value, time: f64, keyframes: &Value) -> Self {
        Self::from_props_with_keyframes_and_transition(props, time, keyframes, None)
    }

    /// Resolves keyframes and applies an incoming transition in the same GPU
    /// uniform update. This keeps transition playback on the native compositor.
    pub fn from_props_with_keyframes_and_transition(
        props: &Value,
        time: f64,
        keyframes: &Value,
        transition_in: Option<(&str, f32)>,
    ) -> Self {
        Self::from_props_with_keyframes_and_transitions(props, time, keyframes, transition_in, None)
    }

    /// Resolves keyframes and applies incoming and outgoing transition state in
    /// one uniform update. Outgoing state is currently used for dip-to-black.
    pub fn from_props_with_keyframes_and_transitions(
        props: &Value,
        time: f64,
        keyframes: &Value,
        transition_in: Option<(&str, f32)>,
        transition_out: Option<(&str, f32)>,
    ) -> Self {
        let resolved = resolve_keyframed_props(props, keyframes, time);
        Self::from_props_with_transitions(&resolved, time, transition_in, transition_out)
    }

    /// Maps the active incoming transition along with the clip's visual properties.
    pub fn from_props_with_transition(
        props: &Value,
        time: f64,
        transition_in: Option<(&str, f32)>,
    ) -> Self {
        Self::from_props_with_transitions(props, time, transition_in, None)
    }

    /// Maps both sides of a transition. Kind 10 is an outgoing dip-to-black;
    /// it remains opaque while its RGB values are driven to black.
    pub fn from_props_with_transitions(
        props: &Value,
        time: f64,
        transition_in: Option<(&str, f32)>,
        transition_out: Option<(&str, f32)>,
    ) -> Self {
        let mut u = Self::identity();
        let transform = &props["transform"];
        let effects = &props["effects"];
        let colors = &props["colors"];
        let light = &colors["light"];
        let color = &colors["color"];
        let detail = &colors["detail"];

        let preset = effects["preset"]["id"].as_str().unwrap_or("none");
        let preset_intensity = (n(&effects["preset"], "intensity", 100.0) / 100.0).clamp(0.0, 1.0);

        // ------------------------------------------------------------ transform
        // Text PNGs are stored cropped to their editable text frame. Their
        // local layout transform is separate from the clip Transform tab, so
        // editing text never overwrites user-authored clip transforms.
        let text_layout = &props["text"]["layout"];
        let layout_scale_w = (n(&text_layout["scale"], "w", 100.0) / 100.0).clamp(0.01, 10.0);
        let layout_scale_h = (n(&text_layout["scale"], "h", 100.0) / 100.0).clamp(0.01, 10.0);
        let scale_w = (n(&transform["scale"], "w", 100.0) / 100.0 * layout_scale_w).clamp(0.01, 10.0);
        let scale_h = (n(&transform["scale"], "h", 100.0) / 100.0 * layout_scale_h).clamp(0.01, 10.0);
        let rotation = n(transform, "rotation", 0.0).to_radians();
        let opacity = (n(transform, "opacity", 100.0) / 100.0).clamp(0.0, 1.0);
        u.geo0 = [scale_w as f32, scale_h as f32, rotation as f32, opacity as f32];

        // Both local text-frame placement and the clip Transform position are
        // reference-pixel offsets from the composition centre.
        let mut pos_x = n(&transform["position"], "x", 0.0)
            + n(&text_layout["position"], "x", 0.0);
        let mut pos_y = n(&transform["position"], "y", 0.0)
            + n(&text_layout["position"], "y", 0.0);
        if on(effects, "shake") {
            let shake = &effects["shake"];
            let amount = n(shake, "amount", 18.0) * 0.18;
            let freq = (n(shake, "frequency", 8.0) * (1.0 - n(shake, "smoothness", 35.0) / 120.0)).max(0.5);
            let phase = std::f64::consts::TAU * time * freq;
            pos_x += phase.sin() * amount;
            pos_y += phase.cos() * amount;
        }
        u.geo1 = [(pos_x / 1920.0) as f32, (pos_y / 1080.0) as f32, flag(on(effects, "mirror")), time as f32];

        let crop = &transform["crop"];
        u.crop = [
            (n(crop, "left", 0.0) / 1920.0).clamp(0.0, 0.45) as f32,
            (n(crop, "right", 0.0) / 1920.0).clamp(0.0, 0.45) as f32,
            (n(crop, "top", 0.0) / 1080.0).clamp(0.0, 0.45) as f32,
            (n(crop, "bottom", 0.0) / 1080.0).clamp(0.0, 0.45) as f32,
        ];

        let distort = &transform["distort"];
        u.distort = [
            n(distort, "tl", 0.0) as f32,
            n(distort, "tr", 0.0) as f32,
            n(distort, "br", 0.0) as f32,
            n(distort, "bl", 0.0) as f32,
        ];

        if let Some((kind, progress)) = transition_in {
            let transition_kind = match kind {
                "fade" | "crossfade" | "dissolve" => 1.0,
                "wipe" | "wipeleft" | "wiperight" | "wipeup" | "wipedown"
                | "diagtl" | "diagtr" | "diagbl" | "diagbr" => 2.0,
                "iris" | "circleopen" | "circleclose" | "radial" => 3.0,
                "slide" | "slideleft" | "slideright" | "slideup" | "slidedown" => 4.0,
                "dip" | "fadeblack" => 5.0,
                "zoom" | "zoomin" => 6.0,
                "spin" => 7.0,
                "blur" | "hblur" => 8.0,
                "checker" | "pixelize" => 9.0,
                _ => 1.0,
            };
            let axis = match kind {
                "wipeup" | "slideup" => [0.0, 1.0],
                "wipedown" | "slidedown" => [0.0, -1.0],
                "wiperight" | "slideright" => [-1.0, 0.0],
                "diagtl" => [1.0, 1.0],
                "diagtr" => [-1.0, 1.0],
                "diagbl" => [1.0, -1.0],
                "diagbr" => [-1.0, -1.0],
                _ => [1.0, 0.0],
            };
            u.transition = [axis[0], axis[1], progress.clamp(0.0, 1.0), transition_kind];
        } else if let Some(("dip", progress)) = transition_out {
            u.transition = [1.0, 0.0, progress.clamp(0.0, 1.0), 10.0];
        }

        // ---------------------------------------------------------------- light
        let mut brightness = (n(light, "exposure", 0.0) * 0.5 + n(light, "brightness", 0.0) / 100.0 * 0.25).clamp(-1.0, 1.0);
        let mut contrast = (1.0 + n(light, "contrast", 0.0) / 100.0 + n(detail, "clarity", 0.0) / 180.0).clamp(0.0, 4.0);
        let mut saturation = (1.0 + n(color, "saturation", 0.0) / 100.0).clamp(0.0, 3.0);
        let vibrance = (n(color, "vibrance", 0.0) / 100.0).clamp(-1.0, 1.0);
        match preset {
            "vivid" => { saturation *= 1.0 + 0.7 * preset_intensity; contrast *= 1.08; }
            "noir" => { saturation *= 1.0 - preset_intensity; contrast *= 1.0 + 0.2 * preset_intensity; brightness -= 0.02 * preset_intensity; }
            "fade" => { contrast *= 1.0 - 0.16 * preset_intensity; brightness += 0.08 * preset_intensity; saturation *= 1.0 - 0.2 * preset_intensity; }
            "cinema" => { contrast *= 1.0 + 0.15 * preset_intensity; saturation *= 1.0 - 0.15 * preset_intensity; }
            "teal_orange" => { saturation *= 1.0 + 0.12 * preset_intensity; contrast *= 1.0 + 0.05 * preset_intensity; }
            "sepia" => { saturation *= 1.0 - 0.62 * preset_intensity; }
            "bleach" => { contrast *= 1.0 + 0.30 * preset_intensity; saturation *= 1.0 - 0.55 * preset_intensity; }
            "pastel" => { brightness += 0.045 * preset_intensity; contrast *= 1.0 - 0.12 * preset_intensity; saturation *= 1.0 - 0.10 * preset_intensity; }
            _ => {}
        }
        u.light = [brightness as f32, contrast as f32, saturation as f32, vibrance as f32];

        // ------------------------------------------------------------------ HSL
        let hsl_index = n(&colors["hsl"], "selected", 0.0).round().clamp(0.0, 8.0);
        let hsl_key = (hsl_index as usize).to_string();
        let hsl = &colors["hsl"][hsl_key.as_str()];
        u.hsl = [
            n(hsl, "hue", 0.0) as f32,
            (n(hsl, "saturation", 0.0) / 100.0).clamp(-1.0, 1.0) as f32,
            (n(hsl, "lightness", 0.0) / 200.0).clamp(-0.5, 0.5) as f32,
            hsl_index as f32,
        ];

        // ------------------------------------------- colour balance (wheels etc.)
        // Same maths as filters.rs: [shadows rgb, midtones rgb, highlights rgb].
        let mut balance = [0.0_f64; 9];
        let shadow_lift = ((n(light, "shadows", 0.0) + n(light, "blacks", 0.0)) / 100.0 * 0.2).clamp(-0.4, 0.4);
        let highlight_gain = ((n(light, "highlights", 0.0) + n(light, "whites", 0.0)) / 100.0 * 0.2).clamp(-0.4, 0.4);
        for channel in 0..3 { balance[channel] += shadow_lift; }
        for channel in 6..9 { balance[channel] += highlight_gain; }
        let temperature = (n(color, "temperature", 0.0) / 100.0).clamp(-1.0, 1.0);
        for band in [0usize, 3, 6] {
            balance[band] += temperature;
            balance[band + 2] -= temperature;
        }
        for (key, band) in [("shadows", 0usize), ("tint", 3usize), ("highlights", 6usize)] {
            if let Some(wheel) = colors["grading"][key].as_object() {
                let amount = wheel.get("amount").and_then(Value::as_f64).unwrap_or(0.0) / 100.0 * 0.3;
                let angle = wheel.get("hue").and_then(Value::as_f64).unwrap_or(0.0).to_radians();
                if amount.abs() > 0.001 {
                    balance[band] += amount * angle.cos();
                    balance[band + 1] += amount * (angle - std::f64::consts::TAU / 3.0).cos();
                    balance[band + 2] += amount * (angle + std::f64::consts::TAU / 3.0).cos();
                }
            }
        }
        match preset {
            "warm" => for band in [0usize, 3, 6] { balance[band] += 0.12 * preset_intensity; balance[band + 2] -= 0.10 * preset_intensity; },
            "cool" => for band in [0usize, 3, 6] { balance[band] -= 0.08 * preset_intensity; balance[band + 2] += 0.12 * preset_intensity; },
            "teal_orange" => {
                balance[0] -= 0.08 * preset_intensity; balance[1] += 0.025 * preset_intensity; balance[2] += 0.15 * preset_intensity;
                balance[3] += 0.025 * preset_intensity; balance[4] += 0.02 * preset_intensity;
                balance[6] += 0.14 * preset_intensity; balance[7] += 0.055 * preset_intensity; balance[8] -= 0.075 * preset_intensity;
            }
            "sepia" => for band in [0usize, 3, 6] { balance[band] += 0.18 * preset_intensity; balance[band + 1] += 0.065 * preset_intensity; balance[band + 2] -= 0.14 * preset_intensity; },
            "bleach" => { balance[6] += 0.035 * preset_intensity; balance[8] -= 0.025 * preset_intensity; },
            _ => {}
        }
        for value in &mut balance { *value = value.clamp(-1.0, 1.0); }
        u.bal_s = [balance[0] as f32, balance[1] as f32, balance[2] as f32, 0.0];
        u.bal_m = [balance[3] as f32, balance[4] as f32, balance[5] as f32, 0.0];
        u.bal_h = [balance[6] as f32, balance[7] as f32, balance[8] as f32, 0.0];

        // -------------------------------------------------------------- sharpen
        let mut sharpen = (n(detail, "sharpness", 0.0) / 100.0).clamp(0.0, 1.0);
        let mut sharpen_radius = 2.5;
        if on(effects, "sharpen") {
            let s = &effects["sharpen"];
            let radius = (n(s, "radius", 5.0).round() as i32).clamp(3, 23) | 1;
            let threshold = 1.0 - n(s, "threshold", 10.0).clamp(0.0, 100.0) / 125.0;
            sharpen += n(s, "amount", 45.0) / 100.0 * threshold;
            sharpen_radius = radius as f64 * 0.5;
        }

        // ----------------------------------------------------------------- blur
        let mut blur_mode = 0.0_f64;
        let mut blur_value = 0.0_f64;
        let blur_angle = 0.0_f64;
        if on(effects, "blur") {
            let b = &effects["blur"];
            // Gaussian blur is the single supported mode so preview and export
            // use the same kernel family and the same strength mapping.
            let amount = n(b, "amount", 0.0);
            blur_mode = 1.0;
            blur_value = (amount * 0.08).max(0.1);
        }
        u.fx0 = [sharpen.min(3.0) as f32, sharpen_radius as f32, blur_value as f32, blur_mode as f32];

        // ------------------------------------------------------------- vignette
        let mut vig_amount = n(detail, "vignette", 0.0) / 100.0 * 0.8;
        let (mut vig_mid, mut vig_feather, mut vig_aspect) = (0.55_f64, 0.5_f64, 1.0_f64);
        if on(effects, "vignette") {
            let v = &effects["vignette"];
            vig_amount += n(v, "amount", 0.0) / 100.0;
            vig_mid = (n(v, "midpoint", 50.0) / 100.0).clamp(0.0, 1.0);
            vig_feather = (n(v, "feather", 50.0) / 100.0).clamp(0.01, 1.0);
            vig_aspect = (1.0 + n(v, "roundness", 0.0) / 100.0 * 0.8).clamp(0.2, 2.0);
        }
        u.fx1 = [blur_angle as f32, vig_amount.clamp(-1.0, 1.0) as f32, vig_mid as f32, vig_feather as f32];

        // ------------------------------------------------------------- chromatic
        let mut chroma_px = 0.0_f64;
        let mut chroma_angle = 0.0_f64;
        if on(effects, "chromatic") {
            chroma_px = n(&effects["chromatic"], "amount", 0.0) * 0.06;
            chroma_angle = n(&effects["chromatic"], "angle", 0.0).to_radians();
        }
        u.fx2 = [vig_aspect as f32, 0.0, 1.0, chroma_px as f32];

        u.fx3 = [chroma_angle as f32, 0.0, 0.0, 1.0];

        // ------------------------------------------------- pixelate / posterize
        let mut pixelate = 0.0_f64;
        if on(effects, "pixelate") {
            let p = &effects["pixelate"];
            pixelate = (n(p, "size", 12.0) * n(p, "blend", 100.0) / 100.0).clamp(1.0, 80.0);
        }
        let (mut post_levels, mut post_blend) = (0.0_f64, 1.0_f64);
        if on(effects, "posterize") {
            let p = &effects["posterize"];
            let blend = n(p, "blend", 100.0).clamp(0.0, 100.0) / 100.0;
            if blend > 0.0 {
                post_levels = n(p, "levels", 6.0).clamp(2.0, 16.0);
                post_blend = blend;
            }
        }
        u.fx4 = [pixelate as f32, post_levels as f32, post_blend as f32, 0.0];

        // -------------------------------------------------------- scanlines
        let (mut scan_spacing, mut scan_amount) = (0.0_f64, 0.0_f64);
        if on(effects, "scanlines") {
            let s = &effects["scanlines"];
            scan_spacing = n(s, "spacing", 4.0).max(1.0);
            scan_amount = (n(s, "amount", 32.0) / 100.0).clamp(0.0, 1.0);
        }
        u.fx5 = [0.0, 1.0, scan_spacing as f32, scan_amount as f32];

        // ------------------------------------------------------ halftone
        let (mut half_size, mut half_contrast, mut half_angle) = (0.0_f64, 0.5_f64, 0.0_f64);
        if on(effects, "halftone") {
            let h = &effects["halftone"];
            half_size = n(h, "size", 5.0).clamp(1.0, 24.0) * 2.0;
            half_angle = n(h, "angle", 45.0).to_radians();
            half_contrast = n(h, "contrast", 48.0).clamp(0.0, 100.0) / 100.0;
        }
        u.fx6 = [0.0, 1.0, half_size as f32, half_contrast as f32];

        // ------------------------------------------------- film burn / light leak
        let (mut burn_amount, mut burn_warmth, mut burn_spread, mut burn_flicker) = (0.0_f64, 0.0_f64, 0.5_f64, 0.0_f64);
        if on(effects, "filmBurn") {
            let b = &effects["filmBurn"];
            burn_amount = n(b, "amount", 42.0) / 100.0 * 0.34;
            burn_warmth = n(b, "warmth", 70.0) / 100.0;
            burn_spread = (0.12 + n(b, "edge", 38.0) / 100.0 * 0.72).clamp(0.12, 0.84);
            burn_flicker = n(b, "flicker", 12.0) / 100.0 * 0.45;
        }
        u.fx7 = [half_angle as f32, burn_amount as f32, burn_warmth as f32, burn_spread as f32];
        let (mut leak_amount, mut leak_warmth, mut leak_position) = (0.0_f64, 0.0_f64, 0.0_f64);
        if on(effects, "lightLeak") {
            let l = &effects["lightLeak"];
            leak_amount = n(l, "amount", 40.0) / 100.0 * 0.5;
            leak_warmth = n(l, "warmth", 70.0) / 100.0;
            leak_position = n(l, "position", 35.0) / 100.0;
        }
        u.fx8 = [burn_flicker as f32, leak_amount as f32, leak_warmth as f32, leak_position as f32];

        // ------------------------------------------------------- Hanabi effects
        let wave = &effects["wave"];
        let wave_amount = if on(effects, "wave") {
            (n(wave, "amount", 0.0) / 100.0).clamp(0.0, 1.0)
        } else { 0.0 };
        let wave_frequency = n(wave, "frequency", 4.0).clamp(1.0, 16.0);
        let swirl = &effects["swirl"];
        let swirl_radians = if on(effects, "swirl") {
            (n(swirl, "amount", 0.0) / 100.0 * 6.283185307179586).clamp(0.0, 6.283185307179586)
        } else { 0.0 };
        let swirl_radius = (n(swirl, "radius", 70.0) / 100.0).clamp(0.10, 0.95);
        u.fx9 = [wave_amount as f32, wave_frequency as f32, swirl_radians as f32, swirl_radius as f32];
        u
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keyframes_interpolate_adjust_and_effect_values() {
        let props = json!({"colors":{"light":{"exposure":0.0}},"effects":{"wave":{"enabled":false,"amount":0.0}}});
        let frames = json!([
            {"time":0.0,"values":{"props":{"colors":{"light":{"exposure":-100.0}},"effects":{"wave":{"enabled":false,"amount":0.0}}}}},
            {"time":10.0,"values":{"props":{"colors":{"light":{"exposure":100.0}},"effects":{"wave":{"enabled":true,"amount":80.0}}}}}
        ]);
        let halfway = resolve_keyframed_props(&props, &frames, 5.0);
        assert!((halfway["colors"]["light"]["exposure"].as_f64().unwrap() - 0.0).abs() < 1e-6);
        assert!((halfway["effects"]["wave"]["amount"].as_f64().unwrap() - 40.0).abs() < 1e-6);
        assert!(!halfway["effects"]["wave"]["enabled"].as_bool().unwrap());
        let end = resolve_keyframed_props(&props, &frames, 10.0);
        assert!(end["effects"]["wave"]["enabled"].as_bool().unwrap());
    }

    #[test]
    fn missing_props_are_neutral() {
        assert_eq!(LayerUniforms::from_props(&Value::Null, 0.0), LayerUniforms::identity());
    }

    #[test]
    fn transform_values_reach_the_uniforms() {
        let props = json!({"transform": {
            "scale": {"w": 50.0, "h": 200.0}, "rotation": 90.0, "opacity": 25.0,
            "position": {"x": 192.0, "y": -108.0}, "crop": {"left": 192.0, "bottom": 108.0}
        }});
        let u = LayerUniforms::from_props(&props, 0.0);
        assert_eq!(u.geo0[0], 0.5);
        assert_eq!(u.geo0[1], 2.0);
        assert!((u.geo0[2] - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert_eq!(u.opacity(), 0.25);
        assert!((u.geo1[0] - 0.1).abs() < 1e-6);
        assert!((u.geo1[1] + 0.1).abs() < 1e-6);
        assert!((u.crop[0] - 0.1).abs() < 1e-6);
        assert!((u.crop[3] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn cropped_title_layout_composes_with_clip_transform() {
        let props = json!({
            "transform": {
                "position": {"x": 10.0, "y": -20.0},
                "scale": {"w": 200.0, "h": 150.0},
                "rotation": 0.0,
                "opacity": 100.0
            },
            "text": {
                "isTitle": true,
                "layout": {
                    "position": {"x": 400.0, "y": 120.0},
                    "scale": {"w": 76.0, "h": 60.0}
                }
            }
        });
        let uniforms = LayerUniforms::from_props(&props, 0.0);

        // The cropped text frame supplies its own placement/size while the
        // clip Transform tab remains additive/multiplicative on top of it.
        assert!((uniforms.geo0[0] - 1.52).abs() < 1e-5);
        assert!((uniforms.geo0[1] - 0.90).abs() < 1e-5);
        assert!((uniforms.geo1[0] - (410.0 / 1920.0)).abs() < 1e-6);
        assert!((uniforms.geo1[1] - (100.0 / 1080.0)).abs() < 1e-6);
    }

    #[test]
    fn expanded_filter_looks_reach_the_live_preview_uniforms() {
        let cases = [
            ("teal_orange", [0.0_f32, 1.05, 1.12, 0.0]),
            ("sepia", [0.0_f32, 1.0, 0.38, 0.0]),
            ("bleach", [0.0_f32, 1.30, 0.45, 0.0]),
            ("pastel", [0.045_f32, 0.88, 0.90, 0.0]),
        ];
        for (preset, expected) in cases {
            let props = json!({"effects": {"preset": {"id": preset, "intensity": 100.0}}});
            let uniforms = LayerUniforms::from_props(&props, 0.0);
            for channel in 0..4 {
                assert!((uniforms.light[channel] - expected[channel]).abs() < 1e-5,
                        "unexpected {preset} preview channel {channel}: {:?}", uniforms.light);
            }
        }
        let teal_orange = LayerUniforms::from_props(
            &json!({"effects": {"preset": {"id": "teal_orange", "intensity": 100.0}}}), 0.0);
        assert!((teal_orange.bal_s[2] - 0.15).abs() < 1e-5);
        assert!((teal_orange.bal_h[0] - 0.14).abs() < 1e-5);
    }

    #[test]
    fn keyframes_interpolate_transform_uniforms_on_the_native_preview_path() {
        let props = json!({"transform": {"position": {"x": 0.0, "y": 0.0},
            "scale": {"w": 100.0, "h": 100.0}, "rotation": 0.0, "opacity": 100.0}});
        let keyframes = json!([
            {"time": 1.0, "values": {"x": 0.0, "y": 100.0, "scaleW": 100.0, "scaleH": 100.0, "rotation": 0.0, "opacity": 100.0}},
            {"time": 3.0, "values": {"x": 960.0, "y": -100.0, "scaleW": 200.0, "scaleH": 50.0, "rotation": 90.0, "opacity": 40.0}}
        ]);
        let u = LayerUniforms::from_props_with_keyframes(&props, 2.0, &keyframes);
        assert!((u.geo1[0] - 0.25).abs() < 1e-5, "position X halfway");
        assert!((u.geo1[1] - 0.0).abs() < 1e-5, "position Y halfway");
        assert!((u.geo0[0] - 1.5).abs() < 1e-5, "scale X halfway");
        assert!((u.geo0[1] - 0.75).abs() < 1e-5, "scale Y halfway");
        assert!((u.geo0[2] - std::f32::consts::FRAC_PI_4).abs() < 1e-5, "rotation halfway");
        assert!((u.opacity() - 0.70).abs() < 1e-5, "opacity halfway");
        let before = LayerUniforms::from_props_with_keyframes(&props, 0.0, &keyframes);
        let after = LayerUniforms::from_props_with_keyframes(&props, 4.0, &keyframes);
        assert_eq!(before.geo0[0], 1.0);
        assert!((after.geo0[0] - 2.0).abs() < 1e-5);
    }

    #[test]
    fn crop_uniforms_clamp_negative_and_overwide_edges() {
        let props = json!({"transform": {"crop": {"left": -800.0, "right": 5000.0, "top": -30.0, "bottom": 5000.0}}});
        let uniforms = LayerUniforms::from_props(&props, 0.0);
        assert_eq!(uniforms.crop, [0.0, 0.45, 0.0, 0.45]);
    }

    #[test]
    fn keyframed_incoming_transition_is_mapped_to_native_uniforms() {
        let props = json!({"transform": {"opacity": 80.0}});
        let keyframes = json!([{"time": 0.0, "values": {"opacity": 60.0}}]);
        let u = LayerUniforms::from_props_with_keyframes_and_transition(
            &props, 0.0, &keyframes, Some(("wipeleft", 0.35))
        );
        assert!((u.opacity() - 0.60).abs() < 1e-5);
        // `wipeleft` reveals from left to right (positive X); `wiperight`
        // uses the opposite axis and is covered by the transition mapping test below.
        assert_eq!(u.transition, [1.0, 0.0, 0.35, 2.0]);
    }

    #[test]
    fn legacy_fractional_keyframe_opacity_is_read_as_a_fraction() {
        let frames = json!([{"time": 0.0, "values": {"opacity": 0.25}}]);
        let u = LayerUniforms::from_props_with_keyframes(&Value::Null, 0.0, &frames);
        assert!((u.opacity() - 0.25).abs() < 1e-5);
    }

    #[test]
    fn colour_and_effect_values_reach_the_uniforms() {
        let props = json!({
            "colors": {"light": {"exposure": 1.0, "contrast": 50.0}, "color": {"saturation": -100.0, "vibrance": 40.0},
                       "grading": {"shadows": {"hue": 0.0, "amount": 100.0}},
                       "hsl": {"selected": 3, "3": {"hue": 20.0, "saturation": 50.0, "lightness": 20.0}}},
            "effects": {"blur": {"enabled": true, "amount": 100.0, "mode": "gaussian"},
                        "pixelate": {"enabled": true, "size": 20.0, "blend": 100.0},
                        "mirror": {"enabled": true}}
        });
        let u = LayerUniforms::from_props(&props, 0.0);
        assert!((u.light[0] - 0.5).abs() < 1e-6);
        assert!((u.light[1] - 1.5).abs() < 1e-6);
        assert_eq!(u.light[2], 0.0);
        assert!((u.light[3] - 0.4).abs() < 1e-6);
        assert!(u.bal_s[0] > 0.2);
        assert_eq!(u.hsl[3], 3.0);
        assert_eq!(u.hsl[0], 20.0);
        assert_eq!(u.fx0[3], 1.0);
        assert!((u.fx0[2] - 8.0).abs() < 1e-5);
        assert_eq!(u.fx4[0], 20.0);
        assert_eq!(u.geo1[2], 1.0);
    }

    #[test]
    fn retained_effects_reach_the_preview_uniforms() {
        let props = json!({"effects": {
            "chromatic": {"enabled": true, "amount": 50.0, "angle": 90.0},
            "pixelate": {"enabled": true, "size": 20.0, "blend": 50.0},
            "sharpen": {"enabled": true, "amount": 45.0, "radius": 5.0, "threshold": 10.0},
            "vignette": {"enabled": true, "amount": 50.0, "midpoint": 50.0, "roundness": 0.0, "feather": 65.0},
            "posterize": {"enabled": true, "levels": 6.0, "blend": 75.0},
            "scanlines": {"enabled": true, "spacing": 4.0, "amount": 32.0},
            "halftone": {"enabled": true, "size": 5.0, "angle": 45.0, "contrast": 48.0},
            "filmBurn": {"enabled": true, "amount": 42.0, "warmth": 70.0, "edge": 38.0, "flicker": 12.0},
            "lightLeak": {"enabled": true, "amount": 40.0, "warmth": 70.0, "position": 35.0},
            "mirror": {"enabled": true},
            "shake": {"enabled": true, "amount": 18.0, "frequency": 8.0, "smoothness": 35.0}
        }});
        let u = LayerUniforms::from_props(&props, 0.25);
        assert!((u.fx2[3] - 3.0).abs() < 1e-5);
        assert!((u.fx3[0] - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((u.fx4[0] - 10.0).abs() < 1e-5);
        assert!((u.fx4[1] - 6.0).abs() < 1e-5);
        assert!((u.fx5[2] - 4.0).abs() < 1e-5);
        assert!((u.fx6[2] - 10.0).abs() < 1e-5);
        assert!(u.fx7[1] > 0.0);
        assert!(u.fx8[1] > 0.0);
        assert_eq!(u.geo1[2], 1.0);
        assert_ne!(u.geo1[0], 0.0);
        assert!(u.fx0[0] > 0.0);
        assert!(u.fx1[1] > 0.0);
    }

    #[test]
    fn retired_effects_do_not_change_preview_uniforms() {
        let neutral = LayerUniforms::from_props(&Value::Null, 0.5);
        let legacy = json!({"effects": {
            "grain": {"enabled": true, "amount": 80.0},
            "lens": {"enabled": true, "amount": 100.0},
            "fisheye": {"enabled": true, "amount": 100.0},
            "glitch": {"enabled": true, "amount": 100.0},
            "glow": {"enabled": true, "amount": 100.0, "radius": 100.0}
        }});
        let actual = LayerUniforms::from_props(&legacy, 0.5);
        assert_eq!(actual.fx2[1], neutral.fx2[1]);
        assert_eq!(actual.fx3[1], neutral.fx3[1]);
        assert_eq!(actual.fx3[2], neutral.fx3[2]);
        assert_eq!(actual.fx5, neutral.fx5);
        assert_eq!(actual.fx6[0..2], neutral.fx6[0..2]);
    }


    #[test]
    fn new_transition_kinds_map_to_distinct_gpu_modes() {
        let props = serde_json::json!({});
        let expected = [("zoom", 6.0), ("spin", 7.0), ("blur", 8.0), ("checker", 9.0)];
        for (name, mode) in expected {
            let uniforms = LayerUniforms::from_props_with_transition(&props, 0.0, Some((name, 0.4)));
            assert_eq!(uniforms.transition[2], 0.4);
            assert_eq!(uniforms.transition[3], mode, "unexpected transition mode for {name}");
        }
    }

    #[test]
    fn dip_to_black_has_separate_incoming_and_outgoing_shader_modes() {
        let props = serde_json::json!({});
        let incoming = LayerUniforms::from_props_with_transition(&props, 0.0, Some(("dip", 0.6)));
        let outgoing = LayerUniforms::from_props_with_transitions(&props, 0.0, None, Some(("dip", 0.4)));
        assert_eq!(incoming.transition[3], 5.0);
        assert_eq!(outgoing.transition[3], 10.0);
    }

    #[test]
    fn transition_and_corner_distort_values_reach_the_uniforms() {
        let props = json!({"transform": {"distort": {"tl": 10.0, "tr": 20.0, "br": 30.0, "bl": 40.0}}});
        let u = LayerUniforms::from_props_with_transition(&props, 0.0, Some(("wiperight", 0.5)));
        assert_eq!(u.distort, [10.0, 20.0, 30.0, 40.0]);
        assert_eq!(u.transition, [-1.0, 0.0, 0.5, 2.0]);
    }

    #[test]
    fn shake_moves_the_layer_over_time() {
        let props = json!({"effects": {"shake": {"enabled": true, "amount": 100.0, "frequency": 1.0, "smoothness": 0.0}}});
        let a = LayerUniforms::from_props(&props, 0.0);
        let b = LayerUniforms::from_props(&props, 0.25);
        assert_ne!(a.geo1[0], b.geo1[0]);
    }
}
