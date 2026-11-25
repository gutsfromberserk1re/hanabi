use serde_json::Value;

fn number(value: &Value, key: &str, fallback: f64) -> f64 { value[key].as_f64().filter(|n| n.is_finite()).unwrap_or(fallback) }
fn enabled(value: &Value, key: &str) -> bool { value[key]["enabled"].as_bool().unwrap_or(false) }
fn n(value: &Value, key: &str, fallback: f64) -> f64 { number(value, key, fallback) }

/// Build an FFmpeg expression for a transform channel using the same linear,
/// clamped interpolation as the native WGPU preview. Keyframe timestamps are
/// absolute timeline seconds (the video PTS is shifted to that same clock).
fn keyframed_value_expr(clip: &Value, field: &str) -> Option<String> {
    keyframed_value_expr_at(clip, field, "t")
}

fn keyframed_value_expr_at(clip: &Value, field: &str, time_variable: &str) -> Option<String> {
    let frames = clip["keyframes"].as_array()?;
    let mut points: Vec<(f64, f64)> = frames.iter().filter_map(|frame| {
        let time = frame["time"].as_f64().filter(|v| v.is_finite())?;
        let mut value = frame["values"][field].as_f64().filter(|v| v.is_finite())?;
        // Compatibility with older saved projects where opacity was 0..1.
        if field == "opacity" && value > 0.0 && value <= 1.0 { value *= 100.0; }
        Some((time, value))
    }).collect();
    if points.is_empty() { return None; }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points.dedup_by(|a, b| {
        if (a.0 - b.0).abs() < 1e-7 { a.1 = b.1; true } else { false }
    });
    if points.len() == 1 { return Some(format!("{:.6}", points[0].1)); }

    let mut expression = format!("{:.6}", points[points.len() - 1].1);
    for pair in points.windows(2).rev() {
        let (t0, v0) = pair[0];
        let (t1, v1) = pair[1];
        let span = (t1 - t0).max(1e-9);
        let interpolated = format!("({v0:.6}+({:.6})*({time_variable}-{t0:.6})/{span:.6})", v1 - v0);
        expression = format!("if(lt({time_variable},{t1:.6}),{interpolated},{expression})");
    }
    let (first_time, first_value) = points[0];
    Some(format!("if(lt({time_variable},{first_time:.6}),{first_value:.6},{expression})"))
}
pub(crate) fn video_filters(clip: &Value, width: u32, height: u32, start: f64, length: f64, transition_in: Option<(&str, f64)>, transition_out: Option<(&str, f64)>) -> (String, String, String) {
    let props = &clip["props"];
    let transform = &props["transform"];
    let effects = &props["effects"];
    let colors = &props["colors"];
    let detail = &colors["detail"];
    let source_start = n(clip, "sourceOffset", 0.0).max(0.0);
    let mut filters = vec![format!("trim=start={source_start:.5}:duration={length:.5}"), format!("setpts=PTS-STARTPTS+{start:.5}/TB"),
        format!("scale={width}:{height}:force_original_aspect_ratio=decrease"), "setsar=1".into(), "format=rgba".into()];

    let crop = &transform["crop"];
    let left = (n(crop, "left", 0.0) / 1920.0).clamp(0.0, 0.45);
    let right = (n(crop, "right", 0.0) / 1920.0).clamp(0.0, 0.45);
    let top = (n(crop, "top", 0.0) / 1080.0).clamp(0.0, 0.45);
    let bottom = (n(crop, "bottom", 0.0) / 1080.0).clamp(0.0, 0.45);
    let keep_w = (1.0 - left - right).max(0.1);
    let keep_h = (1.0 - top - bottom).max(0.1);
    if left + right > 0.0 || top + bottom > 0.0 {
        // Crop the fitted source image, then place the remainder back at its
        // original canvas position. Reconstruct the pre-crop dimensions in
        // the pad expressions so letterboxed/pillarboxed media is treated the
        // same way as the WGPU preview. Never enlarge the remaining picture.
        filters.push(format!(
            "crop=iw*{keep_w:.6}:ih*{keep_h:.6}:iw*{left:.6}:ih*{top:.6},pad={width}:{height}:(ow-iw/{keep_w:.6})/2+iw*{left:.6}/{keep_w:.6}:(oh-ih/{keep_h:.6})/2+ih*{top:.6}/{keep_h:.6}:color=black@0"
        ));
    } else {
        filters.push(format!("pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black@0"));
    }

    // Pre-grade image effects: same order and reference-pixel units as WGSL preview.
    if enabled(effects, "mirror") { filters.push("hflip".into()); }
    let blur_sigma = if enabled(effects, "blur") { (n(&effects["blur"], "amount", 0.0) * 0.08).max(0.1) } else { 0.0 };
    if blur_sigma > 0.25 { filters.push(format!("gblur=sigma={blur_sigma:.4}:steps=2")); }

    if enabled(effects, "pixelate") {
        let pixel = &effects["pixelate"];
        let block = (n(pixel, "size", 12.0) * n(pixel, "blend", 100.0) / 100.0).clamp(1.0, 80.0) * (height as f64 / 1080.0);
        filters.push(format!("scale={}:{}:flags=neighbor,scale={width}:{height}:flags=neighbor", (width as f64 / block).max(1.0) as u32, (height as f64 / block).max(1.0) as u32));
    }
    if enabled(effects, "chromatic") {
        let amount = n(&effects["chromatic"], "amount", 0.0) * 0.06 * (height as f64 / 1080.0);
        let angle = n(&effects["chromatic"], "angle", 0.0).to_radians();
        filters.push(format!("rgbashift=rh={:.3}:rv={:.3}:bh={:.3}:bv={:.3}", amount * angle.cos(), amount * angle.sin(), -amount * angle.cos(), -amount * angle.sin()));
    }
    let mut sharpen_amount = (n(detail, "sharpness", 0.0) / 100.0).clamp(0.0, 1.0);
    let mut sharpen_radius = 2.5;
    if enabled(effects, "sharpen") {
        let sh = &effects["sharpen"];
        let radius = (n(sh, "radius", 5.0).round() as i32).clamp(3, 23) | 1;
        let threshold = 1.0 - n(sh, "threshold", 10.0).clamp(0.0, 100.0) / 125.0;
        sharpen_amount += n(sh, "amount", 45.0) / 100.0 * threshold;
        sharpen_radius = radius as f64 * 0.5;
    }
    sharpen_amount = sharpen_amount.clamp(0.0, 3.0);
    if sharpen_amount > 0.001 {
        let rad = sharpen_radius * height as f64 / 1080.0;
        let blur = format!("({a}+{b}+{c}+{d})/4",
            a=format!("r(min(max(X+{rad:.4},0),W-1),Y)"),
            b=format!("r(min(max(X-{rad:.4},0),W-1),Y)"),
            c=format!("r(X,min(max(Y+{rad:.4},0),H-1))"),
            d=format!("r(X,min(max(Y-{rad:.4},0),H-1))"));
        let ch = |name: &str| format!("{name}(X,Y)+({name}(X,Y)-{})*{sharpen_amount:.5}", blur.replace("r(", &format!("{name}(")));
        filters.push(format!("geq=r='{}':g='{}':b='{}':a='alpha(X,Y)'", ch("r"), ch("g"), ch("b")));
    }

    let light = &colors["light"];
    let color = &colors["color"];
    let brightness = (n(light, "exposure", 0.0) * 0.5 + n(light, "brightness", 0.0) / 100.0 * 0.25).clamp(-1.0, 1.0);
    let contrast = (1.0 + n(light, "contrast", 0.0) / 100.0 + n(detail, "clarity", 0.0) / 180.0).clamp(0.0, 4.0);
    let saturation = (1.0 + n(color, "saturation", 0.0) / 100.0).clamp(0.0, 3.0);
    if brightness.abs() > 0.0001 || (contrast - 1.0).abs() > 0.0001 || (saturation - 1.0).abs() > 0.0001 {
        filters.push(format!("eq=brightness={brightness:.5}:contrast={contrast:.5}:saturation={saturation:.5}"));
    }
    let vibrance = (n(color, "vibrance", 0.0) / 100.0).clamp(-1.0, 1.0);
    if vibrance.abs() > 0.001 { filters.push(format!("vibrance=intensity={vibrance:.4}")); }

    let hsl_index = n(&colors["hsl"], "selected", 0.0).round().clamp(0.0, 8.0) as usize;
    let hsl_key = hsl_index.to_string();
    let hsl = &colors["hsl"][hsl_key.as_str()];
    let hsl_hue = n(hsl, "hue", 0.0);
    let hsl_saturation = (n(hsl, "saturation", 0.0) / 100.0).clamp(-1.0, 1.0);
    let hsl_intensity = (n(hsl, "lightness", 0.0) / 200.0).clamp(-0.5, 0.5);
    let hsl_range = ["a", "r", "r+y", "y", "g", "c", "b", "b+m", "m+r"][hsl_index];
    if hsl_hue.abs() > 0.001 || hsl_saturation.abs() > 0.001 || hsl_intensity.abs() > 0.001 {
        filters.push(format!("huesaturation=colors={hsl_range}:hue={hsl_hue:.3}:saturation={hsl_saturation:.4}:intensity={hsl_intensity:.4}:strength=100:lightness=1"));
    }

    /* Export-side color balance; preview evaluates the matching controls in WGSL below. */
    let preset = effects["preset"]["id"].as_str().unwrap_or("none");
    let intensity = (n(&effects["preset"], "intensity", 100.0) / 100.0).clamp(0.0, 1.0);
    let mut balance = [0.0_f64; 9]; // shadows RGB, midtones RGB, highlights RGB
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
        if let Some(wheel_data) = colors["grading"][key].as_object() {
            let amount = wheel_data.get("amount").and_then(Value::as_f64).unwrap_or(0.0) / 100.0 * 0.3;
            let angle = wheel_data.get("hue").and_then(Value::as_f64).unwrap_or(0.0).to_radians();
            if amount.abs() > 0.001 {
                balance[band] += amount * angle.cos();
                balance[band + 1] += amount * (angle - std::f64::consts::TAU / 3.0).cos();
                balance[band + 2] += amount * (angle + std::f64::consts::TAU / 3.0).cos();
            }
        }
    }

    match preset {
        "warm" => for band in [0usize, 3, 6] { balance[band] += 0.12 * intensity; balance[band + 2] -= 0.10 * intensity; },
        "cool" => for band in [0usize, 3, 6] { balance[band] -= 0.08 * intensity; balance[band + 2] += 0.12 * intensity; },
        "teal_orange" => {
            balance[0] -= 0.08 * intensity; balance[1] += 0.025 * intensity; balance[2] += 0.15 * intensity;
            balance[3] += 0.025 * intensity; balance[4] += 0.02 * intensity;
            balance[6] += 0.14 * intensity; balance[7] += 0.055 * intensity; balance[8] -= 0.075 * intensity;
        }
        "sepia" => for band in [0usize, 3, 6] { balance[band] += 0.18 * intensity; balance[band + 1] += 0.065 * intensity; balance[band + 2] -= 0.14 * intensity; },
        "bleach" => { balance[6] += 0.035 * intensity; balance[8] -= 0.025 * intensity; },
        _ => {}
    }
    if balance.iter().any(|value| value.abs() > 0.001) {
        for value in &mut balance { *value = (*value).clamp(-1.0, 1.0); }
        filters.push(format!("colorbalance=rs={:.4}:gs={:.4}:bs={:.4}:rm={:.4}:gm={:.4}:bm={:.4}:rh={:.4}:gh={:.4}:bh={:.4}",
            balance[0], balance[1], balance[2], balance[3], balance[4], balance[5], balance[6], balance[7], balance[8]));
    }

    // The vignette uses the same radial smoothstep as preview.wgsl.
    let mut vignette_amount = (n(detail, "vignette", 0.0) / 100.0 * 0.8).clamp(-1.0, 1.0);
    let (mut vignette_mid, mut vignette_feather, mut vignette_aspect) = (0.55_f64, 0.5_f64, 1.0_f64);
    if enabled(effects, "vignette") {
        let v = &effects["vignette"];
        vignette_amount += n(v, "amount", 0.0) / 100.0;
        vignette_mid = (n(v, "midpoint", 50.0) / 100.0).clamp(0.0, 1.0);
        vignette_feather = (n(v, "feather", 50.0) / 100.0).clamp(0.01, 1.0);
        vignette_aspect = (1.0 + n(v, "roundness", 0.0) / 100.0 * 0.8).clamp(0.2, 2.0);
    }
    vignette_amount = vignette_amount.clamp(-1.0, 1.0);
    if vignette_amount.abs() > 0.001 {
        let radial = format!("sqrt(pow(((X/max(W-1,1))-0.5)*{:.6}*2,2)+pow(((Y/max(H-1,1))-0.5)*2,2))*0.7071", vignette_aspect);
        let t = format!("max(0,min(1,(({radial}-({:.6}*0.9))/(max({:.6},0.01)*0.8))))", vignette_mid, vignette_feather);
        let mask = format!("(({t})*({t})*(3-2*({t})))");
        let apply = |c: &str| if vignette_amount > 0.0 {
            format!("{c}(X,Y)*(1-{:.6}*{mask})", vignette_amount)
        } else {
            format!("{c}(X,Y)+(255-{c}(X,Y))*{:.6}*{mask}", -vignette_amount)
        };
        filters.push(format!("geq=r='{}':g='{}':b='{}':a='alpha(X,Y)'", apply("r"), apply("g"), apply("b")));
    }

    // Posterize and halftone use the same quantization / dot-grid equations as preview.
    if enabled(effects, "posterize") {
        let posterize = &effects["posterize"];
        let blend = n(posterize, "blend", 100.0).clamp(0.0, 100.0) / 100.0;
        let target = n(posterize, "levels", 6.0).clamp(2.0, 16.0);
        if blend > 0.0 {
            let steps = target - 1.0;
            let quantize = format!("(val*{:.5}+floor(val*{steps:.5}/255+0.5)*255/{steps:.5}*{blend:.5})", 1.0 - blend);
            filters.push(format!("lutrgb=r='{quantize}':g='{quantize}':b='{quantize}'"));
        }
    }
    if enabled(effects, "halftone") {
        let halftone = &effects["halftone"];
        let size = n(halftone, "size", 5.0).clamp(1.0, 24.0) * 2.0;
        let angle = n(halftone, "angle", 45.0).to_radians();
        let contrast = n(halftone, "contrast", 48.0).clamp(0.0, 100.0) / 100.0;
        let px = format!("mod(X*{:.5}+Y*{:.5}+{:.3},{size:.3})-{:.3}", angle.cos(), angle.sin(), size / 2.0, size / 2.0);
        let py = format!("mod(-X*{:.5}+Y*{:.5}+{:.3},{size:.3})-{:.3}", angle.sin(), angle.cos(), size / 2.0, size / 2.0);
        let radius = format!("(1-(0.2126*r(X,Y)+0.7152*g(X,Y)+0.0722*b(X,Y))/255)*{:.3}*{contrast:.3}", size / 2.0);
        filters.push(format!("geq=r='if(lt(hypot({px},{py}),{radius}),0,r(X,Y))':g='if(lt(hypot({px},{py}),{radius}),0,g(X,Y))':b='if(lt(hypot({px},{py}),{radius}),0,b(X,Y))':a='alpha(X,Y)'"));
    }
    if enabled(effects, "scanlines") {
        let scan = &effects["scanlines"];
        let scale = height as f64 / 1080.0;
        let spacing = n(scan, "spacing", 4.0).max(1.0);
        let amount = (n(scan, "amount", 32.0) / 100.0).clamp(0.0, 1.0);
        let row = format!("floor(Y/{scale:.6})");
        let remainder = format!("(({row})-{spacing:.5}*floor(({row})/{spacing:.5}))");
        let apply = |channel: &str| format!("if(lt({remainder},1),{channel}(X,Y)*{:.5},{channel}(X,Y))", 1.0 - amount);
        filters.push(format!("geq=r='{}':g='{}':b='{}':a='alpha(X,Y)'", apply("r"), apply("g"), apply("b")));
    }
    if enabled(effects, "filmBurn") {
        let burn = &effects["filmBurn"];
        let amount = n(burn, "amount", 42.0) / 100.0 * 0.34;
        let warmth = n(burn, "warmth", 70.0) / 100.0;
        let spread = (0.12 + n(burn, "edge", 38.0) / 100.0 * 0.72).clamp(0.12, 0.84);
        let flicker = n(burn, "flicker", 12.0) / 100.0 * 0.45;
        let edge = format!("(max(0,(abs(X-W/2)/(W/2)-{:.4})/{spread:.4})*(1+sin(T*8*PI)*{flicker:.4}))", 1.0 - spread);
        filters.push(format!("geq=r='min(255,r(X,Y)+255*{:.4}*{edge})':g='min(255,g(X,Y)+255*{:.4}*{edge})':b='min(255,b(X,Y)+255*{:.4}*{edge})':a='alpha(X,Y)'", amount * warmth, amount * 0.28, amount * 0.06));
    }
    if enabled(effects, "lightLeak") {
        let leak = &effects["lightLeak"];
        let amount = n(leak, "amount", 40.0) / 100.0 * 0.5;
        let warmth = n(leak, "warmth", 70.0) / 100.0;
        let position = n(leak, "position", 35.0) / 100.0;
        let radius = (0.14 + warmth * 0.38).clamp(0.14, 0.52);
        let hotspot = format!("max(0,1-abs(X/max(W-1,1)-{position:.4})/{radius:.4})");
        filters.push(format!("geq=r='min(255,r(X,Y)+255*{:.4}*{hotspot})':g='min(255,g(X,Y)+255*{:.4}*{hotspot})':b='min(255,b(X,Y)+255*{:.4}*{hotspot})':a='alpha(X,Y)'", amount, amount * (0.25 + warmth * 0.2), amount * (1.0 - warmth) * 0.25));
    }

    if let Some((kind, transition_duration)) = transition_in {
        let d = transition_duration.clamp(0.05, length.max(0.05));
        if kind == "dip" {
            filters.push(format!("fade=t=in:st={:.5}:d={:.5}", start + d / 2.0, (d / 2.0).max(0.025)));
        } else if kind == "wipe" {
            filters.push(format!("geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='alpha(X,Y)*if(lte(X/W,min(max((T-{start:.5})/{d:.5},0),1)),1,0)'"));
        } else if kind == "iris" {
            filters.push(format!("geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='alpha(X,Y)*if(lte(hypot(X-W/2,Y-H/2),min(W,H)*0.5*min(max((T-{start:.5})/{d:.5},0),1)),1,0)'"));
        } else if kind == "slide" {
            /* Slide motion is applied by the shared layer-composite x expression. */
        } else {
            filters.push(format!("fade=t=in:st={start:.5}:d={d:.5}:alpha=1"));
        }
    }
    if let Some((kind, transition_duration)) = transition_out {
        let d = transition_duration.clamp(0.05, length.max(0.05));
        if kind == "dip" { filters.push(format!("fade=t=out:st={:.5}:d={:.5}", start + length - d, (d / 2.0).max(0.025))); }
        filters.push(format!("tpad=stop_mode=clone:stop_duration={d:.5}"));
    }

    let distort = &transform["distort"];
    let tl = n(distort, "tl", 0.0) * width as f64 / 1920.0;
    let tr = n(distort, "tr", 0.0) * width as f64 / 1920.0;
    let br = n(distort, "br", 0.0) * width as f64 / 1920.0;
    let bl = n(distort, "bl", 0.0) * width as f64 / 1920.0;
    if tl.abs() + tr.abs() + br.abs() + bl.abs() > 0.01 {
        filters.push(format!("perspective=x0={tl:.3}:y0={tl:.3}:x1={:.3}:y1={tr:.3}:x2={:.3}:y2={:.3}:x3={bl:.3}:y3={:.3}:sense=source:eval=frame", width as f64 - tr, width as f64 - br, height as f64 - br, height as f64 - bl));
    }

    let scale_w = (n(&transform["scale"], "w", 100.0) / 100.0).clamp(0.01, 10.0);
    let scale_h = (n(&transform["scale"], "h", 100.0) / 100.0).clamp(0.01, 10.0);
    let animated_scale_w = keyframed_value_expr(clip, "scaleW");
    let animated_scale_h = keyframed_value_expr(clip, "scaleH");
    if animated_scale_w.is_some() || animated_scale_h.is_some() {
        let w_expr = animated_scale_w.unwrap_or_else(|| format!("{:.6}", scale_w * 100.0));
        let h_expr = animated_scale_h.unwrap_or_else(|| format!("{:.6}", scale_h * 100.0));
        filters.push(format!("scale=w='iw*({w_expr})/100':h='ih*({h_expr})/100':eval=frame"));
    } else if (scale_w - 1.0).abs() > 0.0001 || (scale_h - 1.0).abs() > 0.0001 {
        filters.push(format!("scale=iw*{scale_w:.5}:ih*{scale_h:.5}"));
    }

    let rotation_degrees = n(transform, "rotation", 0.0);
    let animated_rotation = keyframed_value_expr(clip, "rotation");
    if let Some(expression) = animated_rotation {
        // Constant diagonal bounds let the angle change every frame without
        // changing the filter's output dimensions mid-stream.
        filters.push(format!("rotate=angle='({expression})*PI/180':ow=iw:oh=ih:c=black@0"));
    } else if rotation_degrees.abs() > 0.0001 {
        let rotation = rotation_degrees.to_radians();
        filters.push(format!("rotate={rotation:.6}:ow=iw:oh=ih:c=black@0"));
    }

    let opacity = (n(transform, "opacity", 100.0) / 100.0).clamp(0.0, 1.0);
    if let Some(expression) = keyframed_value_expr_at(clip, "opacity", "T") {
        filters.push(format!("geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='alpha(X,Y)*({expression}/100)'"));
    } else if opacity < 0.999 {
        filters.push(format!("colorchannelmixer=aa={opacity:.4}"));
    }

    let static_x = n(&transform["position"], "x", 0.0);
    let static_y = n(&transform["position"], "y", 0.0);
    let animated_x = keyframed_value_expr(clip, "x");
    let animated_y = keyframed_value_expr(clip, "y");
    let x = animated_x.as_ref().map(|expr| format!("({expr})*{:.6}/1920", width as f64))
        .unwrap_or_else(|| format!("{:.3}", static_x * width as f64 / 1920.0));
    let y = animated_y.as_ref().map(|expr| format!("({expr})*{:.6}/1080", height as f64))
        .unwrap_or_else(|| format!("{:.3}", static_y * height as f64 / 1080.0));
    let (shake_x, shake_y) = if enabled(effects, "shake") {
        let amount = n(&effects["shake"], "amount", 18.0) * width as f64 / 1920.0 * 0.18;
        let freq = (n(&effects["shake"], "frequency", 8.0) * (1.0 - n(&effects["shake"], "smoothness", 35.0) / 120.0)).max(0.5);
        (format!("+sin(2*PI*t*{freq:.3})*{amount:.3}"), format!("+cos(2*PI*t*{freq:.3})*{amount:.3}"))
    } else { (String::new(), String::new()) };
    let overlay_x = if animated_x.is_some() || !shake_x.is_empty() {
        format!("'(main_w-overlay_w)/2+{x}{shake_x}'")
    } else {
        format!("(main_w-overlay_w)/2+{x}{shake_x}")
    };
    let overlay_y = if animated_y.is_some() || !shake_y.is_empty() {
        format!("'(main_h-overlay_h)/2+{y}{shake_y}'")
    } else {
        format!("(main_h-overlay_h)/2+{y}{shake_y}")
    };
    (filters.join(","), overlay_x, overlay_y)
}


