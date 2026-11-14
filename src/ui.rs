//! All rendering.

use crate::app::{App, Field, Phase, RowStatus};
use crate::encode::Outcome;
use crate::fmt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Paragraph, Row, Table, Wrap};

const ACCENT: Color = Color::Rgb(120, 200, 255);
const GOOD: Color = Color::Rgb(110, 220, 150);
const WARN: Color = Color::Rgb(240, 200, 100);
const BAD: Color = Color::Rgb(240, 110, 110);
const DIM: Color = Color::Rgb(140, 140, 150);
const SEL_BG: Color = Color::Rgb(45, 50, 70);

pub fn draw(f: &mut Frame, app: &mut App) {
    let panel_h = match app.phase {
        Phase::Encoding => (5 + app.cfg.parallelism.max(1) as u16).min(14),
        _ => 6,
    };
    // During encoding a full (untruncated) ffmpeg command line sits above the
    // key hints, so you can see exactly what is running.
    let cmd_h = if app.phase == Phase::Encoding { command_height(app, f.area(), panel_h) } else { 0 };
    let mut constraints = vec![Constraint::Length(5), Constraint::Min(4), Constraint::Length(panel_h)];
    if cmd_h > 0 {
        constraints.push(Constraint::Length(cmd_h));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::vertical(constraints).split(f.area());

    draw_header(f, chunks[0], app);
    match app.phase {
        Phase::Scanning => draw_scan_body(f, chunks[1], app),
        Phase::Review => draw_review_body(f, chunks[1], app),
        Phase::Encoding => draw_encode_body(f, chunks[1], app),
        Phase::Summary => draw_summary_body(f, chunks[1], app),
    }
    draw_panel(f, chunks[2], app);
    let footer = chunks.len() - 1;
    if cmd_h > 0 {
        draw_command(f, chunks[footer - 1], app);
    }
    draw_footer(f, chunks[footer], app);

    if app.confirm {
        draw_confirm(f, f.area(), app);
    }
    if app.settings {
        draw_settings(f, f.area(), app);
    }
}

// ---------------------------------------------------------------- header

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(Line::from(vec![
            Span::styled(" ▤ ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled("Video Quality Controller", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
        .border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let phase_badge = match app.phase {
        Phase::Scanning => Span::styled(" SCANNING ", Style::new().fg(Color::Black).bg(WARN)),
        Phase::Review => Span::styled(" REVIEW ", Style::new().fg(Color::Black).bg(ACCENT)),
        Phase::Encoding => Span::styled(" ENCODING ", Style::new().fg(Color::Black).bg(GOOD)),
        Phase::Summary => Span::styled(" SUMMARY ", Style::new().fg(Color::Black).bg(GOOD)),
    };

    let mut line1 = vec![Span::styled("root ", Style::new().fg(DIM)), Span::styled(app.cfg.root.display().to_string(), Style::new().fg(Color::White)), Span::raw("  "), phase_badge];
    if app.cfg.dry_run {
        line1.push(Span::raw(" "));
        line1.push(Span::styled(" DRY RUN ", Style::new().fg(Color::Black).bg(WARN).add_modifier(Modifier::BOLD)));
    }

    let line2 = Line::from(vec![Span::styled("preset ", Style::new().fg(DIM)), Span::styled(app.cfg.preset_summary(), Style::new().fg(Color::Rgb(190, 190, 200)))]);

    let line3 = if let Some(msg) = app.toast_text() {
        Line::from(vec![Span::styled("» ", Style::new().fg(ACCENT)), Span::styled(msg.to_string(), Style::new().fg(ACCENT))])
    } else if app.phase == Phase::Scanning {
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let s = spinner[(app.tick / 2) as usize % spinner.len()];
        Line::from(vec![Span::styled(format!("{s} "), Style::new().fg(WARN)), Span::styled("scanning folder…", Style::new().fg(DIM))])
    } else {
        let cands = app.candidate_count();
        let bytes = app.candidate_bytes();
        Line::from(vec![
            Span::styled("» ", Style::new().fg(DIM)),
            Span::styled(format!("{} files scanned · {cands} candidates over {} kbps · {} total", app.rows.len(), app.cfg.bitrate_threshold_kbps, fmt::bytes(bytes)), Style::new().fg(DIM)),
        ])
    };

    let para = Paragraph::new(vec![Line::from(line1), line2, line3]);
    f.render_widget(para, inner);
}

// ---------------------------------------------------------------- bodies

fn draw_scan_body(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Scanning ").border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(3), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(inner);

    let ratio = if app.scan_total == 0 { 0.0 } else { (app.scan_done as f64 / app.scan_total as f64).clamp(0.0, 1.0) };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("probing headers with ffprobe ", Style::new().fg(DIM)),
            Span::styled(format!("{} / {}", app.scan_done, app.scan_total), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        ])),
        rows[0],
    );
    f.render_widget(Gauge::default().gauge_style(Style::new().fg(ACCENT).bg(Color::Rgb(35, 40, 55))).ratio(ratio).label(format!("{:.0}%", ratio * 100.0)), rows[1]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("current: ", Style::new().fg(DIM)),
            Span::styled(truncate(&app.scan_current, area.width.saturating_sub(20) as usize), Style::new().fg(Color::Rgb(190, 190, 200))),
        ])),
        rows[2],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("found: ", Style::new().fg(DIM)),
            Span::styled(format!("{} files, {} candidates", app.rows.len(), app.candidate_count()), Style::new().fg(GOOD)),
            Span::raw("   "),
            Span::styled("unreadable: ", Style::new().fg(DIM)),
            Span::styled(app.scan_errors.len().to_string(), Style::new().fg(BAD)),
        ])),
        rows[3],
    );

    if !app.scan_errors.is_empty() {
        let lines: Vec<Line> = app
            .scan_errors
            .iter()
            .take(rows[4].height as usize)
            .map(|e| Line::from(Span::styled(truncate(e, inner.width.saturating_sub(2) as usize), Style::new().fg(Color::Rgb(150, 110, 110)))))
            .collect();
        f.render_widget(Paragraph::new(lines), rows[4]);
    }
}

