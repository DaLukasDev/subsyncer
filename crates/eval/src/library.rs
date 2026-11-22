//! Real speech timelines and embedded subtitle tracks from a subsyncer
//! database, the cache a deployment builds up while it syncs a library. They
//! give real speech detection to evaluate against, without the media files.
//!
//! Tracks the embedded-track check accepted are in sync with their audio, so
//! they serve as the truth for perturbed copies, like the synthetic episodes.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use subsyncer_core::align::{self, AlignConfig};
use subsyncer_core::gate::GateConfig;
use subsyncer_core::timeline::{Interval, Timeline};

use crate::perturb::{self, Kind};
use crate::synth::Rng;
use crate::{CaseResult, evaluate_case};

/// A subsyncer database, opened read-only.
pub struct Library {
    conn: Connection,
}

/// An embedded track the check accepted, with its video's speech timeline.
#[derive(Clone)]
pub struct TrackCase {
    pub video: String,
    pub stream: usize,
    pub timeline: Timeline,
    pub track: Vec<Interval>,
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self { conn })
    }

    /// The speech timeline of the first video whose path contains `like`.
    pub fn timeline(&self, like: &str) -> Result<(String, Timeline)> {
        let row: Option<(String, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT path, data FROM timelines WHERE instr(path, ?1) > 0 ORDER BY path, stream LIMIT 1",
                params![like],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((path, data)) = row else { bail!("no speech timeline for a video matching {like:?}") };
        Ok((path, Timeline::decode_rle(&data)?))
    }

    /// The cached dialogue cues of an embedded track of `video` (exact path).
    /// With no stream given, the track with the most cues.
    pub fn track(&self, video: &str, stream: Option<usize>) -> Result<(usize, Vec<Interval>)> {
        let mut stmt = self.conn.prepare(
            "SELECT stream, cues FROM embedded_tracks WHERE path = ?1 AND (?2 IS NULL OR stream = ?2)
             ORDER BY json_array_length(cues) DESC LIMIT 1",
        )?;
        let row: Option<(i64, String)> =
            stmt.query_row(params![video, stream.map(|s| s as i64)], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((stream, json)) = row else { bail!("no cached embedded track for {video}") };
        Ok((stream as usize, parse_cues(&json)?))
    }

    /// Every accepted embedded track that has a speech timeline. Ordered by
    /// path, so runs are reproducible.
    pub fn accepted_tracks(&self) -> Result<Vec<TrackCase>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.path, t.stream, t.cues, s.data FROM embedded_tracks t
             JOIN timelines s ON s.path = t.path AND s.size = t.size AND s.mtime_ns = t.mtime_ns
             WHERE EXISTS (SELECT 1 FROM embedded_checks c WHERE c.path = t.path AND c.size = t.size
                           AND c.mtime_ns = t.mtime_ns AND c.stream = t.stream AND c.used = 1)
             GROUP BY t.path, t.stream ORDER BY t.path, t.stream",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, Vec<u8>>(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (video, stream, cues, data) = row?;
            out.push(TrackCase {
                video,
                stream: stream as usize,
                timeline: Timeline::decode_rle(&data)?,
                track: parse_cues(&cues)?,
            });
        }
        Ok(out)
    }
}

fn parse_cues(json: &str) -> Result<Vec<Interval>> {
    let pairs: Vec<(i64, i64)> = serde_json::from_str(json).context("cached embedded track")?;
    Ok(pairs.into_iter().map(|(s, e)| Interval::new(s, e)).collect())
}

#[derive(Clone, Debug)]
pub struct LibraryOptions {
    pub kinds: Vec<Kind>,
    /// Perturbations per (track, kind).
    pub repeats: u64,
    pub seed: u64,
    /// Use at most this many tracks (spread evenly over the library).
    pub limit: Option<usize>,
    pub threads: usize,
}

