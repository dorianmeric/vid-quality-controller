//! Encoding workers: ffmpeg (libsvtav1) + verification + safe in-place replace.
//!
//! Preset (defaults, all configurable):
//!   -v: libsvtav1 -crf 35 -preset 6 -pix_fmt yuv420p  (resolution & fps untouched)
//!   -a: aac -b:a 160k -ac 2
//!
//! A job is only allowed to replace the original when the produced file
//!   * probes cleanly and still has a video stream,
//!   * has the same duration within tolerance,
//!   * is strictly smaller.

use crate::config::Config;
use crate::event::Ev;
use crate::probe::{self, MediaInfo};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Verified and swapped in.
    Replaced,
    /// Verified, but dry-run so originals were left alone.
    DryRun,
    /// Encoding worked but the result was rejected (not smaller / length mismatch).
    Rejected,
    /// ffmpeg or verification failed.
    Failed,
    /// Aborted because the user cancelled.
    Cancelled,
}

impl Outcome {
    pub fn short(self) -> &'static str {
        match self {
            Outcome::Replaced => "replaced",
            Outcome::DryRun => "dry-run",
            Outcome::Rejected => "rejected",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
        }
    }
    pub fn is_success(self) -> bool {
        matches!(self, Outcome::Replaced | Outcome::DryRun)
    }
}

#[derive(Debug, Clone)]
pub struct JobResult {
    pub path: PathBuf,
    pub final_path: PathBuf,
    pub outcome: Outcome,
    pub old_size: u64,
    pub new_size: u64,
    pub old_bitrate_bps: u64,
    pub new_bitrate_bps: u64,
    pub old_duration: f64,
    pub new_duration: f64,
    pub encode_secs: f64,
    pub total_secs: f64,
    /// Short human summary: shown next to `[failed]` in the UI and in reports.
    pub note: String,
    /// Full diagnostics (command line, ffmpeg stderr, position reached).
    /// Only filled in for failures; written to the run log.
    pub detail: String,
}

/// A job failure, carrying both the one-line reason for the UI and the full
/// diagnostics for the log.
#[derive(Debug)]
pub struct JobError {
    pub message: String,
    pub detail: String,
    /// Wall-clock seconds spent before failing (filled in by `run_job`).
    pub elapsed_secs: f64,
}

impl JobError {
    fn with_detail(message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: detail.into(),
            elapsed_secs: 0.0,
        }
    }
}

