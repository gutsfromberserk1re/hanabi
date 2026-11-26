mod engine;

use eframe::egui::{epaint, *};
use serde_json::{json, Value};
use std::{collections::{HashMap, HashSet}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

const INK: Color32 = Color32::from_rgb(38, 43, 54);
const INK_SOFT: Color32 = Color32::from_rgb(112, 119, 132);
const BLUE: Color32 = Color32::from_rgb(91, 153, 238);
fn panel() -> Color32 { Color32::from_rgba_unmultiplied(250, 252, 255, 154) }
fn panel_edge() -> Color32 { Color32::from_rgba_unmultiplied(255, 255, 255, 218) }
fn soft_edge() -> Color32 { Color32::from_rgba_unmultiplied(153, 166, 185, 76) }
const MIN_PROJECT_SECONDS: f64 = 60.0;
const PREVIEW_FPS: u64 = 30;

#[derive(Clone)]
struct MediaItem {
    path: String,
    name: String,
    kind: String,
    duration: f64,
    has_audio: bool,
    thumbnail: Option<String>,
    waveform: Option<String>,
}

#[derive(Clone)]
enum PendingTask {
    Inspect { path: String },
    Preview,
    Export { path: String },
}

struct HanabiApp {
    timeline: Value,
    media: Vec<MediaItem>,
    selected_media: Option<usize>,
    selected_clip: Option<String>,
    inspector_tab: usize,
    tool: usize,
    playhead: f64,
    playing: bool,
    preview_dirty: bool,
    preview_pending: bool,
    last_preview_request: Instant,
    preview_texture: Option<TextureHandle>,
    preview_message: String,
    tasks: HashMap<u64, PendingTask>,
    exporting: bool,
    show_keyframes: bool,
    section_open: [bool; 4],
    status: String,
    status_until: Instant,
    last_playback_tick: Instant,
}

impl HanabiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = Visuals::light();
        visuals.window_fill = Color32::TRANSPARENT;
        visuals.panel_fill = Color32::TRANSPARENT;
        // Keep the native window transparent so Windows Acrylic can provide
        // the blurred backdrop. These colors are only subtle tints, not a
        // painted opaque canvas behind the editor.
        visuals.extreme_bg_color = Color32::from_rgba_unmultiplied(248, 250, 253, 28);
        visuals.faint_bg_color = Color32::from_rgba_unmultiplied(248, 250, 253, 18);
        visuals.selection.bg_fill = BLUE;
        visuals.selection.stroke = Stroke::new(1.0_f32, Color32::from_rgb(58, 112, 194));
        visuals.widgets.noninteractive.bg_fill = Color32::from_white_alpha(90);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, soft_edge());
        visuals.widgets.inactive.bg_fill = Color32::from_rgba_unmultiplied(242, 244, 247, 220);
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(225));
        visuals.widgets.hovered.bg_fill = Color32::from_rgba_unmultiplied(255, 255, 255, 246);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(193, 205, 222));
        visuals.widgets.active.bg_fill = Color32::from_rgb(226, 237, 252);
        cc.egui_ctx.set_visuals(visuals);

        // Windows Acrylic supplies the real blurred backdrop. The UI itself is
        // a neutral white-tinted glass surface and never paints the old gradient.
        #[cfg(target_os = "windows")]
        let _ = window_vibrancy::apply_acrylic(cc, Some((247, 249, 253, 148)));

        let timeline = engine::request(json!({"command":"timeline_get"})).unwrap_or_else(|error| {
            eprintln!("Hanabi engine initialization: {error}");
            empty_timeline()
        });
        Self {
            timeline,
            media: Vec::new(),
            selected_media: None,
            selected_clip: None,
            inspector_tab: 0,
            tool: 2,
            playhead: 0.0,
            playing: false,
            preview_dirty: true,
            preview_pending: false,
            last_preview_request: Instant::now() - Duration::from_secs(1),
            preview_texture: None,
            preview_message: "Import a video or image to begin".into(),
            tasks: HashMap::new(),
            exporting: false,
            show_keyframes: true,
            section_open: [true, true, false, true],
            status: "Ready".into(),
            status_until: Instant::now() + Duration::from_secs(2),
            last_playback_tick: Instant::now(),
        }
    }

    fn toast(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_until = Instant::now() + Duration::from_secs(4);
    }

    fn project_duration(&self) -> f64 {
        let from_model = self.timeline["durationSeconds"].as_f64().unwrap_or(MIN_PROJECT_SECONDS);
        let from_clips = self.timeline["clips"].as_array().into_iter().flatten()
            .map(|clip| clip["start"].as_f64().unwrap_or(0.0) + clip["dur"].as_f64().unwrap_or(0.0))
            .fold(0.0_f64, f64::max);
        from_model.max(from_clips).max(MIN_PROJECT_SECONDS)
    }

    fn selected_clip_value(&self) -> Option<&Value> {
        let id = self.selected_clip.as_deref()?;
        self.timeline["clips"].as_array()?.iter().find(|clip| clip["id"].as_str() == Some(id))
    }

    fn has_visual_clip(&self) -> bool {
        self.timeline["clips"].as_array().is_some_and(|clips| clips.iter().any(|clip| {
            let kind = clip["kind"].as_str().unwrap_or("video");
            kind == "video" || kind == "image"
        }))
    }

    fn refresh_timeline(&mut self) {
        match engine::request(json!({"command":"timeline_get"})) {
            Ok(timeline) => self.timeline = timeline,
            Err(error) => self.toast(error),
        }
    }

    fn mark_preview_dirty(&mut self) {
        self.preview_dirty = true;
        self.preview_message.clear();
    }

    fn queue_preview(&mut self) {
        if !self.preview_dirty || self.preview_pending || !self.has_visual_clip() {
            return;
        }
        if self.last_preview_request.elapsed() < Duration::from_millis(30) {
            return;
        }
        let request = json!({
            "command":"preview_frame",
            "at":self.playhead,
            "height":720,
            "fps":PREVIEW_FPS,
            "playing":self.playing
        });
        match engine::submit(request) {
            Ok(id) => {
                self.tasks.insert(id, PendingTask::Preview);
                self.preview_pending = true;
                self.preview_dirty = false;
                self.last_preview_request = Instant::now();
            }
            Err(error) if error.contains("busy") || error.contains("queue") => {}
            Err(error) => self.preview_message = error,
        }
    }

    fn process_engine_events(&mut self, ctx: &Context) {
        loop {
            let event = match engine::poll() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => { self.toast(error); break; }
            };
            let request_id = event["requestId"].as_u64().unwrap_or(0);
            let command = event["command"].as_str().unwrap_or_default().to_owned();
            let task = self.tasks.remove(&request_id);
            let ok = event["ok"].as_bool().unwrap_or(false);
            match task {
                Some(PendingTask::Inspect { path }) => {
                    if !ok {
                        self.toast(format!("Import failed: {}", event["error"].as_str().unwrap_or("Unknown media error")));
                        continue;
                    }
                    let item = MediaItem {
                        path: path.clone(),
                        name: event["name"].as_str().unwrap_or("Media").to_owned(),
                        kind: event["kind"].as_str().unwrap_or("video").to_owned(),
                        duration: event["duration"].as_f64().unwrap_or(5.0).max(0.1),
                        has_audio: event["hasAudio"].as_bool().unwrap_or(false),
                        thumbnail: event["thumbnailPath"].as_str().map(str::to_owned),
                        waveform: event["waveformPath"].as_str().map(str::to_owned),
                    };
                    self.media.push(item.clone());
                    self.selected_media = Some(self.media.len() - 1);
                    self.add_media_clip(&item);
                    self.toast(format!("Imported {}", item.name));
                }
                Some(PendingTask::Preview) => {
                    self.preview_pending = false;
                    if ok {
                        match engine::copy_preview_frame(request_id) {
                            Ok((width, height, _at, pixels)) => {
                                let image = ColorImage::from_rgba_unmultiplied(
                                    [width as usize, height as usize], &pixels,
                                );
                                self.preview_texture = Some(ctx.load_texture(
                                    "hanabi-preview-frame", image, TextureOptions::LINEAR,
                                ));
                                self.preview_message.clear();
                            }
                            Err(error) => self.preview_message = error,
                        }
                    } else {
                        self.preview_message = event["error"].as_str().unwrap_or("Preview could not be rendered").to_owned();
                    }
                    if self.playing { self.preview_dirty = true; }
                }
                Some(PendingTask::Export { path }) => {
                    self.exporting = false;
                    if ok {
                        self.toast(format!("Export complete: {}", path));
                    } else {
                        self.toast(format!("Export failed: {}", event["error"].as_str().unwrap_or("Unknown export error")));
                    }
                }
                None if command == "generate_proxy" => {
                    // Proxy generation is an optional background optimization.
                }
                None => {}
            }
        }
    }

    fn import_media(&mut self) {
        let files = rfd::FileDialog::new()
            .add_filter("Video, audio and images", &["mp4", "mov", "mkv", "avi", "webm", "m4v", "mp3", "wav", "m4a", "aac", "flac", "ogg", "png", "jpg", "jpeg", "webp", "gif"])
            .pick_files();
        let Some(files) = files else { return; };
        for file in files {
            let path = file.to_string_lossy().to_string();
            match engine::submit(json!({"command":"inspect_media","path":path})) {
                Ok(id) => { self.tasks.insert(id, PendingTask::Inspect { path }); }
                Err(error) => self.toast(error),
            }
        }
        self.toast("Inspecting media…");
    }

    fn add_media_clip(&mut self, item: &MediaItem) {
        let id = format!("clip-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
        let track = if item.kind == "audio" { "a1" } else { "v1" };
        let clip = json!({
            "id":id,
            "track":track,
            "start":self.playhead,
            "dur":item.duration,
            "label":item.name,
            "path":item.path,
            "kind":item.kind,
            "hasAudio":item.has_audio,
            "sourceOffset":0.0,
            "sourceDuration":item.duration,
            "props":default_props(),
            "keyframes":[]
        });
        match engine::request(json!({"command":"timeline_add_clip","clip":clip})) {
            Ok(timeline) => {
                self.timeline = timeline;
                self.selected_clip = Some(id);
                self.inspector_tab = 0;
                self.mark_preview_dirty();
            }
            Err(error) => self.toast(format!("Could not add clip: {error}")),
        }
    }

    fn add_selected_media(&mut self) {
        let Some(index) = self.selected_media else { return; };
        let Some(item) = self.media.get(index).cloned() else { return; };
        self.add_media_clip(&item);
    }

    fn save_project(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Hanabi project", &["hanabi"])
            .set_file_name("Untitled.hanabi")
            .save_file() else { return; };
        let project = json!({
            "format": "hanabi-project",
            "version": 1,
            "timeline": self.timeline,
        });
        match std::fs::write(&path, serde_json::to_vec_pretty(&project).unwrap_or_default()) {
            Ok(()) => self.toast(format!("Project saved: {}", path.display())),
            Err(error) => self.toast(format!("Could not save project: {error}")),
        }
    }

    fn open_project(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Hanabi project", &["hanabi", "json"])
            .pick_file() else { return; };
        let loaded = std::fs::read_to_string(&path)
            .map_err(|error| format!("Could not read project: {error}"))
            .and_then(|contents| serde_json::from_str::<Value>(&contents)
                .map_err(|error| format!("Project file is invalid: {error}")));
        let project = match loaded {
            Ok(project) => project,
            Err(error) => { self.toast(error); return; }
        };
        if project["format"].as_str() != Some("hanabi-project")
            || project["version"].as_u64().unwrap_or(0) != 1
            || !project["timeline"]["clips"].is_array()
        {
            self.toast("This is not a supported Hanabi project file");
            return;
        }
        let timeline = &project["timeline"];
        let request = json!({
            "command": "timeline_initialize",
            "clips": timeline["clips"],
            "snapping": timeline["snapping"].as_bool().unwrap_or(true),
            "magnetic": timeline["magnetic"].as_bool().unwrap_or(true),
        });
        match engine::request(request) {
            Ok(timeline) => {
                self.timeline = timeline;
                self.selected_clip = None;
                self.playhead = 0.0;
                self.playing = false;
                self.rebuild_media_library();
                self.mark_preview_dirty();
                self.toast(format!("Project opened: {}", path.display()));
            }
            Err(error) => self.toast(format!("Could not open project: {error}")),
        }
    }

    fn rebuild_media_library(&mut self) {
        let mut seen = HashSet::new();
        self.media.clear();
        if let Some(clips) = self.timeline["clips"].as_array() {
            for clip in clips {
                let Some(path) = clip["path"].as_str().filter(|path| !path.is_empty()) else { continue; };
                if !seen.insert(path.to_owned()) { continue; }
                self.media.push(MediaItem {
                    path: path.to_owned(),
                    name: clip["label"].as_str().unwrap_or("Media").to_owned(),
                    kind: clip["kind"].as_str().unwrap_or("video").to_owned(),
                    duration: clip["sourceDuration"].as_f64().or_else(|| clip["dur"].as_f64()).unwrap_or(0.1).max(0.1),
                    has_audio: clip["hasAudio"].as_bool().unwrap_or(false),
                    thumbnail: None,
                    waveform: None,
                });
            }
        }
        self.selected_media = None;
    }

    fn undo(&mut self) {
        match engine::request(json!({"command":"timeline_undo"})) {
            Ok(timeline) => { self.timeline = timeline; self.mark_preview_dirty(); self.toast("Undo"); }
            Err(error) => self.toast(error),
        }
    }

    fn redo(&mut self) {
        match engine::request(json!({"command":"timeline_redo"})) {
            Ok(timeline) => { self.timeline = timeline; self.mark_preview_dirty(); self.toast("Redo"); }
            Err(error) => self.toast(error),
        }
    }

    fn toggle_snapping(&mut self) {
        let enabled = !self.timeline["snapping"].as_bool().unwrap_or(true);
        match engine::request(json!({"command":"timeline_set_snapping", "enabled":enabled})) {
            Ok(timeline) => {
                self.timeline = timeline;
                self.toast(if enabled { "Snapping enabled" } else { "Snapping disabled" });
            }
            Err(error) => self.toast(error),
        }
    }

    fn split_selected_clip(&mut self) {
        let Some(clip) = self.selected_clip_value().cloned() else {
            self.toast("Select a clip before splitting");
            return;
        };
        let id = clip["id"].as_str().unwrap_or_default();
        let start = clip["start"].as_f64().unwrap_or(0.0);
        let end = start + clip["dur"].as_f64().unwrap_or(0.0);
        if self.playhead <= start + 0.001 || self.playhead >= end - 0.001 {
            self.toast("Move the playhead inside the selected clip to split it");
            return;
        }
        let right_id = format!("clip-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
        match engine::request(json!({"command":"timeline_split_clip", "id":id, "at":self.playhead, "rightId":right_id})) {
            Ok(timeline) => {
                self.timeline = timeline;
                self.mark_preview_dirty();
                self.toast("Clip split at playhead");
            }
            Err(error) => self.toast(error),
        }
    }

    fn delete_selected_clip(&mut self) {
        let Some(id) = self.selected_clip.clone() else {
            self.toast("Select a clip to delete");
            return;
        };
        match engine::request(json!({"command":"timeline_delete_clip", "id":id})) {
            Ok(timeline) => {
                self.timeline = timeline;
                self.selected_clip = None;
                self.mark_preview_dirty();
                self.toast("Clip deleted");
            }
            Err(error) => self.toast(error),
        }
    }

    fn set_clip_property(&mut self, path: &str, value: Value, live: bool) {
        let Some(id) = self.selected_clip.clone() else {
            self.toast("Select a clip first");
            return;
        };
        match engine::request(json!({
            "command":"timeline_set_clip_property",
            "id":id,
            "path":format!("props.{path}"),
            "value":value,
            "live":live,
            "at":self.playhead
        })) {
            Ok(result) => {
                if !live { self.timeline = result; }
                else if let Some(clip) = self.timeline["clips"].as_array_mut().and_then(|clips| clips.iter_mut().find(|clip| clip["id"].as_str() == Some(id.as_str()))) {
                    set_json_path(&mut clip["props"], path, value);
                }
                self.mark_preview_dirty();
            }
            Err(error) => self.toast(error),
        }
    }

    fn end_property_edit(&mut self) {
        if let Ok(timeline) = engine::request(json!({"command":"timeline_end_property_edit"})) {
            self.timeline = timeline;
            self.mark_preview_dirty();
        }
    }

    fn add_keyframe(&mut self) {
        let Some(clip) = self.selected_clip_value().cloned() else {
            self.toast("Select a clip before adding a keyframe");
            return;
        };
        let id = clip["id"].as_str().unwrap_or_default();
        let start = clip["start"].as_f64().unwrap_or(0.0);
        let clip_end = start + clip["dur"].as_f64().unwrap_or(1.0);
        // The renderer treats clip end as exclusive; keep the keyframe on the
        // final visible frame instead of placing it just outside the clip.
        let end = (clip_end - 1.0 / PREVIEW_FPS as f64).max(start);
        let at = self.playhead.clamp(start, end);
        // Store the full editable property snapshot, so Transform, Adjust and
        // effect parameters all animate from one keyframe track.
        let values = json!({"props":clip["props"].clone()});
        match engine::request(json!({"command":"timeline_add_keyframe","id":id,"at":at,"values":values})) {
            Ok(timeline) => {
                self.timeline = timeline;
                self.show_keyframes = true;
                self.mark_preview_dirty();
                self.toast(format!("Keyframe added at {:.2}s", at));
            }
            Err(error) => self.toast(error),
        }
    }

    fn remove_keyframe_at_playhead(&mut self) {
        let Some(clip) = self.selected_clip_value().cloned() else { return; };
        let id = clip["id"].as_str().unwrap_or_default();
        let Some(frames) = clip["keyframes"].as_array() else {
            self.toast("No keyframes on this clip");
            return;
        };
        let candidate = frames.iter().filter_map(|frame| frame["time"].as_f64())
            .min_by(|a, b| (a - self.playhead).abs().total_cmp(&(b - self.playhead).abs()));
        let Some(at) = candidate.filter(|at| (at - self.playhead).abs() <= 1.0 / PREVIEW_FPS as f64) else {
            self.toast("No keyframe near the playhead");
            return;
        };
        match engine::request(json!({"command":"timeline_delete_keyframe","id":id,"at":at})) {
            Ok(timeline) => { self.timeline = timeline; self.mark_preview_dirty(); self.toast("Keyframe removed"); }
            Err(error) => self.toast(error),
        }
    }

    fn export_project(&mut self) {
        if self.exporting { return; }
        if !self.has_visual_clip() {
            self.toast("Add a video or image clip before exporting");
            return;
        }
        let Some(path) = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]).set_file_name("Hanabi-Export.mp4").save_file() else { return; };
        let path = path.to_string_lossy().to_string();
        match engine::submit(json!({
            "command":"export_project",
            "path":path,
            "width":1920,
            "height":1080,
            "fps":30,
            "timeline":self.timeline
        })) {
            Ok(id) => {
                self.tasks.insert(id, PendingTask::Export { path });
                self.exporting = true;
                self.toast("Export started — rendering frames and encoding H.264…");
            }
            Err(error) => self.toast(error),
        }
    }

    fn draw_titlebar(&mut self, ctx: &Context) {
        TopBottomPanel::top("hanabi-titlebar").exact_height(36.0).frame(Frame::none()).show(ctx, |ui| {
            let full = ui.max_rect();
            ui.painter().rect_filled(full, 0.0, Color32::from_rgba_unmultiplied(25, 29, 36, 232));
            let drag_rect = Rect::from_min_max(full.min, pos2(full.right() - 144.0, full.bottom()));
            let drag = ui.interact(drag_rect, ui.id().with("window-drag"), Sense::click_and_drag());
            if drag.drag_started_by(PointerButton::Primary) {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                let maximized = ctx.input(|input| input.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            ui.painter().circle_filled(pos2(17.0, full.center().y), 7.0, BLUE);
            ui.painter().circle_stroke(pos2(17.0, full.center().y), 7.0, Stroke::new(1.0_f32, Color32::WHITE));
            ui.painter().text(pos2(32.0, full.center().y), Align2::LEFT_CENTER, "Hanabi", FontId::proportional(14.0), Color32::from_rgb(241, 244, 249));
            ui.painter().text(pos2(92.0, full.center().y), Align2::LEFT_CENTER, "VIDEO EDITOR", FontId::proportional(9.0), Color32::from_rgb(157, 166, 180));

            let button_w = 46.0;
            for (index, glyph) in ["—", "□", "×"].iter().enumerate() {
                let rect = Rect::from_min_size(pos2(full.right() - button_w * (3 - index) as f32, full.top()), vec2(button_w, full.height()));
                let response = ui.interact(rect, ui.id().with(("window-button", index)), Sense::click());
                let hover = response.hovered();
                if hover {
                    let fill = if index == 2 { Color32::from_rgb(220, 75, 85) } else { Color32::from_white_alpha(190) };
                    ui.painter().rect_filled(rect.shrink2(vec2(3.0, 3.0)), 8.0, fill);
                }
                let col = if index == 2 && hover { Color32::WHITE } else { Color32::from_rgb(176, 185, 198) };
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, *glyph, FontId::proportional(17.0), col);
                if response.clicked() {
                    match index {
                        0 => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
                        1 => {
                            let maximized = ctx.input(|input| input.viewport().maximized.unwrap_or(false));
                            ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                        }
                        _ => ctx.send_viewport_cmd(ViewportCommand::Close),
                    }
                }
            }
        });
    }

    fn draw_inspector(&mut self, ctx: &Context) {
        SidePanel::right("hanabi-inspector")
            .default_width(350.0).min_width(310.0).max_width(430.0).resizable(true)
            .frame(Frame::none())
            .show(ctx, |ui| {
                surface(ui, |ui| {
                    let tab_h = 38.0;
                    Frame::none()
                        .fill(Color32::from_rgba_unmultiplied(226, 231, 239, 126))
                        .rounding(22.0)
                        .inner_margin(Margin::same(4.0))
                        .show(ui, |ui| {
                            let labels = ["Transform", "Colors", "Media"];
                            let (strip, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), tab_h), Sense::hover(),
                            );
                            let segment_w = strip.width() / labels.len() as f32;
                            let mut responses = Vec::with_capacity(labels.len());
                            for index in 0..labels.len() {
                                let rect = Rect::from_min_size(
                                    pos2(strip.left() + index as f32 * segment_w, strip.top()),
                                    vec2(segment_w, strip.height()),
                                );
                                responses.push(ui.interact(
                                    rect, ui.id().with(("hanabi-tab", index)), Sense::click(),
                                ));
                            }
                            for (index, response) in responses.iter().enumerate() {
                                if response.clicked() { self.inspector_tab = index; }
                            }
                            let indicator = ui.ctx().animate_value_with_time(
                                ui.id().with("hanabi-tab-indicator"), self.inspector_tab as f32, 0.18,
                            );
                            let active = Rect::from_min_size(
                                pos2(strip.left() + indicator * segment_w, strip.top()),
                                vec2(segment_w, strip.height()),
                            ).shrink(1.0);
                            ui.painter().add(epaint::Shadow {
                                offset: vec2(0.0, 2.0), blur: 7.0, spread: 0.0,
                                color: Color32::from_black_alpha(24),
                            }.as_shape(active, 19.0));
                            ui.painter().rect_filled(active, 19.0, Color32::from_white_alpha(232));
                            ui.painter().rect_stroke(active, 19.0, Stroke::new(1.0_f32, panel_edge()));
                            for (index, label) in labels.iter().enumerate() {
                                let rect = Rect::from_min_size(
                                    pos2(strip.left() + index as f32 * segment_w, strip.top()),
                                    vec2(segment_w, strip.height()),
                                );
                                ui.painter().text(
                                    rect.center(), Align2::CENTER_CENTER, *label,
                                    FontId::proportional(12.5),
                                    if self.inspector_tab == index { INK } else { INK_SOFT },
                                );
                            }
                        });
                    ui.add_space(9.0);
                    ScrollArea::vertical().id_salt("hanabi-inspector-scroll").auto_shrink([false, false]).show(ui, |ui| {
                        match self.inspector_tab {
                            0 => self.draw_transform_tab(ui),
                            1 => self.draw_adjust_tab(ui),
                            _ => self.draw_media_tab(ui),
                        }
                    });
                });
            });
    }

    fn draw_transform_tab(&mut self, ui: &mut Ui) {
        let Some(clip) = self.selected_clip_value().cloned() else {
            empty_state(ui, "Select a clip", "Import media and select a timeline clip to edit its transform.");
            return;
        };
        ui.label(RichText::new(clip["label"].as_str().unwrap_or("Selected clip")).strong().color(INK));
        ui.add_space(8.0);
        self.property_slider(ui, "Position X", "transform.position.x", -1920.0, 1920.0, 0.0, "px");
        self.property_slider(ui, "Position Y", "transform.position.y", -1080.0, 1080.0, 0.0, "px");
        self.property_slider(ui, "Scale", "transform.scale.w", 1.0, 400.0, 100.0, "%");
        self.property_slider(ui, "Rotation", "transform.rotation", -180.0, 180.0, 0.0, "°");
        self.property_slider(ui, "Opacity", "transform.opacity", 0.0, 100.0, 100.0, "%");
        ui.add_space(12.0);
        section_surface(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("◇  Keyframes").strong().color(INK));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let response = ui.checkbox(&mut self.show_keyframes, "Show");
                    if response.changed() { self.mark_preview_dirty(); }
                });
            });
            ui.add_space(7.0);
            let count = clip["keyframes"].as_array().map_or(0, Vec::len);
            ui.label(RichText::new(format!("{count} keyframes · playhead {:.2}s", self.playhead)).size(12.0).color(INK_SOFT));
            ui.horizontal(|ui| {
                if glass_button(ui, "＋ Add at playhead").clicked() { self.add_keyframe(); }
                if glass_button(ui, "− Remove nearby").clicked() { self.remove_keyframe_at_playhead(); }
            });
            ui.label(RichText::new("Transform, color and effect values interpolate between keyframes in preview and export.").size(11.0).color(INK_SOFT));
        });
    }

    fn draw_adjust_tab(&mut self, ui: &mut Ui) {
        if self.selected_clip_value().is_none() {
            empty_state(ui, "No clip selected", "Select a video or image clip to adjust its look and effects.");
            return;
        }
        self.draw_adjust_section(ui, 0, "Light", "☼");
        self.draw_adjust_section(ui, 1, "Color", "◉");
        self.draw_adjust_section(ui, 2, "Detail", "◈");
        self.draw_adjust_section(ui, 3, "Video Effects", "✧");
    }

    fn draw_adjust_section(&mut self, ui: &mut Ui, index: usize, title: &str, glyph: &str) {
        let id = ui.id().with(("adjust-section", index));
        let open = self.section_open[index];
        let anim = ui.ctx().animate_bool_with_time(id, open, 0.16);
        section_surface(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
            let response = response.on_hover_cursor(CursorIcon::PointingHand);
            if response.clicked() { self.section_open[index] = !open; }
            let painter = ui.painter();
            painter.text(rect.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, glyph, FontId::proportional(16.0), INK);
            painter.text(rect.left_center() + vec2(29.0, 0.0), Align2::LEFT_CENTER, title, FontId::proportional(14.0), INK);
            let center = rect.right_center() - vec2(12.0, 0.0);
            painter.circle_stroke(center, 8.0, Stroke::new(1.0_f32, INK_SOFT));
            painter.line_segment([center - vec2(3.5, 0.0), center + vec2(3.5, 0.0)], Stroke::new(1.2_f32, INK_SOFT));
            if !open { painter.line_segment([center - vec2(0.0, 3.5), center + vec2(0.0, 3.5)], Stroke::new(1.2_f32, INK_SOFT)); }
            if anim > 0.002 {
                // Reserve only the currently visible height, but lay out the
                // complete contents in a clipped child. This gives the section
                // a real short expand/collapse motion instead of popping open.
                let target_height = match index { 0 => 336.0, 1 | 2 => 164.0, _ => 520.0 };
                let visible_height = target_height * anim;
                let (visible_rect, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), visible_height), Sense::hover(),
                );
                let full_rect = Rect::from_min_size(
                    visible_rect.min, vec2(visible_rect.width(), target_height),
                );
                let parent_clip = ui.clip_rect();
                let mut content = ui.new_child(
                    UiBuilder::new().max_rect(full_rect).layout(Layout::top_down(Align::Min)),
                );
                content.set_clip_rect(visible_rect.intersect(parent_clip));
                content.add_space(5.0);
                match index {
                    0 => {
                        self.property_slider(&mut content, "Exposure", "colors.light.exposure", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Brightness", "colors.light.brightness", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Contrast", "colors.light.contrast", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Highlights", "colors.light.highlights", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Shadows", "colors.light.shadows", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Whites", "colors.light.whites", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Blacks", "colors.light.blacks", -100.0, 100.0, 0.0, "");
                    }
                    1 => {
                        self.property_slider(&mut content, "Temperature", "colors.color.temperature", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Vibrance", "colors.color.vibrance", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Saturation", "colors.color.saturation", -100.0, 100.0, 0.0, "");
                    }
                    2 => {
                        self.property_slider(&mut content, "Sharpness", "colors.detail.sharpness", 0.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Clarity", "colors.detail.clarity", -100.0, 100.0, 0.0, "");
                        self.property_slider(&mut content, "Vignette", "colors.detail.vignette", -100.0, 100.0, 0.0, "");
                    }
                    _ => self.draw_effect_controls(&mut content),
                }
            }
        });
        ui.add_space(7.0);
    }

    fn draw_effect_controls(&mut self, ui: &mut Ui) {
        self.effect_control(ui, "Chromatic Aberration", "effects.chromatic", &[ ("Amount", "amount", 0.0, 100.0, 28.0), ("Angle", "angle", 0.0, 360.0, 0.0) ]);
        self.effect_control(ui, "Sine Wave", "effects.wave", &[ ("Amount", "amount", 0.0, 100.0, 0.0), ("Frequency", "frequency", 1.0, 16.0, 4.0) ]);
        self.effect_control(ui, "Swirl", "effects.swirl", &[ ("Amount", "amount", 0.0, 100.0, 0.0), ("Radius", "radius", 10.0, 95.0, 70.0) ]);
        self.effect_control(ui, "Blur", "effects.blur", &[ ("Amount", "amount", 0.0, 100.0, 0.0) ]);
        self.effect_control(ui, "Pixelate", "effects.pixelate", &[ ("Block size", "size", 2.0, 80.0, 12.0) ]);
    }

    fn effect_control(&mut self, ui: &mut Ui, title: &str, path: &str, params: &[(&str, &str, f32, f32, f32)]) {
        let enabled = self.selected_clip_value().map(|clip| json_bool(&clip["props"], &format!("{path}.enabled"), false)).unwrap_or(false);
        ui.horizontal(|ui| {
            let mut checked = enabled;
            if ui.checkbox(&mut checked, title).changed() {
                self.set_clip_property(&format!("{path}.enabled"), json!(checked), false);
            }
        });
        for (label, field, min, max, default) in params {
            ui.add_enabled_ui(enabled, |ui| {
                self.property_slider(ui, label, &format!("{path}.{field}"), *min, *max, *default, "");
            });
        }
        ui.add_space(2.0);
    }

    fn draw_media_tab(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Media Library").strong().color(INK));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if glass_button(ui, "＋ Import").clicked() { self.import_media(); }
            });
        });
        ui.add_space(8.0);
        if self.media.is_empty() {
            empty_state(ui, "Your library is empty", "Import videos, audio or images to add them to your project.");
            return;
        }
        for index in 0..self.media.len() {
            let selected = self.selected_media == Some(index);
            let item = self.media[index].clone();
            Frame::none()
                .fill(if selected { Color32::from_rgba_unmultiplied(228, 239, 255, 220) } else { Color32::from_white_alpha(115) })
                .stroke(Stroke::new(1.0_f32, if selected { Color32::from_rgb(161, 190, 230) } else { soft_edge() }))
                .rounding(13.0).inner_margin(Margin::same(9.0)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(if item.kind == "audio" { "♫" } else if item.kind == "image" { "▧" } else { "▶" });
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&item.name).color(INK).strong());
                            let mut flags = Vec::new();
                            if item.has_audio { flags.push("audio"); }
                            if item.thumbnail.is_some() { flags.push("thumbnail"); }
                            if item.waveform.is_some() { flags.push("waveform"); }
                            let suffix = if flags.is_empty() { String::new() } else { format!(" · {}", flags.join(" · ")) };
                            ui.label(RichText::new(format!("{} · {:.1}s{}", item.kind, item.duration, suffix)).size(10.5).color(INK_SOFT));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if glass_button(ui, "Add").clicked() { self.add_media_clip(&item); }
                            if glass_button(ui, "Select").clicked() { self.selected_media = Some(index); }
                        });
                    });
                });
            ui.add_space(6.0);
        }
    }

    fn property_slider(&mut self, ui: &mut Ui, label: &str, path: &str, min: f32, max: f32, default: f32, unit: &str) {
        if self.selected_clip_value().is_none() { return; }
        let mut value = self.selected_clip_value().map(|clip| json_number(&clip["props"], path, default as f64) as f32).unwrap_or(default);
        let response = ui.horizontal(|ui| {
            ui.add_sized(
                vec2(76.0, 24.0),
                Label::new(RichText::new(label).size(12.0).color(INK)),
            );
            let value_width = 56.0;
            let slider_width = (ui.available_width() - value_width - ui.spacing().item_spacing.x).max(54.0);
            let response = ui.add_sized(
                vec2(slider_width, 22.0),
                Slider::new(&mut value, min..=max).show_value(false),
            );
            Frame::none()
                .fill(Color32::from_rgba_unmultiplied(231, 235, 242, 150))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 190)))
                .rounding(12.0)
                .inner_margin(Margin::symmetric(7.0, 4.0))
                .show(ui, |ui| {
                    ui.set_min_size(vec2(value_width, 23.0));
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(format!("{value:.0}{unit}")).size(11.0).color(INK_SOFT));
                    });
                });
            response
        }).inner;
        ui.add_space(5.0);
        if response.changed() {
            self.set_clip_property(path, json!(value), true);
            if path == "transform.scale.w" {
                self.set_clip_property("transform.scale.h", json!(value), true);
            }
        }
        if response.drag_stopped() {
            self.end_property_edit();
        }
    }

    fn draw_timeline(&mut self, ctx: &Context) {
        TopBottomPanel::bottom("hanabi-timeline")
            .exact_height(224.0).frame(Frame::none())
            .show(ctx, |ui| {
                surface(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("TIMELINE").strong().size(11.0).color(INK_SOFT));
                        ui.label(RichText::new(format!("{} clips", self.timeline["clips"].as_array().map_or(0, Vec::len))).size(11.0).color(INK_SOFT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if glass_button(ui, "＋ Keyframe").clicked() { self.add_keyframe(); }
                            let keyframes_label = if self.show_keyframes { "◆ Keys" } else { "◇ Keys" };
                            if glass_button(ui, keyframes_label).clicked() { self.show_keyframes = !self.show_keyframes; }
                            if glass_button(ui, "⌫ Delete").clicked() { self.delete_selected_clip(); }
                            if glass_button(ui, "✂ Split").clicked() { self.split_selected_clip(); }
                            if glass_button(ui, "↷").clicked() { self.redo(); }
                            if glass_button(ui, "↶").clicked() { self.undo(); }
                            let snap_label = if self.timeline["snapping"].as_bool().unwrap_or(true) { "⌁ Snap" } else { "⌁ Snap off" };
                            if glass_button(ui, snap_label).clicked() { self.toggle_snapping(); }
                            ui.menu_button("Project", |ui| {
                                if ui.button("Open project…").clicked() { ui.close_menu(); self.open_project(); }
                                if ui.button("Save project…").clicked() { ui.close_menu(); self.save_project(); }
                            });
                        });
                    });
                    ui.add_space(5.0);
                    self.timeline_canvas(ui);
                });
            });
    }

    fn timeline_canvas(&mut self, ui: &mut Ui) {
        let rows: [(&str, &str); 10] = [("V1", "v1"), ("V2", "v2"), ("V3", "v3"), ("V4", "v4"), ("A1", "a1"), ("A2", "a2"), ("A3", "a3"), ("A4", "a4"), ("A5", "a5"), ("A6", "a6")];
        let total_w = ui.available_width();
        let total_h = ui.available_height().max(145.0);
        let (canvas, _) = ui.allocate_exact_size(vec2(total_w, total_h), Sense::hover());
        let label_w = 43.0;
        let axis_h = 18.0;
        let tracks = Rect::from_min_max(pos2(canvas.left() + label_w, canvas.top() + axis_h), canvas.right_bottom());
        let row_h = tracks.height() / rows.len() as f32;
        let duration = self.project_duration();
        let painter = ui.painter().clone();
        painter.rect_filled(canvas, 11.0, Color32::from_rgba_unmultiplied(250, 252, 255, 112));
        painter.rect_stroke(canvas, 11.0, Stroke::new(1.0_f32, Color32::from_white_alpha(220)));

        for second in 0..=(duration.ceil() as usize / 5) {
            let t = second as f64 * 5.0;
            let x = tracks.left() + (t / duration) as f32 * tracks.width();
            painter.line_segment([pos2(x, canvas.top() + 1.0), pos2(x, tracks.bottom())], Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(130, 143, 163, 45)));
            painter.text(pos2(x + 3.0, canvas.top() + 8.0), Align2::LEFT_CENTER, format!("{t:.0}s"), FontId::proportional(9.5), INK_SOFT);
        }

        for (index, (label, _track)) in rows.iter().enumerate() {
            let top = tracks.top() + index as f32 * row_h;
            let row = Rect::from_min_size(pos2(tracks.left(), top), vec2(tracks.width(), row_h));
            if index % 2 == 0 { painter.rect_filled(row, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 50)); }
            painter.text(pos2(canvas.left() + 4.0, row.center().y), Align2::LEFT_CENTER, *label, FontId::proportional(10.0), INK_SOFT);
            painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(145, 156, 173, 52)));
        }

        // Track background interaction provides scrubbing; clip interactions are
        // painted on top, so dragging a block moves it rather than moving time.
        let background_response = ui.interact(tracks, ui.id().with("timeline-scrub"), Sense::click_and_drag());
        if background_response.clicked() || background_response.dragged() {
            if let Some(pointer) = background_response.interact_pointer_pos() {
                self.playhead = (((pointer.x - tracks.left()) / tracks.width()) as f64 * duration).clamp(0.0, duration);
                self.mark_preview_dirty();
            }
        }

        let clips = self.timeline["clips"].as_array().cloned().unwrap_or_default();
        for clip in clips {
            let track = clip["track"].as_str().unwrap_or("v1");
            let Some(row_index) = rows.iter().position(|row| row.1 == track) else { continue; };
            let start = clip["start"].as_f64().unwrap_or(0.0).max(0.0);
            let end = start + clip["dur"].as_f64().unwrap_or(1.0).max(0.1);
            let x0 = tracks.left() + (start / duration) as f32 * tracks.width();
            let x1 = tracks.left() + (end / duration) as f32 * tracks.width();
            let top = tracks.top() + row_index as f32 * row_h + 2.0;
            let rect = Rect::from_min_max(pos2(x0, top), pos2(x1.max(x0 + 13.0), top + row_h - 4.0));
            let id = clip["id"].as_str().unwrap_or_default().to_owned();
            let selected = self.selected_clip.as_deref() == Some(id.as_str());
            let response = ui.interact(rect, ui.id().with(("timeline-clip", id.clone())), Sense::click_and_drag());
            let base = if track.starts_with('a') { Color32::from_rgb(150, 179, 209) } else { Color32::from_rgb(116, 167, 229) };
            let fill = if selected { base } else { base.gamma_multiply(0.78) };
            painter.rect_filled(rect, 6.0, fill);
            painter.rect_stroke(rect, 6.0, Stroke::new(if selected { 2.0_f32 } else { 1.0_f32 }, if selected { Color32::WHITE } else { Color32::from_white_alpha(170) }));
            painter.text(rect.left_center() + vec2(7.0, 0.0), Align2::LEFT_CENTER, clip["label"].as_str().unwrap_or("Clip"), FontId::proportional(9.5), Color32::WHITE);
            if response.clicked() {
                self.selected_clip = Some(id.clone());
                self.inspector_tab = 0;
            }
            if response.drag_stopped() {
                let delta = response.drag_delta().x as f64 / tracks.width() as f64 * duration;
                let desired_start = (start + delta).max(0.0);
                let _ = engine::request(json!({
                    "command":"timeline_move_clip", "id":id, "start":desired_start,
                    "track":track, "playhead":self.playhead, "snapThreshold":0.18
                })).map(|timeline| self.timeline = timeline).map_err(|error| self.toast(error));
                self.mark_preview_dirty();
            }
            // Narrow edge hit-zones trim the source in place; the middle of the
            // clip remains a move target. The backend keeps linked audio and
            // source offsets synchronized for these edits.
            let handle_w = 9.0_f32.min(rect.width() * 0.25);
            let left_handle_rect = Rect::from_min_max(rect.left_top(), pos2(rect.left() + handle_w, rect.bottom()));
            let right_handle_rect = Rect::from_min_max(pos2(rect.right() - handle_w, rect.top()), rect.right_bottom());
            let left_handle = ui.interact(left_handle_rect, ui.id().with(("trim-left", id.clone())), Sense::click_and_drag());
            let right_handle = ui.interact(right_handle_rect, ui.id().with(("trim-right", id.clone())), Sense::click_and_drag());
            if left_handle.hovered() || right_handle.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            if left_handle.drag_stopped() {
                let delta = left_handle.drag_delta().x as f64 / tracks.width() as f64 * duration;
                let next_start = (start + delta).max(0.0);
                let source_duration = clip["sourceDuration"].as_f64();
                match engine::request(json!({
                    "command":"timeline_trim_clip", "id":id, "start":next_start,
                    "end":end, "sourceDuration":source_duration
                })) {
                    Ok(timeline) => { self.timeline = timeline; self.mark_preview_dirty(); }
                    Err(error) => self.toast(format!("Trim failed: {error}")),
                }
            } else if right_handle.drag_stopped() {
                let delta = right_handle.drag_delta().x as f64 / tracks.width() as f64 * duration;
                let next_end = (end + delta).max(start + 1.0 / PREVIEW_FPS as f64);
                let source_duration = clip["sourceDuration"].as_f64();
                match engine::request(json!({
                    "command":"timeline_trim_clip", "id":id, "start":start,
                    "end":next_end, "sourceDuration":source_duration
                })) {
                    Ok(timeline) => { self.timeline = timeline; self.mark_preview_dirty(); }
                    Err(error) => self.toast(format!("Trim failed: {error}")),
                }
            }
            if selected {
                painter.rect_filled(left_handle_rect.shrink2(vec2(2.0, 4.0)), 2.0, Color32::from_white_alpha(185));
                painter.rect_filled(right_handle_rect.shrink2(vec2(2.0, 4.0)), 2.0, Color32::from_white_alpha(185));
            }
            if selected && self.show_keyframes {
                if let Some(frames) = clip["keyframes"].as_array() {
                    for frame in frames {
                        if let Some(at) = frame["time"].as_f64() {
                            let kx = tracks.left() + (at / duration) as f32 * tracks.width();
                            let center = pos2(kx, rect.top() + 4.0);
                            painter.add(Shape::convex_polygon(
                                vec![center + vec2(0.0, -4.0), center + vec2(4.0, 0.0), center + vec2(0.0, 4.0), center + vec2(-4.0, 0.0)],
                                Color32::from_rgb(255, 226, 139), Stroke::new(1.0_f32, Color32::from_rgb(176, 128, 35)),
                            ));
                        }
                    }
                }
            }
        }

        let play_x = tracks.left() + (self.playhead / duration) as f32 * tracks.width();
        painter.line_segment([pos2(play_x, canvas.top()), pos2(play_x, tracks.bottom())], Stroke::new(1.6_f32, BLUE));
        painter.circle_filled(pos2(play_x, tracks.top() - 2.0), 4.0, BLUE);
    }
}

