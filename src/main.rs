//! MP4 Splitter: cut a video into pieces without re-encoding.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![allow(unknown_lints, clippy::collapsible_if, clippy::chunks_exact_to_as_chunks)]

mod app;
#[cfg(not(windows))]
mod audio;
#[cfg(windows)]
#[path = "audio_win.rs"]
mod audio;
mod deflate;
#[cfg(windows)]
mod dialogs;
mod export;
#[cfg(any(not(windows), test))]
#[cfg_attr(windows, allow(dead_code))]
mod ffmpeg;
#[allow(dead_code)]
mod fonts;
#[cfg(windows)]
mod mf;
mod mp4;
#[allow(dead_code)]
mod objects;
mod paint;
#[cfg(not(windows))]
mod player;
#[cfg(windows)]
#[path = "player_win.rs"]
mod player;
#[allow(dead_code)]
mod raster;
mod shell;
mod split;
mod theme;
#[allow(dead_code)]
mod uifb;
#[cfg(windows)]
#[path = "shell_win.rs"]
mod window;
#[cfg(not(windows))]
#[path = "shell_winit.rs"]
mod window;

use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "MP4 Splitter — split MP4 videos losslessly

Usage:
  mp4-splitter [VIDEO]                     open the window
  mp4-splitter --split N VIDEO             split into N equal pieces, no window
  mp4-splitter --cut T1,T2,... VIDEO       cut at these times (seconds), no window
       [--start S] [--end E]               keep only S..E (seconds)
       [--drop 2,4]                        leave out these pieces (from 1)
       [--join]                            join what is kept into one file
  mp4-splitter --info VIDEO                print what the file holds

Pieces are written next to the video as <name>_part01.mp4, ...; joined ones
as <name>_edited.mp4.";

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
    if args.iter().any(|a| matches!(a.as_str(), "--split" | "--cut" | "--info" | "--start" | "--end" | "--drop" | "--join")) {
        if let Err(e) = cli(&args) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    let open = args.iter().find(|a| !a.starts_with('-')).map(PathBuf::from);
    if let Err(e) = window::run(open) {
        eprintln!("error: can't open a window: {e}");
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
    let (mut drops, mut join) = (Vec::new(), false);
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
            "--drop" => {
                for k in val()?.split(',').filter(|k| !k.trim().is_empty()) {
                    drops.push(k.trim().parse::<usize>().map_err(|_| anyhow::anyhow!("--drop needs piece numbers: {k}"))?);
                }
            }
            "--join" => join = true,
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
    let total = plan.on.len();
    for k in drops {
        anyhow::ensure!((1..=total).contains(&k), "--drop {k}: there are {total} pieces");
        anyhow::ensure!(plan.set_kept(k - 1, false), "--drop would leave nothing");
    }
    let pieces = plan.kept_pieces();
    let dir = file.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if join {
        println!("{}", export::write_joined(&file, &pieces, &split::joined_path(&file, &dir))?.display());
        return Ok(());
    }
    let outs = split::output_paths(&file, &dir, pieces.len());
    for p in export::write_pieces(&file, &pieces, &outs, |_| {})? {
        println!("{}", p.display());
    }
    Ok(())
}
