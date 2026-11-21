//! Configuration shared by all subcommands. Every option can be set with a
//! `SUBSYNCER_*` environment variable.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Args;
use clap::builder::BoolishValueParser;

use crate::engine::{EmbeddedCheck, Settings};
use crate::media::Tools;

#[derive(Args, Clone, Debug)]
pub struct GlobalOpts {
    /// Directory for the SQLite cache/job database (the Docker image sets
    /// /data). Default: $XDG_CACHE_HOME/subsyncer or ~/.cache/subsyncer.
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

    /// How embedded subtitle tracks are checked against the audio before
    /// being used as the timing reference.
    #[arg(long, env = "SUBSYNCER_EMBEDDED_CHECK", value_enum, default_value_t = EmbeddedCheck::Sampled, global = true)]
    pub embedded_check: EmbeddedCheck,

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

    /// Stop an ffmpeg/ffprobe run that makes no progress for this many
    /// seconds (e.g. a file on a hung network mount).
    #[arg(long, env = "SUBSYNCER_FFMPEG_TIMEOUT_SECS", default_value_t = 120.0, global = true)]
    pub ffmpeg_timeout_secs: f64,
}

impl GlobalOpts {
    pub fn settings(&self) -> Result<Settings> {
        let at_least = |name: &str, v: Option<f64>, min: f64| -> Result<()> {
            match v {
                Some(v) if !(v.is_finite() && v >= min) => bail!("{name} must be a number of at least {min}, not {v}"),
                _ => Ok(()),
            }
        };
        at_least("SUBSYNCER_MAX_OFFSET_SECS", Some(self.max_offset_secs), 0.0)?;
        at_least("SUBSYNCER_WINDOW_SECS", self.window_secs, 0.0)?;
        at_least("SUBSYNCER_SPLIT_PENALTY", self.split_penalty, 0.0)?;
        at_least("SUBSYNCER_MIN_CONFIDENCE", self.min_confidence, 0.0)?;
        at_least("SUBSYNCER_MIN_IMPROVEMENT", self.min_improvement, -1.0)?;
        if self.min_improvement.is_some_and(|m| m > 1.0) {
            bail!("SUBSYNCER_MIN_IMPROVEMENT is a share of speech overlap, at most 1");
        }
        if !(self.ffmpeg_timeout_secs.is_finite() && self.ffmpeg_timeout_secs > 0.0) {
            bail!("SUBSYNCER_FFMPEG_TIMEOUT_SECS must be a positive number of seconds");
        }
        let mut s = Settings {
            tools: Tools {
                ffmpeg: self.ffmpeg.clone(),
                ffprobe: self.ffprobe.clone(),
                timeout: Duration::from_secs_f64(self.ffmpeg_timeout_secs),
                cancel: None,
            },
            embedded_check: self.embedded_check,
            ..Default::default()
        };
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

    /// The data directory: `--data-dir`/`SUBSYNCER_DATA_DIR`, else the
    /// user's cache directory. None with `--no-cache`.
    pub fn resolve_data_dir(&self) -> Option<PathBuf> {
        if self.no_cache {
            return None;
        }
        match &self.data_dir {
            Some(d) if !d.as_os_str().is_empty() => Some(d.clone()),
            _ => default_data_dir(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME")),
        }
    }
}

/// `$XDG_CACHE_HOME/subsyncer`, else `~/.cache/subsyncer`. A relative or
/// empty XDG_CACHE_HOME is ignored, as the XDG spec says.
fn default_data_dir(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let xdg = xdg_cache_home.map(PathBuf::from).filter(|p| p.is_absolute());
    let home = home.filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".cache"));
    Some(xdg.or(home)?.join("subsyncer"))
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

    fn opts(args: &[&str]) -> GlobalOpts {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            global: GlobalOpts,
        }
        <Cli as clap::Parser>::try_parse_from(std::iter::once("subsyncer").chain(args.iter().copied())).unwrap().global
    }

    #[test]
    fn tuning_values_are_checked() {
        assert!(opts(&[]).settings().is_ok());
        assert!(opts(&["--window-secs=30", "--min-improvement=-0.5"]).settings().is_ok());
        for bad in [
            "--window-secs=-1",
            "--window-secs=NaN",
            "--split-penalty=-5",
            "--min-confidence=inf",
            "--min-improvement=2",
            "--max-offset-secs=-3",
            "--ffmpeg-timeout-secs=0",
        ] {
            assert!(opts(&[bad]).settings().is_err(), "{bad}");
        }
    }

    #[test]
    fn data_dir_defaults_to_the_user_cache() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(default_data_dir(os("/xdg"), os("/home/u")), Some(PathBuf::from("/xdg/subsyncer")));
        assert_eq!(default_data_dir(os("rel"), os("/home/u")), Some(PathBuf::from("/home/u/.cache/subsyncer")));
        assert_eq!(default_data_dir(None, os("/home/u")), Some(PathBuf::from("/home/u/.cache/subsyncer")));
        assert_eq!(default_data_dir(None, None), None);
        assert_eq!(opts(&["--data-dir", "/d"]).resolve_data_dir(), Some(PathBuf::from("/d")));
        assert_eq!(opts(&["--data-dir", "/d", "--no-cache"]).resolve_data_dir(), None);
    }

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
