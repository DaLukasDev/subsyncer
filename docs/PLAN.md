# subsyncer — implementation brief

## Goal
A Rust tool that syncs subtitle files to video, replacing Bazarr's built-in
sync (ffsubsync). Ships as a single binary and a Docker image. Bazarr calls it
through its custom post-processing command.

## Starting state
- Project dir: /home/dalukas/dev/subtitle-syncer — empty, not a git repo.
- Machine: Linux (WSL2). docker and ffmpeg installed. cargo is NOT installed;
  install rustup first.
- Licence: MIT OR Apache-2.0. Do not link GPL code (no `alass-core`).

## Why ffsubsync falls short, and the design answer
1. Weak voice detection (confused by music/effects) -> Silero VAD.
2. Only one global offset + framerate fix -> piecewise alignment with splits
   (ad breaks, recaps, different cuts).
3. Ignores embedded subtitle tracks -> use them as reference when usable.
4. Can make good subtitles worse -> quality gate, write only on improvement.
5. Re-decodes audio per subtitle -> cache the speech timeline per media file.

## Repo layout (cargo workspace)
- crates/core  — pure library: parsers, alignment, scoring. No IO, no ffmpeg.
- crates/cli   — binary `subsyncer`: ffmpeg/ffprobe, VAD, HTTP, cache, config.
- crates/eval  — evaluation harness binary.
- Dockerfile, docker-compose.example.yml, .github/workflows/, README.md

## Crates
clap, axum, tokio, rustfft, ort (ONNX Runtime, for Silero), encoding_rs,
chardetng, rusqlite (bundled), serde, tracing, anyhow/thiserror.
ffmpeg and ffprobe are subprocesses, never linked.

## Subcommands
- `subsyncer sync <video> <subtitle> [--reference auto|audio|embedded]
  [--dry-run] [--output <path>] [--no-backup]`
- `subsyncer serve` — HTTP daemon with job queue.
- `subsyncer scan <dir> [--dry-run]` — walk a library, sync every
  video+subtitle pair not already processed (tracked in SQLite).

## Pipeline
All timelines are boolean arrays on a 10 ms grid.

1. Probe. `ffprobe -of json -show_streams` for audio and subtitle streams.

2. Reference timeline.
   - Embedded: if a text subtitle stream exists (subrip, ass, mov_text),
     extract it with ffmpeg and use its cue intervals. Reject it if it looks
     like a forced/signs track: fewer than 50% of the target's cue count.
     Then fall back to audio.
   - Audio: default audio stream -> 16 kHz mono s16le via ffmpeg pipe. If the
     layout has a centre channel, use it alone (`pan=mono|c0=FC`), since
     dialogue lives there. Run Silero VAD (512-sample windows, state carried
     between windows), threshold to speech/non-speech, resample to 10 ms.
   - Embed the Silero ONNX model with `include_bytes!`.

3. Global alignment (crates/core).
   - Map both timelines to +1 (speech/cue) and -1 (silence/gap).
   - FFT cross-correlation, take the peak offset.
   - Repeat for framerate ratios: 1, 25/23.976, 23.976/25, 25/24, 24/25,
     24/23.976, 23.976/24 (scale subtitle times first). A ratio other than 1
     must beat ratio 1 by a configurable margin to be chosen.

4. Piecewise alignment (crates/core), around the global result.
   - Offsets d range over global offset ± window (default 60 s, 10 ms steps).
   - score(i, d) = sum of reference values over cue i shifted by d, where
     speech = +1 and non-speech = -lambda. O(1) via prefix sums.
   - dp[i][d] = score(i, d) + max(dp[i-1][d], max_d'(dp[i-1][d']) - penalty)
   - Keep one backpointer bit per cell (stay/jump) plus the per-row argmax.
   - Post-process: merge segments shorter than N cues into a neighbour;
     ensure shifted cues stay in order and do not go negative.
   - If the result has zero splits, output is just the global offset.