impl eframe::App for HanabiApp {
    fn clear_color(&self, _visuals: &Visuals) -> [f32; 4] { [0.0, 0.0, 0.0, 0.0] }

    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.process_engine_events(ctx);
        if !ctx.wants_keyboard_input() {
            let shortcuts = ctx.input(|input| (
                input.modifiers.command && input.key_pressed(Key::S),
                input.modifiers.command && input.key_pressed(Key::O),
                input.modifiers.command && input.key_pressed(Key::Z) && !input.modifiers.shift,
                input.modifiers.command && (input.key_pressed(Key::Y) || (input.modifiers.shift && input.key_pressed(Key::Z))),
                input.modifiers.command && input.key_pressed(Key::B),
                input.key_pressed(Key::Delete),
            ));
            if shortcuts.0 { self.save_project(); }
            if shortcuts.1 { self.open_project(); }
            if shortcuts.2 { self.undo(); }
            if shortcuts.3 { self.redo(); }
            if shortcuts.4 { self.split_selected_clip(); }
            if shortcuts.5 { self.delete_selected_clip(); }
        }
        let now = Instant::now();
        if self.playing {
            let dt = now.duration_since(self.last_playback_tick).as_secs_f64().min(0.1);
            self.playhead += dt;
            if self.playhead >= self.project_duration() {
                self.playhead = 0.0;
            }
            self.mark_preview_dirty();
        }
        self.last_playback_tick = now;

