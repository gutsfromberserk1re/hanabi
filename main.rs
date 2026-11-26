use eframe::egui::{epaint, *};
use std::hash::Hash;

// ───────────────────────── config ─────────────────────────

/// true  = paint the solid neumorphic backdrop
/// false = fully transparent window (desktop shows through). For a REAL OS blur
///         behind the window add the `window-vibrancy` crate and apply it at startup.
const FAKE_BACKDROP: bool = true;
const DURATION: f32 = 30.0; // seconds, demo only

const INK: Color32 = Color32::from_rgb(38, 42, 52);
const INK_SOFT: Color32 = Color32::from_rgb(110, 116, 128);
const BLUE: Color32 = Color32::from_rgb(96, 165, 255);
/// flat backdrop colour. depth comes from drop shadows only (no highlights)
const BG: Color32 = Color32::from_rgb(208, 213, 222);
/// panels / buttons sit a touch lighter than the backdrop and cast a drop shadow
const SURFACE: Color32 = Color32::from_rgb(224, 228, 235);

fn sh_dark() -> Color32 {
    Color32::from_rgba_unmultiplied(40, 52, 80, 70)
}
fn in_dark() -> Color32 {
    Color32::from_rgba_unmultiplied(40, 52, 80, 55)
}
// ───────────────────────── params ─────────────────────────

#[derive(Clone, Copy)]
struct Param {
    name: &'static str,
    min: f32,
    max: f32,
    def: f32,
    unit: &'static str,
}
const fn p(name: &'static str, min: f32, max: f32, def: f32, unit: &'static str) -> Param {
    Param { name, min, max, def, unit }
}

const T_PARAMS: [Param; 6] = [
    p("Position X", -1920.0, 1920.0, 0.0, "px"),
    p("Position Y", -1080.0, 1080.0, 0.0, "px"),
    p("Scale", 0.0, 400.0, 100.0, "%"),
    p("Rotation", -180.0, 180.0, 0.0, "°"),
    p("Opacity", 0.0, 100.0, 100.0, "%"),
    p("Speed", 0.1, 4.0, 1.0, "x"),
];

const LIGHT: [Param; 7] = [
    p("Exposure", -100.0, 100.0, 0.0, ""),
    p("Brightness", -100.0, 100.0, 0.0, ""),
    p("Contrast", -100.0, 100.0, 0.0, ""),
    p("Highlights", -100.0, 100.0, 0.0, ""),
    p("Shadows", -100.0, 100.0, 0.0, ""),
    p("Whites", -100.0, 100.0, 0.0, ""),
    p("Blacks", -100.0, 100.0, 0.0, ""),
];
const COLOR: [Param; 3] = [
    p("Temperature", -100.0, 100.0, 0.0, ""),
    p("Vibrance", -100.0, 100.0, 0.0, ""),
    p("Saturation", -100.0, 100.0, 0.0, ""),
];
const DETAIL: [Param; 3] = [
    p("Sharpness", 0.0, 100.0, 0.0, ""),
    p("Clarity", -100.0, 100.0, 0.0, ""),
    p("Vignette", -100.0, 100.0, 0.0, ""),
];
const FX: [Param; 6] = [
    p("Blur", 0.0, 100.0, 0.0, "%"),
    p("Soft glow", 0.0, 100.0, 0.0, "%"),
    p("Grain", 0.0, 100.0, 0.0, "%"),
    p("VHS", 0.0, 100.0, 0.0, "%"),
    p("Chromatic", 0.0, 100.0, 0.0, "%"),
    p("Sine wave", 0.0, 100.0, 0.0, "%"),
];
/// (title, icon id for `section_icon`, params)
const SECTIONS: [(&str, &str, &[Param]); 4] = [
    ("Light", "light", &LIGHT),
    ("Color", "color", &COLOR),
    ("Detail", "detail", &DETAIL),
    ("Video effects", "fx", &FX),
];

/// height of one framed param row (see `param_row`)
const ROW_H: f32 = 42.0;

// ───────────────────────── app ─────────────────────────

struct App {
    logo: Option<TextureHandle>,
    project_title: String,
    export_open: bool,
    exp_quality: usize, // 0 = low, 1 = high
    exp_fps: usize,     // 0 = 30 fps, 1 = 60 fps
    tab: usize,
    tool: usize,
    playing: bool,
    looping: bool,
    grid: bool,
    crop: bool,
    pos: f32, // 0..1
    keys: Vec<f32>, // keyframe positions, 0..1
    transform: Vec<f32>,
    colors: Vec<Vec<f32>>,
    open: [bool; 4],
    media: Vec<String>,
    selected_media: Option<usize>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(Visuals::light());
        // logo.png sits next to main.rs and is embedded in the exe at compile time
        let logo = image::load_from_memory(include_bytes!("logo.png")).ok().map(|img| {
            let img = img.resize(96, 96, image::imageops::FilterType::Lanczos3).to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            cc.egui_ctx.load_texture("logo", ColorImage::from_rgba_unmultiplied(size, img.as_raw()), TextureOptions::LINEAR)
        });
        Self {
            logo,
            project_title: "Untitled project".to_string(),
            export_open: false,
            exp_quality: 1,
            exp_fps: 0,
            tab: 0,
            tool: 0,
            playing: false,
            looping: false,
            grid: false,
            crop: false,
            pos: 0.0,
            keys: vec![],
            transform: T_PARAMS.iter().map(|p| p.def).collect(),
            colors: SECTIONS
                .iter()
                .map(|s| s.2.iter().map(|p| p.def).collect())
                .collect(),
            open: [true, false, false, false],
            media: vec![],
            selected_media: None,
        }
    }

    /// export button handler. presets: exp_quality 0/1 = low/high, exp_fps 0/1 = 30/60.
    fn start_export(&mut self) {
        // TODO: hand (self.exp_quality, self.exp_fps) to the export backend
    }

    fn key_at_pos(&self) -> Option<usize> {
        self.keys.iter().position(|k| (k - self.pos).abs() < 0.004)
    }
}

