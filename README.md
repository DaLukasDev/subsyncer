# subsyncer

Syncs subtitle files to video. A drop-in replacement for Bazarr's built-in
sync (ffsubsync), shipped as a single binary and a Docker image. Bazarr calls
it through its custom post-processing command.

| ffsubsync problem | subsyncer |
|---|---|
| Weak voice detection, confused by music and effects | Silero VAD on the **centre channel** when the audio has one (dialogue lives there) |
| One global offset + framerate fix | Global offset/framerate, then **piecewise alignment** with splits (ad breaks, recaps, different cuts) |
| Ignores embedded subtitle tracks | Uses an embedded text track as the reference when it's usable |
| Can make good subtitles worse | **Quality gate**: writes only when confident *and* measurably better |
| Re-decodes audio for every subtitle | Speech timeline **cached** per media file (SQLite) |

## Bazarr setup

1. Run subsyncer next to Bazarr (see [`docker-compose.example.yml`](docker-compose.example.yml)).
   Mount your media at the **same paths** as in Bazarr, or translate them with
   `SUBSYNCER_PATH_MAP`.
2. In Bazarr, **Settings → Subtitles**:
   - turn **off** *Automatic Subtitles Synchronization*;
   - turn **on** *Custom Post-Processing* and set the command to:

   ```sh
   sh -c 'curl -fsS -X POST --data-urlencode "video=$1" --data-urlencode "subtitle=$2" http://subsyncer:8080/sync' _ "{{episode}}" "{{subtitles}}"
   ```

   - leave the post-processing score threshold **off**, so every download is checked.

   With `SUBSYNCER_API_KEY` set, add `-H "X-Api-Key: <key>"` after `-fsS`.

Why it has this shape: since v1.6 Bazarr shell-splits the command, substitutes a
`{{variable}}` only when it is a whole argument, and runs it without a shell. A
plain `curl ... --data-urlencode "video={{episode}}"` would therefore send the
literal text `{{episode}}`. Here Bazarr passes the paths as `$1`/`$2` to `sh`,
so they are never parsed as shell code. Paths with spaces, quotes, `$` or
apostrophes are safe. The script must not start or end with a `"`, because
Bazarr strips those from each argument. The same command also works on older
Bazarr versions that substituted into a shell string.

Verified against `lscr.io/linuxserver/bazarr` v1.6.2, which includes `curl`
and `sh`. [`scripts/e2e-bazarr.sh`](scripts/e2e-bazarr.sh) reproduces the hook
end to end: it runs Bazarr's own `pp_replace()` and `postprocessing()` against
a subsyncer container.

Bazarr's log shows the response, e.g.
`Post-processing result ...: {"job_id":12,"status":"queued","deduplicated":false}`.
The outcome is available at `GET /jobs/12` and in subsyncer's log.

## CLI

```sh
subsyncer sync <video> <subtitle> [--reference auto|audio|embedded] [--dry-run] [--output <path>] [--no-backup] [--json]
subsyncer scan <dir> [--dry-run] [--force] [--include-forced] [--json]
subsyncer serve            # HTTP daemon (see below)
subsyncer health           # exit 0 if the daemon's /healthz answers (container healthcheck)
```

`sync` exits with 0 for `synced`/`already_synced`, 2 for `low_confidence` and 1
for `failed`.

`scan` pairs each subtitle with the video whose name it extends (`Movie.mkv` ←
`Movie.en.srt`, `Movie.srt`). It skips `*.forced.*` unless `--include-forced` is
given, and records processed pairs in SQLite, so re-runs only touch new or
changed files. Failures are retried on the next scan.

Example:

```
$ subsyncer sync movie.mkv movie.nl.srt
movie.nl.srt: synced
  reference audio:1 | offset -0.010s | splits 1 | speech overlap 0.413 -> 0.621 | confidence 15.4
    from    23.000s  offset +0.100s  (35 cues)
    from   332.500s  offset -29.880s  (41 cues)
  speech overlap 0.413 -> 0.621
  wrote movie.nl.srt (backup movie.nl.srt.bak)
```

