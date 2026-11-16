//! Compare confidence measures on correct vs wrong-video alignments.
use subsyncer_core::align::{self, AlignConfig, DEFAULT_RATIOS, scale};
use subsyncer_core::timeline::{FRAME_MS, Interval, Timeline};
use subsyncer_eval::{perturb, synth};

fn curve(reference: &Timeline, cues: &[Interval], k: usize) -> Vec<f64> {
    // direct (slow but simple) normalized correlation over ±k frames using prefix sums of ±1 ref
    let nr = reference.len() as i64;
    let p: Vec<i64> = std::iter::once(0)
        .chain(reference.frames().iter().scan(0i64, |a, &b| {
            *a += if b { 1 } else { -1 };
            Some(*a)
        }))
        .collect();
    let q = |x: i64| -> i64 { p[x.clamp(0, nr) as usize] };
    let frames: Vec<(i64, i64)> = cues.iter().map(|c| c.frames()).collect();
    let ns = frames.iter().map(|f| f.1).max().unwrap_or(0);
    let total_ref = |lo: i64, hi: i64| q(hi) - q(lo);
    (0..=2 * k)
        .map(|j| {
            let d = j as i64 - k as i64;
            // sum over sub frames t in [0,ns): s[t]*r[t+d]; s = +1 in cues, -1 else.
            let all = total_ref(d, ns + d); // sum r over window (as if s=-1 everywhere, negated)
            let incue: i64 = frames.iter().map(|&(a, b)| total_ref(a + d, b + d)).sum();
            let raw = 2 * incue - all;
            let lo = 0i64.max(-d);
            let hi = ns.min(nr - d);
            raw as f64 / (hi - lo).max(1) as f64
        })
        .collect()
}

fn z_excl(c: &[f64], peak: usize, excl: usize, lo: usize, hi: usize) -> f64 {
    let v: Vec<f64> = (lo..hi).filter(|&j| j.abs_diff(peak) > excl).map(|j| c[j]).collect();
    let m = v.iter().sum::<f64>() / v.len() as f64;
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    (c[peak] - m) / sd.max(1e-12)
}

fn main() {
    let cfg = AlignConfig::default();
    let k = (cfg.max_offset_ms / FRAME_MS) as usize;
    println!("kind seed z_global z_local30 z_hp10 z_hp3 correct");
    for seed in 0..60u64 {
        for kind in [perturb::Kind::Offset, perturb::Kind::WrongVideo] {
            let mut rng = synth::Rng::new(seed + 5000);
            let len = rng.range(20 * 60_000, 50 * 60_000);
            let noise = if seed % 2 == 0 { synth::NoiseModel::default() } else { synth::NoiseModel::hard() };
            let ep = synth::episode(&mut rng, len, &noise);
            let mut reference = ep.reference.clone();
            if kind == perturb::Kind::WrongVideo {
                let mut o = synth::Rng::new(seed + 99_999);
                let l2 = o.range(20 * 60_000, 50 * 60_000);
                reference = synth::episode(&mut o, l2, &noise).reference;
            }
            let mut prng = synth::Rng::new(seed);
            let p = perturb::perturb(perturb::Kind::Offset, &ep.cues, ep.len_ms, &mut prng);
            let _ = (DEFAULT_RATIOS, scale(0, 1.0));
            let c = curve(&reference, &p.cues, k);
            let peak = c.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
            let zg = z_excl(&c, peak, 100, 0, c.len());
            let zl = z_excl(&c, peak, 100, peak.saturating_sub(3000), (peak + 3000).min(c.len()));
            let hp = |w: usize| -> f64 {
                let n = c.len();
                let pre: Vec<f64> = std::iter::once(0.0)
                    .chain(c.iter().scan(0.0, |a, &x| {
                        *a += x;
                        Some(*a)
                    }))
                    .collect();
                let h: Vec<f64> = (0..n)
                    .map(|j| {
                        let a = j.saturating_sub(w);
                        let b = (j + w + 1).min(n);
                        c[j] - (pre[b] - pre[a]) / (b - a) as f64
                    })
                    .collect();
                z_excl(&h, peak, 100, 0, n)
            };
            let a = align::align(&reference, &p.cues, &cfg);
            let correct = kind != perturb::Kind::WrongVideo
                && a.cues
                    .iter()
                    .zip(&p.truth)
                    .filter(|(x, t)| t.map(|t| (x.start_ms - t.start_ms).abs() <= 200).unwrap_or(false))
                    .count()
                    * 100
                    >= p.cues.len() * 95;
            println!("{} {} {:.2} {:.2} {:.2} {:.2} {}", kind.name(), seed, zg, zl, hp(1000), hp(300), correct);
        }
    }
}
