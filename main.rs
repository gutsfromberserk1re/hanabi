use eframe::egui::{epaint, *};
use std::hash::Hash;

// ───────────────────────── config ─────────────────────────

/// true  = paint a pastel backdrop so the glass has something to "frost"
/// false = fully transparent window (desktop shows through). For a REAL OS blur
///         behind the window add the `window-vibrancy` crate and apply it at startup.
const FAKE_BACKDROP: bool = true;
const DURATION: f32 = 30.0; // seconds, demo only

const INK: Color32 = Color32::from_rgb(38, 42, 52);
const INK_SOFT: Color32 = Color32::from_rgb(110, 116, 128);
const BLUE: Color32 = Color32::from_rgb(96, 165, 255);

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
const SECTIONS: [(&str, &str, &[Param]); 3] = [
    ("Light", "☀", &LIGHT),
    ("Color", "💧", &COLOR),
    ("Detail", "▲", &DETAIL),
];

// ───────────────────────── app ─────────────────────────

struct App {
    tab: usize,
    tool: usize,
    playing: bool,
    pos: f32, // 0..1
    transform: Vec<f32>,
    colors: Vec<Vec<f32>>,
    open: [bool; 3],
    keyframes: bool,
    media: Vec<String>,
    selected_media: Option<usize>,
    logo: Option<TextureHandle>,
    project_name: String,
    export_target: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let logo = load_hanabi_logo(&cc.egui_ctx);
        Self {
            tab: 0,
            tool: 2,
            playing: false,
            pos: 0.0,
            transform: T_PARAMS.iter().map(|p| p.def).collect(),
            colors: SECTIONS
                .iter()
                .map(|s| s.2.iter().map(|p| p.def).collect())
                .collect(),
            open: [true, false, false],
            keyframes: false,
            media: vec![],
            selected_media: None,
            logo,
            project_name: "Untitled Project".to_owned(),
            export_target: None,
        }
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
                self.pos = 1.0;
                self.playing = false;
            }
            ctx.request_repaint();
        }

        CentralPanel::default().frame(Frame::none()).show(ctx, |ui| {
            let full = ui.max_rect();
            if FAKE_BACKDROP {
                draw_backdrop(ui.painter(), full);
            }

            let title_bar = Rect::from_min_max(
                full.min + vec2(8.0, 5.0),
                pos2(full.right() - 8.0, full.top() + 52.0),
            );
            self.titlebar_ui(ui, title_bar);

            let gap = 14.0;
            let area = Rect::from_min_max(
                pos2(full.left() + gap, title_bar.bottom() + 8.0),
                full.max - vec2(gap, gap),
            );
            let tl_h = (area.height() * 0.30).max(150.0);
            let top = Rect::from_min_max(area.min, pos2(area.right(), area.bottom() - tl_h - gap));
            let timeline = Rect::from_min_max(pos2(area.left(), top.bottom() + gap), area.max);

            let tool_w = 48.0;
            let right_w = (area.width() * 0.27).clamp(320.0, 420.0);
            let toolbar = Rect::from_center_size(
                pos2(top.left() + tool_w / 2.0, top.center().y + top.height() * 0.15),
                vec2(tool_w, 150.0),
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
    }
}

// ───────────────────────── panels ─────────────────────────

impl App {
    fn titlebar_ui(&mut self, ui: &mut Ui, r: Rect) {
        let painter = ui.painter().clone();
        let dark = ui.visuals().dark_mode;

        let bar_fill = if dark {
            Color32::from_rgba_unmultiplied(28, 30, 36, 178)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 132)
        };
        let edge = if dark {
            Color32::from_white_alpha(34)
        } else {
            Color32::from_white_alpha(190)
        };
        let primary = if dark {
            Color32::from_rgb(238, 240, 246)
        } else {
            INK
        };
        let secondary = if dark {
            Color32::from_rgb(160, 166, 178)
        } else {
            INK_SOFT
        };

        shadow(&painter, r, 17.0, 3.0, 12.0, if dark { 42 } else { 20 });
        painter.rect_filled(r, 17.0, bar_fill);
        painter.rect_stroke(r, 17.0, Stroke::new(1.0, edge));

        // macOS-style traffic lights: close, minimize, and maximize.
        let control_y = r.center().y;
        let controls = [
            (Color32::from_rgb(255, 95, 86), "close"),
            (Color32::from_rgb(255, 189, 46), "minimize"),
            (Color32::from_rgb(39, 201, 63), "maximize"),
        ];
        for (i, (color, action)) in controls.iter().enumerate() {
            let center = pos2(r.left() + 20.0 + i as f32 * 19.0, control_y);
            let hit = Rect::from_center_size(center, vec2(16.0, 16.0));
            let response = ui
                .interact(hit, ui.id().with(("mac-control", i)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);

            painter.circle_filled(center, 6.2, *color);
            painter.circle_stroke(
                center,
                6.2,
                Stroke::new(0.8, Color32::from_black_alpha(28)),
            );

            if response.hovered() {
                let symbol = match *action {
                    "close" => "×",
                    "minimize" => "−",
                    _ => "+",
                };
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    symbol,
                    FontId::proportional(10.0),
                    Color32::from_black_alpha(150),
                );
            }

            if response.clicked() {
                match *action {
                    "close" => ui.ctx().send_viewport_cmd(ViewportCommand::Close),
                    "minimize" => {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true))
                    }
                    _ => {
                        let maximized = ui.ctx().input(|input| {
                            input.viewport().maximized.unwrap_or(false)
                        });
                        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                    }
                }
            }
        }

        // Brand mark. Load an image from the project's logo/ folder when present.
        let logo_rect = Rect::from_center_size(
            pos2(r.left() + 100.0, control_y),
            vec2(29.0, 29.0),
        );
        if let Some(texture) = &self.logo {
            painter.image(
                texture.id(),
                logo_rect,
                Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            painter.circle_filled(
                logo_rect.center(),
                14.0,
                if dark {
                    Color32::from_rgb(232, 236, 246)
                } else {
                    Color32::from_rgb(45, 51, 66)
                },
            );
            painter.text(
                logo_rect.center(),
                Align2::CENTER_CENTER,
                "H",
                FontId::proportional(17.0),
                if dark { INK } else { Color32::WHITE },
            );
        }

        let brand_pos = pos2(r.left() + 122.0, control_y);
        painter.text(
            brand_pos,
            Align2::LEFT_CENTER,
            "Hanabi",
            FontId::proportional(16.0),
            primary,
        );

        let divider_x = r.left() + 184.0;
        painter.line_segment(
            [pos2(divider_x, r.center().y - 10.0), pos2(divider_x, r.center().y + 10.0)],
            Stroke::new(1.0, if dark {
                Color32::from_white_alpha(45)
            } else {
                Color32::from_black_alpha(22)
            }),
        );
        painter.text(
            pos2(divider_x + 14.0, control_y),
            Align2::LEFT_CENTER,
            &self.project_name,
            FontId::proportional(14.0),
            secondary,
        );

        // Export action.
        let export_rect = Rect::from_center_size(
            pos2(r.right() - 58.0, control_y),
            vec2(86.0, 32.0),
        );
        let export_response = ui
            .interact(export_rect, ui.id().with("titlebar-export"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let export_fill = if dark {
            Color32::from_rgb(224, 230, 244)
        } else {
            Color32::from_rgb(48, 57, 76)
        };
        let export_text = if dark { INK } else { Color32::WHITE };
        shadow(&painter, export_rect, 11.0, 2.0, 6.0, 24);
        painter.rect_filled(
            export_rect,
            11.0,
            if export_response.hovered() {
                export_fill.gamma_multiply(0.88)
            } else {
                export_fill
            },
        );
        painter.text(
            export_rect.center(),
            Align2::CENTER_CENTER,
            "↑  Export",
            FontId::proportional(13.0),
            export_text,
        );

        if export_response.clicked() {
            // This UI-only editor has no render/export pipeline wired to this button yet.
            // Keep the action useful by opening the destination picker for an MP4 export.
            self.export_target = rfd::FileDialog::new()
                .set_file_name("Hanabi-Export.mp4")
                .add_filter("MP4 video", &["mp4"])
                .save_file()
                .map(|path| path.display().to_string());
        }

        // Drag the frameless window from the unused title-bar area.
        let drag_rect = Rect::from_min_max(
            pos2(r.left() + 205.0, r.top()),
            pos2(export_rect.left() - 12.0, r.bottom()),
        );
        let drag = ui.interact(drag_rect, ui.id().with("titlebar-drag"), Sense::click_and_drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
    }

    fn toolbar_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        glass(&pt, r, 24.0, Color32::from_white_alpha(46));
        for i in 0..3 {
            let c = pos2(r.center().x, r.top() + 28.0 + i as f32 * 46.0);
            let br = Rect::from_center_size(c, vec2(38.0, 38.0));
            let resp = ui
                .interact(br, ui.id().with(("tool", i)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                self.tool = i;
            }
            let hot = ui.ctx().animate_bool_with_time(
                ui.id().with(("toolhot", i)),
                self.tool == i || resp.hovered(),
                0.12,
            );
            if hot > 0.0 {
                let a = if self.tool == i { 200.0 } else { 120.0 } * hot;
                shadow(&pt, br, 19.0, 3.0, 8.0, (22.0 * hot) as u8);
                pt.rect_filled(br, 19.0, Color32::from_white_alpha(a as u8));
            }
            match i {
                0 => {
                    pt.text(c, Align2::CENTER_CENTER, "✂", FontId::proportional(22.0), INK);
                }
                1 => {
                    pt.text(c, Align2::CENTER_CENTER, "Aa", FontId::proportional(18.0), INK);
                }
                _ => {
                    let pts = vec![
                        c + vec2(-6.0, -9.0),
                        c + vec2(-6.0, 9.0),
                        c + vec2(9.0, 0.0),
                    ];
                    pt.add(Shape::convex_polygon(pts, INK, Stroke::new(2.0_f32, INK)));
                }
            }
        }
    }

    fn preview_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        glass(&pt, r, 22.0, Color32::from_white_alpha(40));
        pt.text(
            r.left_top() + vec2(18.0, 16.0),
            Align2::LEFT_TOP,
            format!("{} / {}", fmt_time(self.pos * DURATION), fmt_time(DURATION)),
            FontId::monospace(13.0),
            INK_SOFT,
        );
        pt.text(r.center(), Align2::CENTER_CENTER, "Preview", FontId::proportional(20.0), Color32::from_black_alpha(40));

        // seek bar
        let seek = Rect::from_min_max(
            pos2(r.left() + 24.0, r.bottom() - 82.0),
            pos2(r.right() - 24.0, r.bottom() - 58.0),
        );
        let seek_p = p("seek", 0.0, 1.0, 0.0, "");
        slider(ui, seek, "seek", &mut self.pos, &seek_p, false, false);

        // play button
        let c = pos2(r.center().x, r.bottom() - 30.0);
        let br = Rect::from_center_size(c, vec2(44.0, 44.0));
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
        shadow(&pt, br, 22.0, 3.0, 10.0, 30);
        pt.circle_filled(c, 21.0 + hot * 1.5, Color32::WHITE);
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
    }

    fn right_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        glass(&pt, r, 22.0, Color32::from_white_alpha(40));

        // ── tab bar (neumorphic pill, acrylic tabs) ──
        let bar = Rect::from_min_size(r.min + vec2(10.0, 10.0), vec2(r.width() - 20.0, 46.0));
        pt.rect_filled(bar, 23.0, Color32::from_black_alpha(16));
        pt.rect_stroke(bar, 23.0, Stroke::new(1.5_f32, Color32::from_black_alpha(14)));
        pt.rect_stroke(bar.translate(vec2(0.0, 1.0)), 23.0, Stroke::new(1.0_f32, Color32::from_white_alpha(120)));

        let tw = bar.width() / 3.0;
        let target = bar.left() + self.tab as f32 * tw;
        let x = ui.ctx().animate_value_with_time(ui.id().with("tabx"), target, 0.18);
        let act = Rect::from_min_size(pos2(x, bar.top()), vec2(tw, bar.height())).shrink(4.0);
        shadow(&pt, act, 19.0, 3.0, 10.0, 34);
        pt.rect_filled(act, 19.0, Color32::from_white_alpha(215)); // acrylic white tint
        pt.rect_stroke(act, 19.0, Stroke::new(1.0_f32, Color32::WHITE));

        for (i, name) in ["Transform", "Colors", "Media"].iter().enumerate() {
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
        let foot = if self.tab == 0 { 56.0 } else { 0.0 };
        let content = Rect::from_min_max(
            pos2(r.left() + 12.0, bar.bottom() + 12.0),
            pos2(r.right() - 12.0, r.bottom() - 12.0 - foot),
        );
        let mut cui = ui.new_child(
            UiBuilder::new()
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
        );
        cui.set_clip_rect(content);
        let tab = self.tab;
        ScrollArea::vertical()
            .id_salt(("scroll", tab))
            .auto_shrink([false, false])
            .show(&mut cui, |ui| match tab {
                0 => self.transform_tab(ui),
                1 => self.colors_tab(ui),
                _ => self.media_tab(ui),
            });

        // keyframes button pinned to the bottom of the Transform tab
        if self.tab == 0 {
            let kb = Rect::from_center_size(pos2(r.center().x, r.bottom() - 34.0), vec2(160.0, 38.0));
            let resp = ui
                .interact(kb, ui.id().with("kf"), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                self.keyframes = !self.keyframes;
            }
            let on = ui.ctx().animate_bool_with_time(ui.id().with("kfon"), self.keyframes, 0.15);
            shadow(&pt, kb, 19.0, 3.0, 10.0, 26);
            pt.rect_filled(kb, 19.0, Color32::from_white_alpha(170));
            if on > 0.0 {
                pt.rect_filled(kb, 19.0, BLUE.gamma_multiply(0.35 * on));
            }
            pt.rect_stroke(kb, 19.0, Stroke::new(1.0_f32, Color32::WHITE));
            let col = if self.keyframes { Color32::from_rgb(30, 90, 190) } else { INK };
            pt.text(kb.center(), Align2::CENTER_CENTER, "◆  Keyframes", FontId::proportional(15.0), col);
        }
    }

    fn transform_tab(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        for (i, prm) in T_PARAMS.iter().enumerate() {
            param_row(ui, &mut self.transform[i], prm, false, false);
            ui.add_space(4.0);
        }
    }

    fn colors_tab(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        for (si, &(title, icon, params)) in SECTIONS.iter().enumerate() {
            let open = self.open[si];
            let w = ui.available_width();
            let (hr, resp) = ui.allocate_exact_size(vec2(w, 42.0), Sense::click());
            let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                self.open[si] = !open;
            }
            let pt = ui.painter();
            let a = if open { 170 } else if resp.hovered() { 130 } else { 95 };
            pt.rect_filled(hr, 15.0, Color32::from_white_alpha(a));
            pt.rect_stroke(hr, 15.0, Stroke::new(1.0_f32, Color32::from_white_alpha(190)));
            pt.text(hr.left_center() + vec2(18.0, 0.0), Align2::CENTER_CENTER, icon, FontId::proportional(17.0), INK);
            pt.text(hr.left_center() + vec2(40.0, 0.0), Align2::LEFT_CENTER, title, FontId::proportional(15.0), INK);
            let cc = hr.right_center() - vec2(20.0, 0.0);
            pt.circle_stroke(cc, 9.0, Stroke::new(1.0_f32, INK_SOFT));
            pt.line_segment([cc - vec2(4.0, 0.0), cc + vec2(4.0, 0.0)], Stroke::new(1.3_f32, INK_SOFT));
            if !open {
                pt.line_segment([cc - vec2(0.0, 4.0), cc + vec2(0.0, 4.0)], Stroke::new(1.3_f32, INK_SOFT));
            }
            ui.add_space(4.0);

            if open {
                Frame::none()
                    .fill(Color32::from_white_alpha(70))
                    .rounding(15.0)
                    .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(150)))
                    .inner_margin(Margin::symmetric(10.0, 6.0))
                    .show(ui, |ui| {
                        let w = ui.available_width();
                        let (rr, rresp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click());
                        let rresp = rresp.on_hover_cursor(CursorIcon::PointingHand);
                        let col = if rresp.hovered() { INK } else { INK_SOFT };
                        ui.painter().text(rr.right_center(), Align2::RIGHT_CENTER, "↺ Reset", FontId::proportional(13.0), col);
                        if rresp.clicked() {
                            for (j, prm) in params.iter().enumerate() {
                                self.colors[si][j] = prm.def;
                            }
                        }
                        for (j, prm) in params.iter().enumerate() {
                            let grad = prm.name == "Temperature";
                            param_row(ui, &mut self.colors[si][j], prm, prm.min < 0.0, grad);
                        }
                    });
                ui.add_space(8.0);
            }
        }
    }

    fn media_tab(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let (hr, _) = ui.allocate_exact_size(vec2(w, 44.0), Sense::hover());
        ui.painter().text(hr.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, "Media", FontId::proportional(17.0), INK);

        // "+" import button (top right)
        let c = hr.right_center() - vec2(20.0, 0.0);
        let br = Rect::from_center_size(c, vec2(36.0, 36.0));
        let resp = ui
            .interact(br, ui.id().with("import"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let hot = ui.ctx().animate_bool_with_time(ui.id().with("importhot"), resp.hovered(), 0.12);
        {
            let pt = ui.painter();
            shadow(pt, br, 18.0, 3.0, 9.0, 30);
            pt.circle_filled(c, 17.0 + hot, Color32::from_white_alpha(215));
            pt.circle_stroke(c, 17.0 + hot, Stroke::new(1.0_f32, Color32::WHITE));
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
            let (rr, rs) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
            let rs = rs.on_hover_cursor(CursorIcon::PointingHand);
            if rs.clicked() {
                self.selected_media = Some(i);
            }
            let sel = self.selected_media == Some(i);
            let a = if sel { 190 } else if rs.hovered() { 130 } else { 90 };
            let pt = ui.painter();
            pt.rect_filled(rr, 13.0, Color32::from_white_alpha(a));
            pt.rect_stroke(rr, 13.0, Stroke::new(1.0_f32, Color32::from_white_alpha(180)));
            let mut name = self.media[i].clone();
            if name.chars().count() > 30 {
                name = name.chars().take(28).collect::<String>() + "…";
            }
            pt.text(rr.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, format!("🎞  {name}"), FontId::proportional(14.0), INK);
            ui.add_space(5.0);
        }
    }

    fn timeline_ui(&mut self, ui: &mut Ui, r: Rect) {
        let pt = ui.painter().clone();
        glass(&pt, r, 20.0, Color32::from_white_alpha(40));

        let head_w = 58.0;
        let head = Rect::from_min_max(r.min, pos2(r.left() + head_w, r.bottom())).shrink2(vec2(6.0, 6.0));
        pt.rect_filled(head, 14.0, Color32::from_white_alpha(120));
        pt.rect_stroke(head, 14.0, Stroke::new(1.0_f32, Color32::from_white_alpha(180)));

        let tracks = Rect::from_min_max(pos2(r.left() + head_w + 6.0, r.top() + 6.0), r.max - vec2(6.0, 6.0));
        let mid = tracks.center().y;
        pt.text(pos2(head.center().x, (head.top() + mid) / 2.0), Align2::CENTER_CENTER, "V1", FontId::proportional(13.0), INK_SOFT);
        pt.text(pos2(head.center().x, (mid + head.bottom()) / 2.0), Align2::CENTER_CENTER, "A1", FontId::proportional(13.0), INK_SOFT);

        // the divider line between the two tracks
        pt.line_segment(
            [pos2(r.left() + 10.0, mid), pos2(r.right() - 10.0, mid)],
            Stroke::new(1.5_f32, Color32::from_rgb(170, 205, 235)),
        );

        // scrub by clicking / dragging in the track area
        let resp = ui.interact(tracks, ui.id().with("scrub"), Sense::click_and_drag());
        if resp.is_pointer_button_down_on() {
            if let Some(pp) = resp.interact_pointer_pos() {
                self.pos = ((pp.x - tracks.left()) / tracks.width()).clamp(0.0, 1.0);
            }
        }
        let x = tracks.left() + self.pos * tracks.width();
        pt.line_segment([pos2(x, tracks.top()), pos2(x, tracks.bottom())], Stroke::new(2.0_f32, BLUE));
        pt.circle_filled(pos2(x, tracks.top() + 4.0), 5.0, BLUE);
    }
}

// ───────────────────────── widgets ─────────────────────────

/// label | slider | value pill
fn param_row(ui: &mut Ui, v: &mut f32, prm: &Param, bipolar: bool, grad: bool) {
    let w = ui.available_width();
    let (row, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
    let cy = row.center().y;
    let label_w = 86.0;
    let pill = Rect::from_center_size(pos2(row.right() - 32.0, cy), vec2(60.0, 24.0));
    {
        let pt = ui.painter();
        pt.text(pos2(row.left() + 6.0, cy), Align2::LEFT_CENTER, prm.name, FontId::proportional(14.0), INK);
        pt.rect_filled(pill, 12.0, Color32::from_black_alpha(14));
        let s = if prm.max - prm.min < 20.0 {
            format!("{:.2}{}", v, prm.unit)
        } else {
            format!("{:.0}{}", v, prm.unit)
        };
        pt.text(pill.center(), Align2::CENTER_CENTER, s, FontId::proportional(13.0), INK);
    }
    let srect = Rect::from_min_max(pos2(row.left() + label_w, cy - 12.0), pos2(pill.left() - 8.0, cy + 12.0));
    slider(ui, srect, prm.name, v, prm, bipolar, grad);
}

/// neumorphic slider: recessed groove, blue fill, soft white knob.
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
    let track = Rect::from_min_max(pos2(rect.left() + 3.0, cy - 4.0), pos2(rect.right() - 3.0, cy + 4.0));
    let hot = ui
        .ctx()
        .animate_bool_with_time(id.with("hot"), resp.hovered() || resp.dragged(), 0.12);

    let pt = ui.painter();
    pt.rect_filled(track, 4.0, Color32::from_black_alpha(22));
    pt.line_segment(
        [track.left_top() + vec2(4.0, 0.5), track.right_top() + vec2(-4.0, 0.5)],
        Stroke::new(1.0_f32, Color32::from_black_alpha(26)),
    );
    pt.line_segment(
        [track.left_bottom() + vec2(4.0, 1.0), track.right_bottom() + vec2(-4.0, 1.0)],
        Stroke::new(1.0_f32, Color32::from_white_alpha(150)),
    );

    if grad {
        let a = Color32::from_rgb(70, 130, 255);
        let b = Color32::from_rgb(255, 175, 60);
        let mut m = Mesh::default();
        m.colored_vertex(track.left_top(), a);
        m.colored_vertex(track.right_top(), b);
        m.colored_vertex(track.right_bottom(), b);
        m.colored_vertex(track.left_bottom(), a);
        m.add_triangle(0, 1, 2);
        m.add_triangle(0, 2, 3);
        pt.add(Shape::mesh(m));
    } else {
        let sx = if bipolar {
            x0 + ((prm.def - prm.min) / (prm.max - prm.min)) * (x1 - x0)
        } else {
            track.left()
        };
        let fill = Rect::from_min_max(pos2(sx.min(kx), track.top()), pos2(sx.max(kx), track.bottom()));
        if fill.width() > 0.5 {
            pt.rect_filled(fill, 4.0, BLUE);
        }
    }

    let c = pos2(kx, cy);
    let rad = kr + hot * 1.5;
    pt.circle_filled(c + vec2(0.0, 3.0), rad + 3.0, Color32::from_black_alpha(10));
    pt.circle_filled(c + vec2(0.0, 2.0), rad + 1.0, Color32::from_black_alpha(26));
    pt.circle_filled(c, rad, Color32::WHITE);
    pt.circle_stroke(c, rad, Stroke::new(1.0_f32, Color32::from_black_alpha(18)));
    changed
}

// ───────────────────────── paint helpers ─────────────────────────

fn shadow(p: &Painter, r: Rect, rad: f32, off: f32, blur: f32, alpha: u8) {
    p.add(
        epaint::Shadow {
            offset: vec2(0.0, off),
            blur,
            spread: 0.0,
            color: Color32::from_black_alpha(alpha),
        }
        .as_shape(r, rad),
    );
}

/// frosted glass panel: soft shadow + translucent fill + bright edge
fn glass(p: &Painter, r: Rect, rad: f32, fill: Color32) {
    shadow(p, r, rad, 8.0, 26.0, 30);
    p.rect_filled(r, rad, fill);
    p.rect_stroke(r, rad, Stroke::new(1.0_f32, Color32::from_white_alpha(170)));
    p.rect_stroke(r.shrink(1.0), rad - 1.0, Stroke::new(1.0_f32, Color32::from_black_alpha(8)));
}

fn draw_backdrop(p: &Painter, r: Rect) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), Color32::from_rgb(186, 208, 255));
    m.colored_vertex(r.right_top(), Color32::from_rgb(255, 205, 226));
    m.colored_vertex(r.right_bottom(), Color32::from_rgb(255, 232, 192));
    m.colored_vertex(r.left_bottom(), Color32::from_rgb(196, 240, 226));
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
    // soft blobs so the glass has something to sit over
    for (c, base, col) in [
        (r.lerp_inside(vec2(0.25, 0.3)), 260.0, (120, 160, 255)),
        (r.lerp_inside(vec2(0.78, 0.65)), 300.0, (255, 150, 190)),
    ] {
        for k in 0..16 {
            let rad = base * (1.0 - k as f32 / 16.0);
            p.circle_filled(c, rad, Color32::from_rgba_unmultiplied(col.0, col.1, col.2, 7));
        }
    }
}

