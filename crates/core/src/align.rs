//! Global (offset + framerate) and piecewise alignment of cues against a
//! reference speech timeline.

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use crate::timeline::{FRAME_MS, Interval, Timeline, frame_to_ms};

/// Framerate ratios tried by the global search. Subtitle times are
/// multiplied by the ratio before offsetting.
pub const DEFAULT_RATIOS: [f64; 7] =
    [1.0, 25.0 / 23.976, 23.976 / 25.0, 25.0 / 24.0, 24.0 / 25.0, 24.0 / 23.976, 23.976 / 24.0];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AlignConfig {
    /// Global offset search range (±), ms.
    pub max_offset_ms: i64,
    /// Framerate ratios to try (1.0 should be included).
    pub ratios: Vec<f64>,
    /// A ratio other than 1 must beat ratio 1's normalized correlation
    /// score by this much.
    pub ratio_margin: f64,
    /// Enable piecewise alignment around the global result.
    pub piecewise: bool,
    /// Piecewise offset range around the global offset (±), ms.
    pub window_ms: i64,
    /// Cost of starting a new segment, in frame-score units (one frame of
    /// speech inside a cue scores +1).
    pub split_penalty: f64,
    /// Weight of non-speech frames inside a cue (they score -lambda).
    pub lambda: f64,
    /// Extra cost per frame of overlap created by a backward jump larger
    /// than the gap between the cues on either side of it. Real cuts sit in
    /// dialogue gaps; overlap only happens when the subtitle has content the
    /// video lacks.
    pub overlap_penalty: f64,
    /// Segments with fewer cues are merged into a neighbour.
    pub min_segment_cues: usize,
    /// Adjacent segments whose offsets differ by less than this are merged.
    pub min_split_shift_ms: i64,
    /// Each segment of a split result needs its own correlation peak at
    /// least this prominent; weaker segments are merged into a neighbour.
    pub min_segment_confidence: f64,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            max_offset_ms: 60_000,
            ratios: DEFAULT_RATIOS.to_vec(),
            ratio_margin: 0.01,
            piecewise: true,
            window_ms: 60_000,
            split_penalty: 2000.0,
            lambda: 1.0,
            overlap_penalty: 0.5,
            min_segment_cues: 3,
            min_split_shift_ms: 400,
            min_segment_confidence: 6.0,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RatioCandidate {
    pub ratio: f64,
    pub offset_ms: i64,
    /// Peak correlation normalized by overlap length, in [-1, 1].
    pub score: f64,
    pub prominence: f64,
    pub peaks: Vec<i64>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GlobalResult {
    pub ratio: f64,
    pub offset_ms: i64,
    pub score: f64,
    /// How far the correlation peak stands out from the other offsets
    /// (z-score of the peak, excluding a ±1 s neighbourhood).
    pub prominence: f64,
    pub candidates: Vec<RatioCandidate>,
    /// Offsets (ms) of the strongest local correlation peaks at the chosen
    /// ratio. Several strong peaks hint at a split subtitle.
    pub peaks: Vec<i64>,
}

impl GlobalResult {
    fn from_candidate(c: &RatioCandidate, candidates: Vec<RatioCandidate>) -> Self {
        Self {
            ratio: c.ratio,
            offset_ms: c.offset_ms,
            score: c.score,
            prominence: c.prominence,
            peaks: c.peaks.clone(),
            candidates,
        }
    }
}

/// A run of consecutive (time-sorted) cues sharing one offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    pub first_cue: usize,
    /// Inclusive.
    pub last_cue: usize,
    pub offset_ms: i64,
}

/// Maps original subtitle times to corrected times:
/// `t -> round(t * ratio) + offset(segment containing anchor)`, kept within
/// that segment's clip bounds.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Mapping {
    pub ratio: f64,
    /// (segment start in original ms, offset ms), sorted by start. The first
    /// segment extends to -infinity.
    pub segments: Vec<(i64, i64)>,
    /// Per segment, the corrected time range its timestamps are kept in
    /// (`lo`, `hi`). Where a backward jump makes two segments overlap, the
    /// side with content the video lacks is clipped against the boundary,
    /// which hides its cues (they end up zero length). Empty means no
    /// clipping.
    #[serde(default)]
    pub clips: Vec<(i64, i64)>,
}

