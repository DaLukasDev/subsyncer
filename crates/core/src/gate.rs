//! Quality gate: decide whether an alignment is trustworthy enough to write.

use crate::align::{Alignment, overlap_score};
use crate::timeline::{Interval, Timeline};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
}

impl Default for GateConfig {
    fn default() -> Self {
        Self { already_synced_ms: 100, min_confidence: 6.0, min_improvement: 0.01, small_shift_ms: 300 }
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
