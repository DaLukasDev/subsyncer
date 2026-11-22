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
    let m = Mapping { ratio: 1.0, segments: vec![(i64::MIN, 100), (5000, -200)], clips: Vec::new() };
    assert_eq!(m.apply(0, 10), 110);
    assert_eq!(m.apply(4999, 6000), 6100);
    assert_eq!(m.apply(5000, 5000), 4800);
    assert_eq!(m.splits(), 1);
    // The first segment's tail is hidden against the second's start.
    let m = Mapping { clips: vec![(i64::MIN, 4800), (i64::MIN, i64::MAX)], ..m };
    assert_eq!(m.apply(4000, 4500), 4600);
    assert_eq!(m.apply(4900, 4900), 4800);
    assert_eq!(m.apply(4900, 5900), 4800);
    assert_eq!(m.apply(5000, 5000), 4800);
}

#[test]
fn extra_content_is_hidden_and_the_mapping_agrees() {
    let (reference, truth) = synth(30 * 60_000, 9);
    // The subtitle has 30 s of content the video lacks at ~10 min: later
    // cues are 30 s late, and the gap holds lines with no place in the video.
    let (cut, t) = (10 * 60_000, 30_000);
    let mut subs: Vec<Interval> = truth
        .iter()
        .map(|c| if c.start_ms >= cut { Interval::new(c.start_ms + t, c.end_ms + t) } else { *c })
        .collect();
    let extra: Vec<Interval> =
        (0..8).map(|k| Interval::new(cut + 1_000 + k * 3_500, cut + 3_500 + k * 3_500)).collect();
    subs.extend(&extra);
    subs.sort();
    let a = align(&reference, &subs, &AlignConfig::default());
    assert_eq!(a.splits(), 1, "segments {:?}", a.segments);
    // Extra lines that overlap the other side's content are hidden; ones in
    // the silence before it are left where they are.
    let hidden = subs.iter().zip(&a.cues).filter(|(s, c)| extra.contains(s) && c.end_ms <= c.start_ms).count();
    assert!(hidden * 2 >= extra.len(), "{hidden} of {} extra lines hidden", extra.len());
    // Any other timestamp (sound descriptions, inline times) maps the same
    // way as the aligned dialogue, so it is hidden along with it.
    for (s, c) in subs.iter().zip(&a.cues) {
        assert_eq!(
            (a.mapping.apply(s.start_ms, s.start_ms), a.mapping.apply(s.start_ms, s.end_ms)),
            (c.start_ms, c.end_ms)
        );
    }
}

#[test]
fn overlap_score_basics() {
    let reference = Timeline::from_intervals([Interval::new(1000, 2000)], Some(500));
    assert_eq!(overlap_score(&reference, &[Interval::new(1000, 2000)]), 1.0);
    assert_eq!(overlap_score(&reference, &[Interval::new(1500, 2500)]), 0.5);
    assert_eq!(overlap_score(&reference, &[Interval::new(3000, 4000)]), 0.0);
}

/// `truth` with the offset changing in steps: from each `(at_ms, shift_ms)`
/// on, the subtitle is `shift_ms` early (the alignment must add it).
fn stepped(truth: &[Interval], steps: &[(i64, i64)]) -> Vec<Interval> {
    truth
        .iter()
        .map(|c| {
            let d = steps.iter().rev().find(|s| c.start_ms >= s.0).map_or(0, |s| s.1);
            Interval::new(c.start_ms - d, c.end_ms - d)
        })
        .collect()
}

fn within(a: &Alignment, truth: &[Interval], tol_ms: i64) -> f64 {
    let ok = a.cues.iter().zip(truth).filter(|(x, y)| (x.start_ms - y.start_ms).abs() <= tol_ms).count();
    ok as f64 / truth.len() as f64
}

#[test]
fn small_steps_are_found() {
    // Act breaks of other lengths than the subtitle's source: the offset
    // changes by about a second a few times. Steps this small cost too much
    // for the main split search on their own.
    let (reference, truth) = synth(42 * 60_000, 21);
    let subs = stepped(&truth, &[(0, 100), (6 * 60_000, 1_100), (21 * 60_000, 1_500), (33 * 60_000, 900)]);
    let a = align(&reference, &subs, &AlignConfig::default());
    assert_eq!(a.mapping.ratio, 1.0);
    assert!(a.splits() >= 3, "segments {:?}", a.segments);
    assert!(within(&a, &truth, 200) >= 0.98, "{:.3}, segments {:?}", within(&a, &truth, 200), a.segments);
}

#[test]
fn a_slow_drift_stays_a_drift() {
    // Timed for 24 fps, played at 23.976: about 2.5 s of drift over 42
    // minutes, which a few small steps could imitate.
    let (reference, truth) = synth(42 * 60_000, 23);
    let r = 24.0 / 23.976;
    let subs: Vec<Interval> =
        truth.iter().map(|c| Interval::new(scale(c.start_ms, r) - 400, scale(c.end_ms, r) - 400)).collect();
    let a = align(&reference, &subs, &AlignConfig::default());
    assert!((a.mapping.ratio - 23.976 / 24.0).abs() < 1e-9, "ratio {}", a.mapping.ratio);
    assert_eq!(a.splits(), 0, "segments {:?}", a.segments);
    assert!(within(&a, &truth, 30) >= 0.99);
}

#[test]
fn a_small_step_needs_both_sides() {
    let (reference, truth) = synth(30 * 60_000, 5);
    let rs = RefScore::new(&reference, 1.0);
    let cfg = AlignConfig::default();
    let mid = truth.len() / 2;
    let split = |off: i64| {
        vec![
            Segment { first_cue: 0, last_cue: mid - 1, offset_ms: 0 },
            Segment { first_cue: mid, last_cue: truth.len() - 1, offset_ms: off },
        ]
    };
    // A step the cues don't show: merged.
    let (segs, _) = check_segments(&reference, &rs, &truth, split(600), &cfg);
    assert_eq!(segs.len(), 1, "{segs:?}");
    assert!(segs[0].offset_ms.abs() <= 100, "{segs:?}");
    // A real one stays.
    let subs = stepped(&truth, &[(truth[mid].start_ms, 600)]);
    let (segs, _) = check_segments(&reference, &rs, &subs, split(600), &cfg);
    assert_eq!(segs, split(600));
}

#[test]
fn a_low_split_penalty_keeps_the_splits() {
    // A lower penalty must not end up with fewer splits than the default:
    // when the search fits noise all over, it is repeated with a higher one.
    let (reference, truth) = synth(42 * 60_000, 21);
    let subs = stepped(&truth, &[(0, 100), (6 * 60_000, 1_100), (21 * 60_000, 1_500), (33 * 60_000, 900)]);
    for split_penalty in [100.0, 500.0] {
        let a = align(&reference, &subs, &AlignConfig { split_penalty, ..AlignConfig::default() });
        assert!(within(&a, &truth, 200) >= 0.98, "penalty {split_penalty}: segments {:?}", a.segments);
    }
}
