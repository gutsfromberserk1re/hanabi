//! Internal C-compatible API used by Hanabi's Rust UI wrapper.
//!
//! UI edits use synchronous, low-cost timeline calls. Media inspection and
//! preview jobs use submit/poll so FFmpeg never stalls the drawing thread.

use hanabi_timeline_core::{Clip, Timeline, TrackId};
use serde_json::{json, Value};
mod decoder;
mod filters;
mod renderer;
use decoder::{DecodedFrame, DecoderPool};
use renderer::{GpuLayerFrame, GpuPreviewRenderer, LayerUniforms};
use std::{
    collections::{hash_map::DefaultHasher, HashMap, VecDeque},
    ffi::{CStr, CString},
    hash::{Hash, Hasher},
    io::{Read, Write},
    os::raw::c_char,
    panic::{catch_unwind, AssertUnwindSafe},
    process::{Child, ChildStdout, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc, Condvar, Mutex, OnceLock,
    },
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

struct Backend {
    timeline: Mutex<Timeline>,
    /// One undo snapshot for an entire inspector/keyframe drag, not one per UI frame.
    property_edit: Mutex<Option<Timeline>>,
    undo: Mutex<Vec<Timeline>>,
    redo: Mutex<Vec<Timeline>>,
    next_task: AtomicU64,
    task_tx: Sender<(u64, Value)>,
    preview_task_tx: Sender<(u64, Value)>,
    proxy_task_tx: Sender<(u64, Value)>,
    event_rx: Mutex<Receiver<String>>,
    preview_inflight: Arc<AtomicBool>,
}

static BACKEND: OnceLock<Backend> = OnceLock::new();
static PREVIEW_RESTARTS: AtomicU64 = AtomicU64::new(0);
static PREVIEW_PROXY_CLIPS: AtomicU64 = AtomicU64::new(0);
static PREVIEW_ORIGINAL_CLIPS: AtomicU64 = AtomicU64::new(0);
/// Preview telemetry. PATH: 0 = none yet, 1 = WGPU compositor.
/// A visual decode/composite failure is reported as an error; there is no FFmpeg visual fallback.
static PREVIEW_PATH: AtomicU64 = AtomicU64::new(0);
/// Last WGPU preview failure reason: 0 = none, 4 = missing/empty source,
/// 5 = decoder error, 6 = GPU composite error.
static PREVIEW_FALLBACK_REASON: AtomicU64 = AtomicU64::new(0);
static PREVIEW_COMPOSITE_US: AtomicU64 = AtomicU64::new(0);
static PREVIEW_TRANSFER_US: AtomicU64 = AtomicU64::new(0);
static PREVIEW_CONVERT_US: AtomicU64 = AtomicU64::new(0);
static PREVIEW_PUBLISH_US: AtomicU64 = AtomicU64::new(0);
fn note_wgpu_preview_failure(reason: u64, detail: impl Into<String>) -> Result<Option<(u32, u32, f64)>, String> {
    PREVIEW_FALLBACK_REASON.store(reason, Ordering::Relaxed);
    Err(format!("WGPU preview failed (reason {reason}): {}. FFmpeg visual fallback is disabled to preserve preview/export consistency.", detail.into()))
}
static MEDIA_PROXIES: OnceLock<Mutex<HashMap<String, std::path::PathBuf>>> = OnceLock::new();
static PREVIEW_FRAME: OnceLock<Mutex<PreviewFrame>> = OnceLock::new();
/// Latest timeline as an immutable `Arc`. Preview requests used to clone the
/// whole timeline, serialize it to JSON on the UI thread, and re-parse it on the
/// worker for *every frame*. Now the snapshot is rebuilt only when the timeline
/// actually changed, and playback frames just clone the `Arc`.
static PREVIEW_SNAPSHOT: OnceLock<Mutex<Option<Arc<Timeline>>>> = OnceLock::new();

struct PreviewFrame {
    request_id: u64,
    width: u32,
    height: u32,
    at: f64,
    pixels: Vec<u8>,
}

impl Default for PreviewFrame {
    fn default() -> Self { Self { request_id: 0, width: 0, height: 0, at: 0.0, pixels: Vec::new() } }
}

struct PreviewStream {
    signature: u64,
    width: u32,
    height: u32,
    fps: u32,
    origin_index: u64,
    child: Child,
    ring: Arc<FrameRing>,
    script: std::path::PathBuf,
    next_index: u64,
    last_index: Option<u64>,
}

/* Decode-ahead ring. A reader thread drains FFmpeg's pipe continuously into this
   bounded queue so the preview request path normally finds its frame already
   waiting instead of blocking on the decoder. When the ring is full the reader
   stops reading, the pipe fills, and FFmpeg itself backs off. */
struct RingState {
    frames: VecDeque<(u64, Vec<u8>)>,
    free: Vec<Vec<u8>>,
    closed: bool,
    ended: bool,
}

struct FrameRing {
    state: Mutex<RingState>,
    changed: Condvar,
    capacity: usize,
}

fn preview_reader(mut stdout: ChildStdout, ring: Arc<FrameRing>, frame_bytes: usize) {
    let mut index = 0u64;
    loop {
        let mut buf = {
            let Ok(mut st) = ring.state.lock() else { return; };
            while st.frames.len() >= ring.capacity && !st.closed {
                st = match ring.changed.wait(st) { Ok(guard) => guard, Err(_) => return };
            }
            if st.closed { return; }
            st.free.pop().unwrap_or_default()
        };
        buf.resize(frame_bytes, 0);
        if stdout.read_exact(&mut buf).is_err() {
            if let Ok(mut st) = ring.state.lock() { st.ended = true; }
            ring.changed.notify_all();
            return;
        }
        {
            let Ok(mut st) = ring.state.lock() else { return; };
            if st.closed { return; }
            st.frames.push_back((index, buf));
        }
        index += 1;
        ring.changed.notify_all();
    }
}

impl Drop for PreviewStream {
    fn drop(&mut self) {
        if let Ok(mut st) = self.ring.state.lock() { st.closed = true; }
        self.ring.changed.notify_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.script);
    }
}

#[derive(Clone)]
struct DecodeSource {
    /// Stable timeline clip identity; each clip maintains its own decode cursor.
    stream_key: String,
    path: String,
    source_time: f64,
    remaining: f64,
    /// Per-layer decode width. Small picture-in-picture layers should not
    /// force a full-resolution decode when they occupy only a fraction of the canvas.
    max_width: u32,
    /// Still-image items are decoded once and reused for all timeline times.
    is_still_image: bool,
}

struct DecodeTask {
    sources: Vec<DecodeSource>,
    fps: f64,
    /// Decode ahead only while the transport is playing (the worker idles
    /// when paused/scrubbing); look-ahead during a scrub only delays the next seek.
    prefetch: bool,
    /// Explicit frame-step (arrow keys): cache the frames before a seek target.
    stepping: bool,
}

struct DecodeBatch {
    frames: Vec<DecodedFrame>,
    decode_ms: f64,
    transfer_us: u64,
    convert_us: u64,
    prefetched_frames: usize,
}

/// Decode-ahead worker: FFmpeg decoder contexts stay on one dedicated
/// thread, the current frame is returned immediately, then a small look-ahead
/// (three frames) is filled while the GPU compositor
/// renders the current frame. Look-ahead runs only during playback.
/// The request/reply channel is bounded so rapid seeks cannot build a backlog.
struct DecodeAheadWorker {
    requests: mpsc::SyncSender<DecodeTask>,
    replies: Receiver<Result<DecodeBatch, String>>,
}

impl DecodeAheadWorker {
    fn new() -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<DecodeTask>(1);
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<DecodeBatch, String>>(1);
        thread::Builder::new()
            .name("hanabi-decode-ahead".into())
            .spawn(move || decode_ahead_loop(request_rx, reply_tx))
            .expect("failed to start Hanabi decode-ahead worker");
        Self { requests: request_tx, replies: reply_rx }
    }

    fn decode(&self, task: DecodeTask) -> Result<DecodeBatch, String> {
        self.requests.send(task).map_err(|_| "Hanabi decode worker stopped".to_owned())?;
        self.replies.recv().map_err(|_| "Hanabi decode worker did not return a frame".to_owned())?
    }
}

fn decode_ahead_loop(
    requests: Receiver<DecodeTask>,
    replies: mpsc::SyncSender<Result<DecodeBatch, String>>,
) {
    let mut pool = DecoderPool::default();
    let mut pending: Option<DecodeTask> = None;
    let mut prefetched_from_previous = 0usize;
    loop {
        let task = match pending.take() {
            Some(task) => task,
            None => match requests.recv() { Ok(task) => task, Err(_) => return },
        };

        decoder::begin_decode_timing();
        let decode_started = Instant::now();
        let current: Result<Vec<DecodedFrame>, String> = task.sources.iter()
            .map(|source| {
                if source.is_still_image {
                    pool.still_frame(&source.path, source.max_width)
                } else {
                    pool.frame_at(&source.stream_key, &source.path, source.source_time, source.max_width, task.stepping)
                }
            })
            .collect();
        let decode_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
        let (transfer_us, convert_us) = decoder::take_decode_timing();
        let Ok(frames) = current else {
            let error = current.err().unwrap_or_else(|| "Video frame decode failed".into());
            if replies.send(Err(error)).is_err() { return; }
            continue;
        };

        // Return the requested frame before look-ahead. The caller can composite
        // and upload it while this worker advances FFmpeg and fills the frame cache.
        if replies.send(Ok(DecodeBatch {
            frames,
            decode_ms,
            transfer_us,
            convert_us,
            prefetched_frames: prefetched_from_previous,
        })).is_err() {
            return;
        }
        let mut prefetched = 0usize;
        'prefetch: for step in 1..=PREFETCH_FRAMES {
            if !task.prefetch { break; }
            for source in &task.sources {
                if source.is_still_image { continue; }
                // Step in *source* frames; at a 60 fps
                // preview over 30 fps footage, timeline-fps steps only covered half as far.
                let source_fps = pool.frame_rate_of(&source.path).unwrap_or(task.fps).max(1.0);
                let ahead_seconds = step as f64 / source_fps;
                // A new seek/playback request takes priority over warming old frames.
                match requests.try_recv() {
                    Ok(next) => { pending = Some(next); break 'prefetch; }
                    Err(TryRecvError::Disconnected) => return,
                    Err(TryRecvError::Empty) => {}
                }
                if ahead_seconds >= source.remaining { continue; }
                if pool.frame_at(&source.stream_key, &source.path, source.source_time + ahead_seconds, source.max_width, false).is_ok() {
                    prefetched += 1;
                }
            }
        }
        // The bounded decode cache owns the actual frames. Report the warmed
        // frame count with the next request; a fresh seek can interrupt this loop
        // at any time without building a stale playback backlog.
        prefetched_from_previous = prefetched;
    }
}

/// Frames decoded ahead of the playhead during playback (three-frame queue).
const PREFETCH_FRAMES: usize = 3;

struct PreviewEngine {
    stream: Option<PreviewStream>,
    decoder_worker: DecodeAheadWorker,
}

impl Default for PreviewEngine {
    fn default() -> Self {
        Self { stream: None, decoder_worker: DecodeAheadWorker::new() }
    }
}

fn publish_preview_buffer(request_id: u64, width: u32, height: u32, at: f64, pixels: &mut Vec<u8>, reuse_last: bool) -> Result<(), String> {
    let slot = PREVIEW_FRAME.get_or_init(|| Mutex::new(PreviewFrame::default()));
    let mut slot = slot.lock().map_err(|_| "Preview frame lock failed")?;
    if !reuse_last {
        slot.pixels.resize(pixels.len(), 0);
        std::mem::swap(&mut slot.pixels, pixels);
    }
    slot.request_id = request_id;
    slot.width = width;
    slot.height = height;
    slot.at = at;
    Ok(())
}

fn publish_black_frame(request_id: u64, width: u32, height: u32, at: f64) -> Result<(), String> {
    let slot = PREVIEW_FRAME.get_or_init(|| Mutex::new(PreviewFrame::default()));
    let mut slot = slot.lock().map_err(|_| "Preview frame lock failed")?;
    slot.pixels.resize(width as usize * height as usize * 4, 0);
    for pixel in slot.pixels.chunks_exact_mut(4) { pixel[3] = 255; }
    slot.request_id = request_id;
    slot.width = width;
    slot.height = height;
    slot.at = at;
    Ok(())
}

fn remember_media_proxy(source: &str, proxy: std::path::PathBuf) {
    let cache = MEDIA_PROXIES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut cache) = cache.lock() { cache.insert(source.to_owned(), proxy); }
}

fn preview_source_path(clip: &Clip) -> String {
    let source = clip.extra.get("path").and_then(Value::as_str).unwrap_or_default();
    let Some(cache) = MEDIA_PROXIES.get() else { return source.to_owned(); };
    let Ok(cache) = cache.lock() else { return source.to_owned(); };
    cache.get(source).filter(|proxy| proxy.is_file())
        .map(|proxy| proxy.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_owned())
}

fn backend() -> &'static Backend {
    BACKEND.get_or_init(|| {
        let (task_tx, task_rx) = mpsc::channel::<(u64, Value)>();
        let (preview_task_tx, preview_task_rx) = mpsc::channel::<(u64, Value)>();
        let (proxy_task_tx, proxy_task_rx) = mpsc::channel::<(u64, Value)>();
        let (event_tx, event_rx) = mpsc::channel::<String>();
        let preview_inflight = Arc::new(AtomicBool::new(false));
        let preview_events = event_tx.clone();
        let preview_worker_inflight = Arc::clone(&preview_inflight);
        let proxy_events = event_tx.clone();
        let proxy_tasks = proxy_task_tx.clone();
        thread::Builder::new()
            .name("hanabi-media-worker".into())
            .spawn(move || media_worker(task_rx, event_tx, proxy_tasks))
            .expect("failed to start Hanabi media worker");
        thread::Builder::new()
            .name("hanabi-preview-worker".into())
            .spawn(move || preview_worker(preview_task_rx, preview_events, preview_worker_inflight))
            .expect("failed to start Hanabi preview worker");
        thread::Builder::new()
            .name("hanabi-proxy-worker".into())
            .spawn(move || proxy_worker(proxy_task_rx, proxy_events))
            .expect("failed to start Hanabi proxy worker");
        Backend {
            timeline: Mutex::new(Timeline::default()),
            property_edit: Mutex::new(None),
            undo: Mutex::new(Vec::new()),
            redo: Mutex::new(Vec::new()),
            next_task: AtomicU64::new(1),
            task_tx,
            preview_task_tx,
            proxy_task_tx,
            event_rx: Mutex::new(event_rx),
            preview_inflight,
        }
    })
}