        self.queue_preview();
        self.draw_titlebar(ctx);
        self.draw_timeline(ctx);
        self.draw_inspector(ctx);
        CentralPanel::default().frame(Frame::none()).show(ctx, |ui| {
            ui.add_space(5.0);
            let workspace = ui.available_size();
            let workspace_h = (workspace.y - 9.0).max(180.0);
            let toolbar_w = 52.0;
            let gap = 10.0;
            let preview_w = (workspace.x - toolbar_w - gap).max(220.0);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // A separate floating tool rail, rather than embedding the
                // controls inside the preview card.
                ui.allocate_ui_with_layout(
                    vec2(toolbar_w, workspace_h),
                    Layout::top_down(Align::Center),
                    |ui| {
                        let rail_h = 154.0;
                        ui.add_space(((workspace_h - rail_h) * 0.5).max(0.0));
                        let rail_frame = Frame::none()
                            .fill(Color32::from_rgba_unmultiplied(250, 252, 255, 190))
                            .stroke(Stroke::new(1.0_f32, panel_edge()))
                            .rounding(24.0)
                            .inner_margin(Margin::same(5.0));
                        rail_frame.show(ui, |ui| {
                            ui.set_min_width(40.0);
                            ui.vertical_centered(|ui| {
                                for (index, glyph) in ["✂", "Aa", "▶"].iter().enumerate() {
                                    let selected = self.tool == index;
                                    let (rect, response) = ui.allocate_exact_size(vec2(40.0, 42.0), Sense::click());
                                    if response.clicked() { self.tool = index; }
                                    let t = ui.ctx().animate_bool_with_time(
                                        ui.id().with(("tool", index)), selected || response.hovered(), 0.14,
                                    );
                                    if t > 0.01 {
                                        ui.painter().add(epaint::Shadow {
                                            offset: vec2(0.0, 2.0), blur: 7.0, spread: 0.0,
                                            color: Color32::from_black_alpha(22),
                                        }.as_shape(rect.shrink(2.0), 13.0));
                                        ui.painter().rect_filled(
                                            rect.shrink(2.0), 13.0,
                                            Color32::from_white_alpha((112.0 + 112.0 * t) as u8),
                                        );
                                        ui.painter().rect_stroke(rect.shrink(2.0), 13.0, Stroke::new(1.0_f32, panel_edge()));
                                    }
                                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, *glyph, FontId::proportional(17.0), INK);
                                    ui.add_space(2.0);
                                }
                            });
                        });
                    },
                );
                ui.add_space(gap);
                ui.allocate_ui_with_layout(
                    vec2(preview_w, workspace_h),
                    Layout::top_down(Align::Min),
                    |ui| {
                        let card_w = preview_w;
                        let card_h = workspace_h;
                        let card_rect = ui.available_rect_before_wrap().shrink(1.0);
                        ui.painter().add(epaint::Shadow {
                            offset: vec2(0.0, 5.0), blur: 18.0, spread: 0.0,
                            color: Color32::from_black_alpha(20),
                        }.as_shape(card_rect, 22.0));
                        Frame::none()
                            .fill(Color32::from_rgba_unmultiplied(250, 252, 255, 166))
                            .stroke(Stroke::new(1.0_f32, panel_edge()))
                            .rounding(22.0)
                            .inner_margin(Margin::same(16.0))
                            .show(ui, |ui| {
                                // Force the card's content region to occupy the full
                                // allocated width and height. This also keeps empty-state
                                // labels centered and normally wrapped.
                                ui.set_min_size(vec2((card_w - 32.0).max(120.0), (card_h - 32.0).max(120.0)));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{} / {}", fmt_time(self.playhead), fmt_time(self.project_duration())))
                                        .monospace().size(11.0).strong().color(INK_SOFT));
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if self.exporting {
                                            ui.spinner();
                                            ui.label(RichText::new("Exporting").size(11.0).color(INK_SOFT));
                                        } else if glass_button(ui, "⇧ Export").clicked() {
                                            self.export_project();
                                        }
                                    });
                                });

