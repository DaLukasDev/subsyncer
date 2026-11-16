//! ffprobe / ffmpeg subprocess wrappers. Nothing here links against FFmpeg.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Default for Tools {
    fn default() -> Self {
        Self { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub duration_ms: Option<i64>,
    pub streams: Vec<Stream>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stream {
    /// Absolute stream index in the container (`-map 0:<index>`).
    pub index: usize,
    pub codec_type: String,
    pub codec_name: String,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
}

#[derive(Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
}

#[derive(Deserialize)]
struct RawStream {
    index: usize,
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    #[serde(default)]
    disposition: std::collections::HashMap<String, i64>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

pub fn probe(tools: &Tools, media: &Path) -> Result<Probe> {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-of", "json", "-show_streams", "-show_format"])
        .arg(media)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", tools.ffprobe.display()))?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_probe(&out.stdout)
}

pub fn parse_probe(json: &[u8]) -> Result<Probe> {
    let raw: RawProbe = serde_json::from_slice(json).context("parsing ffprobe output")?;
    let duration_ms =
        raw.format.and_then(|f| f.duration).and_then(|d| d.parse::<f64>().ok()).map(|d| (d * 1000.0).round() as i64);
    let streams = raw
        .streams
        .into_iter()
        .map(|s| {
            let tag = |k: &str| s.tags.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
            let disp = |k: &str| s.disposition.get(k).copied().unwrap_or(0) != 0;
            Stream {
                index: s.index,
                codec_type: s.codec_type.clone().unwrap_or_default(),
                codec_name: s.codec_name.clone().unwrap_or_default(),
                channels: s.channels,
                channel_layout: s.channel_layout.clone(),
                language: tag("language"),
                title: tag("title"),
                default: disp("default"),
                forced: disp("forced"),
                hearing_impaired: disp("hearing_impaired"),
            }
        })
        .collect();
    Ok(Probe { duration_ms, streams })
}

impl Probe {
    pub fn audio_streams(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.codec_type == "audio")
    }

    /// The default audio stream, else the first one.
    pub fn default_audio(&self) -> Option<&Stream> {
        self.audio_streams().find(|s| s.default).or_else(|| self.audio_streams().next())
    }

    /// Text-based subtitle streams usable as a timing reference.
    pub fn text_subtitles(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| {
            s.codec_type == "subtitle"
                && matches!(s.codec_name.as_str(), "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt" | "text")
        })
    }
}

/// Whether a channel layout includes a front-centre channel.
pub fn has_front_center(layout: &str) -> bool {
    let l = layout.to_ascii_lowercase();
    if l.contains("(front)") && (l.starts_with("6.0") || l.starts_with("6.1")) {
        return false; // FL FR FLC FRC SL SR (+LFE): no FC
    }
    if l.starts_with("3.0(back)") || l.starts_with("2.1") || l.starts_with("quad") || l.starts_with("2.2") {
        return false;
    }
    let b = l.as_bytes();
    if b.len() >= 3 && (b'3'..=b'9').contains(&b[0]) && b[1] == b'.' && b[2].is_ascii_digit() {
        return true;
    }
    l.starts_with("hexagonal") || l.starts_with("octagonal") || l.starts_with("hexadecagonal") || l.starts_with("22.2")
}

/// The `-af` filter used to get dialogue: the centre channel alone when
/// the layout has one.
pub fn dialogue_filter(stream: &Stream) -> Option<String> {
    match (&stream.channel_layout, stream.channels) {
        (Some(layout), _) if has_front_center(layout) => Some("pan=mono|c0=FC".into()),
        // Unknown layout with 6 channels: almost certainly 5.1 (FC = c2).
        (None, Some(6)) => Some("pan=mono|c0=c2".into()),
        _ => None,
    }
}

