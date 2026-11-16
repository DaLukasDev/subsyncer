//! Evaluation harness for subsyncer: synthetic scenarios (no media, run in
//! CI) and a real-corpus mode.

pub mod perturb;
pub mod report;
pub mod synth;

use subsyncer_core::align::{self, AlignConfig};
use subsyncer_core::gate::{self, GateConfig, Outcome};
use subsyncer_core::timeline::{Interval, Timeline};

use perturb::{Kind, Perturbed};

/// Tolerance for counting a cue as correctly placed.
pub const TOLERANCE_MS: i64 = 200;

#[derive(Clone, Debug, serde::Serialize)]
pub struct CaseResult {
    pub source: String,
    pub kind: Kind,
    pub desc: String,
    pub outcome: Outcome,
    pub written: bool,
    /// Cues with a ground-truth position.
    pub n_truth: usize,
    /// Cues within tolerance in what the user ends up with (input if
    /// nothing was written).
    pub within: usize,
    pub median_err_ms: i64,
    /// Same, for the raw alignment regardless of the gate.
    pub raw_within: usize,
    pub raw_median_err_ms: i64,
    pub splits: usize,
    pub ratio: f64,
    pub offset_ms: i64,
    pub prominence: f64,
    pub score_before: f64,
    pub score_after: f64,
    pub elapsed_ms: u64,
}

impl CaseResult {
    pub fn within_frac(&self) -> f64 {
        if self.n_truth == 0 { 1.0 } else { self.within as f64 / self.n_truth as f64 }
    }
    pub fn raw_within_frac(&self) -> f64 {
        if self.n_truth == 0 { 1.0 } else { self.raw_within as f64 / self.n_truth as f64 }
    }
}

/// (within tolerance, median abs error) of `got` vs `truth` by cue start.
pub fn score_cues(got: &[Interval], truth: &[Option<Interval>]) -> (usize, usize, i64) {
    let mut errs: Vec<i64> =
        got.iter().zip(truth).filter_map(|(g, t)| t.map(|t| (g.start_ms - t.start_ms).abs())).collect();
    let n = errs.len();
    let within = errs.iter().filter(|&&e| e <= TOLERANCE_MS).count();
    errs.sort_unstable();
    let median = if n == 0 { 0 } else { errs[n / 2] };
    (n, within, median)
}

/// Align a perturbed cue list against a reference and score it.
pub fn evaluate_case(
    source: &str,
    reference: &Timeline,
    p: &Perturbed,
    align_cfg: &AlignConfig,
    gate_cfg: &GateConfig,
) -> CaseResult {
    let t = std::time::Instant::now();
    let a = align::align(reference, &p.cues, align_cfg);
    let v = gate::evaluate(reference, &p.cues, &a, gate_cfg);
    let elapsed_ms = t.elapsed().as_millis() as u64;
    let written = v.should_write();
    let effective = if written { &a.cues } else { &p.cues };
    let (n_truth, within, median_err_ms) = score_cues(effective, &p.truth);
    let (_, raw_within, raw_median_err_ms) = score_cues(&a.cues, &p.truth);
    CaseResult {
        source: source.to_string(),
        kind: p.kind,
        desc: p.desc.clone(),
        outcome: v.outcome,
        written,
        n_truth,
        within,
        median_err_ms,
        raw_within,
        raw_median_err_ms,
        splits: v.splits,
        ratio: v.ratio,
        offset_ms: v.offset_ms,
        prominence: v.prominence,
        score_before: v.score_before,
        score_after: v.score_after,
        elapsed_ms,
    }
}

#[derive(Clone, Debug)]
pub struct SyntheticOptions {
    pub seeds: std::ops::Range<u64>,
    pub kinds: Vec<Kind>,
    pub noise: synth::NoiseModel,
    pub threads: usize,
}

/// Run every (seed, kind) case in parallel.
pub fn run_synthetic(opts: &SyntheticOptions, align_cfg: &AlignConfig, gate_cfg: &GateConfig) -> Vec<CaseResult> {
    let jobs: Vec<(u64, Kind)> = opts.seeds.clone().flat_map(|s| opts.kinds.iter().map(move |&k| (s, k))).collect();
    let threads = opts.threads.max(1).min(jobs.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(seed, kind)) = jobs.get(i) else { break };
                    let mut rng = synth::Rng::new(seed);
                    let len_ms = rng.range(20 * 60_000, 50 * 60_000);
                    let mut ep = synth::episode(&mut rng, len_ms, &opts.noise);
                    if kind == Kind::WrongVideo {
                        let mut other = synth::Rng::new(seed.wrapping_add(7919));
                        let other_len = other.range(20 * 60_000, 50 * 60_000);
                        let o = synth::episode(&mut other, other_len, &opts.noise);
                        ep.reference = o.reference;
                    }
                    // Separate stream for the perturbation so every kind sees
                    // the same episode for a given seed.
                    let mut prng = synth::Rng::new(seed ^ (kind as u64 + 1).wrapping_mul(0xA5A5_5A5A));
                    let p = perturb::perturb(kind, &ep.cues, ep.len_ms, &mut prng);
                    let r = evaluate_case(&format!("synthetic:{seed}"), &ep.reference, &p, align_cfg, gate_cfg);
                    results.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut v = results.into_inner().unwrap();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}
