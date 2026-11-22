//! Checking an embedded subtitle track against the audio before it is used
//! as the reference.
//!
//! Embedded text tracks are usually in sync with the video, but not always:
//! a track muxed in from another release or cut can be off. Trusting a bad
//! track would "correct" a good subtitle to match it, with high confidence,
//! because two subtitle tracks line up almost perfectly. So the track is
//! compared with speech detected in the audio first:
//!
//! 1. A few short audio samples, spread over the file where the track has
//!    the most dialogue, the last one at the end of the dialogue
//!    ([`plan_samples`], [`check_sample`]). If every sample clearly lines up
//!    with the track, the track is used; if any clearly disagrees, it is not
//!    ([`judge_samples`]).
//! 2. Otherwise the whole audio is analysed: the track is aligned against it
//!    like any subtitle, and every two-minute stretch of it is checked on its
//!    own ([`check_full`]).
//! 3. If syncing a subtitle against an accepted track splits the subtitle,
//!    the result relies on the track's timing in each segment's region,
//!    which the samples may have missed. Each region is checked against the
//!    audio again ([`check_region`]).
//!
//! The thresholds are calibrated with `subsyncer-eval embedded` (synthetic
//! episodes) and on real audio (`scripts/embedded-check-e2e.py`).

use serde::{Deserialize, Serialize};

use crate::align::{self, AlignConfig, detrend, peak_prominence, span_correlation};
use crate::timeline::{FRAME_MS, Interval, Timeline};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckConfig {
    /// Largest overall timing error of the track still counted as in sync
    /// (judged on the average of the samples, or the full alignment).
    pub tolerance_ms: i64,
    /// Largest error of a single window (sample or stretch of the full
    /// check). Wider: a correctly timed subtitle drifts by ±150 ms or so
    /// from one two-minute stretch to the next.
    pub window_tolerance_ms: i64,
    /// Measured offset of a track that is in sync: speech detection reports
    /// speech about 100 ms after cue starts. The tolerance is centred here.
    pub expected_lag_ms: i64,
    /// Number of audio samples.
    pub samples: usize,
    /// Cues starting inside a sample's window are checked.
    pub window_ms: i64,
    /// Offsets searched around each sample (±). The audio decoded per
    /// sample is `window_ms + 2 * pad_ms` long.
    pub pad_ms: i64,
    /// Each sample's correlation peak must be at least this prominent
    /// (detrended z-score) to count as a clear answer.
    pub min_prominence: f64,
    /// Samples with fewer cues give no answer.
    pub min_cues: usize,
    /// Shorter videos skip sampling: analysing all of their audio is cheap.
    pub min_duration_ms: i64,
    /// Full check: alignment confidence the track needs against the audio.
    pub min_confidence: f64,
    /// Full check: offsets searched for each window of the track (±).
    pub scan_range_ms: i64,
    /// Full check: a window that lines up elsewhere at least this clearly
    /// rejects the track.
    pub scan_min_prominence: f64,
    /// Full check: a window whose in-sync support is below this doesn't
    /// back the track up.
    pub scan_min_support: f64,
    /// Full check: this many unsupported windows in a row reject the track
    /// (a stretch that is off). Noise only causes isolated ones.
    pub scan_max_unsupported_run: usize,
    /// Full check: so does a larger share of cues in unsupported windows.
    pub scan_max_unsupported_fraction: f64,
    /// Region checks (segments of a split result): fewest cues to tell.
    pub min_region_cues: usize,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            tolerance_ms: 250,
            window_tolerance_ms: 400,
            expected_lag_ms: 100,
            samples: 3,
            window_ms: 120_000,
            pad_ms: 15_000,
            min_prominence: 4.0,
            min_cues: 10,
            min_duration_ms: 20 * 60_000,
            min_confidence: 6.0,
            scan_range_ms: 60_000,
            scan_min_prominence: 6.0,
            scan_min_support: 1.5,
            scan_max_unsupported_run: 2,
            scan_max_unsupported_fraction: 0.2,
            min_region_cues: 5,
        }
    }
}

