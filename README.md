# Cetus Rust Prototype

This repository contains a Rust prototype for a high-performance rendering pipeline that drives Headless Chrome and encodes video with ffmpeg. It focuses on efficient capture paths (JPEG/mjpeg), optional frame caching, and a small set of features to validate performance improvements over PNG-based pipelines.

Features
- Capture frames from Chrome using PNG or JPEG (mjpeg) and stream to ffmpeg (image2pipe) or write frames to disk and then encode.
- --frame-codec (png|mjpeg)
- --jpeg-quality (1-100)
- --frames-dir to write frames as frame-%09d.{png|jpg}
- --keep-frames to preserve cached frames
- Manifest written to frames-dir/manifest.json when --frames-dir is used

Building

Requirements
- Rust toolchain (rustup)
- Chrome or chrome-headless-shell (CHROME_PATH env or --chrome)
- ffmpeg (in PATH or set FFMPEG_PATH or use --ffmpeg)

Build

```bash
cargo build --release
```

Run

```bash
# stream-mode (image2pipe)
./target/release/cetus_rs_prototype ./examples/smoke.html -o out-mjpeg.mp4 --fps 30 --frame-codec=mjpeg --jpeg-quality=80

# frames-dir mode
./target/release/cetus_rs_prototype ./examples/smoke.html -o out-from-frames.mp4 --fps 30 --frame-codec=mjpeg --jpeg-quality=80 --frames-dir ./tmp-frames
```

Bench

See bench/run-bench.sh for a simple benchmark comparing png vs mjpeg paths.

Notes
- JPEG does not preserve alpha. Use png if your composition relies on transparency.
- headless_chrome crate and Chrome versions may need to be compatible; if Chrome fails to start, try chrome-headless-shell and set CHROME_PATH.