fn media_worker(tasks: Receiver<(u64, Value)>, events: Sender<String>, proxy_tasks: Sender<(u64, Value)>) {
    while let Ok((id, request)) = tasks.recv() {
        let command = request["command"].as_str().unwrap_or_default().to_owned();
        let result = match command.as_str() {
            "inspect_media" => {
                let source = request["path"].as_str().unwrap_or_default();
                let result = inspect_media(source);
                if let Ok(metadata) = &result {
                    if let Some(proxy_path) = metadata["proxyPath"].as_str().filter(|value| !value.is_empty()) {
                        let proxy_request = json!({"command":"generate_proxy", "path":source, "proxyPath":proxy_path});
                        let _ = proxy_tasks.send((id | (1u64 << 63), proxy_request));
                    }
                }
                result
            }
            "export_project" => export_project(&request, id),
            "" => Err("Background command is required".into()),
            other => Err(format!("Unsupported background command: {other}")),
        };
        let response = match result {
            Ok(value) => {
                let mut fields = value.as_object().cloned().unwrap_or_default();
                fields.insert("requestId".into(), json!(id));
                fields.insert("command".into(), json!(command));
                fields.insert("ok".into(), json!(true));
                Value::Object(fields)
            }
            Err(error) => json!({"requestId": id, "command": command, "ok": false, "error": error}),
        };
        if events.send(response.to_string()).is_err() {
            break;
        }
    }
}

fn preview_worker(tasks: Receiver<(u64, Value)>, events: Sender<String>, preview_inflight: Arc<AtomicBool>) {
    /* The preview process and its pipe are owned exclusively by this thread.
       That gives FFmpeg one clear owner and keeps decode waits off the UI and
       media-import/export workers. */
    let mut engine = PreviewEngine::default();
    while let Ok((id, request)) = tasks.recv() {
        let queued_ms = request["submittedAtMs"].as_u64().unwrap_or(0);
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let queue_ms = now_ms.saturating_sub(queued_ms);
        let total_started = Instant::now();
        let (result, setup_ms, decode_ms, ring_depth) = engine.render(&request, id);
        preview_inflight.store(false, Ordering::Release);
        let response = match result {
            Ok((width, height, at)) => json!({"requestId":id,"frameId":id,"command":"preview_frame","ok":true,
                "width":width,"height":height,"at":at,"queueMs":queue_ms,
                "streamSetupMs":setup_ms,"decodeWaitMs":decode_ms,"ringDepth":ring_depth,
                "hwDecode":decoder::last_frame_was_hardware() as u8,
                "path":PREVIEW_PATH.load(Ordering::Relaxed),"fallbackReason":PREVIEW_FALLBACK_REASON.load(Ordering::Relaxed),
                "compositeMs":PREVIEW_COMPOSITE_US.load(Ordering::Relaxed) as f64/1000.0,
                "publishMs":PREVIEW_PUBLISH_US.load(Ordering::Relaxed) as f64/1000.0,
                "transferMs":PREVIEW_TRANSFER_US.load(Ordering::Relaxed) as f64/1000.0,
                "convertMs":PREVIEW_CONVERT_US.load(Ordering::Relaxed) as f64/1000.0,
                "restarts":PREVIEW_RESTARTS.load(Ordering::Relaxed),"proxyClips":PREVIEW_PROXY_CLIPS.load(Ordering::Relaxed),"originalClips":PREVIEW_ORIGINAL_CLIPS.load(Ordering::Relaxed),
                "totalMs":total_started.elapsed().as_secs_f64()*1000.0}),
            Err(error) => json!({"requestId":id,"command":"preview_frame","ok":false,"error":error,
                "queueMs":queue_ms,"streamSetupMs":setup_ms,"decodeWaitMs":decode_ms,
                "totalMs":total_started.elapsed().as_secs_f64()*1000.0}),
        };
        if events.send(response.to_string()).is_err() { break; }
    }
}

fn proxy_worker(tasks: Receiver<(u64, Value)>, events: Sender<String>) {
    while let Ok((id, request)) = tasks.recv() {
        let source = request["path"].as_str().unwrap_or_default();
        let proxy = request["proxyPath"].as_str().unwrap_or_default();
        let result = (|| -> Result<Value, String> {
            if source.is_empty() || proxy.is_empty() { return Err("Proxy source and destination are required".into()); }
            let destination = std::path::Path::new(proxy);
            if destination.is_file() { return Ok(json!({"proxyPath": proxy, "cached": true})); }
            if !std::path::Path::new(source).is_file() { return Err(format!("Media file is missing: {source}")); }
            let ffmpeg = multimedia_binary("ffmpeg");
            let temporary = destination.with_file_name(format!("proxy-render-{id}.mp4"));
            let mut command = media_command(&ffmpeg);
            command.args(["-hide_banner", "-loglevel", "error", "-i"]).arg(source)
                .args(["-vf", "scale='min(1920,iw)':'min(1080,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2",
                       "-an", "-threads", "2", "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
                       "-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-bf", "0",
                       "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-y"])
                .arg(&temporary);
            let output = command.output().map_err(|e| format!("Could not start FFmpeg proxy encode: {e}"))?;
            if !output.status.success() {
                let _ = std::fs::remove_file(&temporary);
                return Err(ffmpeg_error(output));
            }
            if !destination.exists() {
                std::fs::rename(&temporary, destination).map_err(|e| format!("Could not finalize media proxy: {e}"))?;
            } else {
                let _ = std::fs::remove_file(&temporary);
            }
            Ok(json!({"proxyPath": proxy, "cached": false}))
        })();
        let response = match result {
            Ok(value) => {
                let mut fields = value.as_object().cloned().unwrap_or_default();
                fields.insert("requestId".into(), json!(id));
                fields.insert("command".into(), json!("generate_proxy"));
                fields.insert("ok".into(), json!(true));
                Value::Object(fields)
            }
            Err(error) => json!({"requestId": id, "command": "generate_proxy", "ok": false, "error": error}),
        };
        if events.send(response.to_string()).is_err() { break; }
    }
}

fn media_content_hash(path: &std::path::Path) -> Result<u64, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("Could not read media for cache: {e}"))?;
    let mut hasher = DefaultHasher::new();
    let mut block = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut block).map_err(|e| format!("Could not hash media: {e}"))?;
        if read == 0 { break; }
        block[..read].hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn inspect_media(path: &str) -> Result<Value, String> {
    use std::{
        path::Path,
    };
    let path = Path::new(path);
    if !path.is_file() {
        return Err("The selected media file could not be found".into());
    }
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = if ["png", "jpg", "jpeg", "webp", "gif"].contains(&extension.as_str()) {
        "image"
    } else if ["mp3", "wav", "m4a", "aac", "flac", "ogg"].contains(&extension.as_str()) {
        "audio"
    } else if ["mp4", "mov", "mkv", "avi", "webm", "m4v"].contains(&extension.as_str()) {
        "video"
    } else {
        return Err("This file type is not supported".into());
    };

    let mut duration = 5.0;
    let mut has_audio = false;
    if kind != "image" {
        let output = media_command(multimedia_binary("ffprobe"))
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration:stream=codec_type,duration",
                "-of",
                "json",
            ])
            .arg(path)
            .output()
            .map_err(|e| format!("Could not start FFprobe: {e}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        let data: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("Invalid FFprobe output: {e}"))?;
        let seconds = |v: &Value| {
            v.as_f64()
                .or_else(|| v.as_str()?.parse().ok())
                .filter(|n: &f64| n.is_finite() && *n > 0.0)
        };
        has_audio = data["streams"]
            .as_array()
            .is_some_and(|streams| streams.iter().any(|s| s["codec_type"] == "audio"));
        duration = seconds(&data["format"]["duration"])
            .or_else(|| {
                data["streams"]
                    .as_array()?
                    .iter()
                    .filter_map(|s| seconds(&s["duration"]))
                    .reduce(f64::max)
            })
            .ok_or_else(|| "FFprobe did not report a usable duration".to_owned())?;
    }

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("Media");
    let media_hash = media_content_hash(path)?;
    let cache_dir = std::env::temp_dir().join("hanabi-media-cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let waveform = cache_dir.join(format!("{media_hash:016x}-wave-v7.png"));
    let thumbnail = cache_dir.join(format!("{media_hash:016x}-thumb-v3.bmp"));
    let proxy = if kind == "video" {
        cache_dir.join(format!("{media_hash:016x}-proxy-1080p-g30-v4.mp4"))
    } else { std::path::PathBuf::new() };
    if kind == "video" { remember_media_proxy(&path.to_string_lossy(), proxy.clone()); }
    let ffmpeg = multimedia_binary("ffmpeg");
    let waveform_path = if has_audio {
        let generated = if waveform.is_file() {
            true
        } else {
            media_command(&ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-filter_complex", "showwavespic=s=1024x128:colors=white:scale=sqrt:draw=full:filter=peak", "-frames:v", "1", "-y"])
                .arg(&waveform)
                .status()
                .is_ok_and(|status| status.success())
        };
        generated.then(|| waveform.to_string_lossy().into_owned())
    } else { None };
    // Raylib streams cached audio proxies so MP4/MOV audio never needs to be
    // decoded by FFmpeg during playback. OGG is preferred; MP3 is the fallback
    // for bundled FFmpeg builds that omit libvorbis.
    let audio_playback = cache_dir.join(format!("{media_hash:016x}-playback-v2.ogg"));
    let audio_playback_path = if has_audio {
        let ogg_ready = if audio_playback.metadata().is_ok_and(|meta| meta.len() > 256) {
            true
        } else {
            let _ = std::fs::remove_file(&audio_playback);
            media_command(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(path)
                .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "2", "-ar", "48000", "-c:a", "libvorbis", "-q:a", "4", "-y"])
                .arg(&audio_playback)
                .status()
                .is_ok_and(|status| status.success())
                && audio_playback.metadata().is_ok_and(|meta| meta.len() > 256)
        };
        if ogg_ready {
            Some(audio_playback.to_string_lossy().into_owned())
        } else {
            let mp3 = cache_dir.join(format!("{media_hash:016x}-playback-v2.mp3"));
            let mp3_ready = if mp3.metadata().is_ok_and(|meta| meta.len() > 256) {
                true
            } else {
                let _ = std::fs::remove_file(&mp3);
                media_command(&ffmpeg)
                    .args(["-hide_banner", "-loglevel", "error", "-i"])
                    .arg(path)
                    .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "2", "-ar", "48000", "-c:a", "libmp3lame", "-q:a", "3", "-y"])
                    .arg(&mp3)
                    .status()
                    .is_ok_and(|status| status.success())
                    && mp3.metadata().is_ok_and(|meta| meta.len() > 256)
            };
            mp3_ready.then(|| mp3.to_string_lossy().into_owned())
        }
    } else { None };
    let thumbnail_path = if kind == "audio" {
        None
    } else {
        let generated = if thumbnail.metadata().is_ok_and(|meta| meta.len() > 128) {
            true
        } else {
            let _ = std::fs::remove_file(&thumbnail);
            let mut command = media_command(&ffmpeg);
            command.args(["-v", "error"]);
            if kind == "video" {
                let seek = format!("{:.3}", (duration * 0.1).min(2.0));
                command.args(["-ss", seek.as_str()]);
            }
            command.arg("-i").arg(path)
                .args(["-frames:v", "1", "-vf", "scale=320:180:force_original_aspect_ratio=decrease,pad=320:180:(ow-iw)/2:(oh-ih)/2:color=black", "-y"])
                .arg(&thumbnail);
            let first_attempt = command.status().is_ok_and(|status| status.success())
                && thumbnail.metadata().is_ok_and(|meta| meta.len() > 128);
            if first_attempt {
                true
            } else {
                let _ = std::fs::remove_file(&thumbnail);
                let cli_fallback = kind == "video" && media_command(&ffmpeg)
                    .args(["-v", "error", "-i"])
                    .arg(path)
                    .args(["-frames:v", "1", "-vf", "scale=320:180:force_original_aspect_ratio=decrease,pad=320:180:(ow-iw)/2:(oh-ih)/2:color=black", "-y"])
                    .arg(&thumbnail)
                    .status()
                    .is_ok_and(|status| status.success())
                    && thumbnail.metadata().is_ok_and(|meta| meta.len() > 128);
                if cli_fallback {
                    true
                } else if kind != "audio" {
                    // The lightweight package may not ship the FFmpeg CLI. Use the
                    // in-process decoder as a bounded 320x180 fallback for visual media.
                    let _ = std::fs::remove_file(&thumbnail);
                    path.to_str().is_some_and(|source| {
                        decoder::write_thumbnail(source, &thumbnail, 320, 180).is_ok()
                            && thumbnail.metadata().is_ok_and(|meta| meta.len() > 128)
                    })
                } else {
                    false
                }
            }
        };
        generated.then(|| thumbnail.to_string_lossy().into_owned())
    };
    Ok(json!({
        "path": path.to_string_lossy(), "name": name, "duration": duration,
        "kind": kind, "hasAudio": has_audio,
        "proxyPath": proxy.to_string_lossy(),
        "audioPlaybackPath": audio_playback_path,
        "filmstripPath": Value::Null, "waveformPath": waveform_path, "thumbnailPath": thumbnail_path
    }))
}

fn multimedia_binary(name: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    if let Ok(dir) = std::env::var("HANABI_FFMPEG_DIR") {
        let candidate = PathBuf::from(dir).join(&filename);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for path in [
                dir.join(&filename),
                dir.join("ffmpeg").join(&filename),
                dir.join("resources/ffmpeg").join(&filename),
            ] {
                if path.is_file() {
                    return path;
                }
            }
        }
    }
    PathBuf::from(filename)
}