/// One audio sample to analyse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// Cues starting in `[start_ms, start_ms + window_ms)` are checked.
    pub start_ms: i64,
    pub window_ms: i64,
    /// Audio to analyse: `[audio_start_ms, audio_start_ms + audio_len_ms)`.
    pub audio_start_ms: i64,
    pub audio_len_ms: i64,
}

/// Pick where to sample. The last sample covers the end of the track's
/// dialogue: a cut there would only show in the last stretch, while a cut
/// anywhere earlier shifts everything after it, later samples included.
/// The others go where the track has the most dialogue, one per equal part
/// of the time before the last sample. Empty if the video is too short to
/// bother (check the full audio instead).
pub fn plan_samples(track: &[Interval], duration_ms: i64, cfg: &CheckConfig) -> Vec<Sample> {
    let n = cfg.samples as i64;
    let (Some(dialogue_end), w) = (track.iter().map(|c| c.end_ms).max(), cfg.window_ms) else {
        return Vec::new();
    };
    if n == 0 || duration_ms < cfg.min_duration_ms {
        return Vec::new();
    }
    // Window starts must leave room for the padding on both sides.
    let (lo, hi) = (cfg.pad_ms, duration_ms - cfg.pad_ms - w);
    if hi < lo {
        return Vec::new();
    }
    let mut starts: Vec<i64> = track.iter().map(|c| c.start_ms).collect();
    starts.sort_unstable();
    let cues_in = |s: i64| starts.partition_point(|&x| x < s + w) - starts.partition_point(|&x| x < s);
    let step = 1000;
    let mut last = (dialogue_end + step - w).clamp(lo, hi);
    while cues_in(last) < cfg.min_cues && last - step >= lo {
        last -= step;
    }
    let mut out = Vec::new();
    if n > 1 {
        let part = (last - lo) / (n - 1);
        if part < w {
            return Vec::new();
        }
        let tl = Timeline::from_intervals(track.iter().copied(), Some((duration_ms / FRAME_MS) as usize));
        let pre = tl.count_prefix();
        let at = |ms: i64| pre[((ms / FRAME_MS) as usize).min(pre.len() - 1)];
        let coverage = |s: i64| at(s + w) - at(s);
        for k in 0..n - 1 {
            let first = lo + k * part;
            let range = || (first..=first + part - w).step_by(step as usize);
            let best = range().map(coverage).max().unwrap_or(0);
            out.push(range().find(|&s| coverage(s) == best).unwrap_or(first));
        }
    }
    out.push(last);
    out.into_iter()
        .map(|s| Sample { start_ms: s, window_ms: w, audio_start_ms: s - cfg.pad_ms, audio_len_ms: w + 2 * cfg.pad_ms })
        .collect()
}

/// How one sample of the track compares with the audio.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleCheck {
    pub start_ms: i64,
    /// Track cues inside the window.
    pub cues: usize,
    /// Shift that best lines the track's cues up with the audio.
    pub offset_ms: i64,
    /// How clearly that shift stands out (detrended z-score).
    pub prominence: f64,
    /// How clearly the track lines up at the in-sync offset (expected lag
    /// ± window tolerance), on the same scale.
    pub support: f64,
}

/// Compare the track's cues inside `sample` with speech detected in the
/// sample's audio (`audio` frame 0 is `sample.audio_start_ms`).
pub fn check_sample(audio: &Timeline, track: &[Interval], sample: &Sample, cfg: &CheckConfig) -> SampleCheck {
    if audio.active_fraction() < 0.005 {
        return SampleCheck { start_ms: sample.start_ms, cues: 0, offset_ms: 0, prominence: 0.0, support: 0.0 };
    }
    window_check(audio, sample.audio_start_ms, track, sample.start_ms, sample.window_ms, cfg.pad_ms, cfg)
}

