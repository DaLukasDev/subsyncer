# How subsyncer works

All timelines are boolean arrays on a 10 ms grid: "is someone speaking here?"
for the video, "is a subtitle shown here?" for the subtitle. Syncing means
finding the shift (or set of shifts) that makes them line up.

## Pipeline

1. **Probe** the video's streams with `ffprobe`.
2. **Build a reference timeline** for the video.
   - *Embedded subtitles*: if the video contains a text subtitle track (subrip,
     ass, mov_text, webvtt), its timing can be the reference. The track with
     the most dialogue cues wins, but a regular track is preferred over an
     SDH one with nearly as many (90%). Forced/signs tracks are skipped: those
     with the forced flag, "forced"/"signs" in the title, or fewer than 50% of
     the target subtitle's cues. The track is used only once it has passed a
     check against the audio ([below](#checking-embedded-tracks)). All
     candidate tracks are extracted in one pass over the file, and their cues
     and the check's verdict are cached per video file.
   - *Audio*: otherwise ffmpeg decodes the default audio stream to 16 kHz mono.
     Commentary and audio-description tracks (by disposition or title) are
     passed over while the video has another audio track.
     If the layout has a centre channel, that channel is used alone
     (`pan=mono|c0=FC`), because that is where dialogue lives in a surround mix.
     Silero VAD v6 runs on 512-sample windows with its state carried between
     windows, and the result goes through hysteresis thresholding (0.5/0.35),
     gap filling, and resampling to 10 ms. The model is embedded in the binary.
     The result is cached per video file, so later subtitles for the same video
     skip this step.
3. **Pick the cues to align.** Sound descriptions (`[door slams]`), music
   (`♪ ... ♪`), ASS comments, drawings and sign lines have no matching speech,
   so they are left out of alignment. They still move with their neighbours.
4. **Global alignment.** FFT cross-correlation of the two timelines as ±1
   signals, for framerate ratios 1, 25/23.976, 24/25, 24/23.976 and their
   inverses. Confidence is the z-score of the correlation peak on a detrended
   curve, so slow changes in dialogue density don't hide a sharp true peak.
5. **Piecewise alignment.** A dynamic program over (cue, offset): each cue scores
   +1 per speech frame and −λ per non-speech frame (O(1) via prefix sums), and
   starting a new segment costs `SPLIT_PENALTY`.
   - A backward jump larger than the dialogue gap at that point also pays for
     the overlap it creates, because real cuts sit in gaps.
   - The offset range covers every strong correlation peak and widens if a
     segment lands on its edge.
   - Ratio 1 and the best other ratio both go through the DP, and its objective
     decides between them. Inserted chunks build up offset like a framerate
     drift does, so the global correlation alone can't tell them apart.
   - Short or near-identical segments are merged.
   - **Each segment must have its own sharp correlation peak**, otherwise it is
     merged into a neighbour. Without this, a subtitle for a different video
     gets "fitted" with several spurious segments.
   - Overlaps from removed content are collapsed so cues stay in order.
6. **Quality gate.**
   - `already_synced`: nothing moves by 100 ms or more, or a correction of at
     most 300 ms gives no measurable gain.
   - `low_confidence`: the peak isn't prominent enough, or the speech overlap
     doesn't improve enough. Nothing is written.
   - `synced`: written.
   - `failed`: an error, e.g. no audio stream or an unparsable file.
7. **Write.** Only timestamp bytes change. Encoding (UTF-8, UTF-16 and legacy
   code pages via chardetng), BOM, line endings and all styling are preserved,
   and an unshifted file renders byte-identical. The new file is written to a
   temp file in the same directory, then renamed over the original. The previous
   version is kept as `<subtitle>.bak`.

Formats: SRT, WebVTT (including inline karaoke timestamps) and ASS/SSA.

## Checking embedded tracks

An embedded track is a better reference than detected speech when it is
right: its timing is exact. But a track muxed in from another release or cut
can be off, and trusting it would move a correct subtitle to match it, with
high confidence, because two subtitle tracks line up almost perfectly. So with
`--reference auto` (what Bazarr uses) the track is checked first. The code is
in `crates/core/src/verify.rs`.

1. **Samples.** Speech is detected in three 2.5-minute stretches of the audio.
   ffmpeg seeks to them, so only those are decoded. Two go where the track has
   the most dialogue, one at the end of its dialogue. The track's cues in each
   2-minute window are correlated with the speech over ±15 s.
   - If every window lines up clearly (detrended z-score ≥ 4) within ±400 ms
     of the expected position, and their average within ±250 ms, the track is
     used.
   - If one lines up clearly somewhere else, it isn't.
   - The expected position is +100 ms: detected speech starts about that long
     after a cue.
2. **Full audio**, used when the samples don't settle it. It is also used when
   the video is shorter than 20 minutes (decoding it all is cheap then) and
   when its speech timeline is already cached. The track is aligned against
   the full audio like any subtitle. It must need no frame-rate change and no
   shift beyond 250 ms, and must line up confidently. Each 2-minute stretch is
   also checked on its own over ±60 s:
   - none may line up clearly elsewhere;
   - no two stretches in a row may fail to support it;
   - the stretches that don't support it may hold at most 20% of its cues;
   - the last stretch must support it.
3. **Regions of a split result.** Samples can miss a stretch that is off,
   typically near the end. That matters only when it changes the result: the
   subtitle then gets split to follow the track there. So when the result
   against an accepted track has several segments, the track is checked
   against the audio in each segment's region. If the track (rather than the
   subtitle) has the cut, its cues there line up at a known other offset: the
   difference between the segments' shifts. The in-sync offset must beat that
   offset on both the plain and the detrended correlation.

A rejected track means the sync uses the audio instead. If the audio can't be
analysed at all, the track is used unchecked, as before 0.1.1. Other modes:
- `--reference embedded` always trusts the track;
- `SUBSYNCER_EMBEDDED_CHECK=full` skips the samples;
- `SUBSYNCER_EMBEDDED_CHECK=off` turns the check off.

Three samples take about 1–1.5 s. The full audio of a 45-minute episode takes
5–10 s. The verdict is cached, so later subtitles for the same video skip the
check (`"cached": true`). The job report records each check under
`embedded_check`.
[evaluation.md](evaluation.md#embedded-track-check) has the measurements.

## The Bazarr command

Since v1.6, Bazarr shell-splits the post-processing command, substitutes a
`{{variable}}` only when it is a whole argument, and runs it without a shell. A
plain `curl ... --data-urlencode "video={{episode}}"` would therefore send the
literal text `{{episode}}`. The recommended command passes the paths to `sh` as
`$1`/`$2` instead:

```sh
sh -c 'curl -fsS -X POST --data-urlencode "video=$1" --data-urlencode "subtitle=$2" http://subsyncer:8080/sync' _ "{{episode}}" "{{subtitles}}"
```

The paths are never parsed as shell code, so spaces, quotes, `$` and
apostrophes are safe. The script must not start or end with a `"`, because
Bazarr strips those from each argument. The same command also works on older
Bazarr versions that substituted into a shell string.

This was verified against `lscr.io/linuxserver/bazarr` v1.6.2, which includes
`curl` and `sh`. [`scripts/e2e-bazarr.sh`](../scripts/e2e-bazarr.sh) reproduces
the hook end to end: it runs Bazarr's own `pp_replace()` and `postprocessing()`
inside the Bazarr image against a subsyncer container.

## Docker image

The image is Debian 13 slim (~136 MB):

- **ONNX Runtime 1.31** is prebuilt by the `ort` crate and linked into the
  binary.
- **FFmpeg** is a minimal static 8.1 build made from source by
  [`docker/build-ffmpeg.sh`](../docker/build-ffmpeg.sh): every demuxer, native
  audio decoders and text subtitle codecs, and no video, GPU or X11 libraries.
  That is 19 MB instead of the ~450 MB Debian's `ffmpeg` package pulls in. An
  audio codec outside that set fails with a clear ffmpeg error.
- It runs subsyncer as `PUID:PGID` using `tini` and `gosu` (or directly under
  `docker --user`), with a `HEALTHCHECK` on `/healthz` and a volume at `/data`.

Images that were measured and rejected:

| image | size | VAD on 12 min of audio |
|---|---|---|
| **Debian slim + minimal FFmpeg (used)** | **136 MB** | **~1.8 s** |
| Alpine (musl, Alpine's ONNX Runtime 1.23 and ffmpeg) | 209 MB | ~2.85 s |
| Debian slim + Debian's `ffmpeg` package | 566 MB | ~1.95 s |

Bookworm doesn't work, because the prebuilt ONNX Runtime needs glibc ≥ 2.38.

## Platform requirements

ONNX Runtime is linked statically, so the `subsyncer` binary depends only on
glibc and libstdc++. It needs **glibc ≥ 2.38 and the libstdc++ from GCC 13+**
(Ubuntu 24.04+, Debian 13+, Fedora 39+), the floor set by the prebuilt ONNX
Runtime. `ffmpeg` and `ffprobe` must be on `PATH` (or set `SUBSYNCER_FFMPEG` /
`SUBSYNCER_FFPROBE`).

## Code layout

- `crates/core`: parsers, alignment, scoring, the embedded-track check. Pure:
  no IO, no ffmpeg.
- `crates/cli`: the `subsyncer` binary, covering ffmpeg/ffprobe, VAD, HTTP,
  cache and config.
- `crates/eval`: the evaluation harness (see [evaluation.md](evaluation.md)).
