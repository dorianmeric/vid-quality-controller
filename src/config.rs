//! Persistent configuration (`config.json` next to the working directory).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Folder that is scanned recursively.
    pub root: PathBuf,
    /// Any video whose *total* bitrate is above this is a candidate.
    pub bitrate_threshold_kbps: u32,
    /// Ignore files smaller than this (not worth the CPU).
    pub min_size_mb: f64,
    /// SVT-AV1 quality (lower = better/bigger). 35 is the house default.
    pub crf: u8,
    /// SVT-AV1 speed preset 0..13 (higher = faster, bigger files).
    pub preset: u8,
    /// Output pixel format. `yuv420p` = 8-bit, most compatible.
    pub pix_fmt: String,
    pub audio_bitrate_kbps: u32,
    pub audio_channels: u16,
    /// Add `+faststart` for mp4/m4v outputs (slower, better for streaming).
    pub faststart: bool,
    /// Skip files that are already AV1 (re-encoding them is a quality loss).
    pub skip_av1: bool,
    /// Concurrent encode jobs.
    pub parallelism: usize,
    /// Concurrent ffprobe jobs during the scan.
    pub probe_parallelism: usize,
    /// Allowed |new_duration - old_duration| in seconds before a result is rejected.
    pub duration_tolerance_secs: f64,
    /// Keep the original as `<name>.vqc-bak` instead of deleting it.
    pub keep_backup: bool,
    /// Encode + verify but never touch the originals.
    pub dry_run: bool,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Extra arguments appended to the ffmpeg command line.
    pub ffmpeg_extra_args: Vec<String>,
    /// File extensions (lowercase, with dot) that are considered videos.
    pub extensions: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: PathBuf::from(r"E:\Po"),
            bitrate_threshold_kbps: 2000,
            min_size_mb: 20.0,
            crf: 35,
            preset: 6,
            pix_fmt: "yuv420p".into(),
            audio_bitrate_kbps: 160,
            audio_channels: 2,
            faststart: true,
            skip_av1: true,
            parallelism: 4,
            probe_parallelism: 6,
            duration_tolerance_secs: 1.0,
            keep_backup: false,
            dry_run: false,
            ffmpeg: PathBuf::from(r"C:\P\ffmpeg\bin\ffmpeg.exe"),
            ffprobe: PathBuf::from(r"C:\P\ffmpeg\bin\ffprobe.exe"),
            ffmpeg_extra_args: Vec::new(),
            extensions: [
                "mp4", "m4v", "mkv", "avi", "mov", "wmv", "ts", "webm", "flv", "mpg", "mpeg",
                "m2ts", "divx", "3gp",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

impl Config {
    /// Load the config, creating it with defaults when missing.
    /// Returns the config plus an optional human-readable note for the UI.
    pub fn load_or_create(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    let note = cfg.resolve_tools();
                    (cfg, note)
                }
                Err(e) => (
                    Config::default(),
                    Some(format!("config parse error ({e}); using defaults")),
                ),
            },
            Err(_) => {
                let mut cfg = Config::default();
                let note = cfg.resolve_tools();
                let _ = cfg.save(path);
                (cfg, note)
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(path, text)
    }

    /// Make sure ffmpeg/ffprobe point at something that exists.
    /// Falls back to the known install dir, then to PATH.
    pub fn resolve_tools(&mut self) -> Option<String> {
        let mut notes = Vec::new();
        match resolve_tool(&self.ffmpeg, "ffmpeg.exe") {
            Some(p) => {
                if p != self.ffmpeg {
                    notes.push(format!("ffmpeg -> {}", p.display()));
                }
                self.ffmpeg = p;
            }
            None => notes.push("ffmpeg.exe NOT FOUND".to_string()),
        }
        match resolve_tool(&self.ffprobe, "ffprobe.exe") {
            Some(p) => {
                if p != self.ffprobe {
                    notes.push(format!("ffprobe -> {}", p.display()));
                }
                self.ffprobe = p;
            }
            None => notes.push("ffprobe.exe NOT FOUND".to_string()),
        }
        if notes.is_empty() {
            None
        } else {
            Some(notes.join(" | "))
        }
    }

    pub fn tools_ok(&self) -> bool {
        self.ffmpeg.is_file() && self.ffprobe.is_file()
    }

    pub fn threshold_bps(&self) -> u64 {
        self.bitrate_threshold_kbps as u64 * 1000
    }

    pub fn min_size_bytes(&self) -> u64 {
        (self.min_size_mb.max(0.0) * 1024.0 * 1024.0) as u64
    }

    pub fn is_video_ext(&self, path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let ext = ext.to_ascii_lowercase();
                self.extensions.contains(&ext)
            }
            None => false,
        }
    }

    /// One-line summary of the encode preset, used in the header.
    pub fn preset_summary(&self) -> String {
        format!(
            "AV1/SVT crf{} p{} · AAC {}k {}ch · thr {} kbps · {} jobs",
            self.crf,
            self.preset,
            self.audio_bitrate_kbps,
            self.audio_channels,
            self.bitrate_threshold_kbps,
            self.parallelism
        )
    }
}

fn resolve_tool(preferred: &Path, exe: &str) -> Option<PathBuf> {
    if preferred.is_file() {
        return Some(preferred.to_path_buf());
    }
    let fallback = Path::new(r"C:\P\ffmpeg\bin").join(exe);
    if fallback.is_file() {
        return Some(fallback);
    }
    // Search PATH.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
