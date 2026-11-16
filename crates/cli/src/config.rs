//! Configuration shared by all subcommands. Every option can be set with a
//! `SUBSYNCER_*` environment variable.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Args;
use clap::builder::BoolishValueParser;

use crate::engine::Settings;
use crate::media::Tools;

#[derive(Args, Clone, Debug)]
pub struct GlobalOpts {
    /// Directory for the SQLite cache/job database. Default: /data if
    /// writable, else the user cache directory.
    #[arg(long, env = "SUBSYNCER_DATA_DIR", global = true)]
    pub data_dir: Option<PathBuf>,

    /// Disable the speech-timeline cache.
    #[arg(long, global = true)]
    pub no_cache: bool,

    /// Maximum global offset searched, in seconds (±).
    #[arg(long, env = "SUBSYNCER_MAX_OFFSET_SECS", default_value_t = 60.0, global = true)]
    pub max_offset_secs: f64,

    /// Piecewise search window around the global offset, in seconds (±).
    #[arg(long, env = "SUBSYNCER_WINDOW_SECS", global = true)]
    pub window_secs: Option<f64>,

    /// Cost of introducing a split (frame-score units; higher = fewer splits).
    #[arg(long, env = "SUBSYNCER_SPLIT_PENALTY", global = true)]
    pub split_penalty: Option<f64>,

    /// Disable piecewise alignment (global offset + framerate only).
    #[arg(long, env = "SUBSYNCER_NO_SPLITS", global = true, value_parser = BoolishValueParser::new(), num_args = 0..=1, default_missing_value = "true", default_value = "false")]
    pub no_splits: bool,

    /// Minimum correlation peak prominence required to write.
    #[arg(long, env = "SUBSYNCER_MIN_CONFIDENCE", global = true)]
    pub min_confidence: Option<f64>,

    /// Minimum gain in speech overlap (0-1) required to write.
    #[arg(long, env = "SUBSYNCER_MIN_IMPROVEMENT", global = true)]
    pub min_improvement: Option<f64>,

    /// Keep `<subtitle>.bak` when overwriting.
    #[arg(long, env = "SUBSYNCER_BACKUP", global = true, value_parser = BoolishValueParser::new(), default_value = "true")]
    pub backup: bool,

    /// Rewrite request paths: `/bazarr/path=/local/path,...`.
    #[arg(long, env = "SUBSYNCER_PATH_MAP", global = true)]
    pub path_map: Option<String>,

    /// Log filter (error, warn, info, debug, trace or a tracing filter).
    #[arg(long, env = "SUBSYNCER_LOG_LEVEL", default_value = "info", global = true)]
    pub log_level: String,

    /// Log format.
    #[arg(long, env = "SUBSYNCER_LOG_FORMAT", default_value = "text", global = true, value_parser = ["text", "json"])]
    pub log_format: String,

    #[arg(long, env = "SUBSYNCER_FFMPEG", default_value = "ffmpeg", global = true)]
    pub ffmpeg: PathBuf,

    #[arg(long, env = "SUBSYNCER_FFPROBE", default_value = "ffprobe", global = true)]
    pub ffprobe: PathBuf,
}

impl GlobalOpts {
    pub fn settings(&self) -> Result<Settings> {
        let mut s = Settings {
            tools: Tools { ffmpeg: self.ffmpeg.clone(), ffprobe: self.ffprobe.clone() },
            ..Default::default()
        };
        if !(self.max_offset_secs.is_finite() && self.max_offset_secs >= 0.0) {
            bail!("max offset must be a non-negative number of seconds");
        }
        s.align.max_offset_ms = (self.max_offset_secs * 1000.0).round() as i64;
        if let Some(w) = self.window_secs {
            s.align.window_ms = (w * 1000.0).round() as i64;
        }
        if let Some(p) = self.split_penalty {
            s.align.split_penalty = p;
        }
        s.align.piecewise = !self.no_splits;
        if let Some(c) = self.min_confidence {
            s.gate.min_confidence = c;
        }
        if let Some(m) = self.min_improvement {
            s.gate.min_improvement = m;
        }
        Ok(s)
    }

    pub fn path_map(&self) -> Result<PathMap> {
        PathMap::parse(self.path_map.as_deref().unwrap_or(""))
    }

    /// Resolve the data directory. `required` makes failure an error
    /// (server mode); otherwise the cache is simply disabled.
    pub fn resolve_data_dir(&self) -> Option<PathBuf> {
        if self.no_cache {
            return None;
        }
        if let Some(d) = &self.data_dir {
            return Some(d.clone());
        }
        let data = Path::new("/data");
        if data.is_dir() && is_writable(data) {
            return Some(data.to_path_buf());
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("subsyncer"))
    }
}

fn is_writable(dir: &Path) -> bool {
    tempfile::tempfile_in(dir).is_ok()
}

/// Prefix-based path rewriting for paths sent by Bazarr.
#[derive(Clone, Debug, Default)]
pub struct PathMap {
    rules: Vec<(String, String)>,
}

impl PathMap {
    pub fn parse(spec: &str) -> Result<Self> {
        let mut rules = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((from, to)) = part.split_once('=') else {
                bail!("invalid PATH_MAP entry {part:?}; expected /from=/to");
            };
            let (from, to) = (from.trim().trim_end_matches('/'), to.trim().trim_end_matches('/'));
            if from.is_empty() {
                bail!("invalid PATH_MAP entry {part:?}: empty source prefix");
            }
            rules.push((from.to_string(), to.to_string()));
        }
        // Longest prefix first.
        rules.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
        Ok(Self { rules })
    }

    pub fn apply(&self, path: &str) -> PathBuf {
        for (from, to) in &self.rules {
            if let Some(rest) = path.strip_prefix(from.as_str())
                && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
            {
                return PathBuf::from(format!("{to}{rest}"));
            }
        }
        PathBuf::from(path)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_map() {
        let m = PathMap::parse("/tv=/media/tv, /movies/=/mnt/movies,/tv/anime=/anime").unwrap();
        assert_eq!(m.apply("/tv/Show/S01E01.mkv"), PathBuf::from("/media/tv/Show/S01E01.mkv"));
        assert_eq!(m.apply("/tv/anime/x.mkv"), PathBuf::from("/anime/x.mkv"));
        assert_eq!(m.apply("/movies/A (2000)/a.mkv"), PathBuf::from("/mnt/movies/A (2000)/a.mkv"));
        assert_eq!(m.apply("/tvshows/x.mkv"), PathBuf::from("/tvshows/x.mkv"));
        assert_eq!(m.apply("/other/x"), PathBuf::from("/other/x"));
        assert!(PathMap::parse("nonsense").is_err());
        assert!(PathMap::parse("").unwrap().is_empty());
    }
}