/// Perturb every accepted track and align it against its own audio (or, for
/// [`Kind::WrongVideo`], the next video's).
pub fn run(
    cases: &[TrackCase],
    opts: &LibraryOptions,
    align_cfg: &AlignConfig,
    gate_cfg: &GateConfig,
) -> Vec<CaseResult> {
    let picked: Vec<usize> = match opts.limit {
        Some(n) if n < cases.len() => (0..n).map(|i| i * cases.len() / n).collect(),
        _ => (0..cases.len()).collect(),
    };
    // A wrong video needs another video to play it.
    let kinds: Vec<Kind> = opts.kinds.iter().copied().filter(|&k| k != Kind::WrongVideo || cases.len() > 1).collect();
    let jobs: Vec<(usize, Kind, u64)> = picked
        .iter()
        .flat_map(|&i| kinds.iter().flat_map(move |&k| (0..opts.repeats).map(move |r| (i, k, r))))
        .collect();
    let threads = opts.threads.max(1).min(jobs.len().max(1));
    let one = AlignConfig { ratios: vec![1.0], ..align_cfg.clone() };
    let lags: Vec<i64> = cases
        .iter()
        .map(|c| {
            let off = align::global_align(&c.timeline, &c.track, &one).offset_ms;
            if off.abs() <= 300 { off } else { 0 }
        })
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(i, kind, rep)) = jobs.get(n) else { break };
                    let case = &cases[i];
                    let seed = opts.seed ^ (i as u64).wrapping_mul(0x9E37_79B9) ^ rep.wrapping_mul(0x85EB_CA6B);
                    let mut rng = Rng::new(seed ^ (kind as u64 + 1).wrapping_mul(0xA5A5_5A5A));
                    let len_ms = case.timeline.duration_ms();
                    let mut p = perturb::perturb(kind, &case.track, len_ms, &mut rng);
                    // Real speech detection starts about 100 ms after a cue:
                    // count a cue as right where the aligner puts the
                    // unperturbed track.
                    let lag = lags[i];
                    for t in p.truth.iter_mut().flatten() {
                        *t = Interval::new(t.start_ms + lag, t.end_ms + lag);
                    }
                    let reference = if kind == Kind::WrongVideo {
                        &cases[(i + cases.len() / 2 + 1) % cases.len()].timeline
                    } else {
                        &case.timeline
                    };
                    p.desc = format!("{} [seed {seed}]", p.desc);
                    let r = evaluate_case(&case.video, reference, &p, align_cfg, gate_cfg);
                    results.lock().unwrap().push((n, r));
                }
            });
        }
    });
    let mut v = results.into_inner().unwrap();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

/// Align one video's subtitle (a file, or its cached embedded track) against
/// its speech timeline and print the result, with where each two-minute
/// stretch of the result lines up with the speech.
pub fn diagnose(
    lib: &Library,
    like: &str,
    subtitle: Option<&Path>,
    perturbation: Option<(Kind, u64)>,
    align_cfg: &AlignConfig,
    gate_cfg: &GateConfig,
) -> Result<()> {
    use subsyncer_core::verify::{self, CheckConfig};
    use subsyncer_core::{SubtitleFile, gate};

    let (video, timeline) = lib.timeline(like)?;
    let cues: Vec<Interval> = match subtitle {
        Some(p) => {
            let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
            let file = SubtitleFile::parse(&bytes, None)?;
            file.dialogue_cues().into_iter().map(|c| c.1).collect()
        }
        None => lib.track(&video, None)?.1,
    };
    let mut truth: Option<Vec<Option<Interval>>> = None;
    let cues = match perturbation {
        Some((kind, seed)) => {
            let mut rng = Rng::new(seed ^ (kind as u64 + 1).wrapping_mul(0xA5A5_5A5A));
            let p = perturb::perturb(kind, &cues, timeline.duration_ms(), &mut rng);
            let one = AlignConfig { ratios: vec![1.0], ..align_cfg.clone() };
            let lag = align::global_align(&timeline, &cues, &one).offset_ms;
            let lag = if lag.abs() <= 300 { lag } else { 0 };
            println!("perturbed: {} (lag {lag} ms)", p.desc);
            truth = Some(p.truth.iter().map(|t| t.map(|t| Interval::new(t.start_ms + lag, t.end_ms + lag))).collect());
            p.cues
        }
        None => cues,
    };
    println!(
        "{video}: {} s of audio, {:.1}% speech, {} dialogue cues",
        timeline.duration_ms() / 1000,
        timeline.active_fraction() * 100.0,
        cues.len()
    );
    let a = align::align(&timeline, &cues, align_cfg);
    let v = gate::evaluate(&timeline, &cues, &a, gate_cfg);
    println!(
        "{:?}: ratio {:.5}, offset {} ms, {} splits, confidence {:.1}, overlap {:.3} -> {:.3} ({})",
        v.outcome, v.ratio, v.offset_ms, v.splits, v.prominence, v.score_before, v.score_after, v.reason
    );
    for s in &a.segments {
        println!(
            "  from {:>8.1} s: offset {:>6} ms ({} cues)",
            cues[s.first_cue].start_ms as f64 / 1000.0,
            s.offset_ms,
            s.last_cue - s.first_cue + 1
        );
    }
    if let Some(t) = &truth {
        let (n, within, med) = crate::score_cues(&a.cues, t);
        println!("  {within}/{n} cues within {} ms of the truth, median error {med} ms", crate::TOLERANCE_MS);
    }
    // Where each stretch of the corrected cues lines up: 0 means the result
    // is right there.
    let placed: Vec<Interval> = a.cues.iter().copied().filter(|c| c.end_ms > c.start_ms).collect();
    let cfg = CheckConfig { expected_lag_ms: 0, ..CheckConfig::default() };
    println!("  stretches (video time, cues, lines up at ms, prominence, support):");
    for w in verify::scan(&timeline, &placed, &cfg) {
        let flag = if verify::lines_up_elsewhere(&w, &cfg) { "  <- lines up elsewhere" } else { "" };
        println!(
            "    {:>6.1} min {:>3} {:>+6} {:>5.1} {:>5.1}{flag}",
            w.start_ms as f64 / 60_000.0,
            w.cues,
            w.offset_ms,
            w.prominence,
            w.support
        );
    }
    Ok(())
}