impl eframe::App for App {
    fn clear_color(&self, _v: &Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // transparent window
    }

    fn update(&mut self, ctx: &Context, _f: &mut eframe::Frame) {
        if self.playing {
            self.pos += ctx.input(|i| i.stable_dt) / DURATION;
            if self.pos >= 1.0 {
                if self.looping {
                    self.pos = 0.0;
                } else {
                    self.pos = 1.0;
                    self.playing = false;
                }
            }
            ctx.request_repaint();
        }

        CentralPanel::default().frame(Frame::none()).show(ctx, |ui| {
            let full = ui.max_rect();
            if FAKE_BACKDROP {
                // rounded window (square when maximized)
                let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                let win_rad = if maximized { 0.0 } else { 24.0 };
                ui.painter().rect_filled(full, win_rad, BG);
                ui.painter().rect_stroke(full.shrink(0.5), win_rad, Stroke::new(1.0_f32, Color32::from_black_alpha(36)));
            }

            let gap = 16.0;
            let title_h = 44.0;
            self.titlebar_ui(ui, Rect::from_min_max(full.min, pos2(full.right(), full.top() + title_h)));
            let area = Rect::from_min_max(pos2(full.left() + gap, full.top() + title_h), full.max - vec2(gap, gap));
            let tl_h = (area.height() * 0.30).max(150.0);
            let top = Rect::from_min_max(area.min, pos2(area.right(), area.bottom() - tl_h - gap));
            let timeline = Rect::from_min_max(pos2(area.left(), top.bottom() + gap), area.max);

            let tool_w = 52.0;
            let right_w = (area.width() * 0.27).clamp(320.0, 420.0);
            let toolbar = Rect::from_center_size(
                pos2(top.left() + tool_w / 2.0, top.center().y + top.height() * 0.15),
                vec2(tool_w, 156.0),
            );
            let right = Rect::from_min_max(pos2(top.right() - right_w, top.top()), top.max);
            let preview = Rect::from_min_max(
                pos2(toolbar.right() + gap, top.top()),
                pos2(right.left() - gap, top.bottom()),
            );

            self.toolbar_ui(ui, toolbar);
            self.preview_ui(ui, preview);
            self.right_ui(ui, right);
            self.timeline_ui(ui, timeline);
        });

        if self.export_open {
            self.export_modal(ctx);
        }
    }
}

// ───────────────────────── panels ─────────────────────────