/// Best offset (within ±`range_ms`) and its prominence for the track's cues
/// starting in `[start_ms, start_ms + window_ms)`. `audio` frame 0 is
/// `audio_start_ms`.
fn window_check(
    audio: &Timeline,
    audio_start_ms: i64,
    track: &[Interval],
    start_ms: i64,
    window_ms: i64,
    range_ms: i64,
    cfg: &CheckConfig,
) -> SampleCheck {
    let end = start_ms + window_ms;
    let frames: Vec<(i64, i64)> = track
        .iter()
        .filter(|c| c.start_ms >= start_ms && c.start_ms < end)
        .map(|c| Interval::new(c.start_ms - audio_start_ms, c.end_ms - audio_start_ms).frames())
        .collect();
    let mut out = SampleCheck { start_ms, cues: frames.len(), offset_ms: 0, prominence: 0.0, support: 0.0 };
    if frames.len() < cfg.min_cues.max(1) {
        return out;
    }
    let w = range_ms / FRAME_MS;
    let curve = span_correlation(audio, &frames, 0, w);
    let best = (0..curve.len()).max_by(|&a, &b| curve[a].total_cmp(&curve[b]).then(b.cmp(&a))).unwrap_or(0);
    let hp = detrend(&curve, (3000 / FRAME_MS) as usize);
    let exclude = (1000 / FRAME_MS) as usize;
    out.offset_ms = (best as i64 - w) * FRAME_MS;
    out.prominence = peak_prominence(&hp, best, exclude);
    let idx = |ms: i64| (ms.div_euclid(FRAME_MS) + w).clamp(0, 2 * w) as usize;
    let tol = cfg.window_tolerance_ms;
    let (lo, hi) = (idx(cfg.expected_lag_ms - tol), idx(cfg.expected_lag_ms + tol));
    let at = (lo..=hi).max_by(|&a, &b| hp[a].total_cmp(&hp[b])).unwrap_or(lo);
    out.support = peak_prominence(&hp, at, exclude);
    out
}

/// Whether one window lines up with the audio within the window tolerance.
fn in_sync(c: &SampleCheck, cfg: &CheckConfig) -> bool {
    (c.offset_ms - cfg.expected_lag_ms).abs() <= cfg.window_tolerance_ms
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleVerdict {
    /// Every sample clearly lines up: use the track.
    Pass,
    /// A sample clearly disagrees, or all line up at an average offset
    /// beyond the tolerance: don't use the track.
    Fail,
    /// No clear answer: check the full audio.
    Inconclusive,
}

/// Combine sample checks. A sample counts only when it has enough cues and
/// a clear correlation peak. Each must be within the window tolerance, and
/// their average within the overall tolerance.
pub fn judge_samples(checks: &[SampleCheck], cfg: &CheckConfig) -> SampleVerdict {
    let clear = |c: &SampleCheck| c.cues >= cfg.min_cues && c.prominence >= cfg.min_prominence;
    if checks.iter().any(|c| clear(c) && !in_sync(c, cfg)) {
        return SampleVerdict::Fail;
    }
    if checks.is_empty() || !checks.iter().all(clear) {
        return SampleVerdict::Inconclusive;
    }
    if (mean_offset(checks) - cfg.expected_lag_ms).abs() > cfg.tolerance_ms {
        SampleVerdict::Fail
    } else {
        SampleVerdict::Pass
    }
}

/// Average offset of some window checks, ms.
pub fn mean_offset(checks: &[SampleCheck]) -> i64 {
    if checks.is_empty() { 0 } else { checks.iter().map(|c| c.offset_ms).sum::<i64>() / checks.len() as i64 }
}

/// The track checked against the full audio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FullCheck {
    pub accepted: bool,
    pub offset_ms: i64,
    pub ratio: f64,
    pub splits: usize,
    /// Largest timing error of any segment of the alignment (the shift it
    /// would need, minus the expected lag).
    pub max_error_ms: i64,
    pub confidence: f64,
    /// Windows of the track with enough cues to check on their own.
    pub windows: usize,
    /// Windows that clearly line up at a different offset.
    pub disagreeing: Vec<SampleCheck>,
    /// Longest run of consecutive windows that don't support the track.
    pub unsupported_run: usize,
    /// Share of the checked cues in unsupported windows.
    pub unsupported_fraction: f64,
    /// Whether the window covering the end of the track supports it.
    pub end_supported: bool,
    /// Every window checked (diagnostics).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub scan: Vec<SampleCheck>,
}

