//! Real-corpus evaluation: a folder of videos with known-good subtitles.
//! Subtitles are perturbed, re-synced, and compared to the originals.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use subsyncer::engine::{Engine, ReferenceMode};
use subsyncer_core::gate::{GateConfig, Outcome};
use subsyncer_core::timeline::Interval;
use subsyncer_core::{AlignConfig, Format, SubtitleFile};
use tracing::{info, warn};

use crate::perturb::{self, Kind, Perturbed};
use crate::synth::Rng;
use crate::{CaseResult, evaluate_case, score_cues};

#[derive(Clone, Debug)]
pub struct CorpusOptions {
    pub dir: PathBuf,
    pub kinds: Vec<Kind>,
    /// Perturbations per (pair, kind).
    pub repeats: u64,
    pub reference: ReferenceMode,
    /// Also run ffsubsync / alass when they are on PATH.
    pub baselines: bool,
    pub seed: u64,
}

/// External baseline tools, invoked as processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    Ffsubsync,
    Alass,
}

impl Baseline {
    pub fn name(&self) -> &'static str {
        match self {
            Baseline::Ffsubsync => "ffsubsync",
            Baseline::Alass => "alass",
        }
    }
    fn available(&self) -> bool {
        let bin = match self {
            Baseline::Ffsubsync => "ffsubsync",
            Baseline::Alass => "alass",
        };
        Command::new(bin).arg("--help").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
    }
    fn run(&self, video: &Path, input: &Path, output: &Path) -> Result<()> {
        let mut cmd = match self {
            Baseline::Ffsubsync => {
                let mut c = Command::new("ffsubsync");
                c.arg(video).arg("-i").arg(input).arg("-o").arg(output);
                c
            }
            Baseline::Alass => {
                let mut c = Command::new("alass");
                c.arg(video).arg(input).arg(output);
                c
            }
        };
        let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {}", self.name()))?;
        if !out.status.success() || !output.exists() {
            bail!("{} failed: {}", self.name(), String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
        }
        Ok(())
    }
}

pub struct CorpusResults {
    pub ours: Vec<CaseResult>,
    pub baselines: Vec<(Baseline, Vec<CaseResult>)>,
}

/// Write cues as a plain SRT whose text is the cue's index, so baseline
/// output can be matched back cue by cue.
fn write_indexed_srt(path: &Path, cues: &[Interval]) -> Result<()> {
    let fmt = |ms: i64| {
        let ms = ms.max(0);
        format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    };
    let mut s = String::new();
    for (i, c) in cues.iter().enumerate() {
        s.push_str(&format!("{}\n{} --> {}\ncue {}\n\n", i + 1, fmt(c.start_ms), fmt(c.end_ms), i));
    }
    std::fs::write(path, s)?;
    Ok(())
}

fn read_indexed_srt(path: &Path, n: usize) -> Result<Vec<Interval>> {
    let bytes = std::fs::read(path)?;
    let f = SubtitleFile::parse(&bytes, Some(Format::Srt))?;
    let mut out = vec![Interval::new(i64::MIN / 4, i64::MIN / 4); n];
    for i in 0..f.cues().len() {
        let text = f.cue_text(i);
        if let Some(idx) = text.trim().strip_prefix("cue ").and_then(|x| x.trim().parse::<usize>().ok())
            && idx < n
        {
            out[idx] = f.cues()[i].interval();
        }
    }
    Ok(out)
}

