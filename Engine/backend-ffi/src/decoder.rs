//! Persistent in-process FFmpeg video decoder used by the preview worker.
//!
//! A decoder context is opened once per timeline clip and reused for sequential
//! playback. Clips that share a file but use different source offsets therefore
//! keep independent read cursors. Seeking flushes that context instead of spawning ffmpeg/ffprobe
//! processes for every preview frame. Recently decoded frames are kept in a
//! bounded LRU-style cache so scrubbing back to a nearby frame is immediate.

use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, OnceLock,
    },
    time::UNIX_EPOCH,
};

use ffmpeg_next as ffmpeg;
use ffmpeg::{
    codec,
    ffi,
    format,
    frame,
    media::Type,
    util::{
        format::Pixel,
        rational::Rational,
    },
};

const MAX_CACHED_FRAME_BYTES: usize = 128 * 1024 * 1024;
// Each active timeline clip can own a decoder context. Keep enough slots for
// common multi-track compositions while bounding memory use on mid-range PCs.
const MAX_OPEN_DECODERS: usize = 12;
const SEEK_REUSE_WINDOW_SECONDS: f64 = 1.25;
/// Frames converted *before* a step-seek target so repeated reverse frame
/// steps are served from the cache instead of re-seeking to a keyframe
/// (ported from Yoru's `Player::past` history).
const STEP_HISTORY_FRAMES: usize = 8;
/// Prefix on errors that mean "the hardware decode path failed", so the
/// decoder can reopen itself in software instead of failing the preview.
const HW_ERROR_PREFIX: &str = "Hardware decode failed";

/// True when the most recent converted frame came from a hardware decoder
/// (reported in the F3 overlay so HW vs SW decode is visible).
static LAST_FRAME_HW: AtomicBool = AtomicBool::new(false);

pub fn last_frame_was_hardware() -> bool { LAST_FRAME_HW.load(Relaxed) }

// Request-scoped timing: microseconds spent moving decoded frames to system
// memory and in the fused convert+scale kernel. These are included in
// `decodeWaitMs`; totals are collected across all source layers in the batch.
thread_local! {
    // Per-thread counters keep thumbnail/media work from contaminating preview
    // telemetry. The preview worker resets and snapshots its own counters around
    // the requested batch, before decode-ahead starts touching more frames.
    static DECODE_TRANSFER_US: Cell<u64> = Cell::new(0);
    static DECODE_CONVERT_US: Cell<u64> = Cell::new(0);
}

pub fn begin_decode_timing() {
    DECODE_TRANSFER_US.with(|value| value.set(0));
    DECODE_CONVERT_US.with(|value| value.set(0));
}

pub fn take_decode_timing() -> (u64, u64) {
    let transfer = DECODE_TRANSFER_US.with(|value| { let total = value.get(); value.set(0); total });
    let convert = DECODE_CONVERT_US.with(|value| { let total = value.get(); value.set(0); total });
    (transfer, convert)
}

static FFMPEG_INIT: OnceLock<Result<(), String>> = OnceLock::new();

fn ensure_ffmpeg() -> Result<(), String> {
    match FFMPEG_INIT.get_or_init(|| ffmpeg::init().map_err(|e| e.to_string())) {
        Ok(()) => Ok(()),
        Err(error) => Err(format!("FFmpeg decoder initialization failed: {error}")),
    }
}

#[derive(Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub pts_seconds: f64,
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct FrameKey {
    path: String,
    index: i64,
    max_width: u32,
}

struct VideoDecoder {
    path: String,
    input: format::context::Input,
    decoder: ffmpeg::decoder::Video,
    /// Kept alive for the lifetime of the decoder because `AVCodecContext::opaque`
    /// points at it. Declared after `decoder` so the decoder drops first.
    hw_callback_state: Option<Box<AtomicBool>>,
    /// Reusable system-memory target for `av_hwframe_transfer_data()`.
    hw_transfer: frame::Video,
    stream_index: usize,
    time_base: Rational,
    frame_rate: f64,
    cursor_seconds: f64,
    // FFmpeg's decoder drain is a one-way state until the next seek. Repeated
    // preview requests at/after EOF must not send a second EOF packet.
    decoder_eof: bool,
    // Maximum decoded RGBA width for preview. Keeping this near the monitor's
    // effective resolution avoids uploading full 1080p/4K frames for a small
    // preview surface.
    max_width: u32,
    // Stream-level color tags are authoritative across all frames. Keep them
    // with the decoder because AVFrame tags may be unset or inconsistent.
    stream_color_space: ffi::AVColorSpace,
    stream_color_range: ffi::AVColorRange,
}