/// Check the whole track against the full speech timeline, two ways:
///
/// - align it like any subtitle: it must need no framerate change, no
///   segment may be off by more than the tolerance, and the alignment must
///   be confident;
/// - check each `window_ms` stretch of it on its own over ±`scan_range_ms`:
///   none may clearly line up somewhere else, and the stretches must back
///   up the in-sync offset (no run of unsupported windows, few unsupported
///   cues overall, and the final stretch supported). The aligner prefers
///   not to split, so a stretch that is off can hide in its result.
pub fn check_full(audio: &Timeline, track: &[Interval], align_cfg: &AlignConfig, cfg: &CheckConfig) -> FullCheck {
    let a = align::align(audio, track, align_cfg);
    let max_error_ms = a.segments.iter().map(|s| (s.offset_ms - cfg.expected_lag_ms).abs()).max().unwrap_or(0);
    let scan = scan(audio, track, cfg);
    let windows = scan.len();
    let mut disagreeing: Vec<SampleCheck> = scan.iter().copied().filter(|c| lines_up_elsewhere(c, cfg)).collect();
    disagreeing.reverse();
    let (mut run, mut unsupported_run, mut unsupported_cues) = (0, 0, 0);
    for c in &scan {
        if c.support < cfg.scan_min_support {
            run += 1;
            unsupported_cues += c.cues;
        } else {
            run = 0;
        }
        unsupported_run = unsupported_run.max(run);
    }
    let total_cues: usize = scan.iter().map(|c| c.cues).sum();
    // Nothing comes after the final stretch to show a cut inside it, so it
    // must back the track up on its own.
    let end_supported = scan.last().is_some_and(|c| c.support >= cfg.scan_min_support);
    let unsupported_fraction = if total_cues == 0 { 1.0 } else { unsupported_cues as f64 / total_cues as f64 };
    let accepted = !track.is_empty()
        && a.mapping.ratio == 1.0
        && max_error_ms <= cfg.tolerance_ms
        && a.confidence >= cfg.min_confidence
        && disagreeing.is_empty()
        && unsupported_run < cfg.scan_max_unsupported_run
        && unsupported_fraction <= cfg.scan_max_unsupported_fraction
        && end_supported;
    FullCheck {
        accepted,
        offset_ms: a.global.offset_ms,
        ratio: a.mapping.ratio,
        splits: a.splits(),
        max_error_ms,
        confidence: a.confidence,
        windows,
        disagreeing,
        unsupported_run,
        unsupported_fraction,
        end_supported,
        scan,
    }
}

/// Each `window_ms` stretch of `cues` checked on its own against `audio`,
/// over ±`scan_range_ms`, in time order. The windows are laid out backwards
/// from the last cue, so the final one covers the end in full; windows with
/// fewer than `min_cues` cues are left out.
pub fn scan(audio: &Timeline, cues: &[Interval], cfg: &CheckConfig) -> Vec<SampleCheck> {
    let mut out = Vec::new();
    let first = cues.iter().map(|c| c.start_ms).min();
    let last = cues.iter().map(|c| c.start_ms).max();
    if let (Some(first), Some(last)) = (first, last)
        && audio.active_fraction() >= 0.005
        && cfg.window_ms > 0
    {
        let mut start = last + 1 - cfg.window_ms;
        while start + cfg.window_ms > first {
            let c = window_check(audio, 0, cues, start, cfg.window_ms, cfg.scan_range_ms, cfg);
            if c.cues >= cfg.min_cues {
                out.push(c);
            }
            start -= cfg.window_ms;
        }
        out.reverse();
    }
    out
}

