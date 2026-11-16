//! The sync pipeline: probe, build a reference timeline, align, gate, write.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use subsyncer_core::timeline::{FRAME_MS, Timeline};
use subsyncer_core::{AlignConfig, Format, GateConfig, Outcome, SubtitleFile, pipeline};
use tracing::{debug, info, warn};

use crate::cache::{Cache, FileKey};
use crate::media::{self, Tools};
use crate::vad::{self, SpeechDetector};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReferenceMode {
    /// Embedded text subtitle if usable, else audio.
    #[default]
    Auto,
    Audio,
    Embedded,
}

impl ReferenceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReferenceMode::Auto => "auto",
            ReferenceMode::Audio => "audio",
            ReferenceMode::Embedded => "embedded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "audio" => Some(Self::Audio),
            "embedded" => Some(Self::Embedded),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub tools: Tools,
    pub align: AlignConfig,
    pub gate: GateConfig,
    /// Embedded tracks with fewer dialogue cues than this fraction of the
    /// target's are treated as forced/signs tracks and rejected.
    pub min_embedded_cue_ratio: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tools: Tools::default(),
            align: AlignConfig::default(),
            gate: GateConfig::default(),
            min_embedded_cue_ratio: 0.5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub video: PathBuf,
    pub subtitle: PathBuf,
    pub reference: ReferenceMode,
    pub dry_run: bool,
    /// Write here instead of replacing the subtitle.
    pub output: Option<PathBuf>,
    pub backup: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentReport {
    /// First cue start (original time, ms) covered by this segment.
    pub from_ms: i64,
    pub offset_ms: i64,
    pub cues: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncReport {
    pub video: PathBuf,
    pub subtitle: PathBuf,
    pub outcome: Option<Outcome>,
    pub reason: String,
    /// "embedded:<stream>" or "audio:<stream>".
    pub reference: Option<String>,
    pub format: Option<Format>,
    pub encoding: Option<String>,
    pub cues: usize,
    pub dialogue_cues: usize,
    pub offset_ms: i64,
    pub ratio: f64,
    pub splits: usize,
    pub segments: Vec<SegmentReport>,
    pub score_before: f64,
    pub score_after: f64,
    pub prominence: f64,
    pub max_shift_ms: i64,
    pub dry_run: bool,
    pub written: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub cache_hit: bool,
    pub elapsed_ms: u64,
}

impl SyncReport {
    pub fn outcome_str(&self) -> &'static str {
        self.outcome.map(|o| o.as_str()).unwrap_or("failed")
    }
}

pub struct Engine {
    pub settings: Settings,
    pub cache: Option<Arc<Cache>>,
}

impl Engine {
    pub fn new(settings: Settings, cache: Option<Arc<Cache>>) -> Self {
        Self { settings, cache }
    }

    /// Run a sync. Errors are reported as `failed` outcomes, never panics.
    pub fn sync(&self, req: &SyncRequest) -> SyncReport {
        let t0 = Instant::now();
        let mut report = SyncReport {
            video: req.video.clone(),
            subtitle: req.subtitle.clone(),
            dry_run: req.dry_run,
            ratio: 1.0,
            ..Default::default()
        };
        if let Err(e) = self.sync_inner(req, &mut report) {
            report.outcome = Some(Outcome::Failed);
            report.reason = format!("{e:#}");
        }
        report.elapsed_ms = t0.elapsed().as_millis() as u64;
        report
    }

