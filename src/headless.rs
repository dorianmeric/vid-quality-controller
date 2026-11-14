//! Headless mode (`--headless`): scan, encode every candidate, print a report.
//! Useful for scripting and for verifying the pipeline without a terminal.

use crate::app::is_candidate;
use crate::config::Config;
use crate::encode::{self, EncodeOpts};
use crate::event::Ev;
use crate::fmt;
use crate::log::RunLog;
use crate::probe::MediaInfo;
use crate::report;
use anyhow::{Result, bail};
use crossbeam_channel::unbounded;
use std::io::Write;
use std::time::{Duration, Instant};

pub fn run(cfg: Config, scan_only: bool) -> Result<()> {
    if !cfg.tools_ok() {
        bail!("ffmpeg/ffprobe not found (ffmpeg={}, ffprobe={})", cfg.ffmpeg.display(), cfg.ffprobe.display());
    }

    println!("scanning {} …", cfg.root.display());
    let mut log = RunLog::create(&cfg);
    if let Some(path) = log.path() {
        println!("log: {}", path.display());
    }
    let (tx, rx) = unbounded();
    crate::scan::start(cfg.clone(), tx.clone());

    let mut found: Vec<MediaInfo> = Vec::new();
    let mut scan_errors = 0usize;
    for ev in rx.iter() {
        match ev {
            Ev::ScanProgress { done, total, .. } => {
                if done % 200 == 0 {
                    print!("\r  probed {done}/{total}");
                    let _ = std::io::stdout().flush();
                }
            }
            Ev::ScanFile(info) => found.push(*info),
            Ev::ScanError { .. } => scan_errors += 1,
            Ev::ScanDone { elapsed_secs, .. } => {
                println!("\r  probed {} files in {elapsed_secs:.1}s", found.len());
                break;
            }
            _ => {}
        }
    }

    let candidates: Vec<MediaInfo> = found.iter().filter(|i| is_candidate(&cfg, i)).cloned().collect();
    let total_bytes: u64 = candidates.iter().map(|c| c.size_bytes).sum();
    // Same order as the TUI's default view: biggest files first.
    let mut candidates = candidates;
    candidates.sort_by_key(|c| std::cmp::Reverse(c.size_bytes));
    log.line(&format!("scan done: {} files probed, {} candidates, {}, {} unreadable", found.len(), candidates.len(), fmt::bytes(total_bytes), scan_errors));
    println!("{} files scanned · {} unreadable · {} candidates over {} kbps · {}", found.len(), scan_errors, candidates.len(), cfg.bitrate_threshold_kbps, fmt::bytes(total_bytes));
    if candidates.is_empty() {
        return Ok(());
    }
    if scan_only {
        println!("\n{:<10} {:<10} {:<11} {:<8} file", "size", "bitrate", "res", "length");
        let sorted = &candidates;
        for c in sorted {
            println!(
                "{:<10} {:<10} {:<11} {:<8} {}",
                fmt::bytes_short(c.size_bytes),
                fmt::bitrate(c.bitrate_bps),
                fmt::resolution(c.width, c.height),
                fmt::duration(c.duration_secs),
                c.path.display()
            );
        }
        println!("\n{} candidates · {} total", sorted.len(), fmt::bytes(total_bytes));
        return Ok(());
    }
    if cfg.dry_run {
        println!("DRY RUN — originals will not be touched");
    }
    let video = if cfg.two_pass { format!("{} (VBR)", cfg.video_mode()) } else { cfg.video_mode() };
    println!("encoder: libsvtav1 {video} preset{} {} · AAC {}k {}ch (copied if the encode fails) · {} workers\n", cfg.preset, cfg.pix_fmt, cfg.audio_bitrate_kbps, cfg.audio_channels, cfg.parallelism);

    let opts = EncodeOpts::from_config(&cfg);
    let total = candidates.len();
    log.line(&format!("encode start: {total} files (headless)"));
    let (control, handle) = encode::start(candidates, opts, tx.clone());
    let started = Instant::now();
    let mut results = Vec::new();
    let mut last_tick = Instant::now();

    for ev in rx.iter() {
        match ev {
            Ev::JobStarted { path, .. } => {
                println!("  ▸ {}", path.display());
            }
            Ev::JobProgress { .. } => {}
            Ev::JobFinished(res) => {
                let r = *res;
                let tag = if r.outcome.is_success() { "ok  " } else { "skip" };
                let change =
                    if r.outcome.is_success() { format!("{} -> {} ({:+.1}%)", fmt::bytes(r.old_size), fmt::bytes(r.new_size), -r.saved_pct()) } else { format!("kept {}", fmt::bytes(r.old_size)) };
                println!("  [{tag}] {} · {change} · {:.1}s · {}", r.path.display(), r.total_secs, r.note);
                log.job_result(&r);
                results.push(r);
                let _ = std::io::stdout().flush();
            }
            Ev::EncodeDone { elapsed_secs } => {
                println!("\ndone in {elapsed_secs:.1}s");
                break;
            }
            _ => {}
        }
        if last_tick.elapsed() > Duration::from_secs(5) {
            last_tick = Instant::now();
            println!("  … {}/{} finished ({:.0}s elapsed)", results.len(), total, started.elapsed().as_secs_f64());
        }
    }
    let _ = control;
    let _ = handle.join();

    let before: u64 = results.iter().filter(|r| r.outcome.is_success()).map(|r| r.old_size).sum();
    let after: u64 = results.iter().filter(|r| r.outcome.is_success()).map(|r| r.new_size).sum();
    let path = report::write(&results, &cfg);
    println!(
        "total: {} -> {} · freed {} ({:.1}%) · report {}",
        fmt::bytes(before),
        fmt::bytes(after),
        fmt::bytes(before.saturating_sub(after)),
        if before > 0 { (before - after) as f64 * 100.0 / before as f64 } else { 0.0 },
        path.display()
    );
    Ok(())
}
