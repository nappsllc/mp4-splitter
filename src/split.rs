//! The split plan: what is kept (start..end), where it is cut in the
//! middle, and the pieces that come out. Pure: no files, no UI.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The shortest piece (and the closest two markers may be).
pub const MIN_PIECE: Duration = Duration::from_millis(100);

/// What can be dragged on the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Start,
    End,
    Cut(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub duration: Duration,
    pub start: Duration,
    pub end: Duration,
    /// Sorted, each strictly inside `start..end` and at least
    /// [`MIN_PIECE`] from its neighbours.
    pub cuts: Vec<Duration>,
}

impl Plan {
    pub fn new(duration: Duration) -> Plan {
        Plan { duration, start: Duration::ZERO, end: duration, cuts: Vec::new() }
    }

    /// The kept length.
    pub fn kept(&self) -> Duration {
        self.end - self.start
    }

    /// The pieces, in order.
    pub fn pieces(&self) -> Vec<(Duration, Duration)> {
        let mut edges = Vec::with_capacity(self.cuts.len() + 2);
        edges.push(self.start);
        edges.extend_from_slice(&self.cuts);
        edges.push(self.end);
        edges.windows(2).map(|w| (w[0], w[1])).collect()
    }

    /// Where a marker is.
    pub fn at(&self, m: Marker) -> Option<Duration> {
        match m {
            Marker::Start => Some(self.start),
            Marker::End => Some(self.end),
            Marker::Cut(i) => self.cuts.get(i).copied(),
        }
    }

    /// Every marker's time, sorted (start, the cuts, end).
    pub fn edges(&self) -> Vec<Duration> {
        let mut v = vec![self.start];
        v.extend_from_slice(&self.cuts);
        v.push(self.end);
        v
    }

    /// Move the start to `t` (kept before the end and the first cut); where
    /// it landed.
    pub fn set_start(&mut self, t: Duration) -> Duration {
        let limit = self.cuts.first().copied().unwrap_or(self.end).saturating_sub(MIN_PIECE);
        self.start = t.min(limit).min(self.duration);
        self.start
    }

    /// Move the end to `t`; where it landed.
    pub fn set_end(&mut self, t: Duration) -> Duration {
        let limit = self.cuts.last().copied().unwrap_or(self.start) + MIN_PIECE;
        self.end = t.max(limit).min(self.duration);
        self.end
    }

    /// Set the start at `t`, dropping the cuts it passes (the I key).
    pub fn mark_in(&mut self, t: Duration) {
        let t = t.min(self.end.saturating_sub(MIN_PIECE));
        self.cuts.retain(|c| *c >= t + MIN_PIECE);
        self.start = t;
    }

    /// Set the end at `t`, dropping the cuts it passes (the O key).
    pub fn mark_out(&mut self, t: Duration) {
        let t = t.max(self.start + MIN_PIECE).min(self.duration);
        self.cuts.retain(|c| *c + MIN_PIECE <= t);
        self.end = t;
    }

    /// Cut at `t`; its index, or `None` when `t` is outside the kept part or
    /// too close to another marker.
    pub fn add_cut(&mut self, t: Duration) -> Option<usize> {
        if t < self.start + MIN_PIECE || t + MIN_PIECE > self.end {
            return None;
        }
        if self.cuts.iter().any(|c| c.abs_diff(t) < MIN_PIECE) {
            return None;
        }
        let i = self.cuts.partition_point(|c| *c < t);
        self.cuts.insert(i, t);
        Some(i)
    }

    pub fn remove_cut(&mut self, i: usize) {
        if i < self.cuts.len() {
            self.cuts.remove(i);
        }
    }

    /// Move cut `i` to `t`, kept between its neighbours; where it landed.
    pub fn move_cut(&mut self, i: usize, t: Duration) -> Option<Duration> {
        let lo = if i == 0 { self.start } else { *self.cuts.get(i - 1)? } + MIN_PIECE;
        let hi = self.cuts.get(i + 1).copied().unwrap_or(self.end).saturating_sub(MIN_PIECE);
        if lo > hi {
            return self.cuts.get(i).copied();
        }
        let t = t.clamp(lo, hi);
        *self.cuts.get_mut(i)? = t;
        Some(t)
    }