impl Mapping {
    pub fn identity() -> Self {
        Self { ratio: 1.0, segments: vec![(i64::MIN, 0)], clips: Vec::new() }
    }
    pub fn constant(ratio: f64, offset_ms: i64) -> Self {
        Self { ratio, segments: vec![(i64::MIN, offset_ms)], clips: Vec::new() }
    }
    fn segment_index(&self, anchor_ms: i64) -> usize {
        self.segments.partition_point(|&(s, _)| s <= anchor_ms).saturating_sub(1)
    }
    pub fn offset_for(&self, anchor_ms: i64) -> i64 {
        self.segments[self.segment_index(anchor_ms)].1
    }
    /// Map time `t`, choosing the segment by `anchor` (normally the cue
    /// start, so a cue's start and end move together).
    pub fn apply(&self, anchor_ms: i64, t_ms: i64) -> i64 {
        let k = self.segment_index(anchor_ms);
        let t = scale(t_ms, self.ratio) + self.segments[k].1;
        match self.clips.get(k) {
            // Same order as `resolve_overlaps`: the lower bound comes from
            // the boundary before this segment, the upper from the next.
            Some(&(lo, hi)) => t.max(lo).min(hi),
            None => t,
        }
    }
    pub fn splits(&self) -> usize {
        self.segments.len().saturating_sub(1)
    }
}

#[inline]
pub fn scale(t_ms: i64, ratio: f64) -> i64 {
    if ratio == 1.0 { t_ms } else { (t_ms as f64 * ratio).round() as i64 }
}

/// Full alignment result.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Alignment {
    pub global: GlobalResult,
    pub segments: Vec<Segment>,
    /// Mapping for any timestamp, by anchor time.
    pub mapping: Mapping,
    /// Corrected interval for each input cue (same order as the input).
    pub cues: Vec<Interval>,
    /// Correlation peak prominence of the corrected cues against the
    /// reference. With splits this checks that all segments line up.
    pub confidence: f64,
}

impl Alignment {
    pub fn splits(&self) -> usize {
        self.segments.len().saturating_sub(1)
    }
}