impl App {
    /// seamless titlebar: same backdrop colour, themed window buttons
    fn titlebar_ui(&mut self, ui: &mut Ui, r: Rect) {
        let ctx = ui.ctx().clone();
        let pt = ui.painter().clone();

        // drag region first, so the buttons registered after it win the hit-test
        let drag = ui.interact(r, ui.id().with("titledrag"), Sense::click_and_drag());
        if drag.drag_started_by(PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
        }

        // logo · Hanabi | project title
        let mut x = r.left() + 24.0;
        if let Some(tex) = &self.logo {
            let lr = Rect::from_center_size(pos2(x + 13.0, r.center().y), vec2(26.0, 26.0));
            pt.image(tex.id(), lr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            x += 26.0 + 10.0;
        }
        let name = pt.text(pos2(x, r.center().y), Align2::LEFT_CENTER, "Hanabi", FontId::proportional(15.0), INK);
        let sep_x = name.right() + 12.0;
        pt.line_segment([pos2(sep_x, r.center().y - 8.0), pos2(sep_x, r.center().y + 8.0)], Stroke::new(1.0_f32, in_dark()));
        pt.text(pos2(sep_x + 12.0, r.center().y), Align2::LEFT_CENTER, &self.project_title, FontId::proportional(14.0), INK_SOFT);

        // export button, just left of the window buttons
        let ex = Rect::from_center_size(pos2(r.right() - 187.0, r.center().y), vec2(92.0, 32.0));
        let eresp = ui.interact(ex, ui.id().with("exportbtn"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let ehot = ctx.animate_bool_with_time(eresp.id.with("hot"), eresp.hovered(), 0.12);
        raised(&pt, ex, 16.0, 3.5 + ehot);
        {
            // little "send out of a tray" icon
            let ic = pos2(ex.left() + 22.0, ex.center().y);
            let st = Stroke::new(1.7_f32, BLUE);
            pt.line_segment([ic + vec2(0.0, 4.0), ic + vec2(0.0, -5.0)], st);
            pt.add(Shape::convex_polygon(vec![ic + vec2(0.0, -8.0), ic + vec2(-3.6, -4.0), ic + vec2(3.6, -4.0)], BLUE, Stroke::NONE));
            pt.line_segment([ic + vec2(-6.0, 1.0), ic + vec2(-6.0, 7.0)], st);
            pt.line_segment([ic + vec2(-6.0, 7.0), ic + vec2(6.0, 7.0)], st);
            pt.line_segment([ic + vec2(6.0, 7.0), ic + vec2(6.0, 1.0)], st);
        }
        pt.text(pos2(ex.left() + 38.0, ex.center().y), Align2::LEFT_CENTER, "Export", FontId::proportional(14.0), INK);
        if eresp.clicked() {
            self.export_open = true;
        }

        let ink = INK_SOFT;
        let st = Stroke::new(1.7_f32, ink);
        // close · maximize · minimize (right → left)
        let cx = |i: f32| pos2(r.right() - 30.0 - i * 40.0, r.center().y);

        let close = round_btn(ui, cx(0.0), 15.0, "wclose", false);
        let c = cx(0.0);
        let cc = if close.hovered() { Color32::from_rgb(226, 98, 98) } else { ink };
        let cs = Stroke::new(1.7_f32, cc);
        pt.line_segment([c + vec2(-4.5, -4.5), c + vec2(4.5, 4.5)], cs);
        pt.line_segment([c + vec2(-4.5, 4.5), c + vec2(4.5, -4.5)], cs);
        if close.clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let maxb = round_btn(ui, cx(1.0), 15.0, "wmax", false);
        pt.rect_stroke(Rect::from_center_size(cx(1.0), vec2(10.0, 10.0)), 2.0, st);
        if maxb.clicked() {
            let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
        }

        let minb = round_btn(ui, cx(2.0), 15.0, "wmin", false);
        let c = cx(2.0);
        pt.line_segment([c + vec2(-5.0, 2.5), c + vec2(5.0, 2.5)], st);
        if minb.clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
    }

    /// centred popup: quality + frame-rate presets, then Export
    fn export_modal(&mut self, ctx: &Context) {
        let full = ctx.screen_rect();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let win_rad = if maximized { 0.0 } else { 24.0 };
        let popup = Rect::from_center_size(full.center(), vec2(400.0, 360.0));
        let (mut close, mut go) = (false, false);

        Area::new(Id::new("export_modal"))
            .order(Order::Foreground)
            .fixed_pos(full.min)
            .show(ctx, |ui| {
                ui.set_clip_rect(full);
                let p = ui.painter().clone();

                // scrim; registered first so everything on the popup wins the hit-test
                let scrim = ui.interact(full, Id::new("export_scrim"), Sense::click());
                p.rect_filled(full, win_rad, Color32::from_black_alpha(80));
                if scrim.clicked() {
                    if let Some(pos) = scrim.interact_pointer_pos() {
                        if !popup.contains(pos) {
                            close = true;
                        }
                    }
                }

                raised(&p, popup, 26.0, 10.0);
                let l = popup.left() + 28.0;
                p.text(pos2(l, popup.top() + 36.0), Align2::LEFT_CENTER, "Export", FontId::proportional(21.0), INK);
                p.text(pos2(l, popup.top() + 62.0), Align2::LEFT_CENTER, &self.project_title, FontId::proportional(13.0), INK_SOFT);

                let xc = pos2(popup.right() - 38.0, popup.top() + 40.0);
                let xr = round_btn(ui, xc, 15.0, "export_close", false);
                let xs = Stroke::new(1.7_f32, INK_SOFT);
                p.line_segment([xc + vec2(-4.5, -4.5), xc + vec2(4.5, 4.5)], xs);
                p.line_segment([xc + vec2(-4.5, 4.5), xc + vec2(4.5, -4.5)], xs);
                if xr.clicked() {
                    close = true;
                }

                let seg_w = popup.width() - 56.0;
                p.text(pos2(l + 6.0, popup.top() + 102.0), Align2::LEFT_CENTER, "Quality", FontId::proportional(13.0), INK_SOFT);
                segmented(ui, Rect::from_min_size(pos2(l, popup.top() + 118.0), vec2(seg_w, 48.0)), "exp_q", &["Low", "High"], &mut self.exp_quality);
                p.text(pos2(l + 6.0, popup.top() + 192.0), Align2::LEFT_CENTER, "Frame rate", FontId::proportional(13.0), INK_SOFT);
                segmented(ui, Rect::from_min_size(pos2(l, popup.top() + 208.0), vec2(seg_w, 48.0)), "exp_f", &["Low · 30 fps", "High · 60 fps"], &mut self.exp_fps);

                // primary action: filled blue pill
                let eb = Rect::from_center_size(pos2(popup.center().x, popup.bottom() - 54.0), vec2(190.0, 46.0));
                let er = ui.interact(eb, Id::new("export_go"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                let hot = ui.ctx().animate_bool_with_time(Id::new("export_go_hot"), er.hovered(), 0.12);
                let fill = Color32::from_rgb((96.0 + 16.0 * hot) as u8, (165.0 + 10.0 * hot) as u8, 255);
                raised_fill(&p, eb, 23.0, 4.5, fill);
                p.text(eb.center(), Align2::CENTER_CENTER, "Export", FontId::proportional(16.0), Color32::WHITE);
                if er.clicked() {
                    go = true;
                }
            });

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if go {
            self.start_export();
            self.export_open = false;
        } else if close {
            self.export_open = false;
        }
    }

    fn toolbar_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        raised(&pt, r, 26.0, 7.0);
        for i in 0..3 {
            let c = pos2(r.center().x, r.top() + 30.0 + i as f32 * 48.0);
            let br = Rect::from_center_size(c, vec2(40.0, 40.0));
            let resp = ui
                .interact(br, ui.id().with(("tool", i)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                self.tool = i;
            }
            let active = self.tool == i;
            let hot = ui.ctx().animate_bool_with_time(ui.id().with(("toolhot", i)), resp.hovered(), 0.12);
            if active {
                pressed(&pt, br, 20.0, 3.0);
            } else if hot > 0.0 {
                raised(&pt, br, 20.0, 3.0 * hot);
            }
            let col = if active { BLUE } else { INK };
            match i {
                0 => {
                    pt.text(c, Align2::CENTER_CENTER, "✂", FontId::proportional(22.0), col);
                }
                1 => {
                    pt.text(c, Align2::CENTER_CENTER, "Aa", FontId::proportional(18.0), col);
                }
                _ => cursor_icon(&pt, c, col), // select tool: mouse pointer
            }
        }
    }

    fn preview_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        raised(&pt, r, 24.0, 8.0);

        // viewer stage: recessed screen
        let stage = Rect::from_min_max(r.min + vec2(18.0, 44.0), pos2(r.right() - 18.0, r.bottom() - 100.0));
        pressed(&pt, stage, 16.0, 3.0);
        pt.text(
            r.left_top() + vec2(24.0, 18.0),
            Align2::LEFT_TOP,
            format!("{} / {}", fmt_time(self.pos * DURATION), fmt_time(DURATION)),
            FontId::monospace(13.0),
            INK_SOFT,
        );
        pt.text(stage.center(), Align2::CENTER_CENTER, "Preview", FontId::proportional(20.0), Color32::from_black_alpha(40));

        if self.grid {
            let st = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(110, 116, 128, 110));
            let sp = pt.with_clip_rect(stage.intersect(pt.clip_rect()));
            for k in 1..3 {
                let t = k as f32 / 3.0;
                let x = stage.left() + stage.width() * t;
                let y = stage.top() + stage.height() * t;
                sp.line_segment([pos2(x, stage.top()), pos2(x, stage.bottom())], st);
                sp.line_segment([pos2(stage.left(), y), pos2(stage.right(), y)], st);
            }
        }
        if self.crop {
            let f = stage.shrink(16.0);
            let st = Stroke::new(3.0_f32, BLUE);
            let l = 22.0;
            for (corner, dx, dy) in [
                (f.left_top(), 1.0, 1.0),
                (f.right_top(), -1.0, 1.0),
                (f.left_bottom(), 1.0, -1.0),
                (f.right_bottom(), -1.0, -1.0),
            ] {
                pt.line_segment([corner, corner + vec2(l * dx, 0.0)], st);
                pt.line_segment([corner, corner + vec2(0.0, l * dy)], st);
            }
        }

        // seek bar (+ keyframe diamonds)
        let seek = Rect::from_min_max(pos2(r.left() + 26.0, r.bottom() - 84.0), pos2(r.right() - 26.0, r.bottom() - 60.0));
        let seek_p = p("seek", 0.0, 1.0, 0.0, "");
        slider(ui, seek, "seek", &mut self.pos, &seek_p, false, false);
        for k in &self.keys {
            let kx = seek.left() + 10.0 + k * (seek.width() - 20.0);
            diamond(&pt, pos2(kx, seek.top() - 1.0), 4.0, BLUE, true);
        }

        // ── bottom controls: [crop loop grid]  (play)  [‹ ◆ ›] ──
        let c = pos2(r.center().x, r.bottom() - 32.0);

        // play button
        let br = Rect::from_center_size(c, vec2(46.0, 46.0));
        let resp = ui
            .interact(br, ui.id().with("play"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            if self.pos >= 1.0 {
                self.pos = 0.0;
            }
            self.playing = !self.playing;
        }
        let hot = ui.ctx().animate_bool_with_time(ui.id().with("playhot"), resp.hovered(), 0.12);
        raised(&pt, br, 23.0, 4.0 + hot * 1.5);
        if self.playing {
            for dx in [-4.5, 4.5] {
                pt.rect_filled(Rect::from_center_size(c + vec2(dx, 0.0), vec2(4.0, 14.0)), 1.5, INK);
            }
        } else {
            pt.add(Shape::convex_polygon(
                vec![c + vec2(-5.0, -8.0), c + vec2(-5.0, 8.0), c + vec2(8.0, 0.0)],
                INK,
                Stroke::NONE,
            ));
        }

        // left cluster: crop · loop · grid (left → right)
        let lc = |i: f32| pos2(c.x - 156.0 + i * 46.0, c.y);
        if round_btn(ui, lc(0.0), 18.0, "crop", self.crop).clicked() {
            self.crop = !self.crop;
        }
        crop_icon(&pt, lc(0.0), if self.crop { BLUE } else { INK });
        if round_btn(ui, lc(1.0), 18.0, "loop", self.looping).clicked() {
            self.looping = !self.looping;
        }
        loop_icon(&pt, lc(1.0), if self.looping { BLUE } else { INK });
        if round_btn(ui, lc(2.0), 18.0, "grid", self.grid).clicked() {
            self.grid = !self.grid;
        }
        grid_icon(&pt, lc(2.0), if self.grid { BLUE } else { INK });

        // right: keyframe controls  ‹ ◆ ›
        let kc = pos2(c.x + 128.0, c.y);
        let pill = Rect::from_center_size(kc, vec2(124.0, 40.0));
        pressed(&pt, pill, 20.0, 3.0);
        for (i, dx) in [-40.0_f32, 0.0, 40.0].iter().enumerate() {
            let bc = kc + vec2(*dx, 0.0);
            let rect = Rect::from_center_size(bc, vec2(34.0, 32.0));
            let resp = ui
                .interact(rect, ui.id().with(("kf", i)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            let hot = ui.ctx().animate_bool_with_time(resp.id.with("hot"), resp.hovered(), 0.12);
            if hot > 0.0 {
                raised(&pt, Rect::from_center_size(bc, vec2(30.0, 28.0)), 14.0, 2.0 * hot);
            }
            match i {
                0 => {
                    pt.add(Shape::convex_polygon(
                        vec![bc + vec2(3.5, -6.0), bc + vec2(3.5, 6.0), bc + vec2(-4.5, 0.0)],
                        BLUE,
                        Stroke::NONE,
                    ));
                    if resp.clicked() {
                        let prev = self.keys.iter().copied().filter(|k| *k < self.pos - 0.004).fold(None, |a: Option<f32>, k| Some(a.map_or(k, |a| a.max(k))));
                        if let Some(k) = prev {
                            self.pos = k;
                        }
                    }
                }
                1 => {
                    diamond(&pt, bc, 7.0, BLUE, self.key_at_pos().is_some());
                    if resp.clicked() {
                        match self.key_at_pos() {
                            Some(idx) => {
                                self.keys.remove(idx);
                            }
                            None => {
                                self.keys.push(self.pos);
                                self.keys.sort_by(|a, b| a.total_cmp(b));
                            }
                        }
                    }
                }
                _ => {
                    pt.add(Shape::convex_polygon(
                        vec![bc + vec2(-3.5, -6.0), bc + vec2(-3.5, 6.0), bc + vec2(4.5, 0.0)],
                        BLUE,
                        Stroke::NONE,
                    ));
                    if resp.clicked() {
                        let next = self.keys.iter().copied().filter(|k| *k > self.pos + 0.004).fold(None, |a: Option<f32>, k| Some(a.map_or(k, |a| a.min(k))));
                        if let Some(k) = next {
                            self.pos = k;
                        }
                    }
                }
            }
        }
    }

    fn right_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        raised(&pt, r, 24.0, 8.0);

        // ── tab bar: recessed track, raised active tab ──
        let bar = Rect::from_min_size(r.min + vec2(12.0, 12.0), vec2(r.width() - 24.0, 48.0));
        pressed(&pt, bar, 24.0, 4.5);

        let tw = bar.width() / 3.0;
        let target = bar.left() + self.tab as f32 * tw;
        let x = ui.ctx().animate_value_with_time(ui.id().with("tabx"), target, 0.18);
        let act = Rect::from_min_size(pos2(x, bar.top()), vec2(tw, bar.height())).shrink(5.0);
        raised(&pt, act, 19.0, 5.0);

        for (i, name) in ["Transform", "Effects", "Media"].iter().enumerate() {
            let tr = Rect::from_min_size(pos2(bar.left() + i as f32 * tw, bar.top()), vec2(tw, bar.height()));
            let resp = ui
                .interact(tr, ui.id().with(("tab", i)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                self.tab = i;
            }
            let col = if self.tab == i { INK } else { INK_SOFT };
            pt.text(tr.center(), Align2::CENTER_CENTER, *name, FontId::proportional(15.0), col);
        }

        // ── content ──
        let foot = 0.0;
        let content = Rect::from_min_max(
            pos2(r.left() + 14.0, bar.bottom() + 14.0),
            pos2(r.right() - 14.0, r.bottom() - 14.0 - foot),
        );
        let mut cui = ui.new_child(
            UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
        );
        cui.set_clip_rect(content.expand2(vec2(6.0, 4.0)));
        let tab = self.tab;
        ScrollArea::vertical()
            .id_salt(("scroll", tab))
            .auto_shrink([false, false])
            .show(&mut cui, |ui| match tab {
                0 => self.transform_tab(ui),
                1 => self.colors_tab(ui),
                _ => self.media_tab(ui),
            });
    }

    fn transform_tab(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ui.add_space(6.0);
        for (i, prm) in T_PARAMS.iter().enumerate() {
            param_row(ui, &mut self.transform[i], prm, false, false);
        }
    }

    fn colors_tab(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ui.add_space(6.0);
        for (si, &(title, icon, params)) in SECTIONS.iter().enumerate() {
            let open = self.open[si];
            let w = ui.available_width() - 6.0;
            let (hr, resp) = ui.allocate_exact_size(vec2(w, 44.0), Sense::click());
            let hr = hr.translate(vec2(3.0, 0.0));
            let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                self.open[si] = !open;
            }
            let hot = ui.ctx().animate_bool_with_time(resp.id.with("hot"), resp.hovered(), 0.12);
            let pt = ui.painter();
            raised(pt, hr, 16.0, 3.5 + hot);
            section_icon(pt, hr.left_center() + vec2(22.0, 0.0), icon);
            pt.text(hr.left_center() + vec2(44.0, 0.0), Align2::LEFT_CENTER, title, FontId::proportional(15.0), INK);
            // +/- knob: raised when closed, pressed when open
            let cc = hr.right_center() - vec2(24.0, 0.0);
            let kr = Rect::from_center_size(cc, vec2(22.0, 22.0));
            if open {
                pressed(pt, kr, 11.0, 2.0);
            } else {
                raised(pt, kr, 11.0, 2.0);
            }
            pt.line_segment([cc - vec2(4.5, 0.0), cc + vec2(4.5, 0.0)], Stroke::new(1.6_f32, if open { BLUE } else { INK_SOFT }));
            if !open {
                pt.line_segment([cc - vec2(0.0, 4.5), cc + vec2(0.0, 4.5)], Stroke::new(1.6_f32, INK_SOFT));
            }
            ui.add_space(10.0);

            if open {
                // recessed tray; every row inside is its own raised card
                let h = 10.0 + 24.0 + params.len() as f32 * ROW_H + 8.0;
                let (_, tray) = ui.allocate_space(vec2(w, h));
                let tray = tray.translate(vec2(3.0, 0.0));
                pressed(ui.painter(), tray, 20.0, 3.5);
                let inner = tray.shrink2(vec2(8.0, 0.0)).shrink2(vec2(0.0, 10.0));
                let mut cui = ui.new_child(
                    UiBuilder::new()
                        .max_rect(inner)
                        .layout(Layout::top_down(Align::Min)),
                );
                cui.spacing_mut().item_spacing = vec2(0.0, 0.0);

                let (rr, rresp) = cui.allocate_exact_size(vec2(inner.width(), 24.0), Sense::click());
                let rresp = rresp.on_hover_cursor(CursorIcon::PointingHand);
                let col = if rresp.hovered() { BLUE } else { INK_SOFT };
                cui.painter().text(rr.right_center() - vec2(8.0, 0.0), Align2::RIGHT_CENTER, "↺ Reset", FontId::proportional(13.0), col);
                if rresp.clicked() {
                    for (j, prm) in params.iter().enumerate() {
                        self.colors[si][j] = prm.def;
                    }
                }
                for (j, prm) in params.iter().enumerate() {
                    let grad = prm.name == "Temperature";
                    param_row(&mut cui, &mut self.colors[si][j], prm, prm.min < 0.0, grad);
                }
                ui.add_space(12.0);
            }
        }
    }

    fn media_tab(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let w = ui.available_width();
        let (hr, _) = ui.allocate_exact_size(vec2(w, 48.0), Sense::hover());
        ui.painter().text(hr.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, "Media", FontId::proportional(17.0), INK);

        // "+" import button (top right)
        let c = hr.right_center() - vec2(24.0, 0.0);
        let resp = round_btn(ui, c, 17.0, "import", false);
        {
            let pt = ui.painter();
            pt.line_segment([c - vec2(6.0, 0.0), c + vec2(6.0, 0.0)], Stroke::new(2.0_f32, INK));
            pt.line_segment([c - vec2(0.0, 6.0), c + vec2(0.0, 6.0)], Stroke::new(2.0_f32, INK));
        }
        if resp.clicked() {
            if let Some(files) = rfd::FileDialog::new()
                .add_filter("media", &["mp4", "mov", "mkv", "avi", "webm", "mp3", "wav", "png", "jpg", "jpeg"])
                .pick_files()
            {
                for f in files {
                    if let Some(n) = f.file_name() {
                        self.media.push(n.to_string_lossy().to_string());
                    }
                }
            }
        }

        if self.media.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing here yet — hit + to import").color(INK_SOFT));
            });
        }
        for i in 0..self.media.len() {
            let (rr, rs) = ui.allocate_exact_size(vec2(ui.available_width() - 6.0, 44.0), Sense::click());
            let rr = rr.translate(vec2(3.0, 0.0));
            let rs = rs.on_hover_cursor(CursorIcon::PointingHand);
            if rs.clicked() {
                self.selected_media = Some(i);
            }
            let sel = self.selected_media == Some(i);
            let pt = ui.painter();
            if sel {
                pressed(pt, rr, 15.0, 3.0);
            } else {
                raised(pt, rr, 15.0, if rs.hovered() { 4.0 } else { 3.0 });
            }
            let mut name = self.media[i].clone();
            if name.chars().count() > 30 {
                name = name.chars().take(28).collect::<String>() + "…";
            }
            pt.text(rr.left_center() + vec2(16.0, 0.0), Align2::LEFT_CENTER, format!("🎞  {name}"), FontId::proportional(14.0), if sel { BLUE } else { INK });
            ui.add_space(10.0);
        }
    }

    fn timeline_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        raised(&pt, r, 22.0, 8.0);

        let head_w = 58.0;
        let head = Rect::from_min_max(r.min, pos2(r.left() + head_w, r.bottom())).shrink2(vec2(8.0, 8.0));
        pressed(&pt, head, 15.0, 3.0);

        let tracks = Rect::from_min_max(pos2(r.left() + head_w + 8.0, r.top() + 8.0), r.max - vec2(8.0, 8.0));
        pressed(&pt, tracks, 15.0, 3.0);
        let mid = tracks.center().y;
        pt.text(pos2(head.center().x, (head.top() + mid) / 2.0), Align2::CENTER_CENTER, "V1", FontId::proportional(13.0), INK_SOFT);
        pt.text(pos2(head.center().x, (mid + head.bottom()) / 2.0), Align2::CENTER_CENTER, "A1", FontId::proportional(13.0), INK_SOFT);

        // divider between the two tracks
        pt.line_segment([pos2(tracks.left() + 10.0, mid), pos2(tracks.right() - 10.0, mid)], Stroke::new(1.0_f32, in_dark()));

        // scrub by clicking / dragging in the track area
        let resp = ui.interact(tracks, ui.id().with("scrub"), Sense::click_and_drag());
        if resp.is_pointer_button_down_on() {
            if let Some(pp) = resp.interact_pointer_pos() {
                self.pos = ((pp.x - tracks.left()) / tracks.width()).clamp(0.0, 1.0);
            }
        }
        for k in &self.keys {
            let kx = tracks.left() + k * tracks.width();
            diamond(&pt, pos2(kx, tracks.top() + 16.0), 4.5, BLUE, true);
        }
        let x = tracks.left() + self.pos * tracks.width();
        pt.line_segment([pos2(x, tracks.top()), pos2(x, tracks.bottom())], Stroke::new(2.0_f32, BLUE));
        pt.circle_filled(pos2(x, tracks.top() + 4.0), 5.0, BLUE);
    }
}

// ───────────────────────── widgets ─────────────────────────

/// recessed track with a raised sliding thumb (same look as the tab bar)
fn segmented(ui: &mut Ui, rect: Rect, id: impl Hash, labels: &[&str], sel: &mut usize) {
    let p = ui.painter().clone();
    let base = ui.id().with(id);
    pressed(&p, rect, rect.height() / 2.0, 4.0);
    let w = rect.width() / labels.len() as f32;
    let target = rect.left() + *sel as f32 * w;
    let x = ui.ctx().animate_value_with_time(base.with("x"), target, 0.16);
    let act = Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height())).shrink(5.0);
    raised(&p, act, act.height() / 2.0, 5.0);
    for (i, label) in labels.iter().enumerate() {
        let tr = Rect::from_min_size(pos2(rect.left() + i as f32 * w, rect.top()), vec2(w, rect.height()));
        let resp = ui.interact(tr, base.with(i), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            *sel = i;
        }
        let col = if *sel == i { INK } else { INK_SOFT };
        p.text(tr.center(), Align2::CENTER_CENTER, *label, FontId::proportional(15.0), col);
    }
}

