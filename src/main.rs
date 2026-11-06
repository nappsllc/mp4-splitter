//! MP4 Splitter: cut a video into pieces without re-encoding.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod export;
mod ffmpeg;
mod mp4;
mod paint;
mod player;
mod split;
mod theme;

use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "MP4 Splitter — split MP4 videos losslessly

Usage:
  mp4-splitter [VIDEO]                     open the window
  mp4-splitter --split N VIDEO             split into N equal pieces, no window
  mp4-splitter --cut T1,T2,... VIDEO       cut at these times (seconds), no window
       [--start S] [--end E]               keep only S..E (seconds)
  mp4-splitter --info VIDEO                print what the file holds

Pieces are written next to the video as <name>_part01.mp4, ...";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("mp4-splitter {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| matches!(a.as_str(), "--split" | "--cut" | "--info" | "--start" | "--end")) {
        if let Err(e) = cli(&args) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    let open = args.iter().find(|a| !a.starts_with('-')).map(PathBuf::from);
    let event_loop = match winit::event_loop::EventLoop::<app::UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("error: can't open a window: {e}");
            std::process::exit(1);
        }
    };
    let mut app = app::App::new(event_loop.create_proxy(), open);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn secs(v: &str) -> anyhow::Result<Duration> {
    let x: f64 = v.trim().parse().map_err(|_| anyhow::anyhow!("not a time in seconds: {v}"))?;
    anyhow::ensure!(x >= 0.0 && x.is_finite(), "not a time in seconds: {v}");
    Ok(Duration::from_secs_f64(x))
}

fn cli(args: &[String]) -> anyhow::Result<()> {
    let mut it = args.iter();
    let (mut n, mut cuts, mut start, mut end, mut info, mut file) = (None, Vec::new(), None, None, false, None);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| anyhow::anyhow!("{a} needs a value"));
        match a.as_str() {
            "--split" => n = Some(val()?.parse::<usize>().map_err(|_| anyhow::anyhow!("--split needs a count"))?),
            "--cut" => {
                for t in val()?.split(',').filter(|t| !t.trim().is_empty()) {
                    cuts.push(secs(t)?);
                }
            }
            "--start" => start = Some(secs(val()?)?),
            "--end" => end = Some(secs(val()?)?),
            "--info" => info = true,
            f if !f.starts_with('-') => file = Some(PathBuf::from(f)),
            other => anyhow::bail!("unknown option {other}"),
        }
    }
    let file = file.ok_or_else(|| anyhow::anyhow!("no video given"))?;
    let i = mp4::info(&mut std::io::BufReader::new(std::fs::File::open(&file)?))?;
    if info {
        println!("duration: {:.3} s", i.duration.as_secs_f64());
        if let Some((w, h)) = i.size {
            println!("size: {w}x{h}");
        }
        println!("frames: {}  key frames: {}  fps: {:.3}", i.frames.len(), i.keys.len(), i.fps());
        println!("video: {}  sound: {}", i.video_codec.as_deref().unwrap_or("none"), if i.has_audio { "yes" } else { "no" });
        return Ok(());
    }
    let mut plan = split::Plan::new(i.duration);
    if let Some(s) = start {
        plan.mark_in(s);
    }
    if let Some(e) = end {
        plan.mark_out(e);
    }
    if let Some(n) = n {
        anyhow::ensure!(n >= 1, "--split needs at least 1");
        plan.split_equal(n, |t| t);
    }
    for c in cuts {
        anyhow::ensure!(plan.add_cut(c).is_some(), "can't cut at {:.3} s", c.as_secs_f64());
    }
    let pieces = plan.pieces();
    let dir = file.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let outs = split::output_paths(&file, &dir, pieces.len());
    for p in export::write_pieces(&file, &pieces, &outs, |_| {})? {
        println!("{}", p.display());
    }
    Ok(())
}
