//! HTTP daemon: `POST /sync` queues a job, a pool of worker threads runs
//! them.
//!
//! Workers are plain threads waiting on the queue's condition variable.
//! Two jobs for the same subtitle never run at once: the later one waits.
//! Each running job has a cancel flag, set by `DELETE /jobs/{id}` or at
//! shutdown, which stops its ffmpeg processes. Jobs interrupted by a
//! shutdown are put back in the queue and run again on the next start.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use subsyncer_core::Outcome;
use tracing::{error, info, warn};

use crate::cache::{Cache, JobRow, now_secs};
use crate::config::PathMap;
use crate::engine::{Engine, ReferenceMode, SyncRequest};
use crate::scan::SUB_EXTS;

pub struct ServerConfig {
    pub listen: String,
    pub workers: usize,
    pub api_key: Option<String>,
    pub path_map: PathMap,
    pub backup: bool,
    /// Only files under these directories are accepted; empty accepts any.
    pub allowed_dirs: Vec<PathBuf>,
    /// Finished jobs are deleted after this long; None keeps them.
    pub job_retention: Option<Duration>,
}

/// Maximum queued jobs before `POST /sync` answers 503.
const MAX_QUEUE: usize = 10_000;
/// A job is given up once the process stopped this many times while
/// running it: each was a crash or kill, most likely caused by the file.
pub const MAX_ATTEMPTS: i64 = 3;
/// How long a shutdown waits for running jobs to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How often old jobs and stale cache entries are cleaned up.
const MAINTENANCE_EVERY: Duration = Duration::from_secs(24 * 3600);

struct QueuedJob {
    id: i64,
    req: SyncRequest,
}

struct Running {
    subtitle: PathBuf,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<QueuedJob>,
    running: HashMap<i64, Running>,
    /// Shutting down: nothing new starts or is accepted.
    stopping: bool,
}

/// Counters since start, for `/metrics`.
#[derive(Default)]
struct Metrics {
    /// Finished jobs by outcome, plus "cancelled".
    finished: Mutex<BTreeMap<&'static str, u64>>,
    job_ms: AtomicU64,
    cache_hits: AtomicU64,
}

