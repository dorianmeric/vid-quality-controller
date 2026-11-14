//! Messages sent from background threads to the UI thread.

use crate::encode::JobResult;
use crate::probe::MediaInfo;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Ev {
    ScanStart { total: usize },
    ScanProgress { done: usize, total: usize, current: String },
    ScanFile(Box<MediaInfo>),
    ScanError { path: PathBuf, msg: String },
    ScanDone { scanned: usize, errors: usize, elapsed_secs: f64 },
    /// A job started; `command` is the full ffmpeg command line of its first pass.
    JobStarted { id: u64, path: PathBuf, duration_secs: f64, size_bytes: u64, command: String },
    /// A job moved on to another ffmpeg invocation (second pass, audio fallback).
    /// `note` says why; `command` replaces the one shown for that job.
    JobCommand { id: u64, note: String, command: String },
    JobProgress { id: u64, out_secs: f64, speed: f64, out_bytes: u64 },
    JobFinished(Box<JobResult>),
    EncodeDone { elapsed_secs: f64 },
}