fn media_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

fn ffmpeg_uses_option_files(binary: &std::path::Path) -> bool {
    static SUPPORTS_OPTION_FILES: OnceLock<bool> = OnceLock::new();
    *SUPPORTS_OPTION_FILES.get_or_init(|| {
        media_command(binary).arg("-version").output().ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|version| {
                let version = version.split_whitespace().nth(2)?;
                if version.starts_with("N-") || version.starts_with("git-") { Some(true) }
                else {
                    let mut parts = version.split('.');
                    let major = parts.next()?.parse::<u32>().ok()?;
                    let minor = parts.next().unwrap_or("0").parse::<u32>().ok()?;
                    Some(major > 7 || (major == 7 && minor >= 1))
                }
            })
            .unwrap_or(false)
    })
}

fn append_filter_graph_arg(command: &mut std::process::Command, binary: &std::path::Path, path: &std::path::Path) {
    if ffmpeg_uses_option_files(binary) {
        command.arg("-/filter_complex").arg(path);
    } else {
        command.arg("-filter_complex_script").arg(path);
    }
}

#[derive(Clone)]
struct RenderInput { path: String, image: bool, seek: f64 }

fn render_inputs(timeline: &Timeline, deduplicate: bool) -> Result<(Vec<RenderInput>, HashMap<String, usize>), String> {
    let mut clips: Vec<_> = timeline.clips.iter().filter(|clip| clip.extra.get("path").and_then(Value::as_str).is_some()).collect();
    clips.sort_by(|a, b| {
        let order = |track: TrackId| track.sort_index();
        order(a.track).cmp(&order(b.track)).then_with(|| a.start.total_cmp(&b.start))
    });
    let mut inputs: Vec<RenderInput> = Vec::with_capacity(clips.len());
    let mut indices = HashMap::with_capacity(clips.len());
    let mut path_indices = HashMap::with_capacity(clips.len());
    for clip in clips {
        let path = clip.extra.get("path").and_then(Value::as_str).unwrap_or_default();
        if !std::path::Path::new(&path).is_file() { return Err(format!("Media file is missing: {path}")); }
        if deduplicate {
            if let Some(&input_index) = path_indices.get(path) {
                indices.insert(clip.id.clone(), input_index);
                continue;
            }
        }
        let image = clip.extra.get("kind").and_then(Value::as_str) == Some("image");
        let input_index = inputs.len();
        indices.insert(clip.id.clone(), input_index);
        if deduplicate { path_indices.insert(path.to_owned(), input_index); }
        let seek = clip.extra.get("previewSeek").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        inputs.push(RenderInput { path: path.to_owned(), image, seek });
    }
    Ok((inputs, indices))
}

fn project_duration(timeline: &Timeline) -> f64 {
    timeline.clips.iter().map(Clip::end).fold(0.0, f64::max)
}

fn transition_out(clip: &Clip) -> Option<(String, f64)> {
    let transition = &clip.extra["props"]["transitionOut"];
    let stored_kind = transition["type"].as_str()?;
    let kind = match stored_kind {
        "none" => return None,
        "crossfade" | "fade" | "dissolve" => "crossfade",
        "wipe" | "wipeleft" => "wipe",
        "iris" | "circleopen" => "iris",
        "slide" | "slideleft" => "slide",
        "dip" | "fadeblack" => "dip",
        "zoom" | "zoomin" => "zoom",
        "spin" => "spin",
        "blur" | "hblur" => "blur",
        "checker" | "pixelize" => "checker",
        value => value,
    };
    if !matches!(kind,
        "fade" | "crossfade" | "dissolve" | "dip" |
        "wipe" | "wipeleft" | "wiperight" | "wipeup" | "wipedown" |
        "diagtl" | "diagtr" | "diagbl" | "diagbr" |
        "slide" | "slideleft" | "slideright" | "slideup" | "slidedown" |
        "iris" | "circleopen" | "circleclose" | "radial" |
        "zoom" | "zoomin" | "spin" | "blur" | "checker" |
        "squeezeh" | "squeezev")
    { return None; }
    let duration = transition["duration"].as_f64().filter(|v| v.is_finite()).unwrap_or(1.0).max(0.05).min(clip.duration.min(3.0).max(0.05));
    Some((kind.to_owned(), duration))
}

fn transition_between(left: &Clip, right: &Clip) -> Option<(String, f64)> {
    if left.track != right.track || (left.end() - right.start).abs() > 0.04 { return None; }
    let (kind, duration) = transition_out(left)?;
    let preserve_preview_duration = left.extra.get("previewSeekAdjusted").and_then(Value::as_bool) == Some(true);
    let duration = if preserve_preview_duration { duration } else { duration.min(left.duration * 0.5).min(right.duration * 0.5) };
    (duration > 0.0).then_some((kind, duration))
}

/// Neighbor pairs are discovered within each video lane, never by looking at
/// adjacent entries in a globally sorted multi-lane list. An overlay clip on
/// V2 must not break a transition between two consecutive clips on V1.
fn same_track_neighbor_indices(clips: &[&Clip]) -> Vec<(usize, usize)> {
    let mut order: Vec<usize> = (0..clips.len()).collect();
    order.sort_by(|left, right| {
        clips[*left].track.sort_index().cmp(&clips[*right].track.sort_index())
            .then_with(|| clips[*left].start.total_cmp(&clips[*right].start))
            .then_with(|| clips[*left].id.cmp(&clips[*right].id))
    });
    order.windows(2).filter_map(|pair| {
        (clips[pair[0]].track == clips[pair[1]].track).then_some((pair[0], pair[1]))
    }).collect()
}

fn transition_preroll_source_time(source_offset: f64, duration: f64, elapsed: f64) -> f64 {
    let duration = duration.max(0.0);
    let source_offset = source_offset.max(0.0);
    let available_preroll = source_offset.min(duration);
    let missing_preroll = duration - available_preroll;
    let source_start = source_offset - available_preroll;
    source_start + (elapsed.max(0.0) - missing_preroll).max(0.0)
}

fn timeline_transition_start_at(timeline: &Timeline, at: f64) -> Option<f64> {
    let mut clips: Vec<_> = timeline.clips.iter()
        .filter(|clip| clip.track.is_video() && clip.extra.get("kind").and_then(Value::as_str) != Some("audio"))
        .collect();
    clips.sort_by(|a, b| {
        let rank = |track: TrackId| track.sort_index();
        rank(a.track).cmp(&rank(b.track)).then_with(|| a.start.total_cmp(&b.start))
    });
    same_track_neighbor_indices(&clips).into_iter().find_map(|(left, right)| {
        let (_, duration) = transition_between(clips[left], clips[right])?;
        let start = clips[right].start - duration;
        (at >= start - 0.02 && at <= clips[right].start + 0.02).then_some(start.max(0.0))
    })
}

fn split_source_streams(
    graph: &mut Vec<String>,
    clips: &[&Clip],
    indices: &HashMap<String, usize>,
    video: bool,
    prefix: &str,
) -> HashMap<String, String> {
    let mut by_input: HashMap<usize, Vec<&Clip>> = HashMap::new();
    for clip in clips {
        if let Some(&input) = indices.get(&clip.id) { by_input.entry(input).or_default().push(*clip); }
    }
    let stream = if video { "v" } else { "a" };
    let splitter = if video { "split" } else { "asplit" };
    let mut labels_by_clip = HashMap::with_capacity(clips.len());
    for (input, uses) in by_input {
        let labels: Vec<_> = uses.iter().enumerate()
            .map(|(ordinal, clip)| format!("{prefix}_{}_{}", clip.id.replace(|c: char| !c.is_ascii_alphanumeric(), "_"), ordinal))
            .collect();
        if labels.len() == 1 {
            let passthrough = if video { "null" } else { "anull" };
            graph.push(format!("[{input}:{stream}]{passthrough}[{}]", labels[0]));
        } else {
            graph.push(format!("[{input}:{stream}]{splitter}={}[{}]", labels.len(), labels.join("][")));
        }
        for (clip, label) in uses.into_iter().zip(labels) { labels_by_clip.insert(clip.id.clone(), label); }
    }
    labels_by_clip
}

fn export_filter_graph(timeline: &Timeline, width: u32, height: u32, fps: u32) -> Result<(String, Vec<RenderInput>, f64, bool), String> {
    export_filter_graph_mode(timeline, width, height, fps, true)
}

fn export_filter_graph_mode(timeline: &Timeline, width: u32, height: u32, fps: u32, deduplicate: bool) -> Result<(String, Vec<RenderInput>, f64, bool), String> {
    let duration = project_duration(timeline);
    if duration <= 0.0 { return Err("Add at least one clip before exporting".into()); }
    let (inputs, indices) = render_inputs(timeline, deduplicate)?;
    let mut graph = vec![format!("color=c=black:s={width}x{height}:r={fps}:d={duration:.5},format=rgba[base0]")];
    let mut video_clips: Vec<_> = timeline.clips.iter().filter(|clip| clip.track.is_video() && clip.extra.get("kind").and_then(Value::as_str) != Some("audio") && indices.contains_key(&clip.id)).collect();
    video_clips.sort_by(|a, b| {
        let order = |track: TrackId| track.sort_index();
        order(a.track).cmp(&order(b.track)).then_with(|| a.start.total_cmp(&b.start))
    });
    let mut outgoing = vec![None; video_clips.len()];
    for (left, right) in same_track_neighbor_indices(&video_clips) {
        outgoing[left] = transition_between(video_clips[left], video_clips[right]);
    }
    let mut incoming = vec![None; video_clips.len()];
    for i in 1..video_clips.len() { incoming[i] = outgoing[i - 1].clone(); }
    let clip_streams = split_source_streams(&mut graph, &video_clips, &indices, true, "vsrc");
    let mut video_branches: Vec<(String, Option<String>, Option<String>)> = Vec::with_capacity(video_clips.len());
    for (i, clip) in video_clips.iter().enumerate() {
        let source = &clip_streams[&clip.id];
        let has_incoming = incoming[i].is_some();
        let has_outgoing = outgoing[i].is_some();
        let mut labels = Vec::with_capacity(1 + usize::from(has_incoming) + usize::from(has_outgoing));
        labels.push(format!("v{i}_body_src"));
        if has_incoming { labels.push(format!("v{i}_in_src")); }
        if has_outgoing { labels.push(format!("v{i}_out_src")); }
        if labels.len() == 1 {
            graph.push(format!("[{source}]null[{}]", labels[0]));
        } else {
            graph.push(format!("[{source}]split={}[{}]", labels.len(), labels.join("][")));
        }
        let mut at = 1;
        let incoming_source = if has_incoming { let label = labels[at].clone(); at += 1; Some(label) } else { None };
        let outgoing_source = if has_outgoing { Some(labels[at].clone()) } else { None };
        video_branches.push((labels[0].clone(), incoming_source, outgoing_source));
    }
    let mut body_layers: Vec<Option<(String, String, String)>> = vec![None; video_clips.len()];
    for (i, clip) in video_clips.iter().enumerate() {
        let value = serde_json::to_value(clip).unwrap_or_default();
        let (filters, x, y) = filters::video_filters(&value, width, height, clip.start, clip.duration, None, None);
        let label = format!("vbody{i}");
        graph.push(format!("[{}]{filters}[{label}]", video_branches[i].0));
        body_layers[i] = Some((label, x, y));
    }

    let mut transition_layers = vec![None; video_clips.len()];
    for i in 0..video_clips.len().saturating_sub(1) {
        let Some((kind, duration)) = outgoing[i].as_ref() else { continue; };
        let left = video_clips[i];
        let right = video_clips[i + 1];
        let transition_start = right.start - duration;
        let left_source = left.extra.get("sourceOffset").and_then(Value::as_f64).unwrap_or(0.0) + left.duration - duration;
        let requested_right_source = right.extra.get("sourceOffset").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let available_preroll = requested_right_source.min(*duration);
        let missing_preroll = (*duration - available_preroll).max(0.0);
        let right_source = requested_right_source - available_preroll;
        let mut left_value = serde_json::to_value(left).map_err(|e| e.to_string())?;
        let mut right_value = serde_json::to_value(right).map_err(|e| e.to_string())?;
        left_value["sourceOffset"] = json!(left_source);
        right_value["sourceOffset"] = json!(right_source);
        let (left_filters, left_x, left_y) = filters::video_filters(&left_value, width, height, 0.0, *duration, None, None);
        let (mut right_filters, right_x, right_y) = filters::video_filters(&right_value, width, height, 0.0, *duration, None, None);
        if missing_preroll > 1.0 / fps.max(1) as f64 {
            right_filters.push_str(&format!(",tpad=start_mode=clone:start_duration={missing_preroll:.5}"));
        }
        let left_raw = format!("tx{i}a_raw");
        let right_raw = format!("tx{i}b_raw");
        let left_full = format!("tx{i}a");
        let right_full = format!("tx{i}b");
        let output = format!("tx{i}_out");
        let left_bg = format!("tx{i}a_bg");
        let right_bg = format!("tx{i}b_bg");
        graph.push(format!("[{}]{left_filters},fps={fps},settb=AVTB,format=rgba[{left_raw}]", video_branches[i].2.as_ref().unwrap()));
        graph.push(format!("[{}]{right_filters},fps={fps},settb=AVTB,format=rgba[{right_raw}]", video_branches[i + 1].1.as_ref().unwrap()));
        graph.push(format!("color=c=black@0:s={width}x{height}:r={fps}:d={duration:.5},format=rgba,settb=AVTB[{left_bg}]"));
        graph.push(format!("color=c=black@0:s={width}x{height}:r={fps}:d={duration:.5},format=rgba,settb=AVTB[{right_bg}]"));
        graph.push(format!("[{left_bg}][{left_raw}]overlay=x={left_x}:y={left_y}:shortest=1:repeatlast=0:format=auto,format=rgba,setpts=PTS-STARTPTS[{left_full}]"));
        graph.push(format!("[{right_bg}][{right_raw}]overlay=x={right_x}:y={right_y}:shortest=1:repeatlast=0:format=auto,format=rgba,setpts=PTS-STARTPTS[{right_full}]"));
        let ffmpeg_kind = match kind.as_str() {
            "crossfade" => "fade",
            "dip" => "fadeblack",
            "zoom" => "zoomin",
            "spin" => "radial",
            "blur" => "hblur",
            "checker" => "pixelize",
            "wipe" => "wipeleft",
            "iris" => "circleopen",
            "slide" => "slideleft",
            value => value,
        };
        graph.push(format!("[{left_full}][{right_full}]xfade=transition={ffmpeg_kind}:duration={duration:.5}:offset=0,format=rgba,setpts=PTS-STARTPTS+{transition_start:.5}/TB[{output}]"));
        transition_layers[i] = Some(output);
    }

    let mut base = "base0".to_owned();
    let mut layer_index = 0;
    for i in 0..video_clips.len() {
        if let Some((label, x, y)) = body_layers[i].as_ref() {
            let next = format!("base{}", layer_index + 1);
            graph.push(format!("[{base}][{label}]overlay=x={x}:y={y}:eval=frame:eof_action=pass:repeatlast=0:format=auto[{next}]"));
            base = next;
            layer_index += 1;
        }
        if let Some(label) = transition_layers[i].as_ref() {
            let next = format!("base{}", layer_index + 1);
            graph.push(format!("[{base}][{label}]overlay=x=0:y=0:eof_action=pass:repeatlast=0:format=auto[{next}]"));
            base = next;
            layer_index += 1;
        }
    }
    graph.push(format!("[{base}]format=yuv420p[vout]"));

    let mut audio_labels = Vec::new();
    for clip in &timeline.clips {
        let is_audio = !clip.track.is_video() || clip.extra.get("hasAudio").and_then(Value::as_bool).unwrap_or(false);
        let Some(&input) = indices.get(&clip.id) else { continue; };
        if !is_audio { continue; }
        let kind = clip.extra.get("kind").and_then(Value::as_str).unwrap_or_default();
        if kind == "image" { continue; }
        let value = serde_json::to_value(clip).map_err(|e| e.to_string())?;
        let af = filters::audio_filters(&value, clip.duration, duration);
        let label = format!("a{}", audio_labels.len());
        graph.push(format!("[{input}:a]{af}[{label}]"));
        audio_labels.push(label);
    }
    if audio_labels.len() == 1 {
        // The master limiter is also applied to a single source so export has
        // the same predictable ceiling regardless of how many clips overlap.
        graph.push(format!("[{}]atrim=duration={duration:.5},asetpts=PTS-STARTPTS,alimiter=limit=0.95:attack=5:release=50[aout]", audio_labels[0]));
    } else if !audio_labels.is_empty() {
        let inputs = audio_labels.iter().map(|label| format!("[{label}]")).collect::<String>();
        // Mix every active timeline lane first, then protect the master bus from
        // clipping when several full-level clips overlap.
        graph.push(format!("{inputs}amix=inputs={}:duration=longest:normalize=0,atrim=duration={duration:.5},asetpts=PTS-STARTPTS,alimiter=limit=0.95:attack=5:release=50[aout]", audio_labels.len()));
    }
    Ok((graph.join(";"), inputs, duration, !audio_labels.is_empty()))
}

