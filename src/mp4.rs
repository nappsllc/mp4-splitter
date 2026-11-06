//! Lossless MP4 cutting (adapted from Rustshot's trim, Apache-2.0,
//! https://github.com/nappsllc/rustshot `src/rec/cut/mp4.rs`).
//!
//! Trim an MP4 without re-encoding: the samples from the key frame at
//! or before the new start up to the last one (in decode order) shown
//! before the new end are copied into a new file (a cut in decode order
//! loses no reference), and an edit list makes it start and end exactly
//! where asked (players show the presentation the edit list describes;
//! the frames before the start are decoded but not shown).
//! Sound (every sample a sync sample) starts as early as the kept video
//! does (to within a packet), and its edit list skips as much: players
//! that ignore edit lists (Windows' own) keep picture and sound in step.
//!
//! The file must be a plain (not fragmented) ISO BMFF: `ftyp`, `moov`,
//! `mdat` in any order, as Media Foundation, our muxer, ffmpeg and
//! AVAssetWriter write them. The new file is `ftyp`, `moov` (with `co64`
//! offsets, so its size does not depend on them), then one `mdat` with the
//! samples in their original order. Boxes the trim does not touch (the
//! sample descriptions, handlers, user data) are copied as they are;
//! per-sample tables the trim cannot cut (sample groups, `sdtp`) are
//! dropped.

use anyhow::{Context, Result, anyhow, bail, ensure};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

/// A file with sound only: sound kept before the start for the decoder to
/// settle (80 ms, as Opus asks; plenty for AAC's one frame).
pub const AUDIO_PREROLL: Duration = Duration::from_millis(80);

// --- reading ------------------------------------------------------------------

fn be16(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes([s[0], s[1]])).ok_or_else(|| anyhow!("box too short"))
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| anyhow!("box too short"))
}

fn be64(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8).map(|s| u64::from_be_bytes(s.try_into().unwrap_or_default())).ok_or_else(|| anyhow!("box too short"))
}

/// A box in memory: its type and its body (after the header).
#[derive(Clone, Debug)]
pub(crate) struct Bx<'a> {
    pub(crate) kind: [u8; 4],
    pub(crate) body: &'a [u8],
    /// The whole box (header included), to copy as is.
    pub(crate) raw: &'a [u8],
}

/// The boxes in `data`.
pub(crate) fn children(data: &[u8]) -> Result<Vec<Bx<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let s32 = be32(data, at)? as usize;
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap_or_default();
        let (size, head) = match s32 {
            1 => (be64(data, at + 8)? as usize, 16),
            0 => (data.len() - at, 8),
            n => (n, 8),
        };
        ensure!(size >= head && at + size <= data.len(), "a broken {} box", String::from_utf8_lossy(&kind));
        out.push(Bx { kind, body: &data[at + head..at + size], raw: &data[at..at + size] });
        at += size;
    }
    Ok(out)
}

fn find<'a>(boxes: &[Bx<'a>], kind: &[u8; 4]) -> Option<Bx<'a>> {
    boxes.iter().find(|b| &b.kind == kind).cloned()
}

fn need<'a>(boxes: &[Bx<'a>], kind: &[u8; 4]) -> Result<Bx<'a>> {
    find(boxes, kind).ok_or_else(|| anyhow!("no {} box", String::from_utf8_lossy(kind)))
}

/// A top-level box of the file: where it is.
#[derive(Clone, Copy, Debug)]
struct Top {
    kind: [u8; 4],
    at: u64,
    size: u64,
    head: u64,
}

fn top_boxes<F: Read + Seek>(f: &mut F) -> Result<Vec<Top>> {
    let len = f.seek(SeekFrom::End(0))?;
    let mut out = Vec::new();
    let mut at = 0u64;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let s32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind: [u8; 4] = [h[4], h[5], h[6], h[7]];
        let (size, head) = match s32 {
            1 => {
                f.read_exact(&mut h[8..16])?;
                (u64::from_be_bytes(h[8..16].try_into().unwrap_or_default()), 16)
            }
            0 => (len - at, 8),
            n => (n, 8),
        };
        ensure!(size >= head && at + size <= len, "the file is cut short in a {} box", String::from_utf8_lossy(&kind));
        out.push(Top { kind, at, size, head });
        at += size;
    }
    Ok(out)
}

/// An edit list's first media segment: leading empty time (movie
/// timescale) and the media time it starts at (track timescale).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub empty: u64,
    pub media_time: i64,
}

fn read_edit(elst: &[u8]) -> Result<Edit> {
    let v = elst.first().copied().unwrap_or(0);
    let n = be32(elst, 4)? as usize;
    let mut e = Edit::default();
    let w = if v == 1 { 20 } else { 12 };
    for i in 0..n {
        let at = 8 + i * w;
        let (dur, mt) = if v == 1 { (be64(elst, at)?, be64(elst, at + 8)? as i64) } else { (be32(elst, at)? as u64, be32(elst, at + 4)? as i32 as i64) };
        if mt == -1 {
            e.empty += dur;
        } else {
            e.media_time = mt;
            break;
        }
    }
    Ok(e)
}

/// One track's samples (decode order).
#[derive(Clone, Debug, Default)]
pub struct Samples {
    pub sizes: Vec<u32>,
    pub offsets: Vec<u64>,
    pub durations: Vec<u32>,
    /// Composition offsets (`None`: none), and the `ctts` version.
    pub ctts: Option<(Vec<i32>, u8)>,
    /// Sync samples (`None`: every sample is one).
    pub sync: Option<Vec<bool>>,
    /// Each sample's description index (1-based).
    pub desc: Vec<u32>,
}

impl Samples {
    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    pub fn is_sync(&self, i: usize) -> bool {
        self.sync.as_ref().is_none_or(|s| s[i])
    }

    /// Decode times.
    pub fn dts(&self) -> Vec<u64> {
        let mut t = 0u64;
        self.durations
            .iter()
            .map(|d| {
                let v = t;
                t += *d as u64;
                v
            })
            .collect()
    }