impl Metrics {
    fn record(&self, outcome: &'static str, elapsed: Duration, cache_hit: bool) {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner()).entry(outcome).or_default() += 1;
        self.job_ms.fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
        if cache_hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct App {
    engine: Arc<Engine>,
    cache: Arc<Cache>,
    cfg: ServerConfig,
    queue: Mutex<Queue>,
    /// Signalled whenever the queue changes.
    changed: Condvar,
    metrics: Metrics,
}

impl App {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `p` is inside one of the allowed directories (always, with
    /// none configured). Judged on the path itself, with `..` resolved.
    fn allowed(&self, p: &Path) -> bool {
        self.cfg.allowed_dirs.is_empty() || self.cfg.allowed_dirs.iter().any(|d| normalize(p).starts_with(d))
    }
}

/// `p` made absolute, with `.` and `..` resolved without touching the disk.
pub fn normalize(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// The daemon's state, without the listener (so tests can drive it).
pub struct Server {
    app: Arc<App>,
}

impl Server {
    pub fn new(engine: Arc<Engine>, mut cfg: ServerConfig) -> Result<Self> {
        let cache = engine.cache.clone().context("the server needs a data directory")?;
        cfg.allowed_dirs = cfg.allowed_dirs.iter().map(|d| normalize(d)).collect();
        let app =
            App { engine, cache, cfg, queue: Mutex::default(), changed: Condvar::new(), metrics: Metrics::default() };
        Ok(Self { app: Arc::new(app) })
    }

    pub fn router(&self) -> Router {
        router(self.app.clone())
    }

    /// Queue the jobs a previous run left queued or running. A job the
    /// process stopped during `MAX_ATTEMPTS` times is failed instead.
    /// Returns how many were queued.
    pub fn requeue_unfinished(&self) -> Result<usize> {
        let app = &self.app;
        let mut q = app.queue();
        for j in app.cache.unfinished_jobs()? {
            if j.attempts >= MAX_ATTEMPTS {
                warn!(job = j.id, subtitle = %j.subtitle, attempts = j.attempts, "giving up on a job the process stopped during each time");
                let reason = format!(
                    "subsyncer stopped {} times while syncing this file (a crash or kill each time); gave up",
                    j.attempts
                );
                app.cache.finish_job(j.id, "done", &json!({ "outcome": "failed", "reason": reason }))?;
                continue;
            }
            let req = SyncRequest {
                video: PathBuf::from(&j.video),
                subtitle: PathBuf::from(&j.subtitle),
                reference: ReferenceMode::parse(&j.reference).unwrap_or_default(),
                dry_run: j.dry_run,
                backup: app.cfg.backup,
                ..Default::default()
            };
            q.waiting.push_back(QueuedJob { id: j.id, req });
        }
        Ok(q.waiting.len())
    }

    pub fn start_workers(&self) {
        for n in 0..self.app.cfg.workers.max(1) {
            let app = self.app.clone();
            std::thread::Builder::new()
                .name(format!("worker-{n}"))
                .spawn(move || worker(&app, n))
                .expect("spawning a worker thread");
        }
    }

    /// Delete old jobs and stale cache entries now and then.
    pub fn start_maintenance(&self) {
        let app = self.app.clone();
        std::thread::Builder::new()
            .name("maintenance".into())
            .spawn(move || {
                loop {
                    maintain(&app);
                    std::thread::sleep(MAINTENANCE_EVERY);
                }
            })
            .expect("spawning the maintenance thread");
    }

    /// Stop: no new jobs start, running ones are cancelled (their ffmpeg is
    /// killed at once) and put back in the queue for the next start. Waits
    /// up to `grace` for them to stop.
    pub fn shut_down(&self, grace: Duration) {
        let app = &self.app;
        let mut q = app.queue();
        q.stopping = true;
        for r in q.running.values() {
            r.cancel.store(true, Ordering::Relaxed);
        }
        app.changed.notify_all();
        let deadline = Instant::now() + grace;
        while !q.running.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            q = app.changed.wait_timeout(q, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        let busy: Vec<i64> = q.running.keys().copied().collect();
        drop(q);
        for id in busy {
            // Still finishing up; it runs again on the next start.
            if let Err(e) = app.cache.requeue_job(id) {
                warn!(job = id, "db: {e:#}");
            }
        }
    }
}

pub fn run(engine: Arc<Engine>, cfg: ServerConfig) -> Result<()> {
    let addr: SocketAddr = cfg.listen.parse().with_context(|| format!("invalid listen address {:?}", cfg.listen))?;
    let server = Server::new(engine, cfg)?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    // Bind before starting any work, so a taken port fails cleanly.
    let listener = rt.block_on(tokio::net::TcpListener::bind(addr)).with_context(|| format!("binding {addr}"))?;
    let requeued = server.requeue_unfinished()?;
    if requeued > 0 {
        info!(jobs = requeued, "re-queued unfinished jobs");
    }
    server.start_workers();
    server.start_maintenance();
    let app = &server.app;
    info!(
        %addr,
        workers = app.cfg.workers,
        auth = app.cfg.api_key.is_some(),
        db = %app.cache.path().display(),
        "subsyncer listening"
    );
    let served =
        rt.block_on(async { axum::serve(listener, server.router()).with_graceful_shutdown(shutdown_signal()).await });
    server.shut_down(SHUTDOWN_GRACE);
    // Nothing in the runtime needs to finish: handlers are done.
    rt.shutdown_timeout(Duration::from_secs(1));
    info!("shut down");
    Ok(served?)
}

fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/sync", post(post_sync))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job).delete(delete_job))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_api_key));
    Router::new().route("/healthz", get(healthz)).route("/metrics", get(metrics)).merge(protected).with_state(app)
}

fn worker(app: &App, n: usize) {
    while let Some(job) = next_job(app) {
        let id = job.id;
        run_job(app, n, job);
        app.queue().running.remove(&id);
        app.changed.notify_all();
    }
}

/// Wait for a job to run: the first in the queue whose subtitle isn't being
/// synced right now. None once shutting down.
fn next_job(app: &App) -> Option<QueuedJob> {
    let mut q = app.queue();
    loop {
        if q.stopping {
            return None;
        }
        let free = q.waiting.iter().position(|j| !q.running.values().any(|r| r.subtitle == j.req.subtitle));
        if let Some(mut job) = free.and_then(|i| q.waiting.remove(i)) {
            let cancel = Arc::new(AtomicBool::new(false));
            job.req.cancel = Some(cancel.clone());
            q.running.insert(job.id, Running { subtitle: job.req.subtitle.clone(), cancel });
            return Some(job);
        }
        q = app.changed.wait(q).unwrap_or_else(|e| e.into_inner());
    }
}

fn run_job(app: &App, worker: usize, job: QueuedJob) {
    if let Err(e) = app.cache.mark_job_running(job.id) {
        warn!(job = job.id, "db: {e:#}");
    }
    info!(job = job.id, worker, subtitle = %job.req.subtitle.display(), "job started");
    let t = Instant::now();
    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.engine.sync(&job.req)));
    let cache_hit = report.as_ref().is_ok_and(|r| r.cache_hit);
    let (status, value, outcome) = match report {
        Ok(r) if job.req.cancelled() && r.outcome == Some(Outcome::Failed) => {
            if app.queue().stopping {
                if let Err(e) = app.cache.requeue_job(job.id) {
                    warn!(job = job.id, "db: {e:#}");
                }
                info!(job = job.id, "job interrupted by the shutdown; it runs again on the next start");
                return;
            }
            info!(job = job.id, "job cancelled");
            ("cancelled", serde_json::to_value(&r).unwrap_or(Value::Null), "cancelled")
        }
        Ok(r) => {
            info!(
                job = job.id,
                outcome = r.outcome_str(),
                offset_ms = r.offset_ms,
                splits = r.splits,
                elapsed_ms = r.elapsed_ms,
                reason = %r.reason,
                "job finished"
            );
            ("done", serde_json::to_value(&r).unwrap_or(Value::Null), r.outcome_str())
        }
        Err(_) => {
            error!(job = job.id, "job panicked");
            ("done", json!({ "outcome": "failed", "reason": "internal error (a panic); see the log" }), "failed")
        }
    };
    app.metrics.record(outcome, t.elapsed(), cache_hit);
    if let Err(e) = app.cache.finish_job(job.id, status, &value) {
        warn!(job = job.id, "db: {e:#}");
    }
}

fn maintain(app: &App) {
    if let Some(keep) = app.cfg.job_retention {
        match app.cache.prune_jobs(now_secs() - keep.as_secs() as i64) {
            Ok(0) => {}
            Ok(n) => info!(jobs = n, "deleted old jobs"),
            Err(e) => warn!("deleting old jobs: {e:#}"),
        }
    }
    match app.cache.prune_files() {
        Ok(0) => {}
        Ok(n) => info!(entries = n, "dropped stale cache entries"),
        Err(e) => warn!("cleaning the cache: {e:#}"),
    }
}

/// Run blocking work (database, file system) off the async threads.
async fn blocking(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        error!("request handler failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal error" }))).into_response()
    })
}

