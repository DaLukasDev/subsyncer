//! Text encoding detection and lossless round-tripping.
//!
//! Subtitle timestamps are ASCII, so for every ASCII-compatible encoding the
//! parsers work directly on the original bytes and only the timestamp spans
//! are ever replaced. Everything else (text, styling, line endings) stays
//! byte-for-byte identical. Encodings that are not ASCII-compatible (UTF-16,
//! ISO-2022-JP) are transcoded to UTF-8 for parsing and encoded back on write;
//! that path is only taken after checking the round trip is lossless.

use encoding_rs::{Encoding, ISO_2022_JP, UTF_8, UTF_16BE, UTF_16LE};
use std::borrow::Cow;

use crate::Error;

/// How the original file was encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextEncoding {
    pub encoding: &'static Encoding,
    /// The file started with a byte-order mark.
    pub bom: bool,
}

impl TextEncoding {
    pub fn name(&self) -> &'static str {
        self.encoding.name()
    }

    /// Whether the parser can work on the original bytes directly.
    fn ascii_compatible(&self) -> bool {
        self.encoding.is_ascii_compatible()
    }
}

/// A subtitle file prepared for parsing: the "working" bytes are always
/// ASCII-compatible.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub encoding: TextEncoding,
    /// Bytes the parser sees (BOM stripped; transcoded to UTF-8 if needed).
    pub working: Vec<u8>,
}