pub fn run_corpus(
    engine: &Engine,
    opts: &CorpusOptions,
    align_cfg: &AlignConfig,
    gate_cfg: &GateConfig,
) -> Result<CorpusResults> {
    let pairs = subsyncer::scan::find_pairs(&opts.dir, false);
    if pairs.is_empty() {
        bail!("no video + subtitle pairs found in {}", opts.dir.display());
    }
    let baselines: Vec<Baseline> = if opts.baselines {
        [Baseline::Ffsubsync, Baseline::Alass].into_iter().filter(|b| b.available()).collect()
    } else {
        Vec::new()
    };
    for b in &baselines {
        info!("baseline available: {}", b.name());
    }
    let tmp = tempfile::tempdir()?;
    let mut ours = Vec::new();
    let mut base_results: Vec<(Baseline, Vec<CaseResult>)> = baselines.iter().map(|&b| (b, Vec::new())).collect();
    // Load every pair's truth and reference first, so `wrong_video` can align
    // a subtitle against a different video from the corpus.
    struct Item {
        video: PathBuf,
        source: String,
        truth: Vec<Interval>,
        reference: subsyncer_core::Timeline,
    }
    let mut items: Vec<Item> = Vec::new();
    for (video, sub) in pairs {
        let bytes = std::fs::read(&sub)?;
        let hint = sub.extension().and_then(|e| e.to_str()).and_then(Format::from_extension);
        let file = match SubtitleFile::parse(&bytes, hint) {
            Ok(f) => f,
            Err(e) => {
                warn!(subtitle = %sub.display(), "skipping: {e}");
                continue;
            }
        };
        let truth: Vec<Interval> = file.dialogue_cues().into_iter().map(|x| x.1).collect();
        let t = Instant::now();
        let (reference, label) = match engine.reference_timeline(&video, opts.reference, truth.len()) {
            Ok(r) => r,
            Err(e) => {
                warn!(video = %video.display(), "skipping: {e:#}");
                continue;
            }
        };
        info!(
            video = %video.display(),
            reference = %label,
            cues = truth.len(),
            speech = format!("{:.1}%", reference.active_fraction() * 100.0),
            elapsed_ms = t.elapsed().as_millis() as u64,
            "reference ready"
        );
        let source = sub.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        items.push(Item { video, source, truth, reference });
    }
    if opts.kinds.contains(&Kind::WrongVideo) && items.len() < 2 {
        warn!("wrong_video needs at least two videos in the corpus; skipping it");
    }
    for (i, item) in items.iter().enumerate() {
        let len_ms = item.reference.duration_ms();
        for &kind in &opts.kinds {
            // For wrong_video, sync against the next video in the corpus.
            let (video, reference) = if kind == Kind::WrongVideo {
                if items.len() < 2 {
                    continue;
                }
                let other = &items[(i + 1) % items.len()];
                (&other.video, &other.reference)
            } else {
                (&item.video, &item.reference)
            };
            for rep in 0..opts.repeats {
                let mut rng = Rng::new(opts.seed ^ (rep + 1) ^ ((kind as u64 + 1) << 20) ^ hash(&item.source));
                let p = perturb::perturb(kind, &item.truth, len_ms, &mut rng);
                ours.push(evaluate_case(&item.source, reference, &p, align_cfg, gate_cfg));
                for (b, results) in base_results.iter_mut() {
                    match run_baseline(*b, video, &p, tmp.path(), &item.source) {
                        Ok(r) => results.push(r),
                        Err(e) => warn!("{}: {e:#}", b.name()),
                    }
                }
            }
        }
    }
    Ok(CorpusResults { ours, baselines: base_results })
}

fn run_baseline(b: Baseline, video: &Path, p: &Perturbed, tmp: &Path, source: &str) -> Result<CaseResult> {
    let input = tmp.join("in.srt");
    let output = tmp.join("out.srt");
    let _ = std::fs::remove_file(&output);
    write_indexed_srt(&input, &p.cues)?;
    let t = Instant::now();
    b.run(video, &input, &output)?;
    let elapsed = t.elapsed();
    let got = read_indexed_srt(&output, p.cues.len())?;
    let (n_truth, within, median) = score_cues(&got, &p.truth);
    Ok(CaseResult {
        source: source.to_string(),
        kind: p.kind,
        desc: p.desc.clone(),
        outcome: Outcome::Synced,
        written: true,
        n_truth,
        within,
        median_err_ms: median,
        raw_within: within,
        raw_median_err_ms: median,
        splits: 0,
        ratio: 1.0,
        offset_ms: 0,
        prominence: 0.0,
        score_before: 0.0,
        score_after: 0.0,
        stretches_off: 0,
        elapsed_ms: elapsed.min(Duration::from_secs(86_400)).as_millis() as u64,
    })
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}