// ---- Hardware decode (ported from Yoru) ------------------------------------
// Tries CUDA/NVDEC, then D3D11VA, then DXVA2. If the device or codec has no
// matching configuration, nothing is changed and FFmpeg decodes in software.
#[cfg(windows)]
fn configure_hardware_decoder(
    context: &mut codec::context::Context,
    callback_state: &mut Option<Box<AtomicBool>>,
) -> bool {
    use ffi::AVHWDeviceType;

    let candidates = [
        (AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, "CUDA"),
        (AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, "D3D11VA"),
        (AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2, "DXVA2"),
    ];
    unsafe {
        let codec_ctx = context.as_mut_ptr();
        let decoder = if !(*codec_ctx).codec.is_null() {
            (*codec_ctx).codec
        } else {
            ffi::avcodec_find_decoder((*codec_ctx).codec_id)
        };
        if decoder.is_null() {
            return false;
        }
        for (device_type, name) in candidates {
            let mut index = 0;
            let mut supported = false;
            loop {
                let config = ffi::avcodec_get_hw_config(decoder, index);
                if config.is_null() {
                    break;
                }
                if (*config).device_type == device_type && ((*config).methods & 1) != 0 {
                    supported = true; // AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX
                    break;
                }
                index += 1;
            }
            if !supported {
                continue;
            }
            let mut device_ref = std::ptr::null_mut();
            let result = ffi::av_hwdevice_ctx_create(
                &mut device_ref,
                device_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if result < 0 || device_ref.is_null() {
                continue;
            }
            // AVCodecContext owns this reference after assignment.
            (*codec_ctx).hw_device_ctx = device_ref;
            let mut state = Box::new(AtomicBool::new(false));
            (*codec_ctx).opaque = (&mut *state as *mut AtomicBool).cast();
            (*codec_ctx).get_format = Some(select_hardware_pixel_format);
            (*codec_ctx).extra_hw_frames = 8;
            *callback_state = Some(state);
            eprintln!("Hanabi: trying {name} hardware decoding");
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
fn configure_hardware_decoder(
    _context: &mut codec::context::Context,
    _callback_state: &mut Option<Box<AtomicBool>>,
) -> bool {
    false
}

#[cfg(windows)]
unsafe extern "C" fn select_hardware_pixel_format(
    context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    use ffi::{AVHWDeviceType, AVPixelFormat};
    if context.is_null() || formats.is_null() {
        return AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let wanted = unsafe {
        let device_ref = (*context).hw_device_ctx;
        if device_ref.is_null() || (*device_ref).data.is_null() {
            AVPixelFormat::AV_PIX_FMT_NONE
        } else {
            let device = &*((*device_ref).data.cast::<ffi::AVHWDeviceContext>());
            match device.type_ {
                AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA => AVPixelFormat::AV_PIX_FMT_CUDA,
                AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA => AVPixelFormat::AV_PIX_FMT_D3D11,
                AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2 => AVPixelFormat::AV_PIX_FMT_DXVA2_VLD,
                _ => AVPixelFormat::AV_PIX_FMT_NONE,
            }
        }
    };
    // FFmpeg re-invokes get_format after a failed hardware init; the flag lets
    // the second call fall through to a software format.
    let retrying_after_hw_failure = unsafe {
        !(*context).opaque.is_null()
            && (*((*context).opaque.cast::<AtomicBool>())).swap(true, Relaxed)
    };
    let mut current = formats;
    let mut first_software = AVPixelFormat::AV_PIX_FMT_NONE;
    unsafe {
        while *current != AVPixelFormat::AV_PIX_FMT_NONE {
            let value = *current;
            if value == wanted && !retrying_after_hw_failure {
                return value;
            }
            let is_hardware = matches!(value,
                AVPixelFormat::AV_PIX_FMT_CUDA |
                AVPixelFormat::AV_PIX_FMT_D3D11 |
                AVPixelFormat::AV_PIX_FMT_DXVA2_VLD |
                AVPixelFormat::AV_PIX_FMT_D3D11VA_VLD
            );
            if !is_hardware && first_software == AVPixelFormat::AV_PIX_FMT_NONE {
                first_software = value;
            }
            current = current.add(1);
        }
        if first_software != AVPixelFormat::AV_PIX_FMT_NONE { first_software } else { *formats }
    }
}

/// Hardware decode is worth its GPU->CPU readback on large frames (1080p and
/// up). Smaller sources decode fast enough in software. `HANABI_HWDEC=1|0`
/// forces it on/off for A/B testing.
fn hw_decode_wanted(coded_width: u32) -> bool {
    match std::env::var("HANABI_HWDEC").ok().as_deref() {
        Some("0") | Some("off") | Some("false") => false,
        Some("1") | Some("on") | Some("true") => true,
        _ => coded_width >= 1920,
    }
}

/// Hardware frames are opaque device surfaces. Copy them to a reusable system
/// memory frame (NV12 / P010, both handled by the converter below). Software
/// frames are returned untouched.
fn to_software<'a>(decoded: &'a frame::Video, scratch: &'a mut frame::Video) -> Result<&'a frame::Video, String> {
    if !matches!(decoded.format(), Pixel::CUDA | Pixel::D3D11 | Pixel::DXVA2_VLD) {
        return Ok(decoded);
    }
    let fail = |what: &str| format!("{HW_ERROR_PREFIX}: {what}");
    unsafe {
        let src = decoded.as_ptr();
        let frames_ref = (*src).hw_frames_ctx;
        if frames_ref.is_null() || (*frames_ref).data.is_null() {
            return Err(fail("frame has no hardware frames context"));
        }
        let frames = &*((*frames_ref).data.cast::<ffi::AVHWFramesContext>());
        let sw_format = frames.sw_format;
        if sw_format == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            return Err(fail("unknown software surface format"));
        }
        let dst = scratch.as_mut_ptr();
        let needs_reallocation = (*dst).buf[0].is_null()
            || (*dst).format != sw_format as i32
            || (*dst).width != (*src).width
            || (*dst).height != (*src).height;
        if needs_reallocation {
            ffi::av_frame_unref(dst);
            (*dst).format = sw_format as i32;
        }
        if ffi::av_hwframe_transfer_data(dst, src, 0) < 0 {
            // A driver may change allocation requirements after a format change.
            ffi::av_frame_unref(dst);
            (*dst).format = sw_format as i32;
            if ffi::av_hwframe_transfer_data(dst, src, 0) < 0 {
                return Err(fail("surface transfer to system memory failed"));
            }
        }
    }
    Ok(&*scratch)
}

impl VideoDecoder {
    fn open(path: &str, max_width: u32) -> Result<Self, String> {
        Self::open_with(path, max_width, true)
    }

    fn open_with(path: &str, max_width: u32, allow_hardware: bool) -> Result<Self, String> {
        ensure_ffmpeg()?;
        if !Path::new(path).is_file() {
            return Err(format!("Media file is missing: {path}"));
        }

        let input = format::input(path)
            .map_err(|error| format!("Could not open video source '{path}': {error}"))?;
        let (stream_index, time_base, frame_rate, decoder, hw_callback_state,
             stream_color_space, stream_color_range) = {
            let stream = input
                .streams()
                .best(Type::Video)
                .ok_or_else(|| format!("No video stream found in '{path}'"))?;
            let stream_index = stream.index();
            let time_base = stream.time_base();
            let rate = stream.avg_frame_rate();
            let fps = f64::from(rate);
            let fps = if fps.is_finite() && fps > 0.1 && fps < 1000.0 { fps } else { 30.0 };
            let mut context = codec::context::Context::from_parameters(stream.parameters())
                .map_err(|error| format!("Could not read video stream parameters: {error}"))?;
            // Frame-threaded decode lets FFmpeg parallelise H.264/HEVC across cores.
            context.set_threading(ffmpeg::threading::Config::kind(ffmpeg::threading::Type::Frame));
            let (coded_width, stream_color_space, stream_color_range) = unsafe {
                let codec_context = &*context.as_ptr();
                (codec_context.width.max(0) as u32, codec_context.colorspace, codec_context.color_range)
            };
            let mut hw_callback_state = None;
            let hw_configured = allow_hardware
                && hw_decode_wanted(coded_width)
                && configure_hardware_decoder(&mut context, &mut hw_callback_state);
            let decoder = match context.decoder().video() {
                Ok(decoder) => decoder,
                Err(hw_error) if hw_configured => {
                    // A driver may create a device but fail while the codec
                    // initialises it. Re-open from the original stream
                    // parameters without hardware rather than failing.
                    eprintln!("Hanabi: hardware decoder init failed for {path} ({hw_error}); using software");
                    hw_callback_state = None;
                    let mut fallback = codec::context::Context::from_parameters(stream.parameters())
                        .map_err(|error| format!("Could not read video stream parameters: {error}"))?;
                    fallback.set_threading(ffmpeg::threading::Config::kind(ffmpeg::threading::Type::Frame));
                    fallback.decoder().video()
                        .map_err(|error| format!("Could not initialize video decoder: {error}"))?
                }
                Err(error) => return Err(format!("Could not initialize video decoder: {error}")),
            };
            (stream_index, time_base, fps, decoder, hw_callback_state,
             stream_color_space, stream_color_range)
        };
        Ok(Self {
            path: path.to_owned(),
            input,
            decoder,
            hw_callback_state,
            hw_transfer: frame::Video::empty(),
            stream_index,
            time_base,
            frame_rate,
            cursor_seconds: -1.0,
            decoder_eof: false,
            max_width: max_width.max(2) & !1,
            stream_color_space,
            stream_color_range,
        })
    }

    fn hardware_in_use(&self) -> bool { self.hw_callback_state.is_some() }

    /// Move a decoded frame into system memory if needed, then convert + scale it
    /// to preview RGBA in a single pass.
    fn convert(&mut self, decoded: &frame::Video, pts_seconds: f64) -> Result<DecodedFrame, String> {
        let max_width = self.max_width;
        let transfer_started = std::time::Instant::now();
        let software = to_software(decoded, &mut self.hw_transfer)?;
        DECODE_TRANSFER_US.with(|value| value.set(value.get().saturating_add(transfer_started.elapsed().as_micros() as u64)));
        LAST_FRAME_HW.store(!std::ptr::eq(software, decoded), Relaxed);
        let convert_started = std::time::Instant::now();
        let converted = convert_frame_to_rgba(
            software, pts_seconds, max_width, self.stream_color_space, self.stream_color_range,
        );
        DECODE_CONVERT_US.with(|value| value.set(value.get().saturating_add(convert_started.elapsed().as_micros() as u64)));
        converted
    }

    fn seek(&mut self, seconds: f64) -> Result<(), String> {
        let timestamp = (seconds.max(0.0) * 1_000_000.0).round() as i64;
        if let Err(error) = self.input.seek(timestamp, ..timestamp) {
            // Some containers reject timestamp seeks. Reopen from the start and
            // let the decode loop scan forward (discarding before conversion)
            // instead of leaving the preview stuck at its old position (Yoru).
            eprintln!("Hanabi: video seek to {seconds:.3}s failed for {} ({error}); reopening source", self.path);
            let hardware = self.hardware_in_use();
            let fresh = Self::open_with(&self.path, self.max_width, hardware)
                .map_err(|reopen| format!("Video seek failed ({error}) and reopen failed ({reopen})"))?;
            *self = fresh;
        }
        self.input.clear_eof();
        self.decoder.flush();
        self.decoder_eof = false;
        self.cursor_seconds = -1.0;
        Ok(())
    }

    fn decode_first_frame(&mut self) -> Result<DecodedFrame, String> {
        let mut decoded = frame::Video::empty();
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        self.decoder.send_packet(&packet)
                            .map_err(|error| format!("Thumbnail decode packet failed: {error}"))?;
                        while self.decoder.receive_frame(&mut decoded).is_ok() {
                            let timestamp = decoded.timestamp().or_else(|| decoded.pts())
                                .map(|pts| pts as f64 * f64::from(self.time_base)).unwrap_or(0.0);
                            return self.convert(&decoded, timestamp);
                        }
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()
                        .map_err(|error| format!("Thumbnail decoder flush failed: {error}"))?;
                    while self.decoder.receive_frame(&mut decoded).is_ok() {
                        let timestamp = decoded.timestamp().or_else(|| decoded.pts())
                            .map(|pts| pts as f64 * f64::from(self.time_base)).unwrap_or(0.0);
                        return self.convert(&decoded, timestamp);
                    }
                    return Err("No video frame was available for this media thumbnail".into());
                }
                Err(error) => return Err(format!("Could not read media for thumbnail: {error}")),
            }
        }
    }

    /// Decode the frame at `target_seconds`. Frames before the target are
    /// discarded *before* any pixel conversion. With `stepping`, the few frames
    /// immediately before the target are converted and returned as history so
    /// reverse frame-steps do not each pay for a keyframe seek.
    fn decode_at(&mut self, target_seconds: f64, stepping: bool) -> Result<(DecodedFrame, Vec<DecodedFrame>), String> {
        match self.decode_at_inner(target_seconds, stepping) {
            Err(error) if self.hardware_in_use() && !error.starts_with("No decoded video frame") => {
                // The hardware path misbehaved on this file; reopen in software
                // and retry once (the new decoder never uses hardware).
                eprintln!("Hanabi: {error}; falling back to software decode for {}", self.path);
                let fresh = Self::open_with(&self.path, self.max_width, false)?;
                *self = fresh;
                self.decode_at_inner(target_seconds, stepping)
            }
            other => other,
        }
    }

    fn decode_at_inner(&mut self, target_seconds: f64, stepping: bool) -> Result<(DecodedFrame, Vec<DecodedFrame>), String> {
        let target = target_seconds.max(0.0);
        let tolerance = (0.5 / self.frame_rate).max(0.002);
        let must_seek = self.cursor_seconds < 0.0
            || target + tolerance < self.cursor_seconds
            || target - self.cursor_seconds > SEEK_REUSE_WINDOW_SECONDS;
        if must_seek { self.seek(target)?; }
        let keep_history = stepping && must_seek;
        let history_start = target - STEP_HISTORY_FRAMES as f64 / self.frame_rate - tolerance;
        let mut history: Vec<DecodedFrame> = Vec::new();

        let mut decoded = frame::Video::empty();
        let mut packet = ffmpeg::Packet::empty();
        loop {
            // Drain any frames already queued by the previous packet before
            // reading another packet. This keeps decoder state continuous.
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let timestamp = decoded.timestamp().or_else(|| decoded.pts());
                let pts_seconds = timestamp
                    .map(|pts| pts as f64 * f64::from(self.time_base))
                    .unwrap_or(target);
                self.cursor_seconds = pts_seconds;
                if pts_seconds + tolerance >= target {
                    let frame = self.convert(&decoded, pts_seconds)?;
                    return Ok((frame, history));
                }
                if keep_history && pts_seconds >= history_start {
                    if let Ok(frame) = self.convert(&decoded, pts_seconds) {
                        history.push(frame);
                        if history.len() > STEP_HISTORY_FRAMES { history.remove(0); }
                    }
                }
            }

            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        if let Err(error) = self.decoder.send_packet(&packet) {
                            if self.hardware_in_use() {
                                return Err(format!("{HW_ERROR_PREFIX}: packet rejected ({error})"));
                            }
                            // A damaged software packet is skipped, like Yoru, so one bad
                            // packet does not drop the whole preview to the slow fallback.
                        }
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    if !self.decoder_eof {
                        match self.decoder.send_eof() {
                            Ok(()) | Err(ffmpeg::Error::Eof) => self.decoder_eof = true,
                            Err(error) => return Err(format!("Video decoder flush failed: {error}")),
                        }
                    }
                    while self.decoder.receive_frame(&mut decoded).is_ok() {
                        let timestamp = decoded.timestamp().or_else(|| decoded.pts());
                        let pts_seconds = timestamp
                            .map(|pts| pts as f64 * f64::from(self.time_base))
                            .unwrap_or(target);
                        self.cursor_seconds = pts_seconds;
                        if pts_seconds + tolerance >= target {
                            let frame = self.convert(&decoded, pts_seconds)?;
                            return Ok((frame, history));
                        }
                    }
                    return Err(format!("No decoded video frame at {target:.3}s"));
                }
                Err(error) => return Err(format!("Could not read video packet: {error}")),
            }
        }
    }
}