                                ui.add_space(4.0);
                                let inner_width = ui.available_width();
                                let inner_height = ui.available_height();
                                let controls_height = 76.0;
                                let preview_height = (inner_height - controls_height - 10.0).max(120.0);
                                ui.allocate_ui_with_layout(
                                    vec2(inner_width, preview_height),
                                    Layout::top_down(Align::Center),
                                    |ui| {
                                        ui.centered_and_justified(|ui| {
                                            if let Some(texture) = &self.preview_texture {
                                                let aspect = texture.size_vec2().x / texture.size_vec2().y.max(1.0);
                                                let available = ui.available_size();
                                                let mut size = vec2(available.x, available.x / aspect.max(0.1));
                                                if size.y > available.y { size = vec2(available.y * aspect, available.y); }
                                                ui.image((texture.id(), size));
                                            } else {
                                                ui.vertical_centered(|ui| {
                                                    ui.label(RichText::new("Preview").size(18.0)
                                                        .color(Color32::from_rgba_unmultiplied(120, 130, 145, 100)));
                                                    if !self.preview_message.is_empty()
                                                        && self.preview_message != "Import a video or image to begin"
                                                    {
                                                        ui.add_space(5.0);
                                                        ui.label(RichText::new(&self.preview_message).size(11.0).color(INK_SOFT));
                                                    }
                                                });
                                            }
                                        });
                                    },
                                );
                                ui.add_space(2.0);
                                let timeline_duration = self.project_duration();
                                let slider_response = ui.add_sized(
                                    vec2(inner_width, 18.0),
                                    Slider::new(&mut self.playhead, 0.0..=timeline_duration).show_value(false),
                                );
                                if slider_response.drag_stopped() || slider_response.clicked() {
                                    self.mark_preview_dirty();
                                }
                                ui.add_space(3.0);
                                ui.horizontal(|ui| {
                                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                        if glass_button(ui, "⏮").clicked() {
                                            self.playhead = 0.0;
                                            self.mark_preview_dirty();
                                        }
                                        let play_label = if self.playing { "Ⅱ Pause" } else { "▶ Play" };
                                        if glass_button(ui, play_label).clicked() {
                                            self.playing = !self.playing;
                                            self.last_playback_tick = Instant::now();
                                            self.mark_preview_dirty();
                                        }
                                        if glass_button(ui, "◀ Frame").clicked() {
                                            self.playhead = (self.playhead - 1.0 / PREVIEW_FPS as f64).max(0.0);
                                            self.mark_preview_dirty();
                                        }
                                        if glass_button(ui, "Frame ▶").clicked() {
                                            self.playhead = (self.playhead + 1.0 / PREVIEW_FPS as f64).min(self.project_duration());
                                            self.mark_preview_dirty();
                                        }
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(RichText::new(format!("{} fps", PREVIEW_FPS)).size(10.0).color(INK_SOFT));
                                    });
                                });
                            });
                    },
                );
            });
        });
        if Instant::now() < self.status_until {
            Area::new(Id::new("hanabi-toast")).anchor(Align2::CENTER_BOTTOM, vec2(0.0, -238.0)).show(ctx, |ui| {
                Frame::none().fill(Color32::from_rgba_unmultiplied(38, 45, 58, 226)).rounding(12.0).inner_margin(Margin::symmetric(13.0, 8.0)).show(ui, |ui| {
                    ui.label(RichText::new(&self.status).size(11.0).color(Color32::WHITE));
                });
            });
        }
        ctx.request_repaint_after(Duration::from_millis(if self.playing { 12 } else { 40 }));
    }
}