    fn sync_inner(&self, req: &SyncRequest, report: &mut SyncReport) -> Result<()> {
        let bytes = std::fs::read(&req.subtitle).with_context(|| format!("reading {}", req.subtitle.display()))?;
        let hint = req.subtitle.extension().and_then(|e| e.to_str()).and_then(Format::from_extension);
        let file = SubtitleFile::parse(&bytes, hint).context("parsing subtitle")?;
        report.format = Some(file.format());
        report.encoding = Some(file.encoding().name().to_string());
        report.cues = file.cues().len();
        let dialogue = file.dialogue_cues();
        report.dialogue_cues = dialogue.len();
        if dialogue.is_empty() {
            bail!("subtitle has no dialogue cues");
        }
        if !req.video.is_file() {
            bail!("video not found: {}", req.video.display());
        }

        let (reference, label) = self.reference(req, dialogue.len(), report)?;
        report.reference = Some(label);
        let t = Instant::now();
        let result = pipeline::sync(&file, &reference, &self.settings.align, &self.settings.gate);
        debug!(elapsed_ms = t.elapsed().as_millis() as u64, "aligned");
        let v = &result.verdict;
        report.outcome = Some(v.outcome);
        report.reason = v.reason.clone();
        report.offset_ms = v.offset_ms;
        report.ratio = v.ratio;
        report.splits = v.splits;
        report.score_before = v.score_before;
        report.score_after = v.score_after;
        report.prominence = v.prominence;
        report.max_shift_ms = v.max_shift_ms;
        report.segments = result
            .alignment
            .segments
            .iter()
            .map(|s| SegmentReport {
                from_ms: result.dialogue[s.first_cue].1.start_ms,
                offset_ms: s.offset_ms,
                cues: s.last_cue - s.first_cue + 1,
            })
            .collect();

        if !v.should_write() || req.dry_run {
            return Ok(());
        }
        let out = result.render(&file).context("rendering subtitle")?;
        let target = req.output.clone().unwrap_or_else(|| req.subtitle.clone());
        let backup = req.backup && req.output.is_none();
        report.backup = write_atomic(&target, &out, backup)?;
        report.written = Some(target);
        Ok(())
    }

    /// Build the reference timeline for `video` (used by the eval harness).
    /// `target_cues` is the dialogue cue count of the subtitle being synced,
    /// used to reject forced/signs embedded tracks.
    pub fn reference_timeline(
        &self,
        video: &Path,
        mode: ReferenceMode,
        target_cues: usize,
    ) -> Result<(Timeline, String)> {
        let req = SyncRequest {
            video: video.to_path_buf(),
            subtitle: PathBuf::new(),
            reference: mode,
            dry_run: true,
            output: None,
            backup: false,
        };
        self.reference(&req, target_cues, &mut SyncReport::default())
    }

    /// Build the reference timeline according to the requested mode.
    fn reference(&self, req: &SyncRequest, target_cues: usize, report: &mut SyncReport) -> Result<(Timeline, String)> {
        let probe = media::probe(&self.settings.tools, &req.video)?;
        if req.reference != ReferenceMode::Audio {
            match self.embedded_reference(req, &probe, target_cues) {
                Ok(Some(r)) => return Ok(r),
                Ok(None) if req.reference == ReferenceMode::Embedded => bail!("no usable embedded text subtitle"),
                Ok(None) => {}
                Err(e) if req.reference == ReferenceMode::Embedded => return Err(e),
                Err(e) => warn!("embedded subtitle reference failed, using audio: {e:#}"),
            }
        }
        self.audio_reference(req, &probe, report)
    }

    fn embedded_reference(
        &self,
        req: &SyncRequest,
        probe: &media::Probe,
        target_cues: usize,
    ) -> Result<Option<(Timeline, String)>> {
        let candidates: Vec<&media::Stream> = probe
            .text_subtitles()
            .filter(|s| {
                let title = s.title.clone().unwrap_or_default().to_ascii_lowercase();
                !s.forced && !title.contains("forced") && !title.contains("sign")
            })
            .take(6)
            .collect();
        let mut best: Option<(usize, Timeline, usize)> = None;
        for s in candidates {
            let bytes = match media::extract_subtitle(&self.settings.tools, &req.video, s) {
                Ok(b) => b,
                Err(e) => {
                    warn!(stream = s.index, "extracting embedded subtitle: {e:#}");
                    continue;
                }
            };
            let Ok(sub) = SubtitleFile::parse(&bytes, None) else { continue };
            let cues = sub.dialogue_cues();
            let n = cues.len();
            let min = (target_cues as f64 * self.settings.min_embedded_cue_ratio).ceil() as usize;
            debug!(stream = s.index, cues = n, min, "embedded subtitle candidate");
            if n < min.max(1) {
                continue;
            }
            if best.as_ref().is_none_or(|b| n > b.2) {
                let len_frames = probe
                    .duration_ms
                    .unwrap_or_else(|| cues.iter().map(|c| c.1.end_ms).max().unwrap_or(0))
                    .max(cues.iter().map(|c| c.1.end_ms).max().unwrap_or(0))
                    / FRAME_MS;
                let tl = Timeline::from_intervals(cues.iter().map(|c| c.1), Some(len_frames as usize));
                best = Some((s.index, tl, n));
            }
        }
        Ok(best.map(|(idx, tl, _)| (tl, format!("embedded:{idx}"))))
    }

