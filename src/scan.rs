//! Recursive folder scan with a small ffprobe thread pool.

use crate::config::Config;
use crate::event::Ev;
use crate::probe;
use crossbeam_channel::Sender;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use walkdir::WalkDir;

/// Walk `cfg.root` and probe every video file, streaming results to the UI.
pub fn start(cfg: Config, tx: Sender<Ev>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let started = Instant::now();
        let files = collect_files(&cfg);
        let total = files.len();
        let _ = tx.send(Ev::ScanStart { total });

        let queue = Arc::new(Mutex::new(VecDeque::from(files)));
        let done = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicUsize::new(0));
        let workers = cfg.probe_parallelism.clamp(1, 16);

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            let errors = Arc::clone(&errors);
            let tx = tx.clone();
            let ffprobe = cfg.ffprobe.clone();
            handles.push(thread::spawn(move || {
                loop {
                    let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                    let Some(path) = next else { break };
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(Ev::ScanProgress {
                        done: n,
                        total,
                        current: short_name(&path),
                    });
                    match probe::probe(&ffprobe, &path) {
                        Ok(info) => {
                            let _ = tx.send(Ev::ScanFile(Box::new(info)));
                        }
                        Err(e) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(Ev::ScanError {
                                path,
                                msg: e.to_string(),
                            });
                        }
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }

        let _ = tx.send(Ev::ScanDone {
            scanned: done.load(Ordering::Relaxed),
            errors: errors.load(Ordering::Relaxed),
            elapsed_secs: started.elapsed().as_secs_f64(),
        });
    })
}

fn collect_files(cfg: &Config) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !cfg.root.is_dir() {
        return out;
    }
    for entry in WalkDir::new(&cfg.root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !cfg.is_video_ext(path) {
            continue;
        }
        // Skip our own temp dirs, backups and leftover files.
        if is_ours(path) {
            continue;
        }
        out.push(path.to_path_buf());
    }
    out.sort();
    out
}

fn is_ours(path: &std::path::Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s.starts_with(".vqc-") || s.ends_with(".vqc-bak")
    })
}

fn short_name(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if parent.is_empty() {
        name
    } else {
        format!("{parent}/{name}")
    }
}
