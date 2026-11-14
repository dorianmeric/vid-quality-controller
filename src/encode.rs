//! Encoding workers: ffmpeg (libsvtav1) + verification + safe in-place replace.
//!
//! Preset (defaults, all configurable):
//!   -v: libsvtav1 -b:v <75% of the threshold> -preset 6 -pix_fmt yuv420p
//!       in two passes (analysis + encode); with `two_pass = false` it is a
//!       single pass at -crf 35 instead, because SVT-AV1 refuses multi-pass
//!       in CRF mode. Resolution & fps are untouched either way.
//!   -a: aac -b:a 160k -ac 2 — re-encoded by default. When an encode fails, the
//!       job is retried once with the audio stream-copied instead: that needs no
//!       audio decoder, so damaged or unsupported audio tracks (which can abort
//!       the whole ffmpeg pipeline) no longer block the shrink.
//!
//! The muxer is always forced with `-f` (see `Container`): ffmpeg would otherwise
//! pick it from the output extension and reject AV1/AAC there (e.g. `.m4v` selects
//! the iPod muxer, which cannot hold AV1).
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
        Self { message: message.into(), detail: detail.into(), elapsed_secs: 0.0 }
    }
}

impl From<anyhow::Error> for JobError {
    fn from(e: anyhow::Error) -> Self {
        Self { message: format!("{e:#}"), detail: String::new(), elapsed_secs: 0.0 }
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
        if self.new_size == 0 || !self.outcome.is_success() { 0 } else { self.old_size as i64 - self.new_size as i64 }
    }
    pub fn saved_pct(&self) -> f64 {
        if self.old_size == 0 || self.saved_bytes() == 0 { 0.0 } else { self.saved_bytes() as f64 * 100.0 / self.old_size as f64 }
    }
}

