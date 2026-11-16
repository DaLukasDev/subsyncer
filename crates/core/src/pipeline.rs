//! High-level, IO-free sync: subtitle + reference timeline -> verdict and
//! corrected bytes.

use crate::Error;
use crate::align::{self, AlignConfig, Alignment};
use crate::gate::{self, GateConfig, Verdict};
use crate::subtitle::{SubtitleFile, TsKind};
use crate::timeline::{Interval, Timeline};

#[derive(Clone, Debug)]
pub struct SyncResult {
    pub verdict: Verdict,
    pub alignment: Alignment,
    /// (cue index in the file, original interval), time-sorted; the cues
    /// that were aligned.
    pub dialogue: Vec<(usize, Interval)>,
}

impl SyncResult {
    /// Render the corrected subtitle.
    pub fn render(&self, file: &SubtitleFile) -> Result<Vec<u8>, Error> {
        render_aligned(file, &self.dialogue, &self.alignment)
    }
}

/// Align `file` against `reference` and judge the result.
pub fn sync(file: &SubtitleFile, reference: &Timeline, align_cfg: &AlignConfig, gate_cfg: &GateConfig) -> SyncResult {
    let dialogue = file.dialogue_cues();
    let intervals: Vec<Interval> = dialogue.iter().map(|&(_, iv)| iv).collect();
    let alignment = align::align(reference, &intervals, align_cfg);
    let verdict = gate::evaluate(reference, &intervals, &alignment, gate_cfg);
    SyncResult { verdict, alignment, dialogue }
}

/// Apply an alignment to every cue of `file`. Aligned (dialogue) cues get
/// their computed intervals; all other timestamps go through the mapping,
/// anchored at their cue's original start.
pub fn render_aligned(
    file: &SubtitleFile,
    dialogue: &[(usize, Interval)],
    alignment: &Alignment,
) -> Result<Vec<u8>, Error> {
    let mut explicit: Vec<Option<Interval>> = vec![None; file.cues().len()];
    for (k, &(i, _)) in dialogue.iter().enumerate() {
        if let Some(iv) = alignment.cues.get(k) {
            explicit[i] = Some(*iv);
        }
    }
    let cues = file.cues();
    file.render(|i, kind, ms| {
        let anchor = cues[i].start.ms;
        match (kind, explicit[i]) {
            (TsKind::Start, Some(iv)) => iv.start_ms,
            (TsKind::End, Some(iv)) => iv.end_ms,
            (TsKind::Inline, Some(iv)) => {
                alignment.mapping.apply(anchor, ms).clamp(iv.start_ms, iv.end_ms.max(iv.start_ms))
            }
            (_, None) => alignment.mapping.apply(anchor, ms),
        }
    })
}
