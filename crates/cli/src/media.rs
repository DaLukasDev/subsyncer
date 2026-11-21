//! ffprobe / ffmpeg subprocess wrappers. Nothing here links against FFmpeg.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use subsyncer_core::classify;

/// Default for [`Tools::timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// An ffmpeg/ffprobe run that produces no output for this long is
    /// stopped (a stalled network mount would otherwise block forever).
    pub timeout: Duration,
    /// Once set, running ffmpeg/ffprobe processes are stopped and new ones
    /// fail with [`Cancelled`].
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for Tools {
    fn default() -> Self {
        Self { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into(), timeout: DEFAULT_TIMEOUT, cancel: None }
    }
}

impl Tools {
    pub fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

/// The work was cancelled (see [`Tools::cancel`]).
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

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
    /// A commentary track (disposition or title).
    pub commentary: bool,
    /// An audio-description track for the visually impaired (disposition
    /// or title).
    pub audio_description: bool,
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
    let mut cmd = Command::new(&tools.ffprobe);
    cmd.args(["-v", "error", "-of", "json", "-show_streams", "-show_format"]).arg(media_arg(media)?);
    let mut json = Vec::new();
    let out = run(tools, &mut cmd, |b| {
        json.extend_from_slice(b);
        Ok(())
    })?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", out.stderr.trim());
    }
    parse_probe(&json)
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
            let title_words = tag("title").as_deref().map(classify::name_words).unwrap_or_default();
            let titled = |prefixes: &[&str]| title_words.iter().any(|w| prefixes.iter().any(|p| w.starts_with(p)));
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
                commentary: disp("comment") || titled(&["comment"]),
                audio_description: disp("visual_impaired")
                    || disp("descriptions")
                    || titled(&["description", "descriptive", "described"]),
            }
        })
        .collect();
    Ok(Probe { duration_ms, streams })
}

