//! The embedded-track check on synthetic episodes (no media needed).

use subsyncer_core::align::AlignConfig;
use subsyncer_core::verify::{CheckConfig, SampleVerdict};
use subsyncer_eval::SyntheticOptions;
use subsyncer_eval::embedded;
use subsyncer_eval::perturb::Kind;
use subsyncer_eval::synth::NoiseModel;

#[test]
fn bad_tracks_do_no_harm_and_good_tracks_are_used() {
    let mut kinds = Kind::ALL.to_vec();
    kinds.extend(Kind::EXTRA);
    let opts = SyntheticOptions {
        seeds: 0..30,
        kinds,
        noise: NoiseModel::default(),
        threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
    };
    // The synthetic speech model has no onset lag to speak of.
    let cfg = CheckConfig { expected_lag_ms: 0, ..CheckConfig::default() };
    let cases = embedded::run(&opts, &AlignConfig::default(), &cfg);

    let good: Vec<_> = cases.iter().filter(|c| !c.bad).collect();
    assert!(good.iter().all(|c| c.accepted), "a good track was rejected");
    let fast = good.iter().filter(|c| c.sample_verdict == Some(SampleVerdict::Pass)).count();
    assert!(fast * 10 >= good.len() * 9, "only {fast}/{} good tracks passed on samples alone", good.len());

    // Bad tracks: never accepted except late cuts, and those must not move
    // any line of a correct subtitle.
    for c in cases.iter().filter(|c| c.bad && c.accepted) {
        assert_eq!(c.kind, Kind::LateInsert, "{:?} seed {} accepted: {}", c.kind, c.seed, c.desc);
        assert_eq!(c.harmed, 0, "seed {} ({}) moved {} lines", c.seed, c.desc, c.harmed);
    }
    // Wrong video and plain offsets are rejected outright.
    assert!(cases.iter().filter(|c| matches!(c.kind, Kind::WrongVideo | Kind::Offset)).all(|c| !c.accepted));
}