/// round neumorphic icon button. `active` = pressed in. caller paints the icon.
fn round_btn(ui: &mut Ui, c: Pos2, rad: f32, id: impl Hash, active: bool) -> Response {
    let rect = Rect::from_center_size(c, vec2(rad * 2.0, rad * 2.0));
    let resp = ui
        .interact(rect, ui.id().with(id), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let hot = ui.ctx().animate_bool_with_time(resp.id.with("hot"), resp.hovered(), 0.12);
    let pt = ui.painter();
    if active {
        pressed(pt, rect, rad, 2.5);
    } else {
        raised(pt, rect, rad, 3.0 + hot);
    }
    resp
}

/// framed row: raised card holding  label | slider | recessed value pill
fn param_row(ui: &mut Ui, v: &mut f32, prm: &Param, bipolar: bool, grad: bool) {
    let w = ui.available_width();
    let (row, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
    let card = row.shrink2(vec2(3.0, 3.0));
    let cy = card.center().y;
    let label_w = 80.0;
    let pill = Rect::from_center_size(pos2(card.right() - 14.0 - 30.0, cy), vec2(60.0, 24.0));
    {
        let pt = ui.painter();
        raised(pt, card, 14.0, 2.5);
        pt.text(pos2(card.left() + 14.0, cy), Align2::LEFT_CENTER, prm.name, FontId::proportional(13.0), INK);
        pressed(pt, pill, 12.0, 1.8);
        let s = if prm.max - prm.min < 20.0 {
            format!("{:.2}{}", v, prm.unit)
        } else {
            format!("{:.0}{}", v, prm.unit)
        };
        pt.text(pill.center(), Align2::CENTER_CENTER, s, FontId::proportional(12.5), INK);
    }
    let srect = Rect::from_min_max(pos2(card.left() + 14.0 + label_w, cy - 12.0), pos2(pill.left() - 8.0, cy + 12.0));
    slider(ui, srect, prm.name, v, prm, bipolar, grad);
}

/// neumorphic slider: recessed groove, blue fill, raised knob.
/// double-click resets to default.
fn slider(ui: &mut Ui, rect: Rect, salt: impl Hash, v: &mut f32, prm: &Param, bipolar: bool, grad: bool) -> bool {
    let id = ui.id().with(salt);
    let resp = ui.interact(rect, id, Sense::click_and_drag());
    let kr = 10.0;
    let (x0, x1) = (rect.left() + kr, rect.right() - kr);
    let mut changed = false;

    if resp.is_pointer_button_down_on() {
        if let Some(pp) = resp.interact_pointer_pos() {
            let t = ((pp.x - x0) / (x1 - x0)).clamp(0.0, 1.0);
            *v = prm.min + t * (prm.max - prm.min);
            changed = true;
        }
    }
    if resp.double_clicked() {
        *v = prm.def;
        changed = true;
    }

    let t = ((*v - prm.min) / (prm.max - prm.min)).clamp(0.0, 1.0);
    let kx = x0 + t * (x1 - x0);
    let cy = rect.center().y;
    let track = Rect::from_min_max(pos2(rect.left() + 3.0, cy - 4.5), pos2(rect.right() - 3.0, cy + 4.5));
    let hot = ui
        .ctx()
        .animate_bool_with_time(id.with("hot"), resp.hovered() || resp.dragged(), 0.12);

    let pt = ui.painter();
    pressed(pt, track, 4.5, 1.6);
    let inner = track.shrink(1.5);

    if grad {
        let a = Color32::from_rgb(70, 130, 255);
        let b = Color32::from_rgb(255, 175, 60);
        let mut m = Mesh::default();
        m.colored_vertex(inner.left_top(), a);
        m.colored_vertex(inner.right_top(), b);
        m.colored_vertex(inner.right_bottom(), b);
        m.colored_vertex(inner.left_bottom(), a);
        m.add_triangle(0, 1, 2);
        m.add_triangle(0, 2, 3);
        pt.add(Shape::mesh(m));
    } else {
        let sx = if bipolar {
            x0 + ((prm.def - prm.min) / (prm.max - prm.min)) * (x1 - x0)
        } else {
            inner.left()
        };
        let fill = Rect::from_min_max(pos2(sx.min(kx), inner.top()), pos2(sx.max(kx), inner.bottom()));
        if fill.width() > 0.5 {
            pt.rect_filled(fill, 3.0, BLUE);
        }
    }

    // raised knob
    let c = pos2(kx, cy);
    let rad = kr + hot * 1.0;
    let kn = Rect::from_center_size(c, vec2(rad * 2.0, rad * 2.0));
    raised_fill(pt, kn, rad, 2.5, Color32::from_rgb(240, 243, 248));
    changed
}

// ───────────────────────── neumorphic paint helpers ─────────────────────────

/// raised surface: soft drop shadow below, flat fill, no highlight
fn raised_fill(p: &Painter, r: Rect, rad: f32, d: f32, fill: Color32) {
    // wide soft shadow + a tight one so the edge stays defined
    p.add(epaint::Shadow { offset: vec2(0.0, d * 1.4), blur: d * 3.2, spread: 0.0, color: sh_dark() }.as_shape(r, rad));
    p.add(epaint::Shadow { offset: vec2(0.0, d * 0.4), blur: d * 1.2, spread: 0.0, color: Color32::from_black_alpha(26) }.as_shape(r, rad));
    p.rect_filled(r, rad, fill);
}

fn raised(p: &Painter, r: Rect, rad: f32, d: f32) {
    raised_fill(p, r, rad, d, SURFACE);
}

/// points of a rounded-rect outline, clockwise from the top-left arc
fn rrect_points(r: Rect, rad: f32) -> Vec<Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI};
    let rad = rad.min(r.width() / 2.0).min(r.height() / 2.0).max(0.0);
    let steps = 8;
    let corners = [
        (pos2(r.left() + rad, r.top() + rad), PI),
        (pos2(r.right() - rad, r.top() + rad), PI + FRAC_PI_2),
        (pos2(r.right() - rad, r.bottom() - rad), 0.0),
        (pos2(r.left() + rad, r.bottom() - rad), FRAC_PI_2),
    ];
    let mut pts = Vec::with_capacity(4 * (steps + 1));
    for (c, a0) in corners {
        for i in 0..=steps {
            let a = a0 + FRAC_PI_2 * i as f32 / steps as f32;
            pts.push(c + vec2(a.cos(), a.sin()) * rad);
        }
    }
    pts
}