fn draw_review_body(f: &mut Frame, area: Rect, app: &mut App) {
    let title = format!(" Candidates {} · selected {} / {} ", app.visible.len(), app.selected_count(), fmt::bytes(app.selected_bytes()));
    let block = Block::bordered().title(title).border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let widths = [
        Constraint::Length(100),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(30),
    ];
    let header = Row::new(vec!["file", "resolution", "fps", "codec", "bitrate", "size", "length", "quality", "folder"]).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .visible
        .iter()
        .take(2000)
        .map(|&i| {
            let r = &app.rows[i];
            let info = &r.info;
            let flag = if r.status == RowStatus::Running {
                Span::styled("⟳", Style::new().fg(WARN))
            } else if let RowStatus::Done(o) = r.status {
                Span::styled(if o.is_success() { "✔" } else { "✖" }, Style::new().fg(if o.is_success() { GOOD } else { BAD }))
            } else if r.selected {
                Span::styled("✔", Style::new().fg(GOOD))
            } else {
                Span::styled("·", Style::new().fg(DIM))
            };

            // How far over the threshold this file is.
            let over = info.bitrate_bps as f64 / app.cfg.threshold_bps().max(1) as f64;
            let quality = if info.is_av1() {
                Span::styled("av1 (skip)", Style::new().fg(DIM))
            } else if over > 4.0 {
                Span::styled(format!("{over:.1}x over"), Style::new().fg(BAD))
            } else if over > 2.0 {
                Span::styled(format!("{over:.1}x over"), Style::new().fg(WARN))
            } else {
                Span::styled(format!("{over:.1}x over"), Style::new().fg(DIM))
            };

            let style = if r.candidate { Style::new().fg(Color::Rgb(205, 205, 215)) } else { Style::new().fg(Color::Rgb(105, 105, 115)) };

            Row::new(vec![
                Cell::from(Line::from(vec![flag, Span::raw(" "), Span::styled(truncate(&info.name(), 50), style)])),
                Cell::from(fmt::resolution(info.width, info.height)),
                Cell::from(fmt::fps(info.fps)),
                Cell::from(info.vcodec.clone()),
                Cell::from(fmt::bitrate(info.bitrate_bps)),
                Cell::from(fmt::bytes_short(info.size_bytes)),
                Cell::from(fmt::duration(info.duration_secs)),
                Cell::from(quality),
                Cell::from(Span::styled(truncate(&info.folder(), 150), Style::new().fg(Color::Rgb(110, 110, 120)))),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(rows, widths).header(header).column_spacing(1).row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD).fg(Color::White)).highlight_symbol("▸ ");

    f.render_stateful_widget(table, inner, &mut app.table_state);
}

fn draw_encode_body(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" In progress ").border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(enc) = &app.enc else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    // Newest at the bottom: keep the tail of the list in completion order.
    let room = inner.height as usize;
    let skip = enc.results.len().saturating_sub(room);
    for r in enc.results.iter().skip(skip) {
        let (tag, color) = match r.outcome {
            Outcome::Replaced => ("replaced", GOOD),
            Outcome::DryRun => ("dry-run ", GOOD),
            Outcome::Rejected => ("rejected", WARN),
            Outcome::Failed => ("failed  ", BAD),
            Outcome::Cancelled => ("cancel  ", WARN),
        };
        let saved = if r.outcome.is_success() {
            format!("{} → {}  -{} ({:.0}%)", fmt::bytes(r.old_size), fmt::bytes(r.new_size), fmt::bytes(r.saved_bytes().max(0) as u64), r.saved_pct())
        } else {
            format!("kept {}", fmt::bytes(r.old_size))
        };
        // Bitrate before → after. Nothing was written for a rejection/failure, so
        // those only have the source bitrate to show.
        let bitrate = if r.new_bitrate_bps > 0 {
            format!("{} → {}", fmt::bitrate(r.old_bitrate_bps), fmt::bitrate(r.new_bitrate_bps))
        } else {
            fmt::bitrate(r.old_bitrate_bps)
        };
        // Failures and rejections carry the reason; show it at the end of the row.
        let path = display_path(&r.path);
        let reason = if r.outcome.is_success() || r.note.is_empty() {
            String::new()
        } else {
            let used = 9 + path.chars().count() + 2 + saved.chars().count() + 2 + bitrate.chars().count();
            let room = (inner.width as usize).saturating_sub(used + 4);
            if room < 8 { String::new() } else { format!("  ·  {}", truncate(&r.note, room)) }
        };
        lines.push(Line::from(vec![
            Span::styled(format!("[{tag}] "), Style::new().fg(color)),
            Span::styled(path, Style::new().fg(Color::Rgb(200, 200, 210))),
            Span::raw("  "),
            Span::styled(saved, Style::new().fg(DIM)),
            Span::raw("  "),
            Span::styled(bitrate, Style::new().fg(Color::Rgb(150, 150, 165))),
            Span::styled(reason, Style::new().fg(color)),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("waiting for the first result…", Style::new().fg(DIM))));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_summary_body(f: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::bordered().title(" Before / after ").border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let widths = [
        Constraint::Length(36),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Min(10),
    ];
    let header = Row::new(vec!["file", "before", "after", "saved", "saved%", "bitrate", "length Δ", "time", "status"]).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD));

    let results: Vec<crate::encode::JobResult> = app.enc.as_ref().map(|e| e.results.clone()).unwrap_or_default();

    let rows: Vec<Row> = results
        .iter()
        .map(|r| {
            let (color, label) = match r.outcome {
                Outcome::Replaced => (GOOD, "replaced"),
                Outcome::DryRun => (GOOD, "dry-run"),
                Outcome::Rejected => (WARN, "rejected"),
                Outcome::Failed => (BAD, "failed"),
                Outcome::Cancelled => (WARN, "cancelled"),
            };
            let new = if r.new_size > 0 { fmt::bytes_short(r.new_size) } else { "—".into() };
            let saved = if r.new_size > 0 { format!("-{}", fmt::bytes_short(r.saved_bytes().max(0) as u64)) } else { "—".into() };
            let pct = if r.new_size > 0 { format!("{:.0}%", r.saved_pct()) } else { "—".into() };
            let delta = if r.new_duration > 0.0 { format!("{:+.2}s", r.new_duration - r.old_duration) } else { "—".into() };
            Row::new(vec![
                Cell::from(truncate(&r.path.display().to_string(), 35)),
                Cell::from(fmt::bytes_short(r.old_size)),
                Cell::from(new),
                Cell::from(Span::styled(saved, Style::new().fg(color))),
                Cell::from(Span::styled(pct, Style::new().fg(color))),
                Cell::from(format!("{}→{}", fmt::bits_short(r.old_bitrate_bps), fmt::bits_short(r.new_bitrate_bps))),
                Cell::from(delta),
                Cell::from(format!("{:.0}s", r.total_secs)),
                // The status column is the flexible one, so the reason for a
                // failure/rejection fits at the end of the row.
                Cell::from(Line::from(vec![
                    Span::styled(label, Style::new().fg(color)),
                    Span::styled(if r.note.is_empty() { String::new() } else { format!(" · {}", r.note) }, Style::new().fg(if r.outcome.is_success() { DIM } else { color })),
                ])),
            ])
            .style(Style::new().fg(Color::Rgb(200, 200, 210)))
        })
        .collect();

    let table = Table::new(rows, widths).header(header).column_spacing(1).row_highlight_style(Style::new().bg(SEL_BG));
    f.render_stateful_widget(table, inner, &mut app.table_state);
}

// ---------------------------------------------------------------- panel

fn draw_panel(f: &mut Frame, area: Rect, app: &App) {
    match app.phase {
        Phase::Encoding => draw_encode_panel(f, area, app),
        Phase::Summary => draw_summary_panel(f, area, app),
        _ => draw_review_panel(f, area, app),
    }
}

fn draw_review_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Plan ").border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let selected = app.selected_count();
    let bytes = app.selected_bytes();
    let avg_bps: u64 = {
        let total_bps: u64 = app.rows.iter().filter(|r| r.candidate && r.selected).map(|r| r.info.bitrate_bps).sum();
        if selected > 0 { total_bps / selected as u64 } else { 0 }
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("queued ", Style::new().fg(DIM)),
            Span::styled(format!("{selected} files"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(" · ", Style::new().fg(DIM)),
            Span::styled(fmt::bytes(bytes), Style::new().fg(ACCENT)),
            Span::styled(format!(" (avg {})", fmt::bitrate(avg_bps)), Style::new().fg(DIM)),
        ]),
        Line::from(vec![
            Span::styled("encode ", Style::new().fg(DIM)),
            Span::styled(
                format!("libsvtav1 {} preset{} → AAC {}k {}ch (copied if that fails)", app.cfg.video_mode(), app.cfg.preset, app.cfg.audio_bitrate_kbps, app.cfg.audio_channels),
                Style::new().fg(Color::Rgb(190, 190, 200)),
            ),
            Span::styled("  (resolution & framerate untouched)", Style::new().fg(DIM)),
        ]),
        Line::from(vec![
            Span::styled("workers ", Style::new().fg(DIM)),
            Span::styled(format!("{}", app.cfg.parallelism), Style::new().fg(Color::Rgb(190, 190, 200))),
            Span::styled(" · sort ", Style::new().fg(DIM)),
            Span::styled(app.sort.label(), Style::new().fg(Color::Rgb(190, 190, 200))),
            Span::styled(" · view ", Style::new().fg(DIM)),
            Span::styled(if app.show_all { "all files" } else { "candidates" }, Style::new().fg(Color::Rgb(190, 190, 200))),
            Span::styled(" · dry-run ", Style::new().fg(DIM)),
            Span::styled(if app.cfg.dry_run { "on" } else { "off" }, Style::new().fg(if app.cfg.dry_run { WARN } else { DIM })),
        ]),
        detail_line(app),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// Source details for the row under the cursor.
fn detail_line(app: &App) -> Line<'static> {
    let row = app.table_state.selected().and_then(|sel| app.visible.get(sel)).map(|&i| &app.rows[i]);
    let Some(r) = row else {
        return Line::from(Span::styled("select a file with ↑↓ to inspect it", Style::new().fg(Color::Rgb(80, 80, 90))));
    };
    let info = &r.info;
    let vbr = info.video_bitrate_bps.map(fmt::bitrate).unwrap_or_else(|| "vbr ?".into());
    Line::from(vec![
        Span::styled("source ", Style::new().fg(DIM)),
        Span::styled(
            format!(
                "{} {}fps {}@{vbr} + {} {}ch ({} track{}) · {} · {}",
                fmt::resolution(info.width, info.height),
                fmt::fps(info.fps),
                info.vcodec,
                info.acodec,
                info.audio_channels,
                info.audio_streams,
                if info.audio_streams == 1 { "" } else { "s" },
                fmt::bytes(info.size_bytes),
                fmt::duration(info.duration_secs)
            ),
            Style::new().fg(Color::Rgb(170, 170, 185)),
        ),
    ])
}

fn draw_encode_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Progress ").border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(enc) = &app.enc else { return };

    let mut constraints = vec![Constraint::Length(2), Constraint::Length(1)];
    for _ in 0..enc.slots.len() {
        constraints.push(Constraint::Length(1));
    }
    let rows = Layout::vertical(constraints).split(inner);

    let overall = enc.overall();
    let label = format!(
        "{}/{} files · {:.1}% · {} saved · {}",
        enc.completed,
        enc.total,
        overall * 100.0,
        fmt::bytes(enc.saved_bytes().max(0) as u64),
        match enc.eta_secs() {
            Some(eta) => format!("ETA {}", fmt::duration(eta)),
            None => "ETA --:--".to_string(),
        }
    );
    f.render_widget(Gauge::default().gauge_style(Style::new().fg(GOOD).bg(Color::Rgb(35, 40, 55)).add_modifier(Modifier::BOLD)).ratio(overall).label(label), rows[0]);

    let status = if enc.cancelling {
        Span::styled("cancelling…", Style::new().fg(BAD))
    } else if enc.paused {
        Span::styled("PAUSED", Style::new().fg(WARN).add_modifier(Modifier::BOLD))
    } else if enc.finished {
        Span::styled("done", Style::new().fg(GOOD))
    } else {
        Span::styled("running", Style::new().fg(GOOD))
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![status, Span::styled(format!("   elapsed {} · {} workers", fmt::duration(enc.started.elapsed().as_secs_f64()), enc.slots.len()), Style::new().fg(DIM))])),
        rows[1],
    );

    for (i, slot) in enc.slots.iter().enumerate() {
        let area_row = rows[i + 2];
        match slot.id {
            Some(_) => {
                let pct = slot.frac * 100.0;
                // Bytes written so far, as a share of the original file size.
                // (Not the saving: 19% of source means 81% smaller.)
                let output_share = if slot.out_bytes > 0 && slot.src_bytes > 0 { format!("{:.0}%", slot.out_bytes as f64 * 100.0 / slot.src_bytes as f64) } else { String::new() };
                let position = fmt::duration(slot.frac * slot.dur);
                let length = fmt::duration(slot.dur);
                let eta = match slot.eta_secs() {
                    Some(eta) => fmt::duration_long(eta),
                    None => "estimating…".to_string(),
                };
                let full = format!(
                    "filename: {} · percent done: {:.0}% · speed: {:.2}x · position: {position} / {length} · eta: {eta} · size: {output_share} of orig",
                    truncate(&slot.name, 34),
                    pct,
                    slot.speed,
                );
                // The verbose label is long; fall back to a compact form so the
                // ETA stays visible on narrower terminals.
                let available = area_row.width.saturating_sub(2) as usize;
                let label = if full.chars().count() <= available {
                    full
                } else {
                    let compact = format!("{} · {:.0}% · {:.2}x · {position}/{length} · eta: {eta} · size: {output_share} of orig", truncate(&slot.name, 28), pct, slot.speed,);
                    if compact.chars().count() <= available { compact } else { truncate(&compact, available) }
                };
                f.render_widget(Gauge::default().gauge_style(Style::new().fg(ACCENT).bg(Color::Rgb(30, 34, 46))).ratio(slot.frac.clamp(0.0, 1.0)).label(label), area_row);
            }
            None => {
                f.render_widget(Paragraph::new(Line::from(Span::styled("idle", Style::new().fg(Color::Rgb(70, 70, 80))))), area_row);
            }
        }
    }
}

