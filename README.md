# FineShot

An ultra-lightweight native Windows screenshot and annotation utility written in Rust (`winit` + `softbuffer` + `tiny-skia` + `xcap`), designed for minimal CPU, GPU, and RAM usage.

## Performance
- **~1.18 MB Binary:** Standalone single-file executable with no external runtime dependencies.
- **Minimal Idle Footprint:** Lives quietly in the system tray and trims its working set when idle, with dynamically unloaded WinHTTP / Save-As subprocesses so shell and TLS libraries do not linger in RAM.
- **Instant Per-Monitor Capture:** Captures each monitor directly in native `0x00RRGGBB` layout with `present_with_damage` blitting and 16 ms frame pacing for smooth low-GPU region selection.

## Features
- **Instant Region & Full-Screen Capture:** Trigger anytime via configurable global hotkey (`PrintScreen` by default) or tray icon click, plus `--instant` CLI mode.
- **Annotation Tools:** Freehand Pen, Line, Arrow, Rectangle, and cache-linear O(W×H) Privacy Blur, with undo (`Ctrl+Z`).
- **Flexible Export:** Copy directly to clipboard (`Ctrl+C` / `Enter`, zero-copy fast path for full unannotated captures) or Save As across 8 image formats (`PNG`, `JPG`, `WEBP`, `BMP`, `TIFF`, `QOI`, `TGA`, `GIF`).
- **Windows Integration:** Native Settings window with hotkey rebinding, default export format selector, Windows login autostart toggle (`HKCU\...\Run`), and one-click GitHub auto-updates.

## Build & Run
Requires **Rust 1.85+** (Edition 2024).

```powershell
# Build optimized release binary (outputs to target\release\fineshot.exe)
cargo build --release

# Run unit tests
cargo test
```

## License
[MIT License](LICENSE)