async fn require_api_key(State(app): State<Arc<App>>, headers: HeaderMap, req: Request, next: Next) -> Response {
    if let Some(key) = &app.cfg.api_key {
        let given = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !constant_time_eq(given.as_bytes(), key.as_bytes()) {
            return reject(StatusCode::UNAUTHORIZED, "missing or invalid X-Api-Key");
        }
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize, Default)]
struct SyncBody {
    video: Option<String>,
    subtitle: Option<String>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default, deserialize_with = "de_boolish")]
    dry_run: Option<bool>,
}

fn de_boolish<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum B {
        Bool(bool),
        Str(String),
        Num(i64),
    }
    Ok(match Option::<B>::deserialize(d)? {
        None => None,
        Some(B::Bool(b)) => Some(b),
        Some(B::Num(n)) => Some(n != 0),
        Some(B::Str(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" | "no" | "off" => Some(false),
            "1" | "true" | "yes" | "on" => Some(true),
            other => return Err(serde::de::Error::custom(format!("dry_run must be true or false, not {other:?}"))),
        },
    })
}

/// Answer a rejected request with `{"error": ...}` and log why. With
/// `curl -f` (as in the Bazarr command) the body isn't shown in Bazarr's log,
/// so subsyncer's log is where users find the reason.
fn reject(status: StatusCode, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    warn!(status = status.as_u16(), "request rejected: {msg}");
    (status, Json(json!({ "error": msg }))).into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    reject(StatusCode::BAD_REQUEST, msg)
}

fn db_error(e: anyhow::Error) -> Response {
    error!("db: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("db: {e:#}") }))).into_response()
}

async fn post_sync(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let parsed: Result<SyncBody, String> = if ctype.starts_with("application/json") {
        serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))
    } else {
        serde_urlencoded::from_bytes(&body).map_err(|e| format!("invalid form body: {e}"))
    };
    let body = match parsed {
        Ok(b) => b,
        Err(e) => return bad_request(e),
    };
    let (Some(video), Some(subtitle)) =
        (body.video.filter(|s| !s.trim().is_empty()), body.subtitle.filter(|s| !s.trim().is_empty()))
    else {
        return bad_request("both `video` and `subtitle` are required");
    };
    let Some(reference) = ReferenceMode::parse(body.reference.as_deref().unwrap_or("auto")) else {
        return bad_request("`reference` must be auto, audio or embedded");
    };
    let dry_run = body.dry_run.unwrap_or(false);
    blocking(move || enqueue(&app, &video, &subtitle, reference, dry_run)).await
}

