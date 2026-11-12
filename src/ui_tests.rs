//! Render / state tests. These exercise every screen through ratatui's
//! TestBackend so layout or index bugs surface without a real terminal.

#![cfg(test)]

use crate::app::{
    App, EncodeState, Phase, RowItem, RowStatus, Slot, SortMode, is_candidate,
};
use crate::config::Config;
use crate::encode::{JobResult, Outcome};
use crate::probe::MediaInfo;
use crossbeam_channel::unbounded;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;
use std::time::Instant;

fn media(name: &str, mb: f64, mbps: f64, codec: &str, secs: f64) -> MediaInfo {
    MediaInfo {
        path: PathBuf::from(format!("E:\\Po\\folder\\{name}")),
        size_bytes: (mb * 1024.0 * 1024.0) as u64,
        duration_secs: secs,
        bitrate_bps: (mbps * 1_000_000.0) as u64,
        video_bitrate_bps: Some((mbps * 0.95 * 1_000_000.0) as u64),
        width: 1920,
        height: 1080,
        fps: 29.97,
        vcodec: codec.into(),
        acodec: "aac".into(),
        audio_channels: 2,
        audio_streams: 1,
        has_video: true,
    }
}

fn test_app() -> App {
    let (tx, _rx) = unbounded();
    let mut app = App::new(Config::default(), PathBuf::from("config.json"), None, tx);
    app.rows = vec![
        RowItem {
            info: media("a.mp4", 900.0, 6.5, "h264", 1200.0),
            candidate: true,
            selected: true,
            status: RowStatus::Pending,
        },
        RowItem {
            info: media("b.mp4", 40.0, 2.4, "hevc", 130.0),
            candidate: true,
            selected: true,
            status: RowStatus::Done(Outcome::Replaced),
        },
        RowItem {
            info: media("c.mp4", 12.0, 0.8, "h264", 120.0),
            candidate: false,
            selected: false,
            status: RowStatus::Pending,
        },
        RowItem {
            info: media("d.mkv", 300.0, 3.0, "av1", 600.0),
            candidate: false,
            selected: false,
            status: RowStatus::Pending,
        },
    ];
    app.refresh_visible();
    app.table_state.select(Some(0));
    app
}

