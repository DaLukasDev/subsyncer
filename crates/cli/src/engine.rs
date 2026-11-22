//! The sync pipeline: probe, build a reference timeline, align, gate, write.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use subsyncer_core::timeline::{FRAME_MS, Interval, Timeline};
use subsyncer_core::verify::{self, CheckConfig, FullCheck, RegionCheck, SampleCheck, SampleVerdict};
use subsyncer_core::{AlignConfig, Alignment, Format, GateConfig, Outcome, SubtitleFile, classify, pipeline};
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

/// How an embedded subtitle track is checked against the audio before it is
/// used as the reference (with `--reference auto`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddedCheck {
    /// A few short audio samples; the full audio only if they don't settle it.
    #[default]
    Sampled,
    /// Always the full audio.
    Full,
    /// Trust embedded tracks without checking.
    Off,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub tools: Tools,
    pub align: AlignConfig,
    pub gate: GateConfig,
    /// Embedded tracks with fewer dialogue cues than this fraction of the
    /// target's are treated as forced/signs tracks and rejected.
    pub min_embedded_cue_ratio: f64,
    pub embedded_check: EmbeddedCheck,
    pub check: CheckConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tools: Tools::default(),
            align: AlignConfig::default(),
            gate: GateConfig::default(),
            min_embedded_cue_ratio: 0.5,
            embedded_check: EmbeddedCheck::default(),
            check: CheckConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SyncRequest {
    pub video: PathBuf,
    pub subtitle: PathBuf,
    pub reference: ReferenceMode,
    pub dry_run: bool,
    /// Write here instead of replacing the subtitle.
    pub output: Option<PathBuf>,
    pub backup: bool,
    /// Once set, the sync stops (ffmpeg is killed) and fails as cancelled,
    /// without writing anything.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl SyncRequest {
    pub fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }
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
    /// Dialogue lines the result hides: content the video doesn't have.
    pub hidden_cues: usize,
    /// Where each two-minute stretch of the result lines up with the
    /// reference on its own: `offset_ms` 0 is where the result put it, and
    /// `start_ms` is video time. Single stretches are noisy, but one that
    /// lines up clearly elsewhere points at a part the result gets wrong.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stretches: Vec<SampleCheck>,
    pub dry_run: bool,
    pub written: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub cache_hit: bool,
    /// How an embedded subtitle track was checked against the audio.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub embedded_check: Option<EmbeddedCheckReport>,
    pub elapsed_ms: u64,
}

/// What checking an embedded subtitle track against the audio found.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EmbeddedCheckReport {
    /// `embedded:<stream>`
    pub track: String,
    /// `samples`, `samples+full`, `full`, or `unchecked` (no usable audio).
    pub method: String,
    /// Whether the track was used as the reference.
    pub used: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub samples: Vec<SampleCheck>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sample_verdict: Option<SampleVerdict>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full: Option<FullCheck>,
    /// Taken from the cache: an earlier sync of the same video checked it.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub cached: bool,
    /// Checks of the regions the segments of a split result rely on.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub regions: Vec<RegionCheck>,
    pub elapsed_ms: u64,
}

/// An embedded subtitle track picked as the reference candidate.
struct EmbeddedTrack {
    label: String,
    stream: usize,
    timeline: Timeline,
    /// Dialogue cues, time-sorted.
    cues: Vec<Interval>,
}

/// The reference a subtitle is aligned against.
struct Reference {
    timeline: Timeline,
    label: String,
    probe: media::Probe,
    /// Set when the reference is an embedded track that passed the check.
    checked: Option<Checked>,
}

struct Checked {
    cues: Vec<Interval>,
    /// The full speech timeline and its label, if the check analysed it.
    audio: Option<(Timeline, String)>,
}

enum Decision {
    Use {
        audio: Option<(Timeline, String)>,
    },
    Reject {
        audio: Option<(Timeline, String)>,
    },
    /// The audio can't be analysed: use the track as it is.
    Unchecked,
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
        // To notice the subtitle being replaced while this sync runs.
        let read_as = FileKey::of(&req.subtitle).ok();
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

