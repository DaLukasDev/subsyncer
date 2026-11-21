use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use subsyncer::cache::Cache;
use subsyncer::config::GlobalOpts;
use subsyncer::engine::{Engine, ReferenceMode, SyncReport, SyncRequest};
use subsyncer_core::Outcome;
use tracing::warn;

#[derive(Parser)]
#[command(name = "subsyncer", version, about = "Sync subtitle files to video", propagate_version = true)]
struct Cli {
    #[command(flatten)]
    global: GlobalOpts,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sync one subtitle file to a video.
    Sync {
        video: PathBuf,
        subtitle: PathBuf,
        /// Timing reference.
        #[arg(long, value_enum, default_value_t = ReferenceMode::Auto)]
        reference: ReferenceMode,
        /// Analyse only; do not write anything.
        #[arg(long)]
        dry_run: bool,
        /// Write the result here instead of replacing the subtitle.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Do not keep `<subtitle>.bak`.
        #[arg(long)]
        no_backup: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run the HTTP daemon with a job queue.
    Serve {
        /// Listen address.
        #[arg(long, env = "SUBSYNCER_LISTEN", default_value = "0.0.0.0:8080")]
        listen: String,
        /// Concurrent sync workers.
        #[arg(long, env = "SUBSYNCER_WORKERS", default_value_t = 1)]
        workers: usize,
        /// Require this key in the X-Api-Key header.
        #[arg(long, env = "SUBSYNCER_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
    },
    /// Print the detected speech intervals of a media file (debugging).
    #[command(hide = true)]
    Vad {
        media: PathBuf,
        /// Audio stream index (default: default audio stream).
        #[arg(long)]
        stream: Option<usize>,
        /// Downmix all channels instead of using the centre channel.
        #[arg(long)]
        downmix: bool,
        /// Also print the raw per-window probabilities.
        #[arg(long)]
        probs: bool,
        /// Analyse only from here (seconds); printed times stay absolute.
        #[arg(long)]
        start: Option<f64>,
        /// Analyse only this long (seconds).
        #[arg(long)]
        len: Option<f64>,
    },
    /// Check the daemon's /healthz (for container healthchecks).
    Health {
        #[arg(long, env = "SUBSYNCER_LISTEN", default_value = "127.0.0.1:8080")]
        listen: String,
    },
    /// Sync every video+subtitle pair in a library not yet processed.
    Scan {
        dir: PathBuf,
        /// Analyse only; do not write or record anything.
        #[arg(long)]
        dry_run: bool,
        /// Re-process pairs already recorded as processed.
        #[arg(long)]
        force: bool,
        /// Timing reference.
        #[arg(long, value_enum, default_value_t = ReferenceMode::Auto)]
        reference: ReferenceMode,
        /// Also sync forced-subtitle files (`*.forced.*`).
        #[arg(long)]
        include_forced: bool,
        /// Print one JSON report per line.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    subsyncer::logging::init(&cli.global.log_level, &cli.global.log_format);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn open_cache(global: &GlobalOpts, required: bool) -> Result<Option<Arc<Cache>>> {
    let Some(dir) = global.resolve_data_dir() else { return Ok(None) };
    match Cache::open(&dir) {
        Ok(c) => Ok(Some(Arc::new(c))),
        Err(e) if required => Err(e),
        Err(e) => {
            warn!("cache disabled: {e:#}");
            Ok(None)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    if let Cmd::Health { listen } = &cli.cmd {
        subsyncer::server::health_check(listen)?;
        return Ok(ExitCode::SUCCESS);
    }
    let settings = cli.global.settings()?;
    subsyncer::media::check_tools(&settings.tools)?;
    match cli.cmd {
        Cmd::Sync { video, subtitle, reference, dry_run, output, no_backup, json } => {
            let engine = Engine::new(settings, open_cache(&cli.global, false)?);
            let req =
                SyncRequest { video, subtitle, reference, dry_run, output, backup: cli.global.backup && !no_backup };
            let report = engine.sync(&req);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report);
            }
            Ok(exit_code(&report))
        }
        Cmd::Serve { listen, workers, api_key } => {
            let cache = open_cache(&cli.global, true)?
                .ok_or_else(|| anyhow::anyhow!("serve needs a data directory (SUBSYNCER_DATA_DIR)"))?;
            let engine = Arc::new(Engine::new(settings, Some(cache)));
            let cfg = subsyncer::server::ServerConfig {
                listen,
                workers: workers.max(1),
                api_key: api_key.filter(|k| !k.is_empty()),
                path_map: cli.global.path_map()?,
                backup: cli.global.backup,
            };
            subsyncer::server::run(engine, cfg)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Vad { media, stream, downmix, probs, start, len } => {
            let p = subsyncer::media::probe(&settings.tools, &media)?;
            let st = match stream {
                Some(i) => p.streams.iter().find(|s| s.index == i).ok_or_else(|| anyhow::anyhow!("no stream {i}"))?,
                None => p.default_audio().ok_or_else(|| anyhow::anyhow!("no audio stream"))?,
            };
            let filter = if downmix { None } else { subsyncer::media::dialogue_filter(st) };
            let mut det = subsyncer::vad::SpeechDetector::new()?;
            let start_ms = (start.unwrap_or(0.0) * 1000.0).round() as i64;
            let range = (start.is_some() || len.is_some())
                .then(|| (start_ms, len.map_or(i64::MAX / 4, |l| (l * 1000.0).round() as i64)));
            subsyncer::media::decode_audio(&settings.tools, &media, st.index, filter.as_deref(), range, |s| {
                det.push_s16(s)
            })?;
            let (tl, pr) = det.finish()?;
            eprintln!("stream {} filter {:?}: {:.1}% speech", st.index, filter, tl.active_fraction() * 100.0);
            if probs {
                for (i, p) in pr.iter().enumerate() {
                    println!("{} {:.3}", start_ms + i as i64 * 32, p);
                }
            } else {
                for iv in tl.intervals() {
                    println!("{} {}", start_ms + iv.start_ms, start_ms + iv.end_ms);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Health { .. } => unreachable!("handled above"),
        Cmd::Scan { dir, dry_run, force, reference, include_forced, json } => {
            let engine = Engine::new(settings, open_cache(&cli.global, false)?);
            let opts =
                subsyncer::scan::ScanOptions { dry_run, force, reference, include_forced, backup: cli.global.backup };
            let summary = subsyncer::scan::scan(&engine, &dir, &opts, |r| {
                if json {
                    println!("{}", serde_json::to_string(r).unwrap_or_default());
                } else {
                    print_report(r);
                }
            })?;
            eprintln!("{summary}");
            Ok(if summary.failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
        }
    }
}

fn exit_code(r: &SyncReport) -> ExitCode {
    match r.outcome {
        Some(Outcome::Synced | Outcome::AlreadySynced) => ExitCode::SUCCESS,
        Some(Outcome::LowConfidence) => ExitCode::from(2),
        _ => ExitCode::from(1),
    }
}

fn print_report(r: &SyncReport) {
    println!("{}: {}", r.subtitle.display(), r.outcome_str());
    if r.outcome != Some(Outcome::Failed) {
        let ratio = if r.ratio != 1.0 { format!(", framerate x{:.5}", r.ratio) } else { String::new() };
        println!(
            "  reference {} | offset {:+.3}s{} | splits {} | speech overlap {:.3} -> {:.3} | confidence {:.1}",
            r.reference.as_deref().unwrap_or("-"),
            r.offset_ms as f64 / 1000.0,
            ratio,
            r.splits,
            r.score_before,
            r.score_after,
            r.prominence
        );
        if r.splits > 0 {
            for s in &r.segments {
                println!(
                    "    from {:>9.3}s  offset {:+.3}s  ({} cues)",
                    s.from_ms as f64 / 1000.0,
                    s.offset_ms as f64 / 1000.0,
                    s.cues
                );
            }
        }
    }
    if let Some(ec) = &r.embedded_check {
        let how = match ec.method.as_str() {
            "samples" => {
                format!(" (checked with {} audio samples, {:.1}s)", ec.samples.len(), ec.elapsed_ms as f64 / 1000.0)
            }
            "samples+full" => {
                format!(" (checked with samples, then the full audio, {:.1}s)", ec.elapsed_ms as f64 / 1000.0)
            }
            "full" => format!(" (checked with the full audio, {:.1}s)", ec.elapsed_ms as f64 / 1000.0),
            _ => String::new(),
        };
        let verdict = match (ec.used, ec.method.as_str()) {
            (true, "unchecked") => "used unchecked",
            (true, _) => "used",
            (false, _) => "not used",
        };
        println!("  {} {verdict}: {}{how}", ec.track, ec.summary);
    }
    println!("  {}", r.reason);
    if let Some(w) = &r.written {
        println!(
            "  wrote {}{}",
            w.display(),
            r.backup.as_ref().map(|b| format!(" (backup {})", b.display())).unwrap_or_default()
        );
    } else if r.dry_run && r.outcome == Some(Outcome::Synced) {
        println!("  dry run: nothing written");
    }
}
