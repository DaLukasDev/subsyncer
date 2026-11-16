# Evaluation

`crates/eval` measures how well subsyncer recovers known-good timing:

```sh
cargo run --release -p subsyncer-eval -- synthetic --seeds 60 --compare [--hard]
cargo run --release -p subsyncer-eval -- corpus /path/to/videos-with-good-subs [--baselines]
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
| subtitle for a different video (must not be written) | 0 of 60 rewritten | 0 of 60 rewritten | 0 of 60 rewritten |

¹ 30% of utterances missed by VAD, 3 false alarms per minute, 12% untranslated
lines, 8% sound-description cues.

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
Calibrating on a corpus of real TV episodes is the next tuning step;
contributions of results are welcome.

## Known bias

Subtitles typically start about 180 ms before the speech onset that VAD detects
and end about 60 ms after it. Audio-referenced corrections therefore carry a
lead-in bias of roughly +100 ms, within the 200 ms target. The gate treats
corrections that small as already synced.
