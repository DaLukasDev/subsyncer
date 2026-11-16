//! Subtitle parsing and timestamp-only rewriting for SRT, WebVTT and ASS/SSA.
//!
//! Parsers record the byte span of every timestamp in the file. Rendering
//! replaces only those spans; everything else is copied verbatim, so an
//! unshifted file renders byte-identical to its input.

mod ass;
mod srt;
pub mod timestamp;

use std::borrow::Cow;
use std::ops::Range;

use crate::Error;
use crate::classify;
use crate::encoding::{self, Prepared, TextEncoding};
use crate::timeline::Interval;
pub use timestamp::TsStyle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Srt,
    Ass,
    Vtt,
}

impl Format {
    /// Guess from a file extension (without the dot).
    pub fn from_extension(ext: &str) -> Option<Format> {
        match ext.to_ascii_lowercase().as_str() {
            "srt" => Some(Format::Srt),
            "ass" | "ssa" => Some(Format::Ass),
            "vtt" | "webvtt" => Some(Format::Vtt),
            _ => None,
        }
    }

    /// Detect the format from the (ASCII-compatible) content.
    pub fn detect(working: &[u8]) -> Format {
        let head = &working[..working.len().min(64 * 1024)];
        let trimmed = trim_start(head);
        if trimmed.starts_with(b"WEBVTT") {
            return Format::Vtt;
        }
        let lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
        if contains(&lower, b"[script info]") || contains(&lower, b"[events]") || contains(&lower, b"\ndialogue:") {
            return Format::Ass;
        }
        Format::Srt
    }
}

/// Which timestamp of a cue is being mapped during rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsKind {
    Start,
    End,
    /// Timestamp inside the cue text (WebVTT karaoke).
    Inline,
}

/// A timestamp occurrence in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub ms: i64,
    pub span: Range<usize>,
    pub style: TsStyle,
}

/// One cue (SRT/VTT block or ASS `Dialogue:`/`Comment:` line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cue {
    pub start: Timestamp,
    pub end: Timestamp,
    /// Timestamps inside the cue text (WebVTT karaoke `<00:00:01.000>`).
    pub inline: Vec<Timestamp>,
    /// Raw text span (markup included).
    pub text: Range<usize>,
    /// ASS `Comment:` line (never displayed).
    pub comment: bool,
    /// ASS style name span.
    pub style: Option<Range<usize>>,
}

impl Cue {
    pub fn interval(&self) -> Interval {
        Interval::new(self.start.ms, self.end.ms)
    }
}

/// A parsed subtitle file that can be re-rendered with new timings.
#[derive(Clone, Debug)]
pub struct SubtitleFile {
    format: Format,
    prepared: Prepared,
    cues: Vec<Cue>,
}

impl SubtitleFile {
    /// Parse a subtitle file. The format is detected from the content;
    /// `hint` (usually from the extension) is used when detection is not
    /// conclusive.
    pub fn parse(bytes: &[u8], hint: Option<Format>) -> Result<Self, Error> {
        let prepared = encoding::prepare(bytes)?;
        let detected = Format::detect(&prepared.working);
        let format = match (detected, hint) {
            (Format::Srt, Some(h)) => h, // SRT is the fallback, so trust the hint
            (d, _) => d,
        };
        let mut cues = parse_as(format, &prepared.working);
        let mut format = format;
        if cues.is_empty() && format != detected {
            cues = parse_as(detected, &prepared.working);
            format = detected;
        }
        if cues.is_empty() {
            return Err(Error::Parse("no subtitle cues found".into()));
        }
        Ok(Self { format, prepared, cues })
    }