    /// Composition (presentation) time of sample `i`, given its decode time.
    pub fn cts(&self, i: usize, dts: u64) -> i64 {
        dts as i64 + self.ctts.as_ref().map_or(0, |(c, _)| c[i] as i64)
    }
}

/// What the trim needs of a track.
#[derive(Clone, Debug)]
pub(crate) struct Track<'a> {
    pub(crate) tkhd: Bx<'a>,
    pub(crate) mdhd: Bx<'a>,
    pub(crate) hdlr: Bx<'a>,
    /// The `minf` children before `stbl` (`vmhd`/`smhd`, `dinf`).
    pub(crate) minf_rest: Vec<Bx<'a>>,
    pub(crate) stsd: Bx<'a>,
    pub(crate) timescale: u32,
    pub(crate) video: bool,
    pub(crate) edit: Edit,
    pub(crate) s: Samples,
}

fn parse_samples(stbl: &[Bx]) -> Result<Samples> {
    let mut s = Samples::default();
    // Sizes.
    if let Some(stsz) = find(stbl, b"stsz") {
        let b = stsz.body;
        let (fixed, n) = (be32(b, 4)?, be32(b, 8)? as usize);
        s.sizes = if fixed != 0 { vec![fixed; n] } else { (0..n).map(|i| be32(b, 12 + i * 4)).collect::<Result<_>>()? };
    } else if let Some(stz2) = find(stbl, b"stz2") {
        let b = stz2.body;
        let (bits, n) = (b.get(7).copied().unwrap_or(0), be32(b, 8)? as usize);
        s.sizes = (0..n)
            .map(|i| match bits {
                4 => b.get(12 + i / 2).map(|v| if i % 2 == 0 { (v >> 4) as u32 } else { (v & 15) as u32 }).ok_or_else(|| anyhow!("stz2 too short")),
                8 => b.get(12 + i).map(|v| *v as u32).ok_or_else(|| anyhow!("stz2 too short")),
                16 => be16(b, 12 + i * 2).map(u32::from),
                _ => Err(anyhow!("bad stz2 field size {bits}")),
            })
            .collect::<Result<_>>()?;
    } else {
        bail!("no sample sizes");
    }
    let n = s.sizes.len();
    // Chunk offsets.
    let chunks: Vec<u64> = if let Some(co) = find(stbl, b"stco") {
        (0..be32(co.body, 4)? as usize).map(|i| be32(co.body, 8 + i * 4).map(u64::from)).collect::<Result<_>>()?
    } else {
        let co = need(stbl, b"co64")?;
        (0..be32(co.body, 4)? as usize).map(|i| be64(co.body, 8 + i * 8)).collect::<Result<_>>()?
    };
    // Samples per chunk.
    let stsc = need(stbl, b"stsc")?.body;
    let runs: Vec<(u32, u32, u32)> =
        (0..be32(stsc, 4)? as usize).map(|i| Ok((be32(stsc, 8 + i * 12)?, be32(stsc, 12 + i * 12)?, be32(stsc, 16 + i * 12)?))).collect::<Result<_>>()?;
    s.offsets.reserve(n);
    s.desc.reserve(n);
    let mut k = 0usize;
    for (ci, &chunk_at) in chunks.iter().enumerate() {
        let c = ci as u32 + 1;
        let run = runs.iter().rev().find(|r| r.0 <= c).ok_or_else(|| anyhow!("bad stsc"))?;
        let mut at = chunk_at;
        for _ in 0..run.1 {
            if k >= n {
                break;
            }
            s.offsets.push(at);
            s.desc.push(run.2);
            at += s.sizes[k] as u64;
            k += 1;
        }
    }
    ensure!(k == n, "the chunks hold {k} of {n} samples");
    // Durations.
    let stts = need(stbl, b"stts")?.body;
    for i in 0..be32(stts, 4)? as usize {
        let (cnt, d) = (be32(stts, 8 + i * 8)?, be32(stts, 12 + i * 8)?);
        s.durations.extend(std::iter::repeat_n(d, cnt as usize));
    }
    s.durations.resize(n, s.durations.last().copied().unwrap_or(0));
    // Composition offsets.
    if let Some(ctts) = find(stbl, b"ctts") {
        let b = ctts.body;
        let mut c = Vec::with_capacity(n);
        for i in 0..be32(b, 4)? as usize {
            let (cnt, off) = (be32(b, 8 + i * 8)?, be32(b, 12 + i * 8)? as i32);
            c.extend(std::iter::repeat_n(off, cnt as usize));
        }
        c.resize(n, 0);
        s.ctts = Some((c, b[0]));
    }
    // Sync samples.
    if let Some(stss) = find(stbl, b"stss") {
        let b = stss.body;
        let mut sync = vec![false; n];
        for i in 0..be32(b, 4)? as usize {
            let k = be32(b, 8 + i * 4)? as usize;
            if (1..=n).contains(&k) {
                sync[k - 1] = true;
            }
        }
        s.sync = Some(sync);
    }
    Ok(s)
}

fn parse_track<'a>(trak: Bx<'a>) -> Result<Track<'a>> {
    let kids = children(trak.body)?;
    let tkhd = need(&kids, b"tkhd")?;
    let mdia = children(need(&kids, b"mdia")?.body)?;
    let mdhd = need(&mdia, b"mdhd")?;
    let hdlr = need(&mdia, b"hdlr")?;
    let minf_all = children(need(&mdia, b"minf")?.body)?;
    let stbl = children(need(&minf_all, b"stbl")?.body)?;
    let stsd = need(&stbl, b"stsd")?;
    let minf_rest = minf_all.into_iter().filter(|b| &b.kind != b"stbl").collect();
    let timescale = if mdhd.body.first() == Some(&1) { be32(mdhd.body, 20)? } else { be32(mdhd.body, 12)? };
    ensure!(timescale > 0, "a track without a time scale");
    let video = hdlr.body.get(8..12) == Some(b"vide");
    let edit = match find(&kids, b"edts").map(|e| children(e.body)).transpose()?.and_then(|e| find(&e, b"elst")) {
        Some(elst) => read_edit(elst.body)?,
        None => Edit::default(),
    };
    Ok(Track { tkhd, mdhd, hdlr, minf_rest, stsd, timescale, video, edit, s: parse_samples(&stbl)? })
}