fn surface(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    let rect = ui.available_rect_before_wrap().shrink(1.0);
    ui.painter().add(epaint::Shadow {
        offset: vec2(0.0, 5.0), blur: 18.0, spread: 0.0,
        color: Color32::from_black_alpha(18),
    }.as_shape(rect, 20.0));
    Frame::none().fill(panel()).stroke(Stroke::new(1.0_f32, panel_edge())).rounding(20.0)
        .inner_margin(Margin::same(12.0)).show(ui, add_contents);
}

fn section_surface(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    Frame::none().fill(Color32::from_rgba_unmultiplied(255, 255, 255, 104))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(166, 176, 190, 78)))
        .rounding(15.0).inner_margin(Margin::same(8.0)).show(ui, add_contents);
}

fn glass_button(ui: &mut Ui, label: &str) -> Response {
    let font_id = TextStyle::Button.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font_id.clone(), INK);
    let padding = ui.spacing().button_padding;
    let mut desired_size = galley.size() + padding * 2.0 + vec2(4.0, 3.0);
    desired_size.x = desired_size.x.max(42.0);
    desired_size.y = desired_size.y.max(29.0);
    let (rect, response) = ui.allocate_exact_size(desired_size, Sense::click());
    let fill = if response.is_pointer_button_down_on() {
        Color32::from_rgba_unmultiplied(226, 236, 250, 238)
    } else if response.hovered() {
        Color32::from_rgba_unmultiplied(255, 255, 255, 248)
    } else {
        Color32::from_rgba_unmultiplied(250, 252, 255, 218)
    };
    ui.painter().add(epaint::Shadow {
        offset: vec2(0.0, 2.0), blur: 7.0, spread: 0.0,
        color: Color32::from_black_alpha(if response.hovered() { 30 } else { 20 }),
    }.as_shape(rect, 11.0));
    ui.painter().rect_filled(rect, 11.0, fill);
    ui.painter().rect_stroke(rect, 11.0, Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 225)));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font_id, INK);
    response
}

