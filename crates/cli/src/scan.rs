//! Library scan: find video + subtitle pairs and sync those not yet done.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use subsyncer_core::Outcome;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::cache::FileKey;
use crate::engine::{Engine, ReferenceMode, SyncReport, SyncRequest};

pub const VIDEO_EXTS: &[&str] =
    &["mkv", "mp4", "m4v", "avi", "mov", "webm", "ts", "m2ts", "wmv", "mpg", "mpeg", "ogv", "flv"];
pub const SUB_EXTS: &[&str] = &["srt", "ass", "ssa", "vtt"];

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub dry_run: bool,
    pub force: bool,
    pub reference: ReferenceMode,
    pub include_forced: bool,
    pub backup: bool,
    /// Videos processed at once.
    pub jobs: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub pairs: usize,
    pub skipped: usize,
    pub synced: usize,
    pub already_synced: usize,
    pub low_confidence: usize,
    pub failed: usize,
}

impl fmt::Display for ScanSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} pairs: {} synced, {} already synced, {} low confidence, {} failed, {} skipped (already processed)",
            self.pairs, self.synced, self.already_synced, self.low_confidence, self.failed, self.skipped
        )
    }
}

fn ext_of(p: &Path) -> Option<String> {
    p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

/// Find (video, subtitle) pairs. A subtitle belongs to the video whose file
/// stem is the longest prefix of the subtitle name followed by a dot
/// (`Movie.mkv` <- `Movie.en.srt`, `Movie.srt`).
pub fn find_pairs(dir: &Path, include_forced: bool) -> Vec<(PathBuf, PathBuf)> {
    let mut by_dir: BTreeMap<PathBuf, (Vec<PathBuf>, Vec<PathBuf>)> = BTreeMap::new();
    for entry in WalkDir::new(dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        let name = entry.file_name().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Some(ext) = ext_of(p) else { continue };
        let parent = p.parent().unwrap_or(Path::new("")).to_path_buf();
        if VIDEO_EXTS.contains(&ext.as_str()) {
            by_dir.entry(parent).or_default().0.push(p.to_path_buf());
        } else if SUB_EXTS.contains(&ext.as_str()) {
            by_dir.entry(parent).or_default().1.push(p.to_path_buf());
        }
    }
    let mut pairs = Vec::new();
    for (_, (videos, subs)) in by_dir {
        for sub in subs {
            let sname = sub.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !include_forced && sname.to_ascii_lowercase().contains(".forced.") {
                continue;
            }
            let best = videos
                .iter()
                .filter_map(|v| {
                    let stem = v.file_stem()?.to_string_lossy().into_owned();
                    (sname.len() > stem.len() && sname.starts_with(&stem) && sname.as_bytes()[stem.len()] == b'.')
                        .then_some((stem.len(), v))
                })
                .max_by_key(|(len, _)| *len);
            if let Some((_, v)) = best {
                pairs.push((v.clone(), sub));
            }
        }
    }
    pairs.sort_by(|a, b| a.1.cmp(&b.1));
    pairs
}

/// What became of one pair.
enum Item {
    /// Processed before, and nothing changed since.
    Skipped,
    /// The files couldn't be read.
    Unreadable,
    Done(Box<SyncReport>),
}

/// Sync every pair under `dir` not processed before. Reports arrive in
/// `on_report` as they finish (in any order with `jobs` > 1).
pub fn scan(
    engine: &Engine,
    dir: &Path,
    opts: &ScanOptions,
    mut on_report: impl FnMut(&SyncReport),
) -> Result<ScanSummary> {
    if !dir.is_dir() {
        anyhow::bail!("not a directory: {}", dir.display());
    }
    let pairs = find_pairs(dir, opts.include_forced);
    let mut sum = ScanSummary { pairs: pairs.len(), ..Default::default() };
    // The subtitles of one video go to one worker, one after the other, so
    // its audio is analysed once and then comes from the cache.
    let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut by_video: HashMap<PathBuf, usize> = HashMap::new();
    for (video, sub) in pairs {
        let k = *by_video.entry(video.clone()).or_insert_with(|| {
            groups.push((video, Vec::new()));
            groups.len() - 1
        });
        groups[k].1.push(sub);
    }
    let settings = engine.settings_fingerprint(opts.reference);
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..opts.jobs.clamp(1, groups.len().max(1)) {
            let tx = tx.clone();
            let (groups, next, settings) = (&groups, &next, &settings);
            s.spawn(move || {
                while let Some((video, subs)) = groups.get(next.fetch_add(1, Ordering::Relaxed)) {
                    for sub in subs {
                        if tx.send(process(engine, video, sub, opts, settings)).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        drop(tx);
        for item in rx {
            match item {
                Item::Skipped => sum.skipped += 1,
                Item::Unreadable => {}
                Item::Done(report) => {
                    match report.outcome {
                        Some(Outcome::Synced) => sum.synced += 1,
                        Some(Outcome::AlreadySynced) => sum.already_synced += 1,
                        Some(Outcome::LowConfidence) => sum.low_confidence += 1,
                        _ => sum.failed += 1,
                    }
                    on_report(&report);
                }
            }
        }
    });
    Ok(sum)
}

fn process(engine: &Engine, video: &Path, sub: &Path, opts: &ScanOptions, settings: &str) -> Item {
    let (sub_key, video_key) = match (FileKey::of(sub), FileKey::of(video)) {
        (Ok(s), Ok(v)) => (s, v),
        _ => {
            warn!(subtitle = %sub.display(), "cannot stat pair; skipping");
            return Item::Unreadable;
        }
    };
    // Skip pairs processed before whose files haven't changed. A
    // low-confidence result is retried once the version or settings
    // change: they may settle it now.
    if !opts.force
        && let Some(cache) = &engine.cache
        && let Ok(Some(st)) = cache.get_scan_state(&sub_key.path)
        && st.sub_size == sub_key.size
        && st.sub_mtime_ns == sub_key.mtime_ns
        && st.video_size == video_key.size
        && st.video_mtime_ns == video_key.mtime_ns
        && (st.settings == settings || st.outcome != Outcome::LowConfidence.as_str())
    {
        debug!(subtitle = %sub.display(), outcome = %st.outcome, "already processed");
        return Item::Skipped;
    }
    let req = SyncRequest {
        video: video.to_path_buf(),
        subtitle: sub.to_path_buf(),
        reference: opts.reference,
        dry_run: opts.dry_run,
        output: None,
        backup: opts.backup,
        cancel: None,
    };
    let report = engine.sync(&req);
    // Record deterministic outcomes; failures are retried next scan.
    if !opts.dry_run
        && report.outcome != Some(Outcome::Failed)
        && let Some(cache) = &engine.cache
    {
        let after = FileKey::of(sub).unwrap_or(sub_key);
        if let Err(e) = cache.put_scan_state(&after, &video_key, report.outcome_str(), settings) {
            warn!("recording scan state: {e:#}");
        }
    }
    Item::Done(Box::new(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::create_dir_all(p.join("Show/Season 1")).unwrap();
        for f in [
            "Movie (2000).mkv",
            "Movie (2000).en.srt",
            "Movie (2000).nl.forced.srt",
            "Movie (2000).en.srt.bak",
            "Movie (2000) Part 2.mkv",
            "Movie (2000) Part 2.srt",
            "orphan.srt",
            "Show/Season 1/S01E01.mp4",
            "Show/Season 1/S01E01.en.hi.ass",
            "Show/Season 1/S01E01.nfo",
        ] {
            std::fs::write(p.join(f), b"x").unwrap();
        }
        let got: Vec<(String, String)> = find_pairs(p, false)
            .into_iter()
            .map(|(v, s)| {
                (v.strip_prefix(p).unwrap().display().to_string(), s.strip_prefix(p).unwrap().display().to_string())
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("Movie (2000) Part 2.mkv".to_string(), "Movie (2000) Part 2.srt".to_string()),
                ("Movie (2000).mkv".to_string(), "Movie (2000).en.srt".to_string()),
                ("Show/Season 1/S01E01.mp4".to_string(), "Show/Season 1/S01E01.en.hi.ass".to_string()),
            ]
        );
        assert_eq!(find_pairs(p, true).len(), 4);
    }

    const SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n";

    /// An engine whose syncs fail at once (no ffprobe), to exercise the scan
    /// without media.
    fn failing_engine(cache: Option<std::sync::Arc<crate::cache::Cache>>) -> Engine {
        let tools = crate::media::Tools { ffprobe: "/nonexistent/ffprobe".into(), ..Default::default() };
        Engine::new(crate::engine::Settings { tools, ..Default::default() }, cache)
    }

    fn opts(jobs: usize) -> ScanOptions {
        ScanOptions {
            dry_run: false,
            force: false,
            reference: ReferenceMode::Auto,
            include_forced: false,
            backup: true,
            jobs,
        }
    }

    #[test]
    fn parallel_scan_reports_every_pair_once() {
        let d = tempfile::tempdir().unwrap();
        for v in 0..5 {
            std::fs::write(d.path().join(format!("e{v}.mkv")), b"").unwrap();
            for lang in ["en", "nl"] {
                std::fs::write(d.path().join(format!("e{v}.{lang}.srt")), SRT).unwrap();
            }
        }
        let mut seen = Vec::new();
        let sum = scan(&failing_engine(None), d.path(), &opts(3), |r| seen.push(r.subtitle.clone())).unwrap();
        assert_eq!((sum.pairs, sum.failed), (10, 10));
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 10);
    }

    #[test]
    fn low_confidence_is_retried_after_settings_change() {
        let d = tempfile::tempdir().unwrap();
        let (video, sub) = (d.path().join("a.mkv"), d.path().join("a.en.srt"));
        std::fs::write(&video, b"").unwrap();
        std::fs::write(&sub, SRT).unwrap();
        let cache = std::sync::Arc::new(crate::cache::Cache::open(&d.path().join("data")).unwrap());
        let engine = failing_engine(Some(cache.clone()));
        let current = engine.settings_fingerprint(ReferenceMode::Auto);
        let (sk, vk) = (FileKey::of(&sub).unwrap(), FileKey::of(&video).unwrap());
        let skipped = |outcome: &str, settings: &str| {
            cache.put_scan_state(&sk, &vk, outcome, settings).unwrap();
            scan(&engine, d.path(), &opts(1), |_| {}).unwrap().skipped == 1
        };
        assert!(!skipped("low_confidence", "older"), "retried after a change");
        assert!(skipped("low_confidence", &current));
        assert!(skipped("already_synced", "older"));
        assert!(skipped("synced", "older"));
    }
}