/// recessed surface: slightly darker fill plus ONE gradient mesh hugging the inside of the
/// rounded outline (dark at the top edge, fading to nothing toward the centre and bottom).
/// A single smooth mesh has no segment joints, so there are no tick marks along the edge.
fn pressed(p: &Painter, r: Rect, rad: f32, d: f32) {
    p.rect_filled(r, rad, Color32::from_black_alpha(16));
    let depth = (d * 1.8).clamp(2.5, 8.0).min(r.height() / 2.0 - 0.5).max(1.0);
    let outer = rrect_points(r, rad);
    let inner = rrect_points(r.shrink(depth), (rad - depth).max(0.0));
    let n = outer.len();
    let mut m = Mesh::default();
    for q in &outer {
        let ty = ((q.y - r.top()) / r.height().max(1.0)).clamp(0.0, 1.0);
        let a = 58.0 * (1.0 - ty) + 8.0 * ty;
        m.colored_vertex(*q, Color32::from_rgba_unmultiplied(40, 52, 80, a as u8));
    }
    for q in &inner {
        m.colored_vertex(*q, Color32::from_rgba_unmultiplied(40, 52, 80, 0));
    }
    for i in 0..n {
        let j = (i + 1) % n;
        let (o0, o1, i0, i1) = (i as u32, j as u32, (n + i) as u32, (n + j) as u32);
        m.add_triangle(o0, o1, i1);
        m.add_triangle(o0, i1, i0);
    }
    p.add(Shape::mesh(m));
}