impl From<anyhow::Error> for JobError {
    fn from(e: anyhow::Error) -> Self {
        Self {
            message: format!("{e:#}"),
            detail: String::new(),
            elapsed_secs: 0.0,
        }
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

impl JobResult {
    /// Bytes freed. Zero for any outcome where nothing was actually replaced
    /// (rejected / failed / cancelled), since `new_size` is 0 there.
    pub fn saved_bytes(&self) -> i64 {
        if self.new_size == 0 || !self.outcome.is_success() {
            0
        } else {
            self.old_size as i64 - self.new_size as i64
        }
    }
    pub fn saved_pct(&self) -> f64 {
        if self.old_size == 0 || self.saved_bytes() == 0 {
            0.0
        } else {
            self.saved_bytes() as f64 * 100.0 / self.old_size as f64
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodeOpts {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub parallelism: usize,
    pub crf: u8,
    pub preset: u8,
    pub pix_fmt: String,
    pub audio_bitrate_kbps: u32,
    pub audio_channels: u16,
    pub faststart: bool,
    pub tolerance_secs: f64,
    pub keep_backup: bool,
    pub dry_run: bool,
    pub extra_args: Vec<String>,
}

impl EncodeOpts {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            ffmpeg: cfg.ffmpeg.clone(),
            ffprobe: cfg.ffprobe.clone(),
            parallelism: cfg.parallelism.max(1),
            crf: cfg.crf,
            preset: cfg.preset,
            pix_fmt: cfg.pix_fmt.clone(),
            audio_bitrate_kbps: cfg.audio_bitrate_kbps,
            audio_channels: cfg.audio_channels,
            faststart: cfg.faststart,
            tolerance_secs: cfg.duration_tolerance_secs,
            keep_backup: cfg.keep_backup,
            dry_run: cfg.dry_run,
            extra_args: cfg.ffmpeg_extra_args.clone(),
        }
    }
}

/// Shared handle used by the UI to pause / cancel / kill the workers.
pub struct Control {
    paused: AtomicBool,
    cancelled: AtomicBool,
    pids: Mutex<HashMap<u64, u32>>,
}

impl Control {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            pids: Mutex::new(HashMap::new()),
        }
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn toggle_pause(&self) -> bool {
        let now = !self.is_paused();
        self.paused.store(now, Ordering::Relaxed);
        now
    }
    /// Stop starting new jobs and kill anything in flight.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        let pids: Vec<u32> = self.pids.lock().map(|m| m.values().copied().collect()).unwrap_or_default();
        for pid in pids {
            kill_pid(pid);
        }
    }
    fn register(&self, id: u64, pid: u32) {
        if let Ok(mut m) = self.pids.lock() {
            m.insert(id, pid);
        }
        // Race: if cancellation happened between spawn and registration, kill now.
        if self.is_cancelled() {
            kill_pid(pid);
        }
    }
    fn unregister(&self, id: u64) {
        if let Ok(mut m) = self.pids.lock() {
            m.remove(&id);
        }
    }
}

/// Spawn the worker pool. Returns the control handle and the coordinator
/// thread, which joins every worker and then emits `Ev::EncodeDone`.
pub fn start(
    jobs: Vec<MediaInfo>,
    opts: EncodeOpts,
    tx: Sender<Ev>,
) -> (Arc<Control>, thread::JoinHandle<()>) {
    let control = Arc::new(Control::new());
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let opts = Arc::new(opts);
    let workers = opts.parallelism.max(1);

    let control2 = Arc::clone(&control);
    let coordinator = thread::spawn(move || {
        let started = Instant::now();
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let control = Arc::clone(&control2);
            let opts = Arc::clone(&opts);
            let tx = tx.clone();
            handles.push(thread::spawn(move || loop {
                if control.is_cancelled() {
                    break;
                }
                while control.is_paused() && !control.is_cancelled() {
                    thread::sleep(Duration::from_millis(120));
                }
                if control.is_cancelled() {
                    break;
                }
                let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                let Some(job) = next else { break };
                let result = run_job(&job, &opts, &control, &tx);
                match result {
                    Ok(res) => {
                        let _ = tx.send(Ev::JobFinished(Box::new(res)));
                    }
                    Err(e) => {
                        let _ = tx.send(Ev::JobFinished(Box::new(JobResult {
                            path: job.path.clone(),
                            final_path: job.path.clone(),
                            outcome: if control.is_cancelled() {
                                Outcome::Cancelled
                            } else {
                                Outcome::Failed
                            },
                            old_size: job.size_bytes,
                            new_size: 0,
                            old_bitrate_bps: job.bitrate_bps,
                            new_bitrate_bps: 0,
                            old_duration: job.duration_secs,
                            new_duration: 0.0,
                            encode_secs: e.elapsed_secs,
                            total_secs: e.elapsed_secs,
                            note: e.message,
                            detail: e.detail,
                        })));
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let _ = tx.send(Ev::EncodeDone {
            elapsed_secs: started.elapsed().as_secs_f64(),
        });
    });

    (control, coordinator)
}

fn run_job(
    job: &MediaInfo,
    opts: &EncodeOpts,
    control: &Arc<Control>,
    tx: &Sender<Ev>,
) -> Result<JobResult, JobError> {
    let t0 = Instant::now();
    let (final_path, container) = output_target(&job.path);
    let tmp_dir = tmp_dir_for(&job.path);
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("creating temp dir {}", tmp_dir.display()))?;
    let out_name = final_path
        .file_name()
        .ok_or_else(|| anyhow!("no file name"))?
        .to_string_lossy()
        .to_string();
    let out_path = tmp_dir.join(&out_name);

    let _ = tx.send(Ev::JobStarted {
        id: job_id(&job.path),
        path: job.path.clone(),
        duration_secs: job.duration_secs,
        size_bytes: job.size_bytes,
    });

    let result = encode_and_verify(job, opts, control, tx, &out_path, &container, &final_path, t0)
        .map_err(|mut e| {
            if e.elapsed_secs <= 0.0 {
                e.elapsed_secs = t0.elapsed().as_secs_f64();
            }
            e
        });

    // Always clean the temp folder.
    let _ = std::fs::remove_dir_all(&tmp_dir);

    result
}

#[allow(clippy::too_many_arguments)]
fn encode_and_verify(
    job: &MediaInfo,
    opts: &EncodeOpts,
    control: &Arc<Control>,
    tx: &Sender<Ev>,
    out_path: &Path,
    container: &str,
    final_path: &Path,
    t0: Instant,
) -> Result<JobResult, JobError> {
    let job_id = job_id(&job.path);

    let mut cmd = Command::new(&opts.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(["-progress", "pipe:1", "-nostats"])
        .arg("-i")
        .arg(&job.path)
        .args(["-map", "0:v:0", "-map", "0:a?", "-map_metadata", "0"])
        .args(["-c:v", "libsvtav1"])
        .args(["-crf", &opts.crf.to_string()])
        .args(["-preset", &opts.preset.to_string()])
        .args(["-pix_fmt", &opts.pix_fmt])
        .args(["-c:a", "aac"])
        .args(["-b:a", &format!("{}k", opts.audio_bitrate_kbps)])
        .args(["-ac", &opts.audio_channels.to_string()]);
    if container == "mp4" && opts.faststart {
        cmd.args(["-movflags", "+faststart"]);
    }
    for a in &opts.extra_args {
        cmd.arg(a);
    }
    cmd.arg(out_path);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", opts.ffmpeg.display()))?;
    let command_line = command_line(&cmd);
    let pid = child.id();
    control.register(job_id, pid);

    // Drain stderr so ffmpeg can never block on a full pipe.
    let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let stderr_reader = {
        let tail = Arc::clone(&stderr_tail);
        let stderr = child.stderr.take();
        thread::spawn(move || {
            if let Some(err) = stderr {
                let reader = BufReader::new(err);
                for line in reader.lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    // SVT-AV1 is chatty on stderr even at -loglevel error;
                    // drop its banner noise so real errors stay in the tail.
                    if line.contains("Svt[info]") {
                        continue;
                    }
                    if let Ok(mut t) = tail.lock() {
                        t.push_back(line);
                        while t.len() > 40 {
                            t.pop_front();
                        }
                    }
                }
            }
        })
    };

    let mut last_emit = Instant::now();
    let mut last_secs = 0.0f64;
    let mut last_speed = 0.0f64;
    let mut cancelled = false;

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "out_time_us" => {
                    if let Ok(us) = value.trim().parse::<i64>() {
                        last_secs = (us as f64 / 1_000_000.0).max(0.0);
                    }
                }
                "speed" => {
                    last_speed = value.trim().trim_end_matches('x').parse().unwrap_or(0.0);
                }
                "progress" => {
                    if control.is_cancelled() {
                        cancelled = true;
                        control.cancel();
                    }
                    if last_emit.elapsed() >= Duration::from_millis(180) {
                        last_emit = Instant::now();
                        let out_bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
                        let _ = tx.send(Ev::JobProgress {
                            id: job_id,
                            out_secs: last_secs,
                            speed: last_speed,
                            out_bytes,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let status = child.wait().context("waiting for ffmpeg")?;
    let _ = stderr_reader.join();
    control.unregister(job_id);
    let encode_secs = t0.elapsed().as_secs_f64();

    // Final progress tick regardless of throttling.
    let out_bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    let _ = tx.send(Ev::JobProgress {
        id: job_id,
        out_secs: last_secs.max(job.duration_secs),
        speed: last_speed,
        out_bytes,
    });

    if cancelled || control.is_cancelled() {
        let _ = std::fs::remove_file(out_path);
        return Ok(JobResult {
            path: job.path.clone(),
            final_path: final_path.to_path_buf(),
            outcome: Outcome::Cancelled,
            old_size: job.size_bytes,
            new_size: 0,
            old_bitrate_bps: job.bitrate_bps,
            new_bitrate_bps: 0,
            old_duration: job.duration_secs,
            new_duration: 0.0,
            encode_secs,
            total_secs: t0.elapsed().as_secs_f64(),
            note: "cancelled by user".into(),
            detail: String::new(),
        });
    }

    if !status.success() {
        let tail: Vec<String> = stderr_tail
            .lock()
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default();
        let _ = std::fs::remove_file(out_path);
        let reached = if job.duration_secs > 0.0 {
            format!(
                "{:.0}% of {}",
                (last_secs / job.duration_secs * 100.0).clamp(0.0, 100.0),
                crate::fmt::duration(job.duration_secs)
            )
        } else {
            "?".to_string()
        };
        let mut detail = format!(
            "exit: {status}\nsource: {}\nreached: {reached} after {encode_secs:.1}s\ncommand: {command_line}\nstderr:"
            ,
            job.path.display()
        );
        if tail.is_empty() {
            detail.push_str(" (none)\n");
        } else {
            detail.push('\n');
            for line in &tail {
                detail.push_str("  ");
                detail.push_str(line);
                detail.push('\n');
            }
        }
        // Lead with the error itself: the row in the UI only has room for the
        // start of it, and the timings/exit code are already in the log line.
        let note = if tail.is_empty() {
            format!("at {reached} · ffmpeg {status} with no error output (killed?)")
        } else {
            format!("at {reached} · {}", summarize_stderr(&tail))
        };
        return Err(JobError::with_detail(note, detail));
    }

    // ---- verification ----
    let info = probe::probe(&opts.ffprobe, out_path).context("probing the encoded file")?;
    if !info.has_video {
        let _ = std::fs::remove_file(out_path);
        return Ok(rejected(job, final_path, encode_secs, "encoded file has no video stream".into()));
    }
    let tolerance = opts.tolerance_secs.max(job.duration_secs * 0.001);
    let delta = (info.duration_secs - job.duration_secs).abs();
    if job.duration_secs > 0.0 && delta > tolerance {
        let _ = std::fs::remove_file(out_path);
        return Ok(rejected(
            job,
            final_path,
            encode_secs,
            format!(
                "duration mismatch: {:.2}s vs {:.2}s (tolerance {:.2}s)",
                info.duration_secs, job.duration_secs, tolerance
            ),
        ));
    }
    if info.size_bytes >= job.size_bytes {
        let _ = std::fs::remove_file(out_path);
        return Ok(rejected(
            job,
            final_path,
            encode_secs,
            format!(
                "encoded file is not smaller ({} vs {})",
                crate::fmt::bytes(info.size_bytes),
                crate::fmt::bytes(job.size_bytes)
            ),
        ));
    }

    let new_bitrate = info.bitrate_bps;
    let mut note = format!(
        "duration delta {:+.2}s",
        info.duration_secs - job.duration_secs
    );
    if final_path != job.path {
        note.push_str(&format!(" · renamed to .{}", ext_of(final_path)));
    }

    if opts.dry_run {
        let _ = std::fs::remove_file(out_path);
        return Ok(JobResult {
            path: job.path.clone(),
            final_path: final_path.to_path_buf(),
            outcome: Outcome::DryRun,
            old_size: job.size_bytes,
            new_size: info.size_bytes,
            old_bitrate_bps: job.bitrate_bps,
            new_bitrate_bps: new_bitrate,
            old_duration: job.duration_secs,
            new_duration: info.duration_secs,
            encode_secs,
            total_secs: t0.elapsed().as_secs_f64(),
            note,
            detail: String::new(),
        });
    }

    replace_original(&job.path, out_path, final_path, opts.keep_backup)?;

    Ok(JobResult {
        path: job.path.clone(),
        final_path: final_path.to_path_buf(),
        outcome: Outcome::Replaced,
        old_size: job.size_bytes,
        new_size: info.size_bytes,
        old_bitrate_bps: job.bitrate_bps,
        new_bitrate_bps: new_bitrate,
        old_duration: job.duration_secs,
        new_duration: info.duration_secs,
        encode_secs,
        total_secs: t0.elapsed().as_secs_f64(),
        note,
        detail: String::new(),
    })
}

fn rejected(job: &MediaInfo, final_path: &Path, encode_secs: f64, note: String) -> JobResult {
    JobResult {
        path: job.path.clone(),
        final_path: final_path.to_path_buf(),
        outcome: Outcome::Rejected,
        old_size: job.size_bytes,
        new_size: 0,
        old_bitrate_bps: job.bitrate_bps,
        new_bitrate_bps: 0,
        old_duration: job.duration_secs,
        new_duration: 0.0,
        encode_secs,
        total_secs: encode_secs,
        note,
        detail: String::new(),
    }
}

/// Swap the verified file into place, with a rollback path if the rename fails.
fn replace_original(orig: &Path, new: &Path, final_path: &Path, keep_backup: bool) -> Result<()> {
    let backup = if keep_backup {
        let dir = orig.parent().unwrap_or(Path::new(".")).join(".vqc-backup");
        std::fs::create_dir_all(&dir).ok();
        dir.join(orig.file_name().map(|n| n.to_owned()).unwrap_or_default())
    } else {
        sibling(orig, ".vqc-bak")
    };

    std::fs::rename(orig, &backup)
        .with_context(|| format!("moving original aside: {}", orig.display()))?;
    if let Err(e) = std::fs::rename(new, final_path) {
        let _ = std::fs::rename(&backup, orig);
        return Err(anyhow!("installing encoded file: {e}"));
    }
    if !keep_backup {
        let _ = std::fs::remove_file(&backup);
    }
    Ok(())
}

fn output_target(orig: &Path) -> (PathBuf, String) {
    let ext = ext_of(orig);
    match ext.as_str() {
        "mp4" | "m4v" | "mov" => (orig.to_path_buf(), "mp4".to_string()),
        "mkv" | "webm" => (orig.to_path_buf(), "mkv".to_string()),
        _ => (orig.with_extension("mkv"), "mkv".to_string()),
    }
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

fn tmp_dir_for(orig: &Path) -> PathBuf {
    orig.parent().unwrap_or(Path::new(".")).join(format!(
        ".vqc-tmp-{}-{:x}",
        std::process::id(),
        job_id(orig)
    ))
}

pub fn job_id(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Rebuild the ffmpeg command line for the log (quoting args with spaces).
fn command_line(cmd: &Command) -> String {
    let mut out = String::new();
    let mut push = |part: &str| {
        if !out.is_empty() {
            out.push(' ');
        }
        if part.contains(' ') {
            out.push('"');
            out.push_str(part);
            out.push('"');
        } else {
            out.push_str(part);
        }
    };
    push(&cmd.get_program().to_string_lossy());
    for arg in cmd.get_args() {
        push(&arg.to_string_lossy());
    }
    out
}

/// Pick the most useful line out of ffmpeg's stderr tail. ffmpeg usually ends
/// with a generic "Conversion failed!", so prefer the specific error above it.
fn summarize_stderr(tail: &[String]) -> String {
    let specific = tail.iter().rev().find(|line| {
        let l = line.to_ascii_lowercase();
        (l.contains("error")
            || l.contains("unable")
            || l.contains("no space")
            || l.contains("invalid")
            || l.contains("denied")
            || l.contains("cannot"))
            && !l.contains("conversion failed")
    });
    let chosen = specific
        .or_else(|| tail.iter().rev().find(|l| !l.trim().is_empty()))
        .map(|s| s.trim())
        .unwrap_or("no error output from ffmpeg");
    truncate(chosen, 300)
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outcome: Outcome, old: u64, new: u64) -> JobResult {
        JobResult {
            path: PathBuf::from("a.mp4"),
            final_path: PathBuf::from("a.mp4"),
            outcome,
            old_size: old,
            new_size: new,
            old_bitrate_bps: 0,
            new_bitrate_bps: 0,
            old_duration: 10.0,
            new_duration: 10.0,
            encode_secs: 1.0,
            total_secs: 1.0,
            note: String::new(),
            detail: String::new(),
        }
    }

    #[test]
    fn savings_only_count_real_replacements() {
        let ok = result(Outcome::Replaced, 1000, 400);
        assert_eq!(ok.saved_bytes(), 600);
        assert!((ok.saved_pct() - 60.0).abs() < 1e-9);

        // Nothing was installed, so nothing was freed.
        for outcome in [Outcome::Rejected, Outcome::Failed, Outcome::Cancelled] {
            let r = result(outcome, 1000, 0);
            assert_eq!(r.saved_bytes(), 0, "{outcome:?} must not claim savings");
            assert_eq!(r.saved_pct(), 0.0);
        }

        // Dry run reports the projected saving but never touches the file.
        let dry = result(Outcome::DryRun, 1000, 250);
        assert_eq!(dry.saved_bytes(), 750);
    }

    #[test]
    fn container_targets() {
        let (p, c) = output_target(Path::new("x/a.mp4"));
        assert_eq!((p.to_str(), c.as_str()), (Some("x/a.mp4"), "mp4"));
        let (p, c) = output_target(Path::new("x/a.MKV"));
        assert_eq!((p.to_str(), c.as_str()), (Some("x/a.MKV"), "mkv"));
        // Containers that cannot hold AV1 get renamed to mkv.
        let (p, _) = output_target(Path::new("x/a.avi"));
        assert_eq!(p.to_str(), Some("x/a.mkv"));
        let (p, _) = output_target(Path::new("x/a.ts"));
        assert_eq!(p.to_str(), Some("x/a.mkv"));
    }

    #[test]
    fn temp_dirs_are_unique_per_file() {
        let a = tmp_dir_for(Path::new("x/one.mp4"));
        let b = tmp_dir_for(Path::new("x/two.mp4"));
        assert_ne!(a, b, "concurrent jobs in one folder must not share a temp dir");
        assert_eq!(a.parent(), Some(Path::new("x")));
    }

    #[test]
    fn replace_swaps_content_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("vqc-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let orig = dir.join("clip.mp4");
        let staged = dir.join("staged.mp4");
        std::fs::write(&orig, b"ORIGINAL").unwrap();
        std::fs::write(&staged, b"NEW").unwrap();

        replace_original(&orig, &staged, &orig, false).unwrap();
        assert_eq!(std::fs::read(&orig).unwrap(), b"NEW");
        assert!(!staged.exists(), "staged file should have been moved");
        assert!(
            !sibling(&orig, ".vqc-bak").exists(),
            "backup should be removed when keep_backup is off"
        );

        // keep_backup keeps the original content around.
        let staged2 = dir.join("staged2.mp4");
        std::fs::write(&staged2, b"NEWER").unwrap();
        replace_original(&orig, &staged2, &orig, true).unwrap();
        assert_eq!(std::fs::read(&orig).unwrap(), b"NEWER");
        let backup = dir.join(".vqc-backup").join("clip.mp4");
        assert_eq!(std::fs::read(&backup).unwrap(), b"NEW");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod stderr_tests {
    use super::*;

    #[test]
    fn picks_the_specific_error_not_the_generic_tail() {
        let tail = vec![
            "[libx264 @ 0000] using cpu capabilities".to_string(),
            "Error while decoding stream #0:0: Invalid data found when processing input".to_string(),
            "Conversion failed!".to_string(),
        ];
        let s = summarize_stderr(&tail);
        assert!(s.contains("Invalid data found"), "got: {s}");
        assert!(!s.contains("Conversion failed"), "got: {s}");
    }

    #[test]
    fn falls_back_to_last_line_then_to_a_placeholder() {
        let tail = vec!["some warning".to_string(), "moov atom not found".to_string()];
        assert_eq!(summarize_stderr(&tail), "moov atom not found");
        assert!(summarize_stderr(&[]).contains("no error output"));
    }

    #[test]
    fn command_line_quotes_paths_with_spaces() {
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-i").arg(r"E:\Po\my clip.mp4").arg("-y");
        let line = command_line(&cmd);
        assert!(line.starts_with("ffmpeg -i "), "got: {line}");
        assert!(line.contains(r#""E:\Po\my clip.mp4""#), "got: {line}");
    }
}