fn enqueue(app: &App, video: &str, subtitle: &str, reference: ReferenceMode, dry_run: bool) -> Response {
    let video_path = app.cfg.path_map.apply(video.trim());
    let sub_path = app.cfg.path_map.apply(subtitle.trim());
    let ext = sub_path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if !SUB_EXTS.contains(&ext.as_str()) {
        return bad_request(format!("subtitle must be a .{} file: {}", SUB_EXTS.join(", ."), sub_path.display()));
    }
    let files = [("video", &video_path, video), ("subtitle", &sub_path, subtitle)];
    // Before the existence check, so a refused path doesn't reveal whether
    // something is there.
    for (what, p, _) in files {
        if !app.allowed(p) {
            return reject(StatusCode::FORBIDDEN, format!("{what} is outside SUBSYNCER_ALLOWED_DIRS: {}", p.display()));
        }
    }
    for (what, p, orig) in files {
        if !p.is_file() {
            let mapped = if p.as_os_str() != orig.trim() { format!(" (mapped from {orig})") } else { String::new() };
            return reject(StatusCode::UNPROCESSABLE_ENTITY, format!("{what} not found: {}{mapped}", p.display()));
        }
    }
    // Held while the job is stored, so two identical requests can't both
    // miss each other.
    let mut q = app.queue();
    if q.stopping {
        return reject(StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    }
    // A job for the same request still waiting covers this one. A running
    // one doesn't: the subtitle may have been replaced since it started.
    if let Some(j) = q.waiting.iter().find(|j| {
        j.req.subtitle == sub_path
            && j.req.video == video_path
            && j.req.reference == reference
            && j.req.dry_run == dry_run
    }) {
        return (StatusCode::ACCEPTED, Json(json!({ "job_id": j.id, "status": "queued", "deduplicated": true })))
            .into_response();
    }
    if q.waiting.len() >= MAX_QUEUE {
        return reject(StatusCode::SERVICE_UNAVAILABLE, "queue full");
    }
    let id = match app.cache.insert_job(
        &video_path.to_string_lossy(),
        &sub_path.to_string_lossy(),
        reference.as_str(),
        dry_run,
    ) {
        Ok(id) => id,
        Err(e) => return db_error(e),
    };
    info!(job = id, subtitle = %sub_path.display(), "job queued");
    let req = SyncRequest {
        video: video_path,
        subtitle: sub_path,
        reference,
        dry_run,
        backup: app.cfg.backup,
        ..Default::default()
    };
    q.waiting.push_back(QueuedJob { id, req });
    drop(q);
    app.changed.notify_all();
    (StatusCode::ACCEPTED, Json(json!({ "job_id": id, "status": "queued", "deduplicated": false }))).into_response()
}

fn job_json(j: &JobRow) -> Value {
    let r = j.result.as_ref();
    let field = |k: &str| r.and_then(|r| r.get(k)).cloned().unwrap_or(Value::Null);
    json!({
        "id": j.id,
        "status": j.status,
        "video": j.video,
        "subtitle": j.subtitle,
        "reference": j.reference,
        "dry_run": j.dry_run,
        "created_at": j.created_at,
        "started_at": j.started_at,
        "finished_at": j.finished_at,
        "outcome": field("outcome"),
        "reason": field("reason"),
        "offset_ms": field("offset_ms"),
        "ratio": field("ratio"),
        "splits": field("splits"),
        "score_before": field("score_before"),
        "score_after": field("score_after"),
        "prominence": field("prominence"),
        "result": j.result,
    })
}

async fn get_job(State(app): State<Arc<App>>, UrlPath(id): UrlPath<i64>) -> Response {
    blocking(move || match app.cache.get_job(id) {
        Ok(Some(j)) => Json(job_json(&j)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such job" }))).into_response(),
        Err(e) => db_error(e),
    })
    .await
}

/// Cancel a job: a queued one is dropped, a running one stopped (its
/// ffmpeg is killed; nothing is written).
async fn delete_job(State(app): State<Arc<App>>, UrlPath(id): UrlPath<i64>) -> Response {
    blocking(move || {
        let mut q = app.queue();
        if let Some(i) = q.waiting.iter().position(|j| j.id == id) {
            q.waiting.remove(i);
            drop(q);
            app.changed.notify_all();
            let result = json!({ "outcome": null, "reason": "cancelled before it started" });
            if let Err(e) = app.cache.finish_job(id, "cancelled", &result) {
                return db_error(e);
            }
            app.metrics.record("cancelled", Duration::ZERO, false);
            info!(job = id, "job cancelled");
            return Json(json!({ "id": id, "status": "cancelled" })).into_response();
        }
        if let Some(r) = q.running.get(&id) {
            r.cancel.store(true, Ordering::Relaxed);
            return (StatusCode::ACCEPTED, Json(json!({ "id": id, "status": "cancelling" }))).into_response();
        }
        drop(q);
        match app.cache.get_job(id) {
            Ok(Some(j)) => {
                (StatusCode::CONFLICT, Json(json!({ "error": format!("job {id} is already {}", j.status) })))
                    .into_response()
            }
            Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such job" }))).into_response(),
            Err(e) => db_error(e),
        }
    })
    .await
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    status: Option<String>,
}

async fn list_jobs(State(app): State<Arc<App>>, Query(q): Query<ListQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    blocking(move || match app.cache.list_jobs(limit, q.status.as_deref()) {
        Ok(rows) => Json(json!({ "jobs": rows.iter().map(job_json).collect::<Vec<_>>() })).into_response(),
        Err(e) => db_error(e),
    })
    .await
}

async fn healthz(State(app): State<Arc<App>>) -> Json<Value> {
    let (queued, running) = {
        let q = app.queue();
        (q.waiting.len(), q.running.len())
    };
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "queued": queued,
        "running": running,
        "workers": app.cfg.workers,
    }))
}

