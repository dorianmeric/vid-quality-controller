//! Small formatting helpers used by both the TUI and the report writer.

/// Wall-clock time in the machine's local timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LocalTime {
    /// `2026-10-12 01:05:03`
    pub fn datetime(&self) -> String {
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// `2026-10-12-010530` — used for log file names.
    pub fn stamp(&self) -> String {
        format!("{:04}-{:02}-{:02}-{:02}{:02}{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// `01:05:03` — used as a log line prefix.
    pub fn clock(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }
}

/// Current local time. Uses `GetLocalTime` on Windows so log timestamps match
/// the user's clock; falls back to UTC elsewhere.
#[cfg(windows)]
pub fn local_now() -> LocalTime {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    unsafe extern "system" {
        fn GetLocalTime(out: *mut SystemTime);
    }

    let mut st = SystemTime { year: 0, month: 0, day_of_week: 0, day: 0, hour: 0, minute: 0, second: 0, milliseconds: 0 };
    // SAFETY: GetLocalTime only writes into the struct we pass by pointer.
    unsafe { GetLocalTime(&mut st) };
    if st.year == 0 {
        return utc_now();
    }
    LocalTime { year: st.year as i32, month: st.month as u32, day: st.day as u32, hour: st.hour as u32, minute: st.minute as u32, second: st.second as u32 }
}

#[cfg(not(windows))]
pub fn local_now() -> LocalTime {
    utc_now()
}

fn utc_now() -> LocalTime {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    LocalTime { year: year as i32, month, day, hour: (rem / 3600) as u32, minute: ((rem % 3600) / 60) as u32, second: (rem % 60) as u32 }
}

/// Howard Hinnant's days-from-civil inverse.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
    if f >= GIB { format!("{:.2}G", f / GIB) } else { format!("{:.0}M", f / MIB) }
}

/// `h:mm:ss` (or `m:ss` for short clips).
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Compact duration with only the non-zero units, e.g. `11d 23h 12m 10s`,
/// `4m 5s`, `12s`.
pub fn duration_long(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "—".into();
    }
    let total = secs.round() as u64;
    let (d, h, m, s) = (total / 86_400, (total % 86_400) / 3600, (total % 3600) / 60, total % 60);
    let mut parts: Vec<String> = Vec::new();
    for (value, name) in [(d, "d"), (h, "h"), (m, "m"), (s, "s")] {
        if value > 0 {
            parts.push(format!("{value}{name}"));
        }
    }
    if parts.is_empty() {
        return "0 seconds".into();
    }
    parts.join(" ")
}

/// Compact bitrate: `3.42 Mbps` / `812 kbps`.
pub fn bitrate(bps: u64) -> String {
    let k = bps as f64 / 1000.0;
    if k >= 1000.0 { format!("{:.2} Mbps", k / 1000.0) } else { format!("{k:.0} kbps") }
}

/// Compact bitrate for tight table cells: `3.4M` / `812k`.
pub fn bits_short(bps: u64) -> String {
    let k = bps as f64 / 1000.0;
    if k >= 1000.0 { format!("{:.1}M", k / 1000.0) } else { format!("{k:.0}k") }
}

pub fn resolution(w: u32, h: u32) -> String {
    if w == 0 || h == 0 { "?".into() } else { format!("{w}x{h}") }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_durations_skip_zero_units() {
        assert_eq!(duration_long(0.0), "0 seconds");
        assert_eq!(duration_long(1.0), "1s");
        assert_eq!(duration_long(59.0), "59s");
        assert_eq!(duration_long(60.0), "1m");
        assert_eq!(duration_long(61.0), "1m 1s");
        assert_eq!(duration_long(245.0), "4m 5s");
        assert_eq!(duration_long(3600.0), "1h");
        assert_eq!(duration_long(3660.0), "1h 1m");
        assert_eq!(duration_long(86_400.0), "1d");
        assert_eq!(duration_long(1_033_930.0), "11d 23h 12m 10s");
        assert_eq!(duration_long(-5.0), "—");
        assert_eq!(duration_long(f64::NAN), "—");
    }
}
