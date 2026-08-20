use anyhow::{Context, Result};
use clap::Parser;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use serde::Deserialize;
use std::io::Write;
use std::path::PathBuf;
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

    // spawn ffmpeg
    let mut ffmpeg = Command::new(&ffmpeg_path)
        .args(&[
            "-y",
            "-f", "image2pipe",
            "-vcodec", "png",
            "-r", &fps.to_string(),
            "-i", "pipe:0",
            "-c:v", "libx264",
            "-pix_fmt", "yuv420p",
            "-movflags", "+faststart",
            args.output.to_str().unwrap(),
        ])
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
        // capture screenshot as PNG (full page false; clip size)
        let png_data = tab
            .capture_screenshot(headless_chrome::protocol::page::ScreenshotFormat::Png, None, Some((width, height)))
            .context("capture screenshot")?;
        // write to ffmpeg stdin
        stdin.write_all(&png_data).context("write frame to ffmpeg")?;
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