// --- the plan -----------------------------------------------------------------

/// The samples of a track to keep, and its new edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keep {
    /// Samples `first..last` (decode order).
    pub first: usize,
    pub last: usize,
    /// Leading empty time (movie timescale), the media time the shown part
    /// starts at in the new track (track timescale), and how long it
    /// shows (movie timescale).
    pub empty: u64,
    pub media_time: u64,
    pub shown: u64,
}

fn ticks(d: Duration, scale: u32) -> u64 {
    (d.as_nanos() * scale as u128 / 1_000_000_000) as u64
}

/// Which samples of a track to keep for `start..end` (presentation
/// time), the track's `edit` and timescale given, the movie's timescale
/// `movie`. Video starts at the last key frame shown at or before the
/// start and ends with the last frame (decode order) shown before the
/// end; sound starts
/// `preroll` before the start.
#[allow(clippy::too_many_arguments)]
pub fn plan(s: &Samples, edit: Edit, timescale: u32, movie: u32, video: bool, start: Duration, end: Duration, preroll: Duration) -> Option<Keep> {
    let n = s.len();
    if n == 0 || end <= start {
        return None;
    }
    let dts = s.dts();
    // Presentation p → media time: edit.media_time + (p - empty).
    let empty_d = Duration::from_nanos((edit.empty as u128 * 1_000_000_000 / movie.max(1) as u128) as u64);
    let from = start.max(empty_d) - empty_d; // into the media part
    let to = end.saturating_sub(empty_d);
    if to.is_zero() {
        return None;
    }
    let m0 = edit.media_time.max(0) as u64;
    let (ms, me) = (m0 + ticks(from, timescale), m0 + ticks(to, timescale));
    let (first, last) = if video {
        let first = (0..n).rev().find(|&i| s.is_sync(i) && s.cts(i, dts[i]) <= ms as i64).or_else(|| (0..n).find(|&i| s.is_sync(i)))?;
        // Up to the last frame (decode order) shown before the end: a cut
        // in decode order loses no reference, and a player that ignores
        // the edit list's length still stops there.
        let last = (first..n).rev().find(|&i| s.cts(i, dts[i]) < me as i64).map_or(first + 1, |i| i + 1).max(first + 1);
        (first, last)
    } else {
        let back = ms.saturating_sub(ticks(preroll, timescale));
        let first = (0..n).rev().find(|&i| dts[i] <= back).unwrap_or(0);
        let last = (first..n).find(|&i| dts[i] >= me).unwrap_or(n).max(first + 1);
        (first, last)
    };
    let base = dts[first];
    let empty = ticks(empty_d.saturating_sub(start), movie);
    let media_time = ms.saturating_sub(base);
    let shown = ticks(end - start.max(empty_d), movie);
    Some(Keep { first, last, empty, media_time, shown })
}

// --- writing ------------------------------------------------------------------

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    put32(&mut b, (body.len() + 8) as u32);
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, 0, 0, 0];
    b.extend_from_slice(body);
    boxed(kind, &b)
}

/// A copy of a header box's body with its duration set (versions 0 and 1:
/// `mvhd`, `mdhd` at 16 / 24, `tkhd` at 20 / 28).
fn with_duration(body: &[u8], at0: usize, at1: usize, d: u64) -> Vec<u8> {
    let mut b = body.to_vec();
    if b.first() == Some(&1) {
        if b.len() >= at1 + 8 {
            b[at1..at1 + 8].copy_from_slice(&d.to_be_bytes());
        }
    } else if b.len() >= at0 + 4 {
        b[at0..at0 + 4].copy_from_slice(&(d.min(u32::MAX as u64) as u32).to_be_bytes());
    }
    b
}

/// Run-length (count, value) pairs.
fn runs<T: PartialEq + Copy>(v: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for x in v {
        match out.last_mut() {
            Some((n, y)) if *y == x => *n += 1,
            _ => out.push((1, x)),
        }
    }
    out
}

/// The edit list box for these entries: how long (movie timescale) and the
/// media time shown from (track timescale; -1 for empty time).
fn elst(entries: &[(u64, i64)]) -> Vec<u8> {
    let wide = entries.iter().any(|&(d, mt)| d > u32::MAX as u64 || mt > i32::MAX as i64);
    let mut b = Vec::new();
    put32(&mut b, entries.len() as u32);
    for &(d, mt) in entries {
        if wide {
            put64(&mut b, d);
            put64(&mut b, mt as u64);
        } else {
            put32(&mut b, d as u32);
            put32(&mut b, mt as i32 as u32);
        }
        put32(&mut b, 0x0001_0000); // rate 1.0
    }
    boxed(b"edts", &full(b"elst", u8::from(wide), &b))
}

/// One sample of the new file: where its bytes are (which source, where)
/// and its table entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutSample {
    pub src: usize,
    pub off: u64,
    pub size: u32,
    pub dur: u32,
    /// Composition offset.
    pub cto: i32,
    pub sync: bool,
    pub desc: u32,
}

/// A track of the new file: the old one's boxes, the samples, its edit.
pub(crate) struct OutTrack<'t, 'a> {
    pub t: &'t Track<'a>,
    pub samples: Vec<OutSample>,
    /// Write `ctts` (with this version) / `stss`.
    pub ctts: Option<u8>,
    pub stss: bool,
    /// The edit list's entries (see [`elst`]).
    pub edit: Vec<(u64, i64)>,
    /// Sample descriptions to write instead of the old track's.
    pub stsd: Option<Vec<u8>>,
}