/// Align time-sorted dialogue cue intervals to the reference timeline.
pub fn align(reference: &Timeline, cues: &[Interval], cfg: &AlignConfig) -> Alignment {
    let mut global = global_align(reference, cues, cfg);
    let single = |c: &RatioCandidate| {
        vec![Segment { first_cue: 0, last_cue: cues.len().saturating_sub(1), offset_ms: c.offset_ms }]
    };
    let chosen = global.candidates.iter().find(|c| c.ratio == global.ratio).cloned();
    let (mut ratio, mut segments) = match &chosen {
        Some(c) => (c.ratio, single(c)),
        None => (1.0, vec![Segment { first_cue: 0, last_cue: cues.len().saturating_sub(1), offset_ms: 0 }]),
    };
    if cfg.piecewise && cues.len() >= 2 * cfg.min_segment_cues.max(1) && chosen.is_some() {
        // Inserted chunks accumulate offset like a framerate drift does, so
        // let the piecewise objective arbitrate between ratio 1 and the best
        // other ratio rather than trusting the global correlation alone.
        let one = global.candidates.iter().find(|c| c.ratio == 1.0).cloned();
        let other =
            global.candidates.iter().filter(|c| c.ratio != 1.0).max_by(|a, b| a.score.total_cmp(&b.score)).cloned();
        let mut tried: Vec<(RatioCandidate, Vec<Segment>, f64)> = Vec::new();
        for c in [one, other].into_iter().flatten() {
            let (segs, obj) = piecewise_for(reference, cues, &c, cfg);
            tried.push((c, segs, obj));
        }
        let one_obj = tried.iter().find(|t| t.0.ratio == 1.0).map(|t| t.2).unwrap_or(f64::NEG_INFINITY);
        let best = tried
            .iter()
            .filter(|t| t.0.ratio == 1.0 || t.2 > one_obj + cfg.ratio_margin)
            .max_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((c, segs, _)) = best {
            ratio = c.ratio;
            segments = if segs.len() > 1 { segs.clone() } else { single(c) };
            global = GlobalResult::from_candidate(c, std::mem::take(&mut global.candidates));
        }
    }
    let scaled: Vec<Interval> =
        cues.iter().map(|c| Interval::new(scale(c.start_ms, ratio), scale(c.end_ms, ratio))).collect();
    let mut out: Vec<Interval> = Vec::with_capacity(cues.len());
    for seg in &segments {
        for c in &scaled[seg.first_cue..=seg.last_cue.min(scaled.len().saturating_sub(1))] {
            out.push(Interval::new(c.start_ms + seg.offset_ms, c.end_ms + seg.offset_ms));
        }
    }
    out.truncate(cues.len());
    let mut clips = resolve_overlaps(reference, &segments, &mut out, cfg.lambda);
    // With splits, every segment must stand on its own: a segment fitted to
    // noise has no sharp correlation peak of its own. Weak segments are
    // merged into a neighbour until all remaining ones are convincing.
    let mut confidence = global.prominence;
    if segments.len() > 1 {
        let rs = RefScore::new(reference, cfg.lambda);
        loop {
            let proms: Vec<f64> =
                segments.iter().map(|s| segment_prominence(reference, &scaled, s, cfg.window_ms)).collect();
            let (weak, &min) = proms.iter().enumerate().min_by(|a, b| a.1.total_cmp(b.1)).expect("segments");
            if min >= cfg.min_segment_confidence || segments.len() == 1 {
                confidence = if segments.len() == 1 && segments[0].offset_ms == global.offset_ms {
                    global.prominence
                } else {
                    min
                };
                break;
            }
            // Merge into the neighbour whose offset fits the weak cues best.
            let nbs = [weak.checked_sub(1), (weak + 1 < segments.len()).then_some(weak + 1)];
            let w = segments[weak];
            let nb = nbs
                .into_iter()
                .flatten()
                .max_by(|&a, &b| {
                    let sa = seg_score(&rs, &scaled, w.first_cue, w.last_cue, segments[a].offset_ms);
                    let sb = seg_score(&rs, &scaled, w.first_cue, w.last_cue, segments[b].offset_ms);
                    sa.total_cmp(&sb)
                })
                .expect("a neighbour");
            let (a, b) = (weak.min(nb), weak.max(nb));
            segments[a] = Segment {
                first_cue: segments[a].first_cue,
                last_cue: segments[b].last_cue,
                offset_ms: segments[nb].offset_ms,
            };
            segments.remove(b);
        }
        if segments.len() == 1 {
            // Fall back to the plain global result.
            segments[0].offset_ms = global.offset_ms;
            confidence = global.prominence;
        }
        out.clear();
        for seg in &segments {
            for c in &scaled[seg.first_cue..=seg.last_cue] {
                out.push(Interval::new(c.start_ms + seg.offset_ms, c.end_ms + seg.offset_ms));
            }
        }
        clips = resolve_overlaps(reference, &segments, &mut out, cfg.lambda);
    }
    let mapping = Mapping {
        ratio,
        segments: segments
            .iter()
            .enumerate()
            .map(|(k, s)| (if k == 0 { i64::MIN } else { cues[s.first_cue].start_ms }, s.offset_ms))
            .collect(),
        clips,
    };
    Alignment { global, segments, mapping, cues: out, confidence }
}

/// Detrended correlation-peak prominence of one segment's cues on their own,
/// over offsets `segment offset ± window`.
fn segment_prominence(reference: &Timeline, scaled: &[Interval], seg: &Segment, window_ms: i64) -> f64 {
    let frames: Vec<(i64, i64)> = scaled[seg.first_cue..=seg.last_cue].iter().map(|c| c.frames()).collect();
    let w = window_ms / FRAME_MS;
    let curve = span_correlation(reference, &frames, seg.offset_ms.div_euclid(FRAME_MS), w);
    if curve.is_empty() {
        return 0.0;
    }
    let hp = detrend(&curve, (3000 / FRAME_MS) as usize);
    // Peak near the chosen offset (the DP objective differs slightly from
    // plain correlation).
    let near = (200 / FRAME_MS) as usize;
    let mid = w as usize;
    let peak = (mid.saturating_sub(near)..=(mid + near).min(hp.len() - 1))
        .max_by(|&a, &b| hp[a].total_cmp(&hp[b]))
        .unwrap_or(mid);
    peak_prominence(&hp, peak, (1000 / FRAME_MS) as usize)
}

