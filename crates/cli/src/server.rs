//! HTTP daemon: `POST /sync` queues a job, a bounded worker pool runs them.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::cache::{Cache, JobRow};
use crate::config::PathMap;
use crate::engine::{Engine, ReferenceMode, SyncRequest};

pub struct ServerConfig {
    pub listen: String,
    pub workers: usize,
    pub api_key: Option<String>,
    pub path_map: PathMap,
    pub backup: bool,
}

struct QueuedJob {
    id: i64,
    req: SyncRequest,
}

struct App {
    engine: Arc<Engine>,
    cache: Arc<Cache>,
    cfg: ServerConfig,
    queue: Mutex<VecDeque<QueuedJob>>,
    notify: Notify,
    running: AtomicUsize,
}

impl App {
    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<QueuedJob>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Maximum queued jobs before `POST /sync` answers 503.
const MAX_QUEUE: usize = 10_000;

pub fn run(engine: Arc<Engine>, cfg: ServerConfig) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(serve(engine, cfg))
}

pub fn router_for_tests(engine: Arc<Engine>, cfg: ServerConfig) -> (Router, impl FnOnce()) {
    let app = build_app(engine, cfg);
    let a2 = app.clone();
    (router(app), move || spawn_workers(a2))
}

fn build_app(engine: Arc<Engine>, cfg: ServerConfig) -> Arc<App> {
    let cache = engine.cache.clone().expect("server requires a cache");
    Arc::new(App {
        engine,
        cache,
        cfg,
        queue: Mutex::new(VecDeque::new()),
        notify: Notify::new(),
        running: AtomicUsize::new(0),
    })
}

async fn serve(engine: Arc<Engine>, cfg: ServerConfig) -> Result<()> {
    let addr: SocketAddr = cfg.listen.parse().with_context(|| format!("invalid listen address {:?}", cfg.listen))?;
    let app = build_app(engine, cfg);
    // Re-queue jobs left unfinished by a previous run.
    for j in app.cache.unfinished_jobs()? {
        let reference = ReferenceMode::parse(&j.reference).unwrap_or_default();
        let req = SyncRequest {
            video: PathBuf::from(&j.video),
            subtitle: PathBuf::from(&j.subtitle),
            reference,
            dry_run: j.dry_run,
            output: None,
            backup: app.cfg.backup,
            cancel: None,
        };
        app.queue().push_back(QueuedJob { id: j.id, req });
    }
    let requeued = app.queue().len();
    if requeued > 0 {
        info!(jobs = requeued, "re-queued unfinished jobs");
    }
    spawn_workers(app.clone());
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    info!(
        %addr,
        workers = app.cfg.workers,
        auth = app.cfg.api_key.is_some(),
        db = %app.cache.path().display(),
        "subsyncer listening"
    );
    axum::serve(listener, router(app)).with_graceful_shutdown(shutdown_signal()).await?;
    info!("shut down");
    Ok(())
}

fn router(app: Arc<App>) -> Router {
    let protected = Router::new()
        .route("/sync", post(post_sync))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_api_key));
    Router::new().route("/healthz", get(healthz)).merge(protected).with_state(app)
}

fn spawn_workers(app: Arc<App>) {
    for n in 0..app.cfg.workers {
        let app = app.clone();
        tokio::spawn(async move { worker(app, n).await });
    }
}

async fn worker(app: Arc<App>, n: usize) {
    loop {
        let job = loop {
            if let Some(j) = app.queue().pop_front() {
                break j;
            }
            app.notify.notified().await;
        };
        app.running.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = app.cache.mark_job_running(job.id) {
            warn!(job = job.id, "db: {e:#}");
        }
        info!(job = job.id, worker = n, subtitle = %job.req.subtitle.display(), "job started");
        let engine = app.engine.clone();
        let req = job.req.clone();
        let report = tokio::task::spawn_blocking(move || engine.sync(&req)).await;
        app.running.fetch_sub(1, Ordering::SeqCst);
        let (value, outcome) = match report {
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
                (serde_json::to_value(&r).unwrap_or(Value::Null), r.outcome_str())
            }
            Err(e) => {
                error!(job = job.id, "job panicked: {e}");
                (json!({ "outcome": "failed", "reason": format!("internal error: {e}") }), "failed")
            }
        };
        let _ = outcome;
        if let Err(e) = app.cache.finish_job(job.id, "done", &value) {
            warn!(job = job.id, "db: {e:#}");
        }
    }
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
            _ => Some(true),
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
    let reference = match ReferenceMode::parse(body.reference.as_deref().unwrap_or("auto")) {
        Some(r) => r,
        None => return bad_request("`reference` must be auto, audio or embedded"),
    };
    let video_path = app.cfg.path_map.apply(video.trim());
    let sub_path = app.cfg.path_map.apply(subtitle.trim());
    for (what, p, orig) in [("video", &video_path, &video), ("subtitle", &sub_path, &subtitle)] {
        if !p.is_file() {
            let mapped = if p.as_os_str() != orig.trim() { format!(" (mapped from {orig})") } else { String::new() };
            return reject(StatusCode::UNPROCESSABLE_ENTITY, format!("{what} not found: {}{mapped}", p.display()));
        }
    }
    let dry_run = body.dry_run.unwrap_or(false);
    // Dedupe: a job for the same subtitle still waiting in the queue covers
    // this request.
    {
        let q = app.queue();
        if let Some(j) = q.iter().find(|j| j.req.subtitle == sub_path && j.req.dry_run == dry_run) {
            return (StatusCode::ACCEPTED, Json(json!({ "job_id": j.id, "status": "queued", "deduplicated": true })))
                .into_response();
        }
        if q.len() >= MAX_QUEUE {
            return reject(StatusCode::SERVICE_UNAVAILABLE, "queue full");
        }
    }
    let id = match app.cache.insert_job(
        &video_path.to_string_lossy(),
        &sub_path.to_string_lossy(),
        reference.as_str(),
        dry_run,
    ) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("db: {e:#}") }))).into_response();
        }
    };
    let req = SyncRequest {
        video: video_path,
        subtitle: sub_path,
        reference,
        dry_run,
        backup: app.cfg.backup,
        ..Default::default()
    };
    info!(job = id, subtitle = %req.subtitle.display(), "job queued");
    app.queue().push_back(QueuedJob { id, req });
    app.notify.notify_one();
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
    match app.cache.get_job(id) {
        Ok(Some(j)) => Json(job_json(&j)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such job" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("db: {e:#}") }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    status: Option<String>,
}

async fn list_jobs(State(app): State<Arc<App>>, Query(q): Query<ListQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    match app.cache.list_jobs(limit, q.status.as_deref()) {
        Ok(rows) => Json(json!({ "jobs": rows.iter().map(job_json).collect::<Vec<_>>() })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("db: {e:#}") }))).into_response(),
    }
}

async fn healthz(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "queued": app.queue().len(),
        "running": app.running.load(Ordering::SeqCst),
        "workers": app.cfg.workers,
    }))
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
