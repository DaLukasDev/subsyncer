use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use subsyncer::cache::Cache;
use subsyncer::engine::{Engine, ReferenceMode, Settings};
use subsyncer_core::verify::CheckConfig;
use subsyncer_core::{AlignConfig, GateConfig};
use subsyncer_eval::perturb::Kind;
use subsyncer_eval::report::{summarize, table};
use subsyncer_eval::synth::NoiseModel;
use subsyncer_eval::{SyntheticOptions, corpus, embedded, run_synthetic};

#[derive(Parser)]
#[command(name = "subsyncer-eval", about = "Evaluate subsyncer alignment quality")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone, Default)]
struct Tuning {
    /// Perturbation kinds (comma-separated); default all.
    #[arg(long, value_delimiter = ',')]
    kinds: Vec<String>,
    /// Global offset + framerate only.
    #[arg(long)]
    global_only: bool,
    /// Run both global-only and piecewise and report both.
    #[arg(long)]
    compare: bool,
    #[arg(long)]
    split_penalty: Option<f64>,
    #[arg(long)]
    lambda: Option<f64>,
    #[arg(long)]
    overlap_penalty: Option<f64>,
    #[arg(long)]
    ratio_margin: Option<f64>,
    #[arg(long)]
    min_confidence: Option<f64>,
    #[arg(long)]
    min_improvement: Option<f64>,
    #[arg(long)]
    window_secs: Option<f64>,
    #[arg(long)]
    min_segment_cues: Option<usize>,
    /// Print per-case results as JSON lines instead of the table.
    #[arg(long)]
    json: bool,
    /// Print each case that ends below 95 % within tolerance.
    #[arg(long)]
    show_failures: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Synthetic scenarios (no media needed).
    Synthetic {
        #[arg(long, default_value_t = 40)]
        seeds: u64,
        #[arg(long, default_value_t = 0)]
        first_seed: u64,
        /// Use the harder noise model (more VAD errors, untranslated lines).
        #[arg(long)]
        hard: bool,
        #[command(flatten)]
        tuning: Tuning,
    },
    /// The embedded-track check: perturbed cues play a subtitle track
    /// embedded in the video, the synthetic speech timeline its audio.
    Embedded {
        #[arg(long, default_value_t = 60)]
        seeds: u64,
        #[arg(long, default_value_t = 0)]
        first_seed: u64,
        #[arg(long)]
        hard: bool,
        /// Perturbation kinds (comma-separated); default all.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        #[arg(long)]
        samples: Option<usize>,
        #[arg(long)]
        window_secs: Option<f64>,
        #[arg(long)]
        pad_secs: Option<f64>,
        #[arg(long)]
        min_prominence: Option<f64>,
        #[arg(long)]
        min_cues: Option<usize>,
        #[arg(long)]
        scan_min_prominence: Option<f64>,
        #[arg(long)]
        tolerance_ms: Option<i64>,
        /// Measured offset of an in-sync track. The synthetic speech model
        /// has no onset lag to speak of (real Silero output lags cue starts
        /// by about 100 ms, the production default).
        #[arg(long, default_value_t = 0)]
        expected_lag_ms: i64,
        /// Print per-case results as JSON lines instead of the table.
        #[arg(long)]
        json: bool,
        /// Print each wrong decision.
        #[arg(long)]
        show_failures: bool,
    },
    /// Real corpus: a folder of videos with known-good subtitles next to them.
    Corpus {
        dir: PathBuf,
        /// Perturbations per (pair, kind).
        #[arg(long, default_value_t = 3)]
        repeats: u64,
        #[arg(long, value_enum, default_value_t = ReferenceMode::Audio)]
        reference: ReferenceMode,
        /// Also run ffsubsync and alass if installed.
        #[arg(long)]
        baselines: bool,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Cache directory for speech timelines.
        #[arg(long, env = "SUBSYNCER_DATA_DIR")]
        data_dir: Option<PathBuf>,
        #[command(flatten)]
        tuning: Tuning,
    },
}

fn configs(t: &Tuning) -> (AlignConfig, GateConfig) {
    let mut a = AlignConfig::default();
    let mut g = GateConfig::default();
    if let Some(v) = t.split_penalty {
        a.split_penalty = v;
    }
    if let Some(v) = t.lambda {
        a.lambda = v;
    }
    if let Some(v) = t.overlap_penalty {
        a.overlap_penalty = v;
    }
    if let Some(v) = t.ratio_margin {
        a.ratio_margin = v;
    }
    if let Some(v) = t.window_secs {
        a.window_ms = (v * 1000.0) as i64;
    }
    if let Some(v) = t.min_segment_cues {
        a.min_segment_cues = v;
    }
    if let Some(v) = t.min_confidence {
        g.min_confidence = v;
    }
    if let Some(v) = t.min_improvement {
        g.min_improvement = v;
    }
    a.piecewise = !t.global_only;
    (a, g)
}

fn kinds(t: &Tuning) -> Result<Vec<Kind>> {
    if t.kinds.is_empty() {
        return Ok(Kind::ALL.to_vec());
    }
    t.kinds.iter().map(|k| Kind::parse(k).ok_or_else(|| anyhow::anyhow!("unknown kind {k:?}"))).collect()
}

fn methods(t: &Tuning) -> Vec<(&'static str, AlignConfig, GateConfig)> {
    let (a, g) = configs(t);
    if t.compare {
        let mut ga = a.clone();
        ga.piecewise = false;
        let mut pa = a;
        pa.piecewise = true;
        vec![("global", ga, g.clone()), ("piecewise", pa, g)]
    } else {
        vec![(if a.piecewise { "piecewise" } else { "global" }, a, g)]
    }
}