fn filter_script(id: u64, contents: &str) -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join("hanabi-media-cache");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create render cache: {e}"))?;
    let path = dir.join(format!("graph-{id}.txt"));
    std::fs::write(&path, contents).map_err(|e| format!("Could not write FFmpeg graph: {e}"))?;
    Ok(path)
}

fn ffmpeg_error(output: std::process::Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    if detail.is_empty() { "FFmpeg could not render this request".into() }
    else { detail.chars().take(320).collect() }
}

fn resolved_export_path(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if path.exists() {
        return path.canonicalize().map_err(|error| format!("Invalid export path: {error}"));
    }
    let file_name = path.file_name().ok_or("Export path must include a file name")?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let parent = parent.canonicalize().map_err(|error| format!("Export folder is unavailable: {error}"))?;
    Ok(parent.join(file_name))
}

fn same_filesystem_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    #[cfg(windows)]
    { left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy()) }
    #[cfg(not(windows))]
    { left == right }
}

fn ensure_export_does_not_overwrite_input(output: &str, inputs: &[RenderInput]) -> Result<(), String> {
    let output_path = resolved_export_path(std::path::Path::new(output))?;
    for input in inputs {
        let input_path = std::path::Path::new(&input.path).canonicalize()
            .map_err(|error| format!("Could not resolve source media path: {error}"))?;
        if same_filesystem_path(&output_path, &input_path) {
            return Err(format!("Choose a different export file. The selected output is the same file as source media: {}", input.path));
        }
    }
    Ok(())
}

/// Every visible video-track clip is processed by the WGPU compositor.
/// Audio-only tracks stay in FFmpeg's audio graph and never enter this renderer.
fn is_wgpu_visual_clip(clip: &Clip) -> bool {
    clip.track.is_video() && clip.extra.get("kind").and_then(Value::as_str) != Some("audio")
}

fn export_source_width(width: u32, height: u32) -> u32 {
    width.max(height).clamp(160, 3840) & !1
}

/// Render one timeline timestamp using the exact same ordering, keyframe
/// evaluator, transition uniforms, and WGPU compositor as native preview.
fn render_wgpu_export_frame(
    timeline: &Timeline,
    frame_at: f64,
    width: u32,
    height: u32,
    decoder_pool: &mut DecoderPool,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    let mut ordered_video: Vec<&Clip> = timeline.clips.iter().filter(|clip| {
        clip.track.is_video() && clip.extra.get("kind").and_then(Value::as_str) != Some("audio")
    }).collect();
    ordered_video.sort_by(|a, b| {
        a.track.sort_index().cmp(&b.track.sort_index())
            .then_with(|| a.start.total_cmp(&b.start))
    });

    let mut incoming_transitions: HashMap<String, (String, f64)> = HashMap::new();
    let mut outgoing_transitions: HashMap<String, (String, f64)> = HashMap::new();
    for (left_index, right_index) in same_track_neighbor_indices(&ordered_video) {
        let left = ordered_video[left_index];
        let right = ordered_video[right_index];
        let Some((kind, duration)) = transition_between(left, right) else { continue; };
        let transition_start = right.start - duration;
        if frame_at + 1e-7 >= transition_start && frame_at < right.start - 1e-7 {
            incoming_transitions.insert(right.id.clone(), (kind.clone(), duration));
            if kind == "dip" {
                outgoing_transitions.insert(left.id.clone(), (kind, duration));
            }
        }
    }

    let mut active: Vec<&Clip> = timeline.clips.iter().filter(|clip| {
        clip.track.is_video()
            && clip.extra.get("kind").and_then(Value::as_str) != Some("audio")
            && frame_at + 1e-7 >= clip.start
            && frame_at < clip.end() - 1e-7
    }).collect();
    for clip in &ordered_video {
        if incoming_transitions.contains_key(&clip.id) && !active.iter().any(|current| current.id == clip.id) {
            active.push(*clip);
        }
    }
    active.sort_by(|a, b| {
        a.track.sort_index().cmp(&b.track.sort_index())
            .then_with(|| a.start.total_cmp(&b.start))
    });

    let max_source_width = export_source_width(width, height);
    let mut decoded_frames = Vec::with_capacity(active.len());
    let mut uniforms = Vec::with_capacity(active.len());
    let mut source_keys = Vec::with_capacity(active.len());

    for clip in &active {
        let path = clip.extra.get("path").and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| format!("Video clip '{}' has no source path", clip.id))?;
        let source_offset = clip.extra.get("sourceOffset").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let source_time = if let Some((_, duration)) = incoming_transitions.get(&clip.id) {
            transition_preroll_source_time(source_offset, *duration, frame_at - (clip.start - duration))
        } else {
            (source_offset + (frame_at - clip.start)).max(0.0)
        };
        let props = clip.extra.get("props").unwrap_or(&Value::Null);
        let keyframes = clip.extra.get("keyframes").unwrap_or(&Value::Null);
        let transition = incoming_transitions.get(&clip.id).map(|(kind, duration)| {
            (kind.as_str(), ((frame_at - (clip.start - duration)) / duration.max(1e-9)) as f32)
        });
        let transition_out = outgoing_transitions.get(&clip.id).map(|(kind, duration)| {
            (kind.as_str(), ((frame_at - (clip.end() - duration)) / duration.max(1e-9)) as f32)
        });
        let layer_uniforms = LayerUniforms::from_props_with_keyframes_and_transitions(
            props, frame_at, keyframes, transition, transition_out,
        );
        // Match preview's resolution-aware decode policy, but use the export
        // canvas dimensions instead of its smaller preview-quality dimensions.
        let layer_scale = layer_uniforms.geo0[0].max(layer_uniforms.geo0[1]).clamp(0.01, 10.0);
        let layer_max_width = ((max_source_width as f32 * layer_scale).ceil() as u32)
            .clamp(160, max_source_width);
        let is_still_image = clip.extra.get("kind").and_then(Value::as_str) == Some("image");
        let frame = if is_still_image {
            decoder_pool.still_frame(path, layer_max_width)?
        } else {
            decoder_pool.frame_at(&clip.id, path, source_time, layer_max_width, false)?
        };
        let source_key = if is_still_image {
            format!("still:{}:{}:{}x{}", path, frame.pts_seconds.to_bits(), frame.width, frame.height)
        } else {
            format!("{}:{}:{}x{}", path, frame.pts_seconds.to_bits(), frame.width, frame.height)
        };
        source_keys.push(source_key);
        uniforms.push(layer_uniforms);
        decoded_frames.push(frame);
    }

    let layers: Vec<GpuLayerFrame<'_>> = active.iter().enumerate().map(|(index, clip)| {
        let frame = &decoded_frames[index];
        GpuLayerFrame {
            key: clip.id.as_str(),
            source_key: source_keys[index].as_str(),
            pixels: frame.rgba.as_slice(),
            width: frame.width,
            height: frame.height,
            uniforms: uniforms[index],
        }
    }).collect();

    GpuPreviewRenderer::composite_rgba_into(&layers, width, height, output)
}

/// Keep only source files that actually contribute audio to the final mux.
/// Source paths are deduplicated by `render_inputs`; this remaps those original
/// indices to a compact FFmpeg input list after raw WGPU video has claimed input 0.
fn export_audio_inputs(
    timeline: &Timeline,
    inputs: &[RenderInput],
    source_indices: &HashMap<String, usize>,
) -> (Vec<RenderInput>, HashMap<String, usize>) {
    let mut audio_inputs = Vec::new();
    let mut remapped_sources = HashMap::<usize, usize>::new();
    let mut audio_indices = HashMap::new();

    for clip in &timeline.clips {
        let is_audio = !clip.track.is_video()
            || clip.extra.get("hasAudio").and_then(Value::as_bool).unwrap_or(false);
        if !is_audio || clip.extra.get("kind").and_then(Value::as_str) == Some("image") {
            continue;
        }
        let Some(&source_index) = source_indices.get(&clip.id) else { continue; };
        let compact_index = if let Some(&existing) = remapped_sources.get(&source_index) {
            existing
        } else {
            let next = audio_inputs.len();
            let Some(input) = inputs.get(source_index) else { continue; };
            audio_inputs.push(input.clone());
            remapped_sources.insert(source_index, next);
            next
        };
        audio_indices.insert(clip.id.clone(), compact_index);
    }
    (audio_inputs, audio_indices)
}

/// The video input is raw RGBA from WGPU (input 0). Audio-only source media
/// follow it and are addressed through the compacted `audio_indices` map.
fn export_audio_filter_graph(
    timeline: &Timeline,
    indices: &HashMap<String, usize>,
    input_offset: usize,
) -> (String, bool) {
    let duration = project_duration(timeline);
    let mut graph = Vec::new();
    let audio_clips: Vec<&Clip> = timeline.clips.iter().filter(|clip| {
        let is_audio = !clip.track.is_video()
            || clip.extra.get("hasAudio").and_then(Value::as_bool).unwrap_or(false);
        is_audio
            && clip.extra.get("kind").and_then(Value::as_str) != Some("image")
            && indices.contains_key(&clip.id)
    }).collect();

    // A media input can feed more than one timeline clip. Split its audio
    // stream once so each independently trimmed/positioned clip has its own
    // consumer, matching the established FFmpeg export graph's behavior.
    let shifted_indices: HashMap<String, usize> = indices.iter()
        .map(|(clip_id, input)| (clip_id.clone(), *input + input_offset))
        .collect();
    let audio_streams = split_source_streams(
        &mut graph, &audio_clips, &shifted_indices, false, "wgpu_asrc",
    );
    let mut audio_labels = Vec::new();
    for clip in &audio_clips {
        let Some(source) = audio_streams.get(&clip.id) else { continue; };
        let value = serde_json::to_value(clip).unwrap_or_default();
        let audio = filters::audio_filters(&value, clip.duration, duration);
        let label = format!("a{}", audio_labels.len());
        graph.push(format!("[{source}]{audio}[{label}]"));
        audio_labels.push(label);
    }

    if audio_labels.len() == 1 {
        graph.push(format!(
            "[{}]atrim=duration={duration:.5},asetpts=PTS-STARTPTS,alimiter=limit=0.95:attack=5:release=50[aout]",
            audio_labels[0]
        ));
    } else if !audio_labels.is_empty() {
        let inputs = audio_labels.iter().map(|label| format!("[{label}]")).collect::<String>();
        graph.push(format!(
            "{inputs}amix=inputs={}:duration=longest:normalize=0,atrim=duration={duration:.5},asetpts=PTS-STARTPTS,alimiter=limit=0.95:attack=5:release=50[aout]",
            audio_labels.len()
        ));
    }
    (graph.join(";"), !audio_labels.is_empty())
}

