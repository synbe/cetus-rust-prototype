use anyhow::{Context, Result};
use clap::Parser;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
struct Args {
    /// Input HTML file (composition)
    input: PathBuf,
    /// Output file (e.g., out.mp4)
    #[clap(short, long)]
    output: PathBuf,
    /// Frames per second
    #[clap(long, default_value = "30")]
    fps: usize,
    /// Optional path to Chrome executable (env CHROME_PATH overrides)
    #[clap(long)]
    chrome: Option<PathBuf>,
    /// Optional path to ffmpeg executable (env FFMPEG_PATH overrides)
    #[clap(long)]
    ffmpeg: Option<PathBuf>,
    /// Frame codec to capture: png (default) or mjpeg
    #[clap(long, default_value = "png")]
    frame_codec: String,
    /// JPEG quality when using --frame-codec=mjpeg (1-100)
    #[clap(long, default_value = "80")]
    jpeg_quality: u8,
    /// Optional frames directory to write cached frames (if set, frames are written and then encoded)
    #[clap(long)]
    frames_dir: Option<PathBuf>,
    /// Keep frames after encoding (only relevant when --frames-dir is set)
    #[clap(long)]
    keep_frames: bool,
}

#[derive(Deserialize, Debug)]
struct Composition {
    id: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<f64>,
    fps: Option<u32>,
}

fn build_seek_script(frame: usize, fps: usize) -> String {
    let t = (frame as f64) / (fps as f64);
    format!(
r#"(async function() {{
  window.__cetusTime = {time};
  window.__cetusFrame = {frame};
  window.__cetusFPS = {fps};
  if (typeof window.__cetusSeek === 'function') {{
    try {{ await window.__cetusSeek({time}, {{frame:{frame}, fps:{fps}}}); }} catch(_){{}} 
  }}
  if (typeof window.__cetusRenderFrame === 'function') {{
    try {{ await window.__cetusRenderFrame({time}, {{frame:{frame}, fps:{fps}}}); }} catch(_){{}}
  }}
  if (document.fonts && document.fonts.ready) {{
    try {{ await Promise.race([document.fonts.ready, new Promise(r => setTimeout(r, 2000))]); }} catch(_){{}}
  }}
  // small wait to allow animations to apply
  await new Promise(r => setTimeout(r, 20));
  return true;
}})();"#,
        time = t,
        frame = frame,
        fps = fps
    )
}

fn parse_composition_from_page(tab: &headless_chrome::Tab) -> Result<Composition> {
    // read dataset from #root if exists, fallback to attributes on body
    let script = r#"(function() {
      const el = document.getElementById('root') || document.querySelector('[data-composition-id]') || document.body;
      const d = el.dataset || {};
      return {
        id: d.compositionId || el.getAttribute('data-composition-id') || null,
        width: d.width ? Number(d.width) : Number(el.getAttribute('data-width')) || null,
        height: d.height ? Number(d.height) : Number(el.getAttribute('data-height')) || null,
        duration: d.duration ? Number(d.duration) : Number(el.getAttribute('data-duration')) || null,
        fps: d.fps ? Number(d.fps) : Number(el.getAttribute('data-fps')) || null
      };
    })();"#;
    let v = tab
        .evaluate(script, false)
        .context("evaluate composition script")?
        .value
        .context("no value from composition script")?;
    let comp: Composition = serde_json::from_value(v).context("parse composition JSON")?;
    Ok(comp)
}

fn write_frame_file(dir: &Path, frame_idx: usize, ext: &str, data: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(dir).context("create frames dir")?;
    let filename = format!("frame-{:09}.{}", frame_idx, ext);
    let path = dir.join(&filename);
    let tmp = dir.join(format!("{}.tmp", filename));
    fs::write(&tmp, data).with_context(|| format!("write temp frame {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename temp frame {} -> {}", tmp.display(), path.display()))?;
    Ok(path)
}

