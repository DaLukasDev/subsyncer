//! Quality gate: decide whether an alignment is trustworthy enough to write.

use crate::align::{Alignment, overlap_score};
use crate::timeline::{Interval, Timeline};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GateConfig {
    /// If no cue moves by more than this, the file is already in sync.
    pub already_synced_ms: i64,
    /// Minimum correlation peak prominence (z-score).
    pub min_confidence: f64,
    /// Minimum gain in speech-overlap fraction required to write.
    pub min_improvement: f64,
    /// Corrections up to this size that do not reach `min_improvement` are
    /// reported as already synced rather than low confidence (subtitle
    /// lead-in/lead-out conventions make small offsets ambiguous).
    pub small_shift_ms: i64,
    /// Largest share of dialogue lines a result may hide (content the video
    /// lacks, or lines moved before the start of the video). A subtitle that
    /// loses more than this is more likely for another version of the video.
    pub max_hidden_fraction: f64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            already_synced_ms: 100,
            min_confidence: 6.0,
            min_improvement: 0.01,
            small_shift_ms: 300,
            max_hidden_fraction: 0.25,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    AlreadySynced,
    Synced,
    LowConfidence,
    Failed,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::AlreadySynced => "already_synced",
            Outcome::Synced => "synced",
            Outcome::LowConfidence => "low_confidence",
            Outcome::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    pub outcome: Outcome,
    /// Global offset (ms) at the chosen ratio.
    pub offset_ms: i64,
    pub ratio: f64,
    pub splits: usize,
    /// Fraction of dialogue cue time overlapping reference speech.
    pub score_before: f64,
    pub score_after: f64,
    /// Correlation peak prominence (z-score).
    pub prominence: f64,
    /// Largest change of any dialogue cue start, ms.
    pub max_shift_ms: i64,
    /// Dialogue cues the result hides (zero length): content the video
    /// lacks, or cues moved entirely before the start of the video.
    #[serde(default)]
    pub hidden_cues: usize,
    pub reason: String,
}

impl Verdict {
    /// Whether the corrected subtitle should be written.
    pub fn should_write(&self) -> bool {
        self.outcome == Outcome::Synced
    }
}

/// Judge `alignment` of `original` (time-sorted dialogue cues) against
/// `reference`.
pub fn evaluate(reference: &Timeline, original: &[Interval], alignment: &Alignment, cfg: &GateConfig) -> Verdict {
    let score_before = overlap_score(reference, original);
    let score_after = overlap_score(reference, &alignment.cues);
    let max_shift_ms =
        original.iter().zip(&alignment.cues).map(|(a, b)| (a.start_ms - b.start_ms).abs()).max().unwrap_or(0);
    // Times are clamped to zero when written, so a cue that ends before the
    // video starts is hidden too.
    let hidden_cues = original
        .iter()
        .zip(&alignment.cues)
        .filter(|(o, c)| o.end_ms > o.start_ms && c.end_ms.max(0) <= c.start_ms.max(0))
        .count();
    let g = &alignment.global;
    let mut v = Verdict {
        outcome: Outcome::Failed,
        offset_ms: g.offset_ms,
        ratio: g.ratio,
        splits: alignment.splits(),
        score_before,
        score_after,
        prominence: alignment.confidence,
        max_shift_ms,
        hidden_cues,
        reason: String::new(),
    };
    let improvement = score_after - score_before;
    (v.outcome, v.reason) = if original.is_empty() {
        (Outcome::Failed, "subtitle has no dialogue cues".into())
    } else if reference.active_fraction() < 0.005 {
        (Outcome::Failed, "reference contains no speech".into())
    } else if max_shift_ms < cfg.already_synced_ms {
        (Outcome::AlreadySynced, format!("largest correction {max_shift_ms} ms is below {} ms", cfg.already_synced_ms))
    } else if alignment.confidence < cfg.min_confidence {
        (
            Outcome::LowConfidence,
            format!("alignment confidence {:.2} below {:.2}", alignment.confidence, cfg.min_confidence),
        )
    } else if hidden_cues as f64 > cfg.max_hidden_fraction * original.len() as f64 {
        (
            Outcome::LowConfidence,
            format!(
                "the result would hide {hidden_cues} of {} lines (no matching part in the video), more than {:.0}%",
                original.len(),
                cfg.max_hidden_fraction * 100.0
            ),
        )
    } else if improvement < cfg.min_improvement && max_shift_ms <= cfg.small_shift_ms {
        (
            Outcome::AlreadySynced,
            format!(
                "small correction ({max_shift_ms} ms) gives no measurable gain ({:.3} -> {:.3})",
                score_before, score_after
            ),
        )
    } else if improvement < cfg.min_improvement {
        (
            Outcome::LowConfidence,
            format!(
                "speech overlap would change {:.3} -> {:.3}, below required gain {:.3}",
                score_before, score_after, cfg.min_improvement
            ),
        )
    } else {
        (Outcome::Synced, format!("speech overlap {:.3} -> {:.3}", score_before, score_after))
    };
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::{GlobalResult, Mapping};

    fn alignment(cues: Vec<Interval>) -> Alignment {
        let global = GlobalResult {
            ratio: 1.0,
            offset_ms: -5_000,
            score: 1.0,
            prominence: 20.0,
            candidates: vec![],
            peaks: vec![],
        };
        Alignment { global, segments: vec![], mapping: Mapping::constant(1.0, -5_000), cues, confidence: 20.0 }
    }

    #[test]
    fn hiding_too_many_lines_is_low_confidence() {
        let speech: Vec<Interval> = (0..20).map(|k| Interval::new(k * 3_000, k * 3_000 + 2_000)).collect();
        let reference = Timeline::from_intervals(speech.clone(), Some(6_000));
        let original: Vec<Interval> =
            speech.iter().map(|c| Interval::new(c.start_ms + 5_000, c.end_ms + 5_000)).collect();
        let cfg = GateConfig::default();

        let v = evaluate(&reference, &original, &alignment(speech.clone()), &cfg);
        assert_eq!((v.outcome, v.hidden_cues), (Outcome::Synced, 0));

        // 5 of 20 hidden (25%) is still fine; 6 (30%) is not.
        let mut cues = speech.clone();
        for c in cues.iter_mut().take(5) {
            c.end_ms = c.start_ms;
        }
        let v = evaluate(&reference, &original, &alignment(cues.clone()), &cfg);
        assert_eq!((v.outcome, v.hidden_cues), (Outcome::Synced, 5));
        cues[5] = Interval::new(-3_000, -1_000); // before the start of the video
        let v = evaluate(&reference, &original, &alignment(cues), &cfg);
        assert_eq!((v.outcome, v.hidden_cues), (Outcome::LowConfidence, 6), "{}", v.reason);
    }
}