#[derive(Debug, Clone)]
pub struct EncodeOpts {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub parallelism: usize,
    pub crf: u8,
    /// `-maxrate` ceiling in kbit/s, 0 = none. CRF mode only (see
    /// `encode_command`): SVT-AV1 rejects a max bitrate in VBR mode.
    pub max_bitrate_kbps: u32,
    pub preset: u8,
    pub two_pass: bool,
    pub two_pass_target_kbps: u32,
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
            max_bitrate_kbps: cfg.max_bitrate_kbps,
            preset: cfg.preset,
            two_pass: cfg.two_pass,
            two_pass_target_kbps: cfg.two_pass_target_kbps(),
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
        Self { paused: AtomicBool::new(false), cancelled: AtomicBool::new(false), pids: Mutex::new(HashMap::new()) }
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
pub fn start(jobs: Vec<MediaInfo>, opts: EncodeOpts, tx: Sender<Ev>) -> (Arc<Control>, thread::JoinHandle<()>) {
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
            handles.push(thread::spawn(move || {
                loop {
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
                                outcome: if control.is_cancelled() { Outcome::Cancelled } else { Outcome::Failed },
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
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let _ = tx.send(Ev::EncodeDone { elapsed_secs: started.elapsed().as_secs_f64() });
    });

    (control, coordinator)
}

fn run_job(job: &MediaInfo, opts: &EncodeOpts, control: &Arc<Control>, tx: &Sender<Ev>) -> Result<JobResult, JobError> {
    let t0 = Instant::now();
    let (final_path, container) = output_target(&job.path);
    let tmp_dir = tmp_dir_for(&job.path);
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    std::fs::create_dir_all(&tmp_dir).with_context(|| format!("creating temp dir {}", tmp_dir.display()))?;
    let out_name = final_path.file_name().ok_or_else(|| anyhow!("no file name"))?.to_string_lossy().to_string();
    let out_path = tmp_dir.join(&out_name);

    let result = encode_and_verify(job, opts, control, tx, &out_path, container, &final_path, t0).map_err(|mut e| {
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
    container: Container,
    final_path: &Path,
    t0: Instant,
) -> Result<JobResult, JobError> {
    let job_id = job_id(&job.path);
    let passlog = out_path.parent().unwrap_or(Path::new(".")).join("vqc-pass");

    // Pass 1 analyses the source and writes the stats file, pass 2 encodes with
    // it. Single-pass CRF is just a one-entry layout.
    let layout: &[PassSpec] = if opts.two_pass { &TWO_PASS_LAYOUT } else { &SINGLE_PASS_LAYOUT };
    let base_audio = base_audio_mode(opts);

    // Build the first pass' command before announcing the job, so the UI and the
    // run log can show what is actually running next to the `start:` entry.
    let mut pending_cmd = Some(encode_command(opts, job, layout[0].pass, &passlog, out_path, container, base_audio));
    let first_command = command_line(pending_cmd.as_ref().expect("just built"));
    let _ = tx.send(Ev::JobStarted { id: job_id, path: job.path.clone(), duration_secs: job.duration_secs, size_bytes: job.size_bytes, command: first_command });

    let mut last_speed = 0.0f64;
    let mut cancelled = false;
    let mut failure: Option<PassOutcome> = None;
    // Set when the audio had to be given up on: reported in the result note.
    let mut audio_copied = false;
    let mut retried_with_copy = false;

    for &spec in layout {
        let cmd = match pending_cmd.take() {
            Some(cmd) => cmd,
            None => {
                let cmd = encode_command(opts, job, spec.pass, &passlog, out_path, container, base_audio);
                let note = match spec.pass {
                    Some(n) => format!("pass {n}/{}", layout.len()),
                    None => "next pass".to_string(),
                };
                let _ = tx.send(Ev::JobCommand { id: job_id, note, command: command_line(&cmd) });
                cmd
            }
        };
        let mut outcome = run_pass(cmd, job.duration_secs, control, tx, job_id, out_path, spec)?;

        // Fallback: the audio re-encode is the one part we can cheaply give up.
        // Retrying needs no new analysis pass (pass 1 only ever reads video), so
        // this costs one encode - and it rescues files whose audio track is
        // damaged or whose audio codec has no decoder here.
        if base_audio == AudioMode::Reencode && should_retry_with_copied_audio(spec.pass, outcome.cancelled, outcome.status.success()) {
            retried_with_copy = true;
            let _ = std::fs::remove_file(out_path);
            let retry = encode_command(opts, job, spec.pass, &passlog, out_path, container, AudioMode::Copy);
            let _ = tx.send(Ev::JobCommand { id: job_id, note: "retry with the audio copied".into(), command: command_line(&retry) });
            outcome = run_pass(retry, job.duration_secs, control, tx, job_id, out_path, spec)?;
            audio_copied = outcome.status.success() && !outcome.cancelled;
        }

        last_speed = outcome.speed;
        if outcome.cancelled {
            cancelled = true;
            break;
        }
        if !outcome.status.success() {
            failure = Some(outcome);
            break;
        }
    }

    let encode_secs = t0.elapsed().as_secs_f64();

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

    if let Some(failed) = failure {
        let _ = std::fs::remove_file(out_path);
        let reached = if job.duration_secs > 0.0 {
            format!("{:.0}% of {}", (failed.reached_secs / job.duration_secs * 100.0).clamp(0.0, 100.0), crate::fmt::duration(job.duration_secs))
        } else {
            "?".to_string()
        };
        // Which pass died matters: pass 1 failing usually means an unreadable
        // source, pass 2 failing is a real encoder/muxer problem.
        let label = match failed.pass {
            Some(n) => format!("pass {n}/{}", layout.len()),
            None => "encode".to_string(),
        };
        let status = failed.status;
        let mut detail = format!("exit: {status}\nsource: {}\n{label} failed at {reached} after {encode_secs:.1}s\ncommand: {}\nstderr:", job.path.display(), failed.command_line);
        if failed.stderr_tail.is_empty() {
            detail.push_str(" (none)\n");
        } else {
            detail.push('\n');
            for line in &failed.stderr_tail {
                detail.push_str("  ");
                detail.push_str(line);
                detail.push('\n');
            }
        }
        // Lead with the error itself: the row in the UI only has room for the
        // start of it, and the timings/exit code are already in the log line.
        let damaged = looks_like_damaged_source(&failed.stderr_tail);
        if damaged {
            detail.push_str("\nhint: the source stops decoding here, so the file looks damaged or\n      truncated. Nothing in the settings fixes that: the original was left\n      untouched, redownload it (or trim the readable part and encode that).\n");
        } else if looks_like_audio_mux_rejection(&failed.stderr_tail) {
            detail.push_str(
                "\nhint: the audio is re-encoded to AAC, and this container cannot hold it.\n      Rename the source to .mkv (that muxer takes anything), or change the audio\n      settings / point ffmpeg_extra_args at an audio codec the container accepts.\n",
            );
        }
        if retried_with_copy {
            detail.push_str("\nnote: retried once with the audio stream-copied instead of re-encoded;\n      that attempt failed too.\n");
        }
        let note = if failed.stderr_tail.is_empty() {
            format!("{label} at {reached} · ffmpeg {status} with no error output (killed?)")
        } else if damaged {
            format!("{label} at {reached} · source looks damaged · {}", summarize_stderr(&failed.stderr_tail))
        } else {
            format!("{label} at {reached} · {}", summarize_stderr(&failed.stderr_tail))
        };
        return Err(JobError::with_detail(note, detail));
    }

    // Final progress tick regardless of throttling.
    let _ = tx.send(Ev::JobProgress { id: job_id, out_secs: job.duration_secs, speed: last_speed, out_bytes: std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0) });

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
        return Ok(rejected(job, final_path, encode_secs, format!("duration mismatch: {:.2}s vs {:.2}s (tolerance {:.2}s)", info.duration_secs, job.duration_secs, tolerance)));
    }
    if info.size_bytes >= job.size_bytes {
        let _ = std::fs::remove_file(out_path);
        return Ok(rejected(job, final_path, encode_secs, format!("encoded file is not smaller ({} vs {})", crate::fmt::bytes(info.size_bytes), crate::fmt::bytes(job.size_bytes))));
    }

    let new_bitrate = info.bitrate_bps;
    let mut note = format!("duration delta {:+.2}s", info.duration_secs - job.duration_secs);
    if audio_copied {
        note.push_str(" · audio copied (re-encode failed)");
    }
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

/// What to do with the audio streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AudioMode {
    /// Normal path: AAC at `audio_bitrate_kbps` / `audio_channels`.
    Reencode,
    /// Fallback after a failed encode: mux the source audio as-is.
    Copy,
}

/// Where one pass sits on the job's progress bar.
#[derive(Clone, Copy)]
struct PassSpec {
    /// `None` = single-pass CRF, `Some(1|2)` = two-pass VBR.
    pass: Option<u8>,
    /// Fraction of the job's bar already filled before this pass starts.
    done: f64,
    /// Share of the job's bar this pass fills.
    share: f64,
}

const SINGLE_PASS_LAYOUT: [PassSpec; 1] = [PassSpec { pass: None, done: 0.0, share: 1.0 }];

/// Pass 1 only analyses the source (no audio, no real output), so it is far
/// cheaper than the encode itself: a small slice of the bar keeps the per-file
/// ETA honest.
const PASS1_SHARE: f64 = 0.3;
const TWO_PASS_LAYOUT: [PassSpec; 2] = [PassSpec { pass: Some(1), done: 0.0, share: PASS1_SHARE }, PassSpec { pass: Some(2), done: PASS1_SHARE, share: 1.0 - PASS1_SHARE }];

/// Build one ffmpeg invocation. `pass` is `None` for single-pass CRF, `Some(1)`
/// for the analysis pass (which only writes the stats file, its output is
/// discarded) and `Some(2)` for the VBR encode that consumes those stats.
///
/// `audio` decides what happens to the audio streams: re-encoded to AAC by
/// default, or passed through untouched on the fallback retry.
fn encode_command(opts: &EncodeOpts, job: &MediaInfo, pass: Option<u8>, passlog: &Path, out_path: &Path, container: Container, audio: AudioMode) -> Command {
    let analysis_only = pass == Some(1);
    let mut cmd = Command::new(&opts.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]).args(["-progress", "pipe:1", "-nostats"]).arg("-i").arg(&job.path).args(["-map", "0:v:0"]);
    if !analysis_only {
        cmd.args(["-map", "0:a?", "-map_metadata", "0"]);
    }
    cmd.args(["-c:v", "libsvtav1"]);
    // SVT-AV1 refuses multi-pass in CRF mode ("CRF does not support
    // multi-pass"), so two-pass always targets VBR.
    match pass {
        Some(_) => {
            cmd.args(["-b:v", &format!("{}k", opts.two_pass_target_kbps)]);
        }
        None => {
            cmd.args(["-crf", &opts.crf.to_string()]);
            // Peak ceiling for CRF mode. It cannot be combined with the VBR
            // two-pass mode above: SVT-AV1 rejects it with "Max Bitrate only
            // supported with CRF mode".
            if opts.max_bitrate_kbps > 0 {
                let max = opts.max_bitrate_kbps;
                cmd.args(["-maxrate", &format!("{max}k"), "-bufsize", &format!("{}k", max.saturating_mul(2))]);
            }
        }
    }
    cmd.args(["-preset", &opts.preset.to_string()]).args(["-pix_fmt", &opts.pix_fmt]);
    if let Some(n) = pass {
        // `:v` matters: `-pass` is a per-stream codec option, so a bare `-pass 2`
        // would also apply to every audio stream: its encoder would look for a
        // stats file of its own and fail with "Error reading log file …".
        cmd.args(["-pass:v", &n.to_string()]).arg("-passlogfile").arg(passlog);
    }
    if !analysis_only {
        match audio {
            AudioMode::Reencode => {
                cmd.args(["-c:a", "aac"]).args(["-b:a", &format!("{}k", opts.audio_bitrate_kbps)]).args(["-ac", &opts.audio_channels.to_string()]);
            }
            // Fallback only: mux the source audio untouched. No audio decoder is
            // involved, so a damaged or unsupported track cannot abort the job,
            // and its quality is preserved exactly as it was.
            AudioMode::Copy => {
                cmd.args(["-c:a", "copy"]);
            }
        }
        if container == Container::Mp4 && opts.faststart {
            cmd.args(["-movflags", "+faststart"]);
        }
    }
    for a in &opts.extra_args {
        cmd.arg(a);
    }
    // Force the muxer: never let the output extension pick the format, since some
    // extensions map to muxers that reject AV1 (ipod/m4v, mov) or AAC (webm).
    // This goes after the extra args so it cannot be overridden by accident.
    if analysis_only {
        cmd.args(["-f", "null"]).arg(null_sink());
    } else {
        cmd.args(["-f", container.muxer()]).arg(out_path);
    }
    cmd
}

/// Discard target for the analysis pass (`null` muxer, nothing is written).
fn null_sink() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Map a position inside one pass onto the job's overall progress bar.
fn progress_secs(duration_secs: f64, spec: &PassSpec, in_pass_secs: f64) -> f64 {
    if duration_secs <= 0.0 {
        return in_pass_secs;
    }
    let frac = (in_pass_secs / duration_secs).clamp(0.0, 1.0);
    (spec.done + spec.share * frac) * duration_secs
}

/// What one ffmpeg invocation reported back.
struct PassOutcome {
    pass: Option<u8>,
    status: std::process::ExitStatus,
    command_line: String,
    stderr_tail: Vec<String>,
    /// Media position reached inside this pass.
    reached_secs: f64,
    speed: f64,
    cancelled: bool,
}

/// Which audio mode a job starts in. `audio_bitrate_kbps: 0` means "never
/// re-encode": the audio is copied as-is from the start, and there is nothing
/// left for the failure fallback to rescue.
fn base_audio_mode(opts: &EncodeOpts) -> AudioMode {
    if opts.audio_bitrate_kbps == 0 { AudioMode::Copy } else { AudioMode::Reencode }
}

/// The audio fallback only ever applies to the encode pass: pass 1 reads the
/// video stream alone, so audio cannot be the reason it failed. Success and
/// cancellation are not failures to recover from either.
fn should_retry_with_copied_audio(pass: Option<u8>, cancelled: bool, success: bool) -> bool {
    pass != Some(1) && !cancelled && !success
}

/// Spawn one pass, stream its `-progress` output, drain stderr and wait for it.
fn run_pass(mut cmd: Command, duration_secs: f64, control: &Arc<Control>, tx: &Sender<Ev>, id: u64, out_path: &Path, spec: PassSpec) -> Result<PassOutcome> {
    let program = cmd.get_program().to_string_lossy().to_string();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().with_context(|| format!("spawning {program}"))?;
    let command_line = command_line(&cmd);
    control.register(id, child.id());

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

    let emit = |in_pass_secs: f64, speed: f64| {
        let out_bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
        let _ = tx.send(Ev::JobProgress { id, out_secs: progress_secs(duration_secs, &spec, in_pass_secs), speed, out_bytes });
    };

    let mut last_emit = Instant::now();
    let mut reached = 0.0f64;
    let mut speed = 0.0f64;
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
                        reached = (us as f64 / 1_000_000.0).max(0.0);
                    }
                }
                "speed" => {
                    speed = value.trim().trim_end_matches('x').parse().unwrap_or(0.0);
                }
                "progress" => {
                    if control.is_cancelled() {
                        cancelled = true;
                        control.cancel();
                    }
                    if last_emit.elapsed() >= Duration::from_millis(180) {
                        last_emit = Instant::now();
                        emit(reached, speed);
                    }
                }
                _ => {}
            }
        }
    }

    let status = child.wait().context("waiting for ffmpeg")?;
    let _ = stderr_reader.join();
    control.unregister(id);
    // End the pass at its slice's end, even if ffmpeg printed no progress.
    emit(if status.success() { duration_secs } else { reached }, speed);

    let stderr_tail = stderr_tail.lock().map(|t| t.iter().cloned().collect()).unwrap_or_default();
    Ok(PassOutcome { pass: spec.pass, status, command_line, stderr_tail, reached_secs: reached, speed, cancelled })
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

    std::fs::rename(orig, &backup).with_context(|| format!("moving original aside: {}", orig.display()))?;
    if let Err(e) = std::fs::rename(new, final_path) {
        let _ = std::fs::rename(&backup, orig);
        return Err(anyhow!("installing encoded file: {e}"));
    }
    if !keep_backup {
        let _ = std::fs::remove_file(&backup);
    }
    Ok(())
}

