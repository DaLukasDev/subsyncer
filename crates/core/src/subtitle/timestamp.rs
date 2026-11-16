//! Timestamp parsing and style-preserving formatting.

/// Formatting style of a timestamp as it appeared in the file, so rewritten
/// timestamps look like the originals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsStyle {
    /// Digits in the hour field; 0 when the hour field was absent (WebVTT
    /// `MM:SS.mmm`).
    pub hour_digits: u8,
    /// Separator before the fraction (`,` or `.`), 0 if there was none.
    pub sep: u8,
    /// Number of fraction digits (3 = ms, 2 = centiseconds for ASS).
    pub frac_digits: u8,
}

impl TsStyle {
    pub const SRT: TsStyle = TsStyle { hour_digits: 2, sep: b',', frac_digits: 3 };
    pub const VTT: TsStyle = TsStyle { hour_digits: 2, sep: b'.', frac_digits: 3 };
    pub const ASS: TsStyle = TsStyle { hour_digits: 1, sep: b'.', frac_digits: 2 };

    /// Smallest representable step in ms.
    pub fn resolution_ms(&self) -> i64 {
        10i64.pow(3 - self.frac_digits.min(3) as u32)
    }
}

/// Parse a timestamp starting at `pos`. Accepts `H+:MM:SS[.,]f+`, `MM:SS[.,]f+`
/// and the fraction-less forms. Returns (milliseconds, end position, style).
pub fn parse(b: &[u8], pos: usize) -> Option<(i64, usize, TsStyle)> {
    let mut i = pos;
    let mut groups: [(i64, usize); 3] = [(0, 0); 3];
    let mut n = 0;
    loop {
        let start = i;
        let mut v: i64 = 0;
        while i < b.len() && b[i].is_ascii_digit() && i - start < 6 {
            v = v * 10 + (b[i] - b'0') as i64;
            i += 1;
        }
        let len = i - start;
        if len == 0 {
            return None;
        }
        groups[n] = (v, len);
        n += 1;
        if n < 3 && i + 1 < b.len() && b[i] == b':' && b[i + 1].is_ascii_digit() {
            i += 1;
            continue;
        }
        break;
    }
    if n < 2 {
        return None;
    }
    let (h, m, s, hour_digits) =
        if n == 3 { (groups[0].0, groups[1].0, groups[2].0, groups[0].1) } else { (0, groups[0].0, groups[1].0, 0) };
    // Minutes/seconds fields are at most two digits.
    let (ml, sl) = if n == 3 { (groups[1].1, groups[2].1) } else { (groups[0].1, groups[1].1) };
    if ml > 2 && n == 3 || sl > 2 {
        return None;
    }
    let mut ms = ((h * 60 + m) * 60 + s) * 1000;
    let mut sep = 0u8;
    let mut frac_digits = 0u8;
    if i + 1 < b.len() && (b[i] == b',' || b[i] == b'.') && b[i + 1].is_ascii_digit() {
        sep = b[i];
        i += 1;
        let start = i;
        let mut frac: i64 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            if i - start < 3 {
                frac = frac * 10 + (b[i] - b'0') as i64;
            }
            i += 1;
        }
        let len = (i - start).min(3);
        frac *= 10i64.pow(3 - len as u32);
        frac_digits = len as u8;
        ms += frac;
    }
    Some((ms, i, TsStyle { hour_digits: hour_digits as u8, sep, frac_digits }))
}

/// Format `ms` in `style`. Negative times are clamped to zero.
pub fn format(ms: i64, style: TsStyle) -> String {
    let ms = ms.max(0);
    let res = style.resolution_ms();
    let units = (ms + res / 2) / res; // rounded to the style's resolution
    let per_sec = 1000 / res;
    let frac = units % per_sec;
    let secs = units / per_sec;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let mut out = String::with_capacity(16);
    if style.hour_digits > 0 || h > 0 {
        let width = (style.hour_digits as usize).max(if style.hour_digits == 0 { 2 } else { 1 });
        out.push_str(&format!("{h:0width$}:"));
    }
    out.push_str(&format!("{m:02}:{s:02}"));
    if style.frac_digits > 0 {
        out.push(if style.sep == 0 { '.' } else { style.sep as char });
        out.push_str(&format!("{frac:0w$}", w = style.frac_digits as usize));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variants() {
        assert_eq!(parse(b"00:01:02,345", 0).unwrap().0, 62_345);
        assert_eq!(parse(b"1:01:02.34", 0).unwrap().0, 3_662_340);
        assert_eq!(
            parse(b"01:02.345 -->", 0).unwrap(),
            (62_345, 9, TsStyle { hour_digits: 0, sep: b'.', frac_digits: 3 })
        );
        assert_eq!(parse(b"00:00:01,5", 0).unwrap().0, 1_500);
        assert_eq!(parse(b"00:00:01", 0).unwrap().0, 1_000);
        assert!(parse(b"12", 0).is_none());
        assert!(parse(b"abc", 0).is_none());
    }

    #[test]
    fn format_roundtrip() {
        for (s, ms) in [("00:01:02,345", 62_345), ("1:01:02.34", 3_662_340), ("01:02.345", 62_345)] {
            let (v, _, st) = parse(s.as_bytes(), 0).unwrap();
            assert_eq!(v, ms);
            assert_eq!(format(v, st), s);
        }
        assert_eq!(format(3_600_000, TsStyle { hour_digits: 0, sep: b'.', frac_digits: 3 }), "01:00:00.000");
        assert_eq!(format(1_235, TsStyle::ASS), "0:00:01.24");
        assert_eq!(format(-50, TsStyle::SRT), "00:00:00,000");
    }
}