fn print(t: &Tuning, method: &str, results: &[subsyncer_eval::CaseResult]) {
    if t.json {
        for r in results {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            v["method"] = method.into();
            println!("{v}");
        }
    }
    if t.show_failures {
        for r in results.iter().filter(|r| r.within_frac() < 0.95) {
            eprintln!(
                "  [{method}] {} {} ({}): {:?} within {:.1}% (raw {:.1}%), splits {}, offset {} ms, ratio {:.5}, prom {:.1}, {:.3}->{:.3}",
                r.source,
                r.kind.name(),
                r.desc,
                r.outcome,
                r.within_frac() * 100.0,
                r.raw_within_frac() * 100.0,
                r.splits,
                r.offset_ms,
                r.ratio,
                r.prominence,
                r.score_before,
                r.score_after
            );
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    subsyncer::logging::init("info", "text");
    match cli.cmd {
        Cmd::Synthetic { seeds, first_seed, hard, tuning } => {
            let opts = SyntheticOptions {
                seeds: first_seed..first_seed + seeds,
                kinds: kinds(&tuning)?,
                noise: if hard { NoiseModel::hard() } else { NoiseModel::default() },
                threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            };
            let mut rows = Vec::new();
            for (name, a, g) in methods(&tuning) {
                let results = run_synthetic(&opts, &a, &g);
                print(&tuning, name, &results);
                rows.extend(summarize(name, &results));
            }
            if !tuning.json {
                print!("{}", table(&rows));
            }
        }
        Cmd::Embedded {
            seeds,
            first_seed,
            hard,
            kinds: kind_names,
            samples,
            window_secs,
            pad_secs,
            min_prominence,
            min_cues,
            scan_min_prominence,
            tolerance_ms,
            expected_lag_ms,
            json,
            show_failures,
        } => {
            let t = Tuning { kinds: kind_names, ..Tuning::default() };
            let mut check_kinds = kinds(&t)?;
            if t.kinds.is_empty() {
                check_kinds.extend(Kind::EXTRA);
            }
            let opts = SyntheticOptions {
                seeds: first_seed..first_seed + seeds,
                kinds: check_kinds,
                noise: if hard { NoiseModel::hard() } else { NoiseModel::default() },
                threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            };
            let mut cfg = CheckConfig::default();
            if let Some(v) = samples {
                cfg.samples = v;
            }
            if let Some(v) = window_secs {
                cfg.window_ms = (v * 1000.0) as i64;
            }
            if let Some(v) = pad_secs {
                cfg.pad_ms = (v * 1000.0) as i64;
            }
            if let Some(v) = min_prominence {
                cfg.min_prominence = v;
            }
            if let Some(v) = min_cues {
                cfg.min_cues = v;
            }
            if let Some(v) = scan_min_prominence {
                cfg.scan_min_prominence = v;
            }
            if let Some(v) = tolerance_ms {
                cfg.tolerance_ms = v;
            }
            cfg.expected_lag_ms = expected_lag_ms;
            let cases = embedded::run(&opts, &AlignConfig::default(), &cfg);
            if json {
                for c in &cases {
                    println!("{}", serde_json::to_string(c)?);
                }
            } else {
                print!("{}", embedded::table(&cases, &cfg));
            }
            if show_failures {
                for c in cases.iter().filter(|c| c.accepted == c.bad) {
                    let s: Vec<String> = c
                        .samples
                        .iter()
                        .map(|s| format!("{}@{}s:{:+}ms/z{:.1}", s.cues, s.start_ms / 1000, s.offset_ms, s.prominence))
                        .collect();
                    eprintln!(
                        "  {} seed {} ({}): {} | samples {:?} [{}] | full ok={} err {} ms conf {:.1} ratio {:.4} splits {} windows {} disagree {:?}",
                        c.kind.name(),
                        c.seed,
                        c.desc,
                        if c.accepted { "FALSE ACCEPT" } else { "false reject" },
                        c.sample_verdict,
                        s.join(" "),
                        c.full.accepted,
                        c.full.max_error_ms,
                        c.full.confidence,
                        c.full.ratio,
                        c.full.splits,
                        c.full.windows,
                        c.full
                            .disagreeing
                            .iter()
                            .map(|w| format!("{}s:{:+}ms/z{:.1}", w.start_ms / 1000, w.offset_ms, w.prominence))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
        Cmd::Corpus { dir, repeats, reference, baselines, seed, data_dir, tuning } => {
            if !dir.is_dir() {
                bail!("not a directory: {}", dir.display());
            }
            let cache = match data_dir {
                Some(d) => Some(Arc::new(Cache::open(&d)?)),
                None => None,
            };
            let mut rows = Vec::new();
            let mut first = true;
            for (name, a, g) in methods(&tuning) {
                let engine =
                    Engine::new(Settings { align: a.clone(), gate: g.clone(), ..Default::default() }, cache.clone());
                let opts = corpus::CorpusOptions {
                    dir: dir.clone(),
                    kinds: kinds(&tuning)?,
                    repeats,
                    reference,
                    baselines: baselines && first,
                    seed,
                };
                let res = corpus::run_corpus(&engine, &opts, &a, &g)?;
                print(&tuning, name, &res.ours);
                rows.extend(summarize(name, &res.ours));
                for (b, r) in &res.baselines {
                    print(&tuning, b.name(), r);
                    rows.extend(summarize(b.name(), r));
                }
                first = false;
            }
            if !tuning.json {
                print!("{}", table(&rows));
            }
        }
    }
    Ok(())
}
