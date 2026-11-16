#!/usr/bin/env bash
# End-to-end check of the Bazarr hook without a full *arr stack:
# runs Bazarr's own pp_replace()/postprocessing() inside the linuxserver
# Bazarr image, against a subsyncer container on a shared network.
#
#   scripts/e2e-bazarr.sh <video> <subtitle> [image]
#
# The video and subtitle are copied (video hard-linked when possible) into a
# temp media dir with an awkward name (spaces, quotes, apostrophes).
set -euo pipefail
video=$(realpath "$1"); sub=$(realpath "$2"); image=${3:-subsyncer:dev}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d); trap 'docker rm -f e2e-subsyncer >/dev/null 2>&1; docker network rm e2e-net >/dev/null 2>&1; rm -rf "$work"' EXIT
dir="$work/tv/Show's \"Best\" Bits/Season 01"; mkdir -p "$dir" "$work/data"
base="Show - S01E01 - It's \"Pilot\""
ln "$video" "$dir/$base.mkv" 2>/dev/null || cp "$video" "$dir/$base.mkv"
cp "$sub" "$dir/$base.en.srt"
docker network create e2e-net >/dev/null
docker run -d --name e2e-subsyncer --network e2e-net --network-alias subsyncer \
  -e PUID="$(id -u)" -e PGID="$(id -g)" -e SUBSYNCER_API_KEY=s3cret \
  -v "$work/data:/data" -v "$work/tv:/tv" "$image" >/dev/null
for _ in $(seq 30); do docker exec e2e-subsyncer subsyncer health >/dev/null 2>&1 && break; sleep 1; done
cmd='sh -c '\''curl -fsS -X POST -H "X-Api-Key: s3cret" --data-urlencode "video=$1" --data-urlencode "subtitle=$2" http://subsyncer:8080/sync'\'' _ "{{episode}}" "{{subtitles}}"'
ep="/tv/Show's \"Best\" Bits/Season 01/$base"
docker run --rm --network e2e-net -v "$work/tv:/tv" -v "$here/bazarr_pp.py:/pp.py:ro" \
  --entrypoint /lsiopy/bin/python3 lscr.io/linuxserver/bazarr:latest /pp.py "$cmd" "$ep.mkv" "$ep.en.srt"
for _ in $(seq 120); do
  docker logs e2e-subsyncer 2>&1 | grep -q "job finished" && break; sleep 1
done
docker logs e2e-subsyncer 2>&1 | grep "job finished"
ls -la "$dir"