impl<'t, 'a> OutTrack<'t, 'a> {
    /// The samples `k` keeps of `t` (from source 0).
    pub(crate) fn kept(t: &'t Track<'a>, k: Keep) -> OutTrack<'t, 'a> {
        let s = &t.s;
        let samples = (k.first..k.last)
            .map(|i| OutSample {
                src: 0,
                off: s.offsets[i],
                size: s.sizes[i],
                dur: s.durations[i],
                cto: s.ctts.as_ref().map_or(0, |(c, _)| c[i]),
                sync: s.is_sync(i),
                desc: s.desc[i],
            })
            .collect();
        let mut edit = if k.empty > 0 { vec![(k.empty, -1)] } else { Vec::new() };
        edit.push((k.shown, k.media_time as i64));
        OutTrack { t, samples, ctts: s.ctts.as_ref().map(|c| c.1), stss: s.sync.is_some(), edit, stsd: None }
    }

    /// `pieces` of `t` one after another: what each keeps (`None`: nothing,
    /// the track is silent for that long) and how long it shows (movie
    /// timescale). A piece's samples never overlap the next one's in
    /// composition time (a sample's duration grows if they would), so each
    /// edit list entry shows only its own piece.
    pub(crate) fn joined(t: &'t Track<'a>, pieces: &[(Option<Keep>, u64)], movie: u32) -> OutTrack<'t, 'a> {
        let s = &t.s;
        let dts = s.dts();
        let (mut samples, mut edit) = (Vec::<OutSample>::new(), Vec::new());
        let mut base = 0i64; // where the next piece's first sample decodes
        let mut reach = i64::MIN; // the composition time the pieces so far need
        for &(k, len) in pieces {
            let Some(k) = k else {
                edit.push((len, -1));
                continue;
            };
            if k.empty > 0 {
                edit.push((k.empty, -1));
            }
            let d0 = dts[k.first] as i64;
            let cts = |i: usize| s.cts(i, dts[i]) - d0;
            let lo = (k.first..k.last).map(cts).min().unwrap_or(0);
            if let Some(last) = samples.last_mut() {
                let short = reach - lo - base;
                if short > 0 {
                    last.dur += short as u32;
                    base += short;
                }
            }
            edit.push((k.shown, base + k.media_time as i64));
            let hi = (k.first..k.last).map(|i| cts(i) + s.durations[i] as i64).max().unwrap_or(0);
            let shown = (k.shown as u128 * t.timescale as u128).div_ceil(movie.max(1) as u128) as i64;
            reach = base + hi.max(k.media_time as i64 + shown);
            base += s.durations[k.first..k.last].iter().map(|d| *d as i64).sum::<i64>();
            samples.extend(OutTrack::kept(t, k).samples);
        }
        while edit.last().is_some_and(|e| e.1 == -1) {
            edit.pop();
        }
        OutTrack { t, samples, ctts: s.ctts.as_ref().map(|c| c.1), stss: s.sync.is_some(), edit, stsd: None }
    }

    /// The length of its edit list (movie timescale).
    fn shown(&self) -> u64 {
        self.edit.iter().map(|e| e.0).sum()
    }
}

/// The new `trak` for `o`, its samples at `offsets`.
fn write_trak(o: &OutTrack, offsets: &[u64]) -> Vec<u8> {
    let t = o.t;
    let ss = &o.samples;
    let media_dur: u64 = ss.iter().map(|s| s.dur as u64).sum();
    let mut stbl = o.stsd.clone().unwrap_or_else(|| t.stsd.raw.to_vec());
    // stts
    let rl = runs(ss.iter().map(|s| s.dur));
    let mut b = Vec::new();
    put32(&mut b, rl.len() as u32);
    for (n, d) in rl {
        put32(&mut b, n);
        put32(&mut b, d);
    }
    stbl.extend(full(b"stts", 0, &b));
    // ctts
    if let Some(v) = o.ctts {
        let rl = runs(ss.iter().map(|s| s.cto));
        let mut b = Vec::new();
        put32(&mut b, rl.len() as u32);
        for (n, c) in rl {
            put32(&mut b, n);
            put32(&mut b, c as u32);
        }
        stbl.extend(full(b"ctts", v, &b));
    }
    // stss
    if o.stss {
        let idx: Vec<u32> = ss.iter().enumerate().filter(|(_, s)| s.sync).map(|(i, _)| i as u32 + 1).collect();
        let mut b = Vec::new();
        put32(&mut b, idx.len() as u32);
        for i in idx {
            put32(&mut b, i);
        }
        stbl.extend(full(b"stss", 0, &b));
    }
    // stsz
    let mut b = Vec::new();
    put32(&mut b, 0);
    put32(&mut b, ss.len() as u32);
    for s in ss {
        put32(&mut b, s.size);
    }
    stbl.extend(full(b"stsz", 0, &b));
    // stsc: one sample a chunk, a run per description.
    let mut entries: Vec<(u32, u32)> = Vec::new();
    for (j, s) in ss.iter().enumerate() {
        if entries.last().is_none_or(|e| e.1 != s.desc) {
            entries.push((j as u32 + 1, s.desc));
        }
    }
    let mut b = Vec::new();
    put32(&mut b, entries.len() as u32);
    for (first, d) in entries {
        put32(&mut b, first);
        put32(&mut b, 1);
        put32(&mut b, d);
    }
    stbl.extend(full(b"stsc", 0, &b));
    // co64
    let mut b = Vec::new();
    put32(&mut b, offsets.len() as u32);
    for o in offsets {
        put64(&mut b, *o);
    }
    stbl.extend(full(b"co64", 0, &b));
    let mut minf = Vec::new();
    for x in &t.minf_rest {
        minf.extend_from_slice(x.raw);
    }
    minf.extend(boxed(b"stbl", &stbl));
    let mut mdia = boxed(b"mdhd", &with_duration(t.mdhd.body, 16, 24, media_dur));
    mdia.extend_from_slice(t.hdlr.raw);
    mdia.extend(boxed(b"minf", &minf));
    let mut trak = boxed(b"tkhd", &with_duration(t.tkhd.body, 20, 28, o.shown()));
    trak.extend(elst(&o.edit));
    trak.extend(boxed(b"mdia", &mdia));
    boxed(b"trak", &trak)
}

