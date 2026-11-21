# HTTP API

`subsyncer serve` (the Docker image's default command) listens on
`SUBSYNCER_LISTEN` (default `0.0.0.0:8080`). Jobs run on a pool of
`SUBSYNCER_WORKERS` workers. Two jobs for the same subtitle never run at the
same time: the later one waits for the earlier one.

At shutdown, running jobs are stopped (their ffmpeg is killed) and, like jobs
still queued, run again on the next start. A job the process stopped during
three times in a row (a crash or an out-of-memory kill each time) is failed
instead, so one bad file can't keep the service restarting. Finished jobs are
deleted after `SUBSYNCER_JOB_RETENTION_DAYS` (30; `0` keeps them).

If `SUBSYNCER_API_KEY` is set, every endpoint except `/healthz` and `/metrics`
requires the `X-Api-Key` header.

## `POST /sync`

Queue a sync. The body is JSON or form-encoded:

| field | required | |
|---|---|---|
| `video` | yes | path to the video, as seen by Bazarr (`SUBSYNCER_PATH_MAP` is applied) |
| `subtitle` | yes | path to the subtitle file: `.srt`, `.ass`, `.ssa` or `.vtt` |
| `reference` | no | `auto` (default), `audio` or `embedded` |
| `dry_run` | no | `true` to analyse without writing (`true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`) |

```sh
curl -X POST http://localhost:8080/sync \
  --data-urlencode "video=/tv/Show/Season 01/S01E01.mkv" \
  --data-urlencode "subtitle=/tv/Show/Season 01/S01E01.en.srt"
```

Responses:

- **202** `{"job_id": 12, "status": "queued", "deduplicated": false}`. If the
  same request (same video, subtitle, reference and dry run) is still waiting
  in the queue, that job is returned with `"deduplicated": true`. A request
  for a subtitle that is being synced right now gets a new job, since the
  file may have changed since that sync started.
- **400** for a malformed body, missing fields or a subtitle that isn't an
  `.srt`, `.ass`, `.ssa` or `.vtt` file.
- **401** with a missing or wrong API key.
- **403** when `SUBSYNCER_ALLOWED_DIRS` is set and a file is outside those
  directories.
- **422** when a file doesn't exist. The error shows the path after
  `SUBSYNCER_PATH_MAP`, which helps when mappings are wrong.
- **503** when the queue is full or the service is shutting down.

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
  "result": { "...": "full report: per-segment offsets, hidden_cues, embedded_check, ..." }
}
```

`status` is `queued`, `running`, `done` or `cancelled`. `outcome` is one of
`synced`, `already_synced`, `low_confidence` or `failed`.

`result.hidden_cues` counts the lines the result hides because the video
doesn't have that part (for example a scene missing from this cut).

When the video has a usable embedded subtitle track, `result.embedded_check`
says how it was checked against the audio:
- `method`: `samples`, `samples+full`, `full` or `unchecked`;
- whether it was `used`;
- a one-line `summary`;
- `cached`: `true` if an earlier sync of the same video checked it;
- the individual sample, full and region checks.

## `DELETE /jobs/{id}`

Cancel a job.

- **200** `{"id": 12, "status": "cancelled"}`: it was still queued and won't
  run.
- **202** `{"id": 12, "status": "cancelling"}`: it is running and is being
  stopped. It ends as `cancelled` without writing anything.
- **409** when the job has already finished, **404** when there is no such job.

## `GET /jobs?limit=50&status=done`

Recent jobs, newest first: `{"jobs": [...]}`.

## `GET /healthz`

`{"status": "ok", "version": "0.2.0", "queued": 0, "running": 0, "workers": 1}`.
No auth. `subsyncer health` calls it for container healthchecks.

## `GET /metrics`

Counters in the Prometheus text format, since the process started. No auth.

```
subsyncer_jobs_queued 0
subsyncer_jobs_running 1
subsyncer_workers 1
subsyncer_jobs_finished_total{outcome="synced"} 42
subsyncer_jobs_finished_total{outcome="already_synced"} 17
subsyncer_jobs_finished_total{outcome="low_confidence"} 3
subsyncer_jobs_finished_total{outcome="failed"} 1
subsyncer_jobs_finished_total{outcome="cancelled"} 0
subsyncer_job_seconds_sum 311.204
subsyncer_job_seconds_count 63
subsyncer_speech_cache_hits_total 28
```