/// Whether a window clearly lines up at another offset than the in-sync one.
pub fn lines_up_elsewhere(c: &SampleCheck, cfg: &CheckConfig) -> bool {
    c.prominence >= cfg.scan_min_prominence && !in_sync(c, cfg)
}

/// A region of the track that one segment of a split result relies on,
/// checked against the audio.
///
/// When syncing a subtitle against the track splits it into segments with
/// different shifts, either the subtitle or the track has a cut there. If it
/// is the track, the track's cues in the segment's region line up with the
/// audio at an alternative offset: the difference between the shifts of the
/// other segments and this one. So the track's region is scored at the
/// in-sync offset and at each alternative, and the in-sync offset must win.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RegionCheck {
    pub from_ms: i64,
    pub to_ms: i64,
    /// Track cues checked (a `window_ms` stretch of the region at most).
    pub cues: usize,
    /// Correlation at the in-sync offset, plain and detrended (the
    /// detrended score ignores how dense the dialogue is around it).
    pub in_sync_score: f64,
    pub in_sync_detrended: f64,
    /// Best-scoring alternative offset (measured, like the in-sync one) and
    /// its scores.
    pub alt_offset_ms: i64,
    pub alt_score: f64,
    pub alt_detrended: f64,
    /// The in-sync offset beats every alternative on both scores.
    pub supported: bool,
}

/// Alternative offsets for each segment of a split result, from the
/// segments' shifts: for segment `k`, `shift[j] - shift[k]` for every other
/// distinct shift.
pub fn region_alternatives(shifts: &[i64], k: usize) -> Vec<i64> {
    let mut alts: Vec<i64> = shifts.iter().map(|&o| o - shifts[k]).filter(|&d| d != 0).collect();
    alts.sort_unstable();
    alts.dedup();
    alts
}

/// The audio sample to check `[from_ms, to_ms]` of the track: the
/// `window_ms` stretch with the most cues (or the whole region if shorter),
/// padded to cover the alternative offsets.
pub fn region_sample(track: &[Interval], from_ms: i64, to_ms: i64, alternatives: &[i64], cfg: &CheckConfig) -> Sample {
    let w = cfg.window_ms.min((to_ms - from_ms).max(0) + 1);
    let mut starts: Vec<i64> = track.iter().map(|c| c.start_ms).collect();
    starts.sort_unstable();
    let cues_in = |s: i64| starts.partition_point(|&x| x < s + w) - starts.partition_point(|&x| x < s);
    let last = (to_ms + 1 - w).max(from_ms);
    let start = (from_ms..=last).step_by(1000).max_by_key(|&s| (cues_in(s), std::cmp::Reverse(s))).unwrap_or(from_ms);
    let reach = alternatives.iter().map(|a| a.abs()).max().unwrap_or(0) + cfg.expected_lag_ms.abs() + cfg.tolerance_ms;
    let pad = reach + 5_000;
    let audio_start_ms = (start - pad).max(0);
    Sample { start_ms: start, window_ms: w, audio_start_ms, audio_len_ms: start + w + pad - audio_start_ms }
}