## HTTP API (`serve`)

| Endpoint | |
|---|---|
| `POST /sync` | JSON or form-encoded: `video`, `subtitle`, optional `reference` (`auto`/`audio`/`embedded`), `dry_run`. Returns **202** `{"job_id", "status", "deduplicated"}`. A request for a subtitle that is still waiting in the queue returns the existing job. Missing files return **422** with the mapped path. |
| `GET /jobs/{id}` | `status` (`queued`/`running`/`done`), `outcome`, `reason`, `offset_ms`, `ratio`, `splits`, `score_before`, `score_after`, `prominence`, and the full `result` report. |
| `GET /jobs?limit=50&status=done` | Recent jobs, newest first. |
| `GET /healthz` | `{"status":"ok","queued":..,"running":..}`. No auth. |

If `SUBSYNCER_API_KEY` is set, every endpoint except `/healthz` requires `X-Api-Key`.
Jobs run on a bounded worker pool. Jobs still queued or running at shutdown are
re-queued on the next start.

## Configuration

All options are environment variables (or the matching `--flag`):

| Variable | Default | |
|---|---|---|
| `SUBSYNCER_LISTEN` | `0.0.0.0:8080` | `serve` listen address |
| `SUBSYNCER_WORKERS` | `1` | concurrent jobs (decoding + VAD is CPU-heavy) |
| `SUBSYNCER_DATA_DIR` | `/data` | SQLite cache + job history; outside Docker it falls back to `~/.cache/subsyncer` |
| `SUBSYNCER_PATH_MAP` | | `/bazarr/path=/local/path,...` (longest prefix wins) |
| `SUBSYNCER_BACKUP` | `true` | keep `<subtitle>.bak` when overwriting |
| `SUBSYNCER_MAX_OFFSET_SECS` | `60` | global offset search range (±) |
| `SUBSYNCER_WINDOW_SECS` | `60` | piecewise search window around the global result (±) |
| `SUBSYNCER_SPLIT_PENALTY` | `2000` | cost of a split (higher means fewer splits) |
| `SUBSYNCER_NO_SPLITS` | `false` | global offset + framerate only |
| `SUBSYNCER_MIN_CONFIDENCE` | `6.0` | minimum correlation-peak prominence to write |
| `SUBSYNCER_MIN_IMPROVEMENT` | `0.01` | minimum gain in speech overlap to write |
| `SUBSYNCER_API_KEY` | | require `X-Api-Key` |
| `SUBSYNCER_LOG_LEVEL` | `info` | `error`…`trace` or a tracing filter |
| `SUBSYNCER_LOG_FORMAT` | `text` | `text` or `json` |
| `SUBSYNCER_FFMPEG`, `SUBSYNCER_FFPROBE` | `ffmpeg`, `ffprobe` | binaries (run as subprocesses, never linked) |
| `PUID`, `PGID`, `UMASK` | `1000`, `1000`, `002` | Docker only: the user that runs subsyncer and owns written files |

## How it works

All timelines are boolean arrays on a 10 ms grid.

1. **Probe** streams with `ffprobe`.
2. **Reference timeline.**
   - *Embedded*: the text subtitle track (subrip, ass, mov_text, webvtt) with the
     most dialogue cues. Forced/signs tracks are skipped: those with the forced
     flag, "forced"/"signs" in the title, or fewer than 50% of the target's cues.
   - *Audio*: the default audio stream decoded by ffmpeg to 16 kHz mono. If the
     layout has a centre channel, that channel is used alone (`pan=mono|c0=FC`).
     Silero VAD v6 runs on 512-sample windows with its state carried between
     windows, and the result goes through hysteresis thresholding (0.5/0.35),
     gap filling, and resampling to 10 ms. The model is embedded in the binary.