/// Prometheus text format. Counters start at zero with each process.
async fn metrics(State(app): State<Arc<App>>) -> Response {
    let (queued, running) = {
        let q = app.queue();
        (q.waiting.len(), q.running.len())
    };
    let m = &app.metrics;
    let finished = m.finished.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, v: usize| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
    };
    gauge("subsyncer_jobs_queued", "Jobs waiting to run.", queued);
    gauge("subsyncer_jobs_running", "Jobs running now.", running);
    gauge("subsyncer_workers", "Configured workers.", app.cfg.workers);
    let _ = writeln!(
        out,
        "# HELP subsyncer_jobs_finished_total Jobs finished since start, by outcome.\n\
         # TYPE subsyncer_jobs_finished_total counter"
    );
    for outcome in ["synced", "already_synced", "low_confidence", "failed", "cancelled"] {
        let n = finished.get(outcome).copied().unwrap_or(0);
        let _ = writeln!(out, "subsyncer_jobs_finished_total{{outcome=\"{outcome}\"}} {n}");
    }
    let total: u64 = finished.values().sum();
    let _ = writeln!(
        out,
        "# HELP subsyncer_job_seconds Time spent on finished jobs.\n# TYPE subsyncer_job_seconds summary\n\
         subsyncer_job_seconds_sum {:.3}\nsubsyncer_job_seconds_count {total}\n\
         # HELP subsyncer_speech_cache_hits_total Jobs that reused a cached speech timeline.\n\
         # TYPE subsyncer_speech_cache_hits_total counter\nsubsyncer_speech_cache_hits_total {}",
        m.job_ms.load(Ordering::Relaxed) as f64 / 1000.0,
        m.cache_hits.load(Ordering::Relaxed)
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    info!("shutdown requested");
}

/// Minimal HTTP GET for the container healthcheck (no curl needed).
pub fn health_check(addr: &str) -> Result<()> {
    use std::io::{Read, Write};
    let addr = addr.replace("0.0.0.0", "127.0.0.1");
    let mut s = std::net::TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(s, "GET /healthz HTTP/1.0\r\nHost: {addr}\r\n\r\n")?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    if resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        anyhow::bail!("unhealthy: {}", resp.lines().next().unwrap_or(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_resolves_dot_dot() {
        assert_eq!(normalize(Path::new("/tv/Show/../../etc/passwd")), PathBuf::from("/etc/passwd"));
        assert_eq!(normalize(Path::new("/tv/./Show/a.srt")), PathBuf::from("/tv/Show/a.srt"));
        assert!(normalize(Path::new("rel/a.srt")).is_absolute());
    }

    #[test]
    fn dry_run_values() {
        let parse = |body: &str| serde_urlencoded::from_str::<SyncBody>(body).map(|b| b.dry_run);
        assert_eq!(parse("dry_run=1").unwrap(), Some(true));
        assert_eq!(parse("dry_run=off").unwrap(), Some(false));
        assert_eq!(parse("").unwrap(), None);
        assert!(parse("dry_run=maybe").is_err());
    }
}