    /// Move any marker to `t`; where it landed.
    pub fn move_marker(&mut self, m: Marker, t: Duration) -> Option<Duration> {
        match m {
            Marker::Start => Some(self.set_start(t)),
            Marker::End => Some(self.set_end(t)),
            Marker::Cut(i) => self.move_cut(i, t),
        }
    }

    /// Replace the cuts with ones that make `n` equal pieces of the kept
    /// part (each snapped by `snap`, which may move it to a key frame).
    pub fn split_equal(&mut self, n: usize, snap: impl Fn(Duration) -> Duration) {
        self.cuts.clear();
        let n = n.max(1) as u128;
        let (s, len) = (self.start.as_nanos(), self.kept().as_nanos());
        for k in 1..n {
            let t = Duration::from_nanos((s + len * k / n) as u64);
            self.add_cut(snap(t));
        }
    }

    /// The marker nearest `t` among the cuts, if any.
    pub fn nearest_cut(&self, t: Duration) -> Option<usize> {
        (0..self.cuts.len()).min_by_key(|&i| self.cuts[i].abs_diff(t))
    }
}

/// The next (`dir` > 0) or previous entry of the sorted `list` strictly
/// past `t`, by more than half a millisecond.
pub fn step(list: &[Duration], t: Duration, dir: i32) -> Option<Duration> {
    let eps = Duration::from_micros(500);
    if dir > 0 {
        let i = list.partition_point(|x| *x <= t + eps);
        list.get(i).copied()
    } else {
        let i = list.partition_point(|x| *x + eps < t);
        i.checked_sub(1).map(|i| list[i])
    }
}

/// The entry of the sorted `list` nearest `t`.
pub fn nearest(list: &[Duration], t: Duration) -> Option<Duration> {
    let i = list.partition_point(|x| *x < t);
    [i.checked_sub(1), Some(i)].into_iter().flatten().filter_map(|i| list.get(i).copied()).min_by_key(|x| x.abs_diff(t))
}

/// The frame shown at `t`: the last frame starting at or before it.
pub fn frame_index(frames: &[Duration], t: Duration) -> usize {
    frames.partition_point(|x| *x <= t + Duration::from_micros(500)).saturating_sub(1)
}

/// The files the pieces are written to, in `dir`: `<stem>_trimmed.<ext>`
/// for one piece, else `<stem>_part01.<ext>`, ...; names already taken
/// get ` (2)`, ` (3)`, ...
pub fn output_paths(src: &Path, dir: &Path, n: usize) -> Vec<PathBuf> {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
    let ext = src.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).filter(|e| !e.is_empty()).unwrap_or_else(|| "mp4".into());
    let width = n.to_string().len().max(2);
    (1..=n)
        .map(|k| {
            let base = if n == 1 { format!("{stem}_trimmed") } else { format!("{stem}_part{k:0width$}") };
            unique(dir, &base, &ext)
        })
        .collect()
}

fn unique(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let p = dir.join(format!("{base}.{ext}"));
    if !p.exists() {
        return p;
    }
    (2..).map(|k| dir.join(format!("{base} ({k}).{ext}"))).find(|p| !p.exists()).unwrap_or(p)
}

/// `m:ss.mmm` (or `h:mm:ss.mmm`).
pub fn timecode(t: Duration) -> String {
    let ms = t.as_millis();
    let (h, m, s, f) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{f:03}") } else { format!("{m}:{s:02}.{f:03}") }
}

