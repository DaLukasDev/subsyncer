//! Library scan: find video + subtitle pairs and sync those not yet done.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

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
    for (video, sub) in pairs {
        let (sub_key, video_key) = match (FileKey::of(&sub), FileKey::of(&video)) {
            (Ok(s), Ok(v)) => (s, v),
            _ => {
                warn!(subtitle = %sub.display(), "cannot stat pair; skipping");
                continue;
            }
        };
        if !opts.force
            && let Some(cache) = &engine.cache
            && let Ok(Some(st)) = cache.get_scan_state(&sub_key.path)
            && st.sub_size == sub_key.size
            && st.sub_mtime_ns == sub_key.mtime_ns
            && st.video_size == video_key.size
            && st.video_mtime_ns == video_key.mtime_ns
        {
            debug!(subtitle = %sub.display(), outcome = %st.outcome, "already processed");
            sum.skipped += 1;
            continue;
        }
        let req = SyncRequest {
            video: video.clone(),
            subtitle: sub.clone(),
            reference: opts.reference,
            dry_run: opts.dry_run,
            output: None,
            backup: opts.backup,
            cancel: None,
        };
        let report = engine.sync(&req);
        match report.outcome {
            Some(Outcome::Synced) => sum.synced += 1,
            Some(Outcome::AlreadySynced) => sum.already_synced += 1,
            Some(Outcome::LowConfidence) => sum.low_confidence += 1,
            _ => sum.failed += 1,
        }
        on_report(&report);
        // Record deterministic outcomes; failures are retried next scan.
        if !opts.dry_run
            && report.outcome != Some(Outcome::Failed)
            && let Some(cache) = &engine.cache
        {
            let after = FileKey::of(&sub).unwrap_or(sub_key);
            if let Err(e) = cache.put_scan_state(&after, &video_key, report.outcome_str()) {
                warn!("recording scan state: {e:#}");
            }
        }
    }
    Ok(sum)
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
}