/// Score the track's cues in `sample` (`audio` frame 0 is
/// `sample.audio_start_ms`) at the in-sync offset and at each alternative.
pub fn check_region(
    audio: &Timeline,
    track: &[Interval],
    sample: &Sample,
    alternatives: &[i64],
    cfg: &CheckConfig,
) -> RegionCheck {
    let end = sample.start_ms + sample.window_ms;
    let frames: Vec<(i64, i64)> = track
        .iter()
        .filter(|c| c.start_ms >= sample.start_ms && c.start_ms < end)
        .map(|c| Interval::new(c.start_ms - sample.audio_start_ms, c.end_ms - sample.audio_start_ms).frames())
        .collect();
    let mut out = RegionCheck {
        from_ms: sample.start_ms,
        to_ms: end,
        cues: frames.len(),
        in_sync_score: 0.0,
        in_sync_detrended: 0.0,
        alt_offset_ms: 0,
        alt_score: 0.0,
        alt_detrended: 0.0,
        supported: false,
    };
    if frames.len() < cfg.min_region_cues.max(1) {
        return out;
    }
    let reach = alternatives.iter().map(|a| a.abs()).max().unwrap_or(0) + cfg.expected_lag_ms.abs() + cfg.tolerance_ms;
    let w = reach / FRAME_MS + 1;
    let raw = span_correlation(audio, &frames, 0, w);
    let hp = detrend(&raw, (3000 / FRAME_MS) as usize);
    // Best score with the measured offset within `half` of `ms`.
    let best_near = |curve: &[f64], ms: i64, half: i64| {
        let idx = |x: i64| (x.div_euclid(FRAME_MS) + w).clamp(0, 2 * w) as usize;
        (idx(ms - half)..=idx(ms + half)).map(|i| curve[i]).fold(f64::NEG_INFINITY, f64::max)
    };
    let lag = cfg.expected_lag_ms;
    out.in_sync_score = best_near(&raw, lag, cfg.tolerance_ms);
    out.in_sync_detrended = best_near(&hp, lag, cfg.tolerance_ms);
    out.supported = true;
    let mut best_alt = f64::NEG_INFINITY;
    for &a in alternatives {
        // Ranges around the two offsets must not overlap.
        let half = cfg.tolerance_ms.min(a.abs() / 2 - FRAME_MS).max(0);
        let at = lag + a;
        let (r, d) = (best_near(&raw, at, half), best_near(&hp, at, half));
        let (ri, di) = (best_near(&raw, lag, half), best_near(&hp, lag, half));
        if r >= ri || d >= di {
            out.supported = false;
        }
        if r > best_alt {
            best_alt = r;
            (out.alt_offset_ms, out.alt_score, out.alt_detrended) = (at, r, d);
        }
    }
    out
}

