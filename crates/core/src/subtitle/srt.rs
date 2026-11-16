//! SRT and WebVTT: blocks with a `start --> end` timing line.

use super::{Cue, Timestamp, lines, timestamp};

/// Parse a `start --> end` timing line. Returns the two timestamps.
fn timing_line(b: &[u8], line: std::ops::Range<usize>) -> Option<(Timestamp, Timestamp)> {
    let mut i = line.start;
    while i < line.end && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let (s_ms, s_end, s_style) = timestamp::parse(&b[..line.end], i)?;
    let start = Timestamp { ms: s_ms, span: i..s_end, style: s_style };
    let mut j = s_end;
    while j < line.end && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    let arrow = if b[j..line.end].starts_with(b"-->") {
        3
    } else if b[j..line.end].starts_with(b"->") {
        2
    } else {
        return None;
    };
    j += arrow;
    while j < line.end && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    let (e_ms, e_end, e_style) = timestamp::parse(&b[..line.end], j)?;
    let end = Timestamp { ms: e_ms, span: j..e_end, style: e_style };
    // Anything after must be separated by whitespace (VTT cue settings,
    // SRT coordinates).
    if e_end < line.end && !(b[e_end] == b' ' || b[e_end] == b'\t') {
        return None;
    }
    Some((start, end))
}

/// WebVTT inline timestamps `<HH:MM:SS.mmm>` inside cue text.
fn inline_timestamps(b: &[u8], text: std::ops::Range<usize>) -> Vec<Timestamp> {
    let mut out = Vec::new();
    let mut i = text.start;
    while i < text.end {
        if b[i] == b'<'
            && let Some((ms, end, style)) = timestamp::parse(&b[..text.end], i + 1)
            && end < text.end
            && b[end] == b'>'
            && style.frac_digits > 0
        {
            out.push(Timestamp { ms, span: i + 1..end, style });
            i = end + 1;
            continue;
        }
        i += 1;
    }
    out
}

pub fn parse(b: &[u8], vtt: bool) -> Vec<Cue> {
    let all: Vec<_> = lines(b).collect();
    let mut cues = Vec::new();
    let mut k = 0;
    while k < all.len() {
        let Some((start, end)) = timing_line(b, all[k].clone()) else {
            k += 1;
            continue;
        };
        // Text runs until a blank line or the next timing line.
        let first = k + 1;
        let mut last = first;
        while last < all.len() {
            let l = all[last].clone();
            if b[l.clone()].iter().all(|c| c.is_ascii_whitespace()) || timing_line(b, l).is_some() {
                break;
            }
            last += 1;
        }
        let text = if last > first { all[first].start..all[last - 1].end } else { all[k].end..all[k].end };
        let inline = if vtt { inline_timestamps(b, text.clone()) } else { Vec::new() };
        cues.push(Cue { start, end, inline, text, comment: false, style: None });
        k = last.max(first);
    }
    cues
}
