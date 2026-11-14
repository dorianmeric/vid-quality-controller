//! Full run log, written next to the executable as `log-YYYY-MM-DD-HHMM.txt`.
//!
//! The file is created when the program starts (so the name carries the start
//! time) and is appended to as the run progresses, flushed after every entry so
//! a crash or a kill still leaves a usable log behind.

use crate::config::Config;
use crate::encode::{JobResult, Outcome};
use crate::fmt::{self, LocalTime};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct RunLog {
    path: Option<PathBuf>,
    file: Option<File>,
    started: LocalTime,
    /// Why logging is off, if it is.
    error: Option<String>,
}

impl RunLog {
    /// A log that writes nowhere (used by tests).
    pub fn disabled() -> Self {
        Self { path: None, file: None, started: fmt::local_now(), error: None }
    }

    /// Create the log next to the executable, falling back to the working
    /// directory if that is not writable.
    pub fn create(cfg: &Config) -> Self {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
        if let Some(dir) = exe_dir {
            let log = Self::create_in(&dir, cfg);
            if log.file.is_some() {
                return log;
            }
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut log = Self::create_in(&cwd, cfg);
        if log.file.is_some() {
            log.error = Some(format!("exe folder not writable, log written to {}", cwd.display()));
        }
        log
    }

    pub fn create_in(dir: &Path, cfg: &Config) -> Self {
        let started = fmt::local_now();
        let mut log = Self { path: None, file: None, started, error: None };

        let base = format!("log-{}", started.stamp());
        let mut candidate = dir.join(format!("{base}.txt"));
        let mut n = 2;
        while candidate.exists() {
            candidate = dir.join(format!("{base}-{n}.txt"));
            n += 1;
        }

        match OpenOptions::new().create(true).append(true).open(&candidate) {
            Ok(file) => {
                log.path = Some(candidate);
                log.file = Some(file);
                log.write_header(cfg);
            }
            Err(e) => {
                log.error = Some(format!("could not create log in {}: {e}", dir.display()));
            }
        }
        log
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn started(&self) -> LocalTime {
        self.started
    }

    fn write_header(&mut self, cfg: &Config) {
        let started = self.started.datetime();
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into());
        self.raw(&format!(
            "vid-quality-controller — run log\n\
             started : {started}\n\
             exe     : {exe}\n\
             root    : {}\n\
             preset  : AV1/SVT crf{} preset{} {} · AAC {}k {}ch\n\
             filter  : total bitrate > {} kbps, size >= {} MB, skip_av1={}\n\
             workers : {} encode / {} probe · dry_run={} · keep_backup={}\n\
             ffmpeg  : {}\n\
             ffprobe : {}\n\
             \n",
            cfg.root.display(),
            cfg.crf,
            cfg.preset,
            cfg.pix_fmt,
            cfg.audio_bitrate_kbps,
            cfg.audio_channels,
            cfg.bitrate_threshold_kbps,
            cfg.min_size_mb,
            cfg.skip_av1,
            cfg.parallelism,
            cfg.probe_parallelism,
            cfg.dry_run,
            cfg.keep_backup,
            cfg.ffmpeg.display(),
            cfg.ffprobe.display(),
        ));
    }

    /// Timestamped entry.
    pub fn line(&mut self, msg: &str) {
        let clock = fmt::local_now().clock();
        self.raw(&format!("[{clock}] {msg}\n"));
    }

    /// Entry without a timestamp (continuation lines, headers).
    pub fn raw(&mut self, text: &str) {
        let Some(file) = &mut self.file else { return };
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }

    /// Multi-line block, indented under the previous entry.
    pub fn detail(&mut self, text: &str) {
        for line in text.lines() {
            self.raw(&format!("        {line}\n"));
        }
    }

    /// One entry per finished job, with full diagnostics for failures.
    pub fn job_result(&mut self, r: &JobResult) {
        let tag = match r.outcome {
            Outcome::Replaced => "ok",
            Outcome::DryRun => "dry-run",
            Outcome::Rejected => "rejected",
            Outcome::Failed => "FAILED",
            Outcome::Cancelled => "cancelled",
        };
        let sizes = if r.new_size > 0 { format!("{} -> {} ({:+.1}%)", fmt::bytes(r.old_size), fmt::bytes(r.new_size), -r.saved_pct()) } else { format!("{} (kept)", fmt::bytes(r.old_size)) };
        let bitrates = if r.new_bitrate_bps > 0 { format!("{} -> {}", fmt::bitrate(r.old_bitrate_bps), fmt::bitrate(r.new_bitrate_bps)) } else { fmt::bitrate(r.old_bitrate_bps) };
        let lengths = if r.new_duration > 0.0 { format!("{:.2}s -> {:.2}s", r.old_duration, r.new_duration) } else { format!("{:.2}s", r.old_duration) };
        self.line(&format!("[{tag}] {} | {sizes} | {bitrates} | {lengths} | encode {:.1}s, total {:.1}s | {}", r.path.display(), r.encode_secs, r.total_secs, r.note));
        if !r.detail.is_empty() {
            self.detail(&r.detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vqc-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn log_name_uses_start_time_and_never_clobbers() {
        let dir = temp_dir("name");
        let cfg = Config::default();

        let mut first = RunLog::create_in(&dir, &cfg);
        let path = first.path().expect("log should be created").to_path_buf();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("log-"), "unexpected name: {name}");
        assert!(name.ends_with(".txt"), "unexpected name: {name}");
        // log-YYYY-MM-DD-HHMMSS.txt
        assert_eq!(name.len(), "log-2026-10-12-016530.txt".len(), "unexpected name shape: {name}");
        assert_eq!(path.parent(), Some(dir.as_path()));

        first.line("hello");
        drop(first);

        // A second run in the same minute must not overwrite the first log.
        let second = RunLog::create_in(&dir, &cfg);
        let path2 = second.path().unwrap().to_path_buf();
        assert_ne!(path, path2, "second log must get its own file");
        assert!(path.exists() && path2.exists());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("run log"), "header missing:\n{text}");
        assert!(text.contains("hello"), "entry missing:\n{text}");
        assert!(text.contains("root    :"), "config header missing:\n{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_log_writes_nothing() {
        let mut log = RunLog::disabled();
        assert!(log.path().is_none());
        log.line("ignored");
        log.detail("ignored");
    }
}
