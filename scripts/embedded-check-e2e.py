#!/usr/bin/env python3
"""End-to-end test of the embedded-track check on real audio.

Builds a long audio-only file by playing one video several times over
(sample-exact, so the subtitle timing stays right), embeds perturbed copies
of a correct subtitle track in it, and syncs correct (and deliberately
shifted or cut) external subtitles against each, with the check in sampled,
full and off mode. Prints one line per run and writes results.json.

Usage (from the repository root, after `cargo build --release`):

    python3 scripts/embedded-check-e2e.py WORKDIR \\
        --video tos_51.mkv --track TOS-nl.srt --target TOS-en.srt

The defaults point at testdata/tos/: Tears of Steel (CC-BY, Blender
Foundation, https://mango.blender.org/download/) with its 5.1 audio and the
official subtitles, whose translations all share the English timing. Needs
ffmpeg and ffprobe on PATH and about 300 MB in WORKDIR.
"""
import argparse
import json
import os
import re
import subprocess
import tempfile
import time

ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("workdir")
ap.add_argument("--video", default="testdata/tos/tos_51.mkv", help="video with good audio")
ap.add_argument("--track", default="testdata/tos/TOS-nl.srt", help="correct subtitle to embed (perturbed)")
ap.add_argument("--target", default="testdata/tos/TOS-en.srt", help="correct subtitle to sync")
ap.add_argument("--repeat", type=int, default=3, help="times to play the video (default 3)")
ap.add_argument("--bin", default="target/release/subsyncer")
args = ap.parse_args()
os.makedirs(args.workdir, exist_ok=True)
OUT = args.workdir


def run(*cmd):
    return subprocess.run(cmd, check=True, capture_output=True, text=True)


def parse_srt(path):
    cues = []
    for block in re.split(r"\n\s*\n", open(path, encoding="utf-8-sig").read().strip()):
        lines = block.splitlines()
        for i, line in enumerate(lines):
            m = re.match(r"(\d+):(\d+):(\d+),(\d+) --> (\d+):(\d+):(\d+),(\d+)", line)
            if m:
                g = list(map(int, m.groups()))
                start = ((g[0] * 60 + g[1]) * 60 + g[2]) * 1000 + g[3]
                end = ((g[4] * 60 + g[5]) * 60 + g[6]) * 1000 + g[7]
                cues.append([start, end, "\n".join(lines[i + 1 :])])
                break
    return cues


def ts(ms):
    ms = max(0, int(round(ms)))
    return f"{ms // 3600000:02}:{ms // 60000 % 60:02}:{ms // 1000 % 60:02},{ms % 1000:03}"


def write_srt(path, cues):
    with open(path, "w", encoding="utf-8") as f:
        for i, (s, e, t) in enumerate(sorted((c for c in cues if c[1] > 0), key=lambda c: c[0]), 1):
            f.write(f"{i}\n{ts(s)} --> {ts(e)}\n{t}\n\n")