fn fmt_time(s: f32) -> String {
    let s = s.max(0.0) as u32;
    format!("{:02}:{:02}", s / 60, s % 60)
}


fn load_hanabi_logo(ctx: &Context) -> Option<TextureHandle> {
    use std::path::Path;

    let mut candidates = vec![
        Path::new("logo/hanabi.png").to_path_buf(),
        Path::new("logo/logo.png").to_path_buf(),
        Path::new("logo/icon.png").to_path_buf(),
        Path::new("assets/logo.png").to_path_buf(),
        Path::new("logo/hanabi.jpg").to_path_buf(),
        Path::new("logo/logo.jpg").to_path_buf(),
    ];

    // Also accept any PNG/JPEG placed directly inside the logo folder.
    if let Ok(entries) = std::fs::read_dir("logo") {
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|value| value.to_str()).unwrap_or("");
            if matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg") {
                candidates.push(path);
            }
        }
    }

    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(decoded) = image::load_from_memory(&bytes) else { continue };
        let rgba = decoded.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let pixels = rgba.into_raw();
        let image = ColorImage::from_rgba_unmultiplied(size, &pixels);
        return Some(ctx.load_texture("hanabi-logo", image, TextureOptions::LINEAR));
    }
    None
}

// ───────────────────────── main ─────────────────────────

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([900.0, 560.0])
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Hanabi", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
