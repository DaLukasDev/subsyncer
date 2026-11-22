# Evaluation

`crates/eval` measures how well subsyncer recovers known-good timing:

```sh
cargo run --release -p subsyncer-eval -- synthetic --seeds 60 --compare [--hard]
cargo run --release -p subsyncer-eval -- corpus /path/to/videos-with-good-subs [--baselines]
cargo run --release -p subsyncer-eval -- embedded --seeds 400 [--hard] [--show-failures]
cargo run --release -p subsyncer-eval -- library /path/to/subsyncer.db [--limit 100] [--show-failures]
```

Every alignment change should come with a before/after table from these.

## Synthetic scenarios

These need no media and run in CI. Each scenario:
1. generates a speech track;
2. derives subtitle cues with realistic lead-in/out habits;
3. adds VAD noise (misses, false alarms, jitter);
4. applies a perturbation;
5. measures recovery.

Results for 60 episodes per row. `<=200ms` is the share of cues within 200 ms
of the truth in the output the user ends up with:

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
| offset changes 2–4 times by 0.4–1.5 s (act breaks)² | 36.1% | **98.4%** | 96.2% |
| slow drift, 24 vs 23.976 fps² | 100% | 100% | 100% |
| subtitle for a different video (must not be written) | 0 of 60 rewritten | 0 of 60 rewritten | 0 of 60 rewritten |

¹ 30% of utterances missed by VAD, 3 false alarms per minute, 12% untranslated
lines, 8% sound-description cues.

² Not in the default set: `--kinds steps,slow_drift`. Before the search for
small steps (0.2.0), the steps row was 92.7% (80.3% with hard noise).

Alignment takes about 0.3 s per episode on top of decoding.

## Real corpus

`corpus` takes a folder of videos with known-good subtitles next to them. It
perturbs the subtitles (offsets, framerate, inserted and removed chunks),
re-syncs them, and reports the median absolute cue error and the share of cues
within 200 ms. With `--baselines` it also runs `ffsubsync` and `alass` when
they are on `PATH` (as processes, never linked).

Tested so far on *Tears of Steel* (CC-BY, Blender Foundation) with its official
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
For real TV episodes, see the [library](#real-speech-timelines-from-a-library)
results below.

## Real speech timelines from a library

`library` reads a `subsyncer.db`, the cache a deployment builds up, read-only.
It holds the speech timeline of every video whose full audio was analysed,
and the dialogue cues of their embedded subtitle tracks. The tracks the
embedded-track check accepted are in sync with their audio, so they serve as
the truth: each is perturbed and aligned against its own audio (the wrong
video perturbation uses another video's). This tests alignment on real
speech detection without needing the media.

- Real speech detection reports speech about 100 ms after a cue starts, so
  each track's truth is moved to where the aligner puts the unperturbed
  track. For in-sync input that is left alone, the `<=200ms` column then
  shows that lag rather than an error; judge it by how many were rewritten.
- `--video <part of a path>` aligns one video's track (or `--subtitle` a
  file) and prints the result and where each two-minute stretch of it lines
  up. `--perturb <kind> --seed <n>` perturbs it first, as printed by
  `--show-failures`.

243 accepted tracks from a library of TV episodes (2026-11), one perturbation
each, cues within 200 ms (and the share of files with at least 95% of them):

| perturbation | 0.2.0 | with the search for small steps |
|---|---|---|
| in sync (rewritten) | 3 of 243 | 3 of 243 (the same three, by 0.25–0.31 s) |
| offset 0.3–1.5 s | 85.7% (85.6%) | 85.7% (85.6%) |
| offset 2–55 s | 96.0% (95.9%) | 96.0% (95.9%) |
| framerate mismatch + offset | 91.4% (88.9%) | **94.8% (94.2%)** |
| video has an extra chunk | 92.4% (82.3%) | 92.4% (82.3%) |
| two extra chunks | 89.3% (72.0%) | 89.4% (72.0%) |
| subtitle has extra content | 93.0% (86.0%) | 93.3% (86.4%) |
| framerate + extra chunk | 84.4% (69.1%) | **89.8% (78.2%)** |
| offset changes 2–4 times by 0.4–1.5 s | 58.3% (25.9%) | **73.7% (47.3%)** |
| slow drift, 24 vs 23.976 fps | 87.1% (81.5%) | **92.1% (88.9%)** |
| subtitle for a different video | 0 of 243 rewritten | 0 of 243 rewritten |

Real speech is much harder than the synthetic episodes. In shows with dense
dialogue (60% of the audio is speech), a step of half a second barely
changes how well the cues overlap speech, and stays undetected. Lowering the
main split penalty instead was measured too: at 1000 the steps row reached
67%, but framerate fell to 86% and slow drifts to 74%; at 600, 72% for steps
cost framerate 69% and slow drifts 68%.

## Embedded track check

`embedded` tests the check that decides whether a subtitle track inside the
video can be trusted as the reference
([how it works](how-it-works.md#checking-embedded-tracks)).
- **Setup:** a perturbed copy of the true cues plays the embedded track, and
  the synthetic speech plays its audio. A track is bad if any cue is off by
  more than 250 ms.
- **Cost of a bad track that gets through:** a correct subtitle (the truth
  with jittered boundaries and 10% of lines dropped) is synced against it. We
  count its lines that end up more than 200 ms off.

400 episodes per row, default noise:

| embedded track | used | used without the full audio | lines of a correct subtitle moved wrongly |
|---|---|---|---|
| in sync | 400 of 400 | 98% | 0 |
| offset 0.3–1.5 s | 0 | | |
| offset 2–55 s | 0 | | |
| framerate mismatch + offset | 0 | | |
| video has an extra chunk | 0 | | |
| two extra chunks | 0 | | |
| track has extra content | 0 | | |
| framerate + extra chunk | 0 | | |
| track for a different video | 0 | | |
| extra chunk in the last 15% | 25 of 392 | | 0 |

With hard noise:
- In-sync tracks are used in 399 of 400 cases, 53% of them without the full
  audio.
- Of the bad tracks, only late chunks get through: 27 of 394.
- Those cost 0.45% of a subtitle's lines: at most 12 lines, at the very end.

On real audio, `scripts/embedded-check-e2e.py` builds a 37-minute file from
*Tears of Steel* played three times. It embeds perturbed copies of the
official Dutch subtitle and syncs three English subtitles against each:
correct, correct + 3 s, and one with a cut of its own.
- **Bad tracks:** offsets of 0.5, 2, 5 and 30 s, 25→23.976 fps, cuts at 5%
  and 50%, and a stretch at 36–45% shifted by 4 s. All were rejected; the
  shifted stretch was caught by the region check. Every result was within
  130 ms of the truth. With the check off, the same runs moved correct
  subtitles by 0.5 to 83 s.
- **Good track:** used in every run, including against the subtitle with its
  own cut, whose split was recovered exactly.
- **Late cut:** a cut at 93% of the file got past the samples, but changed no
  result.
- **Time:** 1.4 s with samples, 5.4 s with the full audio, 0.2 s unchecked.

## Known bias

Subtitles typically start about 180 ms before the speech onset that VAD detects
and end about 60 ms after it. Audio-referenced corrections therefore carry a
lead-in bias of roughly +100 ms, within the 200 ms target. The gate treats
corrections that small as already synced.