fn draw_summary_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Totals ").border_style(Style::new().fg(Color::Rgb(70, 75, 95)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(enc) = &app.enc else { return };
    let ok: Vec<&crate::encode::JobResult> = enc.results.iter().filter(|r| r.outcome.is_success()).collect();
    let before: u64 = ok.iter().map(|r| r.old_size).sum();
    let after: u64 = ok.iter().map(|r| r.new_size).sum();
    let saved = before as i64 - after as i64;
    let pct = if before > 0 { saved as f64 * 100.0 / before as f64 } else { 0.0 };
    let total_secs: f64 = ok.iter().map(|r| r.total_secs).sum();
    let rejected = enc.results.iter().filter(|r| r.outcome == Outcome::Rejected).count();
    let failed = enc.results.iter().filter(|r| r.outcome == Outcome::Failed).count();
    let cancelled = enc.results.iter().filter(|r| r.outcome == Outcome::Cancelled).count();

    let lines = vec![
        Line::from(vec![
            Span::styled("processed ", Style::new().fg(DIM)),
            Span::styled(format!("{} files", ok.len()), Style::new().fg(GOOD).add_modifier(Modifier::BOLD)),
            Span::styled(" in ", Style::new().fg(DIM)),
            Span::styled(fmt::duration(enc.elapsed.max(total_secs)), Style::new().fg(Color::White)),
            Span::styled(" · rejected ", Style::new().fg(DIM)),
            Span::styled(rejected.to_string(), Style::new().fg(WARN)),
            Span::styled(" · failed ", Style::new().fg(DIM)),
            Span::styled(failed.to_string(), Style::new().fg(BAD)),
            Span::styled(" · cancelled ", Style::new().fg(DIM)),
            Span::styled(cancelled.to_string(), Style::new().fg(WARN)),
        ]),
        Line::from(vec![
            Span::styled("disk ", Style::new().fg(DIM)),
            Span::styled(fmt::bytes(before), Style::new().fg(Color::Rgb(190, 190, 200))),
            Span::styled(" → ", Style::new().fg(DIM)),
            Span::styled(fmt::bytes(after), Style::new().fg(Color::Rgb(190, 190, 200))),
            Span::styled("  freed ", Style::new().fg(DIM)),
            Span::styled(format!("{} ({:.1}%)", fmt::bytes(saved.max(0) as u64), pct), Style::new().fg(GOOD).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("report ", Style::new().fg(DIM)),
            Span::styled(app.report_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "—".into()), Style::new().fg(ACCENT)),
            Span::styled("   (enter = rescan · q = quit)", Style::new().fg(Color::Rgb(70, 70, 80))),
        ]),
        Line::from(vec![
            Span::styled("log    ", Style::new().fg(DIM)),
            Span::styled(app.log_path().map(|p| p.display().to_string()).unwrap_or_else(|| "— (not writable)".into()), Style::new().fg(ACCENT)),
            Span::styled(if failed > 0 { "   ← full error details for failed files" } else { "" }, Style::new().fg(BAD)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------- command line

/// Rows the current command needs, capped so the rest of the screen survives.
fn command_height(app: &App, area: Rect, panel_h: u16) -> u16 {
    let needed = command_rows(app, area.width).len() as u16;
    // Header (5) + progress panel + footer (1) + a few rows for the result list.
    let max = area.height.saturating_sub(5 + panel_h + 1 + 3).max(1);
    needed.min(max).max(1)
}

/// The bottom ffmpeg line: the full command of the job being encoded, wrapped
/// hard (never elided) so every argument stays readable.
fn command_rows(app: &App, width: u16) -> Vec<String> {
    let text = match app.enc.as_ref().and_then(|enc| enc.active_command()) {
        Some((name, cmd)) => format!("{name} › {cmd}"),
        None => "waiting for the next ffmpeg job…".to_string(),
    };
    hard_wrap(&text, width as usize)
}

fn draw_command(f: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = command_rows(app, area.width)
        .into_iter()
        .take(area.height as usize)
        .enumerate()
        .map(|(i, row)| Line::from(Span::styled(row, Style::new().fg(if i == 0 { Color::Rgb(175, 180, 200) } else { Color::Rgb(140, 145, 160) }))))
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

// ---------------------------------------------------------------- footer

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys: Vec<(&str, &str)> = if app.settings {
        vec![("↑↓", "field"), ("enter", "edit/toggle"), ("s", "save+rescan"), ("esc", "close")]
    } else if app.confirm {
        vec![("enter", "start"), ("d", "dry-run"), ("esc", "cancel")]
    } else {
        match app.phase {
            Phase::Scanning => vec![("t", "settings"), ("q", "quit")],
            Phase::Review => {
                vec![("↑↓", "move"), ("space", "toggle"), ("a", "all"), ("s", "sort"), ("f", "filter"), ("d", "dry-run"), ("r", "rescan"), ("t", "settings"), ("enter", "encode"), ("q", "quit")]
            }
            Phase::Encoding => vec![("p", "pause"), ("c", "cancel"), ("q", "quit")],
            Phase::Summary => vec![("enter", "rescan"), ("q", "quit")],
        }
    };
    let mut spans = Vec::new();
    for (k, d) in keys {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::Rgb(110, 115, 135))));
        spans.push(Span::styled(format!(" {d} "), Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------- popups

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_confirm(f: &mut Frame, area: Rect, app: &App) {
    let popup = centered(72, 12, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered().title(" Start encoding? ").border_style(Style::new().fg(WARN));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let n = app.selected_count();
    let bytes = app.selected_bytes();
    let lines = vec![
        Line::from(vec![
            Span::styled("files: ", Style::new().fg(DIM)),
            Span::styled(n.to_string(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled("  total: ", Style::new().fg(DIM)),
            Span::styled(fmt::bytes(bytes), Style::new().fg(ACCENT)),
        ]),
        Line::from(vec![Span::styled("encoder: ", Style::new().fg(DIM)), Span::styled(app.cfg.preset_summary(), Style::new().fg(Color::White))]),
        Line::from(vec![
            Span::styled("safety: ", Style::new().fg(DIM)),
            Span::styled(format!("length must match within {}s and the result must be smaller", app.cfg.duration_tolerance_secs), Style::new().fg(Color::Rgb(190, 190, 200))),
        ]),
        Line::from(vec![
            Span::styled("mode: ", Style::new().fg(DIM)),
            Span::styled(
                if app.cfg.dry_run {
                    "DRY RUN — originals untouched"
                } else if app.cfg.keep_backup {
                    "originals moved to .vqc-backup/"
                } else {
                    "originals REPLACED in place"
                },
                Style::new().fg(if app.cfg.dry_run { WARN } else { GOOD }).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" enter ", Style::new().fg(Color::Black).bg(GOOD)),
            Span::styled(" start encoding    ", Style::new().fg(DIM)),
            Span::styled(" d ", Style::new().fg(Color::Black).bg(WARN)),
            Span::styled(" toggle dry-run    ", Style::new().fg(DIM)),
            Span::styled(" esc ", Style::new().fg(Color::Black).bg(Color::Gray)),
            Span::styled(" cancel", Style::new().fg(DIM)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_settings(f: &mut Frame, area: Rect, app: &App) {
    let popup = centered(84, 22, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered().title(" Settings ").border_style(Style::new().fg(ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);

    let mut lines: Vec<Line> = Vec::new();
    for (i, field) in Field::ALL.iter().enumerate() {
        let selected = i == app.settings_idx;
        let editing = selected && app.edit.is_some();
        let value = if editing {
            let mut v = app.edit.clone().unwrap_or_default();
            v.push('▏');
            v
        } else {
            field.value(&app.cfg)
        };
        let marker = if selected { "▸ " } else { "  " };
        let label_style = if selected { Style::new().fg(Color::White).bg(SEL_BG) } else { Style::new().fg(DIM) };
        let spans = vec![
            Span::styled(format!("{marker}{:<26}", field.label()), label_style),
            Span::styled(format!("{value:<28}"), Style::new().fg(if editing { WARN } else { ACCENT }).add_modifier(if editing { Modifier::BOLD } else { Modifier::empty() })),
            Span::styled(field.help(), Style::new().fg(Color::Rgb(90, 90, 100))),
        ];
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), rows[0]);

    let hint = vec![
        Line::from(Span::styled("type a value, enter to confirm · esc to discard", Style::new().fg(DIM))),
        Line::from(Span::styled(format!("file: {}", app.cfg_path.display()), Style::new().fg(Color::Rgb(90, 90, 100)))),
    ];
    f.render_widget(Paragraph::new(hint).wrap(Wrap { trim: true }), rows[1]);
}

// ---------------------------------------------------------------- helpers

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        let mut out: String = s.chars().take(keep).collect();
        out.push('…');
        out
    }
}

/// Split `text` into rows of at most `width` characters. Used for the command
/// line, which is shown in full: the terminal wraps it instead of us cutting it.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

/// Longest folder part and file name shown in the in-progress list, in
/// characters. Short paths are shown as they are.
const FOLDER_MAX: usize = 100;
const NAME_MAX: usize = 200;

/// Full path for the in-progress list. Only the parts that are actually too long
/// are shortened: the folder past 100 characters (its tail is kept, that is the
/// part next to the file) and the file name past 200.
pub(crate) fn display_path(p: &std::path::Path) -> String {
    let full = p.display().to_string();
    let Some(name) = p.file_name() else {
        return shorten_start(&full, FOLDER_MAX);
    };
    let name_chars = name.to_string_lossy().chars().count();
    let chars: Vec<char> = full.chars().collect();
    if name_chars >= chars.len() {
        return truncate(&full, NAME_MAX);
    }
    let split = chars.len() - name_chars;
    let folder: String = chars[..split].iter().collect();
    let name: String = chars[split..].iter().collect();
    format!("{}{}", shorten_start(&folder, FOLDER_MAX), truncate(&name, NAME_MAX))
}

/// Cut from the front, keeping the tail of the path in view.
fn shorten_start(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out = String::from("…");
    out.extend(s.chars().skip(count - max + 1));
    out
}