impl Prepared {
    /// Decode a span of the working bytes to text (lossy), e.g. for cue text
    /// classification.
    pub fn decode<'a>(&self, bytes: &'a [u8]) -> Cow<'a, str> {
        let enc = if self.encoding.ascii_compatible() { self.encoding.encoding } else { UTF_8 };
        enc.decode_without_bom_handling(bytes).0
    }

    /// Turn (possibly modified) working bytes back into file bytes in the
    /// original encoding, restoring the BOM.
    pub fn finish(&self, working: &[u8]) -> Result<Vec<u8>, Error> {
        let enc = self.encoding.encoding;
        let mut out = Vec::with_capacity(working.len() + 4);
        if enc == UTF_16LE || enc == UTF_16BE {
            let text = std::str::from_utf8(working).map_err(|_| Error::Encoding("working text is not UTF-8".into()))?;
            if self.encoding.bom {
                out.extend_from_slice(if enc == UTF_16LE { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            }
            for unit in text.encode_utf16() {
                let b = if enc == UTF_16LE { unit.to_le_bytes() } else { unit.to_be_bytes() };
                out.extend_from_slice(&b);
            }
            return Ok(out);
        }
        if self.encoding.bom {
            // Only UTF-8 carries a BOM among the remaining encodings.
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        if self.encoding.ascii_compatible() {
            out.extend_from_slice(working);
        } else {
            let text = std::str::from_utf8(working).map_err(|_| Error::Encoding("working text is not UTF-8".into()))?;
            let (bytes, _, had_errors) = enc.encode(text);
            if had_errors {
                return Err(Error::Encoding(format!("text cannot be represented in {}", enc.name())));
            }
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }
}

/// Detect the encoding of `bytes` and prepare working bytes for parsing.
pub fn prepare(bytes: &[u8]) -> Result<Prepared, Error> {
    // 1. Byte-order marks.
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Ok(Prepared { encoding: TextEncoding { encoding: UTF_8, bom: true }, working: rest.to_vec() });
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, UTF_16LE, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, UTF_16BE, true);
    }
    // 2. BOM-less UTF-16: ASCII text has a NUL in every other byte.
    if let Some(enc) = sniff_utf16(bytes) {
        return utf16(bytes, enc, false);
    }
    // 3. Valid UTF-8 (includes pure ASCII).
    if std::str::from_utf8(bytes).is_ok() {
        return Ok(Prepared { encoding: TextEncoding { encoding: UTF_8, bom: false }, working: bytes.to_vec() });
    }
    // 4. Legacy encodings: let chardetng guess.
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    det.feed(bytes, true);
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    let te = TextEncoding { encoding: enc, bom: false };
    if enc.is_ascii_compatible() {
        return Ok(Prepared { encoding: te, working: bytes.to_vec() });
    }
    // ISO-2022-JP and friends: transcode, but only if the round trip is exact.
    debug_assert!(enc == ISO_2022_JP || !enc.is_ascii_compatible());
    let (text, had_errors) = enc.decode_without_bom_handling(bytes);
    if had_errors {
        return Err(Error::Encoding(format!("malformed {} text", enc.name())));
    }
    let prepared = Prepared { encoding: te, working: text.as_bytes().to_vec() };
    if prepared.finish(&prepared.working)? != bytes {
        return Err(Error::Encoding(format!("{} text does not round-trip losslessly", enc.name())));
    }
    Ok(prepared)
}

fn utf16(bytes: &[u8], enc: &'static Encoding, bom: bool) -> Result<Prepared, Error> {
    if !bytes.len().is_multiple_of(2) {
        return Err(Error::Encoding(format!("odd byte length for {}", enc.name())));
    }
    let text = enc
        .decode_without_bom_handling_and_without_replacement(bytes)
        .ok_or_else(|| Error::Encoding(format!("malformed {} text", enc.name())))?;
    Ok(Prepared { encoding: TextEncoding { encoding: enc, bom }, working: text.into_owned().into_bytes() })
}

fn sniff_utf16(bytes: &[u8]) -> Option<&'static Encoding> {
    let n = bytes.len().min(4096) & !1;
    if n < 8 {
        return None;
    }
    let (mut even_nul, mut odd_nul) = (0usize, 0usize);
    for (i, &b) in bytes[..n].iter().enumerate() {
        if b == 0 {
            if i % 2 == 0 { even_nul += 1 } else { odd_nul += 1 }
        }
    }
    let half = n / 2;
    if odd_nul * 10 >= half * 7 && even_nul * 10 < half {
        Some(UTF_16LE)
    } else if even_nul * 10 >= half * 7 && odd_nul * 10 < half {
        Some(UTF_16BE)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8]) -> Prepared {
        let p = prepare(bytes).unwrap();
        assert_eq!(p.finish(&p.working).unwrap(), bytes);
        p
    }

    #[test]
    fn utf8_with_and_without_bom() {
        let p = roundtrip("1\r\n00:00:01,000 --> 00:00:02,000\r\nHéllo\r\n".as_bytes());
        assert_eq!(p.encoding.encoding, UTF_8);
        assert!(!p.encoding.bom);
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(b"abc");
        let p = roundtrip(&b);
        assert!(p.encoding.bom);
        assert_eq!(p.working, b"abc");
    }

    #[test]
    fn utf16_le_be() {
        let text = "1\n00:00:01,000 --> 00:00:02,000\nhallo wereld\n";
        for (enc, bom) in [(UTF_16LE, true), (UTF_16LE, false), (UTF_16BE, true), (UTF_16BE, false)] {
            let mut bytes = Vec::new();
            if bom {
                bytes.extend_from_slice(if enc == UTF_16LE { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            }
            for u in text.encode_utf16() {
                bytes.extend_from_slice(&if enc == UTF_16LE { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            let p = roundtrip(&bytes);
            assert_eq!(p.encoding.encoding, enc);
            assert_eq!(p.working, text.as_bytes());
        }
    }

    #[test]
    fn legacy_cp1252() {
        // "café crème" in windows-1252, repeated so the detector has data.
        let mut bytes = Vec::new();
        for _ in 0..20 {
            bytes.extend_from_slice(
                b"1\r\n00:00:01,000 --> 00:00:02,000\r\nLe caf\xE9 cr\xE8me est tr\xE8s bon, d\xE9j\xE0 vu.\r\n\r\n",
            );
        }
        let p = roundtrip(&bytes);
        assert!(p.encoding.encoding.is_ascii_compatible());
        assert_ne!(p.encoding.encoding, UTF_8);
        assert!(p.decode(&p.working).contains("café"));
    }
}
