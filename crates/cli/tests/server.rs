//! The HTTP daemon, driven through its router. Syncs run against fake
//! ffprobe scripts, so no media or ffmpeg is needed.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use subsyncer::cache::Cache;
use subsyncer::config::PathMap;
use subsyncer::engine::{Engine, Settings};
use subsyncer::media::Tools;
use subsyncer::server::{Server, ServerConfig};
use tower::ServiceExt;

const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n";
const FORM: &str = "application/x-www-form-urlencoded";

struct Fixture {
    dir: tempfile::TempDir,
    server: Server,
    router: Router,
    cache: Arc<Cache>,
}

/// A server whose ffprobe is the shell script `probe` (its last argument
/// is the video), adjusted by `cfg`.
fn fixture(probe: &str, cfg: impl FnOnce(&mut ServerConfig, &Path)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("ffprobe");
    std::fs::write(&script, format!("#!/bin/sh\n{probe}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(dir.path().join("media")).unwrap();
    let cache = Arc::new(Cache::open(&dir.path().join("data")).unwrap());
    let tools = Tools { ffprobe: script, ..Tools::default() };
    let engine = Arc::new(Engine::new(Settings { tools, ..Settings::default() }, Some(cache.clone())));
    let mut c = ServerConfig {
        listen: "127.0.0.1:0".into(),
        workers: 4,
        api_key: None,
        path_map: PathMap::default(),
        backup: true,
        allowed_dirs: Vec::new(),
        job_retention: None,
    };
    cfg(&mut c, dir.path());
    let server = Server::new(engine, c).unwrap();
    let router = server.router();
    Fixture { dir, server, router, cache }
}

impl Fixture {
    /// A video and subtitle under `media/`.
    fn media(&self, name: &str) -> (String, String) {
        let video = self.dir.path().join("media").join(format!("{name}.mkv"));
        let sub = self.dir.path().join("media").join(format!("{name}.en.srt"));
        std::fs::write(&video, b"").unwrap();
        std::fs::write(&sub, SRT).unwrap();
        (video.display().to_string(), sub.display().to_string())
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<(&str, String)>,
        key: Option<&str>,
    ) -> (StatusCode, String) {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(k) = key {
            b = b.header("x-api-key", k);
        }
        let req = match body {
            Some((ctype, body)) => b.header("content-type", ctype).body(Body::from(body)),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn json(
        &self,
        method: &str,
        uri: &str,
        body: Option<(&str, String)>,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        let (status, text) = self.call(method, uri, body, key).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// Queue a sync with a form body; returns the job id.
    async fn post(&self, video: &str, sub: &str, extra: &[(&str, &str)]) -> i64 {
        let mut fields = vec![("video", video), ("subtitle", sub)];
        fields.extend_from_slice(extra);
        let (status, v) =
            self.json("POST", "/sync", Some((FORM, serde_urlencoded::to_string(&fields).unwrap())), None).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{v}");
        v["job_id"].as_i64().unwrap()
    }

    /// Poll a job until its status is `want`.
    async fn wait_for(&self, id: i64, want: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (_, v) = self.json("GET", &format!("/jobs/{id}"), None, None).await;
            if v["status"] == want {
                return v;
            }
            assert!(Instant::now() < deadline, "job {id} never became {want}: {v}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_are_checked() {
    let f = fixture("exit 1", |c, dir| {
        c.api_key = Some("k".into());
        c.allowed_dirs = vec![dir.join("media")];
    });
    let (video, sub) = f.media("a");
    let form = |fields: &[(&str, &str)]| Some((FORM, serde_urlencoded::to_string(fields).unwrap()));

    // Health and metrics need no key; everything else does.
    assert_eq!(f.call("GET", "/healthz", None, None).await.0, StatusCode::OK);
    let (status, metrics) = f.call("GET", "/metrics", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(metrics.contains("subsyncer_jobs_queued 0"), "{metrics}");
    let ok = form(&[("video", &video), ("subtitle", &sub)]);
    assert_eq!(f.call("POST", "/sync", ok.clone(), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.call("POST", "/sync", ok.clone(), Some("wrong")).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.call("GET", "/jobs", None, None).await.0, StatusCode::UNAUTHORIZED);

    let k = Some("k");
    let bad = [
        form(&[("video", &video)]),
        form(&[("video", &video), ("subtitle", &sub), ("reference", "bogus")]),
        form(&[("video", &video), ("subtitle", &sub), ("dry_run", "maybe")]),
        Some(("application/json", "{not json".into())),
    ];
    for body in bad {
        assert_eq!(f.call("POST", "/sync", body.clone(), k).await.0, StatusCode::BAD_REQUEST, "{body:?}");
    }
    // Only subtitle files can be targeted.
    let text = f.dir.path().join("media/notes.txt");
    std::fs::write(&text, SRT).unwrap();
    let body = form(&[("video", &video), ("subtitle", &text.display().to_string())]);
    assert_eq!(f.call("POST", "/sync", body, k).await.0, StatusCode::BAD_REQUEST);
    // Outside the allowed directories, also by way of `..`.
    let outside = f.dir.path().join("elsewhere.en.srt");
    std::fs::write(&outside, SRT).unwrap();
    let sneaky = format!("{}/media/../elsewhere.en.srt", f.dir.path().display());
    for p in [outside.display().to_string(), sneaky] {
        let body = form(&[("video", &video), ("subtitle", &p)]);
        assert_eq!(f.call("POST", "/sync", body, k).await.0, StatusCode::FORBIDDEN, "{p}");
    }
    let missing = format!("{}/media/missing.en.srt", f.dir.path().display());
    let (status, v) = f.json("POST", "/sync", form(&[("video", &video), ("subtitle", &missing)]), k).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["error"].as_str().unwrap().contains("subtitle not found"), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn identical_waiting_requests_share_a_job_and_can_be_cancelled() {
    let f = fixture("exit 1", |_, _| {});
    let (video, sub) = f.media("a");
    let a = f.post(&video, &sub, &[]).await;
    // The same request as JSON: covered by the waiting job.
    let body = json!({ "video": video, "subtitle": sub, "dry_run": false }).to_string();
    let (status, v) = f.json("POST", "/sync", Some(("application/json", body)), None).await;
    assert_eq!(
        (status, v["job_id"].as_i64(), v["deduplicated"].as_bool()),
        (StatusCode::ACCEPTED, Some(a), Some(true))
    );
    // A different reference is a different request.
    let b = f.post(&video, &sub, &[("reference", "audio")]).await;
    assert_ne!(a, b);

    // No workers are running, so both are still queued.
    let (status, v) = f.json("DELETE", &format!("/jobs/{a}"), None, None).await;
    assert_eq!((status, v["status"].as_str()), (StatusCode::OK, Some("cancelled")));
    assert_eq!(f.wait_for(a, "cancelled").await["reason"], "cancelled before it started");
    assert_eq!(f.call("DELETE", &format!("/jobs/{a}"), None, None).await.0, StatusCode::CONFLICT);
    assert_eq!(f.call("DELETE", "/jobs/999", None, None).await.0, StatusCode::NOT_FOUND);
    let (_, metrics) = f.call("GET", "/metrics", None, None).await;
    assert!(metrics.contains("subsyncer_jobs_queued 1"), "{metrics}");
    assert!(metrics.contains("subsyncer_jobs_finished_total{outcome=\"cancelled\"} 1"), "{metrics}");
}

#[tokio::test(flavor = "multi_thread")]
async fn workers_run_jobs_in_parallel() {
    // Each run marks itself running, waits, and records how many runs it
    // saw at once.
    let f = fixture(
        r#"d=$(dirname "$0")/runs; mkdir -p "$d"; touch "$d/$$"; sleep 0.5; ls "$d" | wc -l > "$d.$$"; rm "$d/$$"; echo boom >&2; exit 1"#,
        |_, _| {},
    );
    f.server.start_workers();
    let mut ids = Vec::new();
    for k in 0..4 {
        let (video, sub) = f.media(&format!("e{k}"));
        ids.push(f.post(&video, &sub, &[]).await);
    }
    for id in ids {
        let v = f.wait_for(id, "done").await;
        assert_eq!(v["outcome"], "failed");
        assert!(v["reason"].as_str().unwrap().contains("boom"), "{v}");
    }
    let most = std::fs::read_dir(f.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("runs."))
        .map(|e| std::fs::read_to_string(e.path()).unwrap().trim().parse::<usize>().unwrap())
        .max();
    assert!(most >= Some(2), "jobs ran one at a time: {most:?}");
    let (_, metrics) = f.call("GET", "/metrics", None, None).await;
    assert!(metrics.contains("subsyncer_jobs_finished_total{outcome=\"failed\"} 4"), "{metrics}");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_subtitle_is_never_synced_twice_at_once() {
    // Fails loudly if a second run for the same video starts while one is
    // going.
    let f = fixture(
        r#"for v; do :; done; mkdir "$v.lock" 2>/dev/null || touch "$(dirname "$0")/overlap"; sleep 0.3; rmdir "$v.lock"; exit 1"#,
        |_, _| {},
    );
    f.server.start_workers();
    let (video, sub) = f.media("a");
    let ids = [
        f.post(&video, &sub, &[]).await,
        f.post(&video, &sub, &[("reference", "audio")]).await,
        f.post(&video, &sub, &[("reference", "embedded")]).await,
    ];
    for id in ids {
        f.wait_for(id, "done").await;
    }
    assert!(!f.dir.path().join("overlap").exists(), "two syncs of one subtitle overlapped");
}

#[tokio::test(flavor = "multi_thread")]
async fn running_jobs_can_be_cancelled() {
    let f = fixture("exec sleep 30", |_, _| {});
    f.server.start_workers();
    let (video, sub) = f.media("a");
    let id = f.post(&video, &sub, &[]).await;
    f.wait_for(id, "running").await;
    let t = Instant::now();
    let (status, v) = f.json("DELETE", &format!("/jobs/{id}"), None, None).await;
    assert_eq!((status, v["status"].as_str()), (StatusCode::ACCEPTED, Some("cancelling")));
    let v = f.wait_for(id, "cancelled").await;
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(v["reason"], "cancelled");
    assert_eq!(std::fs::read_to_string(&sub).unwrap(), SRT, "nothing written");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_puts_running_jobs_back_in_the_queue() {
    let f = fixture("exec sleep 30", |_, _| {});
    f.server.start_workers();
    let (video, sub) = f.media("a");
    let id = f.post(&video, &sub, &[]).await;
    f.wait_for(id, "running").await;
    let t = Instant::now();
    let server = &f.server;
    tokio::task::block_in_place(|| server.shut_down(Duration::from_secs(10)));
    assert!(t.elapsed() < Duration::from_secs(5), "ffmpeg wasn't stopped: {:?}", t.elapsed());
    let j = f.cache.get_job(id).unwrap().unwrap();
    assert_eq!((j.status.as_str(), j.attempts), ("queued", 0), "an interrupted run doesn't count as an attempt");
    let body = Some((FORM, serde_urlencoded::to_string([("video", &video), ("subtitle", &sub)]).unwrap()));
    assert_eq!(f.call("POST", "/sync", body, None).await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_requeues_unfinished_jobs_and_gives_up_on_crashing_ones() {
    let f = fixture("exit 1", |_, _| {});
    let (video, sub) = f.media("a");
    let (v, s) = (video.as_str(), sub.as_str());
    // Left running by three runs that crashed, by one, and never started.
    let crashing = f.cache.insert_job(v, s, "auto", false).unwrap();
    for _ in 0..3 {
        f.cache.mark_job_running(crashing).unwrap();
    }
    let crashed_once = f.cache.insert_job(v, s, "audio", false).unwrap();
    f.cache.mark_job_running(crashed_once).unwrap();
    let queued = f.cache.insert_job(v, s, "embedded", false).unwrap();

    assert_eq!(f.server.requeue_unfinished().unwrap(), 2);
    let gave_up = f.wait_for(crashing, "done").await;
    assert_eq!(gave_up["outcome"], "failed");
    assert!(gave_up["reason"].as_str().unwrap().contains("stopped 3 times"), "{gave_up}");
    let (_, health) = f.json("GET", "/healthz", None, None).await;
    assert_eq!(health["queued"], 2);
    f.server.start_workers();
    for id in [crashed_once, queued] {
        assert_eq!(f.wait_for(id, "done").await["outcome"], "failed");
    }
}

#[test]
fn allowed_dirs_are_normalized() {
    assert_eq!(
        subsyncer::server::normalize(Path::new("/media/tv/../movies/./a.mkv")),
        PathBuf::from("/media/movies/a.mkv")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_can_be_paged_and_filtered() {
    // No workers are started, so the jobs stay queued.
    let f = fixture("exit 1", |_, _| {});
    let mut ids = Vec::new();
    for name in ["a", "b", "c", "d", "e"] {
        let (video, sub) = f.media(name);
        ids.push(f.post(&video, &sub, &[]).await);
    }
    let page = |v: &Value| v["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_i64().unwrap()).collect::<Vec<_>>();
    let (_, v) = f.json("GET", "/jobs?limit=2", None, None).await;
    assert_eq!(page(&v), vec![ids[4], ids[3]]);
    assert_eq!(v["next_before_id"].as_i64(), Some(ids[3]));
    let (_, v) = f.json("GET", &format!("/jobs?limit=2&before_id={}", ids[3]), None, None).await;
    assert_eq!(page(&v), vec![ids[2], ids[1]]);
    let (_, v) = f.json("GET", &format!("/jobs?limit=2&before_id={}", ids[1]), None, None).await;
    assert_eq!(page(&v), vec![ids[0]]);
    assert!(v["next_before_id"].is_null(), "{v}");
    // A part of the video or subtitle path.
    let (_, v) = f.json("GET", "/jobs?path=c.en.srt", None, None).await;
    assert_eq!(page(&v), vec![ids[2]]);
    let (_, v) = f.json("GET", "/jobs?path=media%2Fd.mkv&status=queued", None, None).await;
    assert_eq!(page(&v), vec![ids[3]]);
}