fn preflight_wgpu_export(
    timeline: &Timeline,
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, DecoderPool), String> {
    // Validate every visual source before creating the output file. Use a
    // throwaway pool: pre-decoding a 2px frame advances a sequential decoder,
    // and reusing that decoder for the full-resolution first frame can skip to
    // the next source frame when its conversion width changes.
    {
        let mut validation_pool = DecoderPool::default();
        for clip in timeline.clips.iter().filter(|clip| is_wgpu_visual_clip(clip)) {
            let path = clip.extra.get("path").and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| format!("Visual clip '{}' has no source path", clip.id))?;
            let source_offset = clip.extra.get("sourceOffset").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            if clip.extra.get("kind").and_then(Value::as_str) == Some("image") {
                validation_pool.still_frame(path, 2)?;
            } else {
                validation_pool.frame_at(&clip.id, path, source_offset, 2, false)?;
            }
        }
    }

    // Render frame zero with a fresh pool at the actual export sampling size.
    // Keep that pool for the remaining frames so sequential decode continues
    // from the correct first-frame position without decoding it twice.
    let mut decoder_pool = DecoderPool::default();
    let mut first_frame = Vec::new();
    render_wgpu_export_frame(timeline, 0.0, width, height, &mut decoder_pool, &mut first_frame)?;
    Ok((first_frame, decoder_pool))
}

fn export_project_wgpu(
    timeline: &Timeline,
    output_path: &str,
    width: u32,
    height: u32,
    fps: u32,
    id: u64,
    inputs: &[RenderInput],
    indices: &HashMap<String, usize>,
    duration: f64,
    decoder_pool: &mut DecoderPool,
    mut frame_pixels: Vec<u8>,
) -> Result<Value, String> {
    let (audio_inputs, audio_indices) = export_audio_inputs(timeline, inputs, indices);
    let (audio_graph, has_audio) = export_audio_filter_graph(timeline, &audio_indices, 1);
    let ffmpeg = multimedia_binary("ffmpeg");
    let audio_script = if has_audio { Some(filter_script(id, &audio_graph)?) } else { None };
    let mut command = media_command(&ffmpeg);
    command.args([
        "-hide_banner", "-loglevel", "error",
        "-f", "rawvideo", "-pixel_format", "rgba",
        "-video_size", &format!("{width}x{height}"),
        "-framerate", &fps.to_string(), "-i", "pipe:0",
    ]);
    for input in &audio_inputs {
        command.arg("-i").arg(&input.path);
    }
    if let Some(script) = audio_script.as_ref() {
        append_filter_graph_arg(&mut command, &ffmpeg, script);
    }
    command.args(["-map", "0:v:0"]);
    if has_audio {
        command.args(["-map", "[aout]"]);
    }
    command.args([
        "-t", &format!("{duration:.5}"), "-r", &fps.to_string(),
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p",
    ]);
    if has_audio {
        command.args(["-c:a", "aac", "-b:a", "192k"]);
    } else {
        command.arg("-an");
    }
    command.args(["-movflags", "+faststart", "-y"]).arg(output_path)
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        if let Some(script) = audio_script.as_ref() { let _ = std::fs::remove_file(script); }
        format!("Could not start FFmpeg encoder: {error}")
    })?;
    let stderr_reader = child.stderr.take().map(|mut stderr| thread::spawn(move || {
        // Drain stderr while frames are being written so a verbose encoder can
        // never fill the OS pipe and stall the render worker. Retain only a
        // bounded diagnostic tail.
        let mut captured = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if captured.len() < 1024 * 1024 {
                        let keep = read.min(1024 * 1024 - captured.len());
                        captured.extend_from_slice(&chunk[..keep]);
                    }
                }
            }
        }
        captured
    }));

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.and_then(|reader| reader.join().ok());
        if let Some(script) = audio_script.as_ref() { let _ = std::fs::remove_file(script); }
        return Err("FFmpeg encoder did not expose its frame input".into());
    };
    let frame_count = (duration * fps as f64).ceil().max(1.0) as u64;
    let render_result = (|| -> Result<(), String> {
        for frame_index in 0..frame_count {
            if frame_index > 0 {
                let frame_at = frame_index as f64 / fps as f64;
                render_wgpu_export_frame(timeline, frame_at, width, height, decoder_pool, &mut frame_pixels)?;
            }
            stdin.write_all(&frame_pixels)
                .map_err(|error| format!("FFmpeg stopped accepting WGPU-rendered frames: {error}"))?;
        }
        Ok(())
    })();
    drop(stdin);
    if render_result.is_err() {
        let _ = child.kill();
    }
    let status_result = child.wait();
    let stderr = stderr_reader.and_then(|reader| reader.join().ok()).unwrap_or_default();
    if let Some(script) = audio_script.as_ref() { let _ = std::fs::remove_file(script); }
    if let Err(error) = render_result {
        let _ = std::fs::remove_file(output_path);
        return Err(error);
    }
    let status = status_result.map_err(|error| format!("Could not wait for FFmpeg encoder: {error}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(output_path);
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            "FFmpeg could not encode the WGPU-rendered export".into()
        } else {
            detail.chars().take(320).collect()
        });
    }
    Ok(json!({
        "outputPath": output_path, "duration": duration, "width": width, "height": height,
        "fps": fps, "renderBackend": "wgpu"
    }))
}

fn export_project(request: &Value, id: u64) -> Result<Value, String> {
    let timeline: Timeline = serde_json::from_value(request["timeline"].clone()).map_err(|e| format!("Invalid timeline: {e}"))?;
    let output_path = request["path"].as_str().ok_or("Export path is required")?;
    // H.264 4:2:0 output needs even dimensions. Normalize once so WGPU's
    // render target and FFmpeg's encoder always receive the same valid size.
    let width = (request["width"].as_u64().unwrap_or(1920).clamp(320, 3840) as u32) & !1;
    let height = (request["height"].as_u64().unwrap_or(1080).clamp(180, 2160) as u32) & !1;
    let fps = request["fps"].as_u64().unwrap_or(30).clamp(1, 120) as u32;
    let duration = project_duration(&timeline);
    if duration <= 0.0 { return Err("Add at least one clip before exporting".into()); }

    let (inputs, indices) = render_inputs(&timeline, true)?;
    ensure_export_does_not_overwrite_input(output_path, &inputs)?;

    // WGPU is the sole video renderer for export. FFmpeg receives already
    // composited RGBA frames and handles audio filtering, encoding, and muxing.
    // A source WGPU cannot decode/render is reported before output starts;
    // never silently switch to a second visual implementation.
    let (first_frame, mut decoder_pool) = preflight_wgpu_export(&timeline, width, height)
        .map_err(|error| format!("WGPU export preflight failed: {error}"))?;
    export_project_wgpu(
        &timeline, output_path, width, height, fps, id,
        &inputs, &indices, duration, &mut decoder_pool, first_frame,
    )
}

fn preview_signature(timeline: &Timeline, width: u32, height: u32, fps: u32) -> Result<u64, String> {
    let mut hasher = DefaultHasher::new();
    "hanabi-preview-stream-v2".hash(&mut hasher);
    serde_json::to_vec(timeline).map_err(|e| e.to_string())?.hash(&mut hasher);
    width.hash(&mut hasher); height.hash(&mut hasher); fps.hash(&mut hasher);
    Ok(hasher.finish())
}

fn start_preview_stream(timeline: &Timeline, width: u32, height: u32, fps: u32, id: u64, signature: u64, origin_index: u64) -> Result<PreviewStream, String> {
    let (graph, inputs, duration, _) = export_filter_graph_mode(timeline, width, height, fps, false)?;
    let script = filter_script(id, &graph)?;
    let ffmpeg = multimedia_binary("ffmpeg");
    let mut command = media_command(&ffmpeg);
    command.args(["-hide_banner", "-loglevel", "error", "-filter_complex_threads", "4"]);
    append_filter_graph_arg(&mut command, &ffmpeg, &script);
    for input in inputs {
        if input.image { command.args(["-loop", "1", "-framerate", &fps.to_string()]); }
        else if input.seek > 0.0001 { command.args(["-accurate_seek", "-ss", &format!("{:.6}", input.seek)]); }
        command.args(["-threads", "0", "-i"]).arg(input.path);
    }
    command.args(["-map", "[vout]", "-an", "-t", &format!("{duration:.5}"),
        "-r", &fps.to_string(), "-fps_mode", "cfr", "-s", &format!("{width}x{height}"),
        "-pix_fmt", "rgba", "-flush_packets", "1", "-f", "rawvideo", "pipe:1"])
        .stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        let _ = std::fs::remove_file(&script);
        format!("Could not start FFmpeg preview stream: {error}")
    })?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill(); let _ = child.wait(); let _ = std::fs::remove_file(&script);
        return Err("FFmpeg preview stream has no output pipe".into());
    };
    let frame_bytes = width as usize * height as usize * 4;
    let capacity = (96 * 1024 * 1024 / frame_bytes.max(1)).clamp(4, 24);
    let ring = Arc::new(FrameRing {
        state: Mutex::new(RingState { frames: VecDeque::new(), free: Vec::new(), closed: false, ended: false }),
        changed: Condvar::new(),
        capacity,
    });
    let reader_ring = Arc::clone(&ring);
    thread::Builder::new()
        .name("hanabi-preview-reader".into())
        .spawn(move || preview_reader(stdout, reader_ring, frame_bytes))
        .map_err(|error| format!("Could not start preview reader: {error}"))?;
    Ok(PreviewStream {
        signature, width, height, fps, origin_index, child, ring, script, next_index: 0,
        last_index: None,
    })
}