    pub fn format(&self) -> Format {
        self.format
    }
    pub fn encoding(&self) -> TextEncoding {
        self.prepared.encoding
    }
    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }

    /// Raw (markup included) decoded text of cue `i`.
    pub fn cue_text(&self, i: usize) -> Cow<'_, str> {
        self.prepared.decode(&self.prepared.working[self.cues[i].text.clone()])
    }

    fn cue_style(&self, i: usize) -> Option<Cow<'_, str>> {
        self.cues[i].style.clone().map(|r| self.prepared.decode(&self.prepared.working[r]))
    }

    /// Whether cue `i` likely corresponds to spoken dialogue (as opposed to
    /// sound descriptions, music, signs or comments).
    pub fn is_dialogue(&self, i: usize) -> bool {
        let c = &self.cues[i];
        if c.comment || c.end.ms <= c.start.ms {
            return false;
        }
        classify::is_dialogue(&self.cue_text(i), self.format, self.cue_style(i).as_deref())
    }

    /// Indices of dialogue cues sorted by start time, with their intervals.
    /// Falls back to all non-comment cues if the classifier rejects nearly
    /// everything (e.g. a file using unusual markup).
    pub fn dialogue_cues(&self) -> Vec<(usize, Interval)> {
        let all: Vec<usize> = (0..self.cues.len())
            .filter(|&i| !self.cues[i].comment && self.cues[i].end.ms > self.cues[i].start.ms)
            .collect();
        let dlg: Vec<usize> = all.iter().copied().filter(|&i| self.is_dialogue(i)).collect();
        let chosen = if dlg.len() * 4 >= all.len() { dlg } else { all };
        let mut out: Vec<(usize, Interval)> = chosen.into_iter().map(|i| (i, self.cues[i].interval())).collect();
        out.sort_by_key(|&(i, iv)| (iv.start_ms, iv.end_ms, i));
        out
    }

    /// The original file bytes.
    pub fn original_bytes(&self) -> Result<Vec<u8>, Error> {
        self.prepared.finish(&self.prepared.working)
    }

    /// Render the file with every timestamp of cue `i` mapped through
    /// `map(i, kind, ms)`. Unchanged timestamps keep their original text
    /// exactly.
    pub fn render(&self, mut map: impl FnMut(usize, TsKind, i64) -> i64) -> Result<Vec<u8>, Error> {
        let mut edits: Vec<(Range<usize>, String)> = Vec::new();
        for (i, cue) in self.cues.iter().enumerate() {
            let all = [(TsKind::Start, &cue.start), (TsKind::End, &cue.end)]
                .into_iter()
                .chain(cue.inline.iter().map(|t| (TsKind::Inline, t)));
            for (kind, ts) in all {
                let new = map(i, kind, ts.ms).max(0);
                let res = ts.style.resolution_ms();
                // Compare at the file's resolution so sub-resolution noise
                // doesn't rewrite a timestamp to the same value.
                if (new + res / 2) / res * res != ts.ms {
                    edits.push((ts.span.clone(), timestamp::format(new, ts.style)));
                }
            }
        }
        edits.sort_by_key(|(r, _)| r.start);
        let src = &self.prepared.working;
        let mut out = Vec::with_capacity(src.len() + edits.len() * 2);
        let mut pos = 0;
        for (r, s) in edits {
            debug_assert!(r.start >= pos, "overlapping timestamp spans");
            out.extend_from_slice(&src[pos..r.start]);
            out.extend_from_slice(s.as_bytes());
            pos = r.end;
        }
        out.extend_from_slice(&src[pos..]);
        self.prepared.finish(&out)
    }
}

fn parse_as(format: Format, working: &[u8]) -> Vec<Cue> {
    match format {
        Format::Srt => srt::parse(working, false),
        Format::Vtt => srt::parse(working, true),
        Format::Ass => ass::parse(working),
    }
}

/// Iterate lines as (content range without line terminator, next line start).
pub(crate) fn lines(b: &[u8]) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= b.len() {
            return None;
        }
        let start = pos;
        let end = match b[pos..].iter().position(|&c| c == b'\n') {
            Some(off) => {
                pos = start + off + 1;
                start + off
            }
            None => {
                pos = b.len();
                b.len()
            }
        };
        let end = if end > start && b[end - 1] == b'\r' { end - 1 } else { end };
        Some(start..end)
    })
}

pub(crate) fn trim_start(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
    &b[n..]
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests;