/// A parsed MP4: its `ftyp` and `moov` (held), and the tracks in it.
pub(crate) struct Movie {
    ftyp: Option<Vec<u8>>,
    moov: Vec<u8>,
    head: usize,
}

impl Movie {
    pub fn read<R: Read + Seek>(src: &mut R) -> Result<Movie> {
        let tops = top_boxes(src)?;
        let read = |src: &mut R, t: Top| -> Result<Vec<u8>> {
            src.seek(SeekFrom::Start(t.at))?;
            let mut v = vec![0u8; t.size as usize];
            src.read_exact(&mut v)?;
            Ok(v)
        };
        let ftyp = tops.iter().find(|t| &t.kind == b"ftyp").copied().map(|t| read(src, t)).transpose()?;
        let at = tops.iter().find(|t| &t.kind == b"moov").copied().context("not an MP4 (no moov)")?;
        Ok(Movie { ftyp, moov: read(src, at)?, head: at.head as usize })
    }

    fn kids(&self) -> Result<Vec<Bx<'_>>> {
        let kids = children(&self.moov[self.head..])?;
        ensure!(find(&kids, b"mvex").is_none(), "a fragmented MP4 cannot be trimmed here");
        Ok(kids)
    }

    /// The movie timescale, and the tracks.
    pub fn tracks(&self) -> Result<(u32, Vec<Track<'_>>)> {
        let kids = self.kids()?;
        let mvhd = need(&kids, b"mvhd")?;
        let movie = if mvhd.body.first() == Some(&1) { be32(mvhd.body, 20)? } else { be32(mvhd.body, 12)? }.max(1);
        let tracks: Vec<Track> = kids.iter().filter(|b| &b.kind == b"trak").cloned().map(parse_track).collect::<Result<_>>()?;
        ensure!(!tracks.is_empty(), "no tracks");
        Ok((movie, tracks))
    }
}

/// Write a movie of `tracks` (from `m`'s boxes) whose samples are read
/// from `sources`; the bytes written.
pub(crate) fn write_movie<W: Write>(m: &Movie, tracks: &[OutTrack], sources: &mut [&mut dyn ReadSeek], dst: &mut W) -> Result<u64> {
    let kids = m.kids()?;
    let mvhd = need(&kids, b"mvhd")?;
    // The samples by decode time (the tracks interleaved, each in its own
    // order: Media Foundation reads a track's chunks only forwards), and
    // where each goes in mdat.
    let mut timed: Vec<(u128, usize, usize)> = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        let scale = t.t.timescale.max(1) as u128;
        let mut at = 0u128;
        for (i, s) in t.samples.iter().enumerate() {
            timed.push((at * 1_000_000_000 / scale, ti, i));
            at += s.dur as u128;
        }
    }
    timed.sort_unstable();
    let order: Vec<(usize, u64, usize, usize)> = timed
        .iter()
        .map(|&(_, ti, i)| {
            let s = &tracks[ti].samples[i];
            (s.src, s.off, ti, i)
        })
        .collect();
    let mut rel: Vec<Vec<u64>> = tracks.iter().map(|t| vec![0; t.samples.len()]).collect();
    let mut pos = 0u64;
    for &(_, _, ti, i) in &order {
        rel[ti][i] = pos;
        pos += tracks[ti].samples[i].size as u64;
    }
    let mdat_len = pos;
    let moov = |shift: u64| -> Vec<u8> {
        let shown = tracks.iter().map(OutTrack::shown).max().unwrap_or(0);
        let mut body = boxed(b"mvhd", &with_duration(mvhd.body, 16, 24, shown));
        for (ti, t) in tracks.iter().enumerate() {
            let offs: Vec<u64> = rel[ti].iter().map(|o| o + shift).collect();
            body.extend(write_trak(t, &offs));
        }
        for b in kids.iter().filter(|b| !matches!(&b.kind, b"mvhd" | b"trak")) {
            body.extend_from_slice(b.raw);
        }
        boxed(b"moov", &body)
    };
    let ftyp = m.ftyp.clone().unwrap_or_else(|| boxed(b"ftyp", b"isom\0\0\x02\0isomiso6mp41"));
    let data_at = ftyp.len() as u64 + moov(0).len() as u64 + 16;
    dst.write_all(&ftyp)?;
    dst.write_all(&moov(data_at))?;
    let mut head = Vec::new();
    put32(&mut head, 1);
    head.extend_from_slice(b"mdat");
    put64(&mut head, mdat_len + 16);
    dst.write_all(&head)?;
    // Copy the samples, reading runs of adjacent ones at once.
    let mut buf = Vec::new();
    let mut k = 0;
    let size = |e: &(usize, u64, usize, usize)| tracks[e.2].samples[e.3].size as u64;
    while k < order.len() {
        let (src, start_at) = (order[k].0, order[k].1);
        let mut end_at = start_at + size(&order[k]);
        let mut j = k + 1;
        while j < order.len() && order[j].0 == src && order[j].1 == end_at && end_at - start_at < (4 << 20) {
            end_at += size(&order[j]);
            j += 1;
        }
        buf.resize((end_at - start_at) as usize, 0);
        let r = sources.get_mut(src).context("a sample from no source")?;
        r.seek(SeekFrom::Start(start_at))?;
        r.read_exact(&mut buf).context("read the samples")?;
        dst.write_all(&buf)?;
        k = j;
    }
    dst.flush()?;
    Ok(data_at + mdat_len)
}

/// What [`write_movie`] reads samples from.
pub(crate) trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

/// How a trim went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trimmed {
    /// The video's kept samples (decode order) and how many come before
    /// the start (decoded, not shown).
    pub video_samples: usize,
    pub lead_in: usize,
    pub bytes: u64,
}