fn diamond(p: &Painter, c: Pos2, s: f32, col: Color32, filled: bool) {
    let pts = vec![c + vec2(0.0, -s), c + vec2(s, 0.0), c + vec2(0.0, s), c + vec2(-s, 0.0)];
    if filled {
        p.add(Shape::convex_polygon(pts, col, Stroke::new(1.0_f32, col)));
    } else {
        p.add(Shape::convex_polygon(pts, Color32::TRANSPARENT, Stroke::new(1.6_f32, col)));
    }
}

// ───────────────────────── icons ─────────────────────────

/// mouse pointer (select tool). concave, so it's built from triangles.
fn cursor_icon(p: &Painter, c: Pos2, col: Color32) {
    let pts = [
        c + vec2(-5.0, -9.5), // 0 tip
        c + vec2(-5.0, 7.5),  // 1
        c + vec2(-1.2, 4.0),  // 2 notch
        c + vec2(1.2, 10.0),  // 3 tail
        c + vec2(4.2, 8.8),   // 4
        c + vec2(1.8, 3.0),   // 5
        c + vec2(6.8, 3.0),   // 6 wing
    ];
    let mut m = Mesh::default();
    for q in pts {
        m.colored_vertex(q, col);
    }
    for t in [(0, 1, 2), (0, 2, 5), (0, 5, 6), (2, 3, 4), (2, 4, 5)] {
        m.add_triangle(t.0, t.1, t.2);
    }
    p.add(Shape::mesh(m));
    p.add(Shape::closed_line(pts.to_vec(), Stroke::new(1.2_f32, col)));
}

