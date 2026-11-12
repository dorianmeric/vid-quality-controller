//! Small formatting helpers used by both the TUI and the report writer.

/// Human readable byte size (GiB / MiB / KiB).
pub fn bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let f = n as f64;
    if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.1} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.0} KiB", f / KIB)
    } else {
        format!("{n} B")
    }
}

/// Slightly shorter variant for tables.
pub fn bytes_short(n: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let f = n as f64;
    if f >= GIB {
        format!("{:.2}G", f / GIB)
    } else {
        format!("{:.0}M", f / MIB)
    }
}

/// `h:mm:ss` (or `m:ss` for short clips).
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Compact bitrate: `3.42 Mbps` / `812 kbps`.
pub fn bitrate(bps: u64) -> String {
    let k = bps as f64 / 1000.0;
    if k >= 1000.0 {
        format!("{:.2} Mbps", k / 1000.0)
    } else {
        format!("{k:.0} kbps")
    }
}

/// Compact bitrate for tight table cells: `3.4M` / `812k`.
pub fn bits_short(bps: u64) -> String {
    let k = bps as f64 / 1000.0;
    if k >= 1000.0 {
        format!("{:.1}M", k / 1000.0)
    } else {
        format!("{k:.0}k")
    }
}

pub fn resolution(w: u32, h: u32) -> String {
    if w == 0 || h == 0 {
        "?".into()
    } else {
        format!("{w}x{h}")
    }
}

pub fn fps(v: f64) -> String {
    if v <= 0.0 {
        "?".into()
    } else if (v - v.round()).abs() < 0.02 {
        format!("{:.0}", v)
    } else {
        format!("{v:.2}")
    }
}