/// Correlation of a group of cues with the reference, for offsets
/// `center + j` frames with `j` in `-w..=w` (element `j + w`).
///
/// The cue signal is +1 inside the cues and -1 in the gaps between them,
/// over the span from the first cue start to the last cue end. The reference
/// counts as +1 for speech, -1 for non-speech and 0 outside the timeline.
/// Scores are normalized by the span length. Empty if there are no cues.
pub(crate) fn span_correlation(reference: &Timeline, frames: &[(i64, i64)], center: i64, w: i64) -> Vec<f64> {
    let (Some(span_lo), Some(span_hi)) = (frames.iter().map(|f| f.0).min(), frames.iter().map(|f| f.1).max()) else {
        return Vec::new();
    };
    // Prefix sums of the reference as ±1, zero outside.
    let nr = reference.len() as i64;
    let mut p = Vec::with_capacity(reference.len() + 1);
    p.push(0i64);
    for &b in reference.frames() {
        p.push(p.last().unwrap() + if b { 1 } else { -1 });
    }
    let q = |x: i64| p[x.clamp(0, nr) as usize];
    let span = (span_hi - span_lo).max(1) as f64;
    (-w..=w)
        .map(|j| {
            let d = center + j;
            let all = q(span_hi + d) - q(span_lo + d);
            let inside: i64 = frames.iter().map(|&(a, b)| q(b + d) - q(a + d)).sum();
            (2 * inside - all) as f64 / span
        })
        .collect()
}

/// Piecewise search for one ratio candidate. Returns the merged segments
/// and the objective (segment scores minus split penalties) per cue frame,
/// comparable across ratios.
fn piecewise_for(
    reference: &Timeline,
    cues: &[Interval],
    cand: &RatioCandidate,
    cfg: &AlignConfig,
) -> (Vec<Segment>, f64) {
    let scaled: Vec<Interval> =
        cues.iter().map(|c| Interval::new(scale(c.start_ms, cand.ratio), scale(c.end_ms, cand.ratio))).collect();
    // Search all offsets near any strong correlation peak, so several
    // inserted chunks adding up to more than the window still fit.
    let mut lo = cand.peaks.iter().copied().chain([cand.offset_ms]).min().unwrap_or(0) - cfg.window_ms;
    let mut hi = cand.peaks.iter().copied().chain([cand.offset_ms]).max().unwrap_or(0) + cfg.window_ms;
    // If a segment ends up pinned near an edge of the range, the true offset
    // may lie beyond it: widen that side and retry (bounded).
    let cap = cfg.max_offset_ms + 3 * cfg.window_ms;
    let edge = 2_000;
    let mut segs;
    let mut tries = 0;
    loop {
        let offs = piecewise_offsets(reference, &scaled, lo, hi, cfg);
        segs = merge_segments(reference, &scaled, segments_from_offsets(&offs), cfg);
        tries += 1;
        let at_lo = segs.iter().any(|s| s.offset_ms - lo < edge) && lo > cand.offset_ms - cap;
        let at_hi = segs.iter().any(|s| hi - s.offset_ms < edge) && hi < cand.offset_ms + cap;
        if tries >= 4 || !(at_lo || at_hi) {
            break;
        }
        if at_lo {
            lo = (lo - cfg.window_ms).max(cand.offset_ms - cap);
        }
        if at_hi {
            hi = (hi + cfg.window_ms).min(cand.offset_ms + cap);
        }
    }
    let rs = RefScore::new(reference, cfg.lambda);
    let total: f64 = segs.iter().map(|s| seg_score(&rs, &scaled, s.first_cue, s.last_cue, s.offset_ms)).sum::<f64>()
        - cfg.split_penalty * segs.len().saturating_sub(1) as f64;
    let frames: i64 = scaled
        .iter()
        .map(|c| {
            let (a, b) = c.frames();
            b - a
        })
        .sum();
    (segs, total / frames.max(1) as f64)
}

