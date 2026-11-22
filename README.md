# subsyncer

[![CI](https://github.com/DaLukasDev/subsyncer/actions/workflows/ci.yml/badge.svg)](https://github.com/DaLukasDev/subsyncer/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**subsyncer fixes subtitles that are out of sync with your video.** It listens
to the dialogue in the video, compares it with the subtitle's timing, and
shifts the subtitle so lines appear when people actually speak.

It is built to replace [Bazarr](https://www.bazarr.media/)'s built-in sync.
Bazarr hands every subtitle it downloads to subsyncer, which fixes it in the
background. You can also run it by hand on single files or a whole library.

It handles:

- subtitles that are a few seconds, up to a minute, **early or late**;
- subtitles made for a **different frame rate** (e.g. 25 vs 23.976 fps) that
  drift further off as the episode goes on;
- videos with an **extra ad break, recap or a different cut**, where only part
  of the subtitle is off;
- subtitles from a release whose **act breaks have other lengths**, so the
  offset changes by a second or so a few times during the episode;
- **SRT, ASS/SSA and WebVTT** in any text encoding. Only timestamps change;
  styling and text stay exactly as they were.

It **leaves a subtitle alone when it's already right, or when it isn't sure**,
so a sync never makes a good subtitle worse. When it does change a file, the
original is kept as `<subtitle>.bak`.

### Why not Bazarr's built-in sync (ffsubsync)?

| ffsubsync | subsyncer |
|---|---|
| Voice detection gets confused by music and effects | Neural voice detection (Silero VAD), listening to the centre channel where dialogue lives |
| One shift for the whole file | Also fixes files where only part is off (ad breaks, different cuts) |
| Ignores subtitle tracks already inside the video | Uses them as a precise reference when available, after checking them against the audio |
| Can make a correct subtitle worse | Writes only when it's confident and the result is measurably better |
| Decodes the audio again for every subtitle | Remembers each video, so the second language is instant |

## Quick start with Bazarr

You need Docker with Compose, and Bazarr running in Docker on the same machine.

The image is `ghcr.io/dalukasdev/subsyncer` (amd64 and arm64): `:latest` is
the newest release, `:0.2` the newest 0.2.x, a full version like `:0.2.0` pins
one, and `:edge` follows the `main` branch.

### 1. Add subsyncer next to Bazarr

Add this service to the `docker-compose.yml` that runs Bazarr (full example:
[`docker-compose.example.yml`](docker-compose.example.yml)):

```yaml
  subsyncer:
    image: ghcr.io/dalukasdev/subsyncer:latest
    environment:
      - PUID=1000      # the user that owns your media files (same as Bazarr's)
      - PGID=1000
    volumes:
      - ./subsyncer/data:/data
      - /srv/media/movies:/movies   # mount media at the SAME paths as Bazarr
      - /srv/media/tv:/tv
    restart: unless-stopped
```

The important part is the media volumes: subsyncer must see the files at **the
same paths as Bazarr does**. If it can't, see
[troubleshooting](#troubleshooting).

Start it:

```sh
docker compose up -d subsyncer
docker compose logs subsyncer    # should end with "subsyncer listening"
```

### 2. Point Bazarr at it

In Bazarr, go to **Settings → Subtitle Processing**:

1. Under **Audio Synchronization**, turn **off** *Enable Automatic Subtitles
   Audio Synchronization*. subsyncer takes over this job.
2. Under **Custom Post-Processing**, turn on *Custom Post-Processing* and set
   **Command** to exactly:

   ```sh
   sh -c 'curl -fsS -X POST --data-urlencode "video=$1" --data-urlencode "subtitle=$2" http://subsyncer:8080/sync' _ "{{episode}}" "{{subtitles}}"
   ```

3. Leave both *Score Threshold For Post-Processing* options **off**, so every
   download gets checked.
4. Save.

The command looks odd for a reason: it's the form that survives file names
with spaces, quotes and apostrophes in all current Bazarr versions
([details](docs/how-it-works.md#the-bazarr-command)).

### 3. Check that it works

Download or search a subtitle in Bazarr, then:

- Bazarr's log (**System → Logs**) shows a line like
  `Post-processing result for file ...: {"job_id":1,"status":"queued",...}`;
- `docker compose logs subsyncer` shows the job, for example
  `job finished job=1 outcome="synced" offset_ms=-7300`.

That's it. From now on every subtitle Bazarr downloads is checked and, if
needed, fixed.

## What the results mean

| outcome | what happened to your file |
|---|---|
| `synced` | It was out of sync and has been fixed. The original is in `<subtitle>.bak`. |
| `already_synced` | It was already in sync. Not touched. |
| `low_confidence` | subsyncer couldn't tell for sure (little dialogue, music-heavy audio, or a subtitle for a different version). Not touched. |
| `failed` | Something went wrong, e.g. the video has no audio or the file couldn't be read. The reason is in the log. Not touched. |

**To undo a change**, put the backup back:
`mv "Movie.en.srt.bak" "Movie.en.srt"`. If subsyncer changes the same file
again later, the backup keeps the original; it's only replaced once a new
subtitle arrives (a new download, say).

## Using it without Bazarr

The same image doubles as a command-line tool. Mount your media and pass a
command:

```sh
# Fix one subtitle (add --dry-run to only see what it would do)
docker run --rm -e PUID=$(id -u) -e PGID=$(id -g) -v /srv/media:/media ghcr.io/dalukasdev/subsyncer:latest \
  sync "/media/movies/Movie (2010)/Movie (2010).mkv" "/media/movies/Movie (2010)/Movie (2010).en.srt"

# Check a whole library; pairs each subtitle with the video it belongs to
docker run --rm -e PUID=$(id -u) -e PGID=$(id -g) -v /srv/media:/media -v ./subsyncer/data:/data ghcr.io/dalukasdev/subsyncer:latest \
  scan /media --dry-run
```

Example output:

```
Movie (2010).en.srt: synced
  reference audio:1 | offset -0.010s | splits 1 | speech overlap 0.413 -> 0.621 | confidence 10.1
    from    23.000s  offset +0.100s  (35 cues)
    from   332.500s  offset -29.880s  (41 cues)
  speech overlap 0.413 -> 0.621
  wrote Movie (2010).en.srt (backup Movie (2010).en.srt.bak)
```

Here the first part of the subtitle needed nothing, and from 5:32 on it was
30 seconds late, as if the subtitle came from a version with an extra scene.

All commands:

```sh
subsyncer sync <video> <subtitle> [--reference auto|audio|embedded] [--dry-run] [--output <path>] [--no-backup] [--json]
subsyncer scan <dir> [--dry-run] [--force] [--include-forced] [--no-backup] [--jobs N] [--json]
subsyncer serve     # the HTTP service Bazarr talks to (the image's default)
subsyncer health    # exits 0 if the service is up (used by the Docker healthcheck)
```

`scan` remembers what it already processed, so running it again only looks at
new or changed files. Files that came back `low_confidence` are tried again
after an upgrade or a change of settings. It skips forced subtitles
(`*.forced.*`) unless you pass `--include-forced`, and `--jobs N` works on N
videos at once. `sync` exits with 0 for `synced`/`already_synced`, 2 for
`low_confidence` and 1 for `failed`.

## Configuration

Everything is set with environment variables. Most setups need none besides
`PUID`/`PGID`.

| Variable | Default | What it does |
|---|---|---|
| `PUID`, `PGID` | `1000`, `1000` | User and group that subsyncer runs as and that owns the files it writes |
| `UMASK` | `002` | File creation mask inside the container |
| `SUBSYNCER_PATH_MAP` | | Translate paths Bazarr sends: `/bazarr/path=/subsyncer/path,...` |
| `SUBSYNCER_API_KEY` | | Require an `X-Api-Key` header. Add `-H "X-Api-Key: <key>"` after `-fsS` in the Bazarr command. |
| `SUBSYNCER_BACKUP` | `true` | Keep `<subtitle>.bak` when changing a file |
| `SUBSYNCER_WORKERS` | `1` | How many subtitles to process at once. Each one uses about one CPU core while decoding. |
| `SUBSYNCER_LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` or `trace` |
| `SUBSYNCER_LOG_FORMAT` | `text` | `text` or `json` |
| `SUBSYNCER_LISTEN` | `0.0.0.0:8080` | Address of the HTTP service |
| `SUBSYNCER_ALLOWED_DIRS` | | Only sync files under these directories (comma-separated, as subsyncer sees them), e.g. `/movies,/tv` |
| `SUBSYNCER_DATA_DIR` | `/data` | Where the cache and job history live. Outside Docker the default is `~/.cache/subsyncer`. |
| `SUBSYNCER_JOB_RETENTION_DAYS` | `30` | Delete finished jobs from the history after this many days (`0` keeps them) |

Tuning (the defaults come from the [evaluation](docs/evaluation.md)):

| Variable | Default | What it does |
|---|---|---|
| `SUBSYNCER_MAX_OFFSET_SECS` | `60` | Largest overall shift searched for (±) |
| `SUBSYNCER_WINDOW_SECS` | `60` | How far a part of the file may shift from the overall result (±) |
| `SUBSYNCER_SPLIT_PENALTY` | `2000` | Higher values make subsyncer more reluctant to split a file into parts |
| `SUBSYNCER_NO_SPLITS` | `false` | Only ever apply one shift (plus frame-rate correction) |
| `SUBSYNCER_MIN_CONFIDENCE` | `6.0` | How sure it must be before writing |
| `SUBSYNCER_MIN_IMPROVEMENT` | `0.01` | How much better the result must be before writing |
| `SUBSYNCER_EMBEDDED_CHECK` | `sampled` | How a subtitle track inside the video is checked against the audio before it's used as the reference: `sampled` (a few short audio samples, the full audio only when they don't settle it), `full` (always the full audio) or `off` (trust it) |
| `SUBSYNCER_FFMPEG`, `SUBSYNCER_FFPROBE` | `ffmpeg`, `ffprobe` | FFmpeg binaries, outside Docker |
| `SUBSYNCER_FFMPEG_TIMEOUT_SECS` | `120` | Stop an ffmpeg run that makes no progress for this long (a file on a hung network share) |

## Troubleshooting

#### Bazarr logs `curl: (22) ... error: 422`

subsyncer can't find the file at the path Bazarr sent. `docker compose logs
subsyncer` shows which one, e.g. `request rejected: video not found:
/tv/Show/S01E01.mkv`. Mount your media at the same paths in both containers,
or translate them. For example, if Bazarr sees `/tv` and subsyncer has the same
folder mounted at `/media/tv`:

```yaml
      - SUBSYNCER_PATH_MAP=/tv=/media/tv
```

#### Bazarr logs `curl: (22) ... error: 403`

`SUBSYNCER_ALLOWED_DIRS` is set and the file is outside those directories (the
log names it). Add its directory, as subsyncer sees it.

#### Bazarr logs `curl: (22) ... error: 401`

`SUBSYNCER_API_KEY` is set but the Bazarr command doesn't send it. Add
`-H "X-Api-Key: <key>"` after `-fsS`.

#### Bazarr logs `curl: (6) Could not resolve host: subsyncer`

Bazarr can't reach the container. Both services must be in the same Compose
file or Docker network, and the URL must use the service name (`subsyncer`).

#### Nothing happens after a download

Check that *Custom Post-Processing* is on and that the score threshold options
are off. Then look at Bazarr's log for a `Post-processing result` line.

#### Everything comes back `low_confidence`

Usually the subtitle belongs to a different release of the video, or the audio
has very little dialogue. Run it by hand with `--dry-run --log-level debug` to
see the details. If the video contains its own subtitle track and you know it's
in sync, try `--reference embedded`.

#### Files are owned by the wrong user

Set `PUID`/`PGID` to the user and group that own your media (`id youruser`
shows them).

#### It's slow

The first subtitle for a video takes roughly 5–10 seconds per 45-minute
episode on a modern CPU, mostly listening to the audio. Later subtitles for the same video reuse the result and
take well under a second. Keep `/data` on a volume so the cache survives
restarts.

## How it works

In short:
1. subsyncer builds a "who is speaking when" timeline for the video, by
   listening to the audio with a neural voice detector, or from a subtitle
   track inside the video. Such a track is only used once a few samples of
   the audio confirm it's in sync, since a track muxed in from another release
   can be off.
2. It compares that with the subtitle's timing, trying different frame rates.
3. If only parts of the file are off, it finds where they start and shifts each
   part separately.
4. A quality check decides whether the result is trustworthy enough to write.

Details: [docs/how-it-works.md](docs/how-it-works.md). Accuracy measurements:
[docs/evaluation.md](docs/evaluation.md). On synthetic tests, 99.6% of lines
end up within 200 ms of the right time, and subtitles that belong to a
different video are never changed. HTTP API: [docs/http-api.md](docs/http-api.md).

## Without Docker

Each [release](https://github.com/DaLukasDev/subsyncer/releases) has
`subsyncer` binaries for Linux on x86_64 and arm64. Unpack one and put it on
your `PATH`. You also need `ffmpeg` and `ffprobe` installed, and glibc 2.38 or
newer (Ubuntu 24.04+, Debian 13+, Fedora 39+).

```sh
v=v0.2.0   # the release you downloaded
tar xzf subsyncer-$v-x86_64-unknown-linux-gnu.tar.gz
./subsyncer-$v-x86_64-unknown-linux-gnu/subsyncer sync movie.mkv movie.en.srt
```

## Building from source

You need Rust 1.92 or newer, plus `ffmpeg` and `ffprobe` on `PATH` at runtime.
The first build downloads ONNX Runtime and needs internet access.

```sh
git clone https://github.com/DaLukasDev/subsyncer.git
cd subsyncer
cargo build --release -p subsyncer        # binary: target/release/subsyncer
docker build -t subsyncer:local .         # or the Docker image
```

The binary needs glibc 2.38 or newer (Ubuntu 24.04+, Debian 13+, Fedora 39+).

## Contributing

Issues and pull requests are welcome. Before sending a change, run:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you touch the alignment, include the before/after numbers from
`cargo run --release -p subsyncer-eval -- synthetic --seeds 60 --compare`
(see [docs/evaluation.md](docs/evaluation.md)). Results from real videos with
known-good subtitles (`subsyncer-eval corpus`) are especially valuable.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

subsyncer includes the Silero VAD model (MIT), ONNX Runtime (MIT) and, in the
Docker image, an LGPL build of FFmpeg. See [THIRD_PARTY.md](THIRD_PARTY.md).