/// What each track keeps of `start..end`. The video starts at the key frame
/// before the start, `lead` early. The sound starts as early (to within a
/// packet): players that ignore edit lists (Windows' own) then keep picture
/// and sound in step, and the ones that honour them skip the same time in
/// both.
fn keeps_of(tracks: &[Track], movie: u32, start: Duration, end: Duration) -> Vec<Option<Keep>> {
    let lead = tracks.iter().find(|t| t.video).and_then(|t| {
        let k = plan(&t.s, t.edit, t.timescale, movie, true, start, end, Duration::ZERO)?;
        Some(Duration::from_nanos((k.media_time as u128 * 1_000_000_000 / t.timescale.max(1) as u128) as u64))
    });
    let preroll = lead.unwrap_or(AUDIO_PREROLL);
    tracks.iter().map(|t| plan(&t.s, t.edit, t.timescale, movie, t.video, start, end, if t.video { Duration::ZERO } else { preroll })).collect()
}

/// Copy `src`'s `ranges` (presentation time, in order and apart or touching)
/// into `dst` as one movie: each range keeps its lead-in from its key frame,
/// and an edit list entry per range shows only what was asked. Ranges that
/// touch are one range. The bytes written.
pub fn join<R: Read + Seek, W: Write>(src: &mut R, dst: &mut W, ranges: &[(Duration, Duration)]) -> Result<u64> {
    let mut merged: Vec<(Duration, Duration)> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges {
        ensure!(b > a, "a piece is empty");
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    ensure!(!merged.is_empty(), "nothing to keep");
    let m = Movie::read(src)?;
    let (movie, tracks) = m.tracks()?;
    let keeps: Vec<Vec<Option<Keep>>> = merged.iter().map(|&(a, b)| keeps_of(&tracks, movie, a, b)).collect();
    let outs: Vec<OutTrack> = tracks
        .iter()
        .enumerate()
        .filter(|(ti, _)| keeps.iter().any(|k| k[*ti].is_some()))
        .map(|(ti, t)| {
            let pieces: Vec<(Option<Keep>, u64)> = keeps.iter().zip(&merged).map(|(k, &(a, b))| (k[ti], ticks(b - a, movie))).collect();
            OutTrack::joined(t, &pieces, movie)
        })
        .collect();
    ensure!(!outs.is_empty(), "nothing to keep in those ranges");
    write_movie(&m, &outs, &mut [src], dst)
}

/// Copy `src`'s `start..end` (presentation time) into `dst`.
pub fn trim<R: Read + Seek, W: Write>(src: &mut R, dst: &mut W, start: Duration, end: Duration) -> Result<Trimmed> {
    ensure!(end > start, "the trim is empty");
    let m = Movie::read(src)?;
    let (movie, tracks) = m.tracks()?;
    let keeps = keeps_of(&tracks, movie, start, end);
    ensure!(keeps.iter().any(Option::is_some), "nothing to keep in that range");
    let outs: Vec<OutTrack> = tracks.iter().zip(&keeps).filter_map(|(t, k)| k.map(|k| OutTrack::kept(t, k))).collect();
    let bytes = write_movie(&m, &outs, &mut [src], dst)?;
    let (video_samples, lead_in) = tracks
        .iter()
        .zip(&keeps)
        .find(|(t, _)| t.video)
        .and_then(|(t, k)| {
            let k = (*k)?;
            let dts = t.s.dts();
            let ms = dts[k.first] + k.media_time;
            Some((k.last - k.first, (k.first..k.last).filter(|&i| t.s.cts(i, dts[i]) < ms as i64).count()))
        })
        .unwrap_or((0, 0));
    Ok(Trimmed { video_samples, lead_in, bytes })
}

// --- what the splitter needs to know ------------------------------------------

/// What the splitter shows and steps through.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub duration: Duration,
    /// The picture size (the first video track's `tkhd`).
    pub size: Option<(u32, u32)>,
    /// Every video frame's presentation time, sorted.
    pub frames: Vec<Duration>,
    /// The key frames' presentation times, sorted.
    pub keys: Vec<Duration>,
    pub has_audio: bool,
    /// The video's sample entry (`avc1`, `hvc1`, ...).
    pub video_codec: Option<String>,
    /// Where time 0 is in the video's and the sound's media timelines, for
    /// decoders that ignore edit lists.
    pub zero: Duration,
    pub audio_zero: Duration,
    /// The first picture's time in the video's media timeline: what shows how
    /// a decoder's clock relates to the file's.
    pub first: Duration,
    /// Quarter turns clockwise to show the picture upright.
    pub turn: u8,
}

impl Info {
    /// Frames a second (from the frame list; 30 without video).
    pub fn fps(&self) -> f64 {
        if self.frames.len() >= 2 {
            let span = (*self.frames.last().unwrap_or(&Duration::ZERO) - self.frames[0]).as_secs_f64();
            if span > 0.0 {
                return (self.frames.len() - 1) as f64 / span;
            }
        }
        30.0
    }
}

fn to_dur(ticks: i64, scale: u32) -> Duration {
    Duration::from_nanos((ticks.max(0) as u128 * 1_000_000_000 / scale.max(1) as u128) as u64)
}

