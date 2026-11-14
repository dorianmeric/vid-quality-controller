//! Render / state tests. These exercise every screen through ratatui's
//! TestBackend so layout or index bugs surface without a real terminal.

#![cfg(test)]

use crate::app::{App, EncodeState, Field, Phase, RowItem, RowStatus, Slot, SortMode, is_candidate};
use crate::config::Config;
use crate::encode::{JobResult, Outcome};
use crate::event::Ev;
use crate::probe::MediaInfo;
use crossbeam_channel::unbounded;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;
use std::time::Instant;

fn media(name: &str, mb: f64, mbps: f64, codec: &str, secs: f64) -> MediaInfo {
    MediaInfo {
        // Forward slashes so `file_name()` behaves the same on every platform.
        path: PathBuf::from(format!("E:/Po/folder/{name}")),
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

/// A finished job, for the in-progress / summary lists.
fn job(path: &str, outcome: Outcome) -> JobResult {
    JobResult {
        path: PathBuf::from(path),
        final_path: PathBuf::from(path),
        outcome,
        old_size: 900 * 1024 * 1024,
        new_size: 300 * 1024 * 1024,
        old_bitrate_bps: 6_500_000,
        new_bitrate_bps: 2_100_000,
        old_duration: 1200.0,
        new_duration: 1200.0,
        encode_secs: 100.0,
        total_secs: 101.0,
        note: String::new(),
        detail: String::new(),
    }
}

fn test_app() -> App {
    let (tx, _rx) = unbounded();
    let mut app = App::new(Config::default(), PathBuf::from("config.json"), None, tx);
    app.rows = vec![
        RowItem { info: media("a.mp4", 900.0, 6.5, "h264", 1200.0), candidate: true, selected: true, status: RowStatus::Pending },
        RowItem { info: media("b.mp4", 40.0, 2.4, "hevc", 130.0), candidate: true, selected: true, status: RowStatus::Done(Outcome::Replaced) },
        RowItem { info: media("c.mp4", 12.0, 0.8, "h264", 120.0), candidate: false, selected: false, status: RowStatus::Pending },
        RowItem { info: media("d.mkv", 300.0, 3.0, "av1", 600.0), candidate: false, selected: false, status: RowStatus::Pending },
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
            Slot { id: Some(7), name: "b.mp4".into(), frac: 0.42, speed: 3.5, out_bytes: 10 * 1024 * 1024, dur: 130.0, src_bytes: 40 * 1024 * 1024, started: Some(Instant::now()), command: String::new() },
            Slot { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None, command: String::new() },
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
            detail: String::new(),
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

const LONG_CMD: &str = concat!(
    "C:\\P\\ffmpeg\\bin\\ffmpeg.exe -hide_banner -nostdin -y -loglevel error -progress pipe:1 -nostats ",
    "-i E:\\Po\\folder\\a.mp4 -map 0:v:0 -map 0:a? -map_metadata 0 -c:v libsvtav1 -b:v 1500k -preset 6 ",
    "-pix_fmt yuv420p -pass:v 2 -passlogfile .vqc-tmp-42-abc\\vqc-pass -c:a aac -b:a 160k -ac 2 ",
    "-movflags +faststart -f mp4 .vqc-tmp-42-abc\\a.mp4",
);

fn encoding_app_with_command(cmd: &str) -> App {
    let mut app = test_app();
    app.phase = Phase::Encoding;
    app.enc = Some(EncodeState {
        total: 1,
        completed: 0,
        started: Instant::now(),
        slots: vec![Slot {
            id: Some(7),
            name: "a.mp4".into(),
            frac: 0.1,
            speed: 1.0,
            out_bytes: 0,
            dur: 120.0,
            src_bytes: 0,
            started: Some(Instant::now()),
            command: cmd.into(),
        }],
        results: Vec::new(),
        paused: false,
        cancelling: false,
        elapsed: 0.0,
        finished: false,
    });
    app
}

#[test]
fn encoding_screen_shows_the_whole_ffmpeg_command() {
    let mut app = encoding_app_with_command(LONG_CMD);
    let text = render(&mut app);
    // Joining the rows back together must rebuild the command exactly: it is
    // wrapped, never elided like the gauge labels are.
    let flat = text.replace('\n', "");
    assert!(flat.contains(LONG_CMD), "command must be shown in full:\n{text}");
}

#[test]
fn command_is_logged_with_the_start_entry() {
    let dir = std::env::temp_dir().join(format!("vqc-cmd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut app = encoding_app_with_command(LONG_CMD);
    app.attach_log(crate::log::RunLog::create_in(&dir, &Config::default()));
    let path = app.rows[0].info.path.clone();
    app.on_event(Ev::JobStarted { id: 7, path: path.clone(), duration_secs: 120.0, size_bytes: 1024, command: LONG_CMD.into() });
    app.on_event(Ev::JobCommand { id: 7, note: "pass 2/2".into(), command: "ffmpeg.exe -pass:v 2 stats".into() });

    let log = std::fs::read_to_string(app.log_path().expect("log file")).unwrap();
    assert!(log.contains(&format!("start: {}", path.display())), "start entry missing:\n{log}");
    assert!(log.contains(&format!("command: {LONG_CMD}")), "command must be logged with the start entry:\n{log}");
    assert!(log.contains("pass 2/2: ffmpeg.exe -pass:v 2 stats"), "later passes must be logged too:\n{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn in_progress_lists_the_newest_result_at_the_bottom() {
    let mut app = test_app();
    app.phase = Phase::Encoding;
    app.enc = Some(EncodeState {
        total: 3,
        completed: 2,
        started: Instant::now(),
        slots: vec![Slot { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None, command: String::new() }],
        results: vec![job("E:/Po/first/one.mp4", Outcome::Replaced), job("E:/Po/second/two.mp4", Outcome::Replaced)],
        paused: false,
        cancelling: false,
        elapsed: 10.0,
        finished: false,
    });
    let text = render(&mut app);
    let first = text.find("one.mp4").expect("earlier result missing");
    let second = text.find("two.mp4").expect("later result missing");
    assert!(first < second, "the newest result must be the lower one:\n{text}");
}

#[test]
fn in_progress_shows_full_paths_and_only_shortens_long_ones() {
    // A folder of 101 characters must be shortened to 100, a shorter one kept.
    let long_folder = format!("E:/Po/{}/", "x".repeat(94));
    assert_eq!(long_folder.chars().count(), 101, "folder is one char over the limit");
    let path = format!("{long_folder}clip.mp4");
    let mut app = test_app();
    app.phase = Phase::Encoding;
    app.enc = Some(EncodeState {
        total: 2,
        completed: 2,
        started: Instant::now(),
        slots: vec![Slot { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None, command: String::new() }],
        results: vec![job("E:/Po/short/kept.mp4", Outcome::Replaced), job(&path, Outcome::Replaced)],
        paused: false,
        cancelling: false,
        elapsed: 10.0,
        finished: false,
    });
    let text = render(&mut app);
    assert!(text.contains("E:/Po/short/kept.mp4"), "a short path must be shown in full:\n{text}");
    assert!(text.contains("clip.mp4"), "the file name must survive folder shortening:\n{text}");
    assert!(text.contains('…'), "the over-long folder must be marked as shortened:\n{text}");
    assert!(!text.contains("E:/Po/xxx"), "the folder was not shortened:\n{text}");
}

#[test]
fn long_file_names_are_shortened_at_200_characters() {
    let long_name = format!("{}.mp4", "n".repeat(198));
    assert_eq!(long_name.chars().count(), 202);
    let mut app = test_app();
    app.phase = Phase::Encoding;
    app.enc = Some(EncodeState {
        total: 1,
        completed: 1,
        started: Instant::now(),
        slots: vec![Slot { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None, command: String::new() }],
        results: vec![job(&format!("E:/Po/{long_name}"), Outcome::Replaced)],
        paused: false,
        cancelling: false,
        elapsed: 10.0,
        finished: true,
    });
    let shown = crate::ui::display_path(std::path::Path::new(&format!("E:/Po/{long_name}")));
    assert_eq!(shown.chars().count(), "E:/Po/".chars().count() + 200, "folder + a 200-char name: {shown}");
    assert!(shown.starts_with("E:/Po/nnn"), "folder must be kept: {shown}");
    assert!(shown.ends_with('…'), "name must be marked as shortened: {shown}");
    // And it renders without panicking on a narrow terminal.
    render(&mut app);
}

#[test]
fn failed_job_shows_its_error_in_the_row() {
    let mut app = test_app();
    app.phase = Phase::Encoding;
    app.enc = Some(EncodeState {
        total: 2,
        completed: 2,
        started: Instant::now(),
        slots: vec![Slot { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None, command: String::new() }],
        results: vec![JobResult {
            path: PathBuf::from("E:\\Po\\folder\\broken.mp4"),
            final_path: PathBuf::from("E:\\Po\\folder\\broken.mp4"),
            outcome: Outcome::Failed,
            old_size: 500 * 1024 * 1024,
            new_size: 0,
            old_bitrate_bps: 5_000_000,
            new_bitrate_bps: 0,
            old_duration: 600.0,
            new_duration: 0.0,
            encode_secs: 44.9,
            total_secs: 45.2,
            note: "at 71% of 10:00 · Error while decoding stream #0:0: Invalid data found".into(),
            detail: "command: ffmpeg -i ...".into(),
        }],
        paused: false,
        cancelling: false,
        elapsed: 45.2,
        finished: true,
    });
    let text = render(&mut app);
    assert!(text.contains("[failed"), "tag missing:\n{text}");
    assert!(text.contains("Error while decoding"), "failure reason must be visible in the row:\n{text}");

    // Same reason must survive on the summary screen, at the end of the row.
    app.phase = Phase::Summary;
    let text = render(&mut app);
    assert!(text.contains("broken.mp4"), "row missing:\n{text}");
    assert!(text.contains("Error while decoding"), "summary must show the failure reason:\n{text}");
}

#[test]
fn summary_screen_renders() {
    let mut app = test_app();
    app.phase = Phase::Summary;
    app.enc = Some(EncodeState {
        total: 1,
        completed: 1,
        started: Instant::now(),
        slots: vec![Slot { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None, command: String::new() }],
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
            detail: String::new(),
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
    // Every field must fit the popup: a new one that does not is silently cut off.
    assert!(text.contains("Two-pass VBR encode"), "two-pass setting missing:\n{text}");
    assert!(text.contains("Max output bitrate (kbps)"), "max bitrate setting missing:\n{text}");
    assert!(text.contains("Keep .vqc-backup originals"), "last field must still be visible:\n{text}");
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
fn max_bitrate_field_parses_and_accepts_zero_for_off() {
    let field = Field::MaxBitrate;
    assert!(!field.is_bool(), "a number is typed in, not toggled");
    let mut cfg = Config::default();
    assert_eq!(cfg.max_bitrate_kbps, 2000, "2000 kbit/s by default");
    assert_eq!(field.value(&cfg), "2000");

    field.apply(&mut cfg, "0").unwrap();
    assert_eq!(cfg.max_bitrate_kbps, 0, "0 disables the ceiling");
    field.apply(&mut cfg, " 1800 ").unwrap();
    assert_eq!(cfg.max_bitrate_kbps, 1800);
    assert!(field.apply(&mut cfg, "200001").is_err(), "above the sane range");
    assert!(field.apply(&mut cfg, "lots").is_err());
}

#[test]
fn audio_settings_keep_a_set_bitrate_and_accept_zero() {
    let mut cfg = Config::default();
    assert_eq!(Field::AudioKbps.value(&cfg), "160", "re-encode is the default");
    assert_eq!(Field::AudioChannels.value(&cfg), "2");

    // 0 kbit/s = never re-encode: the encoder copies the audio instead.
    Field::AudioKbps.apply(&mut cfg, "0").unwrap();
    assert_eq!(cfg.audio_bitrate_kbps, 0);
    Field::AudioKbps.apply(&mut cfg, "192").unwrap();
    assert_eq!(cfg.audio_bitrate_kbps, 192);
    assert!(Field::AudioKbps.apply(&mut cfg, "513").is_err());
    assert!(Field::AudioChannels.apply(&mut cfg, "3").is_ok());
    assert!(Field::AudioChannels.apply(&mut cfg, "0").is_err());
}

#[test]
fn two_pass_setting_is_a_toggle_that_defaults_on() {
    let field = Field::TwoPass;
    assert!(field.is_bool(), "two-pass is edited with enter/space, not by typing");
    let mut cfg = Config::default();
    assert!(cfg.two_pass, "two-pass must be on by default");
    assert_eq!(field.value(&cfg), "yes");
    field.toggle(&mut cfg);
    assert!(!cfg.two_pass);
    assert_eq!(field.value(&cfg), "no");
    field.toggle(&mut cfg);
    assert!(cfg.two_pass);
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
    let mut slot = Slot { id: Some(1), name: "a.mp4".into(), frac: 0.5, speed: 4.0, out_bytes: 0, dur: 60.0, src_bytes: 0, started: Some(Instant::now() - Duration::from_secs(30)), command: String::new() };
    let eta = slot.eta_secs().expect("half way there means an ETA");
    assert!((25.0..=35.0).contains(&eta), "half done after 30s should leave ~30s, got {eta}");

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

#[test]
fn encode_order_follows_the_table_order() {
    let (tx, _rx) = unbounded();
    let mut app = App::new(Config::default(), PathBuf::from("c.json"), None, tx);
    // Names sort opposite to sizes, so the two sort modes give different orders.
    app.rows = vec![
        RowItem { info: media("z_big.mp4", 900.0, 6.0, "h264", 600.0), candidate: true, selected: true, status: RowStatus::Pending },
        RowItem { info: media("m_mid.mp4", 400.0, 5.0, "h264", 600.0), candidate: true, selected: true, status: RowStatus::Pending },
        RowItem { info: media("a_small.mp4", 40.0, 4.0, "h264", 600.0), candidate: true, selected: true, status: RowStatus::Pending },
        RowItem { info: media("skip_me.mp4", 500.0, 9.0, "h264", 600.0), candidate: true, selected: false, status: RowStatus::Pending },
    ];

    // Default sort (size, biggest first) is what the table shows.
    app.sort = SortMode::Size;
    app.refresh_visible();
    let order: Vec<String> = app.selected_jobs().iter().map(|m| m.name()).collect();
    assert_eq!(order, ["z_big.mp4", "m_mid.mp4", "a_small.mp4"]);

    // Re-sorting by name must re-order the queue the same way.
    app.sort = SortMode::Name;
    app.refresh_visible();
    let order: Vec<String> = app.selected_jobs().iter().map(|m| m.name()).collect();
    assert_eq!(order, ["a_small.mp4", "m_mid.mp4", "z_big.mp4"]);

    // The deselected file never enters the queue, and only candidates do.
    assert!(!order.iter().any(|n| n == "skip_me.mp4"));

    app.show_all = true;
    app.refresh_visible();
    assert_eq!(app.selected_jobs().len(), 3, "filter must not change the queue");
}