    fn audio_reference(
        &self,
        req: &SyncRequest,
        probe: &media::Probe,
        report: &mut SyncReport,
    ) -> Result<(Timeline, String)> {
        let stream = probe.default_audio().ok_or_else(|| anyhow!("video has no audio stream"))?;
        let filter = media::dialogue_filter(stream);
        let version = format!("{}|{}", vad::VAD_VERSION, filter.as_deref().unwrap_or("downmix"));
        let label = format!("audio:{}", stream.index);
        let key = FileKey::of(&req.video).ok();
        if let (Some(cache), Some(key)) = (&self.cache, &key) {
            match cache.get_timeline(key, stream.index, &version) {
                Ok(Some(tl)) => {
                    report.cache_hit = true;
                    debug!("speech timeline cache hit");
                    return Ok((tl, label));
                }
                Ok(None) => {}
                Err(e) => warn!("cache read failed: {e:#}"),
            }
        }
        let t = Instant::now();
        let tl = match self.detect_speech(&req.video, stream.index, filter.as_deref()) {
            Ok(tl) => tl,
            Err(e) if filter.is_some() => {
                warn!("centre-channel decode failed ({e:#}); retrying with downmix");
                self.detect_speech(&req.video, stream.index, None)?
            }
            Err(e) => return Err(e),
        };
        info!(
            stream = stream.index,
            filter = filter.as_deref().unwrap_or("downmix"),
            secs = tl.duration_ms() / 1000,
            speech = format!("{:.1}%", tl.active_fraction() * 100.0),
            elapsed_ms = t.elapsed().as_millis() as u64,
            "speech detected"
        );
        if let (Some(cache), Some(key)) = (&self.cache, &key)
            && let Err(e) = cache.put_timeline(key, stream.index, &version, &tl)
        {
            warn!("cache write failed: {e:#}");
        }
        Ok((tl, label))
    }

    fn detect_speech(&self, video: &Path, stream: usize, filter: Option<&str>) -> Result<Timeline> {
        let mut det = SpeechDetector::new()?;
        media::decode_audio(&self.settings.tools, video, stream, filter, |s| det.push_s16(s))?;
        if det.samples() == 0 {
            bail!("audio stream decoded to zero samples");
        }
        Ok(det.finish()?.0)
    }
}

/// `<path>.bak`
pub fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Write via a temp file in the same directory and rename over the target.
/// Returns the backup path if one was made.
pub fn write_atomic(target: &Path, bytes: &[u8], backup: bool) -> Result<Option<PathBuf>> {
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".subsyncer-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    if let Ok(meta) = std::fs::metadata(target) {
        let _ = std::fs::set_permissions(tmp.path(), meta.permissions());
    }
    let bak = if backup && target.exists() {
        let b = backup_path(target);
        std::fs::copy(target, &b).with_context(|| format!("writing backup {}", b.display()))?;
        Some(b)
    } else {
        None
    };
    tmp.persist(target).map_err(|e| anyhow!("replacing {}: {}", target.display(), e.error))?;
    Ok(bak)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.en.srt");
        std::fs::write(&p, b"old").unwrap();
        let bak = write_atomic(&p, b"new", true).unwrap().unwrap();
        assert_eq!(bak, dir.path().join("a.en.srt.bak"));
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(std::fs::read(&bak).unwrap(), b"old");
        assert!(write_atomic(&p, b"newer", false).unwrap().is_none());
        assert_eq!(std::fs::read(&p).unwrap(), b"newer");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
