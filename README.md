# Hanabi

Hanabi is a native Rust/egui video-editor shell backed by the selected Rust media engine from the supplied Stella project. The original C/Raylib frontend, its UI headers, icon pack, and unrelated assets are not included.

## What is included

- Native frameless Hanabi window with a custom title strip and Windows Acrylic backdrop (Windows 10 1809+; non-Windows platforms retain the translucent neutral surface).
- Neutral, white-tinted glass surfaces, softly outlined inspector sections, active-tab depth, short hover/expand animations, and no pink/blue gradient background.
- Transform controls and timeline clip/keyframe editing backed by the Rust timeline engine. Keyframes interpolate Transform, Light, Color, Detail and Video Effects parameters; enable switches use step keyframes.
- Adjust sections for Light, Color, Detail, and Video Effects.
- WGPU video/image compositor and shared preview/export uniforms, FFmpeg in-process decoding, background media inspection, and MP4 export/encoding.
- Chromatic aberration, sine-wave displacement, swirl, blur, and pixelation controls. Sine wave and swirl are WGPU shader effects; the same uniforms and shader are used by preview and export.
- FFmpeg audio filtering/mixing during final export when the timeline contains audio tracks.

## Build on Windows

The in-process decoder uses static FFmpeg decode libraries and `ffmpeg-next` binding generation. The Windows GitHub Actions workflow installs the FFmpeg SDK and LLVM/libclang, builds Hanabi, and packages the FFmpeg CLI tools required for media inspection, waveform generation, and export.

1. Upload the extracted project contents to the root of your GitHub repository (keep the `.github/workflows` folder).
2. Open **Actions → Build Hanabi for Windows**.
3. Download the `Hanabi-Windows-x64` artifact from a successful run.
4. Extract it and launch `hanabi.exe`. Keep the `ffmpeg` folder next to the executable.

For local builds, install the same static FFmpeg SDK and set `FFMPEG_DIR` and `LIBCLANG_PATH` before running `cargo build --release --package hanabi`.

## Current integration notes

- Video and still-image preview decoding is performed by the in-process FFmpeg decoder; the preview is composited by WGPU on a worker thread. Export uses that same visual renderer and FFmpeg for encoding/muxing.
- The reference backend's Raylib audio-device playback layer was part of its C frontend and was intentionally not copied. Video preview playback and frame stepping are wired to the decoder/renderer. Timeline audio is mixed into export, but live audio monitoring during timeline playback is not wired into this egui frontend yet.
- The Windows build is the supported packaged target. Acrylic blur is Windows-specific; other operating systems show the same neutral tinted-glass UI without native backdrop blur.
- This package preserves the supplied backend's GPL-3.0-or-later licensing and includes the original license file. The FFmpeg decode SDK and bundled command-line build have separate license notices.

## Project layout

```text
Hanabi/
├── Cargo.toml
├── main.rs
├── engine.rs
├── Engine/
│   ├── timeline-core/
│   └── backend-ffi/
├── .github/workflows/build.yml
├── LICENSE
└── README.md
```