impl PreviewEngine {
    /// Sole visual preview path: decode video/still-image frames in-process,
    /// then evaluate all active transforms, colors, effects and transitions in WGPU.
    /// Unsupported sources fail visibly rather than using a second visual engine.
    fn render_native(
        &mut self,
        timeline: &Timeline,
        frame_at: f64,
        width: u32,
        height: u32,
        request_id: u64,
        fps: u32,
        playing: bool,
        stepping: bool,
        decode_ms: &mut f64,
        ring_depth: &mut usize,
    ) -> Result<Option<(u32, u32, f64)>, String> {
        // During an edit transition the incoming clip becomes a second GPU
        // layer before its timeline cut. The shader applies its transition mask;
        // the outgoing clip remains underneath, so no expensive FFmpeg graph is
        // started merely because the playhead is inside a transition.
        let mut ordered_video: Vec<&Clip> = timeline.clips.iter().filter(|clip| {
            clip.track.is_video() && clip.extra.get("kind").and_then(Value::as_str) != Some("audio")
        }).collect();
        ordered_video.sort_by(|a, b| {
            let rank = |track: TrackId| track.sort_index();
            rank(a.track).cmp(&rank(b.track)).then_with(|| a.start.total_cmp(&b.start))
        });
        let mut incoming_transitions: HashMap<String, (String, f64)> = HashMap::new();
        let mut outgoing_transitions: HashMap<String, (String, f64)> = HashMap::new();
        for (left_index, right_index) in same_track_neighbor_indices(&ordered_video) {
            let left = ordered_video[left_index];
            let right = ordered_video[right_index];
            let (kind, duration) = match transition_between(left, right) {
                Some(value) => value,
                None => continue,
            };
            let transition_start = right.start - duration;
            if frame_at + 1e-7 >= transition_start && frame_at < right.start - 1e-7 {
                incoming_transitions.insert(right.id.clone(), (kind.clone(), duration));
                if kind == "dip" {
                    outgoing_transitions.insert(left.id.clone(), (kind, duration));
                }
            }
        }

        let mut active: Vec<&Clip> = timeline.clips.iter().filter(|clip| {
            clip.track.is_video()
                && clip.extra.get("kind").and_then(Value::as_str) != Some("audio")
                && frame_at + 1e-7 >= clip.start
                && frame_at < clip.end() - 1e-7
        }).collect();
        for clip in &ordered_video {
            if incoming_transitions.contains_key(&clip.id) && !active.iter().any(|current| current.id == clip.id) {
                active.push(*clip);
            }
        }

        // A gap is a valid native frame and should not start an FFmpeg process.
        if active.is_empty() {
            self.stream = None;
            publish_black_frame(request_id, width, height, frame_at)?;
            return Ok(Some((width, height, frame_at)));
        }

        // Keep compositing order stable for every layer. Within one track, the
        // incoming clip must sit above the outgoing clip during its transition.
        active.sort_by(|a, b| {
            let rank = |track: TrackId| track.sort_index();
            rank(a.track).cmp(&rank(b.track)).then_with(|| a.start.total_cmp(&b.start))
        });

        // Video keyframes and still-image layers both use the same WGPU
        // uniforms/compositor. Still images are loaded once by the decode worker.
        for clip in &active {
            let source = clip.extra.get("path").and_then(Value::as_str).unwrap_or_default();
            if source.is_empty() {
                return note_wgpu_preview_failure(4, format!("visual clip '{}' has no source path", clip.id));
            }
        }

        // Resolution-aware source sizing. Decode enough pixels to keep the
        // selected preview crisp without paying 1080p conversion/upload costs
        // when the user deliberately chooses a smaller preview quality.
        // The caps are 4:2:0 friendly and retain modest oversampling before the
        // GPU compositor scales to the exact selected output size.
        let max_source_width = match height {
            0..=240 => 640,   // 240p output: 426x240
            241..=480 => 960, // 480p output: 854x480
            481..=720 => 1440,// 720p output: 1280x720
            _ => 1920,        // 1080p output: 1920x1080
        };

        // Several timeline layers may intentionally reference the same source
        // frame (for example, stacked copies with different transforms/opacity).
        // Decode that source/time only once, then fan the shared Arc pixel buffer
        // back out to every layer. Keep each layer's own index so compositing order
        // and per-layer uniforms remain unchanged.
        let mut sources: Vec<DecodeSource> = Vec::with_capacity(active.len());
        let mut source_indices = Vec::with_capacity(active.len());
        let mut layer_uniforms = Vec::with_capacity(active.len());
        let mut source_lookup: HashMap<(String, i64), usize> = HashMap::new();
        for clip in &active {
            let source = preview_source_path(clip);
            if !std::path::Path::new(&source).is_file() {
                return note_wgpu_preview_failure(4, format!("source media is missing: {source}"));
            }
            let source_offset = clip.extra.get("sourceOffset").and_then(Value::as_f64).unwrap_or(0.0);
            let source_time = if let Some((_, duration)) = incoming_transitions.get(&clip.id) {
                transition_preroll_source_time(source_offset, *duration, frame_at - (clip.start - duration))
            } else {
                (source_offset + (frame_at - clip.start)).max(0.0)
            };
            let remaining = (clip.end() - frame_at).max(0.0);
            let props = clip.extra.get("props").unwrap_or(&Value::Null);
            let keyframes = clip.extra.get("keyframes").unwrap_or(&Value::Null);
            let transition = incoming_transitions.get(&clip.id).map(|(kind, duration)| {
                (kind.as_str(), ((frame_at - (clip.start - duration)) / duration.max(1e-9)) as f32)
            });
            let transition_out = outgoing_transitions.get(&clip.id).map(|(kind, duration)| {
                (kind.as_str(), ((frame_at - (clip.end() - duration)) / duration.max(1e-9)) as f32)
            });
            let uniforms = LayerUniforms::from_props_with_keyframes_and_transitions(
                props, frame_at, keyframes, transition, transition_out,
            );
            // Decode close to the layer's displayed pixel size. A 15% picture-in-picture
            // clip needs far fewer source pixels than a full-canvas background. Keep a
            // 160px floor for motion/detail and use the largest scale axis for rotation.
            let layer_scale = uniforms.geo0[0].max(uniforms.geo0[1]).clamp(0.01, 10.0);
            let layer_max_width = ((max_source_width as f32 * layer_scale).ceil() as u32)
                .clamp(160, max_source_width);
            layer_uniforms.push(uniforms);
            if source.is_empty() {
                return note_wgpu_preview_failure(4, format!("visual clip '{}' has an empty source path", clip.id));
            }
            // Microsecond identity is deliberately finer than a video frame:
            // it merges duplicate layer requests without merging neighbouring
            // source frames from high-frame-rate footage.
            let is_still_image = clip.extra.get("kind").and_then(Value::as_str) == Some("image");
            let source_time_key = if is_still_image { 0 } else { (source_time * 1_000_000.0).round() as i64 };
            let lookup_key = (source.clone(), source_time_key);
            if let Some(&index) = source_lookup.get(&lookup_key) {
                sources[index].remaining = sources[index].remaining.max(remaining);
                sources[index].max_width = sources[index].max_width.max(layer_max_width);
                source_indices.push(index);
            } else {
                let index = sources.len();
                source_lookup.insert(lookup_key, index);
                sources.push(DecodeSource {
                    stream_key: clip.id.clone(), path: source, source_time, remaining,
                    max_width: layer_max_width, is_still_image,
                });
                source_indices.push(index);
            }
        }

        let source_paths: Vec<String> = sources.iter().map(|source| source.path.clone()).collect();
        let batch = match self.decoder_worker.decode(DecodeTask {
            sources,
            fps: fps as f64,
            prefetch: playing,
            stepping,
        }) {
            Ok(batch) => batch,
            Err(error) => {
                return note_wgpu_preview_failure(5, format!("source decoding failed: {error}"));
            }
        };
        *decode_ms = batch.decode_ms;
        PREVIEW_TRANSFER_US.store(batch.transfer_us, Ordering::Relaxed);
        PREVIEW_CONVERT_US.store(batch.convert_us, Ordering::Relaxed);
        let decoded = batch.frames;
        *ring_depth = batch.prefetched_frames;
        // Stable source-frame keys let the GPU keep one uploaded texture for
        // identical source frames shared by multiple clip layers.
        let source_keys: Vec<String> = active.iter().zip(source_indices.iter()).map(|(clip, &source_index)| {
            let frame = &decoded[source_index];
            let path = &source_paths[source_index];
            if clip.extra.get("kind").and_then(Value::as_str) == Some("image") {
                format!("still:{}:{}:{}x{}", path, frame.pts_seconds.to_bits(), frame.width, frame.height)
            } else {
                format!("{}:{}:{}x{}", path, frame.pts_seconds.to_bits(), frame.width, frame.height)
            }
        }).collect();
        let layers: Vec<GpuLayerFrame<'_>> = active.iter().enumerate().map(|(index, clip)| {
            let frame = &decoded[source_indices[index]];
            GpuLayerFrame {
                key: clip.id.as_str(),
                source_key: source_keys[index].as_str(),
                pixels: frame.rgba.as_slice(),
                width: frame.width,
                height: frame.height,
                uniforms: layer_uniforms[index],
            }
        }).collect();

        let composite_started = Instant::now();
        let mut pixels = match GpuPreviewRenderer::composite_rgba(&layers, width, height) {
            Ok(pixels) => pixels,
            Err(error) => {
                return note_wgpu_preview_failure(6, format!("GPU compositing failed: {error}"));
            }
        };
        PREVIEW_COMPOSITE_US.store(composite_started.elapsed().as_micros() as u64, Ordering::Relaxed);
        // Drop the old CLI stream once the native path has produced a frame.
        self.stream = None;
        let publish_started = Instant::now();
        publish_preview_buffer(request_id, width, height, frame_at, &mut pixels, false)?;
        PREVIEW_PUBLISH_US.store(publish_started.elapsed().as_micros() as u64, Ordering::Relaxed);
        PREVIEW_PATH.store(1, Ordering::Relaxed);
        PREVIEW_FALLBACK_REASON.store(0, Ordering::Relaxed);
        Ok(Some((width, height, frame_at)))
    }

    fn render(&mut self, request: &Value, id: u64) -> (Result<(u32, u32, f64), String>, f64, f64, usize) {
        let setup_ms = 0.0;
        let mut decode_ms = 0.0;
        PREVIEW_TRANSFER_US.store(0, Ordering::Relaxed);
        PREVIEW_CONVERT_US.store(0, Ordering::Relaxed);
        let mut ring_depth = 0usize;
        let result = (|| -> Result<(u32, u32, f64), String> {
            let timeline: Arc<Timeline> = if request["useSnapshot"].as_bool() == Some(true) {
                PREVIEW_SNAPSHOT.get()
                    .and_then(|slot| slot.lock().ok().and_then(|slot| slot.clone()))
                    .ok_or("Preview timeline snapshot is unavailable")?
            } else {
                Arc::new(serde_json::from_value(request["timeline"].clone()).map_err(|e| format!("Invalid timeline: {e}"))?)
            };
            let playing = request["playing"].as_bool().unwrap_or(false);
            let stepping = request["stepping"].as_bool().unwrap_or(false);
            let at = request["at"].as_f64().filter(|value| value.is_finite() && *value >= 0.0).unwrap_or(0.0);
            let height = request["height"].as_u64().unwrap_or(720).clamp(240, 1080) as u32;
            let width = ((height as f64 * 16.0 / 9.0).round() as u32) & !1;
            let fps = request["fps"].as_u64().unwrap_or(30).clamp(1, 120) as u32;
            let target_index = (at * fps as f64).round() as u64;
            let frame_at = target_index as f64 / fps as f64;

            // Both preview and export use the same in-process decoder and WGPU
            // compositor. There is deliberately no FFmpeg video-filter fallback:
            // a source WGPU cannot process fails visibly instead of rendering with
            // a second, visually different implementation.
            let native = self.render_native(
                &timeline, frame_at, width, height, id, fps, playing, stepping,
                &mut decode_ms, &mut ring_depth,
            )?;
            native.ok_or_else(|| "WGPU preview did not produce a frame; FFmpeg visual fallback is disabled".to_owned())
        })();
        if result.is_err() { self.stream = None; }
        (result, setup_ms, decode_ms, ring_depth)
    }
}

