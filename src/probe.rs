//! ffprobe wrapper: turns a video file into a `MediaInfo`.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub duration_secs: f64,
    /// Total container bitrate (video + audio + overhead).
    pub bitrate_bps: u64,
    pub video_bitrate_bps: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: String,
    pub acodec: String,
    pub audio_channels: u16,
    pub audio_streams: usize,
    pub has_video: bool,
}

impl MediaInfo {
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn folder(&self) -> String {
        self.path.parent().map(|p| p.display().to_string()).unwrap_or_default()
    }

    pub fn is_av1(&self) -> bool {
        self.vcodec.eq_ignore_ascii_case("av1")
    }
}

/// Run ffprobe and parse the result. Cheap: only headers are read.
pub fn probe(ffprobe: &Path, path: &Path) -> Result<MediaInfo> {
    let out =
        Command::new(ffprobe).args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]).arg(path).output().with_context(|| format!("spawning ffprobe for {}", path.display()))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("unknown error");
        bail!("ffprobe: {first}");
    }

    let v: Value = serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    let empty = Vec::new();
    let streams = v["streams"].as_array().unwrap_or(&empty);

    let mut video: Option<&Value> = None;
    let mut audio: Option<&Value> = None;
    let mut audio_streams = 0usize;
    for s in streams {
        match s["codec_type"].as_str().unwrap_or("") {
            "video" => {
                let is_cover = s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 1;
                if !is_cover && video.is_none() {
                    video = Some(s);
                }
            }
            "audio" => {
                audio_streams += 1;
                if audio.is_none() {
                    audio = Some(s);
                }
            }
            _ => {}
        }
    }

    let fmt = &v["format"];
    let size_bytes = fmt["size"].as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| std::fs::metadata(path).ok().map(|m| m.len())).unwrap_or(0);

    let duration_secs = fmt["duration"].as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| video.and_then(|s| s["duration"].as_str()).and_then(|s| s.parse().ok())).unwrap_or(0.0);

    let bitrate_bps = fmt["bit_rate"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|b| *b > 0)
        .or_else(|| if duration_secs > 0.0 { Some((size_bytes as f64 * 8.0 / duration_secs) as u64) } else { None })
        .unwrap_or(0);

    let video_bitrate_bps = video.and_then(|s| s["bit_rate"].as_str()).and_then(|s| s.parse::<u64>().ok()).filter(|b| *b > 0);

    let width = video.and_then(|s| s["width"].as_u64()).unwrap_or(0) as u32;
    let height = video.and_then(|s| s["height"].as_u64()).unwrap_or(0) as u32;
    let fps = video.and_then(|s| s["avg_frame_rate"].as_str().or_else(|| s["r_frame_rate"].as_str())).and_then(parse_ratio).unwrap_or(0.0);

    let vcodec = video.and_then(|s| s["codec_name"].as_str()).unwrap_or("none").to_string();
    let acodec = audio.and_then(|s| s["codec_name"].as_str()).unwrap_or("none").to_string();
    let audio_channels = audio.and_then(|s| s["channels"].as_u64()).unwrap_or(0) as u16;

    Ok(MediaInfo { path: path.to_path_buf(), size_bytes, duration_secs, bitrate_bps, video_bitrate_bps, width, height, fps, vcodec, acodec, audio_channels, audio_streams, has_video: video.is_some() })
}

fn parse_ratio(s: &str) -> Option<f64> {
    if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.parse().ok()?;
        let d: f64 = d.parse().ok()?;
        if d == 0.0 { None } else { Some(n / d) }
    } else {
        s.parse().ok()
    }
}
