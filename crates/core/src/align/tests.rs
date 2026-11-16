use super::*;

/// Deterministic xorshift for tests.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }
}

/// Speech bursts of 0.5-4 s separated by 0.3-6 s gaps; cues = bursts.
fn synth(len_ms: i64, seed: u64) -> (Timeline, Vec<Interval>) {
    let mut rng = Rng(seed);
    let mut t = rng.range(1000, 5000);
    let mut ivs = Vec::new();
    while t < len_ms - 5000 {
        let d = rng.range(500, 4000);
        ivs.push(Interval::new(t, t + d));
        t += d + rng.range(300, 6000);
    }
    (Timeline::from_intervals(ivs.clone(), Some((len_ms / FRAME_MS) as usize)), ivs)
}

fn shift(ivs: &[Interval], d: i64) -> Vec<Interval> {
    ivs.iter().map(|c| Interval::new(c.start_ms + d, c.end_ms + d)).collect()
}

#[test]
fn global_recovers_offset() {
    let (reference, truth) = synth(20 * 60_000, 7);
    for off in [-23_450i64, -1_000, 0, 370, 12_340] {
        let subs = shift(&truth, -off);
        let g = global_align(&reference, &subs, &AlignConfig::default());
        assert_eq!(g.ratio, 1.0);
        assert!((g.offset_ms - off).abs() <= 10, "offset {off}: got {}", g.offset_ms);
        assert!(g.prominence > 10.0, "prominence {}", g.prominence);
    }
}

#[test]
fn global_recovers_framerate() {
    let (reference, truth) = synth(40 * 60_000, 11);
    // Subtitle timed for 23.976 fps played against 25 fps video: times are
    // too long by 25/23.976, so the fix is ratio 23.976/25.
    let r = 25.0 / 23.976;
    let subs: Vec<Interval> =
        truth.iter().map(|c| Interval::new(scale(c.start_ms, r) + 2000, scale(c.end_ms, r) + 2000)).collect();
    let a = align(&reference, &subs, &AlignConfig::default());
    assert!((a.global.ratio - 23.976 / 25.0).abs() < 1e-9, "ratio {}", a.global.ratio);
    let err: Vec<i64> = a.cues.iter().zip(&truth).map(|(x, y)| (x.start_ms - y.start_ms).abs()).collect();
    assert!(err.iter().all(|&e| e <= 30), "max err {:?}", err.iter().max());
}

#[test]
fn piecewise_recovers_insert() {
    let (reference, truth) = synth(30 * 60_000, 3);
    // Video has an extra 47 s chunk at ~12 min that the subtitle lacks:
    // cues after the chunk are 47 s early in the subtitle.
    let cut = 12 * 60_000;
    let subs: Vec<Interval> = truth
        .iter()
        .filter(|c| !(c.end_ms > cut && c.start_ms < cut + 47_000))
        .map(|c| if c.start_ms >= cut + 47_000 { Interval::new(c.start_ms - 47_000, c.end_ms - 47_000) } else { *c })
        .collect();
    let kept: Vec<Interval> =
        truth.iter().filter(|c| !(c.end_ms > cut && c.start_ms < cut + 47_000)).copied().collect();
    let a = align(&reference, &subs, &AlignConfig::default());
    assert_eq!(a.splits(), 1, "segments {:?}", a.segments);
    let ok = a.cues.iter().zip(&kept).filter(|(x, y)| (x.start_ms - y.start_ms).abs() <= 200).count();
    assert!(ok as f64 / kept.len() as f64 >= 0.98, "{ok}/{}", kept.len());
}

#[test]
fn already_synced_has_no_splits() {
    let (reference, truth) = synth(30 * 60_000, 5);
    let a = align(&reference, &truth, &AlignConfig::default());
    assert_eq!(a.splits(), 0);
    assert_eq!(a.global.offset_ms, 0);
    let v = crate::gate::evaluate(&reference, &truth, &a, &crate::gate::GateConfig::default());
    assert_eq!(v.outcome, crate::gate::Outcome::AlreadySynced);
}

#[test]
fn mapping_segments() {
    let m = Mapping { ratio: 1.0, segments: vec![(i64::MIN, 100), (5000, -200)] };
    assert_eq!(m.apply(0, 10), 110);
    assert_eq!(m.apply(4999, 6000), 6100);
    assert_eq!(m.apply(5000, 5000), 4800);
    assert_eq!(m.splits(), 1);
}

#[test]
fn overlap_score_basics() {
    let reference = Timeline::from_intervals([Interval::new(1000, 2000)], Some(500));
    assert_eq!(overlap_score(&reference, &[Interval::new(1000, 2000)]), 1.0);
    assert_eq!(overlap_score(&reference, &[Interval::new(1500, 2500)]), 0.5);
    assert_eq!(overlap_score(&reference, &[Interval::new(3000, 4000)]), 0.0);
}