5. Quality gate.
   - Metric: fraction of cue time overlapping reference speech, plus peak
     prominence of the correlation (peak vs. the rest of the offsets).
   - Outcomes: `already_synced` (offset under 100 ms, no splits),
     `synced`, `low_confidence` (nothing written), `failed`.
   - Write only if the after-score beats the before-score by a margin.
   - All thresholds configurable; defaults tuned with the eval harness.

6. Write. Rewrite timestamps only; preserve encoding, BOM, line endings,
   styling and all non-timing content. Write to a temp file and rename.
   Keep `<name>.bak` unless disabled.

Formats: SRT, ASS/SSA, WebVTT. Detect encoding with chardetng.

## HTTP API (`serve`)
- POST /sync — accepts JSON and form-encoded: `video`, `subtitle`,
  optional `reference`, `dry_run`. Returns 202 `{job_id}`. Dedupe queued jobs
  for the same subtitle path.
- GET /jobs/{id}, GET /jobs — status, outcome, offset, splits, scores.
- GET /healthz.
- Optional API key via `X-Api-Key`.
- Bounded worker pool; decoding is CPU-heavy.

## Config (env vars, all prefixed SUBSYNCER_)
LISTEN (0.0.0.0:8080), WORKERS (1), DATA_DIR (/data), PATH_MAP
(`/bazarr/path=/local/path,...`), BACKUP (true), MAX_OFFSET_SECS (60),
SPLIT_PENALTY, MIN_CONFIDENCE, API_KEY, LOG_LEVEL.

## Cache (SQLite in DATA_DIR)
- speech timelines keyed by (path, size, mtime, stream index, VAD version),
  stored as a compressed blob.
- job history and scan state.

## Bazarr integration
- Disable Bazarr's automatic sync.
- Post-processing command (form-encoded so paths with quotes are safe):

  ```sh
  curl -s -X POST http://subsyncer:8080/sync \
    --data-urlencode "video={{episode}}" \
    --data-urlencode "subtitle={{subtitles}}"
  ```

- The subsyncer container mounts media at the same paths as Bazarr, or uses
  PATH_MAP.

## Docker
- Multi-stage with cargo-chef; runtime debian:bookworm-slim + ffmpeg.
- Non-root, PUID/PGID support, HEALTHCHECK on /healthz, volume /data.
- Multi-arch amd64 + arm64, pushed to GHCR by GitHub Actions.
- Binary is glibc-linked (ONNX Runtime); also attach it to GitHub releases.

## Eval harness (crates/eval)
- Synthetic tests with no media: generate a speech timeline, derive cues
  with noise, apply perturbations, assert recovery. These run in CI.
- Real corpus mode: a user-supplied folder of video + known-good subtitle.
  Perturb with offsets, framerate scaling and inserted/removed chunks; report
  median absolute cue error and % of cues within 200 ms.
- Optional baselines if installed: `ffsubsync` and `alass` CLIs (invoked as
  processes, never linked).

## Milestones and acceptance
1. Core CLI: workspace, SRT/ASS/VTT parse+write round-trip byte-identical when
   unshifted, ffmpeg extraction, Silero VAD, global offset + framerate.
   Done when: `sync` fixes a constant offset and a 25<->23.976 mismatch.
2. Eval harness with synthetic tests in CI.
   Done when: global-only results are reported per perturbation type.
3. Piecewise alignment, embedded-subtitle reference, quality gate.
   Done when: inserted-chunk perturbations recover with >=95% of cues within
   200 ms on synthetic data, and already-synced input is left untouched.
4. `serve`, queue, cache, Dockerfile, compose example, Bazarr hook.
   Done when: a Bazarr download triggers a sync end to end.
5. `scan`, README, CI release of image and binaries.
6. Out of scope for v1: Whisper/ASR anchoring.

## Verify before relying on (not confirmed)
- Bazarr's post-processing variable names (`{{episode}}`, `{{subtitles}}`)
  and whether `curl` exists in the Bazarr image in use.
- Silero VAD model version, input window size and tensor names.
- `ort` linking/packaging of ONNX Runtime on amd64 and arm64.
- SPLIT_PENALTY, lambda, margins: pick defaults from eval results, not guesses.
