//! SQLite store in DATA_DIR: cached reference timelines, job history and
//! library scan state.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use subsyncer_core::{Interval, Timeline};

pub struct Cache {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// Identity of a media file for cache keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey {
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
}

impl FileKey {
    pub fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Ok(Self { path: canon.to_string_lossy().into_owned(), size: meta.len() as i64, mtime_ns })
    }
}

/// Which jobs to list (see [`Cache::list_jobs`]).
#[derive(Clone, Debug, Default)]
pub struct JobQuery {
    pub limit: usize,
    pub status: Option<String>,
    pub before_id: Option<i64>,
    pub path: Option<String>,
}

/// Schema changes after the original tables, applied in order.
/// `PRAGMA user_version` records how many have been applied. Older versions
/// of subsyncer keep working with an upgraded database: they only add
/// tables and columns.
const MIGRATIONS: &[&str] = &[
    // 1: embedded subtitle tracks (their dialogue cues) and how they fared
    // against the audio.
    "CREATE TABLE embedded_tracks (
        path TEXT NOT NULL,
        size INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL,
        stream INTEGER NOT NULL,
        version TEXT NOT NULL,
        cues TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (path, size, mtime_ns, stream, version)
    );
    CREATE TABLE embedded_checks (
        path TEXT NOT NULL,
        size INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL,
        stream INTEGER NOT NULL,
        version TEXT NOT NULL,
        used INTEGER NOT NULL,
        report TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (path, size, mtime_ns, stream, version)
    );",
    // 2: the settings (and version) a scanned pair was processed with, so
    // low-confidence results are retried after they change.
    "ALTER TABLE scan_state ADD COLUMN settings TEXT NOT NULL DEFAULT '';",
    // 3: how often a job was started without finishing (the process
    // stopped), and an index for deleting old jobs.
    "ALTER TABLE jobs ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
     CREATE INDEX jobs_finished ON jobs(finished_at);",
];

fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let applied: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied.max(0) as usize) {
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
    }
    tx.commit()?;
    Ok(())
}

/// A short, stable fingerprint of some settings, for cache versions.
pub fn fingerprint(value: &impl serde::Serialize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    // FNV-1a: stable across Rust versions, unlike the std hasher.
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}")
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A persisted job row.
#[derive(Clone, Debug, serde::Serialize)]
pub struct JobRow {
    pub id: i64,
    pub video: String,
    pub subtitle: String,
    pub reference: String,
    pub dry_run: bool,
    pub status: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub result: Option<serde_json::Value>,
    /// Times the job was started without finishing.
    pub attempts: i64,
}

#[derive(Clone, Debug)]
pub struct ScanState {
    pub sub_size: i64,
    pub sub_mtime_ns: i64,
    pub video_size: i64,
    pub video_mtime_ns: i64,
    pub outcome: String,
    /// Fingerprint of the version and settings that produced `outcome`.
    pub settings: String,
}

