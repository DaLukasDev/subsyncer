//! Perturbations applied to correct cue timings, with ground truth kept per
//! cue so recovery can be measured.

use subsyncer_core::align::{DEFAULT_RATIOS, scale};
use subsyncer_core::timeline::Interval;

use crate::synth::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Already in sync.
    None,
    /// 0.3-1.5 s constant offset.
    SmallOffset,
    /// 2-55 s constant offset.
    Offset,
    /// Framerate mismatch plus an offset.
    Framerate,
    /// The video has an extra chunk (ad break, recap) the subtitle lacks.
    InsertChunk,
    /// Two inserted chunks.
    InsertTwo,
    /// The subtitle has content the video lacks.
    RemoveChunk,
    /// Framerate mismatch and an inserted chunk.
    FramerateInsert,
    /// Subtitle for a different video (must never be written).
    WrongVideo,
    /// The video has an extra chunk near the end (last 15 %). Not in
    /// `ALL`: used by the embedded-track check, where the end of the file is
    /// the hardest part to cover with samples.
    LateInsert,
    /// The offset changes in a few steps of 0.4-1.5 s, either way, at long
    /// gaps: the subtitle was timed for a version whose act breaks have
    /// other lengths.
    Steps,
    /// The subtle framerate mismatch (24 vs 23.976 fps): a drift of about
    /// 2.5 s over 40 minutes, which a few steps could imitate.
    SlowDrift,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::None,
        Kind::SmallOffset,
        Kind::Offset,
        Kind::Framerate,
        Kind::InsertChunk,
        Kind::InsertTwo,
        Kind::RemoveChunk,
        Kind::FramerateInsert,
        Kind::WrongVideo,
    ];
    /// Extra kinds, not run by default.
    pub const EXTRA: [Kind; 1] = [Kind::LateInsert];
    /// Offsets that change in small steps or gradually, which can look
    /// alike. Not run by default.
    pub const DRIFTS: [Kind; 2] = [Kind::Steps, Kind::SlowDrift];

    pub fn name(&self) -> &'static str {
        match self {
            Kind::None => "none",
            Kind::SmallOffset => "small_offset",
            Kind::Offset => "offset",
            Kind::Framerate => "framerate",
            Kind::InsertChunk => "insert_chunk",
            Kind::InsertTwo => "insert_two",
            Kind::RemoveChunk => "remove_chunk",
            Kind::FramerateInsert => "framerate_insert",
            Kind::WrongVideo => "wrong_video",
            Kind::LateInsert => "late_insert",
            Kind::Steps => "steps",
            Kind::SlowDrift => "slow_drift",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.iter().chain(&Kind::EXTRA).chain(&Kind::DRIFTS).copied().find(|k| k.name() == s)
    }
}

/// A perturbed subtitle: the cue timings as given to the aligner and, for
/// each, where it really belongs (None for content the video lacks).
#[derive(Clone, Debug)]
pub struct Perturbed {
    pub kind: Kind,
    pub desc: String,
    pub cues: Vec<Interval>,
    pub truth: Vec<Option<Interval>>,
}

fn sign(rng: &mut Rng) -> i64 {
    if rng.chance(0.5) { 1 } else { -1 }
}

/// Pick a chunk position in [lo, hi), snapped to just before a cue.
fn chunk_point(rng: &mut Rng, cues: &[(Interval, Option<Interval>)], lo: i64, hi: i64) -> i64 {
    let p = rng.range(lo, hi);
    // Snap to the gap before the next cue starting after p.
    cues.iter().map(|c| c.0.start_ms).find(|&s| s >= p).map(|s| s - 20).unwrap_or(p)
}

