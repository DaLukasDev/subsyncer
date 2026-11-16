//! cargo run --release -p subsyncer-eval --example debug_case -- <seed> <kind>
use subsyncer_core::align::{self, AlignConfig};
use subsyncer_eval::{perturb, synth};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seed: u64 = a[1].parse().unwrap();
    let kind = perturb::Kind::parse(&a[2]).unwrap();
    let mut rng = synth::Rng::new(seed);
    let len_ms = rng.range(20 * 60_000, 50 * 60_000);
    let ep = synth::episode(&mut rng, len_ms, &synth::NoiseModel::default());
    let mut prng = synth::Rng::new(seed ^ (kind as u64 + 1).wrapping_mul(0xA5A5_5A5A));
    let p = perturb::perturb(kind, &ep.cues, ep.len_ms, &mut prng);
    let cfg = AlignConfig::default();
    let r = align::align(&ep.reference, &p.cues, &cfg);
    println!("{} | len {} s | cues {}", p.desc, len_ms / 1000, p.cues.len());
    println!(
        "global ratio {} offset {} prom {:.2} peaks {:?}",
        r.global.ratio, r.global.offset_ms, r.global.prominence, r.global.peaks
    );
    for c in &r.global.candidates {
        println!("  cand {:.5} {} {:.4}", c.ratio, c.offset_ms, c.score);
    }
    for s in &r.segments {
        println!(
            "  seg cues {}..={} (from {} ms) offset {}",
            s.first_cue, s.last_cue, p.cues[s.first_cue].start_ms, s.offset_ms
        );
    }
    println!("confidence {:.2}", r.confidence);
}