/// A short time for the ruler: `m:ss`, or `m:ss.s` / `.ss` when the ticks
/// are that close.
pub fn ruler_label(t: Duration, tick: Duration) -> String {
    let ms = t.as_millis();
    let (m, s) = (ms / 60_000, ms / 1000 % 60);
    let h = m / 60;
    let head = if h > 0 { format!("{h}:{:02}:{s:02}", m % 60) } else { format!("{m}:{s:02}") };
    if tick >= Duration::from_secs(1) {
        head
    } else if tick >= Duration::from_millis(100) {
        format!("{head}.{}", ms % 1000 / 100)
    } else {
        format!("{head}.{:02}", ms % 1000 / 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: f64) -> Duration {
        Duration::from_secs_f64(x)
    }

    #[test]
    fn a_new_plan_is_one_piece() {
        let p = Plan::new(s(10.0));
        assert_eq!(p.pieces(), vec![(s(0.0), s(10.0))]);
    }

    #[test]
    fn split_equal_makes_n_pieces() {
        for n in 1..=9 {
            let mut p = Plan::new(s(9.0));
            p.split_equal(n, |t| t);
            let pieces = p.pieces();
            assert_eq!(pieces.len(), n);
            for (a, b) in pieces {
                assert!((b - a).as_secs_f64() - 9.0 / n as f64 <= 1e-6);
            }
        }
    }

    #[test]
    fn split_equal_works_inside_the_trim() {
        let mut p = Plan::new(s(10.0));
        p.mark_in(s(2.0));
        p.mark_out(s(8.0));
        p.split_equal(3, |t| t);
        assert_eq!(p.cuts, vec![s(4.0), s(6.0)]);
    }

    #[test]
    fn split_equal_snaps_and_drops_duplicates() {
        let mut p = Plan::new(s(10.0));
        // Everything snaps to 5 s: only one cut survives.
        p.split_equal(4, |_| s(5.0));
        assert_eq!(p.cuts, vec![s(5.0)]);
    }

    #[test]
    fn cuts_stay_sorted_and_apart() {
        let mut p = Plan::new(s(10.0));
        assert_eq!(p.add_cut(s(5.0)), Some(0));
        assert_eq!(p.add_cut(s(2.0)), Some(0));
        assert_eq!(p.add_cut(s(5.05)), None);
        assert_eq!(p.add_cut(s(0.05)), None);
        assert_eq!(p.add_cut(s(10.5)), None);
        assert_eq!(p.cuts, vec![s(2.0), s(5.0)]);
    }

    #[test]
    fn markers_cannot_cross() {
        let mut p = Plan::new(s(10.0));
        p.add_cut(s(3.0));
        p.add_cut(s(6.0));
        assert_eq!(p.move_cut(0, s(9.0)), Some(s(6.0) - MIN_PIECE));
        assert_eq!(p.set_start(s(7.0)), p.cuts[0] - MIN_PIECE);
        assert_eq!(p.set_end(s(1.0)), s(6.0) + MIN_PIECE);
        assert_eq!(p.set_end(s(20.0)), s(10.0));
    }

    #[test]
    fn mark_in_and_out_drop_the_cuts_they_pass() {
        let mut p = Plan::new(s(10.0));
        p.split_equal(5, |t| t);
        p.mark_in(s(3.0));
        assert_eq!(p.cuts, vec![s(4.0), s(6.0), s(8.0)]);
        p.mark_out(s(7.0));
        assert_eq!(p.cuts, vec![s(4.0), s(6.0)]);
        assert_eq!(p.pieces().len(), 3);
    }

    #[test]
    fn stepping_and_nearest() {
        let l = vec![s(0.0), s(1.0), s(2.0)];
        assert_eq!(step(&l, s(1.0), 1), Some(s(2.0)));
        assert_eq!(step(&l, s(1.0), -1), Some(s(0.0)));
        assert_eq!(step(&l, s(1.5), -1), Some(s(1.0)));
        assert_eq!(step(&l, s(2.0), 1), None);
        assert_eq!(nearest(&l, s(1.4)), Some(s(1.0)));
        assert_eq!(nearest(&l, s(1.6)), Some(s(2.0)));
        assert_eq!(frame_index(&l, s(1.9)), 1);
        assert_eq!(frame_index(&l, s(0.0)), 0);
    }

    #[test]
    fn names() {
        let d = std::env::temp_dir().join(format!("mp4s-names-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let src = Path::new("/x/My clip.MP4");
        assert_eq!(output_paths(src, &d, 1), vec![d.join("My clip_trimmed.mp4")]);
        let v = output_paths(src, &d, 3);
        assert_eq!(v[2], d.join("My clip_part03.mp4"));
        std::fs::write(d.join("My clip_part01.mp4"), b"").unwrap();
        assert_eq!(output_paths(src, &d, 2)[0], d.join("My clip_part01 (2).mp4"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn timecodes() {
        assert_eq!(timecode(s(83.4)), "1:23.400");
        assert_eq!(timecode(s(3723.0)), "1:02:03.000");
        assert_eq!(ruler_label(s(61.25), s(0.5)), "1:01.2");
        assert_eq!(ruler_label(s(61.25), s(5.0)), "1:01");
    }
}