/// The part of `audio` a sample covers, as its own timeline (frame 0 =
/// `sample.audio_start_ms`). Used where the full timeline is at hand, such
/// as the evaluation harness.
pub fn slice_for(audio: &Timeline, sample: &Sample) -> Timeline {
    let n = audio.len();
    let a = ((sample.audio_start_ms / FRAME_MS).max(0) as usize).min(n);
    let b = (((sample.audio_start_ms + sample.audio_len_ms) / FRAME_MS).max(0) as usize).min(n);
    Timeline::from_frames(audio.frames()[a..b].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic dialogue-like cues: 1.5-3.5 s lines with 0.5-4 s gaps.
    fn cues(len_ms: i64, seed: u64) -> Vec<Interval> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = |lo: i64, hi: i64| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            lo + ((x >> 33) % (hi - lo) as u64) as i64
        };
        let mut out = Vec::new();
        let mut t = next(1_000, 5_000);
        while t < len_ms - 5_000 {
            let d = next(1_500, 3_500);
            out.push(Interval::new(t, t + d));
            t += d + next(500, 4_000);
        }
        out
    }

    fn shifted(c: &[Interval], by: i64) -> Vec<Interval> {
        c.iter().map(|c| Interval::new(c.start_ms + by, c.end_ms + by)).collect()
    }

    #[test]
    fn samples_are_spread_and_padded() {
        let len = 45 * 60_000;
        let c = cues(len, 1);
        let cfg = CheckConfig::default();
        let s = plan_samples(&c, len, &cfg);
        assert_eq!(s.len(), 3);
        for x in &s {
            assert!(x.audio_start_ms >= 0 && x.audio_start_ms + x.audio_len_ms <= len, "{x:?}");
        }
        for p in s.windows(2) {
            assert!(p[0].start_ms + p[0].window_ms <= p[1].start_ms, "{s:?}");
        }
        assert!(s[0].start_ms < len / 3 && s[1].start_ms < 2 * len / 3, "{s:?}");
        assert!(plan_samples(&c, 10 * 60_000, &cfg).is_empty());
        assert!(plan_samples(&[], len, &cfg).is_empty());
    }

    #[test]
    fn last_sample_covers_the_end_of_the_dialogue() {
        // Dialogue stops 8 minutes before the end (credits).
        let len = 30 * 60_000;
        let end = len - 8 * 60_000;
        let c: Vec<Interval> = cues(len, 2).into_iter().filter(|c| c.end_ms < end).collect();
        let last_end = c.last().unwrap().end_ms;
        let s = plan_samples(&c, len, &CheckConfig::default());
        let last = s.last().unwrap();
        assert!(last.start_ms <= last_end && last_end <= last.start_ms + last.window_ms + 1_000, "{last:?}");
    }

    #[test]
    fn sample_finds_offset() {
        let len = 30 * 60_000;
        let truth = cues(len, 3);
        let audio = Timeline::from_intervals(shifted(&truth, 100), Some((len / FRAME_MS) as usize));
        let cfg = CheckConfig::default();
        let plan = plan_samples(&truth, len, &cfg);
        for by in [0, 5_000, -3_000] {
            let track = shifted(&truth, by);
            let checks: Vec<SampleCheck> =
                plan.iter().map(|s| check_sample(&slice_for(&audio, s), &track, s, &cfg)).collect();
            for c in &checks {
                assert!((c.offset_ms - (100 - by)).abs() <= 20, "{by}: {c:?}");
                assert!(c.prominence >= cfg.min_prominence, "{by}: {c:?}");
            }
            let want = if by == 0 { SampleVerdict::Pass } else { SampleVerdict::Fail };
            assert_eq!(judge_samples(&checks, &cfg), want, "{by}");
        }
    }

    #[test]
    fn sample_without_cues_is_inconclusive() {
        let cfg = CheckConfig::default();
        let s = Sample { start_ms: 60_000, window_ms: 120_000, audio_start_ms: 45_000, audio_len_ms: 150_000 };
        let audio = Timeline::from_intervals([Interval::new(1_000, 9_000)], Some(15_000));
        let c = check_sample(&audio, &[], &s, &cfg);
        assert_eq!(c.cues, 0);
        assert_eq!(judge_samples(&[c], &cfg), SampleVerdict::Inconclusive);
        assert_eq!(judge_samples(&[], &cfg), SampleVerdict::Inconclusive);
    }

    #[test]
    fn samples_must_agree_on_average() {
        let cfg = CheckConfig::default();
        let c = |offset_ms| SampleCheck { start_ms: 0, cues: 30, offset_ms, prominence: 8.0, support: 8.0 };
        // Each within the window tolerance, the average too.
        assert_eq!(judge_samples(&[c(-100), c(250), c(150)], &cfg), SampleVerdict::Pass);
        // Each within the window tolerance, but consistently ~0.35 s off.
        assert_eq!(judge_samples(&[c(420), c(450), c(480)], &cfg), SampleVerdict::Fail);
        // One clearly elsewhere.
        assert_eq!(judge_samples(&[c(100), c(4_100), c(100)], &cfg), SampleVerdict::Fail);
        // One unclear.
        let weak = SampleCheck { prominence: 2.0, ..c(100) };
        assert_eq!(judge_samples(&[c(100), weak, c(100)], &cfg), SampleVerdict::Inconclusive);
    }

    #[test]
    fn region_check_tells_track_cut_from_subtitle_cut() {
        let len = 30 * 60_000;
        let truth = cues(len, 5);
        let audio = Timeline::from_intervals(shifted(&truth, 100), Some((len / FRAME_MS) as usize));
        let cfg = CheckConfig::default();
        // The track's last 3 minutes are 20 s early (it lacks a chunk the
        // video has). Syncing a correct subtitle against it splits there
        // with shifts [0, -20000].
        let cut = len - 180_000;
        let bad: Vec<Interval> = truth
            .iter()
            .filter(|c| !(c.start_ms >= cut && c.start_ms < cut + 20_000))
            .map(
                |c| if c.start_ms >= cut + 20_000 { Interval::new(c.start_ms - 20_000, c.end_ms - 20_000) } else { *c },
            )
            .collect();
        let shifts = [0, -20_000];
        let check = |track: &[Interval], k: usize, from: i64, to: i64| {
            let alts = region_alternatives(&shifts, k);
            let s = region_sample(track, from, to, &alts, &cfg);
            check_region(&slice_for(&audio, &s), track, &s, &alts, &cfg)
        };
        assert!(check(&bad, 0, 0, cut).supported);
        let tail = check(&bad, 1, cut, len - 20_000);
        assert!(!tail.supported, "{tail:?}");
        // A good track, with the subtitle being the one with a cut: both
        // regions of the good track hold up.
        assert!(check(&truth, 0, 0, cut).supported);
        assert!(check(&truth, 1, cut, len).supported);
        assert_eq!(region_alternatives(&[0, -20_000, 0], 1), vec![20_000]);
    }

    #[test]
    fn scan_finds_a_stretch_that_lines_up_elsewhere() {
        let len = 30 * 60_000;
        let truth = cues(len, 6);
        let audio = Timeline::from_intervals(truth.iter().copied(), Some((len / FRAME_MS) as usize));
        let cfg = CheckConfig { expected_lag_ms: 0, ..CheckConfig::default() };
        let scan = scan(&audio, &truth, &cfg);
        assert!(scan.len() >= 12, "{} windows", scan.len());
        assert!(scan.iter().all(|w| !lines_up_elsewhere(w, &cfg)), "{scan:?}");
        // Minutes 10 to 14 are 3 s early.
        let off: Vec<Interval> = truth
            .iter()
            .map(|c| {
                if (600_000..840_000).contains(&c.start_ms) {
                    Interval::new(c.start_ms - 3_000, c.end_ms - 3_000)
                } else {
                    *c
                }
            })
            .collect();
        let bad: Vec<SampleCheck> = scan_windows_off(&audio, &off, &cfg);
        assert!(!bad.is_empty() && bad.iter().all(|w| (w.offset_ms - 3_000).abs() <= 50), "{bad:?}");
        assert!(bad.iter().all(|w| w.start_ms > 540_000 && w.start_ms < 840_000), "{bad:?}");
    }

    fn scan_windows_off(audio: &Timeline, cues: &[Interval], cfg: &CheckConfig) -> Vec<SampleCheck> {
        scan(audio, cues, cfg).into_iter().filter(|w| lines_up_elsewhere(w, cfg)).collect()
    }

    #[test]
    fn full_check_accepts_in_sync_and_rejects_offset() {
        let len = 30 * 60_000;
        let truth = cues(len, 4);
        let audio = Timeline::from_intervals(shifted(&truth, 100), Some((len / FRAME_MS) as usize));
        let cfg = CheckConfig::default();
        let a = AlignConfig::default();
        assert!(check_full(&audio, &truth, &a, &cfg).accepted);
        let bad = check_full(&audio, &shifted(&truth, 5_000), &a, &cfg);
        assert!(!bad.accepted);
        assert_eq!(bad.max_error_ms, 5_000);
    }
}