fn empty_state(ui: &mut Ui, title: &str, detail: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("◌").size(25.0).color(Color32::from_rgb(171, 181, 196)));
        ui.add_space(6.0);
        ui.label(RichText::new(title).strong().color(INK));
        ui.add_space(3.0);
        ui.label(RichText::new(detail).size(11.0).color(INK_SOFT));
    });
    ui.add_space(20.0);
}

fn empty_timeline() -> Value {
    json!({"clips":[],"durationSeconds":MIN_PROJECT_SECONDS,"snapping":true,"magnetic":true})
}

fn default_props() -> Value {
    json!({
        "transform": {
            "position":{"x":0.0,"y":0.0},
            "scale":{"w":100.0,"h":100.0},
            "rotation":0.0,
            "opacity":100.0,
            "crop":{"left":0.0,"right":0.0,"top":0.0,"bottom":0.0},
            "distort":{"tl":0.0,"tr":0.0,"br":0.0,"bl":0.0}
        },
        "colors": {
            "light":{"exposure":0.0,"brightness":0.0,"contrast":0.0,"highlights":0.0,"shadows":0.0,"whites":0.0,"blacks":0.0},
            "color":{"temperature":0.0,"vibrance":0.0,"saturation":0.0},
            "detail":{"sharpness":0.0,"clarity":0.0,"vignette":0.0}
        },
        "effects": {
            "chromatic":{"enabled":false,"amount":28.0,"angle":0.0},
            "wave":{"enabled":false,"amount":0.0,"frequency":4.0},
            "swirl":{"enabled":false,"amount":0.0,"radius":70.0},
            "blur":{"enabled":false,"amount":0.0},
            "pixelate":{"enabled":false,"size":12.0,"blend":100.0}
        }
    })
}

