//! Boolean activity timelines on a fixed 10 ms grid.

use crate::Error;

/// Grid resolution of every timeline, in milliseconds.
pub const FRAME_MS: i64 = 10;

/// Convert milliseconds to the nearest frame index.
#[inline]
pub fn ms_to_frame(ms: i64) -> i64 {
    // Round half away from zero isn't needed; round half up is fine.
    (ms + FRAME_MS / 2).div_euclid(FRAME_MS)
}

#[inline]
pub fn frame_to_ms(frame: i64) -> i64 {
    frame * FRAME_MS
}

/// A half-open time interval in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct Interval {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Interval {
    pub fn new(start_ms: i64, end_ms: i64) -> Self {
        Self { start_ms, end_ms }
    }
    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }
    /// Interval in frames `[start, end)`, guaranteed at least one frame long
    /// for non-empty intervals.
    pub fn frames(&self) -> (i64, i64) {
        let s = ms_to_frame(self.start_ms);
        let mut e = ms_to_frame(self.end_ms);
        if self.end_ms > self.start_ms && e <= s {
            e = s + 1;
        }
        (s, e)
    }
}

/// Speech (or cue) activity per 10 ms frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    frames: Vec<bool>,
}

impl Timeline {
    pub fn from_frames(frames: Vec<bool>) -> Self {
        Self { frames }
    }

    /// Build a timeline from intervals. `len` defaults to the end of the last
    /// interval. Negative parts are clipped.
    pub fn from_intervals(intervals: impl IntoIterator<Item = Interval>, len: Option<usize>) -> Self {
        let ivs: Vec<(i64, i64)> = intervals.into_iter().map(|iv| iv.frames()).collect();
        let len = len.unwrap_or_else(|| ivs.iter().map(|&(_, e)| e.max(0) as usize).max().unwrap_or(0));
        let mut frames = vec![false; len];
        for (s, e) in ivs {
            let s = s.clamp(0, len as i64) as usize;
            let e = e.clamp(0, len as i64) as usize;
            frames[s..e.max(s)].fill(true);
        }
        Self { frames }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub fn frames(&self) -> &[bool] {
        &self.frames
    }
    pub fn duration_ms(&self) -> i64 {
        frame_to_ms(self.frames.len() as i64)
    }

    /// Fraction of frames that are active.
    pub fn active_fraction(&self) -> f64 {
        if self.frames.is_empty() {
            return 0.0;
        }
        self.frames.iter().filter(|&&b| b).count() as f64 / self.frames.len() as f64
    }

    /// Maximal runs of active frames as intervals (ms).
    pub fn intervals(&self) -> Vec<Interval> {
        let mut out = Vec::new();
        let mut start: Option<usize> = None;
        for (i, &b) in self.frames.iter().enumerate() {
            match (b, start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    out.push(Interval::new(frame_to_ms(s as i64), frame_to_ms(i as i64)));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            out.push(Interval::new(frame_to_ms(s as i64), frame_to_ms(self.frames.len() as i64)));
        }
        out
    }

    /// Prefix sums of per-frame weights: speech = +1, non-speech = -lambda.
    /// `p[i]` is the sum over frames `0..i`.
    pub fn weighted_prefix(&self, lambda: f64) -> Vec<f64> {
        let mut p = Vec::with_capacity(self.frames.len() + 1);
        let mut acc = 0.0;
        p.push(0.0);
        for &b in &self.frames {
            acc += if b { 1.0 } else { -lambda };
            p.push(acc);
        }
        p
    }

    /// Prefix counts of active frames.
    pub fn count_prefix(&self) -> Vec<u32> {
        let mut p = Vec::with_capacity(self.frames.len() + 1);
        let mut acc = 0u32;
        p.push(0);
        for &b in &self.frames {
            acc += b as u32;
            p.push(acc);
        }
        p
    }

    /// Compact run-length encoding (LEB128 varints of alternating run
    /// lengths, starting with an inactive run). Speech timelines compress to
    /// a few KB per hour.
    pub fn encode_rle(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(1u8); // format version
        write_varint(&mut out, self.frames.len() as u64);
        let mut cur = false;
        let mut run = 0u64;
        for &b in &self.frames {
            if b == cur {
                run += 1;
            } else {
                write_varint(&mut out, run);
                cur = b;
                run = 1;
            }
        }
        write_varint(&mut out, run);
        out
    }

    pub fn decode_rle(bytes: &[u8]) -> Result<Self, Error> {
        let bad = || Error::Parse("invalid timeline blob".into());
        let (&ver, mut rest) = bytes.split_first().ok_or_else(bad)?;
        if ver != 1 {
            return Err(bad());
        }
        let len = read_varint(&mut rest).ok_or_else(bad)? as usize;
        let mut frames = Vec::with_capacity(len);
        let mut cur = false;
        while !rest.is_empty() {
            let run = read_varint(&mut rest).ok_or_else(bad)? as usize;
            if frames.len() + run > len {
                return Err(bad());
            }
            frames.resize(frames.len() + run, cur);
            cur = !cur;
        }
        if frames.len() != len {
            return Err(bad());
        }
        Ok(Self { frames })
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(buf: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&b, rest) = buf.split_first()?;
        *buf = rest;
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_roundtrip() {
        let ivs = vec![Interval::new(100, 250), Interval::new(1000, 1500)];
        let t = Timeline::from_intervals(ivs.clone(), Some(300));
        assert_eq!(t.len(), 300);
        assert_eq!(t.intervals(), ivs);
    }

    #[test]
    fn rle_roundtrip() {
        let t = Timeline::from_intervals(vec![Interval::new(0, 50), Interval::new(3000, 90000)], Some(20000));
        let blob = t.encode_rle();
        assert!(blob.len() < 20);
        assert_eq!(Timeline::decode_rle(&blob).unwrap(), t);
        let empty = Timeline::default();
        assert_eq!(Timeline::decode_rle(&empty.encode_rle()).unwrap(), empty);
        assert!(Timeline::decode_rle(&[1, 5, 9]).is_err());
    }

    #[test]
    fn short_interval_gets_a_frame() {
        assert_eq!(Interval::new(1001, 1003).frames(), (100, 101));
    }
}