3. **Cues.** Sound descriptions (`[door slams]`), music (`♪ ... ♪`), ASS
   comments, drawings and sign lines are left out of alignment. They still move
   with their neighbours.
4. **Global alignment.** FFT cross-correlation of ±1 signals for framerate ratios
   1, 25/23.976, 24/25, 24/23.976 and their inverses. Confidence is the z-score
   of the peak on a detrended correlation curve.
5. **Piecewise alignment.** A DP over (cue, offset): each cue scores +1 per
   speech frame and −λ per non-speech frame (O(1) via prefix sums), and starting
   a new segment costs `SPLIT_PENALTY`. A backward jump larger than the dialogue
   gap at that point also pays for the overlap it creates, because real cuts sit
   in gaps. The offset range covers every strong correlation peak and widens if
   a segment lands on its edge. Ratio 1 and the best other ratio are both run
   through the DP, and its objective decides between them. After that:
   - short or near-identical segments are merged;
   - **each segment must have its own sharp correlation peak**, otherwise it is
     merged into a neighbour;
   - overlaps from removed content are collapsed so cues stay in order.
6. **Quality gate.**
   - `already_synced`: nothing moves by 100 ms or more, or a correction of at
     most 300 ms gives no measurable gain.
   - `low_confidence`: the peak isn't prominent enough, or the overlap doesn't
     improve enough. Nothing is written.
   - `synced`: written. `failed`: an error, e.g. no audio or an unparsable file.
7. **Write.** Only timestamp bytes change. Encoding (UTF-8, UTF-16 and legacy
   code pages via chardetng), BOM, line endings and all styling are preserved,
   and an unshifted file renders byte-identical. The file is written to a temp
   file in the same directory, then renamed over the original. The previous
   version is kept as `.bak`.

Formats: SRT, WebVTT (including inline karaoke timestamps) and ASS/SSA.

## Evaluation

`crates/eval` is the evaluation harness:

```sh
cargo run --release -p subsyncer-eval -- synthetic --seeds 60 --compare [--hard]
cargo run --release -p subsyncer-eval -- corpus /path/to/videos-with-good-subs [--baselines]
```

**Synthetic** scenarios need no media and run in CI. Each one generates a
speech track, derives subtitle cues with realistic lead-in/out habits, adds VAD
noise (misses, false alarms, jitter), applies a perturbation and measures
recovery. Results for 60 episodes per row (`<=200ms` is the share of cues
within 200 ms of the truth in the output the user gets):

| perturbation | global only | piecewise | piecewise, hard noise¹ |
|---|---|---|---|
| already in sync (must be left untouched) | 100% (0 rewritten) | 100% (0 rewritten) | 100% (0 rewritten) |
| offset 0.3–1.5 s | 100% | 100% | 100% |
| offset 2–55 s | 100% | 100% | 100% |
| framerate mismatch + offset | 100% | 100% | 100% |
| video has an extra chunk (ad break) | 65.6% | **99.3%** | 98.5% |
| two extra chunks | 42.7% | **98.6%** | 97.2% |
| subtitle has extra content | 64.5% | **99.4%** | 98.9% |
| framerate + extra chunk | 65.2% | **99.3%** | 98.8% |
| subtitle for a different video (must not be written) | 0 of 60 rewritten | 0 of 60 rewritten | 0 of 60 rewritten |

¹ 30% of utterances missed by VAD, 3 false alarms per minute, 12% untranslated
lines, 8% sound-description cues.

Alignment takes about 0.3 s per episode on top of decoding. VAD on a 12-minute
film takes about 2.4 s on one core; cache hits skip it.

**Real corpus mode** takes a folder of videos with known-good subtitles next to
them, perturbs the subtitles (offsets, framerate, inserted and removed chunks),
re-syncs them, and reports the median absolute cue error and the share of cues
within 200 ms. With `--baselines` it also runs `ffsubsync` and `alass` when
they are on PATH (as processes, never linked).