/// Output container, used both to pick the muxer and for the `.mkv` renaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Matroska,
}

impl Container {
    /// ffmpeg muxer name, always passed as `-f <muxer>`.
    fn muxer(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Matroska => "matroska",
        }
    }
}

/// Where to write the result and which muxer to use for it.
fn output_target(orig: &Path) -> (PathBuf, Container) {
    match ext_of(orig).as_str() {
        // ISO-BMFF family: keep the name, but always mux as mp4 (`m4v` would
        // otherwise select the AV1-less iPod muxer, `mov` the AV1-less mov one).
        "mp4" | "m4v" | "mov" => (orig.to_path_buf(), Container::Mp4),
        "mkv" => (orig.to_path_buf(), Container::Matroska),
        // webm's muxer refuses AAC, every other container cannot hold AV1 at all.
        _ => (orig.with_extension("mkv"), Container::Matroska),
    }
}

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

fn tmp_dir_for(orig: &Path) -> PathBuf {
    orig.parent().unwrap_or(Path::new(".")).join(format!(".vqc-tmp-{}-{:x}", std::process::id(), job_id(orig)))
}

pub fn job_id(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "…" }
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
        (l.contains("error") || l.contains("unable") || l.contains("no space") || l.contains("invalid") || l.contains("denied") || l.contains("cannot")) && !l.contains("conversion failed")
    });
    let chosen = specific.or_else(|| tail.iter().rev().find(|l| !l.trim().is_empty())).map(|s| s.as_str()).unwrap_or("no error output from ffmpeg");
    truncate(&strip_log_prefixes(chosen), 300)
}