fn encode_frames_with_ffmpeg(ffmpeg_path: &str, frames_dir: &Path, ext: &str, fps: usize, output: &Path) -> Result<()> {
    // build ffmpeg args: -y -framerate {fps} -i {frames_dir}/frame-%09d.{ext} -c:v libx264 -pix_fmt yuv420p output
    let pattern = format!("{}/frame-%09d.{}", frames_dir.to_string_lossy(), ext);
    let args = [
        "-y",
        "-framerate",
        &fps.to_string(),
        "-i",
        &pattern,
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        output.to_string_lossy().as_ref(),
    ];
    let status = Command::new(ffmpeg_path)
        .args(&args)
        .stderr(Stdio::inherit())
        .status()
        .context("spawn ffmpeg for frame dir encoding")?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with {}", status);
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let chrome_path = args
        .chrome
        .or_else(|| std::env::var_os("CHROME_PATH").map(PathBuf::from))
        .map(|p| p.to_string_lossy().to_string());

    let ffmpeg_path = args
        .ffmpeg
        .or_else(|| std::env::var_os("FFMPEG_PATH").map(PathBuf::from))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "ffmpeg".to_string());

    // validate frame codec
    let frame_codec = args.frame_codec.to_lowercase();
    if frame_codec != "png" && frame_codec != "mjpeg" {
        anyhow::bail!("unsupported frame codec '{}': only 'png' and 'mjpeg' are supported in this prototype", frame_codec);
    }
    let jpeg_quality = args.jpeg_quality.clamp(1, 100) as i64;

    // Launch browser
    let mut launch_opts = LaunchOptionsBuilder::default();
    launch_opts = launch_opts.headless(true);
    if let Some(ref p) = chrome_path {
        launch_opts = launch_opts.path(Some(std::path::PathBuf::from(p)));
    }
    let browser = Browser::new(launch_opts.build().context("build chrome launch options")?)
        .context("launch chrome")?;
    let tab = browser.new_tab().context("create tab")?;

    // Navigate to file://... input
    let input_url = format!("file://{}", args.input.canonicalize()?.to_string_lossy());
    tab.navigate_to(&input_url)
        .context("navigate to input")?;
    tab.wait_until_navigated().context("wait navigate")?;

    // parse composition
    let comp = parse_composition_from_page(&tab).context("read composition")?;
    let width = comp.width.unwrap_or(1280);
    let height = comp.height.unwrap_or(720);
    let fps = comp.fps.unwrap_or(args.fps as u32);
    let duration = comp.duration.unwrap_or(5.0);
    let total_frames = (duration * (fps as f64)).round() as usize;

    eprintln!("Composition: width={} height={} fps={} duration={} frames={}",
        width, height, fps, duration, total_frames);

    // decide on extension for cached frames
    let ext = if frame_codec == "mjpeg" { "jpg" } else { "png" };

    // If frames_dir provided, write frames to disk then invoke ffmpeg on them.
    if let Some(frames_dir) = args.frames_dir.as_ref() {
        eprintln!("Writing frames to {} ...", frames_dir.display());
        for frame in 0..total_frames {
            let script = build_seek_script(frame, fps as usize);
            let _ = tab.evaluate(&script, true).context("evaluate seek script")?;
            thread::sleep(Duration::from_millis(5));

            let img_data = if frame_codec == "mjpeg" {
                tab.capture_screenshot(
                    headless_chrome::protocol::page::ScreenshotFormat::Jpeg,
                    Some(jpeg_quality),
                    Some((width, height)),
                )
                .context("capture jpeg screenshot")?
            } else {
                tab.capture_screenshot(
                    headless_chrome::protocol::page::ScreenshotFormat::Png,
                    None,
                    Some((width, height)),
                )
                .context("capture png screenshot")?
            };

            write_frame_file(frames_dir, frame, ext, &img_data)?;
            eprintln!("Saved frame {}/{}", frame + 1, total_frames);
        }

        // encode frames to output via ffmpeg
        encode_frames_with_ffmpeg(&ffmpeg_path, frames_dir, ext, fps as usize, &args.output)?;

        if !args.keep_frames {
            let _ = fs::remove_dir_all(frames_dir);
        }

        eprintln!("Rendered {} from frames in {}", args.output.display(), frames_dir.display());
        return Ok(());
    }

    // spawn ffmpeg with correct input codec for image2pipe
    let mut ffmpeg_args = vec!["-y".to_string()];
    ffmpeg_args.push("-f".to_string());
    ffmpeg_args.push("image2pipe".to_string());
    if frame_codec == "mjpeg" {
        ffmpeg_args.push("-vcodec".to_string());
        ffmpeg_args.push("mjpeg".to_string());
    } else {
        ffmpeg_args.push("-vcodec".to_string());
        ffmpeg_args.push("png".to_string());
    }
    ffmpeg_args.push("-r".to_string());
    ffmpeg_args.push(fps.to_string());
    ffmpeg_args.push("-i".to_string());
    ffmpeg_args.push("pipe:0".to_string());
    // rest of encoding args
    ffmpeg_args.push("-c:v".to_string());
    ffmpeg_args.push("libx264".to_string());
    ffmpeg_args.push("-pix_fmt".to_string());
    ffmpeg_args.push("yuv420p".to_string());
    ffmpeg_args.push("-movflags".to_string());
    ffmpeg_args.push("+faststart".to_string());
    ffmpeg_args.push(args.output.to_string_lossy().to_string());

    let mut ffmpeg = Command::new(&ffmpeg_path)
        .args(ffmpeg_args.iter())
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn ffmpeg")?;

    let mut stdin = ffmpeg.stdin.take().context("open ffmpeg stdin")?;

    for frame in 0..total_frames {
        let script = build_seek_script(frame, fps as usize);
        // evaluate and wait
        let _ = tab.evaluate(&script, true).context("evaluate seek script")?;
        // tiny delay to allow layout settle (may be unnecessary)
        thread::sleep(Duration::from_millis(5));

        // capture screenshot in requested format
        let img_data = if frame_codec == "mjpeg" {
            tab.capture_screenshot(
                headless_chrome::protocol::page::ScreenshotFormat::Jpeg,
                Some(jpeg_quality),
                Some((width, height)),
            )
            .context("capture jpeg screenshot")?
        } else {
            tab.capture_screenshot(
                headless_chrome::protocol::page::ScreenshotFormat::Png,
                None,
                Some((width, height)),
            )
            .context("capture png screenshot")?
        };

        // write to ffmpeg stdin
        stdin.write_all(&img_data).context("write frame to ffmpeg")?;
        eprintln!("Wrote frame {}/{}", frame + 1, total_frames);
    }

    // close stdin and wait
    drop(stdin);
    let status = ffmpeg.wait().context("wait ffmpeg")?;
    if !status.success() {
        anyhow::bail!("ffmpeg exited with {}", status);
    }
    eprintln!("Rendered {}", args.output.display());
    Ok(())
}