/// Read what [`Info`] holds from `src` (only `moov` is read).
pub fn info<R: Read + Seek>(src: &mut R) -> Result<Info> {
    let m = Movie::read(src)?;
    let (movie, tracks) = m.tracks()?;
    let kids = m.kids()?;
    let mvhd = need(&kids, b"mvhd")?;
    let d = if mvhd.body.first() == Some(&1) { be64(mvhd.body, 24)? } else { be32(mvhd.body, 16)? as u64 };
    let mut out = Info { duration: to_dur(d as i64, movie), ..Info::default() };
    out.has_audio = tracks.iter().any(|t| t.hdlr.body.get(8..12) == Some(b"soun"));
    if let Some(t) = tracks.iter().find(|t| t.video) {
        let b = t.tkhd.body;
        let off = if b.first() == Some(&1) { 88 } else { 76 };
        if let (Ok(w), Ok(h)) = (be32(b, off), be32(b, off + 4)) {
            let (w, h) = (w >> 16, h >> 16);
            // A quarter turn in the matrix (phones): shown the other way up.
            let m = off - 36;
            let turned = matches!((be32(b, m), be32(b, m + 16)), (Ok(0), Ok(0)));
            let (w, h) = if turned { (h, w) } else { (w, h) };
            out.size = (w > 0 && h > 0).then_some((w, h));
            out.turn = match (be32(b, m).unwrap_or(0) as i32, be32(b, m + 4).unwrap_or(0) as i32) {
                (0, v) if v > 0 => 1,
                (0, v) if v < 0 => 3,
                (v, _) if v < 0 => 2,
                _ => 0,
            };
        }
        out.video_codec = t.stsd.body.get(12..16).map(|c| String::from_utf8_lossy(c).into_owned());
        let dts = t.s.dts();
        let m0 = t.edit.media_time.max(0);
        let empty = to_dur(t.edit.empty as i64, movie);
        out.zero = to_dur(m0, t.timescale).saturating_sub(empty);
        out.first = to_dur((0..t.s.len()).map(|i| t.s.cts(i, dts[i])).min().unwrap_or(0), t.timescale);
        let at = |i: usize| empty + to_dur(t.s.cts(i, dts[i]) - m0, t.timescale);
        // Frames before the edit's start are decoded, not shown.
        let mut frames: Vec<Duration> = (0..t.s.len()).filter(|&i| t.s.cts(i, dts[i]) >= m0).map(at).collect();
        frames.sort_unstable();
        frames.dedup();
        let mut keys: Vec<Duration> = (0..t.s.len()).filter(|&i| t.s.is_sync(i)).map(at).collect();
        keys.sort_unstable();
        keys.dedup();
        out.frames = frames;
        out.keys = keys;
    }
    if let Some(t) = tracks.iter().find(|t| t.hdlr.body.get(8..12) == Some(b"soun")) {
        out.audio_zero = to_dur(t.edit.media_time.max(0), t.timescale);
    }
    if out.duration.is_zero() {
        out.duration = out.frames.last().copied().unwrap_or_default();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! Cut real files made by ffmpeg (skipped when it is not installed).
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn ffmpeg() -> Option<PathBuf> {
        let f = crate::ffmpeg::find();
        if f.is_none() {
            eprintln!("SKIP: ffmpeg is not installed");
        }
        f
    }

    /// A 6 s 30 fps H.264 (key frame every second, B-frames) + AAC file.
    fn fixture(ff: &Path, name: &str, extra: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mp4s-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(name);
        let mut c = Command::new(ff);
        c.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30:duration=6",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=6",
        ]);
        c.args(["-c:v", "libx264", "-g", "30", "-bf", "2", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"]).args(extra).arg(&out);
        assert!(c.status().unwrap().success(), "ffmpeg made no fixture");
        out
    }

    fn info_of(p: &Path) -> Info {
        info(&mut std::io::BufReader::new(std::fs::File::open(p).unwrap())).unwrap()
    }

    fn cut(src: &Path, dst: &Path, a: f64, b: f64) {
        let mut r = std::io::BufReader::new(std::fs::File::open(src).unwrap());
        let mut w = std::io::BufWriter::new(std::fs::File::create(dst).unwrap());
        trim(&mut r, &mut w, Duration::from_secs_f64(a), Duration::from_secs_f64(b)).unwrap();
    }

    /// ffmpeg reads the whole file without a complaint.
    fn decodes_clean(ff: &Path, p: &Path) {
        let out = Command::new(ff).args(["-v", "error", "-i"]).arg(p).args(["-f", "null", "-"]).output().unwrap();
        assert!(out.status.success() && out.stderr.is_empty(), "{}: {}", p.display(), String::from_utf8_lossy(&out.stderr));
    }

    fn duration_by_ffprobe(ff: &Path, p: &Path) -> f64 {
        let probe = ff.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
        let out = Command::new(probe).args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(p).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    #[test]
    fn info_reads_frames_and_keys() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "info.mp4", &[]);
        let i = info_of(&src);
        assert_eq!(i.frames.len(), 180);
        assert_eq!(i.keys.len(), 6);
        assert_eq!(i.size, Some((320, 240)));
        assert!(i.has_audio);
        assert_eq!(i.video_codec.as_deref(), Some("avc1"));
        assert!((i.fps() - 30.0).abs() < 0.01);
        assert!((i.duration.as_secs_f64() - 6.0).abs() < 0.05);
    }

    #[test]
    fn pieces_are_exact_and_decode_cleanly() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "pieces.mp4", &[]);
        // On key frames, and between them (1.5 s is mid-GOP).
        for (k, (a, b)) in [(0.0, 2.0), (2.0, 4.0), (1.5, 4.25), (4.25, 6.0)].into_iter().enumerate() {
            let dst = src.with_file_name(format!("piece{k}.mp4"));
            cut(&src, &dst, a, b);
            decodes_clean(&ff, &dst);
            let got = duration_by_ffprobe(&ff, &dst);
            assert!((got - (b - a)).abs() < 0.035, "piece {a}..{b}: {got} s");
            let i = info_of(&dst);
            let want = ((b - a) * 30.0).round() as usize;
            assert!(i.frames.len().abs_diff(want) <= 1, "piece {a}..{b}: {} frames, want {want}", i.frames.len());
        }
    }

    #[test]
    fn a_mid_gop_piece_starts_on_the_right_picture() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "exact.mp4", &[]);
        let dst = src.with_file_name("exact-cut.mp4");
        let t = 2.0 + 13.0 / 30.0;
        cut(&src, &dst, t, 5.0);
        let want = crate::ffmpeg::picture_at(&ff, &src, Duration::from_secs_f64(t), 320, 240, false).unwrap();
        let got = crate::ffmpeg::picture_at(&ff, &dst, Duration::ZERO, 320, 240, false).unwrap();
        assert!(want.bgra == got.bgra, "the first picture of the piece is not the one at {t:.3} s");
    }

    /// Every picture ffmpeg shows of `p`, 16 x 12 and grey.
    fn frames_of(ff: &Path, p: &Path) -> Vec<Vec<u8>> {
        let out = Command::new(ff)
            .args(["-v", "error", "-i"])
            .arg(p)
            .args(["-map", "0:v", "-fps_mode", "passthrough", "-vf", "scale=16:12", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout.chunks(16 * 12).map(<[u8]>::to_vec).collect()
    }

    /// How long the sound of `p` lasts, decoded.
    fn sound_of(ff: &Path, p: &Path) -> f64 {
        let out = Command::new(ff).args(["-v", "error", "-i"]).arg(p).args(["-map", "0:a", "-f", "s16le", "-ac", "1", "-ar", "8000", "-"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout.len() as f64 / 16000.0
    }

    fn join_to(src: &Path, name: &str, ranges: &[(f64, f64)]) -> PathBuf {
        let dst = src.with_file_name(name);
        let mut r = std::io::BufReader::new(std::fs::File::open(src).unwrap());
        let mut w = std::io::BufWriter::new(std::fs::File::create(&dst).unwrap());
        let ranges: Vec<_> = ranges.iter().map(|&(a, b)| (Duration::from_secs_f64(a), Duration::from_secs_f64(b))).collect();
        join(&mut r, &mut w, &ranges).unwrap();
        dst
    }

    /// The joined file holds exactly the frames of `ranges` (given in frames
    /// at 30 fps), its sound as long as its picture, and it decodes without
    /// an error. A piece starting between key frames gets its first timestamp
    /// from ffmpeg's demuxer a few frames off (it counts decode time from the
    /// first frame shown), which its null muxer remarks on: only `on_keys`
    /// joins have to be silent.
    fn check_join(ff: &Path, src: &Path, name: &str, ranges: &[(usize, usize)], on_keys: bool) {
        let secs: Vec<(f64, f64)> = ranges.iter().map(|&(a, b)| (a as f64 / 30.0, b as f64 / 30.0)).collect();
        let dst = join_to(src, name, &secs);
        if on_keys {
            decodes_clean(ff, &dst);
        } else {
            let out = Command::new(ff).args(["-v", "error", "-i"]).arg(&dst).args(["-f", "null", "-"]).output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success() && err.lines().all(|l| l.contains("non monotonically increasing dts")), "{name}: {err}");
        }
        let want: f64 = secs.iter().map(|(a, b)| b - a).sum();
        let got = duration_by_ffprobe(ff, &dst);
        assert!((got - want).abs() < 0.04, "{name}: {got} s, want {want} s");
        let sound = sound_of(ff, &dst);
        assert!((sound - want).abs() < 0.1, "{name}: {sound} s of sound, want {want} s");
        let all = frames_of(ff, src);
        let expect: Vec<usize> = ranges.iter().flat_map(|&(a, b)| a..b).collect();
        let frames = frames_of(ff, &dst);
        assert_eq!(frames.len(), expect.len(), "{name}: frame count");
        for (k, (f, &i)) in frames.iter().zip(&expect).enumerate() {
            assert!(*f == all[i], "{name}: picture {k} is not frame {i} of the source");
        }
    }

    #[test]
    fn joined_pieces_show_exactly_the_kept_frames() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "join.mp4", &[]);
        // Mid-GOP cuts, on key frames, and to the very end.
        check_join(&ff, &src, "join-a.mp4", &[(15, 66), (102, 123), (150, 174)], false);
        check_join(&ff, &src, "join-b.mp4", &[(0, 30), (60, 90), (120, 180)], true);
    }

    #[test]
    fn joined_pieces_may_share_a_group_of_pictures() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "share.mp4", &[]);
        check_join(&ff, &src, "share-a.mp4", &[(32, 40), (45, 52), (55, 70)], false);
    }

    #[test]
    fn touching_pieces_become_one_edit() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "touch.mp4", &[]);
        let a = join_to(&src, "touch-a.mp4", &[(1.0, 2.0), (2.0, 3.0)]);
        let b = join_to(&src, "touch-b.mp4", &[(1.0, 3.0)]);
        assert!(std::fs::read(a).unwrap() == std::fs::read(b).unwrap());
    }

    #[test]
    fn one_joined_piece_is_a_trim() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "single.mp4", &[]);
        let a = join_to(&src, "single-a.mp4", &[(1.5, 4.25)]);
        let b = src.with_file_name("single-b.mp4");
        cut(&src, &b, 1.5, 4.25);
        assert!(std::fs::read(a).unwrap() == std::fs::read(b).unwrap());
    }

    #[test]
    fn rotation_and_moov_first_files_survive() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "fast.mp4", &["-movflags", "+faststart"]);
        let dst = src.with_file_name("fast-cut.mp4");
        cut(&src, &dst, 1.0, 3.0);
        decodes_clean(&ff, &dst);
        let rot = src.with_file_name("rot.mp4");
        let ok =
            Command::new(&ff).args(["-v", "error", "-y", "-display_rotation", "90", "-i"]).arg(&src).args(["-c", "copy"]).arg(&rot).status().unwrap().success();
        if ok {
            assert_eq!(info_of(&rot).size, Some((240, 320)));
            assert!(matches!(info_of(&rot).turn, 1 | 3));
            let rc = src.with_file_name("rot-cut.mp4");
            cut(&rot, &rc, 0.5, 2.5);
            assert_eq!(info_of(&rc).size, Some((240, 320)));
        }
    }

    #[test]
    fn fragmented_files_are_refused() {
        let Some(ff) = ffmpeg() else { return };
        let src = fixture(&ff, "frag.mp4", &["-movflags", "+frag_keyframe+empty_moov"]);
        let mut r = std::io::BufReader::new(std::fs::File::open(&src).unwrap());
        let err = trim(&mut r, &mut Vec::new(), Duration::from_secs(1), Duration::from_secs(2)).unwrap_err();
        assert!(format!("{err:#}").contains("fragmented"), "{err:#}");
    }
}
