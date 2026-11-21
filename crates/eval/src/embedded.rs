//! Evaluation of the embedded-track check (`subsyncer_core::verify`).
//!
//! The perturbed cues play the embedded track and the synthetic speech
//! timeline plays the audio. A track is bad if any cue is off by more than
//! the tolerance or has no place in the video. The check must not accept a
//! bad track; rejecting a good one only costs some precision, because the
//! audio is used instead.

use std::fmt::Write as _;

use serde::Serialize;
use subsyncer_core::align::AlignConfig;
use subsyncer_core::timeline::Timeline;
use subsyncer_core::verify::{self, CheckConfig, FullCheck, SampleCheck, SampleVerdict};

use subsyncer_core::gate::GateConfig;
use subsyncer_core::timeline::{FRAME_MS, Interval};

use crate::perturb::{Kind, Perturbed};
use crate::synth::Rng;
use crate::{SyntheticOptions, evaluate_case, for_each_case};

#[derive(Clone, Debug, Serialize)]
pub struct CheckCase {
    pub seed: u64,
    pub kind: Kind,
    pub desc: String,
    pub len_ms: i64,
    /// Largest cue error in the track; `i64::MAX` if some cue has no place
    /// in the video.
    pub max_err_ms: i64,
    pub bad: bool,
    pub samples: Vec<SampleCheck>,
    /// `None` when the video is too short to sample.
    pub sample_verdict: Option<SampleVerdict>,
    pub full: FullCheck,
    /// Final decision: samples pass → accept, samples fail → reject,
    /// otherwise the full check decides.
    pub accepted: bool,
    /// The full audio had to be analysed (for the full check or, after a
    /// rejection, for syncing against the audio).
    pub full_audio: bool,
    /// When the samples or the full check accept the track: lines of a
    /// correct subtitle (a jittered copy of the true cues) that end up more
    /// than 200 ms off after syncing against the track, out of
    /// `target_cues`. Only counts as harm if the region checks pass too.
    pub harmed: usize,
    pub target_cues: usize,
    /// The synced result had splits and a segment's region of the track
    /// didn't hold up against the audio, so the track was dropped after all.
    pub region_rejected: bool,
    pub regions: Vec<verify::RegionCheck>,
    /// Good tracks only: a subtitle with a cut of its own synced against the
    /// track. The result should split, and the region checks should pass.
    pub cut_target: Option<CutTarget>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CutTarget {
    pub desc: String,
    pub split: bool,
    pub regions: Vec<verify::RegionCheck>,
    pub regions_ok: bool,
}

pub fn run(opts: &SyntheticOptions, align_cfg: &AlignConfig, cfg: &CheckConfig) -> Vec<CheckCase> {
    for_each_case(opts, |seed, ep, p| check_case(seed, &ep.reference, p, align_cfg, cfg))
}

pub fn check_case(seed: u64, audio: &Timeline, p: &Perturbed, align_cfg: &AlignConfig, cfg: &CheckConfig) -> CheckCase {
    let max_err_ms = p
        .cues
        .iter()
        .zip(&p.truth)
        .map(|(c, t)| t.map_or(i64::MAX, |t| (c.start_ms - t.start_ms).abs()))
        .max()
        .unwrap_or(0);
    let len_ms = audio.duration_ms();
    let plan = verify::plan_samples(&p.cues, len_ms, cfg);
    let samples: Vec<SampleCheck> =
        plan.iter().map(|s| verify::check_sample(&verify::slice_for(audio, s), &p.cues, s, cfg)).collect();
    let sample_verdict = (!plan.is_empty()).then(|| verify::judge_samples(&samples, cfg));
    let full = verify::check_full(audio, &p.cues, align_cfg, cfg);
    let accepted = match sample_verdict {
        Some(SampleVerdict::Pass) => true,
        Some(SampleVerdict::Fail) => false,
        _ => full.accepted,
    };
    let (mut harmed, mut target_cues, mut region_rejected, mut regions) = (0, 0, false, Vec::new());
    let mut cut_target = None;
    if accepted {
        let truth: Vec<Interval> = p.truth.iter().flatten().copied().collect();
        let target = jittered(seed, &truth, None);
        let (h, n, r) = harm(audio, &p.cues, &target, align_cfg, cfg);
        region_rejected = !r.iter().all(|r| r.supported);
        (harmed, target_cues) = (h, n);
        regions = r;
        if max_err_ms <= cfg.tolerance_ms {
            let mut rng = Rng::new(seed ^ 0xC0FFEE);
            let at = rng.range(len_ms * 3 / 10, len_ms * 7 / 10);
            let by = if rng.chance(0.5) { 1 } else { -1 } * rng.range(5_000, 30_000);
            let target = jittered(seed, &truth, Some((at, by)));
            let (_, _, r) = harm(audio, &p.cues, &target, align_cfg, cfg);
            cut_target = Some(CutTarget {
                desc: format!("subtitle cues after {at} ms shifted {by} ms"),
                split: !r.is_empty(),
                regions_ok: r.iter().all(|r| r.supported),
                regions: r,
            });
        }
    }
    let accepted = accepted && !region_rejected;
    CheckCase {
        seed,
        kind: p.kind,
        desc: p.desc.clone(),
        len_ms,
        max_err_ms,
        bad: max_err_ms > cfg.tolerance_ms,
        samples,
        sample_verdict,
        full,
        accepted,
        full_audio: sample_verdict != Some(SampleVerdict::Pass),
        harmed,
        target_cues,
        region_rejected,
        regions,
        cut_target,
    }
}

/// A subtitle for the same video: the true cues with jittered boundaries
/// and some lines dropped, standing in for another language's subtitle of
/// the same release. `cut` shifts the cues after a point, as in a subtitle
/// made for a different cut.
fn jittered(seed: u64, truth: &[Interval], cut: Option<(i64, i64)>) -> Vec<Interval> {
    let mut rng = Rng::new(seed ^ 0x5EED_7A26);
    let mut target: Vec<Interval> = Vec::new();
    for t in truth {
        if rng.chance(0.1) {
            continue;
        }
        let d = cut.map_or(0, |(at, by)| if t.start_ms >= at { by } else { 0 });
        let s = t.start_ms + rng.range(-150, 151) + d;
        let e = (t.end_ms + rng.range(-150, 151)).max(s - d + 300) + d;
        target.push(Interval::new(s, e));
    }
    target.retain(|c| c.start_ms >= 0);
    target.sort();
    target
}

/// Sync `target` against the track as reference. Returns the lines that end
/// up more than 200 ms from where `target` had them (`target` is correct for
/// the video when it has no cut) and, for a split result, the region checks.
fn harm(
    audio: &Timeline,
    track: &[Interval],
    target: &[Interval],
    align_cfg: &AlignConfig,
    cfg: &CheckConfig,
) -> (usize, usize, Vec<verify::RegionCheck>) {
    let len = track.iter().map(|c| c.end_ms).max().unwrap_or(0).max(target.last().map_or(0, |c| c.end_ms));
    let reference = Timeline::from_intervals(track.iter().copied(), Some((len / FRAME_MS) as usize + 1));
    let p = Perturbed {
        kind: Kind::None,
        desc: String::new(),
        truth: target.iter().copied().map(Some).collect(),
        cues: target.to_vec(),
    };
    let r = evaluate_case("harm", &reference, &p, align_cfg, &GateConfig::default());
    let a = subsyncer_core::align::align(&reference, &p.cues, align_cfg);
    let shifts: Vec<i64> = a.segments.iter().map(|s| s.offset_ms).collect();
    let regions: Vec<verify::RegionCheck> = if a.segments.len() < 2 {
        Vec::new()
    } else {
        a.segments
            .iter()
            .enumerate()
            .map(|(k, seg)| {
                let mapped = &a.cues[seg.first_cue..=seg.last_cue];
                let from = mapped.iter().map(|c| c.start_ms).min().unwrap_or(0);
                let to = mapped.iter().map(|c| c.end_ms).max().unwrap_or(0);
                let alts = verify::region_alternatives(&shifts, k);
                let s = verify::region_sample(track, from, to, &alts, cfg);
                verify::check_region(&verify::slice_for(audio, &s), track, &s, &alts, cfg)
            })
            .collect()
    };
    (r.n_truth - r.within, r.n_truth, regions)
}

/// Lowest sample prominence if every sample is in sync (the sample
/// threshold at or below which the samples would pass), else `None`.
fn pass_level(c: &CheckCase, cfg: &CheckConfig) -> Option<f64> {
    if c.samples.is_empty() {
        return None;
    }
    let all_in_sync = c
        .samples
        .iter()
        .all(|s| s.cues >= cfg.min_cues && (s.offset_ms - cfg.expected_lag_ms).abs() <= cfg.window_tolerance_ms)
        && (verify::mean_offset(&c.samples) - cfg.expected_lag_ms).abs() <= cfg.tolerance_ms;
    all_in_sync.then(|| c.samples.iter().map(|s| s.prominence).fold(f64::INFINITY, f64::min))
}

pub fn table(cases: &[CheckCase], cfg: &CheckConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "| kind | cases | bad | samples pass/fail/unsure | full check ok | dropped by region check | accepted | false accept | false reject | full audio | lines wrong after sync |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|");
    let mut kinds: Vec<Kind> = cases.iter().map(|c| c.kind).collect();
    kinds.sort();
    kinds.dedup();
    let row = |name: &str, v: Vec<&CheckCase>, out: &mut String| {
        let n = v.len();
        let count = |f: &dyn Fn(&CheckCase) -> bool| v.iter().filter(|c| f(c)).count();
        let pct = |k: usize| if n == 0 { 0.0 } else { 100.0 * k as f64 / n as f64 };
        let _ = writeln!(
            out,
            "| {name} | {n} | {} | {}/{}/{} | {} | {} | {} | {} | {} | {:.0}% | {} |",
            count(&|c| c.bad),
            count(&|c| c.sample_verdict == Some(SampleVerdict::Pass)),
            count(&|c| c.sample_verdict == Some(SampleVerdict::Fail)),
            count(&|c| c.sample_verdict == Some(SampleVerdict::Inconclusive)),
            count(&|c| c.full.accepted),
            count(&|c| c.region_rejected),
            count(&|c| c.accepted),
            count(&|c| c.accepted && c.bad),
            count(&|c| !c.accepted && !c.bad),
            pct(count(&|c| c.full_audio || c.region_rejected)),
            {
                let h: usize = v.iter().filter(|c| c.accepted).map(|c| c.harmed).sum();
                let t: usize = v.iter().filter(|c| c.accepted).map(|c| c.target_cues).sum();
                let worst = v.iter().filter(|c| c.accepted).map(|c| c.harmed).max().unwrap_or(0);
                if t == 0 {
                    "-".to_string()
                } else {
                    format!("{:.2}% (worst case {worst})", 100.0 * h as f64 / t as f64)
                }
            },
        );
    };
    for k in kinds {
        row(k.name(), cases.iter().filter(|c| c.kind == k).collect(), &mut out);
    }
    row("**all**", cases.iter().collect(), &mut out);

    // How the sample threshold trades good tracks passing (saving the full
    // decode) against bad tracks passing (wrong reference).
    let _ = writeln!(out, "\n| sample threshold | good tracks passing samples | bad tracks passing samples |");
    let _ = writeln!(out, "|---|---|---|");
    let good: Vec<Option<f64>> = cases.iter().filter(|c| !c.bad).map(|c| pass_level(c, cfg)).collect();
    let bad: Vec<Option<f64>> = cases.iter().filter(|c| c.bad).map(|c| pass_level(c, cfg)).collect();
    for t in [3.0, 3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 7.0, 8.0] {
        let g = good.iter().filter(|l| l.is_some_and(|l| l >= t)).count();
        let b = bad.iter().filter(|l| l.is_some_and(|l| l >= t)).count();
        let _ = writeln!(
            out,
            "| {t:.1} | {g}/{} ({:.0}%) | {b}/{} |",
            good.len(),
            if good.is_empty() { 0.0 } else { 100.0 * g as f64 / good.len() as f64 },
            bad.len()
        );
    }
    out
}