        let reference = self.reference(req, dialogue.len(), report)?;
        report.reference = Some(reference.label.clone());
        let t = Instant::now();
        let mut result = pipeline::sync(&file, &reference.timeline, &self.settings.align, &self.settings.gate);
        let mut fallback: Option<Timeline> = None;
        debug!(elapsed_ms = t.elapsed().as_millis() as u64, "aligned");
        // A split result relies on the embedded track's timing in each
        // segment's region; the samples may not have covered those.
        if let Some(checked) = &reference.checked
            && result.alignment.splits() > 0
        {
            let t = Instant::now();
            let regions = self.check_regions(req, &reference.probe, checked, &result.alignment);
            let failed = regions.iter().find(|r| !r.supported).copied();
            if let Some(ec) = report.embedded_check.as_mut() {
                ec.regions = regions;
                ec.elapsed_ms += t.elapsed().as_millis() as u64;
                if let Some(r) = failed {
                    ec.used = false;
                    ec.summary = format!("the part around {} doesn't line up with the audio", clock(r.from_ms));
                }
            }
            if let Some(r) = failed {
                warn!(
                    track = reference.label,
                    from = clock(r.from_ms),
                    "embedded subtitle doesn't line up with the audio where the result splits; using the audio"
                );
                let (tl, label) = match &checked.audio {
                    Some(a) => a.clone(),
                    None => self.audio_reference(req, &reference.probe, report)?,
                };
                report.reference = Some(label);
                result = pipeline::sync(&file, &tl, &self.settings.align, &self.settings.gate);
                fallback = Some(tl);
            }
        }
        let v = &result.verdict;
        if matches!(v.outcome, Outcome::Synced | Outcome::LowConfidence) {
            let placed: Vec<Interval> =
                result.alignment.cues.iter().copied().filter(|c| c.end_ms > c.start_ms).collect();
            let cfg = CheckConfig { expected_lag_ms: 0, ..self.settings.check.clone() };
            report.stretches = verify::scan(fallback.as_ref().unwrap_or(&reference.timeline), &placed, &cfg);
        }
        report.outcome = Some(v.outcome);
        report.reason = v.reason.clone();
        if let Some(hint) = search_edge_hint(v, self.settings.align.max_offset_ms) {
            report.reason.push_str(&hint);
        }
        report.offset_ms = v.offset_ms;
        report.ratio = v.ratio;
        report.splits = v.splits;
        report.score_before = v.score_before;
        report.score_after = v.score_after;
        report.prominence = v.prominence;
        report.max_shift_ms = v.max_shift_ms;
        report.hidden_cues = v.hidden_cues;
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
        if req.cancelled() {
            return Err(media::Cancelled.into());
        }
        let out = result.render(&file).context("rendering subtitle")?;
        // Bazarr may have downloaded a new subtitle in the meantime: writing
        // a shifted copy of the old one over it would lose it. The request
        // that comes with the new file syncs that one.
        if req.output.is_none() && FileKey::of(&req.subtitle).ok() != read_as {
            bail!("the subtitle changed on disk while it was being synced; left it alone");
        }
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
        let req = SyncRequest { video: video.to_path_buf(), reference: mode, dry_run: true, ..Default::default() };
        let r = self.reference(&req, target_cues, &mut SyncReport::default())?;
        Ok((r.timeline, r.label))
    }

    /// Build the reference according to the requested mode.
    fn reference(&self, req: &SyncRequest, target_cues: usize, report: &mut SyncReport) -> Result<Reference> {
        let probe = media::probe(&self.tools(req), &req.video)?;
        if req.reference != ReferenceMode::Audio {
            match self.embedded_reference(req, &probe, target_cues) {
                Ok(Some(track)) => {
                    let mode = match req.reference {
                        ReferenceMode::Embedded => EmbeddedCheck::Off,
                        _ => self.settings.embedded_check,
                    };
                    if mode == EmbeddedCheck::Off {
                        return Ok(Reference { timeline: track.timeline, label: track.label, probe, checked: None });
                    }
                    match self.check_embedded(req, &probe, &track, mode, report) {
                        Decision::Use { audio } => {
                            let checked = Some(Checked { cues: track.cues, audio });
                            return Ok(Reference { timeline: track.timeline, label: track.label, probe, checked });
                        }
                        Decision::Reject { audio: Some((timeline, label)) } => {
                            return Ok(Reference { timeline, label, probe, checked: None });
                        }
                        Decision::Reject { audio: None } => {}
                        Decision::Unchecked => {
                            return Ok(Reference {
                                timeline: track.timeline,
                                label: track.label,
                                probe,
                                checked: None,
                            });
                        }
                    }
                }
                Ok(None) if req.reference == ReferenceMode::Embedded => bail!("no usable embedded text subtitle"),
                Ok(None) => {}
                Err(e) if req.reference == ReferenceMode::Embedded || e.is::<media::Cancelled>() => return Err(e),
                Err(e) => warn!("embedded subtitle reference failed, using audio: {e:#}"),
            }
        }
        let (timeline, label) = self.audio_reference(req, &probe, report)?;
        Ok(Reference { timeline, label, probe, checked: None })
    }

    /// Check an embedded track against the audio: a few samples first, the
    /// full audio if they don't settle it (see `subsyncer_core::verify`).
    /// If the audio can't be analysed at all, the track is used unchecked.
    fn check_embedded(
        &self,
        req: &SyncRequest,
        probe: &media::Probe,
        track: &EmbeddedTrack,
        mode: EmbeddedCheck,
        report: &mut SyncReport,
    ) -> Decision {
        let t0 = Instant::now();
        // The verdict depends only on the track and the audio, so later
        // subtitles for the same video reuse it.
        let key = self.cache.as_ref().and_then(|_| FileKey::of(&req.video).ok());
        let version = self.check_version(mode);
        if let (Some(cache), Some(key)) = (&self.cache, &key) {
            match cache.get_embedded_check(key, track.stream, &version) {
                Ok(Some((used, json))) => match serde_json::from_value::<EmbeddedCheckReport>(json) {
                    Ok(mut rep) => {
                        rep.cached = true;
                        let audio = self.cached_audio(req, probe);
                        report.cache_hit |= audio.is_some();
                        info!(track = rep.track, used, "embedded subtitle checked earlier: {}", rep.summary);
                        report.embedded_check = Some(rep);
                        return if used { Decision::Use { audio } } else { Decision::Reject { audio } };
                    }
                    Err(e) => warn!("cached check unreadable: {e:#}"),
                },
                Ok(None) => {}
                Err(e) => warn!("cache read failed: {e:#}"),
            }
        }
        let cfg = &self.settings.check;
        let mut rep = EmbeddedCheckReport { track: track.label.clone(), ..Default::default() };
        let decision = (|| -> Result<Decision> {
            let stream = probe.default_audio().ok_or_else(|| anyhow!("video has no audio stream"))?;
            let cached = self.cached_audio(req, probe);
            if cached.is_none() && mode == EmbeddedCheck::Sampled {
                let duration = probe.duration_ms.unwrap_or_else(|| track.timeline.duration_ms());
                let plan = verify::plan_samples(&track.cues, duration, cfg);
                if !plan.is_empty() {
                    rep.method = "samples".into();
                    let mut checks = Vec::with_capacity(plan.len());
                    for s in &plan {
                        match self.detect_speech_range(req, stream, s.audio_start_ms, s.audio_len_ms) {
                            Ok(tl) => checks.push(verify::check_sample(&tl, &track.cues, s, cfg)),
                            Err(e) => {
                                warn!("analysing an audio sample failed, checking the full audio: {e:#}");
                                checks.clear();
                                break;
                            }
                        }
                    }
                    let verdict = verify::judge_samples(&checks, cfg);
                    rep.sample_verdict = Some(verdict);
                    rep.samples = checks;
                    match verdict {
                        SampleVerdict::Pass => {
                            let offsets: Vec<String> =
                                rep.samples.iter().map(|c| format!("{:+.2}", c.offset_ms as f64 / 1000.0)).collect();
                            rep.summary = format!(
                                "in sync at {} audio samples (offsets {} s)",
                                rep.samples.len(),
                                offsets.join(" ")
                            );
                            return Ok(Decision::Use { audio: None });
                        }
                        SampleVerdict::Fail => {
                            rep.summary = match rep.samples.iter().find(|c| {
                                c.prominence >= cfg.min_prominence
                                    && (c.offset_ms - cfg.expected_lag_ms).abs() > cfg.window_tolerance_ms
                            }) {
                                Some(c) => format!(
                                    "off by {:+.2} s around {}",
                                    -(c.offset_ms - cfg.expected_lag_ms) as f64 / 1000.0,
                                    clock(c.start_ms)
                                ),
                                None => format!(
                                    "off by {:+.2} s on average",
                                    -(verify::mean_offset(&rep.samples) - cfg.expected_lag_ms) as f64 / 1000.0
                                ),
                            };
                            return Ok(Decision::Reject { audio: None });
                        }
                        SampleVerdict::Inconclusive => rep.method = "samples+full".into(),
                    }
                }
            }
            if rep.method.is_empty() {
                rep.method = "full".into();
            }
            let audio = match cached {
                Some(a) => {
                    report.cache_hit = true;
                    a
                }
                None => self.audio_reference(req, probe, report)?,
            };
            let full = verify::check_full(&audio.0, &track.cues, &self.settings.align, cfg);
            rep.summary = full_summary(&full, cfg);
            let accepted = full.accepted;
            rep.full = Some(full);
            Ok(if accepted { Decision::Use { audio: Some(audio) } } else { Decision::Reject { audio: Some(audio) } })
        })();
        let decision = decision.unwrap_or_else(|e| {
            rep.method = "unchecked".into();
            rep.summary = format!("couldn't check it against the audio ({e:#})");
            Decision::Unchecked
        });
        rep.used = matches!(decision, Decision::Use { .. } | Decision::Unchecked);
        rep.elapsed_ms = t0.elapsed().as_millis() as u64;
        if rep.method == "unchecked" {
            warn!(track = rep.track, "using embedded subtitle unchecked: {}", rep.summary);
        } else {
            if let (Some(cache), Some(key)) = (&self.cache, &key) {
                let stored = serde_json::to_value(&rep)
                    .map_err(anyhow::Error::from)
                    .and_then(|json| cache.put_embedded_check(key, track.stream, &version, rep.used, &json));
                if let Err(e) = stored {
                    warn!("cache write failed: {e:#}");
                }
            }
            info!(
                track = rep.track,
                method = rep.method,
                used = rep.used,
                elapsed_ms = rep.elapsed_ms,
                "embedded subtitle checked: {}",
                rep.summary
            );
        }
        report.embedded_check = Some(rep);
        decision
    }

    /// Check the embedded track in the region each segment of a split result
    /// relies on. A region that can't be checked counts as failed.
    fn check_regions(
        &self,
        req: &SyncRequest,
        probe: &media::Probe,
        checked: &Checked,
        alignment: &Alignment,
    ) -> Vec<RegionCheck> {
        let cfg = &self.settings.check;
        let shifts: Vec<i64> = alignment.segments.iter().map(|s| s.offset_ms).collect();
        alignment
            .segments
            .iter()
            .enumerate()
            .map(|(k, seg)| {
                let mapped = &alignment.cues[seg.first_cue..=seg.last_cue.min(alignment.cues.len() - 1)];
                let from = mapped.iter().map(|c| c.start_ms).min().unwrap_or(0);
                let to = mapped.iter().map(|c| c.end_ms).max().unwrap_or(0);
                let alts = verify::region_alternatives(&shifts, k);
                let s = verify::region_sample(&checked.cues, from, to, &alts, cfg);
                let audio = match &checked.audio {
                    Some((tl, _)) => Ok(verify::slice_for(tl, &s)),
                    None => probe
                        .default_audio()
                        .ok_or_else(|| anyhow!("video has no audio stream"))
                        .and_then(|st| self.detect_speech_range(req, st, s.audio_start_ms, s.audio_len_ms)),
                };
                match audio {
                    Ok(a) => verify::check_region(&a, &checked.cues, &s, &alts, cfg),
                    Err(e) => {
                        warn!("analysing audio for a region check failed: {e:#}");
                        RegionCheck { from_ms: s.start_ms, to_ms: s.start_ms + s.window_ms, ..Default::default() }
                    }
                }
            })
            .collect()
    }

    fn embedded_reference(
        &self,
        req: &SyncRequest,
        probe: &media::Probe,
        target_cues: usize,
    ) -> Result<Option<EmbeddedTrack>> {
        let candidates: Vec<&media::Stream> = probe
            .text_subtitles()
            .filter(|s| {
                let words = s.title.as_deref().map(classify::name_words).unwrap_or_default();
                !s.forced && !words.iter().any(|w| w.starts_with("forced") || w.starts_with("sign"))
            })
            .take(6)
            .collect();
        // Dialogue cues per candidate: from the cache, else extracted (all
        // missing ones in one pass) and cached.
        let key = self.cache.as_ref().and_then(|_| FileKey::of(&req.video).ok());
        let cached = |s: &media::Stream| {
            let (cache, key) = (self.cache.as_ref()?, key.as_ref()?);
            cache.get_embedded_track(key, s.index, TRACK_VERSION).unwrap_or_else(|e| {
                warn!("cache read failed: {e:#}");
                None
            })
        };
        let mut tracks: Vec<(&media::Stream, Option<Vec<Interval>>)> =
            candidates.into_iter().map(|s| (s, cached(s))).collect();
        let missing: Vec<&media::Stream> = tracks.iter().filter(|t| t.1.is_none()).map(|t| t.0).collect();
        let extracted = media::extract_subtitles(&self.tools(req), &req.video, &missing);
        for (s, bytes) in missing.into_iter().zip(extracted) {
            let cues = match bytes {
                Ok(b) => dialogue_intervals(&b),
                Err(e) if e.is::<media::Cancelled>() => return Err(e),
                Err(e) => {
                    warn!(stream = s.index, "extracting embedded subtitle: {e:#}");
                    continue;
                }
            };
            if let (Some(cache), Some(key)) = (&self.cache, &key)
                && let Err(e) = cache.put_embedded_track(key, s.index, TRACK_VERSION, &cues)
            {
                warn!("cache write failed: {e:#}");
            }
            if let Some(t) = tracks.iter_mut().find(|t| t.0.index == s.index) {
                t.1 = Some(cues);
            }
        }
        let min = ((target_cues as f64 * self.settings.min_embedded_cue_ratio).ceil() as usize).max(1);
        let usable: Vec<(&media::Stream, Vec<Interval>)> = tracks
            .into_iter()
            .filter_map(|(s, cues)| {
                let cues = cues?;
                debug!(stream = s.index, cues = cues.len(), min, "embedded subtitle candidate");
                (cues.len() >= min).then_some((s, cues))
            })
            .collect();
        let Some((stream, cues)) = pick_track(usable) else { return Ok(None) };
        let last_end = cues.iter().map(|c| c.end_ms).max().unwrap_or(0);
        let len_frames = probe.duration_ms.unwrap_or(last_end).max(last_end) / FRAME_MS;
        let timeline = Timeline::from_intervals(cues.iter().copied(), Some(len_frames as usize));
        Ok(Some(EmbeddedTrack { label: format!("embedded:{}", stream.index), stream: stream.index, timeline, cues }))
    }

    fn audio_reference(
        &self,
        req: &SyncRequest,
        probe: &media::Probe,
        report: &mut SyncReport,
    ) -> Result<(Timeline, String)> {
        let stream = probe.default_audio().ok_or_else(|| anyhow!("video has no audio stream"))?;
        let filter = media::dialogue_filter(stream);
        let version = timeline_version(filter.as_deref());
        let label = format!("audio:{}", stream.index);
        if let Some(hit) = self.cached_audio(req, probe) {
            report.cache_hit = true;
            return Ok(hit);
        }
        let key = FileKey::of(&req.video).ok();
        let t = Instant::now();
        let tl = match self.detect_speech(req, stream.index, filter.as_deref()) {
            Ok(tl) => tl,
            Err(e) if filter.is_some() && !e.is::<media::Cancelled>() => {
                warn!("centre-channel decode failed ({e:#}); retrying with downmix");
                self.detect_speech(req, stream.index, None)?
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

    /// The full speech timeline from the cache, if it's there.
    fn cached_audio(&self, req: &SyncRequest, probe: &media::Probe) -> Option<(Timeline, String)> {
        let stream = probe.default_audio()?;
        let version = timeline_version(media::dialogue_filter(stream).as_deref());
        let (cache, key) = (self.cache.as_ref()?, FileKey::of(&req.video).ok()?);
        match cache.get_timeline(&key, stream.index, &version) {
            Ok(Some(tl)) => {
                debug!("speech timeline cache hit");
                Some((tl, format!("audio:{}", stream.index)))
            }
            Ok(None) => None,
            Err(e) => {
                warn!("cache read failed: {e:#}");
                None
            }
        }
    }

    /// Fingerprint of the version and everything in the settings that can
    /// change a result (for remembering which results to retry).
    pub fn settings_fingerprint(&self, reference: ReferenceMode) -> String {
        let s = &self.settings;
        crate::cache::fingerprint(&(
            env!("CARGO_PKG_VERSION"),
            vad::VAD_VERSION,
            reference,
            s.embedded_check,
            s.min_embedded_cue_ratio,
            &s.check,
            &s.align,
            &s.gate,
        ))
    }

    /// Cache version of an embedded track's check: everything that can
    /// change its verdict.
    fn check_version(&self, mode: EmbeddedCheck) -> String {
        let s = &self.settings;
        crate::cache::fingerprint(&(env!("CARGO_PKG_VERSION"), vad::VAD_VERSION, mode, &s.check, &s.align))
    }

    /// The tools for one request: its cancel flag stops ffmpeg.
    fn tools(&self, req: &SyncRequest) -> Tools {
        Tools { cancel: req.cancel.clone(), ..self.settings.tools.clone() }
    }

    fn detect_speech(&self, req: &SyncRequest, stream: usize, filter: Option<&str>) -> Result<Timeline> {
        let mut det = SpeechDetector::new()?;
        media::decode_audio(&self.tools(req), &req.video, stream, filter, None, |s| det.push_s16(s))?;
        if det.samples() == 0 {
            bail!("audio stream decoded to zero samples");
        }
        Ok(det.finish()?.0)
    }

    /// Speech timeline of `[start_ms, start_ms + len_ms)` of the audio
    /// (frame 0 is `start_ms`).
    fn detect_speech_range(
        &self,
        req: &SyncRequest,
        stream: &media::Stream,
        start_ms: i64,
        len_ms: i64,
    ) -> Result<Timeline> {
        let run = |filter: Option<&str>| -> Result<Timeline> {
            let mut det = SpeechDetector::new()?;
            let range = Some((start_ms, len_ms));
            media::decode_audio(&self.tools(req), &req.video, stream.index, filter, range, |s| det.push_s16(s))?;
            Ok(det.finish()?.0)
        };
        let filter = media::dialogue_filter(stream);
        match run(filter.as_deref()) {
            Err(e) if filter.is_some() && !e.is::<media::Cancelled>() => {
                debug!("centre-channel decode failed ({e:#}); retrying with downmix");
                run(None)
            }
            r => r,
        }
    }
}

/// Cache version of an embedded track's dialogue cues: which cues count as
/// dialogue can change between versions.
const TRACK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The dialogue cues of an extracted subtitle track, time-sorted. Empty if
/// it can't be parsed.
fn dialogue_intervals(bytes: &[u8]) -> Vec<Interval> {
    let Ok(sub) = SubtitleFile::parse(bytes, None) else { return Vec::new() };
    let mut cues: Vec<Interval> = sub.dialogue_cues().into_iter().map(|c| c.1).collect();
    cues.sort();
    cues
}

/// The embedded track to use as the reference: the one with the most
/// dialogue cues. A track for the hearing impaired (SDH) is passed over for
/// a regular one with nearly as many cues: its extra lines describe sounds,
/// and its timing tends to follow them rather than the dialogue.
fn pick_track(mut tracks: Vec<(&media::Stream, Vec<Interval>)>) -> Option<(&media::Stream, Vec<Interval>)> {
    // The first of the tracks with the most cues among those `keep` allows.
    let best = |keep: &dyn Fn(&media::Stream) -> bool| {
        tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| keep(t.0))
            .max_by_key(|(i, t)| (t.1.len(), std::cmp::Reverse(*i)))
            .map(|(i, t)| (i, t.1.len()))
    };
    let (first, most) = best(&|_| true)?;
    let pick = match best(&|s| !s.hearing_impaired) {
        Some((i, n)) if n * 10 >= most * 9 => i,
        _ => first,
    };
    Some(tracks.swap_remove(pick))
}

/// A low-confidence best offset this close to the end of the search range
/// suggests the real offset lies beyond it.
const EDGE_MS: i64 = 2_000;

/// For a low-confidence result whose best offset is at the edge of the
/// search range: a hint to search further.
fn search_edge_hint(v: &subsyncer_core::Verdict, max_offset_ms: i64) -> Option<String> {
    (v.outcome == Outcome::LowConfidence && max_offset_ms > 0 && max_offset_ms - v.offset_ms.abs() <= EDGE_MS).then(
        || {
            format!(
                "; the best match is at the edge of the ±{} s search range, so the subtitle may be off by more \
                 (try a larger SUBSYNCER_MAX_OFFSET_SECS)",
                max_offset_ms / 1000
            )
        },
    )
}

/// Cache version of a speech timeline: VAD settings plus the audio filter.
fn timeline_version(filter: Option<&str>) -> String {
    format!("{}|{}", vad::VAD_VERSION, filter.unwrap_or("downmix"))
}

/// `m:ss` or `h:mm:ss`.
fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Why the full check accepted or rejected a track, in a few words.
fn full_summary(f: &FullCheck, cfg: &CheckConfig) -> String {
    if f.accepted {
        return format!("in sync with the full audio (confidence {:.1})", f.confidence);
    }
    if f.ratio != 1.0 {
        format!("needs a frame-rate change (x{:.5})", f.ratio)
    } else if let Some(w) = f.disagreeing.first() {
        format!(
            "the part around {} is off by {:+.2} s",
            clock(w.start_ms),
            -(w.offset_ms - cfg.expected_lag_ms) as f64 / 1000.0
        )
    } else if f.max_error_ms > cfg.tolerance_ms {
        format!("off by up to {:.2} s", f.max_error_ms as f64 / 1000.0)
    } else if f.confidence < cfg.min_confidence {
        format!("doesn't line up clearly with the audio (confidence {:.1})", f.confidence)
    } else if !f.end_supported {
        "the end doesn't line up with the audio".to_string()
    } else {
        "parts of it don't line up with the audio".to_string()
    }
}

/// `<path>.bak`
pub fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Write via a temp file in the same directory and rename over the target.
///
/// - A symlink is followed: the file it points to is replaced (and backed
///   up next to it), and the link stays.
/// - The new file keeps the old one's permissions and, where allowed, its
///   owner. A new file gets the permissions the umask allows.
/// - With `backup`, the current file is first copied to `<file>.bak`,
///   unless that backup is still the one from before subsyncer's last
///   change of this file. Returns the backup path, if there is one.
pub fn write_atomic(target: &Path, bytes: &[u8], backup: bool) -> Result<Option<PathBuf>> {
    let is_link = std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink());
    let target = if is_link {
        std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf())
    } else {
        target.to_path_buf()
    };
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let existing = std::fs::metadata(&target).ok();
    let mut builder = tempfile::Builder::new();
    builder.prefix(".subsyncer-").suffix(".tmp");
    if existing.is_none() {
        // The temp file is private (0600) by default; the umask still applies.
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut tmp = builder.tempfile_in(dir).with_context(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    if let Some(meta) = &existing {
        let _ = std::fs::set_permissions(tmp.path(), meta.permissions());
        // Only root may give a file away; anyone may keep their own.
        let _ = std::os::unix::fs::chown(tmp.path(), Some(meta.uid()), Some(meta.gid()));
    }
    // The backup is made after the temp file is written, so the replaced
    // file's mtime is never later than its backup's: see `backup_is_current`.
    let bak = match (&existing, backup) {
        (Some(meta), true) => {
            let b = backup_path(&target);
            if !backup_is_current(meta, &b) {
                std::fs::copy(&target, &b).with_context(|| format!("writing backup {}", b.display()))?;
            }
            Some(b)
        }
        _ => None,
    };
    tmp.persist(&target).map_err(|e| anyhow!("replacing {}: {}", target.display(), e.error))?;
    Ok(bak)
}

/// Whether `bak` still holds what the file was before subsyncer last
/// changed it: the file hasn't been modified since the backup was made.
/// Once something else replaces the file (a new download, an edit), the
/// backup is out of date and gets refreshed.
fn backup_is_current(file: &std::fs::Metadata, bak: &Path) -> bool {
    match (file.modified(), std::fs::metadata(bak).and_then(|m| m.modified())) {
        (Ok(f), Ok(b)) => f <= b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdh_tracks_give_way_to_regular_ones_with_nearly_as_many_cues() {
        let s = |index, hearing_impaired| media::Stream { index, hearing_impaired, ..Default::default() };
        let (a, b, c) = (s(2, false), s(3, true), s(4, false));
        let cues = |n: i64| (0..n).map(|k| Interval::new(k * 1000, k * 1000 + 500)).collect::<Vec<_>>();
        let pick = |t: Vec<(&media::Stream, Vec<Interval>)>| pick_track(t).map(|p| p.0.index);
        assert_eq!(pick(vec![(&a, cues(95)), (&b, cues(100))]), Some(2));
        assert_eq!(pick(vec![(&a, cues(80)), (&b, cues(100))]), Some(3));
        assert_eq!(pick(vec![(&b, cues(100))]), Some(3));
        assert_eq!(pick(vec![(&a, cues(100)), (&c, cues(100))]), Some(2), "ties go to the first");
        assert_eq!(pick(vec![(&a, cues(90)), (&c, cues(100))]), Some(4));
        assert_eq!(pick(vec![]), None);
    }

    #[test]
    fn hint_at_the_edge_of_the_search_range() {
        let v = |outcome, offset_ms| subsyncer_core::Verdict {
            outcome,
            offset_ms,
            ratio: 1.0,
            splits: 0,
            score_before: 0.2,
            score_after: 0.2,
            prominence: 3.0,
            max_shift_ms: offset_ms.abs(),
            hidden_cues: 0,
            reason: String::new(),
        };
        assert!(search_edge_hint(&v(Outcome::LowConfidence, -59_200), 60_000).unwrap().contains("±60 s"));
        assert!(search_edge_hint(&v(Outcome::LowConfidence, 60_000), 60_000).is_some());
        assert!(search_edge_hint(&v(Outcome::LowConfidence, 40_000), 60_000).is_none());
        assert!(search_edge_hint(&v(Outcome::Synced, 59_900), 60_000).is_none());
    }

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

    fn touch_later(path: &Path) {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(later).unwrap();
    }

    #[test]
    fn backup_keeps_the_original_until_the_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.srt");
        let bak = backup_path(&p);
        std::fs::write(&p, b"download 1").unwrap();
        write_atomic(&p, b"synced 1", true).unwrap();
        // Syncing subsyncer's own output again keeps the original.
        write_atomic(&p, b"synced 1 again", true).unwrap();
        assert_eq!(std::fs::read(&bak).unwrap(), b"download 1");
        // A new download replaces the file: its backup is refreshed.
        std::fs::write(&p, b"download 2").unwrap();
        touch_later(&p);
        write_atomic(&p, b"synced 2", true).unwrap();
        assert_eq!(std::fs::read(&bak).unwrap(), b"download 2");
        assert_eq!(std::fs::read(&p).unwrap(), b"synced 2");
    }

    #[test]
    fn new_files_get_umask_permissions_and_links_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        // What the umask gives a plain new file.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        let want = std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777;
        let out = dir.path().join("out.srt");
        assert!(write_atomic(&out, b"x", true).unwrap().is_none(), "nothing to back up");
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, want);

        let real = dir.path().join("real.srt");
        std::fs::write(&real, b"old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("link.srt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let bak = write_atomic(&link, b"new", true).unwrap().unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(bak, std::fs::canonicalize(dir.path()).unwrap().join("real.srt.bak"));
        assert_eq!(std::fs::read(&bak).unwrap(), b"old");
    }
}