pub fn perturb(kind: Kind, truth: &[Interval], len_ms: i64, rng: &mut Rng) -> Perturbed {
    let base: Vec<(Interval, Option<Interval>)> = truth.iter().map(|&c| (c, Some(c))).collect();
    let (desc, out): (String, Vec<(Interval, Option<Interval>)>) = match kind {
        Kind::None => ("in sync".into(), base),
        // The reference is swapped by the caller; keep the cues, but no cue
        // has a correct position in that video.
        Kind::WrongVideo => ("different video".into(), base.into_iter().map(|(c, _)| (c, None)).collect()),
        Kind::SmallOffset => {
            let d = sign(rng) * rng.range(300, 1_500);
            (format!("offset {d} ms"), shift_all(base, -d))
        }
        Kind::Offset => {
            let d = sign(rng) * rng.range(2_000, 55_000);
            (format!("offset {d} ms"), shift_all(base, -d))
        }
        Kind::Framerate => {
            let r = DEFAULT_RATIOS[rng.range(1, DEFAULT_RATIOS.len() as i64) as usize];
            let d = rng.range(-3_000, 3_000);
            (format!("ratio {r:.5}, offset {d} ms"), scale_all(base, r, -d))
        }
        Kind::InsertChunk => {
            let t = rng.range(5_000, 50_000);
            let p = chunk_point(rng, &base, len_ms / 5, len_ms * 4 / 5);
            (format!("video +{t} ms at {p} ms"), insert_chunk(base, p, t))
        }
        Kind::InsertTwo => {
            let t1 = rng.range(5_000, 40_000);
            let p1 = chunk_point(rng, &base, len_ms / 6, len_ms * 2 / 5);
            let mut v = insert_chunk(base, p1, t1);
            let t2 = rng.range(5_000, 40_000);
            let p2 = chunk_point(rng, &v, len_ms / 2, len_ms * 4 / 5 - t1);
            v = insert_chunk(v, p2, t2);
            (format!("video +{t1} ms at {p1} ms, +{t2} ms at {p2} ms"), v)
        }
        Kind::LateInsert => {
            let t = rng.range(5_000, 40_000);
            let p = chunk_point(rng, &base, len_ms * 85 / 100, len_ms * 97 / 100);
            (format!("video +{t} ms at {p} ms"), insert_chunk(base, p, t))
        }
        Kind::RemoveChunk => {
            let t = rng.range(5_000, 40_000);
            let p = chunk_point(rng, &base, len_ms / 5, len_ms * 4 / 5);
            (format!("subtitle +{t} ms at {p} ms"), remove_chunk(base, p, t, rng))
        }
        Kind::Steps => {
            let n = rng.range(2, 5);
            let d0 = rng.range(-1_000, 1_000);
            let mut v = shift_all(base, -d0);
            let mut desc = format!("offset {d0} ms");
            let mut total = d0;
            for k in 0..n {
                // One step per equal part of the middle 80 %, at its longest gap.
                let part = len_ms * 8 / 10 / n;
                let lo = len_ms / 10 + k * part + part / 4;
                let p = longest_gap(&v, lo, lo + part / 2);
                let step = sign(rng) * rng.range(400, 1_500);
                total += step;
                desc.push_str(&format!(", {total} ms from {p} ms"));
                v =
                    v.into_iter()
                        .map(|(c, t)| {
                            if c.start_ms >= p {
                                (Interval::new(c.start_ms - step, c.end_ms - step), t)
                            } else {
                                (c, t)
                            }
                        })
                        .collect();
            }
            (desc, v)
        }
        Kind::SlowDrift => {
            let r = if rng.chance(0.5) { 24.0 / 23.976 } else { 23.976 / 24.0 };
            let d = rng.range(-1_000, 1_000);
            (format!("ratio {r:.5}, offset {d} ms"), scale_all(base, r, -d))
        }
        Kind::FramerateInsert => {
            let r = DEFAULT_RATIOS[rng.range(1, DEFAULT_RATIOS.len() as i64) as usize];
            let t = rng.range(5_000, 40_000);
            let p = chunk_point(rng, &base, len_ms / 5, len_ms * 4 / 5);
            let v = insert_chunk(base, p, t);
            (format!("ratio {r:.5}, video +{t} ms at {p} ms"), scale_all(v, r, 0))
        }
    };
    let mut out = out;
    out.retain(|(c, _)| c.end_ms > 0);
    out.sort_by_key(|(c, _)| (c.start_ms, c.end_ms));
    Perturbed { kind, desc, cues: out.iter().map(|x| x.0).collect(), truth: out.iter().map(|x| x.1).collect() }
}

/// The middle of the longest gap between cues starting in [lo, hi), as the
/// start of the cue after it (where a step takes effect).
fn longest_gap(cues: &[(Interval, Option<Interval>)], lo: i64, hi: i64) -> i64 {
    let mut best = (i64::MIN, lo);
    let mut prev_end: Option<i64> = None;
    for (c, _) in cues {
        if let Some(e) = prev_end
            && c.start_ms >= lo
            && c.start_ms < hi
            && c.start_ms - e > best.0
        {
            best = (c.start_ms - e, c.start_ms);
        }
        prev_end = Some(prev_end.map_or(c.end_ms, |e| e.max(c.end_ms)));
    }
    best.1
}

fn shift_all(v: Vec<(Interval, Option<Interval>)>, d: i64) -> Vec<(Interval, Option<Interval>)> {
    v.into_iter().map(|(c, t)| (Interval::new(c.start_ms + d, c.end_ms + d), t)).collect()
}

/// Subtitle timed for a different framerate: the aligner must multiply by
/// `r` to recover, so the subtitle is the truth divided by `r`.
fn scale_all(v: Vec<(Interval, Option<Interval>)>, r: f64, d: i64) -> Vec<(Interval, Option<Interval>)> {
    v.into_iter()
        .map(|(c, t)| (Interval::new(scale(c.start_ms, 1.0 / r) + d, scale(c.end_ms, 1.0 / r) + d), t))
        .collect()
}

/// Video gained `t` ms at time `p` (in current subtitle time): cues inside
/// [p, p+t) have no counterpart in the subtitle (dropped); cues after are
/// `t` early in the subtitle.
fn insert_chunk(v: Vec<(Interval, Option<Interval>)>, p: i64, t: i64) -> Vec<(Interval, Option<Interval>)> {
    v.into_iter()
        .filter(|(c, _)| !(c.end_ms > p && c.start_ms < p + t))
        .map(|(c, tr)| if c.start_ms >= p + t { (Interval::new(c.start_ms - t, c.end_ms - t), tr) } else { (c, tr) })
        .collect()
}

/// Subtitle has `t` ms of extra content at `p`: later cues are `t` late and
/// the gap is filled with cues that have no place in the video.
fn remove_chunk(
    v: Vec<(Interval, Option<Interval>)>,
    p: i64,
    t: i64,
    rng: &mut Rng,
) -> Vec<(Interval, Option<Interval>)> {
    let mut out: Vec<(Interval, Option<Interval>)> = v
        .into_iter()
        .map(|(c, tr)| if c.start_ms >= p { (Interval::new(c.start_ms + t, c.end_ms + t), tr) } else { (c, tr) })
        .collect();
    let mut s = p + rng.range(200, 1_500);
    while s < p + t - 1_500 {
        let d = rng.range(900, 3_500);
        out.push((Interval::new(s, (s + d).min(p + t - 100)), None));
        s += d + rng.range(200, 2_000);
    }
    out
}