/// Global alignment: FFT cross-correlation of ±1 signals for each ratio.
pub fn global_align(reference: &Timeline, cues: &[Interval], cfg: &AlignConfig) -> GlobalResult {
    let k = (cfg.max_offset_ms / FRAME_MS).max(0) as usize;
    let nr = reference.len();
    let ratios: Vec<f64> = if cfg.ratios.is_empty() { vec![1.0] } else { cfg.ratios.clone() };
    let sub_len = |r: f64| -> usize { cues.iter().map(|c| c.frames_scaled(r).1.max(0) as usize).max().unwrap_or(0) };
    let ns_max = ratios.iter().map(|&r| sub_len(r)).max().unwrap_or(0);
    if nr == 0 || ns_max == 0 {
        return GlobalResult {
            ratio: 1.0,
            offset_ms: 0,
            score: 0.0,
            prominence: 0.0,
            candidates: vec![],
            peaks: vec![],
        };
    }
    let l = (nr.max(ns_max) + k + 1).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(l);
    let inv = planner.plan_fft_inverse(l);

    let mut rbuf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); l];
    for (i, &b) in reference.frames().iter().enumerate() {
        rbuf[i].re = if b { 1.0 } else { -1.0 };
    }
    fwd.process(&mut rbuf);

    let mut candidates = Vec::new();
    for &r in &ratios {
        let ns = sub_len(r);
        let mut sbuf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); l];
        for s in sbuf.iter_mut().take(ns) {
            s.re = -1.0;
        }
        for c in cues {
            let (s, e) = c.frames_scaled(r);
            for x in sbuf.iter_mut().take(e.max(0) as usize).skip(s.max(0) as usize) {
                x.re = 1.0;
            }
        }
        fwd.process(&mut sbuf);
        for (sv, rv) in sbuf.iter_mut().zip(rbuf.iter()) {
            *sv = rv * sv.conj();
        }
        inv.process(&mut sbuf);
        // curve[j] = normalized score for offset d = j - k (frames)
        let mut curve = Vec::with_capacity(2 * k + 1);
        for j in 0..=2 * k {
            let d = j as i64 - k as i64;
            let idx = if d >= 0 { d as usize } else { l - (-d) as usize };
            let raw = sbuf[idx].re / l as f64;
            // overlap of [0, ns) shifted by d with [0, nr)
            let lo = 0i64.max(-d);
            let hi = (ns as i64).min(nr as i64 - d);
            let overlap = (hi - lo).max(1) as f64;
            curve.push(raw / overlap);
        }
        let jbest = argmax(&curve);
        // Judge the peak on a detrended curve: slow variations in dialogue
        // density otherwise inflate the spread and hide a sharp true peak.
        let hp = detrend(&curve, (3000 / FRAME_MS) as usize);
        let prominence = peak_prominence(&hp, jbest, (1000 / FRAME_MS) as usize);
        let peaks = strong_peaks(&hp, 4.0, (2000 / FRAME_MS) as usize, 6)
            .into_iter()
            .map(|j| frame_to_ms(j as i64 - k as i64))
            .collect();
        candidates.push(RatioCandidate {
            ratio: r,
            offset_ms: frame_to_ms(jbest as i64 - k as i64),
            score: curve[jbest],
            prominence,
            peaks,
        });
    }
    // Keep ratio 1 unless another ratio beats it by the margin.
    let one_score = candidates.iter().find(|c| c.ratio == 1.0).map(|c| c.score).unwrap_or(f64::NEG_INFINITY);
    let mut chosen_idx = candidates.iter().position(|c| c.ratio == 1.0).unwrap_or(0);
    for (i, c) in candidates.iter().enumerate() {
        if c.ratio != 1.0 && c.score >= one_score + cfg.ratio_margin && c.score > candidates[chosen_idx].score {
            chosen_idx = i;
        }
    }
    let chosen = candidates[chosen_idx].clone();
    GlobalResult::from_candidate(&chosen, candidates)
}