fn render(app: &mut App) -> String {
    let backend = TestBackend::new(160, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn review_screen_renders() {
    let mut app = test_app();
    app.phase = Phase::Review;
    let text = render(&mut app);
    assert!(text.contains("Candidates"), "header missing:\n{text}");
    assert!(text.contains("a.mp4"), "row missing:\n{text}");
    assert!(text.contains("3.2x over"), "quality flag missing:\n{text}");
    assert!(text.contains("libsvtav1"), "plan missing:\n{text}");
    assert!(text.contains("source"), "detail line missing:\n{text}");
}

#[test]
fn filtering_hides_non_candidates() {
    let mut app = test_app();
    app.phase = Phase::Review;
    let cands = render(&mut app);
    assert!(!cands.contains("c.mp4"), "non-candidate leaked:\n{cands}");
    app.show_all = true;
    app.refresh_visible();
    let all = render(&mut app);
    assert!(all.contains("c.mp4"), "show-all did not reveal file:\n{all}");
}

#[test]
fn sorting_and_selection_are_stable() {
    let mut app = test_app();
    app.sort = SortMode::Bitrate;
    app.refresh_visible();
    assert_eq!(app.visible.len(), 2, "candidates only: a + b (av1 skipped)");
    assert_eq!(app.selected_count(), 2);
    app.toggle_all(); // all currently selected -> deselect
    assert_eq!(app.selected_count(), 0);
    app.toggle_all();
    assert_eq!(app.selected_count(), 2);
}

#[test]
fn scanning_screen_renders() {
    let mut app = test_app();
    app.phase = Phase::Scanning;
    app.scan_total = 100;
    app.scan_done = 42;
    app.scan_current = "folder/file.mp4".into();
    let text = render(&mut app);
    assert!(text.contains("42 / 100"), "progress missing:\n{text}");
}

#[test]
fn encoding_screen_renders() {
    let mut app = test_app();
    app.phase = Phase::Encoding;
    app.enc = Some(EncodeState {
        total: 2,
        completed: 1,
        started: Instant::now(),
        slots: vec![
            Slot {
                id: Some(7),
                name: "b.mp4".into(),
                frac: 0.42,
                speed: 3.5,
                out_bytes: 10 * 1024 * 1024,
                dur: 130.0,
                src_bytes: 40 * 1024 * 1024,
                started: Some(Instant::now()),
            },
            Slot {
                id: None,
                name: String::new(),
                frac: 0.0,
                speed: 0.0,
                out_bytes: 0,
                dur: 0.0,
                src_bytes: 0,
                started: None,
            },
        ],
        results: vec![JobResult {
            path: PathBuf::from("E:\\Po\\folder\\a.mp4"),
            final_path: PathBuf::from("E:\\Po\\folder\\a.mp4"),
            outcome: Outcome::Replaced,
            old_size: 900 * 1024 * 1024,
            new_size: 300 * 1024 * 1024,
            old_bitrate_bps: 6_500_000,
            new_bitrate_bps: 2_100_000,
            old_duration: 1200.0,
            new_duration: 1200.0,
            encode_secs: 100.0,
            total_secs: 101.0,
            note: "duration delta +0.00s".into(),
        }],
        paused: false,
        cancelling: false,
        elapsed: 0.0,
        finished: false,
    });
    let text = render(&mut app);
    assert!(text.contains("1/2 files"), "overall gauge missing:\n{text}");
    assert!(text.contains("b.mp4"), "slot missing:\n{text}");
    assert!(text.contains("replaced"), "recent result missing:\n{text}");
}

#[test]
fn summary_screen_renders() {
    let mut app = test_app();
    app.phase = Phase::Summary;
    app.enc = Some(EncodeState {
        total: 1,
        completed: 1,
        started: Instant::now(),
        slots: vec![Slot {
            id: None,
            name: String::new(),
            frac: 0.0,
            speed: 0.0,
            out_bytes: 0,
            dur: 0.0,
            src_bytes: 0,
            started: None,
        }],
        results: vec![JobResult {
            path: PathBuf::from("E:\\Po\\folder\\a.mp4"),
            final_path: PathBuf::from("E:\\Po\\folder\\a.mp4"),
            outcome: Outcome::Replaced,
            old_size: 900 * 1024 * 1024,
            new_size: 300 * 1024 * 1024,
            old_bitrate_bps: 6_500_000,
            new_bitrate_bps: 2_100_000,
            old_duration: 1200.0,
            new_duration: 1200.0,
            encode_secs: 100.0,
            total_secs: 101.0,
            note: String::new(),
        }],
        paused: false,
        cancelling: false,
        elapsed: 101.0,
        finished: true,
    });
    app.report_path = Some(PathBuf::from("reports/vqc-test.csv"));
    let text = render(&mut app);
    assert!(text.contains("Before / after"), "table missing:\n{text}");
    assert!(text.contains("freed"), "totals missing:\n{text}");
    assert!(text.contains("reports/vqc-test.csv"), "report path missing:\n{text}");
}

#[test]
fn popups_render() {
    let mut app = test_app();
    app.phase = Phase::Review;
    app.confirm = true;
    let text = render(&mut app);
    assert!(text.contains("Start encoding?"), "confirm missing:\n{text}");
    app.confirm = false;
    app.settings = true;
    let text = render(&mut app);
    assert!(text.contains("Settings"), "settings missing:\n{text}");
    assert!(text.contains("Bitrate threshold"), "fields missing:\n{text}");
}

#[test]
fn tiny_terminal_does_not_panic() {
    let mut app = test_app();
    for phase in [Phase::Scanning, Phase::Review, Phase::Encoding, Phase::Summary] {
        app.phase = phase;
        for (w, h) in [(20u16, 6u16), (40, 10), (80, 20)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        }
    }
}

#[test]
fn candidate_rules() {
    let cfg = Config::default();
    // Over threshold, big enough, not AV1 -> candidate.
    assert!(is_candidate(&cfg, &media("x.mp4", 100.0, 3.0, "h264", 60.0)));
    // Under threshold.
    assert!(!is_candidate(&cfg, &media("x.mp4", 100.0, 0.9, "h264", 60.0)));
    // Already AV1 -> skipped by default.
    assert!(!is_candidate(&cfg, &media("x.mp4", 100.0, 5.0, "av1", 60.0)));
    let mut allow_av1 = cfg.clone();
    allow_av1.skip_av1 = false;
    assert!(is_candidate(&allow_av1, &media("x.mp4", 100.0, 5.0, "av1", 60.0)));
    // Too small.
    assert!(!is_candidate(&cfg, &media("x.mp4", 5.0, 5.0, "h264", 60.0)));
    // No duration -> not trustworthy.
    assert!(!is_candidate(&cfg, &media("x.mp4", 100.0, 5.0, "h264", 0.0)));
}

#[test]
fn slot_eta_uses_time_spent_on_that_file() {
    use std::time::Duration;
    let mut slot = Slot {
        id: Some(1),
        name: "a.mp4".into(),
        frac: 0.5,
        speed: 4.0,
        out_bytes: 0,
        dur: 60.0,
        src_bytes: 0,
        started: Some(Instant::now() - Duration::from_secs(30)),
    };
    let eta = slot.eta_secs().expect("half way there means an ETA");
    assert!(
        (25.0..=35.0).contains(&eta),
        "half done after 30s should leave ~30s, got {eta}"
    );

    // Too early to trust: no estimate under 2%.
    slot.frac = 0.01;
    assert!(slot.eta_secs().is_none());

    // No start time -> no estimate.
    slot.frac = 0.5;
    slot.started = None;
    assert!(slot.eta_secs().is_none());

    // Nearly done -> short remainder.
    slot.started = Some(Instant::now() - Duration::from_secs(100));
    slot.frac = 0.99;
    let eta = slot.eta_secs().unwrap();
    assert!(eta < 5.0, "99% done should be almost finished, got {eta}");
}