impl Probe {
    pub fn audio_streams(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.codec_type == "audio")
    }

    /// The audio stream to detect speech in: the default one, else the
    /// first. Commentary and audio-description tracks are passed over while
    /// there is another: their speech doesn't follow the dialogue.
    pub fn default_audio(&self) -> Option<&Stream> {
        let main = |s: &&Stream| !s.commentary && !s.audio_description;
        let pick = |only_main: bool| {
            let mut it = self.audio_streams().filter(move |s| !only_main || main(s));
            let first = it.next()?;
            Some(std::iter::once(first).chain(it).find(|s| s.default).unwrap_or(first))
        };
        pick(true).or_else(|| pick(false))
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
/// `range` is `(start_ms, len_ms)` to decode only part of it; sample 0 is
/// then at `start_ms`. Returns the number of samples decoded.
pub fn decode_audio(
    tools: &Tools,
    media: &Path,
    stream_index: usize,
    filter: Option<&str>,
    range: Option<(i64, i64)>,
    mut sink: impl FnMut(&[i16]) -> Result<()>,
) -> Result<u64> {
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-v", "error"]);
    if let Some((start, len)) = range {
        // Input seeking: fast, and exact when decoding (ffmpeg decodes from
        // the preceding keyframe and drops what comes before `start`).
        cmd.args(["-ss", &secs(start.max(0)), "-t", &secs(len.max(0))]);
    }
    cmd.arg("-i").arg(media_arg(media)?).args(["-map", &format!("0:{stream_index}"), "-vn", "-sn", "-dn"]);
    if let Some(f) = filter {
        cmd.args(["-af", f]);
    }
    cmd.args(["-ac", "1", "-ar", "16000", "-f", "s16le", "-acodec", "pcm_s16le", "pipe:1"]);
    let mut carry: Option<u8> = None;
    let mut samples = Vec::with_capacity(READ_BUF / 2 + 1);
    let mut total = 0u64;
    let out = run(tools, &mut cmd, |mut bytes| {
        samples.clear();
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
        sink(&samples)
    })?;
    if !out.status.success() {
        bail!("ffmpeg audio decode failed: {}", out.stderr.trim());
    }
    Ok(total)
}

/// `media` as an argument ffmpeg and ffprobe can't mistake for an option
/// (`-name.mkv`) or a protocol (`Show: Pilot.mkv`): an absolute path.
fn media_arg(media: &Path) -> Result<PathBuf> {
    std::path::absolute(media).with_context(|| format!("resolving {}", media.display()))
}

fn secs(ms: i64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// Extract text subtitle streams, all in one pass over the file. ASS/SSA are
/// copied as-is (keeps style info for sign detection); everything else is
/// converted to SRT. If that fails, each stream is tried on its own, so one
/// bad stream doesn't cost the others. Results are in `streams` order.
pub fn extract_subtitles(tools: &Tools, media: &Path, streams: &[&Stream]) -> Vec<Result<Vec<u8>>> {
    if streams.is_empty() {
        return Vec::new();
    }
    match extract_run(tools, media, streams) {
        Ok(all) => all.into_iter().map(Ok).collect(),
        Err(e) if e.is::<Cancelled>() => streams.iter().map(|_| Err(Cancelled.into())).collect(),
        Err(e) if streams.len() == 1 => vec![Err(e)],
        Err(_) => {
            streams.iter().map(|s| extract_run(tools, media, &[s]).map(|mut v| v.pop().unwrap_or_default())).collect()
        }
    }
}

fn extract_run(tools: &Tools, media: &Path, streams: &[&Stream]) -> Result<Vec<Vec<u8>>> {
    let dir = tempfile::Builder::new().prefix("subsyncer-").tempdir().context("creating a temp dir")?;
    let mut cmd = Command::new(&tools.ffmpeg);
    // The subtitles go to files; progress on stdout shows ffmpeg is alive
    // while it reads through a large file.
    cmd.args(["-nostdin", "-hide_banner", "-v", "error", "-nostats", "-progress", "pipe:1", "-y", "-i"])
        .arg(media_arg(media)?);
    let mut files = Vec::with_capacity(streams.len());
    for s in streams {
        let (codec, fmt) = match s.codec_name.as_str() {
            "ass" | "ssa" => ("copy", "ass"),
            _ => ("srt", "srt"),
        };
        let file = dir.path().join(format!("{}.{fmt}", s.index));
        cmd.args(["-map", &format!("0:{}", s.index), "-c:s", codec, "-f", fmt]).arg(&file);
        files.push(file);
    }
    let out = run(tools, &mut cmd, |_| Ok(()))?;
    if !out.status.success() {
        bail!("ffmpeg subtitle extraction failed: {}", out.stderr.trim());
    }
    files.iter().map(|f| std::fs::read(f).with_context(|| format!("reading {}", f.display()))).collect()
}

const READ_BUF: usize = 64 * 1024;
/// Keep at most this much of a process's stderr (for error messages).
const STDERR_LIMIT: usize = 16 * 1024;

struct Finished {
    status: ExitStatus,
    stderr: String,
}

enum Stop {
    Cancelled,
    Timeout,
}

/// Run `cmd`, passing its stdout to `on_stdout` as it arrives. A watchdog
/// kills the process when it produces no output on stdout or stderr for
/// `tools.timeout`, or when `tools.cancel` is set; both are errors. A
/// non-zero exit is not: the caller words that error from `stderr`.
fn run(tools: &Tools, cmd: &mut Command, mut on_stdout: impl FnMut(&[u8]) -> Result<()>) -> Result<Finished> {
    if tools.cancelled() {
        return Err(Cancelled.into());
    }
    let program = PathBuf::from(cmd.get_program());
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("running {}", program.display()))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let child = Mutex::new(child);
    let kill = || {
        let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    };
    let start = Instant::now();
    // When output was last seen, in ms after `start`.
    let last_output = AtomicU64::new(0);
    let touch = || last_output.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let err_reader = s.spawn(move || {
            let mut text = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buf) {
                touch();
                let keep = n.min(STDERR_LIMIT.saturating_sub(text.len()));
                text.extend_from_slice(&buf[..keep]);
            }
            String::from_utf8_lossy(&text).into_owned()
        });
        let watchdog = s.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let idle = start.elapsed().saturating_sub(Duration::from_millis(last_output.load(Ordering::Relaxed)));
                let stop = if tools.cancelled() {
                    Some(Stop::Cancelled)
                } else if idle >= tools.timeout {
                    Some(Stop::Timeout)
                } else {
                    None
                };
                if stop.is_some() {
                    kill();
                    return stop;
                }
                std::thread::park_timeout(Duration::from_millis(100));
            }
            None
        });
        let mut buf = vec![0u8; READ_BUF];
        let read = (|| -> Result<()> {
            loop {
                let n = stdout.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                touch();
                on_stdout(&buf[..n])?;
            }
        })();
        if read.is_err() {
            kill();
        }
        done.store(true, Ordering::Release);
        watchdog.thread().unpark();
        let stopped = watchdog.join().expect("watchdog thread");
        let status = child.lock().unwrap_or_else(|e| e.into_inner()).wait();
        let stderr = err_reader.join().unwrap_or_default();
        match stopped {
            Some(Stop::Cancelled) => return Err(Cancelled.into()),
            Some(Stop::Timeout) => bail!(
                "{} made no progress for {} s and was stopped (is the file on a slow or unresponsive mount?)",
                program.display(),
                tools.timeout.as_secs()
            ),
            None => {}
        }
        read?;
        Ok(Finished { status: status?, stderr })
    })
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
    fn commentary_and_description_tracks_are_passed_over() {
        let json = br#"{"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264"},
            {"index":1,"codec_type":"audio","codec_name":"aac","disposition":{"default":1,"comment":1}},
            {"index":2,"codec_type":"audio","codec_name":"aac","tags":{"title":"Audio Description"}},
            {"index":3,"codec_type":"audio","codec_name":"ac3","tags":{"title":"English 5.1"}},
            {"index":4,"codec_type":"audio","codec_name":"aac","tags":{"title":"Director's Commentary"}}
        ]}"#;
        let p = parse_probe(json).unwrap();
        assert_eq!(p.default_audio().unwrap().index, 3);
        assert!(p.streams[1].commentary && p.streams[4].commentary && p.streams[2].audio_description);
        // Only commentary: still better than nothing.
        let only = br#"{"streams":[{"index":0,"codec_type":"audio","tags":{"title":"Commentary"}}]}"#;
        assert_eq!(parse_probe(only).unwrap().default_audio().unwrap().index, 0);
        // A default main track wins over an earlier one.
        let two = br#"{"streams":[{"index":0,"codec_type":"audio"},{"index":1,"codec_type":"audio","disposition":{"default":1}}]}"#;
        assert_eq!(parse_probe(two).unwrap().default_audio().unwrap().index, 1);
    }

    #[test]
    fn media_args_are_absolute() {
        for name in ["-dash.mkv", "Show: Pilot.mkv", "dir/a.mkv", "/abs/a.mkv"] {
            let a = media_arg(Path::new(name)).unwrap();
            assert!(a.is_absolute() && a.ends_with(name.trim_start_matches('/')), "{name}: {}", a.display());
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

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    fn quick() -> Tools {
        Tools { timeout: Duration::from_millis(400), ..Tools::default() }
    }

    #[test]
    fn silent_process_is_stopped() {
        let t = Instant::now();
        let e = run(&quick(), &mut sh("exec sleep 10"), |_| Ok(())).err().expect("timed out");
        assert!(e.to_string().contains("made no progress"), "{e:#}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn steady_output_keeps_a_process_alive() {
        let mut lines = 0;
        let out = run(&quick(), &mut sh("for i in 1 2 3 4 5 6 7 8; do echo x; sleep 0.1; done"), |b| {
            lines += b.iter().filter(|&&c| c == b'\n').count();
            Ok(())
        })
        .unwrap();
        assert!(out.status.success());
        assert_eq!(lines, 8);
    }

    #[test]
    fn cancelling_stops_a_process() {
        let cancel = Arc::new(AtomicBool::new(false));
        let tools = Tools { cancel: Some(cancel.clone()), timeout: Duration::from_secs(60), ..Tools::default() };
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let t = Instant::now();
        let e = run(&tools, &mut sh("exec sleep 10"), |_| Ok(())).err().expect("cancelled");
        setter.join().unwrap();
        assert!(e.is::<Cancelled>(), "{e:#}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        // Already cancelled: nothing is started.
        assert!(run(&tools, &mut sh("exit 0"), |_| Ok(())).err().unwrap().is::<Cancelled>());
    }

    #[test]
    fn failure_keeps_stderr() {
        let out = run(&quick(), &mut sh("echo oops >&2; exit 3"), |_| Ok(())).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stderr.trim(), "oops");
    }

    /// Whether ffmpeg is installed (media tests are skipped without it).
    pub(crate) fn have_ffmpeg() -> bool {
        let ok = check_tools(&Tools::default()).is_ok();
        if !ok {
            eprintln!("ffmpeg/ffprobe not found; skipping");
        }
        ok
    }

    #[test]
    fn extracts_several_subtitle_streams_in_one_pass() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(
            p.join("a.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n",
        )
        .unwrap();
        std::fs::write(
            p.join("b.ass"),
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,2,2,10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hallo\n",
        )
        .unwrap();
        let mkv = p.join("-odd: name.mkv");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-t", "5", "-i", "anullsrc=r=16000:cl=mono", "-i"])
            .arg(p.join("a.srt"))
            .arg("-i")
            .arg(p.join("b.ass"))
            .args(["-map", "0", "-map", "1", "-map", "2", "-c:a", "pcm_s16le", "-c:s", "copy"])
            .arg(&mkv)
            .status()
            .unwrap();
        assert!(status.success());
        let tools = Tools::default();
        let probe = probe(&tools, &mkv).unwrap();
        let subs: Vec<&Stream> = probe.text_subtitles().collect();
        assert_eq!(subs.len(), 2);
        let got = extract_subtitles(&tools, &mkv, &subs);
        let srt = String::from_utf8(got[0].as_ref().unwrap().clone()).unwrap();
        let ass = String::from_utf8(got[1].as_ref().unwrap().clone()).unwrap();
        assert!(srt.contains("00:00:03,000 --> 00:00:04,000"), "{srt}");
        assert!(ass.contains("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hallo"), "{ass}");
    }
}