Tested on *Tears of Steel* (CC-BY, Blender Foundation) with its official
subtitles:
- ±offsets, both 25↔23.976 directions and a 30 s split were recovered.
- All 76 cues landed within 200 ms after a split.
- The untouched subtitle and its translations (de, nl, ja, ru) are
  `already_synced`.
- Against the 5.1 track, the centre channel finds 13.3% speech versus 7.9% for
  a downmix.

Corpus mode on that film alone (4 perturbations per row), cues within 200 ms:

| offsets, framerate | extra chunk | two extra chunks | removed content | framerate + chunk |
|---|---|---|---|---|
| 100% | 98% | 91% | 85% | 71% |

Chunk recovery is weaker here than on the synthetic episodes. The film has only
76 cues in 12 minutes, so segments often have too few cues to pass the
per-segment confidence check, and the result falls back to fewer splits.
Calibrating on a corpus of real TV episodes is the next tuning step.

Subtitles typically start about 180 ms before the speech onset that VAD
detects and end about 60 ms after it, so audio-referenced corrections carry a
lead-in bias of roughly +100 ms. The gate treats corrections that small as
already synced.

## Docker

```sh
docker run -d --name subsyncer -e PUID=1000 -e PGID=1000 \
  -v ./data:/data -v /srv/media/tv:/tv ghcr.io/OWNER/subsyncer:latest
```

The image runs subsyncer as `PUID:PGID` (it uses `tini` and `gosu`, or runs
directly under `docker --user`). It has a `HEALTHCHECK` on `/healthz` and a
volume at `/data`. Images are built for amd64 and arm64.

The image is **Debian 13 slim** (~136 MB). The prebuilt ONNX Runtime 1.31 is
linked into the binary. FFmpeg is a minimal static 8.1 build made from source by
[`docker/build-ffmpeg.sh`](docker/build-ffmpeg.sh): every demuxer, native audio
decoders and text subtitle codecs, and no video, GPU or X11 libraries. That is
19 MB instead of the ~450 MB Debian's `ffmpeg` package pulls in. An audio
codec outside that set fails with a clear ffmpeg error.

Images that were measured and rejected:

| image | size | VAD on 12 min of audio |
|---|---|---|
| **Debian slim + minimal FFmpeg (used)** | **136 MB** | **~1.8 s** |
| Alpine (musl, Alpine's ONNX Runtime 1.23 and ffmpeg) | 209 MB | ~2.85 s |
| Debian slim + Debian's `ffmpeg` package | 566 MB | ~1.95 s |

Bookworm doesn't work, because the prebuilt ONNX Runtime needs glibc ≥ 2.38.

## Building

```sh
cargo build --release -p subsyncer     # needs network once: ort downloads ONNX Runtime
cargo test --workspace
```

ONNX Runtime is linked statically, so the binary depends only on
glibc/libstdc++. Release binaries need **glibc ≥ 2.38 and the libstdc++ from
GCC 13+** (Ubuntu 24.04+, Debian 13+, Fedora 39+), the floor set by the
prebuilt ONNX Runtime. FFmpeg (`ffmpeg`, `ffprobe`) must be on PATH at runtime.

Workspace layout:
- `crates/core`: parsers, alignment, scoring. Pure: no IO, no ffmpeg.
- `crates/cli`: the `subsyncer` binary, covering ffmpeg/ffprobe, VAD, HTTP, cache and config.
- `crates/eval`: the evaluation harness.

GitHub Actions:
- `ci.yml`: fmt, clippy, tests and the synthetic eval report.
- `docker.yml`: multi-arch GHCR images built on native amd64 and arm64 runners.
- `release.yml`: binaries attached to the GitHub release.

## License

MIT OR Apache-2.0. The binary embeds the Silero VAD model (MIT) and links ONNX
Runtime (MIT); see [THIRD_PARTY.md](THIRD_PARTY.md).
