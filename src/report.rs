//! Report writing: a CSV for spreadsheets and a readable text summary.

use crate::config::Config;
use crate::encode::JobResult;
use crate::fmt;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn write(results: &[JobResult], cfg: &Config) -> PathBuf {
    let dir = PathBuf::from("reports");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = timestamp();
    let csv_path = dir.join(format!("vqc-{stamp}.csv"));
    let txt_path = dir.join(format!("vqc-{stamp}.txt"));

    let mut csv = String::new();
    csv.push_str("status,file,final_path,old_bytes,new_bytes,saved_bytes,saved_pct,old_kbps,new_kbps,old_duration_s,new_duration_s,encode_s,total_s,note\n");
    for r in results {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{:.2},{},{},{:.3},{:.3},{:.1},{:.1},{}\n",
            r.outcome.short(),
            csv_escape(&r.path.display().to_string()),
            csv_escape(&r.final_path.display().to_string()),
            r.old_size,
            r.new_size,
            r.saved_bytes(),
            r.saved_pct(),
            r.old_bitrate_bps / 1000,
            r.new_bitrate_bps / 1000,
            r.old_duration,
            r.new_duration,
            r.encode_secs,
            r.total_secs,
            csv_escape(&r.note),
        ));
    }
    let _ = std::fs::write(&csv_path, csv);

    let mut txt = String::new();
    let replaced = results.iter().filter(|r| r.outcome.is_success()).count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.outcome, crate::encode::Outcome::Failed))
        .count();
    let rejected = results
        .iter()
        .filter(|r| matches!(r.outcome, crate::encode::Outcome::Rejected))
        .count();
    let cancelled = results
        .iter()
        .filter(|r| matches!(r.outcome, crate::encode::Outcome::Cancelled))
        .count();
    let old_total: u64 = results.iter().map(|r| r.old_size).sum();
    let new_total: u64 = results
        .iter()
        .filter(|r| r.outcome.is_success())
        .map(|r| r.new_size)
        .sum();
    let saved: i64 = results.iter().map(|r| r.saved_bytes()).sum();

    let _ = writeln!(txt, "Video Quality Controller report — {stamp}");
    let _ = writeln!(txt, "root: {}", cfg.root.display());
    let _ = writeln!(
        txt,
        "preset: AV1/SVT crf{} preset{} {} · AAC {}k {}ch · threshold {} kbps",
        cfg.crf,
        cfg.preset,
        cfg.pix_fmt,
        cfg.audio_bitrate_kbps,
        cfg.audio_channels,
        cfg.bitrate_threshold_kbps
    );
    let _ = writeln!(
        txt,
        "processed {} · replaced {} · rejected {} · failed {} · cancelled {}",
        results.len(),
        replaced,
        rejected,
        failed,
        cancelled
    );
    let _ = writeln!(
        txt,
        "before {} · after {} · saved {} ({:.1}%)",
        fmt::bytes(old_total),
        fmt::bytes(new_total),
        fmt::bytes(saved.max(0) as u64),
        if old_total > 0 {
            saved as f64 * 100.0 / old_total as f64
        } else {
            0.0
        }
    );
    let _ = writeln!(txt);
    for r in results {
        let _ = writeln!(
            txt,
            "[{}] {}\n    {} -> {} ({:+.1}%) · {} -> {} · {:.1}s encode · {}",
            r.outcome.short(),
            r.path.display(),
            fmt::bytes(r.old_size),
            fmt::bytes(r.new_size),
            -r.saved_pct(),
            fmt::bitrate(r.old_bitrate_bps),
            fmt::bitrate(r.new_bitrate_bps),
            r.encode_secs,
            r.note
        );
    }
    let _ = std::fs::write(&txt_path, txt);

    csv_path
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil inverse.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