pub(crate) fn audio_filters(clip: &Value, length: f64, total: f64) -> String {
    let props = &clip["props"];
    let audio = &props["audio"];
    let effects = &props["effects"];
    let mut filters = vec![format!("atrim=start={:.5}:duration={length:.5}", n(clip, "sourceOffset", 0.0).max(0.0)), "asetpts=PTS-STARTPTS".into()];
    let volume = (n(audio, "volume", 100.0) / 100.0).clamp(0.0, 2.0);
    if (volume - 1.0).abs() > 0.0001 { filters.push(format!("volume={volume:.4}")); }
    let pan = (n(audio, "pan", 0.0) / 100.0).clamp(-1.0, 1.0);
    if pan.abs() > 0.001 { filters.push(format!("stereotools=balance_out={pan:.4}")); }
    let bass = n(audio, "bass", 0.0); let treble = n(audio, "treble", 0.0);
    if bass.abs() > 0.001 { filters.push(format!("equalizer=f=100:t=q:w=1:g={bass:.3}")); }
    if treble.abs() > 0.001 { filters.push(format!("equalizer=f=4000:t=q:w=1:g={treble:.3}")); }
    if enabled(effects, "eq") {
        for (key, freq) in [("low", 100), ("mid", 1000), ("high", 8000)] {
            let gain = n(&effects["eq"], key, 0.0);
            if gain.abs() > 0.001 { filters.push(format!("equalizer=f={freq}:t=q:w=1:g={gain:.3}")); }
        }
    }
    if enabled(effects, "reverb") {
        let wet = (n(&effects["reverb"], "wet", 0.0) / 100.0).clamp(0.0, 1.0);
        let delay = (80.0 + n(&effects["reverb"], "room", 0.0) * 3.0).round() as u32;
        let damping = 1.0 - n(&effects["reverb"], "damp", 50.0).clamp(0.0, 100.0) / 125.0;
        filters.push(format!("aecho=1.0:{:.3}:{delay}|{}:{:.3}|{:.3}", 1.0 - wet * 0.45, delay * 2, wet * 0.35 * damping, wet * 0.18 * damping));
    }
    if enabled(effects, "delay") {
        let delay = n(&effects["delay"], "time", 320.0).clamp(1.0, 2000.0).round() as u32;
        let feedback = (n(&effects["delay"], "feedback", 0.0) / 100.0).clamp(0.0, 0.95);
        let mix = (n(&effects["delay"], "mix", 0.0) / 100.0).clamp(0.0, 1.0);
        filters.push(format!("aecho=1.0:{:.3}:{delay}|{}:{feedback:.3}|{:.3}", 1.0 - mix * 0.5, delay * 2, feedback * 0.5));
    }
    if enabled(effects, "compressor") { filters.push(format!("acompressor=threshold={:.3}dB:ratio={:.3}:attack={:.3}:release={:.3}", n(&effects["compressor"], "threshold", -18.0), n(&effects["compressor"], "ratio", 4.0), n(&effects["compressor"], "attack", 12.0), n(&effects["compressor"], "release", 140.0))); }
    if enabled(effects, "limiter") { filters.push(format!("alimiter=limit={:.5}:release={:.3}", 10_f64.powf(n(&effects["limiter"], "ceiling", -1.0) / 20.0), n(&effects["limiter"], "release", 80.0))); }
    if enabled(effects, "denoise") {
        let reduction = n(&effects["denoise"], "reduction", 0.0);
        let sensitivity = n(&effects["denoise"], "sensitivity", 50.0).clamp(0.0, 100.0);
        filters.push(format!("afftdn=nr={:.1}:nf={:.1}", (reduction * 0.24).clamp(0.0, 24.0), -50.0 + sensitivity * 0.3));
    }
    if audio["fadeIn"].as_bool().unwrap_or(false) { filters.push(format!("afade=t=in:st=0:d={:.4}", n(audio, "fadeInDuration", 1.0).clamp(0.0, length))); }
    if audio["fadeOut"].as_bool().unwrap_or(false) { let d = n(audio, "fadeOutDuration", 1.0).clamp(0.0, length); filters.push(format!("afade=t=out:st={:.4}:d={d:.4}", (length - d).max(0.0))); }
    let start = n(clip, "start", 0.0).max(0.0);
    filters.push(format!("adelay={}:all=1", (start * 1000.0).round() as u64));
    let _ = total;
    filters.join(",")
}