impl Cache {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating data dir {}", dir.display()))?;
        let path = dir.join("subsyncer.db");
        let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS timelines (
                path TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                stream INTEGER NOT NULL,
                version TEXT NOT NULL,
                frames INTEGER NOT NULL,
                data BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (path, size, mtime_ns, stream, version)
            );
            CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                video TEXT NOT NULL,
                subtitle TEXT NOT NULL,
                reference TEXT NOT NULL,
                dry_run INTEGER NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                finished_at INTEGER,
                result TEXT
            );
            CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status);
            CREATE TABLE IF NOT EXISTS scan_state (
                subtitle TEXT PRIMARY KEY,
                sub_size INTEGER NOT NULL,
                sub_mtime_ns INTEGER NOT NULL,
                video TEXT NOT NULL,
                video_size INTEGER NOT NULL,
                video_mtime_ns INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                processed_at INTEGER NOT NULL
            );",
        )?;
        let mut conn = conn;
        migrate(&mut conn).with_context(|| format!("upgrading {}", path.display()))?;
        Ok(Self { conn: Mutex::new(conn), path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get_timeline(&self, key: &FileKey, stream: usize, version: &str) -> Result<Option<Timeline>> {
        let conn = self.conn();
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data FROM timelines WHERE path=?1 AND size=?2 AND mtime_ns=?3 AND stream=?4 AND version=?5",
                params![key.path, key.size, key.mtime_ns, stream as i64, version],
                |r| r.get(0),
            )
            .optional()?;
        match blob {
            Some(b) => Ok(Some(Timeline::decode_rle(&b)?)),
            None => Ok(None),
        }
    }

    pub fn put_timeline(&self, key: &FileKey, stream: usize, version: &str, tl: &Timeline) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        // Drop entries for older versions of the same file.
        tx.execute(
            "DELETE FROM timelines WHERE path=?1 AND stream=?2 AND version=?3",
            params![key.path, stream as i64, version],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO timelines (path,size,mtime_ns,stream,version,frames,data,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![key.path, key.size, key.mtime_ns, stream as i64, version, tl.len() as i64, tl.encode_rle(), now_secs()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The dialogue cues of an embedded subtitle track.
    pub fn get_embedded_track(&self, key: &FileKey, stream: usize, version: &str) -> Result<Option<Vec<Interval>>> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT cues FROM embedded_tracks WHERE path=?1 AND size=?2 AND mtime_ns=?3 AND stream=?4 AND version=?5",
                params![key.path, key.size, key.mtime_ns, stream as i64, version],
                |r| r.get(0),
            )
            .optional()?;
        let Some(json) = json else { return Ok(None) };
        let pairs: Vec<(i64, i64)> = serde_json::from_str(&json).context("cached embedded track")?;
        Ok(Some(pairs.into_iter().map(|(s, e)| Interval::new(s, e)).collect()))
    }

    pub fn put_embedded_track(&self, key: &FileKey, stream: usize, version: &str, cues: &[Interval]) -> Result<()> {
        let pairs: Vec<(i64, i64)> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM embedded_tracks WHERE path=?1 AND stream=?2 AND version=?3",
            params![key.path, stream as i64, version],
        )?;
        tx.execute(
            "INSERT INTO embedded_tracks (path,size,mtime_ns,stream,version,cues,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![key.path, key.size, key.mtime_ns, stream as i64, version, serde_json::to_string(&pairs)?, now_secs()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Whether an embedded track passed its check against the audio, and
    /// the check's report.
    pub fn get_embedded_check(
        &self,
        key: &FileKey,
        stream: usize,
        version: &str,
    ) -> Result<Option<(bool, serde_json::Value)>> {
        let row: Option<(bool, String)> = self
            .conn()
            .query_row(
                "SELECT used, report FROM embedded_checks
                 WHERE path=?1 AND size=?2 AND mtime_ns=?3 AND stream=?4 AND version=?5",
                params![key.path, key.size, key.mtime_ns, stream as i64, version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((used, report)) => Ok(Some((used, serde_json::from_str(&report).context("cached check report")?))),
            None => Ok(None),
        }
    }

    pub fn put_embedded_check(
        &self,
        key: &FileKey,
        stream: usize,
        version: &str,
        used: bool,
        report: &serde_json::Value,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM embedded_checks WHERE path=?1 AND stream=?2 AND version=?3",
            params![key.path, stream as i64, version],
        )?;
        tx.execute(
            "INSERT INTO embedded_checks (path,size,mtime_ns,stream,version,used,report,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![key.path, key.size, key.mtime_ns, stream as i64, version, used, report.to_string(), now_secs()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn insert_job(&self, video: &str, subtitle: &str, reference: &str, dry_run: bool) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO jobs (video, subtitle, reference, dry_run, status, created_at) VALUES (?1,?2,?3,?4,'queued',?5)",
            params![video, subtitle, reference, dry_run, now_secs()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Mark a job running and count the attempt; finishing or re-queueing
    /// it uncounts it again, so `attempts` counts runs the process didn't
    /// survive.
    pub fn mark_job_running(&self, id: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE jobs SET status='running', started_at=?2, attempts=attempts+1 WHERE id=?1",
            params![id, now_secs()],
        )?;
        Ok(())
    }

    /// Put a running job back in the queue (interrupted by a shutdown).
    pub fn requeue_job(&self, id: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE jobs SET status='queued', started_at=NULL, attempts=MAX(attempts-1, 0)
             WHERE id=?1 AND status='running'",
            params![id],
        )?;
        Ok(())
    }

    /// Delete finished jobs that finished before `before` (unix seconds).
    pub fn prune_jobs(&self, before: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM jobs WHERE status NOT IN ('queued','running') AND finished_at < ?1",
            params![before],
        )?)
    }

    /// Drop cache entries for files that are gone, and entries superseded by
    /// a newer one for the same file and stream (an older VAD version, say).
    /// If many files seem gone at once, the media is probably just not
    /// mounted, and nothing is dropped for missing files.
    pub fn prune_files(&self) -> Result<usize> {
        const FILE_TABLES: [(&str, &str); 4] = [
            ("timelines", "path"),
            ("embedded_tracks", "path"),
            ("embedded_checks", "path"),
            ("scan_state", "subtitle"),
        ];
        let paths: Vec<String> = {
            let conn = self.conn();
            let sql = FILE_TABLES.map(|(t, c)| format!("SELECT {c} FROM {t}")).join(" UNION ");
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        // Checked without holding the database: this touches the disk.
        let missing: Vec<&String> = paths.iter().filter(|p| !Path::new(p).exists()).collect();
        let unmounted = missing.len() > 20 && missing.len() * 2 > paths.len();
        if unmounted {
            tracing::warn!(
                missing = missing.len(),
                of = paths.len(),
                "most cached files are missing; is the media mounted?"
            );
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut removed = 0;
        if !unmounted {
            for p in &missing {
                for (t, c) in FILE_TABLES {
                    removed += tx.execute(&format!("DELETE FROM {t} WHERE {c}=?1"), params![p])?;
                }
            }
        }
        for t in ["timelines", "embedded_tracks", "embedded_checks"] {
            removed += tx.execute(
                &format!(
                    "DELETE FROM {t} WHERE rowid IN (SELECT rowid FROM (SELECT rowid, ROW_NUMBER() OVER
                     (PARTITION BY path, stream ORDER BY created_at DESC, rowid DESC) AS n FROM {t}) WHERE n > 1)"
                ),
                [],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    pub fn finish_job(&self, id: i64, status: &str, result: &serde_json::Value) -> Result<()> {
        self.conn().execute(
            "UPDATE jobs SET status=?2, finished_at=?3, result=?4 WHERE id=?1",
            params![id, status, now_secs(), result.to_string()],
        )?;
        Ok(())
    }

    fn row_to_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
        let result: Option<String> = r.get(9)?;
        Ok(JobRow {
            id: r.get(0)?,
            video: r.get(1)?,
            subtitle: r.get(2)?,
            reference: r.get(3)?,
            dry_run: r.get(4)?,
            status: r.get(5)?,
            created_at: r.get(6)?,
            started_at: r.get(7)?,
            finished_at: r.get(8)?,
            result: result.and_then(|s| serde_json::from_str(&s).ok()),
            attempts: r.get(10)?,
        })
    }

    pub fn get_job(&self, id: i64) -> Result<Option<JobRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id,video,subtitle,reference,dry_run,status,created_at,started_at,finished_at,result,attempts FROM jobs WHERE id=?1",
                params![id],
                Self::row_to_job,
            )
            .optional()?)
    }

    /// Jobs newest first, optionally only those with `status`, with an id
    /// below `before_id` (the next page), or whose video or subtitle path
    /// contains `path`.
    pub fn list_jobs(&self, q: &JobQuery) -> Result<Vec<JobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id,video,subtitle,reference,dry_run,status,created_at,started_at,finished_at,result,attempts FROM jobs
             WHERE (?1 IS NULL OR status=?1) AND (?3 IS NULL OR id < ?3)
               AND (?4 IS NULL OR instr(video, ?4) > 0 OR instr(subtitle, ?4) > 0)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![q.status, q.limit as i64, q.before_id, q.path], Self::row_to_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Jobs left queued or running by a previous process.
    pub fn unfinished_jobs(&self) -> Result<Vec<JobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id,video,subtitle,reference,dry_run,status,created_at,started_at,finished_at,result,attempts FROM jobs
             WHERE status IN ('queued','running') ORDER BY id",
        )?;
        let rows = stmt.query_map([], Self::row_to_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_scan_state(&self, subtitle: &str) -> Result<Option<ScanState>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT sub_size, sub_mtime_ns, video_size, video_mtime_ns, outcome, settings
                 FROM scan_state WHERE subtitle=?1",
                params![subtitle],
                |r| {
                    Ok(ScanState {
                        sub_size: r.get(0)?,
                        sub_mtime_ns: r.get(1)?,
                        video_size: r.get(2)?,
                        video_mtime_ns: r.get(3)?,
                        outcome: r.get(4)?,
                        settings: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn put_scan_state(&self, sub: &FileKey, video: &FileKey, outcome: &str, settings: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO scan_state
             (subtitle, sub_size, sub_mtime_ns, video, video_size, video_mtime_ns, outcome, processed_at, settings)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                sub.path,
                sub.size,
                sub.mtime_ns,
                video.path,
                video.size,
                video.mtime_ns,
                outcome,
                now_secs(),
                settings
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_applied_once() {
        let dir = tempfile::tempdir().unwrap();
        drop(Cache::open(dir.path()).unwrap());
        let cache = Cache::open(dir.path()).unwrap();
        let v: i64 = cache.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    #[test]
    fn embedded_tracks_and_checks() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let key = FileKey { path: "/m/a.mkv".into(), size: 10, mtime_ns: 5 };
        let cues = vec![Interval::new(100, 900), Interval::new(1_000, 2_000)];
        assert!(cache.get_embedded_track(&key, 3, "v1").unwrap().is_none());
        cache.put_embedded_track(&key, 3, "v1", &cues).unwrap();
        assert_eq!(cache.get_embedded_track(&key, 3, "v1").unwrap(), Some(cues));
        let newer = FileKey { mtime_ns: 6, ..key.clone() };
        cache.put_embedded_track(&newer, 3, "v1", &[]).unwrap();
        assert!(cache.get_embedded_track(&key, 3, "v1").unwrap().is_none(), "stale entry replaced");
        assert_eq!(cache.get_embedded_track(&newer, 3, "v1").unwrap(), Some(vec![]));

        let report = serde_json::json!({"method": "samples"});
        cache.put_embedded_check(&key, 3, "c1", true, &report).unwrap();
        assert_eq!(cache.get_embedded_check(&key, 3, "c1").unwrap(), Some((true, report)));
        assert!(cache.get_embedded_check(&key, 3, "c2").unwrap().is_none());
        assert_eq!(fingerprint(&("a", 1)), fingerprint(&("a", 1)));
        assert_ne!(fingerprint(&("a", 1)), fingerprint(&("a", 2)));
    }

    #[test]
    fn timeline_and_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let key = FileKey { path: "/m/a.mkv".into(), size: 10, mtime_ns: 5 };
        let tl = Timeline::from_intervals([Interval::new(100, 900)], Some(200));
        assert!(cache.get_timeline(&key, 1, "v1").unwrap().is_none());
        cache.put_timeline(&key, 1, "v1", &tl).unwrap();
        assert_eq!(cache.get_timeline(&key, 1, "v1").unwrap(), Some(tl.clone()));
        assert!(cache.get_timeline(&key, 1, "v2").unwrap().is_none());
        let newer = FileKey { mtime_ns: 6, ..key.clone() };
        cache.put_timeline(&newer, 1, "v1", &tl).unwrap();
        assert!(cache.get_timeline(&key, 1, "v1").unwrap().is_none(), "stale entry replaced");

        let id = cache.insert_job("/v.mkv", "/v.srt", "auto", false).unwrap();
        assert_eq!(cache.unfinished_jobs().unwrap().len(), 1);
        cache.mark_job_running(id).unwrap();
        cache.requeue_job(id).unwrap();
        let j = cache.get_job(id).unwrap().unwrap();
        assert_eq!((j.status.as_str(), j.attempts), ("queued", 0));
        cache.mark_job_running(id).unwrap();
        assert_eq!(cache.get_job(id).unwrap().unwrap().attempts, 1);
        cache.finish_job(id, "synced", &serde_json::json!({"offset_ms": 5})).unwrap();
        let j = cache.get_job(id).unwrap().unwrap();
        assert_eq!(j.status, "synced");
        assert_eq!(j.result.unwrap()["offset_ms"], 5);
        assert!(cache.unfinished_jobs().unwrap().is_empty());
        let q = |status: Option<&str>| JobQuery { limit: 10, status: status.map(String::from), ..Default::default() };
        assert_eq!(cache.list_jobs(&q(None)).unwrap().len(), 1);
        assert_eq!(cache.list_jobs(&q(Some("failed"))).unwrap().len(), 0);
        assert_eq!(cache.prune_jobs(now_secs() - 60).unwrap(), 0, "finished just now");
        assert_eq!(cache.prune_jobs(now_secs() + 60).unwrap(), 1);
        assert!(cache.get_job(id).unwrap().is_none());
    }

    #[test]
    fn pruning_drops_gone_and_superseded_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("data")).unwrap();
        let tl = Timeline::from_intervals([Interval::new(100, 900)], Some(200));
        let here = dir.path().join("here.mkv");
        std::fs::write(&here, b"").unwrap();
        let key = |p: &Path| FileKey { path: p.to_string_lossy().into_owned(), size: 1, mtime_ns: 1 };
        cache.put_timeline(&key(&here), 1, "old", &tl).unwrap();
        // Same second, so the rowid decides which is newer.
        cache.put_timeline(&key(&here), 1, "new", &tl).unwrap();
        cache.put_timeline(&key(&dir.path().join("gone.mkv")), 1, "new", &tl).unwrap();
        assert_eq!(cache.prune_files().unwrap(), 2);
        assert!(cache.get_timeline(&key(&here), 1, "new").unwrap().is_some());
        assert!(cache.get_timeline(&key(&here), 1, "old").unwrap().is_none());
        // Many files missing at once: the media is probably not mounted.
        for k in 0..30 {
            cache.put_timeline(&key(&dir.path().join(format!("unmounted{k}.mkv"))), 1, "new", &tl).unwrap();
        }
        assert_eq!(cache.prune_files().unwrap(), 0);
    }
}
