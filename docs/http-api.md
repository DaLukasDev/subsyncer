# HTTP API

`subsyncer serve` (the Docker image's default command) listens on
`SUBSYNCER_LISTEN` (default `0.0.0.0:8080`). Jobs run on a bounded worker pool
(`SUBSYNCER_WORKERS`). Jobs still queued or running at shutdown are re-queued on
the next start.

If `SUBSYNCER_API_KEY` is set, every endpoint except `/healthz` requires the
`X-Api-Key` header.

## `POST /sync`

Queue a sync. The body is JSON or form-encoded:

| field | required | |
|---|---|---|
| `video` | yes | path to the video, as seen by Bazarr (`SUBSYNCER_PATH_MAP` is applied) |
| `subtitle` | yes | path to the subtitle file |
| `reference` | no | `auto` (default), `audio` or `embedded` |
| `dry_run` | no | `true` to analyse without writing |

```sh
curl -X POST http://localhost:8080/sync \
  --data-urlencode "video=/tv/Show/Season 01/S01E01.mkv" \
  --data-urlencode "subtitle=/tv/Show/Season 01/S01E01.en.srt"
```

Responses:

- **202** `{"job_id": 12, "status": "queued", "deduplicated": false}`. A
  request for a subtitle that is still waiting in the queue returns the
  existing job with `"deduplicated": true`.
- **400** for a malformed body or missing fields.
- **422** when a file doesn't exist. The error shows the path after
  `SUBSYNCER_PATH_MAP`, which helps when mappings are wrong.
- **401** with a missing or wrong API key.
- **503** when the queue is full.

## `GET /jobs/{id}`

```json
{
  "id": 12,
  "status": "done",
  "outcome": "synced",
  "reason": "speech overlap 0.413 -> 0.621",
  "offset_ms": -10,
  "ratio": 1.0,
  "splits": 1,
  "score_before": 0.413,
  "score_after": 0.621,
  "prominence": 10.1,
  "video": "...", "subtitle": "...", "reference": "auto", "dry_run": false,
  "created_at": 1763337600, "started_at": 1763337600, "finished_at": 1763337602,
  "result": { "...": "full report, including per-segment offsets" }
}
```

`status` is `queued`, `running` or `done`. `outcome` is one of `synced`,
`already_synced`, `low_confidence` or `failed`.

## `GET /jobs?limit=50&status=done`

Recent jobs, newest first: `{"jobs": [...]}`.

## `GET /healthz`

`{"status": "ok", "version": "0.1.0", "queued": 0, "running": 0, "workers": 1}`.
No auth. `subsyncer health` calls it for container healthchecks.