/// Drop the `[demuxer @ 0000021e421007c0] ` context tags ffmpeg puts in front of
/// its log lines. They carry no information for the user, and the addresses make
/// the same error look different from one run to the next.
fn strip_log_prefixes(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close_rel) = rest[open..].find(']') else { break };
        let close = open + close_rel;
        let tag = &rest[open..=close];
        // Only drop ffmpeg context tags (`[x @ 0000...]`); keep real text that
        // happens to be bracketed, e.g. a message quoting `[0:0]`.
        if tag.contains(" @ ") {
            out.push_str(&rest[..open]);
        } else {
            out.push_str(&rest[..=close]);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does this stderr tail mean the *source* is damaged (truncated download,
/// bit rot) rather than the encode setup being wrong? Those failures deserve a
/// different sentence in the UI: nothing about the settings will fix them.
fn looks_like_damaged_source(tail: &[String]) -> bool {
    const MARKERS: [&str; 9] = [
        "invalid nal unit size",
        "error splitting the input into nal units",
        "decoding error",
        "error while decoding stream",
        "error submitting packet to decoder",
        "invalid data found when processing input",
        "number of bands",
        "reserved bit set",
        "moov atom not found",
    ];
    tail.iter().any(|line| {
        let l = line.to_ascii_lowercase();
        MARKERS.iter().any(|m| l.contains(m))
    })
}

/// Does this stderr tail say the target container cannot hold the copied audio
/// codec? That is the one new failure mode of copying instead of re-encoding.
fn looks_like_audio_mux_rejection(tail: &[String]) -> bool {
    const MARKERS: [&str; 3] = ["could not find tag for codec", "codec not currently supported in container", "not currently supported in container"];
    tail.iter().any(|line| {
        let l = line.to_ascii_lowercase();
        MARKERS.iter().any(|m| l.contains(m))
    })
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(two_pass: bool) -> EncodeOpts {
        let cfg = Config { two_pass, bitrate_threshold_kbps: 2000, ..Config::default() };
        EncodeOpts::from_config(&cfg)
    }

    fn media() -> MediaInfo {
        MediaInfo {
            path: PathBuf::from(r"E:\Po\clip.mp4"),
            size_bytes: 900 * 1024 * 1024,
            duration_secs: 60.0,
            bitrate_bps: 30_000_000,
            video_bitrate_bps: Some(29_000_000),
            width: 1920,
            height: 1080,
            fps: 29.97,
            vcodec: "h264".into(),
            acodec: "aac".into(),
            audio_channels: 2,
            audio_streams: 1,
            has_video: true,
        }
    }

    fn arg_after(args: &[String], flag: &str) -> Option<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
    }

    fn args_of(opts: &EncodeOpts, pass: Option<u8>) -> Vec<String> {
        args_with_audio(opts, pass, AudioMode::Reencode)
    }

    fn args_with_audio(opts: &EncodeOpts, pass: Option<u8>, audio: AudioMode) -> Vec<String> {
        let cmd = encode_command(opts, &media(), pass, Path::new("tmp/vqc-pass"), Path::new("tmp/out.mp4"), Container::Mp4, audio);
        cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect()
    }

    #[test]
    fn two_pass_analysis_targets_vbr_and_writes_nothing() {
        let o = opts(true);
        let args = args_of(&o, Some(1));
        // SVT-AV1 refuses multi-pass in CRF mode, so pass 1 must be VBR too.
        assert_eq!(arg_after(&args, "-b:v").as_deref(), Some("1500k"), "75% of the 2000 kbps threshold");
        assert!(!args.iter().any(|a| a == "-crf"), "pass 1 must not ask for CRF: {args:?}");
        assert_eq!(arg_after(&args, "-pass:v").as_deref(), Some("1"));
        assert_eq!(arg_after(&args, "-passlogfile").as_deref(), Some("tmp/vqc-pass"));
        // Analysis only: output is discarded and no audio is encoded.
        assert_eq!(arg_after(&args, "-f").as_deref(), Some("null"));
        assert!(!args.iter().any(|a| a == "-c:a"), "pass 1 must skip the audio stream: {args:?}");
    }

    #[test]
    fn two_pass_encode_uses_the_same_stats_and_the_real_output() {
        let o = opts(true);
        let args = args_of(&o, Some(2));
        assert_eq!(arg_after(&args, "-b:v").as_deref(), Some("1500k"));
        assert_eq!(arg_after(&args, "-pass:v").as_deref(), Some("2"));
        assert_eq!(arg_after(&args, "-passlogfile").as_deref(), Some("tmp/vqc-pass"), "both passes must share the stats file");
        assert_eq!(arg_after(&args, "-f").as_deref(), Some("mp4"));
        assert_eq!(arg_after(&args, "-c:a").as_deref(), Some("aac"), "audio is re-encoded by default");
        assert_eq!(arg_after(&args, "-b:a").as_deref(), Some("160k"), "the configured audio bitrate");
        assert_eq!(arg_after(&args, "-ac").as_deref(), Some("2"), "the configured channel count");
    }

    #[test]
    fn fallback_audio_is_stream_copied_untouched() {
        let o = opts(true);
        let args = args_with_audio(&o, Some(2), AudioMode::Copy);
        assert_eq!(arg_after(&args, "-c:a").as_deref(), Some("copy"));
        // No decode, no re-encode: the audio encoder options must be gone.
        assert!(!args.iter().any(|a| a == "-b:a" || a == "-ac"), "copy must not re-encode: {args:?}");
        // The fallback changes nothing about the video encode.
        assert_eq!(arg_after(&args, "-b:v").as_deref(), Some("1500k"));
        assert_eq!(arg_after(&args, "-pass:v").as_deref(), Some("2"));
    }

    #[test]
    fn audio_bitrate_zero_means_copy_from_the_start() {
        let mut o = opts(true);
        assert_eq!(base_audio_mode(&o), AudioMode::Reencode);
        o.audio_bitrate_kbps = 0;
        assert_eq!(base_audio_mode(&o), AudioMode::Copy, "0 kbit/s = never re-encode");
    }

    #[test]
    fn the_audio_fallback_only_fires_for_a_failed_encode_pass() {
        // The encode pass failing is the case worth one retry.
        assert!(should_retry_with_copied_audio(Some(2), false, false));
        assert!(should_retry_with_copied_audio(None, false, false), "single-pass CRF has no pass number");
        // Pass 1 only reads video: audio cannot be the reason it failed.
        assert!(!should_retry_with_copied_audio(Some(1), false, false));
        // Success and cancellation are not failures to recover from.
        assert!(!should_retry_with_copied_audio(Some(2), false, true));
        assert!(!should_retry_with_copied_audio(Some(2), true, false));
    }

    #[test]
    fn single_pass_stays_on_crf() {
        let o = opts(false);
        let args = args_of(&o, None);
        assert_eq!(arg_after(&args, "-crf").as_deref(), Some("35"));
        assert!(!args.iter().any(|a| a == "-pass:v" || a == "-b:v"), "single pass must not use -pass:v/-b:v: {args:?}");
        assert_eq!(arg_after(&args, "-f").as_deref(), Some("mp4"));
    }

    #[test]
    fn pass_flags_are_video_scoped() {
        // A bare `-pass 2` is a codec option for *every* output stream, so the
        // audio encoder would look for a stats file of its own and fail.
        let o = opts(true);
        for pass in [1u8, 2] {
            let args = args_of(&o, Some(pass));
            let want = pass.to_string();
            assert!(!args.iter().any(|a| a == "-pass"), "pass {pass} must not set a stream-agnostic -pass: {args:?}");
            assert_eq!(arg_after(&args, "-pass:v").as_deref(), Some(want.as_str()));
        }
    }

    #[test]
    fn single_pass_clamps_the_output_bitrate() {
        let mut o = opts(false);
        assert_eq!(o.max_bitrate_kbps, 2000, "2000 kbit/s by default");
        let args = args_of(&o, None);
        assert_eq!(arg_after(&args, "-maxrate").as_deref(), Some("2000k"));
        assert_eq!(arg_after(&args, "-bufsize").as_deref(), Some("4000k"), "2x the cap");

        // 0 = no ceiling at all.
        o.max_bitrate_kbps = 0;
        let args = args_of(&o, None);
        assert!(!args.iter().any(|a| a == "-maxrate" || a == "-bufsize"), "no cap must add no flags: {args:?}");
    }

    #[test]
    fn two_pass_never_passes_maxrate() {
        // SVT-AV1 hard-fails on -maxrate in VBR mode ("Max Bitrate only
        // supported with CRF mode"), and two-pass always runs VBR.
        let o = opts(true);
        assert_eq!(o.max_bitrate_kbps, 2000);
        for pass in [Some(1u8), Some(2)] {
            let args = args_of(&o, pass);
            assert!(!args.iter().any(|a| a == "-maxrate" || a == "-bufsize"), "pass {pass:?} must not clamp: {args:?}");
        }
    }

    #[test]
    fn passing_progress_is_monotonic_across_passes() {
        let dur = 100.0;
        let single = SINGLE_PASS_LAYOUT[0];
        assert_eq!(progress_secs(dur, &single, 0.0), 0.0);
        assert_eq!(progress_secs(dur, &single, 50.0), 50.0);
        assert_eq!(progress_secs(dur, &single, 100.0), 100.0);

        let (p1, p2) = (TWO_PASS_LAYOUT[0], TWO_PASS_LAYOUT[1]);
        // Pass 1 only fills its slice, pass 2 continues from there to the end.
        assert_eq!(progress_secs(dur, &p1, dur), p1.share * dur);
        assert_eq!(progress_secs(dur, &p2, 0.0), p1.share * dur);
        assert_eq!(progress_secs(dur, &p2, dur), dur);
        assert!(progress_secs(dur, &p1, 60.0) < progress_secs(dur, &p2, 0.0));
    }

    #[test]
    fn two_pass_target_sits_below_the_threshold() {
        let mut cfg = Config { two_pass: true, ..Config::default() };
        assert!(cfg.two_pass, "two-pass is on by default");
        assert!(Config::default().two_pass, "two-pass is on by default");
        cfg.bitrate_threshold_kbps = 2000;
        assert_eq!(cfg.two_pass_target_kbps(), 1500);
        cfg.bitrate_threshold_kbps = 100;
        assert_eq!(cfg.two_pass_target_kbps(), 75);
        // Never degenerate into a zero target.
        cfg.bitrate_threshold_kbps = 0;
        assert_eq!(cfg.two_pass_target_kbps(), 32);
    }

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
        assert_eq!((p.to_str(), c), (Some("x/a.mp4"), Container::Mp4));
        let (p, c) = output_target(Path::new("x/a.MKV"));
        assert_eq!((p.to_str(), c), (Some("x/a.MKV"), Container::Matroska));
        // m4v/mov keep their name but must be muxed as mp4: the m4v extension
        // selects the iPod muxer, which cannot hold AV1.
        for name in ["x/a.m4v", "x/a.MOV"] {
            let (p, c) = output_target(Path::new(name));
            assert_eq!((p.to_str(), c), (Some(name), Container::Mp4));
            assert_eq!(c.muxer(), "mp4");
        }
        // Containers that cannot hold AV1 (or AAC, for webm) get renamed to mkv.
        for name in ["x/a.avi", "x/a.ts", "x/a.webm"] {
            let (p, c) = output_target(Path::new(name));
            assert_eq!(p.to_str(), Some("x/a.mkv"), "{name}");
            assert_eq!(c.muxer(), "matroska", "{name}");
        }
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
        assert!(!sibling(&orig, ".vqc-bak").exists(), "backup should be removed when keep_backup is off");

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
        let tail =
            vec!["[libx264 @ 0000] using cpu capabilities".to_string(), "Error while decoding stream #0:0: Invalid data found when processing input".to_string(), "Conversion failed!".to_string()];
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
    fn strips_ffmpeg_context_tags_and_addresses() {
        let line = "[vist#0:0/h264 @ 0000021e421007c0] [dec:h264 @ 0000021e421001c0] Decoding error: Invalid data found when processing input";
        assert_eq!(strip_log_prefixes(line), "Decoding error: Invalid data found when processing input");
        assert_eq!(strip_log_prefixes("[out#0/mp4 @ 0x5571c0] Nothing was written into output file."), "Nothing was written into output file.");
        // Text that merely contains brackets survives untouched.
        assert_eq!(strip_log_prefixes("stream [0:0] failed"), "stream [0:0] failed");
        assert_eq!(strip_log_prefixes("Conversion failed!"), "Conversion failed!");
    }

    #[test]
    fn recognizes_a_damaged_source() {
        // The real-world case: an .m4v whose bitstream is destroyed at 91%.
        let user = vec![
            "[h264 @ 0000] Error splitting the input into NAL units.".to_string(),
            "[h264 @ 0000] Invalid NAL unit size (-472513162 > 40463).".to_string(),
            "[aac @ 0000] Number of bands (19) exceeds limit (10).".to_string(),
        ];
        assert!(looks_like_damaged_source(&user));
        assert!(looks_like_damaged_source(&["[mov] moov atom not found".to_string()]));

        // Setup failures must not be blamed on the file.
        for line in ["No space left on device", "Permission denied", "Unknown encoder 'libsvtav1'", "Error opening output files: I/O error", "ERROR: Invalid pattern"] {
            assert!(!looks_like_damaged_source(&[line.to_string()]), "{line}");
        }
        assert!(!looks_like_damaged_source(&[]));
    }

    #[test]
    fn damaged_source_summary_is_readable() {
        let tail = vec![
            "[vist#0:0/h264 @ 0000021e421007c0] [dec:h264 @ 0000021e421001c0] Decoding error: Invalid data found when processing input".to_string(),
            "    Last message repeated 10 times".to_string(),
        ];
        let s = summarize_stderr(&tail);
        assert_eq!(s, "Decoding error: Invalid data found when processing input");
        assert!(!s.contains("0000"), "addresses must not leak into the row: {s}");
        assert!(looks_like_damaged_source(&tail));
    }

    #[test]
    fn recognizes_audio_the_container_cannot_hold() {
        let tail = vec!["[mp4 @ 0000] Could not find tag for codec vorbis in stream #1, codec not currently supported in container".to_string()];
        assert!(looks_like_audio_mux_rejection(&tail));
        assert!(!looks_like_audio_mux_rejection(&["[h264 @ 0] Invalid NAL unit size (1 > 2).".to_string()]));
        assert!(!looks_like_audio_mux_rejection(&[]));
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