/// Decode a stream to 16 kHz mono s16le and feed samples to `sink`.
/// Returns the number of samples decoded.
pub fn decode_audio(
    tools: &Tools,
    media: &Path,
    stream_index: usize,
    filter: Option<&str>,
    mut sink: impl FnMut(&[i16]) -> Result<()>,
) -> Result<u64> {
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-v", "error", "-i"]).arg(media).args([
        "-map",
        &format!("0:{stream_index}"),
        "-vn",
        "-sn",
        "-dn",
    ]);
    if let Some(f) = filter {
        cmd.args(["-af", f]);
    }
    cmd.args(["-ac", "1", "-ar", "16000", "-f", "s16le", "-acodec", "pcm_s16le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("running {}", tools.ffmpeg.display()))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Option<u8> = None;
    let mut samples = Vec::with_capacity(buf.len() / 2 + 1);
    let mut total = 0u64;
    let result = (|| -> Result<()> {
        loop {
            let n = stdout.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            samples.clear();
            let mut bytes = &buf[..n];
            if let Some(lo) = carry.take() {
                samples.push(i16::from_le_bytes([lo, bytes[0]]));
                bytes = &bytes[1..];
            }
            let (pairs, rest) = bytes.as_chunks::<2>();
            samples.extend(pairs.iter().map(|&p| i16::from_le_bytes(p)));
            if let [b] = rest {
                carry = Some(*b);
            }
            total += samples.len() as u64;
            sink(&samples)?;
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stderr = err_thread.join().unwrap_or_default();
    result?;
    if !status.success() {
        bail!("ffmpeg audio decode failed: {}", stderr.trim());
    }
    Ok(total)
}

/// Extract a text subtitle stream. ASS/SSA are copied as-is (keeps style
/// info for sign detection); everything else is converted to SRT.
pub fn extract_subtitle(tools: &Tools, media: &Path, stream: &Stream) -> Result<Vec<u8>> {
    let (codec, fmt) = match stream.codec_name.as_str() {
        "ass" | "ssa" => ("copy", "ass"),
        _ => ("srt", "srt"),
    };
    let out = Command::new(&tools.ffmpeg)
        .args(["-nostdin", "-hide_banner", "-v", "error", "-i"])
        .arg(media)
        .args(["-map", &format!("0:{}", stream.index), "-c:s", codec, "-f", fmt, "pipe:1"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", tools.ffmpeg.display()))?;
    if !out.status.success() {
        bail!("ffmpeg subtitle extraction failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Check that ffmpeg and ffprobe can be executed.
pub fn check_tools(tools: &Tools) -> Result<()> {
    for t in [&tools.ffmpeg, &tools.ffprobe] {
        let ok = Command::new(t)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("cannot run {}; is FFmpeg installed and on PATH?", t.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centre_channel_layouts() {
        for l in ["5.1", "5.1(side)", "7.1", "7.1(wide)", "3.0", "4.0", "6.1", "hexagonal"] {
            assert!(has_front_center(l), "{l}");
        }
        for l in ["stereo", "mono", "2.1", "quad", "quad(side)", "3.0(back)", "6.0(front)", "6.1(front)", "downmix"] {
            assert!(!has_front_center(l), "{l}");
        }
    }

    #[test]
    fn parse_probe_json() {
        let json = br#"{"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264"},
            {"index":1,"codec_type":"audio","codec_name":"ac3","channels":6,"channel_layout":"5.1(side)","disposition":{"default":0},"tags":{"language":"eng"}},
            {"index":2,"codec_type":"audio","codec_name":"aac","channels":2,"channel_layout":"stereo","disposition":{"default":1}},
            {"index":3,"codec_type":"subtitle","codec_name":"subrip","disposition":{"forced":1},"tags":{"LANGUAGE":"dut","title":"Forced"}},
            {"index":4,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"}
        ],"format":{"duration":"734.123000"}}"#;
        let p = parse_probe(json).unwrap();
        assert_eq!(p.duration_ms, Some(734_123));
        assert_eq!(p.default_audio().unwrap().index, 2);
        let subs: Vec<_> = p.text_subtitles().collect();
        assert_eq!(subs.len(), 1);
        assert!(subs[0].forced);
        assert_eq!(subs[0].language.as_deref(), Some("dut"));
        assert_eq!(dialogue_filter(&p.streams[1]).as_deref(), Some("pan=mono|c0=FC"));
        assert_eq!(dialogue_filter(&p.streams[2]), None);
    }
}
