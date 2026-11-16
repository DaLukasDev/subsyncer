//! Heuristics for whether a cue is spoken dialogue.
//!
//! Sound descriptions (`[door slams]`), music (`♪ ... ♪`) and ASS signs have
//! no matching speech in the audio, so they are left out of alignment (they
//! are still shifted along with their neighbours).

use crate::subtitle::Format;

/// Strip markup: HTML-like tags, ASS override blocks and ASS escapes.
pub fn strip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // Skip a tag if it closes on this cue; otherwise keep the '<'.
                let rest: String = chars.clone().collect();
                if let Some(end) = rest.find('>') {
                    for _ in 0..rest[..=end].chars().count() {
                        chars.next();
                    }
                } else {
                    out.push(c);
                }
            }
            '{' => {
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        break;
                    }
                }
            }
            '\\' if matches!(chars.peek(), Some('N' | 'n' | 'h')) => {
                chars.next();
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

fn remove_bracketed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '[' | '(' | '（' | '【' => depth += 1,
            ']' | ')' | '）' | '】' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// `text` is the raw cue text, `style` the ASS style name if any.
pub fn is_dialogue(text: &str, format: Format, style: Option<&str>) -> bool {
    if format == Format::Ass {
        // Vector drawings, positioned/moving lines and sign styles are
        // typesetting, not speech.
        let t = text.to_ascii_lowercase();
        if t.contains("\\pos(") || t.contains("\\move(") || has_drawing_tag(&t) {
            return false;
        }
        if let Some(s) = style {
            let s = s.to_ascii_lowercase();
            if s.contains("sign") || s.contains("song") || s.contains("karaoke") || s.contains("title") {
                return false;
            }
        }
    }
    let plain = strip_markup(text);
    let trimmed = plain.trim();
    if trimmed.starts_with(['♪', '♫', '#', '¶']) {
        return false; // sung lyrics / music
    }
    let spoken = remove_bracketed(trimmed);
    spoken.chars().any(|c| c.is_alphanumeric())
}

fn has_drawing_tag(t: &str) -> bool {
    // `\p1`, `\p2`, ... (but not `\pos`, `\pbo`)
    t.match_indices("\\p").any(|(i, _)| t[i + 2..].chars().next().is_some_and(|c| c.is_ascii_digit() && c != '0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogue_detection() {
        assert!(is_dialogue("Hello there.", Format::Srt, None));
        assert!(is_dialogue("<i>Hello</i>", Format::Srt, None));
        assert!(is_dialogue("[John] Hello", Format::Srt, None));
        assert!(!is_dialogue("[door slams]", Format::Srt, None));
        assert!(!is_dialogue("(APPLAUSE)", Format::Srt, None));
        assert!(!is_dialogue("♪ la la la ♪", Format::Srt, None));
        assert!(!is_dialogue("<i>♪ Music ♪</i>", Format::Srt, None));
        assert!(!is_dialogue("  ", Format::Srt, None));
        assert!(is_dialogue("{\\i1}Hello{\\i0}\\NWorld", Format::Ass, Some("Default")));
        assert!(!is_dialogue("{\\pos(100,200)}EXIT", Format::Ass, Some("Default")));
        assert!(!is_dialogue("Hello", Format::Ass, Some("Signs")));
        assert!(!is_dialogue("{\\p1}m 0 0 l 100 0", Format::Ass, Some("Default")));
        assert!(is_dialogue("{\\pbo0}Hi", Format::Ass, Some("Default")));
    }
}