fn section_icon(p: &Painter, c: Pos2, id: &str) {
    match id {
        "light" => {
            p.circle_filled(c, 3.6, INK);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let d = vec2(a.cos(), a.sin());
                p.line_segment([c + d * 6.2, c + d * 8.6], Stroke::new(1.5_f32, INK));
            }
        }
        "color" => {
            // water drop: round bottom + pointed top
            p.circle_filled(c + vec2(0.0, 2.8), 5.2, INK);
            p.add(Shape::convex_polygon(vec![c + vec2(0.0, -8.5), c + vec2(-4.6, 1.5), c + vec2(4.6, 1.5)], INK, Stroke::NONE));
        }
        "detail" => {
            // sharpness: solid triangle
            p.add(Shape::convex_polygon(vec![c + vec2(0.0, -8.0), c + vec2(8.0, 6.5), c + vec2(-8.0, 6.5)], INK, Stroke::new(1.2_f32, INK)));
        }
        _ => {
            // sparkle for video effects: big 4-point star + small one
            let star = |p: &Painter, c: Pos2, s: f32| {
                let t = s * 0.28;
                p.add(Shape::convex_polygon(vec![c + vec2(0.0, -s), c + vec2(t, 0.0), c + vec2(0.0, s), c + vec2(-t, 0.0)], INK, Stroke::NONE));
                p.add(Shape::convex_polygon(vec![c + vec2(-s, 0.0), c + vec2(0.0, -t), c + vec2(s, 0.0), c + vec2(0.0, t)], INK, Stroke::NONE));
            };
            star(p, c + vec2(-1.5, 1.5), 8.0);
            star(p, c + vec2(6.0, -6.0), 3.6);
        }
    }
}

