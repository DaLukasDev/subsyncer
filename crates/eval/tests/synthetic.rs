//! Synthetic regression suite (runs in CI). Thresholds sit a little below
//! what the defaults achieve so noise in the generator doesn't flake.

use subsyncer_core::{AlignConfig, GateConfig, Outcome};
use subsyncer_eval::perturb::Kind;
use subsyncer_eval::report::{summarize, table};
use subsyncer_eval::synth::NoiseModel;
use subsyncer_eval::{SyntheticOptions, run_synthetic};

fn opts(kinds: &[Kind], noise: NoiseModel) -> SyntheticOptions {
    SyntheticOptions {
        seeds: 1000..1012,
        kinds: kinds.to_vec(),
        noise,
        threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
    }
}

#[test]
fn piecewise_defaults_meet_targets() {
    let results =
        run_synthetic(&opts(&Kind::ALL, NoiseModel::default()), &AlignConfig::default(), &GateConfig::default());
    let rows = summarize("piecewise", &results);
    println!("{}", table(&rows));
    let get = |k: Kind| rows.iter().find(|r| r.kind == k.name()).unwrap();

    // A subtitle for a different video is never rewritten.
    assert!(results.iter().filter(|r| r.kind == Kind::WrongVideo).all(|r| !r.written));
    // Already-synced input is never rewritten.
    assert!(results.iter().filter(|r| r.kind == Kind::None).all(|r| !r.written && r.outcome == Outcome::AlreadySynced));
    for k in [Kind::SmallOffset, Kind::Offset, Kind::Framerate] {
        assert!(get(k).within >= 0.98, "{}: {:.3}", k.name(), get(k).within);
    }
    // Milestone 3: inserted chunks recover with >= 95 % of cues within 200 ms.
    assert!(get(Kind::InsertChunk).within >= 0.95, "insert_chunk {:.3}", get(Kind::InsertChunk).within);
    for k in [Kind::InsertTwo, Kind::RemoveChunk, Kind::FramerateInsert] {
        assert!(get(k).within >= 0.90, "{}: {:.3}", k.name(), get(k).within);
    }
}

#[test]
fn global_only_handles_constant_offsets() {
    let cfg = AlignConfig { piecewise: false, ..AlignConfig::default() };
    let kinds = [Kind::None, Kind::SmallOffset, Kind::Offset, Kind::Framerate, Kind::InsertChunk];
    let results = run_synthetic(&opts(&kinds, NoiseModel::default()), &cfg, &GateConfig::default());
    let rows = summarize("global", &results);
    println!("{}", table(&rows));
    let get = |k: Kind| rows.iter().find(|r| r.kind == k.name()).unwrap();
    for k in [Kind::SmallOffset, Kind::Offset, Kind::Framerate] {
        assert!(get(k).within >= 0.98, "{}: {:.3}", k.name(), get(k).within);
    }
    // Global-only can't fix an inserted chunk; it must not report splits.
    assert_eq!(get(Kind::InsertChunk).mean_splits, 0.0);
}

#[test]
fn hard_noise_never_breaks_synced_input() {
    let results = run_synthetic(
        &opts(&[Kind::None, Kind::Offset, Kind::WrongVideo], NoiseModel::hard()),
        &AlignConfig::default(),
        &GateConfig::default(),
    );
    for r in &results {
        if r.kind == Kind::None || r.kind == Kind::WrongVideo {
            assert!(!r.written, "rewrote already-synced input: {r:?}");
        } else {
            // Either fixed, or left alone; never made worse than the input.
            assert!(r.within_frac() >= 0.9 || !r.written, "{r:?}");
        }
    }
}