/// Subtract a centred moving average of half-width `w`.
pub(crate) fn detrend(curve: &[f64], w: usize) -> Vec<f64> {
    let n = curve.len();
    let mut pre = Vec::with_capacity(n + 1);
    pre.push(0.0);
    for &x in curve {
        pre.push(pre.last().unwrap() + x);
    }
    (0..n)
        .map(|j| {
            let a = j.saturating_sub(w);
            let b = (j + w + 1).min(n);
            curve[j] - (pre[b] - pre[a]) / (b - a) as f64
        })
        .collect()
}

/// Local maxima with z-score >= `min_z`, at least `sep` apart, strongest
/// first, at most `max` of them.
fn strong_peaks(curve: &[f64], min_z: f64, sep: usize, max: usize) -> Vec<usize> {
    let n = curve.len() as f64;
    if curve.len() < 3 {
        return Vec::new();
    }
    let mean = curve.iter().sum::<f64>() / n;
    let sd = (curve.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
    if sd <= 1e-12 {
        return Vec::new();
    }
    let mut cand: Vec<usize> = (1..curve.len() - 1)
        .filter(|&j| curve[j] >= curve[j - 1] && curve[j] > curve[j + 1] && (curve[j] - mean) / sd >= min_z)
        .collect();
    cand.sort_by(|&a, &b| curve[b].total_cmp(&curve[a]));
    let mut out: Vec<usize> = Vec::new();
    for j in cand {
        if out.iter().all(|&o| o.abs_diff(j) >= sep) {
            out.push(j);
            if out.len() == max {
                break;
            }
        }
    }
    out
}

/// z-score of the peak against the curve outside ±`exclude` of it.
pub(crate) fn peak_prominence(curve: &[f64], peak: usize, exclude: usize) -> f64 {
    let rest: Vec<f64> =
        curve.iter().enumerate().filter(|(j, _)| j.abs_diff(peak) > exclude).map(|(_, &v)| v).collect();
    if rest.len() < 2 {
        return 0.0;
    }
    let mean = rest.iter().sum::<f64>() / rest.len() as f64;
    let var = rest.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / rest.len() as f64;
    let sd = var.sqrt();
    if sd <= 1e-12 {
        return 0.0;
    }
    (curve[peak] - mean) / sd
}

impl Interval {
    /// Frames of this interval after scaling times by `ratio`.
    pub fn frames_scaled(&self, ratio: f64) -> (i64, i64) {
        Interval::new(scale(self.start_ms, ratio), scale(self.end_ms, ratio)).frames()
    }
}

/// Reference prefix sums extended beyond the timeline: frames outside the
/// reference count as non-speech (-lambda).
struct RefScore {
    p: Vec<f64>,
    lambda: f64,
}

impl RefScore {
    fn new(reference: &Timeline, lambda: f64) -> Self {
        Self { p: reference.weighted_prefix(lambda), lambda }
    }
    #[inline]
    fn q(&self, x: i64) -> f64 {
        let n = (self.p.len() - 1) as i64;
        if x <= 0 {
            -self.lambda * x as f64
        } else if x >= n {
            self.p[n as usize] - self.lambda * (x - n) as f64
        } else {
            self.p[x as usize]
        }
    }
    /// Sum of weights over frames [s, e).
    #[inline]
    fn range(&self, s: i64, e: i64) -> f64 {
        self.q(e) - self.q(s)
    }
}

/// Per-cue offsets (ms) from the split DP, searching offsets in [lo, hi].
fn piecewise_offsets(reference: &Timeline, scaled: &[Interval], lo_ms: i64, hi_ms: i64, cfg: &AlignConfig) -> Vec<i64> {
    let rs = RefScore::new(reference, cfg.lambda);
    let d0 = lo_ms.div_euclid(FRAME_MS);
    let dn = (hi_ms.div_euclid(FRAME_MS) - d0 + 1).max(1) as usize;
    let m = scaled.len();
    let frames: Vec<(i64, i64)> = scaled.iter().map(|iv| iv.frames()).collect();
    let score_row = |i: usize, out: &mut [f64]| {
        let (s, e) = frames[i];
        for (j, o) in out.iter_mut().enumerate() {
            let d = d0 + j as i64;
            *o = rs.range(s + d, e + d);
        }
    };
    let words = dn.div_ceil(64);
    let mut bits = vec![0u64; m * words];
    let mut row_arg = vec![0usize; m];
    let mut prev = vec![0.0f64; dn];
    let mut cur = vec![0.0f64; dn];
    let mut sc = vec![0.0f64; dn];
    score_row(0, &mut prev);
    row_arg[0] = argmax(&prev);
    let mut prev_end = frames[0].1;
    for i in 1..m {
        score_row(i, &mut sc);
        let src = row_arg[i - 1];
        let jump_base = prev[src] - cfg.split_penalty;
        // A backward jump larger than the gap before this cue makes shifted
        // cues overlap; charge for the doubly-covered time.
        let gap = (frames[i].0 - prev_end).max(0);
        prev_end = prev_end.max(frames[i].1);
        let free_back = src as i64 - gap; // targets j >= this need no overlap cost
        let row_bits = &mut bits[i * words..(i + 1) * words];
        for j in 0..dn {
            let stay = prev[j];
            let overlap = (free_back - j as i64).max(0) as f64;
            let jump_val = jump_base - cfg.overlap_penalty * overlap;
            let v = if jump_val > stay {
                row_bits[j / 64] |= 1 << (j % 64);
                jump_val
            } else {
                stay
            };
            cur[j] = v + sc[j];
        }
        row_arg[i] = argmax(&cur);
        std::mem::swap(&mut prev, &mut cur);
    }
    let mut out = vec![0i64; m];
    let mut j = row_arg[m - 1];
    out[m - 1] = j as i64;
    for i in (1..m).rev() {
        if bits[i * words + j / 64] >> (j % 64) & 1 == 1 {
            j = row_arg[i - 1];
        }
        out[i - 1] = j as i64;
    }
    out.iter().map(|&j| (d0 + j) * FRAME_MS).collect()
}

/// Index of the maximum; ties go to the first.
fn argmax(v: &[f64]) -> usize {
    let mut best = 0;
    for (i, &x) in v.iter().enumerate() {
        if x > v[best] {
            best = i;
        }
    }
    best
}

fn segments_from_offsets(offs: &[i64]) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    for (i, &d) in offs.iter().enumerate() {
        match segs.last_mut() {
            Some(s) if s.offset_ms == d => s.last_cue = i,
            _ => segs.push(Segment { first_cue: i, last_cue: i, offset_ms: d }),
        }
    }
    segs
}