fn json_number(root: &Value, path: &str, fallback: f64) -> f64 {
    let mut current = root;
    for key in path.split('.') {
        let Some(next) = current.get(key) else { return fallback; };
        current = next;
    }
    current.as_f64().filter(|value| value.is_finite()).unwrap_or(fallback)
}

fn json_bool(root: &Value, path: &str, fallback: bool) -> bool {
    let mut current = root;
    for key in path.split('.') {
        let Some(next) = current.get(key) else { return fallback; };
        current = next;
    }
    current.as_bool().unwrap_or(fallback)
}

fn set_json_path(root: &mut Value, path: &str, value: Value) {
    let parts: Vec<_> = path.split('.').filter(|part| !part.is_empty()).collect();
    let mut current = root;
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !current[*part].is_object() { current[*part] = json!({}); }
        current = &mut current[*part];
    }
    if let Some(last) = parts.last() { current[*last] = value; }
}

fn fmt_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}.{:02}", total / 60, total % 60, ((seconds.fract().abs() * 100.0).round() as u64).min(99))
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Hanabi")
            .with_inner_size([1500.0, 900.0])
            .with_min_inner_size([1100.0, 690.0])
            .with_transparent(true)
            .with_decorations(false)
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native("Hanabi", options, Box::new(|cc| Ok(Box::new(HanabiApp::new(cc)))))
}