fn crop_icon(p: &Painter, c: Pos2, col: Color32) {
    let st = Stroke::new(2.0_f32, col);
    p.line_segment([c + vec2(-4.0, -9.0), c + vec2(-4.0, 4.0)], st);
    p.line_segment([c + vec2(-4.0, 4.0), c + vec2(9.0, 4.0)], st);
    p.line_segment([c + vec2(4.0, 9.0), c + vec2(4.0, -4.0)], st);
    p.line_segment([c + vec2(4.0, -4.0), c + vec2(-9.0, -4.0)], st);
}

fn loop_icon(p: &Painter, c: Pos2, col: Color32) {
    let (r, n) = (7.5_f32, 24);
    let a0 = -0.3_f32;
    let a1 = a0 + std::f32::consts::TAU * 0.80;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    let e = pts[n];
    p.add(Shape::line(pts, Stroke::new(2.0_f32, col)));
    let t = vec2(-a1.sin(), a1.cos()); // direction of travel at the end
    let nrm = vec2(-t.y, t.x);
    let tip = e + t * 5.0;
    p.add(Shape::convex_polygon(vec![tip, e + nrm * 4.0, e - nrm * 4.0], col, Stroke::NONE));
}

fn grid_icon(p: &Painter, c: Pos2, col: Color32) {
    let f = Rect::from_center_size(c, vec2(16.0, 16.0));
    p.rect_stroke(f, 2.5, Stroke::new(1.8_f32, col));
    let st = Stroke::new(1.3_f32, col);
    for d in [-2.7_f32, 2.7] {
        p.line_segment([pos2(c.x + d, f.top()), pos2(c.x + d, f.bottom())], st);
        p.line_segment([pos2(f.left(), c.y + d), pos2(f.right(), c.y + d)], st);
    }
}

fn fmt_time(s: f32) -> String {
    let s = s.max(0.0) as u32;
    format!("{:02}:{:02}", s / 60, s % 60)
}

// ───────────────────────── main ─────────────────────────

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Hanabi")
            .with_decorations(false)
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([900.0, 560.0])
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Hanabi", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