/// Score of cues `range` at offset `d`.
fn seg_score(rs: &RefScore, scaled: &[Interval], first: usize, last: usize, d_ms: i64) -> f64 {
    let d = d_ms.div_euclid(FRAME_MS);
    scaled[first..=last]
        .iter()
        .map(|c| {
            let (s, e) = c.frames();
            rs.range(s + d, e + d)
        })
        .sum()
}

/// Merge segments that are too short or whose offset barely differs from a
/// neighbour. The merged segment takes whichever of the two offsets scores
/// better over its cues.
fn merge_segments(
    reference: &Timeline,
    scaled: &[Interval],
    mut segs: Vec<Segment>,
    cfg: &AlignConfig,
) -> Vec<Segment> {
    let rs = RefScore::new(reference, cfg.lambda);
    let min_cues = cfg.min_segment_cues.max(1);
    loop {
        if segs.len() <= 1 {
            return segs;
        }
        // Find the weakest segment to merge: smallest offset difference to a
        // neighbour under the threshold, or fewest cues under the minimum.
        let mut pick: Option<(usize, usize, i64)> = None; // (seg, neighbour, key)
        for k in 0..segs.len() {
            let n_cues = segs[k].last_cue - segs[k].first_cue + 1;
            for nb in [k.checked_sub(1), (k + 1 < segs.len()).then_some(k + 1)].into_iter().flatten() {
                let diff = (segs[k].offset_ms - segs[nb].offset_ms).abs();
                let key = if diff < cfg.min_split_shift_ms {
                    diff
                } else if n_cues < min_cues {
                    cfg.min_split_shift_ms + n_cues as i64 * 1_000_000 + diff / 1000
                } else {
                    continue;
                };
                if pick.is_none_or(|p| key < p.2) {
                    pick = Some((k, nb, key));
                }
            }
        }
        let Some((k, nb, _)) = pick else { return segs };
        let (a, b) = (k.min(nb), k.max(nb));
        let first = segs[a].first_cue;
        let last = segs[b].last_cue;
        let oa = seg_score(&rs, scaled, first, last, segs[a].offset_ms);
        let ob = seg_score(&rs, scaled, first, last, segs[b].offset_ms);
        // A short segment adopts its neighbour's offset; otherwise keep the
        // better-scoring offset.
        let n_k = segs[k].last_cue - segs[k].first_cue + 1;
        let offset = if n_k < min_cues {
            segs[nb].offset_ms
        } else if oa >= ob {
            segs[a].offset_ms
        } else {
            segs[b].offset_ms
        };
        segs[a] = Segment { first_cue: first, last_cue: last, offset_ms: offset };
        segs.remove(b);
        // Re-join neighbours that now share an offset.
        let mut i = 1;
        while i < segs.len() {
            if segs[i].offset_ms == segs[i - 1].offset_ms {
                segs[i - 1].last_cue = segs[i].last_cue;
                segs.remove(i);
            } else {
                i += 1;
            }
        }
    }
}