/// Decode a single source frame in-process, downscale it to a small 16:9 bitmap,
/// and write a BMP that Raylib can load in both the lite and full distributions.
/// This deliberately avoids the FFmpeg CLI so media-library thumbnails work in
/// the lite package (which ships decoder libraries but not ffmpeg.exe).
pub fn write_thumbnail(path: &str, destination: &Path, target_width: u32, target_height: u32) -> Result<(), String> {
    if target_width == 0 || target_height == 0 {
        return Err("Thumbnail dimensions must be nonzero".into());
    }
    // Software only: a one-off thumbnail is not worth creating a GPU decode device.
    let mut decoder = VideoDecoder::open_with(path, target_width, false)?;
    let decoded = decoder.decode_first_frame()?;
    let thumbnail = resize_rgba_fit(&decoded, target_width, target_height)?;
    write_bmp24(destination, target_width, target_height, &thumbnail)
}

fn resize_rgba_fit(source: &DecodedFrame, target_width: u32, target_height: u32) -> Result<Vec<u8>, String> {
    let source_width = source.width as usize;
    let source_height = source.height as usize;
    let expected = source_width.checked_mul(source_height).and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "Thumbnail source dimensions overflowed".to_owned())?;
    if source.rgba.len() != expected || source_width == 0 || source_height == 0 {
        return Err("Thumbnail source frame has invalid RGBA data".into());
    }
    let target_w = target_width as usize;
    let target_h = target_height as usize;
    let scale = (target_w as f64 / source_width as f64).min(target_h as f64 / source_height as f64);
    let draw_w = ((source_width as f64 * scale).round() as usize).clamp(1, target_w);
    let draw_h = ((source_height as f64 * scale).round() as usize).clamp(1, target_h);
    let offset_x = (target_w - draw_w) / 2;
    let offset_y = (target_h - draw_h) / 2;
    let mut output = vec![0u8; target_w * target_h * 4];
    // Opaque dark backing makes transparent PNGs and letterboxed media thumbnails clean.
    for pixel in output.chunks_exact_mut(4) { pixel.copy_from_slice(&[12, 13, 17, 255]); }

    let sample = |x: usize, y: usize, channel: usize| -> f64 {
        source.rgba[(y * source_width + x) * 4 + channel] as f64
    };
    for dy in 0..draw_h {
        let sy = (((dy as f64 + 0.5) * source_height as f64 / draw_h as f64) - 0.5)
            .clamp(0.0, source_height.saturating_sub(1) as f64);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(source_height - 1);
        let fy = sy - y0 as f64;
        for dx in 0..draw_w {
            let sx = (((dx as f64 + 0.5) * source_width as f64 / draw_w as f64) - 0.5)
                .clamp(0.0, source_width.saturating_sub(1) as f64);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(source_width - 1);
            let fx = sx - x0 as f64;
            let destination_index = ((offset_y + dy) * target_w + offset_x + dx) * 4;
            let alpha = (sample(x0, y0, 3) * (1.0 - fx) * (1.0 - fy)
                + sample(x1, y0, 3) * fx * (1.0 - fy)
                + sample(x0, y1, 3) * (1.0 - fx) * fy
                + sample(x1, y1, 3) * fx * fy) / 255.0;
            for channel in 0..3 {
                let value = sample(x0, y0, channel) * (1.0 - fx) * (1.0 - fy)
                    + sample(x1, y0, channel) * fx * (1.0 - fy)
                    + sample(x0, y1, channel) * (1.0 - fx) * fy
                    + sample(x1, y1, channel) * fx * fy;
                // Composite alpha over the dark library-tile background.
                output[destination_index + channel] = (value * alpha / 255.0
                    + [12.0, 13.0, 17.0][channel] * (1.0 - alpha)).round().clamp(0.0, 255.0) as u8;
            }
            output[destination_index + 3] = 255;
        }
    }
    Ok(output)
}