def duration_ms(path):
    out = run("ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path).stdout
    return float(out) * 1000


# The long file: 5.1 kept so the centre-channel path is used; 16 kHz FLAC
# keeps it small. The concat filter joins sample-exactly (the concat demuxer
# would add a few tens of ms at each join).
one = os.path.join(OUT, "one.mkv")
long_mkv = os.path.join(OUT, "long.mkv")
if not os.path.exists(long_mkv):
    run("ffmpeg", "-v", "error", "-y", "-i", args.video, "-map", "0:a:0", "-af", "aresample=16000", "-c:a", "flac", one)
    inputs = [x for _ in range(args.repeat) for x in ("-i", args.video)]
    graph = "".join(f"[{k}:a:0]" for k in range(args.repeat)) + f"concat=n={args.repeat}:v=0:a=1,aresample=16000[a]"
    run("ffmpeg", "-v", "error", "-y", *inputs, "-filter_complex", graph, "-map", "[a]", "-c:a", "flac", long_mkv)
seg = duration_ms(one)
L = duration_ms(long_mkv)


def tile(path):
    return [[s + k * seg, e + k * seg, t] for k in range(args.repeat) for s, e, t in parse_srt(path)]


def shift(cues, by, frm=0, to=float("inf")):
    return [[s + by, e + by, t] if frm <= s < to else [s, e, t] for s, e, t in cues]


track = tile(args.track)
target = tile(args.target)
variants = {
    "good": track,
    "offset +0.5s": shift(track, 500),
    "offset -2s": shift(track, -2000),
    "offset +5s": shift(track, 5000),
    "offset +30s": shift(track, 30000),
    "framerate 25/23.976": [[s * 23.976 / 25, e * 23.976 / 25, t] for s, e, t in track],
    "cut at 5% (+6s after)": shift(track, 6000, 0.05 * L),
    "cut at 50% (+4s after)": shift(track, 4000, 0.5 * L),
    "cut at 93% (+10s after)": shift(track, 10000, 0.93 * L),
    "stretch 36-45% +4s": shift(track, 4000, 0.36 * L, 0.45 * L),
}
# Each target with the correction a right answer applies: (before the
# point, point, after the point).
targets = {
    "correct": (target, (0, None, 0)),
    "correct +3s": (shift(target, 3000), (-3000, None, -3000)),
    "own cut at 55% (+8s after)": (shift(target, 8000, 0.55 * L), (0, 0.55 * L, -8000)),
}


def max_error(report, cues, expect):
    """Largest error (ms) of any cue in the result, from the report's segments."""
    before, point, after = expect
    written = report.get("outcome") == "synced"
    worst = 0
    for s, _, _ in cues:
        want = before if point is None or s < point else after
        got = 0
        if written:
            for sg in report.get("segments") or []:
                if sg["from_ms"] <= s:
                    got = sg["offset_ms"]
            got += s * report.get("ratio", 1.0) - s
        worst = max(worst, abs(got - want))
    return worst


rows = []
for tname, (tcues, expect) in targets.items():
    tpath = os.path.join(OUT, "target.srt")
    write_srt(tpath, tcues)
    for vname, vcues in variants.items():
        if tname != "correct" and vname not in ("good", "offset +5s", "cut at 93% (+10s after)"):
            continue
        vsrt = os.path.join(OUT, "track.srt")
        write_srt(vsrt, vcues)
        vmkv = os.path.join(OUT, "variant.mkv")
        run("ffmpeg", "-v", "error", "-y", "-i", long_mkv, "-i", vsrt, "-map", "0", "-map", "1", "-c", "copy", "-c:s", "srt", vmkv)
        for mode in ("sampled", "full", "off"):
            with tempfile.TemporaryDirectory(dir=OUT) as d:
                t = time.time()
                p = subprocess.run(
                    [args.bin, "sync", "--dry-run", "--json", "--embedded-check", mode, "--data-dir", d, vmkv, tpath],
                    capture_output=True,
                    text=True,
                )
                wall = time.time() - t
            r = json.loads(p.stdout)
            ec = r.get("embedded_check") or {}
            row = {
                "target": tname,
                "track": vname,
                "mode": mode,
                "outcome": r.get("outcome"),
                "reference": r.get("reference"),
                "method": ec.get("method", "-"),
                "summary": ec.get("summary", ""),
                "max_error_ms": max_error(r, tcues, expect),
                "seconds": round(wall, 2),
            }
            rows.append(row)
            print(
                f"{tname:26s} | {vname:24s} | {mode:7s} | {row['reference'] or '-':11s} | {row['method']:12s} | "
                f"{str(row['outcome']):14s} | err {row['max_error_ms']:>6.0f} ms | {wall:5.2f}s | {row['summary']}",
                flush=True,
            )
json.dump(rows, open(os.path.join(OUT, "results.json"), "w"), indent=1)
bad = [r for r in rows if r["mode"] != "off" and r["max_error_ms"] > 200]
print(f"\n{len(rows)} runs; {len(bad)} with the check on ended more than 200 ms off")