/// At a backward jump (later segment shifted earlier than the previous one),
/// shifted cues from both sides can overlap. That happens when the subtitle
/// contains content the video lacks. Whichever side of the overlap matches
/// speech worse is treated as the missing content and collapsed against the
/// boundary so cues stay in order. Returns each segment's clip bounds (see
/// [`Mapping::clips`]), so the file's other cues can be hidden the same way.
fn resolve_overlaps(reference: &Timeline, segs: &[Segment], cues: &mut [Interval], lambda: f64) -> Vec<(i64, i64)> {
    let mut clips = vec![(i64::MIN, i64::MAX); segs.len()];
    if cues.is_empty() {
        return clips;
    }
    let rs = RefScore::new(reference, lambda);
    let mean_score = |ivs: &[Interval]| -> f64 {
        let (mut s, mut n) = (0.0, 0i64);
        for iv in ivs {
            let (a, b) = iv.frames();
            s += rs.range(a, b);
            n += b - a;
        }
        if n == 0 { 0.0 } else { s / n as f64 }
    };
    for k in 1..segs.len() {
        let (a, b) = (segs[k - 1], segs[k]);
        if b.offset_ms >= a.offset_ms {
            continue;
        }
        let b_start = cues[b.first_cue].start_ms;
        let a_end = cues[a.last_cue].end_ms;
        if a_end <= b_start {
            continue;
        }
        let a_tail: Vec<usize> = (a.first_cue..=a.last_cue).filter(|&i| cues[i].end_ms > b_start).collect();
        let b_head: Vec<usize> = (b.first_cue..=b.last_cue).filter(|&i| cues[i].start_ms < a_end).collect();
        let sa = mean_score(&a_tail.iter().map(|&i| cues[i]).collect::<Vec<_>>());
        let sb = mean_score(&b_head.iter().map(|&i| cues[i]).collect::<Vec<_>>());
        if sa < sb {
            for &i in &a_tail {
                cues[i].end_ms = cues[i].end_ms.min(b_start);
                cues[i].start_ms = cues[i].start_ms.min(cues[i].end_ms);
            }
            clips[k - 1].1 = clips[k - 1].1.min(b_start);
        } else {
            for &i in &b_head {
                cues[i].start_ms = cues[i].start_ms.max(a_end);
                cues[i].end_ms = cues[i].end_ms.max(cues[i].start_ms);
            }
            clips[k].0 = clips[k].0.max(a_end);
        }
    }
    clips
}

/// Fraction of cue time that overlaps active reference frames.
pub fn overlap_score(reference: &Timeline, cues: &[Interval]) -> f64 {
    let p = reference.count_prefix();
    let n = reference.len() as i64;
    let (mut hit, mut total) = (0u64, 0u64);
    for c in cues {
        let (s, e) = c.frames();
        if e <= s {
            continue;
        }
        total += (e - s) as u64;
        let (cs, ce) = (s.clamp(0, n) as usize, e.clamp(0, n) as usize);
        hit += (p[ce] - p[cs]) as u64;
    }
    if total == 0 { 0.0 } else { hit as f64 / total as f64 }
}

#[cfg(test)]
mod tests;
