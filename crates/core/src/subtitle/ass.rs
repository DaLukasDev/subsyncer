//! ASS / SSA: `Dialogue:` and `Comment:` lines in the `[Events]` section.

use super::{Cue, Timestamp, lines, timestamp};

struct Fields {
    count: usize,
    start: usize,
    end: usize,
    style: Option<usize>,
}

const DEFAULT: Fields = Fields { count: 10, start: 1, end: 2, style: Some(3) };

fn parse_format(b: &[u8]) -> Option<Fields> {
    let names: Vec<String> =
        b.split(|&c| c == b',').map(|f| String::from_utf8_lossy(f).trim().to_ascii_lowercase()).collect();
    let find = |n: &str| names.iter().position(|x| x == n);
    let f = Fields { count: names.len(), start: find("start")?, end: find("end")?, style: find("style") };
    // Text must be the last field so commas inside it are preserved.
    if names.last().map(|s| s.as_str()) != Some("text") {
        return None;
    }
    Some(f)
}

pub fn parse(b: &[u8]) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut section: Option<Vec<u8>> = None;
    let mut fields = DEFAULT;
    for line in lines(b) {
        let l = &b[line.clone()];
        let lt = super::trim_start(l);
        if lt.starts_with(b"[") {
            section = Some(lt.iter().map(|c| c.to_ascii_lowercase()).collect());
            continue;
        }
        let in_events = match &section {
            None => true,
            Some(s) => s.starts_with(b"[events]"),
        };
        if !in_events {
            continue;
        }
        let offset = line.start + (l.len() - lt.len());
        if let Some(rest) = lt.strip_prefix(b"Format:") {
            if let Some(f) = parse_format(rest) {
                fields = f;
            }
            continue;
        }
        let (comment, body_off) = if lt.starts_with(b"Dialogue:") {
            (false, 9)
        } else if lt.starts_with(b"Comment:") {
            (true, 8)
        } else {
            continue;
        };
        // Split into `fields.count` fields; the last (Text) keeps its commas.
        let body_start = offset + body_off;
        let mut spans = Vec::with_capacity(fields.count);
        let mut fs = body_start;
        for (i, &c) in b.iter().enumerate().take(line.end).skip(body_start) {
            if spans.len() + 1 == fields.count {
                break;
            }
            if c == b',' {
                spans.push(fs..i);
                fs = i + 1;
            }
        }
        if spans.len() + 1 != fields.count {
            continue;
        }
        spans.push(fs..line.end);
        let ts = |idx: usize| -> Option<Timestamp> {
            let r = spans[idx].clone();
            let mut s = r.start;
            while s < r.end && b[s] == b' ' {
                s += 1;
            }
            let (ms, e, style) = timestamp::parse(&b[..r.end], s)?;
            if b[e..r.end].iter().any(|c| !c.is_ascii_whitespace()) {
                return None;
            }
            Some(Timestamp { ms, span: s..e, style })
        };
        let (Some(start), Some(end)) = (ts(fields.start), ts(fields.end)) else { continue };
        let style = fields.style.map(|i| {
            let r = spans[i].clone();
            let s = r.start + b[r.clone()].iter().take_while(|c| **c == b' ').count();
            s..r.end.max(s)
        });
        cues.push(Cue { start, end, inline: Vec::new(), text: spans[fields.count - 1].clone(), comment, style });
    }
    cues
}