#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn crop_keeps_the_original_canvas_without_rescaling_the_remainder() {
        let clip = json!({
            "props": { "transform": { "crop": { "left": 192.0, "right": 96.0, "top": 108.0, "bottom": 0.0 } } }
        });
        let (graph, _, _) = video_filters(&clip, 1920, 1080, 0.0, 2.0, None, None);
        assert!(graph.contains("crop=iw*0.850000:ih*0.900000:iw*0.100000:ih*0.100000"));
        assert!(graph.contains("pad=1920:1080:(ow-iw/0.850000)/2+iw*0.100000/0.850000:(oh-ih/0.900000)/2+ih*0.100000/0.900000:color=black@0"));
        let scale_at = graph.find("scale=1920:1080:force_original_aspect_ratio=decrease").unwrap();
        let crop_at = graph.find("crop=iw*0.850000").unwrap();
        let pad_at = graph.find("pad=1920:1080").unwrap();
        assert!(scale_at < crop_at && crop_at < pad_at, "crop must be applied to fitted source before the transparent canvas pad");
    }

    // Preset looks are rendered by the shared WGPU shader for both preview
    // and export. Do not assert for FFmpeg visual-filter equivalents here;
    // renderer::params tests verify these presets reach the WGPU uniforms.

    #[test]
    fn curated_effects_generate_matching_export_operations() {
        let clip = json!({"start": 0.0, "props": {"effects": {
            "blur": {"enabled": true, "amount": 22.0},
            "chromatic": {"enabled": true, "amount": 50.0, "angle": 90.0},
            "shake": {"enabled": true, "amount": 18.0, "frequency": 8.0, "smoothness": 35.0},
            "sharpen": {"enabled": true, "amount": 45.0, "radius": 5.0, "threshold": 10.0},
            "vignette": {"enabled": true, "amount": 50.0, "midpoint": 50.0, "roundness": 0.0, "feather": 65.0},
            "scanlines": {"enabled": true, "spacing": 4.0, "amount": 32.0},
            "filmBurn": {"enabled": true, "amount": 42.0, "warmth": 70.0, "edge": 38.0, "flicker": 12.0},
            "halftone": {"enabled": true, "size": 5.0, "angle": 45.0, "contrast": 48.0},
            "posterize": {"enabled": true, "levels": 6.0, "blend": 100.0},
            "lightLeak": {"enabled": true, "amount": 40.0, "warmth": 70.0, "position": 35.0},
            "mirror": {"enabled": true}
        }}});
        let (graph, overlay_x, overlay_y) = video_filters(&clip, 1920, 1080, 0.0, 2.0, None, None);
        assert!(graph.contains("gblur=sigma="));
        assert!(graph.contains("rgbashift="));
        assert!(graph.contains("geq=r='r(X,Y)+("));
        assert!(graph.contains("sqrt(pow("));
        assert!(graph.contains("floor(Y/"));
        assert!(graph.contains("hflip"));
        assert!(graph.contains("lutrgb"));
        assert!(graph.contains("min(255,r(X,Y)+"));
        assert!(overlay_x.contains("sin(2*PI*t"));
        assert!(overlay_y.contains("cos(2*PI*t"));
    }

    #[test]
    fn retired_effects_are_ignored_in_export_graph() {
        let clip = json!({"start": 0.0, "props": {"effects": {
            "grain": {"enabled": true, "amount": 100.0},
            "lens": {"enabled": true, "amount": 100.0},
            "fisheye": {"enabled": true, "amount": 100.0},
            "glitch": {"enabled": true, "amount": 100.0},
            "glow": {"enabled": true, "amount": 100.0, "radius": 100.0}
        }}});
        let (graph, _, _) = video_filters(&clip, 1920, 1080, 0.0, 2.0, None, None);
        assert!(!graph.contains("noise="));
        assert!(!graph.contains("lenscorrection"));
        assert!(!graph.contains("rgbashift"));
        assert!(!graph.contains("gblur="));
    }

    #[test]
    fn keyframed_transform_channels_are_written_as_frame_evaluated_filters() {
        let clip = json!({
            "start": 0.0,
            "props": { "transform": {
                "position": { "x": 0.0, "y": 0.0 },
                "scale": { "w": 100.0, "h": 100.0 },
                "rotation": 0.0,
                "opacity": 100.0
            }},
            "keyframes": [
                { "time": 0.0, "values": { "x": 0.0, "y": 0.0, "scaleW": 100.0, "scaleH": 100.0, "rotation": 0.0, "opacity": 100.0 } },
                { "time": 2.0, "values": { "x": 960.0, "y": 540.0, "scaleW": 150.0, "scaleH": 75.0, "rotation": 90.0, "opacity": 25.0 } }
            ]
        });
        let (graph, x, y) = video_filters(&clip, 1920, 1080, 0.0, 2.0, None, None);
        assert!(graph.contains("eval=frame"));
        assert!(graph.contains("rotate=angle="));
        assert!(graph.contains("ow=iw:oh=ih:c=black@0"));
        assert!(graph.contains("geq=r='r(X,Y)'"));
        assert!(x.contains("lt(t,2.000000)"));
        assert!(y.contains("lt(t,2.000000)"));
    }
}