fn write_bmp24(destination: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let row_bytes = width.checked_mul(3).ok_or_else(|| "Thumbnail bitmap row size overflowed".to_owned())?;
    let padded_row = (row_bytes + 3) & !3;
    let image_bytes = padded_row.checked_mul(height).ok_or_else(|| "Thumbnail bitmap size overflowed".to_owned())?;
    let file_bytes = 54u32.checked_add(image_bytes).ok_or_else(|| "Thumbnail bitmap file size overflowed".to_owned())?;
    if rgba.len() != width as usize * height as usize * 4 {
        return Err("Thumbnail bitmap received invalid RGBA data".into());
    }

    let mut header = [0u8; 54];
    header[0..2].copy_from_slice(b"BM");
    header[2..6].copy_from_slice(&file_bytes.to_le_bytes());
    header[10..14].copy_from_slice(&54u32.to_le_bytes());
    header[14..18].copy_from_slice(&40u32.to_le_bytes());
    header[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    header[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    header[26..28].copy_from_slice(&1u16.to_le_bytes());
    header[28..30].copy_from_slice(&24u16.to_le_bytes());
    header[34..38].copy_from_slice(&image_bytes.to_le_bytes());
    header[38..42].copy_from_slice(&2835i32.to_le_bytes());
    header[42..46].copy_from_slice(&2835i32.to_le_bytes());

    let mut file = std::fs::File::create(destination)
        .map_err(|error| format!("Could not create media thumbnail '{}': {error}", destination.display()))?;
    use std::io::Write;
    file.write_all(&header).map_err(|error| format!("Could not write thumbnail header: {error}"))?;
    let mut row = vec![0u8; padded_row as usize];
    for y in (0..height as usize).rev() {
        let row_start = y * width as usize * 4;
        for x in 0..width as usize {
            let source_index = row_start + x * 4;
            let target_index = x * 3;
            row[target_index..target_index + 3]
                .copy_from_slice(&[rgba[source_index + 2], rgba[source_index + 1], rgba[source_index]]);
        }
        file.write_all(&row).map_err(|error| format!("Could not write thumbnail row: {error}"))?;
    }
    file.flush().map_err(|error| format!("Could not finish media thumbnail: {error}"))
}

#[inline]
fn clamp8(value: i64) -> u8 { value.clamp(0, 255) as u8 }

#[inline]
fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

/// Fixed-point (20 bit) YCbCr -> RGB coefficients for one bit depth, range and matrix.
#[derive(Clone, Copy)]
struct Coef { yoff: i64, ys: i64, coff: i64, rv: i64, gu: i64, gv: i64, bu: i64 }

impl Coef {
    fn new(depth: u32, full: bool, kr: f64, kb: f64) -> Self {
        let kg = 1.0 - kr - kb;
        let sh = (1u32 << (depth - 8)) as f64;
        let (yoff, ysc, csc) = if full { (0.0, 1.0, 1.0) } else { (16.0 * sh, 255.0 / 219.0, 255.0 / 224.0) };
        let q = (1u64 << 20) as f64;
        Coef {
            yoff: yoff.round() as i64,
            ys: (ysc / sh * q).round() as i64,
            coff: (128.0 * sh).round() as i64,
            rv: (2.0 * (1.0 - kr) * csc / sh * q).round() as i64,
            gu: (2.0 * kb * (1.0 - kb) / kg * csc / sh * q).round() as i64,
            gv: (2.0 * kr * (1.0 - kr) / kg * csc / sh * q).round() as i64,
            bu: (2.0 * (1.0 - kb) * csc / sh * q).round() as i64,
        }
    }

    #[inline(always)]
    fn rgb(&self, y: i64, u: i64, v: i64) -> [u8; 3] {
        let yy = (y - self.yoff) * self.ys + (1 << 19);
        let (u, v) = (u - self.coff, v - self.coff);
        [clamp8((yy + self.rv * v) >> 20), clamp8((yy - self.gu * u - self.gv * v) >> 20), clamp8((yy + self.bu * u) >> 20)]
    }
}

/// Pick the YCbCr matrix the file was mastered with. Untagged HD material is BT.709,
/// untagged SD is BT.601; the old converter always used BT.601, which shifted HD colours.
fn matrix_and_range(
    source: &frame::Video,
    height: u32,
    stream_color_space: ffi::AVColorSpace,
    stream_color_range: ffi::AVColorRange,
) -> (f64, f64, bool) {
    // Prefer the stream tags for every frame. A few decoders populate the first
    // AVFrame with its colour metadata but leave subsequent frames unspecified;
    // using per-frame fallbacks then produces visible hue/temperature jumps
    // between otherwise identical copies of the same source on different tracks.
    let stream_matrix = match stream_color_space {
        ffi::AVColorSpace::AVCOL_SPC_BT709 => Some((0.2126, 0.0722)),
        ffi::AVColorSpace::AVCOL_SPC_BT470BG | ffi::AVColorSpace::AVCOL_SPC_SMPTE170M => Some((0.299, 0.114)),
        ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL | ffi::AVColorSpace::AVCOL_SPC_BT2020_CL => Some((0.2627, 0.0593)),
        _ => None,
    };
    // Do not let per-frame fallback tags override the same stream's matrix:
    // hardware and software decoder outputs can disagree about whether those
    // frame tags were copied. That is enough to make identical stacked clips
    // render with different colour temperatures. Use one deterministic rule
    // for every frame from this stream when the stream-level tag is missing.
    let (kr, kb) = stream_matrix
        .unwrap_or_else(|| if height >= 720 { (0.2126, 0.0722) } else { (0.299, 0.114) });
    let full_range = match stream_color_range {
        ffi::AVColorRange::AVCOL_RANGE_JPEG => true,
        ffi::AVColorRange::AVCOL_RANGE_MPEG => false,
        _ => matches!(source.format(),
            Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P),
    };
    (kr, kb, full_range)
}

/// Split an RGBA image into row bands and fill them on all cores. Frames are converted
/// on the decode worker, so this is what keeps 1080p decode-to-texture latency low.
fn par_rows(rgba: &mut [u8], width: usize, height: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let row_bytes = width * 4;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8).min(height.max(1));
    if threads <= 1 || height < 96 {
        for (y, row) in rgba.chunks_exact_mut(row_bytes).enumerate() { f(y, row); }
        return;
    }
    let rows_per = (height + threads - 1) / threads;
    std::thread::scope(|scope| {
        for (band, chunk) in rgba.chunks_mut(rows_per * row_bytes).enumerate() {
            let f = &f;
            scope.spawn(move || {
                for (i, row) in chunk.chunks_exact_mut(row_bytes).enumerate() { f(band * rows_per + i, row); }
            });
        }
    });
}

/// Preview size for a decoded frame: never upscales, keeps the aspect ratio and
/// uses even dimensions when it has to shrink.
fn scaled_size(source_width: u32, source_height: u32, max_width: u32) -> (u32, u32) {
    if source_width <= max_width { return (source_width, source_height); }
    let out_width = (max_width & !1).max(2);
    let out_height = (((source_height as u64 * out_width as u64 + source_width as u64 / 2)
        / source_width as u64) as u32 & !1).max(2);
    (out_width, out_height)
}

/// Integer box edges mapping `dst` output samples onto `src` source samples
/// (`dst <= src`). Every box holds at least one source sample.
fn box_edges(src: usize, dst: usize) -> Vec<usize> {
    (0..=dst).map(|i| i * src / dst.max(1)).collect()
}

/// Chroma sample range covering the luma range `[a, b)` for a plane subsampled by `shift` bits.
#[inline(always)]
fn chroma_range(a: usize, b: usize, shift: usize) -> (usize, usize) {
    let start = a >> shift;
    let end = ((b + (1 << shift) - 1) >> shift).max(start + 1);
    (start, end)
}

/// Fused YCbCr -> RGBA conversion and area downscale in ONE pass.
///
/// The previous pipeline converted every source pixel to RGBA at full size and
/// then ran a floating-point box filter over the result, so a 1080p frame cost
/// ~2M colour conversions plus ~8M f64 multiplies before it ever reached the
/// GPU. Averaging in YCbCr space (the RGB matrix is linear) and converting once
/// per *output* pixel gives the same area-averaged image for a quarter of the
/// conversions and no float work. `luma`/`chroma` read raw plane samples, so the
/// same loop serves planar, semi-planar and high-bit-depth layouts.
#[allow(clippy::too_many_arguments)]
fn scale_yuv_to_rgba<FY, FC>(
    rgba: &mut [u8], src_w: usize, src_h: usize, out_w: usize, out_h: usize,
    sx: usize, sy: usize, coef: Coef, luma: FY, chroma: FC,
) where
    FY: Fn(usize, usize) -> i64 + Sync,
    FC: Fn(usize, usize) -> (i64, i64) + Sync,
{
    let xe = box_edges(src_w, out_w);
    let ye = box_edges(src_h, out_h);
    let (xe, ye) = (&xe[..], &ye[..]);
    par_rows(rgba, out_w, out_h, |oy, row| {
        let (y0, y1) = (ye[oy], ye[oy + 1]);
        let (cy0, cy1) = chroma_range(y0, y1, sy);
        for ox in 0..out_w {
            let (x0, x1) = (xe[ox], xe[ox + 1]);
            let (cx0, cx1) = chroma_range(x0, x1, sx);
            let mut y_sum = 0i64;
            for yy in y0..y1 { for xx in x0..x1 { y_sum += luma(xx, yy); } }
            let y_n = ((y1 - y0) * (x1 - x0)) as i64;
            let (mut u_sum, mut v_sum) = (0i64, 0i64);
            for cy in cy0..cy1 {
                for cx in cx0..cx1 {
                    let (u, v) = chroma(cx, cy);
                    u_sum += u;
                    v_sum += v;
                }
            }
            let c_n = ((cy1 - cy0) * (cx1 - cx0)) as i64;
            let rgb = coef.rgb((y_sum + y_n / 2) / y_n, (u_sum + c_n / 2) / c_n, (v_sum + c_n / 2) / c_n);
            row[ox * 4..ox * 4 + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    });
}

/// Packed RGB(A) sources: area-average each channel straight into the output size.
fn scale_packed_to_rgba(
    rgba: &mut [u8], src: &[u8], stride: usize, bytes_per_pixel: usize, order: [usize; 4],
    src_w: usize, src_h: usize, out_w: usize, out_h: usize,
) {
    let xe = box_edges(src_w, out_w);
    let ye = box_edges(src_h, out_h);
    let (xe, ye) = (&xe[..], &ye[..]);
    par_rows(rgba, out_w, out_h, |oy, row| {
        let (y0, y1) = (ye[oy], ye[oy + 1]);
        for ox in 0..out_w {
            let (x0, x1) = (xe[ox], xe[ox + 1]);
            let mut sums = [0u32; 4];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let s = yy * stride + xx * bytes_per_pixel;
                    sums[0] += src[s + order[0]] as u32;
                    sums[1] += src[s + order[1]] as u32;
                    sums[2] += src[s + order[2]] as u32;
                    sums[3] += if bytes_per_pixel == 4 { src[s + order[3]] as u32 } else { 255 };
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for c in 0..4 { row[ox * 4 + c] = ((sums[c] + n / 2) / n) as u8; }
        }
    });
}

/// Decode Hanabi's own uncompressed RGBA TGA title textures without creating
/// an FFmpeg format/codec context for each live colour update. Other still-image
/// formats continue through the normal FFmpeg decoder below.
fn decode_uncompressed_tga(path: &str, max_width: u32, revision: i64) -> Result<Option<DecodedFrame>, String> {
    if !Path::new(path).extension().and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tga")) {
        return Ok(None);
    }

    let bytes = std::fs::read(path).map_err(|error| format!("Could not read title texture: {error}"))?;
    if bytes.len() < 18 { return Err("Title TGA header is truncated".into()); }
    let id_length = bytes[0] as usize;
    let color_map_type = bytes[1];
    let image_type = bytes[2];
    let width = u16::from_le_bytes([bytes[12], bytes[13]]) as usize;
    let height = u16::from_le_bytes([bytes[14], bytes[15]]) as usize;
    let depth = bytes[16];
    let descriptor = bytes[17];

    // Raylib's ExportImage writes type-2, 32-bit TGA files. Leave other TGA
    // variants to FFmpeg rather than partially decoding an unsupported layout.
    if color_map_type != 0 || image_type != 2 || (depth != 24 && depth != 32) {
        return Ok(None);
    }
    if width == 0 || height == 0 {
        return Err("Title TGA has invalid dimensions".into());
    }
    let bytes_per_pixel = (depth / 8) as usize;
    let pixels = width.checked_mul(height).ok_or("Title TGA dimensions overflow")?;
    let pixel_bytes = pixels.checked_mul(bytes_per_pixel).ok_or("Title TGA size overflow")?;
    let data_start = 18usize.checked_add(id_length).ok_or("Title TGA offset overflow")?;
    let data_end = data_start.checked_add(pixel_bytes).ok_or("Title TGA size overflow")?;
    if data_end > bytes.len() {
        return Err("Title TGA pixel data is truncated".into());
    }
    if pixel_bytes > 512 * 1024 * 1024 {
        return Err("Title TGA is too large for the preview decoder".into());
    }

    let top_origin = descriptor & 0x20 != 0;
    let right_origin = descriptor & 0x10 != 0;
    let mut bgra = vec![0u8; pixels * 4];
    for y in 0..height {
        let source_y = if top_origin { y } else { height - 1 - y };
        for x in 0..width {
            let source_x = if right_origin { width - 1 - x } else { x };
            let source = data_start + (source_y * width + source_x) * bytes_per_pixel;
            let target = (y * width + x) * 4;
            // TGA stores colour in BGRA order; WGPU expects RGBA.
            bgra[target] = bytes[source];
            bgra[target + 1] = bytes[source + 1];
            bgra[target + 2] = bytes[source + 2];
            bgra[target + 3] = if bytes_per_pixel == 4 { bytes[source + 3] } else { 255 };
        }
    }

    let (out_width, out_height) = scaled_size(width as u32, height as u32, max_width);
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];
    scale_packed_to_rgba(
        &mut rgba, &bgra, width * 4, 4, [2, 1, 0, 3],
        width, height, out_width as usize, out_height as usize,
    );
    Ok(Some(DecodedFrame {
        width: out_width,
        height: out_height,
        // Still-image timestamps are otherwise unused. Carry the file revision
        // here so the WGPU source cache sees a changed title at the same path.
        pts_seconds: revision.max(0) as f64 / 1_000_000_000.0,
        rgba: Arc::new(rgba),
    }))
}

fn convert_frame_to_rgba(
    source: &frame::Video,
    pts_seconds: f64,
    max_width: u32,
    stream_color_space: ffi::AVColorSpace,
    stream_color_range: ffi::AVColorRange,
) -> Result<DecodedFrame, String> {
    let width = source.width();
    let height = source.height();
    if width == 0 || height == 0 { return Err("Decoder returned an empty video frame".into()); }
    let (w, h) = (width as usize, height as usize);
    let (out_width, out_height) = scaled_size(width, height, max_width);
    let (ow, oh) = (out_width as usize, out_height as usize);
    let mut rgba = vec![0u8; ow * oh * 4];
    let pixel_format = source.format();
    let (kr, kb, tagged_full) = matrix_and_range(source, height, stream_color_space, stream_color_range);

    match pixel_format {
        Pixel::RGBA | Pixel::BGRA | Pixel::ARGB | Pixel::ABGR => {
            let order = match pixel_format {
                Pixel::RGBA => [0, 1, 2, 3],
                Pixel::BGRA => [2, 1, 0, 3],
                Pixel::ARGB => [1, 2, 3, 0],
                _ => [3, 2, 1, 0],
            };
            scale_packed_to_rgba(&mut rgba, source.data(0), source.stride(0), 4, order, w, h, ow, oh);
        }
        Pixel::RGB24 | Pixel::BGR24 => {
            let order = if pixel_format == Pixel::RGB24 { [0, 1, 2, 0] } else { [2, 1, 0, 0] };
            scale_packed_to_rgba(&mut rgba, source.data(0), source.stride(0), 3, order, w, h, ow, oh);
        }
        Pixel::YUV420P | Pixel::YUVJ420P | Pixel::YUV422P | Pixel::YUVJ422P | Pixel::YUV444P | Pixel::YUVJ444P => {
            let (sx, sy, jpeg) = match pixel_format {
                Pixel::YUV420P => (1, 1, false), Pixel::YUVJ420P => (1, 1, true),
                Pixel::YUV422P => (1, 0, false), Pixel::YUVJ422P => (1, 0, true),
                Pixel::YUV444P => (0, 0, false), _ => (0, 0, true),
            };
            let coef = Coef::new(8, jpeg || tagged_full, kr, kb);
            let (yp, up, vp) = (source.data(0), source.data(1), source.data(2));
            let (ys, us, vs) = (source.stride(0), source.stride(1), source.stride(2));
            scale_yuv_to_rgba(&mut rgba, w, h, ow, oh, sx, sy, coef,
                |x, y| yp[y * ys + x] as i64,
                |cx, cy| (up[cy * us + cx] as i64, vp[cy * vs + cx] as i64));
        }
        Pixel::NV12 | Pixel::NV21 => {
            let swap = pixel_format == Pixel::NV21;
            let coef = Coef::new(8, tagged_full, kr, kb);
            let (yp, uvp) = (source.data(0), source.data(1));
            let (ys, uvs) = (source.stride(0), source.stride(1));
            scale_yuv_to_rgba(&mut rgba, w, h, ow, oh, 1, 1, coef,
                |x, y| yp[y * ys + x] as i64,
                |cx, cy| {
                    let o = cy * uvs + cx * 2;
                    let (a, b) = (uvp[o] as i64, uvp[o + 1] as i64);
                    if swap { (b, a) } else { (a, b) }
                });
        }
        Pixel::YUV420P9LE | Pixel::YUV420P10LE | Pixel::YUV420P12LE | Pixel::YUV420P16LE
        | Pixel::YUV422P9LE | Pixel::YUV422P10LE | Pixel::YUV422P12LE | Pixel::YUV422P16LE
        | Pixel::YUV444P9LE | Pixel::YUV444P10LE | Pixel::YUV444P12LE | Pixel::YUV444P16LE => {
            let depth: u32 = match pixel_format {
                Pixel::YUV420P9LE | Pixel::YUV422P9LE | Pixel::YUV444P9LE => 9,
                Pixel::YUV420P10LE | Pixel::YUV422P10LE | Pixel::YUV444P10LE => 10,
                Pixel::YUV420P12LE | Pixel::YUV422P12LE | Pixel::YUV444P12LE => 12,
                _ => 16,
            };
            let (sx, sy) = match pixel_format {
                Pixel::YUV420P9LE | Pixel::YUV420P10LE | Pixel::YUV420P12LE | Pixel::YUV420P16LE => (1, 1),
                Pixel::YUV422P9LE | Pixel::YUV422P10LE | Pixel::YUV422P12LE | Pixel::YUV422P16LE => (1, 0),
                _ => (0, 0),
            };
            let coef = Coef::new(depth, tagged_full, kr, kb);
            let (yp, up, vp) = (source.data(0), source.data(1), source.data(2));
            let (ys, us, vs) = (source.stride(0), source.stride(1), source.stride(2));
            scale_yuv_to_rgba(&mut rgba, w, h, ow, oh, sx, sy, coef,
                |x, y| read_u16_le(yp, y * ys + x * 2) as i64,
                |cx, cy| (read_u16_le(up, cy * us + cx * 2) as i64, read_u16_le(vp, cy * vs + cx * 2) as i64));
        }
        Pixel::P010LE => {
            let coef = Coef::new(10, tagged_full, kr, kb);
            let (yp, uvp) = (source.data(0), source.data(1));
            let (ys, uvs) = (source.stride(0), source.stride(1));
            scale_yuv_to_rgba(&mut rgba, w, h, ow, oh, 1, 1, coef,
                |x, y| (read_u16_le(yp, y * ys + x * 2) >> 6) as i64,
                |cx, cy| {
                    let o = cy * uvs + cx * 4;
                    ((read_u16_le(uvp, o) >> 6) as i64, (read_u16_le(uvp, o + 2) >> 6) as i64)
                });
        }
        other => return Err(format!("Preview decoder does not yet convert pixel format {other:?}. Try generating a compatible proxy (8-bit RGB/YUV or supported 9/10/12/16-bit planar YUV and P010).")),
    }

    Ok(DecodedFrame { width: out_width, height: out_height, pts_seconds, rgba: Arc::new(rgba) })
}

#[derive(Default)]
pub struct DecoderPool {
    // Keep an independent sequential decoder per timeline clip. Several clips
    // can point at the same media file while sampling different source-time
    // ranges; a single path-keyed decoder would seek backwards/forwards between
    // those ranges on every preview frame.
    decoders: HashMap<String, VideoDecoder>,
    decoder_order: VecDeque<String>,
    frame_rates: HashMap<String, f64>,
    // Decoded pixels remain shared by media path + source frame + output size,
    // even when different clip-specific decoder contexts produce them.
    frames: HashMap<FrameKey, DecodedFrame>,
    frame_order: VecDeque<FrameKey>,
    cached_bytes: usize,
}

impl DecoderPool {
    fn remember_decoder(&mut self, stream_key: &str, path: &str, decoder: VideoDecoder) {
        self.frame_rates.insert(path.to_owned(), decoder.frame_rate);
        self.decoders.insert(stream_key.to_owned(), decoder);
        self.decoder_order.retain(|item| item != stream_key);
        self.decoder_order.push_back(stream_key.to_owned());
        while self.decoder_order.len() > MAX_OPEN_DECODERS {
            if let Some(oldest) = self.decoder_order.pop_front() {
                self.decoders.remove(&oldest);
            }
        }
    }

    /// Native frame rate of an already-opened source.
    pub fn frame_rate_of(&self, path: &str) -> Option<f64> {
        self.frame_rates.get(path).copied()
    }

    /// Insert one frame into the bounded cache (no-op if already present).
    fn cache_insert(&mut self, key: FrameKey, frame: &DecodedFrame) {
        if self.frames.contains_key(&key) { return; }
        self.cached_bytes += frame.rgba.len();
        self.frames.insert(key.clone(), frame.clone());
        self.frame_order.push_back(key);
        while self.cached_bytes > MAX_CACHED_FRAME_BYTES {
            let Some(oldest) = self.frame_order.pop_front() else { break; };
            if let Some(removed) = self.frames.remove(&oldest) {
                self.cached_bytes = self.cached_bytes.saturating_sub(removed.rgba.len());
            }
        }
    }

    /// Load and cache the first frame of a still image for WGPU compositing.
    /// Image revisions are keyed by file modification time, so a live title can
    /// reuse its path without returning stale pixels after a colour/style edit.
    pub fn still_frame(&mut self, path: &str, max_width: u32) -> Result<DecodedFrame, String> {
        let max_width = max_width.max(2) & !1;
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("Could not inspect still image: {error}"))?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let modified_ns = modified.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let revision = modified_ns.min(i64::MAX as u128) as i64;

        // A live title reuses one of two stable paths. Drop only stale versions
        // of that path so editing never accumulates old title textures in cache.
        let stale: Vec<FrameKey> = self.frames.keys()
            .filter(|key| key.path == path && key.max_width == max_width && key.index != revision)
            .cloned().collect();
        for stale_key in stale {
            if let Some(frame) = self.frames.remove(&stale_key) {
                self.cached_bytes = self.cached_bytes.saturating_sub(frame.rgba.len());
            }
        }
        self.frame_order.retain(|key| key.path != path || key.max_width != max_width || key.index == revision);

        let key = FrameKey { path: path.to_owned(), index: revision, max_width };
        if let Some(frame) = self.frames.get(&key).cloned() {
            if let Some(index) = self.frame_order.iter().position(|item| item == &key) {
                self.frame_order.remove(index);
            }
            self.frame_order.push_back(key);
            return Ok(frame);
        }

        let frame = match decode_uncompressed_tga(path, max_width, revision)? {
            Some(frame) => frame,
            None => {
                let mut decoder = VideoDecoder::open_with(path, max_width, false)?;
                let mut frame = decoder.decode_first_frame()?;
                // Include file revision in the GPU key for all image formats.
                frame.pts_seconds = revision.max(0) as f64 / 1_000_000_000.0;
                frame
            }
        };
        self.cache_insert(key, &frame);
        Ok(frame)
    }

    /// `stream_key` is normally the timeline clip ID. It keeps the decoder's
    /// read head aligned with that clip's own source-time progression. The
    /// pixel cache is still global to the underlying file, so exact duplicate
    /// source-frame requests are shared without a second decode.
    pub fn frame_at(&mut self, stream_key: &str, path: &str, seconds: f64, max_width: u32, stepping: bool) -> Result<DecodedFrame, String> {
        let max_width = max_width.max(2) & !1;
        // Obtain stream metadata once per underlying file. Opening this clip's
        // own decoder below does not need to repeat metadata discovery.
        let frame_rate = if let Some(rate) = self.frame_rates.get(path).copied() {
            rate
        } else {
            let decoder = VideoDecoder::open(path, max_width)?;
            let rate = decoder.frame_rate;
            self.remember_decoder(stream_key, path, decoder);
            rate
        };
        let frame_index = (seconds.max(0.0) * frame_rate).round() as i64;
        let key = FrameKey { path: path.to_owned(), index: frame_index, max_width };
        if let Some(frame) = self.frames.get(&key).cloned() {
            if let Some(index) = self.frame_order.iter().position(|item| item == &key) {
                self.frame_order.remove(index);
            }
            self.frame_order.push_back(key);
            if self.decoders.contains_key(stream_key) {
                self.decoder_order.retain(|item| item != stream_key);
                self.decoder_order.push_back(stream_key.to_owned());
            }
            return Ok(frame);
        }

        let decoder_path_changed = self.decoders.get(stream_key).is_some_and(|decoder| decoder.path != path);
        if decoder_path_changed {
            self.decoders.remove(stream_key);
            self.decoder_order.retain(|item| item != stream_key);
        }
        if !self.decoders.contains_key(stream_key) {
            let decoder = VideoDecoder::open(path, max_width)?;
            self.remember_decoder(stream_key, path, decoder);
        } else {
            self.decoder_order.retain(|item| item != stream_key);
            self.decoder_order.push_back(stream_key.to_owned());
        }

        let decode_result = {
            let decoder = self.decoders.get_mut(stream_key)
                .ok_or("Video decoder cache lost its clip stream")?;
            // Output width can change with animated scale or preview quality. It is
            // a conversion setting, not codec state, so update it without reopening
            // the FFmpeg input/decoder context.
            decoder.max_width = max_width;
            decoder.decode_at(seconds, stepping)
        };
        let (decoded, history) = match decode_result {
            Ok(result) => result,
            Err(error) if error.starts_with("No decoded video frame at") &&
                self.decoders.get(stream_key).is_some_and(|decoder| decoder.decoder_eof) => {
                // Hold the closest already-decoded source frame when a clip's
                // source is shorter than its timeline duration. This is normal
                // EOF behavior, not a reason to fail the WGPU preview request.
                // The cached pixels are still consumed exclusively by WGPU.
                let held = self.frames.iter()
                    .filter(|(cached_key, _)| cached_key.path == path &&
                        cached_key.max_width == max_width && cached_key.index <= frame_index)
                    .max_by_key(|(cached_key, _)| cached_key.index)
                    .map(|(_, frame)| frame.clone());
                match held {
                    Some(frame) => (frame, Vec::new()),
                    None => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        // History first so the requested frame ends up most-recently-used.
        for earlier in &history {
            let index = (earlier.pts_seconds.max(0.0) * frame_rate).round() as i64;
            if index != frame_index {
                self.cache_insert(FrameKey { path: path.to_owned(), index, max_width }, earlier);
            }
        }
        self.cache_insert(key, &decoded);
        Ok(decoded)
    }
}

#[cfg(windows)]
#[link(name = "bcrypt")]
extern "C" {}


#[cfg(test)]
mod thumbnail_tests {
    use super::*;

    #[test]
    fn thumbnail_writer_outputs_small_loadable_bmp() {
        let source = DecodedFrame {
            width: 2,
            height: 1,
            pts_seconds: 0.0,
            rgba: Arc::new(vec![255, 0, 0, 255, 0, 255, 0, 255]),
        };
        let resized = resize_rgba_fit(&source, 4, 4).expect("thumbnail resize");
        assert_eq!(resized.len(), 4 * 4 * 4);
        assert_eq!(&resized[0..4], &[12, 13, 17, 255]); // letterbox is opaque

        let path = std::env::temp_dir().join(format!("hanabi-thumbnail-test-{}.bmp", std::process::id()));
        write_bmp24(&path, 4, 4, &resized).expect("write BMP thumbnail");
        let bytes = std::fs::read(&path).expect("read BMP thumbnail");
        assert_eq!(&bytes[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(bytes[18..22].try_into().unwrap()), 4);
        assert_eq!(u32::from_le_bytes(bytes[22..26].try_into().unwrap()), 4);
        assert_eq!(bytes.len(), 54 + 4 * 4 * 3);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod scaler_tests {
    use super::*;

    #[test]
    fn scaled_size_never_upscales_and_stays_even() {
        assert_eq!(scaled_size(1920, 1080, 960), (960, 540));
        assert_eq!(scaled_size(3840, 2160, 960), (960, 540));
        assert_eq!(scaled_size(640, 360, 960), (640, 360));
        let (w, h) = scaled_size(1919, 1081, 854);
        assert!(w % 2 == 0 && h % 2 == 0 && w <= 854);
    }

    #[test]
    fn box_edges_cover_the_source_without_gaps_or_empty_boxes() {
        let edges = box_edges(1080, 540);
        assert_eq!(edges.len(), 541);
        assert_eq!((edges[0], edges[540]), (0, 1080));
        assert!(edges.windows(2).all(|pair| pair[1] > pair[0]));
        let uneven = box_edges(1920, 854);
        assert_eq!(*uneven.last().unwrap(), 1920);
        assert!(uneven.windows(2).all(|pair| pair[1] > pair[0]));
    }

    #[test]
    fn chroma_range_is_never_empty_and_stays_inside_the_plane() {
        for shift in 0..=1usize {
            for a in 0..8usize {
                for b in (a + 1)..=9usize {
                    let (start, end) = chroma_range(a, b, shift);
                    assert!(end > start);
                    assert!(end <= (9 + (1 << shift) - 1) >> shift);
                }
            }
        }
    }

    #[test]
    fn fused_scaler_keeps_flat_colour_and_area_averages_luma() {
        let coef = Coef::new(8, true, 0.2126, 0.0722);
        let mut flat = vec![0u8; 2 * 2 * 4];
        scale_yuv_to_rgba(&mut flat, 4, 4, 2, 2, 1, 1, coef, |_, _| 128, |_, _| (128, 128));
        for pixel in flat.chunks_exact(4) {
            assert_eq!(pixel.to_vec(), vec![128u8, 128, 128, 255]);
        }
        // Luma 0,60 | 120,180 over two rows: each 2x2 box averages to 30 and 150.
        let mut ramp = vec![0u8; 2 * 4];
        scale_yuv_to_rgba(&mut ramp, 4, 2, 2, 1, 1, 1, coef, |x, _| x as i64 * 60, |_, _| (128, 128));
        assert_eq!((ramp[0], ramp[4]), (30, 150));
    }

    #[test]
    fn packed_scaler_averages_channels() {
        // 2x1 RGBA: pure red and pure blue average into one purple pixel.
        let src = [255u8, 0, 0, 255, 0, 0, 255, 255];
        let mut out = vec![0u8; 4];
        scale_packed_to_rgba(&mut out, &src, 8, 4, [0, 1, 2, 3], 2, 1, 1, 1);
        assert_eq!(out, vec![128u8, 0, 128, 255]);
    }
}

#[cfg(test)]
mod still_image_tests {
    use super::*;

    #[test]
    fn still_image_frames_are_cached_and_reused() {
        let path = std::env::temp_dir().join(format!(
            "hanabi-still-frame-{}.bmp", std::process::id()
        ));
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255,
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        write_bmp24(&path, 2, 2, &rgba).expect("write still-image fixture");

        let mut pool = DecoderPool::default();
        let first = pool.still_frame(path.to_str().unwrap(), 2).expect("decode still image");
        let second = pool.still_frame(path.to_str().unwrap(), 2).expect("reuse still image");
        assert_eq!((first.width, first.height), (2, 2));
        assert!(Arc::ptr_eq(&first.rgba, &second.rgba));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn title_tga_decodes_as_rgba_without_ffmpeg() {
        let path = std::env::temp_dir().join(format!(
            "hanabi-title-tga-{}.tga", std::process::id()
        ));
        let mut data = vec![0u8; 18];
        data[2] = 2; // uncompressed true colour
        data[12..14].copy_from_slice(&2u16.to_le_bytes());
        data[14..16].copy_from_slice(&1u16.to_le_bytes());
        data[16] = 32;
        data[17] = 0x28; // top-left origin, eight alpha bits
        // TGA stores BGRA; decoded output must be RGBA.
        data.extend_from_slice(&[30, 20, 10, 40, 70, 60, 50, 80]);
        std::fs::write(&path, data).expect("write title TGA fixture");

        let frame = decode_uncompressed_tga(path.to_str().unwrap(), 2, 123)
            .expect("TGA parser succeeds").expect("recognized uncompressed TGA");
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(&frame.rgba[..], &[10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(frame.pts_seconds, 123.0 / 1_000_000_000.0);
        let _ = std::fs::remove_file(path);
    }
}
