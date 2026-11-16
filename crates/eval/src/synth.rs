//! Synthetic episodes: a true speech track, subtitle cues derived from it
//! with realistic timing habits, and a noisy "VAD" view of the speech.

use subsyncer_core::timeline::{FRAME_MS, Interval, Timeline};

/// Small deterministic PRNG (xorshift64*), so results are reproducible
/// without extra dependencies.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo) as u64) as i64
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
    /// Log-normal with the given median, clamped.
    pub fn lognormal(&mut self, median: f64, sigma: f64, lo: f64, hi: f64) -> f64 {
        // Box-Muller
        let u1 = self.f64().max(1e-12);
        let u2 = self.f64();
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        (median * (sigma * z).exp()).clamp(lo, hi)
    }
}

/// Knobs for how noisy the synthetic data is.
#[derive(Clone, Debug)]
pub struct NoiseModel {
    /// Fraction of utterances the VAD misses entirely.
    pub vad_miss: f64,
    /// False-alarm bursts per minute (music, effects).
    pub vad_false_per_min: f64,
    /// VAD boundary jitter (± ms).
    pub vad_jitter_ms: i64,
    /// Fraction of utterances without a subtitle.
    pub untranslated: f64,
    /// Fraction of extra cues with no speech (sound descriptions).
    pub spurious: f64,
}

impl Default for NoiseModel {
    fn default() -> Self {
        Self { vad_miss: 0.12, vad_false_per_min: 1.0, vad_jitter_ms: 60, untranslated: 0.05, spurious: 0.03 }
    }
}

impl NoiseModel {
    pub fn hard() -> Self {
        Self { vad_miss: 0.30, vad_false_per_min: 3.0, vad_jitter_ms: 120, untranslated: 0.12, spurious: 0.08 }
    }
}

pub struct Episode {
    pub len_ms: i64,
    /// What the VAD reports (the alignment reference).
    pub reference: Timeline,
    /// Correct cue timings for this video, time-sorted.
    pub cues: Vec<Interval>,
}

/// Generate an episode of roughly `len_ms`.
pub fn episode(rng: &mut Rng, len_ms: i64, noise: &NoiseModel) -> Episode {
    // 1. True utterances: dialogue scenes separated by quiet stretches.
    let mut utts: Vec<Interval> = Vec::new();
    let mut t = rng.range(2_000, 30_000);
    while t < len_ms - 10_000 {
        let scene_end = (t + rng.lognormal(150_000.0, 0.6, 30_000.0, 600_000.0) as i64).min(len_ms - 5_000);
        while t < scene_end {
            let d = rng.lognormal(1_800.0, 0.6, 300.0, 9_000.0) as i64;
            utts.push(Interval::new(t, t + d));
            t += d + rng.lognormal(700.0, 0.9, 80.0, 8_000.0) as i64;
        }
        t += rng.lognormal(25_000.0, 0.8, 5_000.0, 120_000.0) as i64;
    }

    // 2. Cues: merge close utterances, split long ones, add lead-in/out.
    let mut cues: Vec<Interval> = Vec::new();
    let mut i = 0;
    while i < utts.len() {
        let mut s = utts[i].start_ms;
        let mut e = utts[i].end_ms;
        while i + 1 < utts.len() && utts[i + 1].start_ms - e < 400 && utts[i + 1].end_ms - s < 6_000 && rng.chance(0.6)
        {
            i += 1;
            e = utts[i].end_ms;
        }
        i += 1;
        if rng.chance(noise.untranslated) {
            continue;
        }
        if e - s > 7_000 {
            let mid = s + (e - s) / 2;
            cues.push(Interval::new(s, mid));
            s = mid + 40;
        }
        let s2 = s - rng.range(0, 250);
        let mut e2 = e + rng.range(0, 600);
        if e2 - s2 < 1_000 {
            e2 = s2 + 1_000;
        }
        cues.push(Interval::new(s2, e2));
    }
    // Sound descriptions / signs: cues where nobody speaks.
    let n_spurious = (cues.len() as f64 * noise.spurious) as usize;
    for _ in 0..n_spurious {
        let s = rng.range(0, len_ms - 3_000);
        cues.push(Interval::new(s, s + rng.range(800, 2_500)));
    }
    cues.sort();
    // No overlaps (subtitle editors enforce a small gap).
    for k in 1..cues.len() {
        if cues[k].start_ms < cues[k - 1].end_ms + 40 {
            cues[k - 1].end_ms = (cues[k].start_ms - 40).max(cues[k - 1].start_ms + 100);
        }
    }
    cues.retain(|c| c.start_ms >= 0 && c.end_ms > c.start_ms);

    // 3. VAD view: misses, false alarms, jitter, slight onset delay.
    let mut vad: Vec<Interval> = Vec::new();
    for u in &utts {
        if rng.chance(noise.vad_miss) {
            continue;
        }
        let j = noise.vad_jitter_ms;
        vad.push(Interval::new(u.start_ms + 40 + rng.range(-j, j + 1), u.end_ms + rng.range(-j, j + 1)));
    }
    let n_false = (len_ms as f64 / 60_000.0 * noise.vad_false_per_min) as usize;
    for _ in 0..n_false {
        let s = rng.range(0, len_ms - 3_000);
        vad.push(Interval::new(s, s + rng.range(300, 2_000)));
    }
    let reference = Timeline::from_intervals(vad, Some((len_ms / FRAME_MS) as usize));
    Episode { len_ms, reference, cues }
}