fn dispatch(request: Value) -> Result<Value, String> {
    let backend = backend();
    let mut timeline = backend
        .timeline
        .lock()
        .map_err(|_| "Timeline lock failed".to_owned())?;
    let command = request["command"].as_str().ok_or("Command is required")?;
    match command {
        "timeline_get" => serde_json::to_value(timeline.clone()).map_err(|e| e.to_string()),
        "timeline_undo" => {
            let previous = backend.undo.lock().map_err(|_| "Undo history lock failed")?.pop()
                .ok_or("Nothing to undo")?;
            backend.redo.lock().map_err(|_| "Redo history lock failed")?.push(timeline.clone());
            *timeline = previous;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_redo" => {
            let next = backend.redo.lock().map_err(|_| "Redo history lock failed")?.pop()
                .ok_or("Nothing to redo")?;
            backend.undo.lock().map_err(|_| "Undo history lock failed")?.push(timeline.clone());
            *timeline = next;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_initialize" => {
            let before = timeline.clone();
            let clips: Vec<Clip> =
                serde_json::from_value(request["clips"].clone()).map_err(|e| e.to_string())?;
            let snapping = timeline.snapping;
            let magnetic = timeline.magnetic;
            *timeline = Timeline::new(clips).map_err(|e| format!("{e:?}"))?;
            timeline.snapping = snapping;
            timeline.magnetic = magnetic;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_add_clip" => {
            let before = timeline.clone();
            let clip: Clip =
                serde_json::from_value(request["clip"].clone()).map_err(|e| e.to_string())?;
            timeline.add_clip(clip).map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_update_title_clip" => {
            let live = request["live"].as_bool().unwrap_or(false);
            let before = begin_history_change(backend, &timeline, live)?;
            let id = request["id"].as_str().ok_or("Title clip id is required")?;
            let path = request["path"].as_str().ok_or("Title image path is required")?;
            let text = request["text"].clone();
            if !text.is_object() { return Err("Title properties must be an object".into()); }
            let clip = timeline.clips.iter_mut().find(|clip| clip.id == id)
                .ok_or_else(|| format!("Title clip {id} was not found"))?;
            if clip.extra.get("kind").and_then(Value::as_str) != Some("image") ||
               clip.extra.get("props").and_then(|props| props.get("text")).and_then(|value| value.get("isTitle")).and_then(Value::as_bool) != Some(true) {
                return Err("The selected clip is not a Hanabi title".into());
            }
            clip.extra.insert("path".into(), json!(path));
            clip.extra.insert("kind".into(), json!("image"));
            clip.extra.insert("hasAudio".into(), json!(false));
            let props = clip.extra.entry("props".into()).or_insert_with(|| json!({}));
            if !props.is_object() { *props = json!({}); }
            props["text"] = text;
            commit_history_change(backend, before)?;
            if live { Ok(json!({"changed": true})) }
            else { serde_json::to_value(timeline.clone()).map_err(|e| e.to_string()) }
        }
        "timeline_group_clips" => {
            let before = timeline.clone();
            let ids: Vec<String> = serde_json::from_value(request["ids"].clone()).map_err(|e| e.to_string())?;
            let group_id = request["groupId"].as_str().ok_or("Group id is required")?;
            timeline.group_clips(&ids, group_id).map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_ungroup_clip" => {
            let before = timeline.clone();
            timeline.ungroup_clip(request["id"].as_str().ok_or("Clip id is required")?)
                .map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_separate_audio" => {
            let before = timeline.clone();
            let id = request["id"].as_str().ok_or("Clip id is required")?;
            let audio_id = request["audioId"].as_str().ok_or("Audio clip id is required")?;
            timeline.separate_audio(id, audio_id.to_owned()).map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_unlink_clip" => {
            let before = timeline.clone();
            timeline.unlink_clip(request["id"].as_str().ok_or("Clip id is required")?)
                .map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_move_clip" => {
            let before = timeline.clone();
            let track: TrackId =
                serde_json::from_value(request["track"].clone()).map_err(|e| e.to_string())?;
            timeline
                .move_clip(
                    request["id"].as_str().ok_or("Clip id is required")?,
                    request["start"].as_f64().ok_or("Start is required")?,
                    track,
                    request["playhead"].as_f64().unwrap_or(0.0),
                    request["snapThreshold"].as_f64().unwrap_or(0.0),
                )
                .map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_trim_clip" => {
            let before = timeline.clone();
            timeline
                .trim_clip(
                    request["id"].as_str().ok_or("Clip id is required")?,
                    request["start"].as_f64().ok_or("Start is required")?,
                    request["end"].as_f64().ok_or("End is required")?,
                    request["sourceDuration"].as_f64(),
                )
                .map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_split_clip" => {
            let before = timeline.clone();
            timeline
                .split_clip(
                    request["id"].as_str().ok_or("Clip id is required")?,
                    request["at"].as_f64().ok_or("Split time is required")?,
                    request["rightId"]
                        .as_str()
                        .ok_or("New clip id is required")?
                        .to_owned(),
                )
                .map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_delete_clip" => {
            let before = timeline.clone();
            timeline
                .delete_clip(request["id"].as_str().ok_or("Clip id is required")?)
                .map_err(|e| format!("{e:?}"))?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_set_snapping" => {
            let before = timeline.clone();
            timeline.snapping = request["enabled"]
                .as_bool()
                .ok_or("Snapping value is required")?;
            save_history(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_add_keyframe" => {
            let id = request["id"].as_str().ok_or("Clip id is required")?;
            let at = request["at"].as_f64().filter(|value| value.is_finite() && *value >= 0.0).ok_or("Keyframe time is invalid")?;
            {
                let clip = timeline.clips.iter().find(|clip| clip.id == id)
                    .ok_or_else(|| format!("Clip {id} was not found"))?;
                if at < clip.start - 1e-4 || at > clip.end() + 1e-4 {
                    return Err("Keyframe must be inside the selected clip".into());
                }
                if let Some(keyframes) = clip.extra.get("keyframes") {
                    if !keyframes.is_array() { return Err("Clip keyframes are invalid".into()); }
                }
            }
            let before = begin_history_change(backend, &timeline, false)?;
            let clip = timeline.clips.iter_mut().find(|clip| clip.id == id)
                .ok_or_else(|| format!("Clip {id} was not found"))?;
            let keyframes = clip.extra.entry("keyframes".into()).or_insert_with(|| json!([]));
            let list = keyframes.as_array_mut().ok_or("Clip keyframes are invalid")?;
            if let Some(existing) = list.iter_mut().find(|keyframe| (keyframe["time"].as_f64().unwrap_or(-1.0) - at).abs() < 0.0001) {
                existing["values"] = request["values"].clone();
            } else {
                list.push(json!({"time": at, "values": request["values"].clone()}));
            }
            list.sort_by(|a, b| a["time"].as_f64().unwrap_or(0.0).total_cmp(&b["time"].as_f64().unwrap_or(0.0)));
            commit_history_change(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_delete_keyframe" => {
            let id = request["id"].as_str().ok_or("Clip id is required")?;
            let at = request["at"].as_f64().filter(|value| value.is_finite())
                .ok_or("Keyframe time is invalid")?;
            // Validate before opening an undo transaction, so an accidental
            // delete click away from a marker never creates an empty undo step.
            {
                let clip = timeline.clips.iter().find(|clip| clip.id == id)
                    .ok_or_else(|| format!("Clip {id} was not found"))?;
                let list = clip.extra.get("keyframes").and_then(Value::as_array)
                    .ok_or("Clip has no keyframes")?;
                if !list.iter().any(|keyframe|
                    (keyframe["time"].as_f64().unwrap_or(f64::NAN) - at).abs() < 0.001) {
                    return Err("Keyframe could not be found".into());
                }
            }
            let before = begin_history_change(backend, &timeline, false)?;
            let clip = timeline.clips.iter_mut().find(|clip| clip.id == id)
                .ok_or_else(|| format!("Clip {id} was not found"))?;
            let list = clip.extra.get_mut("keyframes").and_then(Value::as_array_mut)
                .ok_or("Clip has no keyframes")?;
            let index = list.iter().position(|keyframe|
                (keyframe["time"].as_f64().unwrap_or(f64::NAN) - at).abs() < 0.001)
                .ok_or("Keyframe could not be found")?;
            list.remove(index);
            commit_history_change(backend, before)?;
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_move_keyframe" => {
            let id = request["id"].as_str().ok_or("Clip id is required")?;
            let from = request["from"].as_f64().filter(|value| value.is_finite()).ok_or("Original keyframe time is invalid")?;
            let to = request["to"].as_f64().filter(|value| value.is_finite()).ok_or("New keyframe time is invalid")?;
            let live = request["live"].as_bool().unwrap_or(false);
            // Validate the move before opening an undo transaction. A blocked
            // drag (for example, dropping onto another keyframe) must not leave
            // an empty history snapshot behind.
            {
                let clip = timeline.clips.iter().find(|clip| clip.id == id)
                    .ok_or_else(|| format!("Clip {id} was not found"))?;
                if to < clip.start - 1e-4 || to > clip.end() + 1e-4 {
                    return Err("Keyframe must stay inside the selected clip".into());
                }
                let list = clip.extra.get("keyframes").and_then(Value::as_array)
                    .ok_or("Clip has no keyframes")?;
                let index = list.iter().position(|keyframe| (keyframe["time"].as_f64().unwrap_or(-1.0) - from).abs() < 0.001)
                    .ok_or("Keyframe could not be found")?;
                if list.iter().enumerate().any(|(other, keyframe)| other != index &&
                    (keyframe["time"].as_f64().unwrap_or(-1.0) - to).abs() < 0.0001) {
                    return Err("Another keyframe already exists at that time".into());
                }
            }
            let before = begin_history_change(backend, &timeline, live)?;
            let clip = timeline.clips.iter_mut().find(|clip| clip.id == id)
                .ok_or_else(|| format!("Clip {id} was not found"))?;
            let list = clip.extra.get_mut("keyframes").and_then(Value::as_array_mut)
                .ok_or("Clip has no keyframes")?;
            let index = list.iter().position(|keyframe| (keyframe["time"].as_f64().unwrap_or(-1.0) - from).abs() < 0.001)
                .ok_or("Keyframe could not be found")?;
            list[index]["time"] = json!(to);
            list.sort_by(|a, b| a["time"].as_f64().unwrap_or(0.0).total_cmp(&b["time"].as_f64().unwrap_or(0.0)));
            commit_history_change(backend, before)?;
            if live { Ok(json!({"changed": true})) }
            else { serde_json::to_value(timeline.clone()).map_err(|e| e.to_string()) }
        }
        "timeline_end_property_edit" => {
            let before = backend.property_edit.lock().map_err(|_| "Property edit lock failed")?.take();
            if let Some(before) = before { save_history(backend, before)?; }
            serde_json::to_value(timeline.clone()).map_err(|e| e.to_string())
        }
        "timeline_set_clip_property" => {
            let live = request["live"].as_bool().unwrap_or(false);
            let before = begin_history_change(backend, &timeline, live)?;
            let id = request["id"].as_str().ok_or("Clip id is required")?;
            let path = request["path"].as_str().ok_or("Property path is required")?;
            let value = request["value"].clone();
            set_timeline_clip_property(&mut timeline, id, path, value.clone())?;
            if let Some(at) = request["at"].as_f64().filter(|value| value.is_finite()) {
                update_keyframe_property(&mut timeline, id, path, value, at);
            }
            commit_history_change(backend, before)?;
            if live { Ok(json!({"changed": true})) }
            else { serde_json::to_value(timeline.clone()).map_err(|e| e.to_string()) }
        }
        "timeline_set_clip_properties" => {
            let live = request["live"].as_bool().unwrap_or(false);
            let before = begin_history_change(backend, &timeline, live)?;
            let id = request["id"].as_str().ok_or("Clip id is required")?;
            let at = request["at"].as_f64().filter(|value| value.is_finite());
            let updates = request["properties"].as_array().ok_or("Property updates are required")?;
            let selected_index = timeline.clips.iter().position(|clip| clip.id == id)
                .ok_or_else(|| format!("Clip {id} was not found"))?;
            let group_id = timeline.clips[selected_index].extra.get("groupId")
                .and_then(Value::as_str).map(str::to_owned);
            // Transform scrubs only touch the selected clip. Clone sibling clips
            // only when this batch actually contains a group-shared color/effect
            // property, keeping per-frame inspector edits proportional to one clip.
            let update_group = group_id.is_some() && updates.iter().any(|update|
                update["path"].as_str().is_some_and(is_group_shared_property));
            let indices: Vec<_> = if update_group {
                let group = group_id.as_deref().unwrap();
                timeline.clips.iter().enumerate()
                    .filter_map(|(index, clip)| (clip.extra.get("groupId").and_then(Value::as_str) == Some(group)).then_some(index))
                    .collect()
            } else { vec![selected_index] };
            let mut working: Vec<_> = indices.iter().map(|&index| (index, timeline.clips[index].clone())).collect();
            for update in updates {
                let path = update["path"].as_str().ok_or("Property path is required")?;
                let value = update["value"].clone();
                let shared = is_group_shared_property(path) && group_id.is_some();
                for (index, clip) in &mut working {
                    if *index == selected_index || shared {
                        set_clip_property(clip, path, value.clone())?;
                        if *index == selected_index {
                            if let Some(at) = at { update_keyframe_property_on_clip(clip, path, value.clone(), at); }
                        }
                    }
                }
            }
            for (index, clip) in working { timeline.clips[index] = clip; }
            commit_history_change(backend, before)?;
            if live { Ok(json!({"changed": true})) }
            else { serde_json::to_value(timeline.clone()).map_err(|e| e.to_string()) }
        }
        _ => Err(format!("Unsupported command: {command}")),
    }
}

fn save_history(backend: &Backend, before: Timeline) -> Result<(), String> {
    let mut undo = backend.undo.lock().map_err(|_| "Undo history lock failed")?;
    undo.push(before);
    if undo.len() > 64 { undo.remove(0); }
    backend.redo.lock().map_err(|_| "Redo history lock failed")?.clear();
    Ok(())
}

fn begin_history_change(backend: &Backend, timeline: &Timeline, live: bool) -> Result<Option<Timeline>, String> {
    let mut pending = backend.property_edit.lock().map_err(|_| "Property edit lock failed")?;
    if live {
        // Clone once at gesture start; do not clone the whole timeline on each
        // subsequent mouse-move frame.
        if pending.is_none() { *pending = Some(timeline.clone()); }
        Ok(None)
    } else {
        Ok(Some(pending.take().unwrap_or_else(|| timeline.clone())))
    }
}

fn commit_history_change(backend: &Backend, before: Option<Timeline>) -> Result<(), String> {
    if let Some(before) = before { save_history(backend, before)?; }
    Ok(())
}

fn update_keyframe_property_on_clip(clip: &mut Clip, path: &str, value: Value, at: f64) {
    let path = path.strip_prefix("props.").unwrap_or(path);
    let Some(list) = clip.extra.get_mut("keyframes").and_then(Value::as_array_mut) else { return; };
    if let Some(keyframe) = list.iter_mut().find(|keyframe|
        (keyframe["time"].as_f64().unwrap_or(f64::NEG_INFINITY) - at).abs() <= 0.001) {
        if !keyframe["values"].is_object() { keyframe["values"] = json!({}); }
        // Keep the legacy top-level Transform channels in sync for projects
        // created before Hanabi began storing complete nested `props` snapshots.
        let legacy_alias = match path {
            "transform.position.x" => Some("x"),
            "transform.position.y" => Some("y"),
            "transform.scale.w" => Some("scaleW"),
            "transform.scale.h" => Some("scaleH"),
            "transform.rotation" => Some("rotation"),
            "transform.opacity" => Some("opacity"),
            _ => None,
        };
        if let Some(alias) = legacy_alias {
            keyframe["values"][alias] = value.clone();
        }
        if !keyframe["values"]["props"].is_object() { keyframe["values"]["props"] = json!({}); }
        let parts: Vec<_> = path.split('.').filter(|part| !part.is_empty()).collect();
        if !parts.is_empty() && parts.len() <= 8 {
            let mut cursor = &mut keyframe["values"]["props"];
            for part in &parts[..parts.len() - 1] {
                if !cursor[*part].is_object() { cursor[*part] = json!({}); }
                cursor = &mut cursor[*part];
            }
            cursor[parts[parts.len() - 1]] = value;
        }
    }
}

fn update_keyframe_property(timeline: &mut Timeline, id: &str, path: &str, value: Value, at: f64) {
    if let Some(clip) = timeline.clips.iter_mut().find(|clip| clip.id == id) {
        update_keyframe_property_on_clip(clip, path, value, at);
    }
}

fn set_json_path(root: &mut Value, parts: &[&str], value: Value) {
    let Some((head, tail)) = parts.split_first() else { return; };
    if tail.is_empty() {
        root[head] = value;
        return;
    }
    if !root[head].is_object() { root[head] = json!({}); }
    set_json_path(&mut root[head], tail, value);
}

fn set_clip_property(clip: &mut Clip, path: &str, value: Value) -> Result<(), String> {
    // The C inspector sends paths rooted at the serialized clip ("props.*"),
    // but this function has already extracted clip.extra["props"]. Keeping
    // that prefix would write to props.props.* where neither preview nor export
    // looks for values. Accept both serialized and relative property paths.
    let path = path.strip_prefix("props.").unwrap_or(path);
    let parts: Vec<_> = path.split('.').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() || parts.len() > 8 { return Err("Invalid property path".into()); }
    let mut props = clip.extra.remove("props").unwrap_or_else(|| json!({}));
    if !props.is_object() { props = json!({}); }
    set_json_path(&mut props, &parts, value);
    clip.extra.insert("props".into(), props);
    Ok(())
}

fn is_group_shared_property(path: &str) -> bool {
    path.starts_with("props.effects.") || path.starts_with("props.colors.")
}

fn set_timeline_clip_property(timeline: &mut Timeline, id: &str, path: &str, value: Value) -> Result<(), String> {
    let selected = timeline.clips.iter().position(|clip| clip.id == id)
        .ok_or_else(|| format!("Clip {id} was not found"))?;
    let group_id = timeline.clips[selected].extra.get("groupId").and_then(Value::as_str).map(str::to_owned);
    let indices: Vec<_> = if is_group_shared_property(path) {
        group_id.as_deref().map(|group| timeline.clips.iter().enumerate()
            .filter_map(|(index, clip)| (clip.extra.get("groupId").and_then(Value::as_str) == Some(group)).then_some(index))
            .collect()).unwrap_or_else(|| vec![selected])
    } else { vec![selected] };
    let mut updated = Vec::with_capacity(indices.len());
    for index in indices {
        let mut clip = timeline.clips[index].clone();
        set_clip_property(&mut clip, path, value.clone())?;
        updated.push((index, clip));
    }
    for (index, clip) in updated { timeline.clips[index] = clip; }
    Ok(())
}

fn c_string(value: String) -> *mut c_char {
    CString::new(value.replace('\0', "\\0"))
        .expect("NUL bytes replaced")
        .into_raw()
}

/// Synchronously dispatches inexpensive timeline commands. Returns owned JSON;
/// release it with `hanabi_engine_free`.
#[no_mangle]
pub unsafe extern "C" fn hanabi_engine_request(input: *const c_char) -> *mut c_char {
    let output = catch_unwind(AssertUnwindSafe(|| {
        if input.is_null() {
            return json!({"ok":false,"error":"Null request"}).to_string();
        }
        let text = CStr::from_ptr(input).to_string_lossy();
        match serde_json::from_str::<Value>(&text)
            .map_err(|e| e.to_string())
            .and_then(dispatch)
        {
            Ok(value) => json!({"ok":true,"result":value}).to_string(),
            Err(error) => json!({"ok":false,"error":error}).to_string(),
        }
    }))
    .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"Backend panic\"}".into());
    c_string(output)
}

/// Queues an expensive task and returns its request id. Results arrive via poll.
#[no_mangle]
pub unsafe extern "C" fn hanabi_engine_submit(input: *const c_char) -> u64 {
    catch_unwind(AssertUnwindSafe(|| {
        if input.is_null() {
            return 0;
        }
        let mut request: Value = match serde_json::from_str(&CStr::from_ptr(input).to_string_lossy()) {
            Ok(value) => value,
            Err(_) => return 0,
        };
        let backend = backend();
        let command = request["command"].as_str().unwrap_or_default().to_owned();
        let is_preview = command == "preview_frame";
        if is_preview {
            if backend.preview_inflight.swap(true, Ordering::AcqRel) { return 0; }
            let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            request["submittedAtMs"] = json!(now_ms);
        }
        if is_preview && request["timeline"].is_null() {
            let snapshot_ready = match backend.timeline.lock() {
                Ok(timeline) => {
                    let slot = PREVIEW_SNAPSHOT.get_or_init(|| Mutex::new(None));
                    match slot.lock() {
                        Ok(mut slot) => {
                            // Rebuild only when the timeline really changed; this is a plain
                            // structural compare, with no serialization or allocation.
                            if !slot.as_ref().is_some_and(|snapshot| **snapshot == *timeline) {
                                *slot = Some(Arc::new(timeline.clone()));
                            }
                            true
                        }
                        Err(_) => false,
                    }
                }
                Err(_) => false,
            };
            if !snapshot_ready { backend.preview_inflight.store(false, Ordering::Release); return 0; }
            request["useSnapshot"] = json!(true);
        } else if matches!(command.as_str(), "preview_frame" | "export_project") && request["timeline"].is_null() {
            let timeline = match backend.timeline.lock() {
                Ok(timeline) => timeline.clone(),
                Err(_) => { if is_preview { backend.preview_inflight.store(false, Ordering::Release); } return 0; }
            };
            request["timeline"] = match serde_json::to_value(timeline) {
                Ok(value) => value,
                Err(_) => { if is_preview { backend.preview_inflight.store(false, Ordering::Release); } return 0; }
            };
        }
        let id = backend.next_task.fetch_add(1, Ordering::Relaxed);
        let sender = if is_preview { &backend.preview_task_tx }
            else if command == "generate_proxy" { &backend.proxy_task_tx }
            else { &backend.task_tx };
        if sender.send((id, request)).is_ok() {
            id
        } else {
            if is_preview { backend.preview_inflight.store(false, Ordering::Release); }
            0
        }
    }))
    .unwrap_or(0)
}

/// Returns one completed background task as JSON, or null when the queue is empty.
#[no_mangle]
pub extern "C" fn hanabi_engine_poll() -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| match backend().event_rx.lock() {
        Ok(rx) => match rx.try_recv() {
            Ok(value) => c_string(value),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => std::ptr::null_mut(),
        },
        Err(_) => std::ptr::null_mut(),
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Copies the newest raw preview frame into caller-owned memory. Returns 1 on
/// success, -1 when the buffer is too small (required length is still written),
/// and 0 when this frame is no longer available.
#[no_mangle]
pub unsafe extern "C" fn hanabi_engine_copy_preview_frame(
    request_id: u64,
    destination: *mut u8,
    capacity: usize,
    width: *mut u32,
    height: *mut u32,
    byte_len: *mut usize,
    at: *mut f64,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if width.is_null() || height.is_null() || byte_len.is_null() || at.is_null() { return 0; }
        let Some(slot) = PREVIEW_FRAME.get() else { return 0; };
        let Ok(slot) = slot.lock() else { return 0; };
        if slot.request_id != request_id || slot.pixels.is_empty() { return 0; }
        *width = slot.width;
        *height = slot.height;
        *byte_len = slot.pixels.len();
        *at = slot.at;
        if destination.is_null() || capacity < slot.pixels.len() { return -1; }
        std::ptr::copy_nonoverlapping(slot.pixels.as_ptr(), destination, slot.pixels.len());
        1
    })).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn hanabi_engine_free(value: *mut c_char) {
    if !value.is_null() {
        drop(CString::from_raw(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_clip(id: &str) -> Clip {
        Clip {
            id: id.into(),
            track: TrackId::V1,
            start: 0.0,
            duration: 2.0,
            label: "Test clip".into(),
            linked_clip_id: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn wgpu_preview_reports_errors_instead_of_using_ffmpeg_visual_fallback() {
        let error = note_wgpu_preview_failure(5, "unsupported test pixel format").unwrap_err();
        assert!(error.contains("FFmpeg visual fallback is disabled"));
        assert!(error.contains("unsupported test pixel format"));
    }

    #[test]
    fn wgpu_is_the_visual_renderer_for_video_and_still_image_clips() {
        let mut video = test_clip("wgpu-video");
        video.extra.insert("path".into(), json!("video.mp4"));
        let mut image = test_clip("wgpu-image");
        image.extra.insert("kind".into(), json!("image"));
        image.extra.insert("path".into(), json!("still.png"));
        assert!(is_wgpu_visual_clip(&video));
        assert!(is_wgpu_visual_clip(&image));

        let mut audio = test_clip("wgpu-audio");
        audio.track = TrackId::A1;
        assert!(!is_wgpu_visual_clip(&audio));
    }

    #[test]
    fn wgpu_export_passes_only_audio_sources_to_ffmpeg() {
        let mut video = test_clip("video");
        video.extra.insert("path".into(), json!("video.mp4"));
        let mut image = test_clip("image");
        image.extra.insert("kind".into(), json!("image"));
        image.extra.insert("path".into(), json!("still.png"));
        let mut audio = test_clip("audio");
        audio.track = TrackId::A1;
        audio.extra.insert("path".into(), json!("music.wav"));

        let timeline = Timeline { clips: vec![video, image, audio], ..Timeline::default() };
        let inputs = vec![
            RenderInput { path: "video.mp4".into(), image: false, seek: 0.0 },
            RenderInput { path: "still.png".into(), image: true, seek: 0.0 },
            RenderInput { path: "music.wav".into(), image: false, seek: 0.0 },
        ];
        let indices = HashMap::from([
            ("video".to_owned(), 0),
            ("image".to_owned(), 1),
            ("audio".to_owned(), 2),
        ]);
        let (audio_inputs, audio_indices) = export_audio_inputs(&timeline, &inputs, &indices);
        assert_eq!(audio_inputs.len(), 1);
        assert_eq!(audio_inputs[0].path, "music.wav");
        assert_eq!(audio_indices.get("audio"), Some(&0));
        assert!(!audio_indices.contains_key("video"));
        assert!(!audio_indices.contains_key("image"));
    }

    #[test]
    fn wgpu_audio_graph_accounts_for_raw_video_input_zero() {
        let mut clip = test_clip("audio-lane");
        clip.track = TrackId::A1;
        clip.extra.insert("path".into(), json!("music.wav"));
        let mut timeline = Timeline::default();
        timeline.clips.push(clip);
        let mut indices = HashMap::new();
        indices.insert("audio-lane".into(), 0);

        let mut second = test_clip("audio-lane-second");
        second.track = TrackId::A2;
        second.start = 1.0;
        second.extra.insert("path".into(), json!("music.wav"));
        timeline.clips.push(second);
        indices.insert("audio-lane-second".into(), 0);

        let (graph, has_audio) = export_audio_filter_graph(&timeline, &indices, 1);
        assert!(has_audio);
        assert!(graph.contains("[1:a]asplit=2"), "reused audio inputs need a split before independent clip filters: {graph}");
        assert!(graph.contains("[aout]"));
    }

    #[test]
    fn export_rejects_output_that_is_the_same_as_source_media() {
        let directory = std::env::temp_dir().join(format!("hanabi-export-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source.mp4");
        std::fs::write(&source, b"source").unwrap();
        let inputs = vec![RenderInput { path: source.to_string_lossy().into_owned(), image: false, seek: 0.0 }];

        let same_path = source.to_string_lossy().into_owned();
        let error = ensure_export_does_not_overwrite_input(&same_path, &inputs).unwrap_err();
        assert!(error.contains("same file as source media"));

        let different_path = directory.join("export.mp4").to_string_lossy().into_owned();
        assert!(ensure_export_does_not_overwrite_input(&different_path, &inputs).is_ok());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ffi_dispatch_returns_structured_error_for_unknown_command() {
        let error = dispatch(json!({"command":"unknown"})).unwrap_err();
        assert!(error.contains("Unsupported command"));
    }

    #[test]
    fn inspector_transform_paths_update_the_values_read_by_preview() {
        let mut clip = test_clip("transform-test");

        // These fully-qualified paths match those emitted by Frontend/right_panel.h.
        for (path, value) in [
            ("props.transform.position.x", json!(960.0)),
            ("props.transform.position.y", json!(540.0)),
            ("props.transform.scale.w", json!(150.0)),
            ("props.transform.scale.h", json!(80.0)),
            ("props.transform.rotation", json!(90.0)),
            ("props.transform.opacity", json!(25.0)),
            ("props.transform.crop.left", json!(120.0)),
            ("props.transform.distort.tl", json!(12.0)),
        ] {
            set_clip_property(&mut clip, path, value).unwrap();
        }

        let props = clip.extra.get("props").expect("clip props should exist");
        assert!(props.get("props").is_none(), "must not create props.props");
        assert_eq!(props["transform"]["crop"]["left"].as_f64(), Some(120.0));
        assert_eq!(props["transform"]["distort"]["tl"].as_f64(), Some(12.0));

        let uniforms = LayerUniforms::from_props(props, 0.0);
        assert!((uniforms.geo0[0] - 1.5).abs() < 0.0001, "scale X");
        assert!((uniforms.geo0[1] - 0.8).abs() < 0.0001, "scale Y");
        assert!((uniforms.geo0[2] - std::f32::consts::FRAC_PI_2).abs() < 0.0001, "rotation");
        assert!((uniforms.opacity() - 0.25).abs() < 0.0001, "opacity");
        assert!((uniforms.geo1[0] - 0.5).abs() < 0.0001, "position X");
        assert!((uniforms.geo1[1] - 0.5).abs() < 0.0001, "position Y");
        assert!((uniforms.crop[0] - (120.0 / 1920.0)).abs() < 0.0001, "crop left");
        assert!((uniforms.distort[0] - 12.0).abs() < 0.0001, "distort top-left");
    }

    #[test]
    fn inspector_color_and_effect_paths_are_written_at_renderer_root() {
        let mut clip = test_clip("color-effects-test");
        set_clip_property(&mut clip, "props.colors.light.exposure", json!(1.5)).unwrap();
        set_clip_property(&mut clip, "props.colors.hsl.0.hue", json!(42.0)).unwrap();
        set_clip_property(&mut clip, "props.effects.blur.enabled", json!(true)).unwrap();

        let props = &clip.extra["props"];
        assert!(props.get("props").is_none(), "must not create props.props");
        assert_eq!(props["colors"]["light"]["exposure"].as_f64(), Some(1.5));
        assert_eq!(props["colors"]["hsl"]["0"]["hue"].as_f64(), Some(42.0));
        assert_eq!(props["effects"]["blur"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn inspector_edits_at_a_keyframe_update_only_that_keyframe_channel() {
        let mut clip = test_clip("keyframe-edit-test");
        clip.extra.insert("keyframes".into(), json!([
            {"time": 0.0, "values": {"x": 10.0, "opacity": 100.0}},
            {"time": 1.0, "values": {"x": 20.0, "opacity": 75.0}},
            {"time": 2.0, "values": {"x": 30.0, "opacity": 50.0}}
        ]));

        update_keyframe_property_on_clip(&mut clip, "props.transform.position.x", json!(42.0), 1.0);
        update_keyframe_property_on_clip(&mut clip, "props.transform.opacity", json!(35.0), 1.0);
        update_keyframe_property_on_clip(&mut clip, "props.transform.scale.w", json!(150.0), 1.0);

        let frames = clip.extra["keyframes"].as_array().unwrap();
        assert_eq!(frames[0]["values"]["x"].as_f64(), Some(10.0));
        assert_eq!(frames[1]["values"]["x"].as_f64(), Some(42.0));
        assert_eq!(frames[1]["values"]["opacity"].as_f64(), Some(35.0));
        assert_eq!(frames[1]["values"]["scaleW"].as_f64(), Some(150.0));
        assert_eq!(frames[2]["values"]["x"].as_f64(), Some(30.0));
    }

    #[test]
    fn transition_pairing_ignores_interleaved_clips_on_other_video_lanes() {
        let mut first = test_clip("first-v1");
        first.start = 0.0;
        first.duration = 2.0;
        first.extra.insert("props".into(), json!({"transitionOut":{"type":"crossfade","duration":0.8}}));
        let mut overlay = test_clip("overlay-v2");
        overlay.track = TrackId::V2;
        overlay.start = 0.8;
        overlay.duration = 0.5;
        let mut second = test_clip("second-v1");
        second.start = 2.0;
        second.duration = 2.0;

        let mut ordered = vec![&first, &overlay, &second];
        ordered.sort_by(|a, b| a.track.sort_index().cmp(&b.track.sort_index()).then_with(|| a.start.total_cmp(&b.start)));
        let pairs = same_track_neighbor_indices(&ordered);
        assert_eq!(pairs.len(), 1);
        assert_eq!(ordered[pairs[0].0].id, "first-v1");
        assert_eq!(ordered[pairs[0].1].id, "second-v1");
        assert_eq!(transition_between(ordered[pairs[0].0], ordered[pairs[0].1]), Some(("crossfade".to_owned(), 0.8)));
    }

    #[test]
    fn new_transition_types_are_preserved_from_clip_properties() {
        for kind in ["zoom", "spin", "blur", "checker", "dip", "crossfade"] {
            let mut clip = test_clip(kind);
            clip.extra.insert("props".into(), json!({"transitionOut":{"type":kind,"duration":0.6}}));
            assert_eq!(transition_out(&clip).map(|value| value.0), Some(kind.to_owned()));
        }
    }

    #[test]
    fn legacy_fadeblack_alias_keeps_the_full_dip_to_black_behavior() {
        let mut clip = test_clip("legacy-dip");
        clip.extra.insert("props".into(), json!({"transitionOut":{"type":"fadeblack","duration":0.6}}));
        assert_eq!(transition_out(&clip).map(|value| value.0), Some("dip".to_owned()));
    }

    #[test]
    fn transition_preroll_holds_first_frame_when_source_has_no_handle() {
        assert!((transition_preroll_source_time(0.0, 0.8, 0.0) - 0.0).abs() < 1e-9);
        assert!((transition_preroll_source_time(0.0, 0.8, 0.4) - 0.0).abs() < 1e-9);
        assert!((transition_preroll_source_time(0.0, 0.8, 0.8) - 0.0).abs() < 1e-9);
        assert!((transition_preroll_source_time(0.0, 0.8, 1.2) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn transition_preroll_consumes_available_source_handle_without_cut_jump() {
        assert!((transition_preroll_source_time(1.5, 0.8, 0.0) - 0.7).abs() < 1e-9);
        assert!((transition_preroll_source_time(1.5, 0.8, 0.8) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn relative_property_paths_remain_supported() {
        let mut clip = test_clip("relative-path-test");
        set_clip_property(&mut clip, "audio.fadeInDuration", json!(1.25)).unwrap();
        assert_eq!(
            clip.extra["props"]["audio"]["fadeInDuration"].as_f64(),
            Some(1.25)
        );
    }
}
