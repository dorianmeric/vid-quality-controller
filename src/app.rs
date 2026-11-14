//! Application state and input handling.

use crate::config::Config;
use crate::encode::{self, Control, EncodeOpts, JobResult, Outcome};
use crate::event::Ev;
use crate::log::RunLog;
use crate::probe::MediaInfo;
use crate::report;
use crossbeam_channel::Sender;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    Review,
    Encoding,
    Summary,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Size,
    Bitrate,
    Name,
    Duration,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Size => "size",
            SortMode::Bitrate => "bitrate",
            SortMode::Name => "name",
            SortMode::Duration => "duration",
        }
    }
    fn next(self) -> Self {
        match self {
            SortMode::Size => SortMode::Bitrate,
            SortMode::Bitrate => SortMode::Duration,
            SortMode::Duration => SortMode::Name,
            SortMode::Name => SortMode::Size,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Pending,
    Running,
    Done(Outcome),
}

pub struct RowItem {
    pub info: MediaInfo,
    pub candidate: bool,
    pub selected: bool,
    pub status: RowStatus,
}

pub struct Slot {
    pub id: Option<u64>,
    pub name: String,
    pub frac: f64,
    pub speed: f64,
    pub out_bytes: u64,
    pub dur: f64,
    pub src_bytes: u64,
    /// When this job actually started encoding, for the per-file ETA.
    pub started: Option<Instant>,
}

impl Slot {
    fn empty() -> Self {
        Self { id: None, name: String::new(), frac: 0.0, speed: 0.0, out_bytes: 0, dur: 0.0, src_bytes: 0, started: None }
    }

    /// Estimated seconds left for *this file*: assume the work done so far
    /// is representative, i.e. `elapsed / frac * (1 - frac)`.
    /// `None` until enough progress has been made to trust the estimate.
    pub fn eta_secs(&self) -> Option<f64> {
        let started = self.started?;
        if self.frac <= 0.02 {
            return None;
        }
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }
        let remaining = (1.0 - self.frac) / self.frac * elapsed;
        Some(remaining.max(0.0))
    }
}

pub struct EncodeState {
    pub total: usize,
    pub completed: usize,
    pub started: Instant,
    pub slots: Vec<Slot>,
    pub results: Vec<JobResult>,
    pub paused: bool,
    pub cancelling: bool,
    pub elapsed: f64,
    pub finished: bool,
}

impl EncodeState {
    /// Fraction of the whole batch, counting partial progress of running jobs.
    pub fn overall(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let partial: f64 = self.slots.iter().map(|s| s.frac).sum();
        ((self.completed as f64 + partial) / self.total as f64).clamp(0.0, 1.0)
    }

    pub fn eta_secs(&self) -> Option<f64> {
        if self.completed == 0 {
            return None;
        }
        let per = self.started.elapsed().as_secs_f64() / self.completed as f64;
        let remaining = self.total.saturating_sub(self.completed) as f64;
        Some(per * remaining)
    }

    pub fn saved_bytes(&self) -> i64 {
        self.results.iter().map(|r| r.saved_bytes()).sum()
    }
}

pub struct App {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub phase: Phase,
    pub quit: bool,
    pub toast: Option<(String, Instant)>,
    pub tick: u64,
    tx: Sender<Ev>,

    // scan
    pub scan_total: usize,
    pub scan_done: usize,
    pub scan_current: String,
    pub scan_errors: Vec<String>,
    pub scan_elapsed: f64,
    pub scanning: bool,

    // review
    pub rows: Vec<RowItem>,
    pub visible: Vec<usize>,
    pub table_state: TableState,
    pub show_all: bool,
    pub sort: SortMode,

    // encode
    pub enc: Option<EncodeState>,
    pub control: Option<Arc<Control>>,
    pub coordinator: Option<JoinHandle<()>>,
    pub confirm: bool,
    pub report_path: Option<PathBuf>,
    /// Full run log (`log-YYYY-MM-DD-HHMM.txt` next to the exe).
    pub log: RunLog,

    // settings
    pub settings: bool,
    pub settings_idx: usize,
    pub edit: Option<String>,
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf, note: Option<String>, tx: Sender<Ev>) -> Self {
        Self {
            cfg,
            cfg_path,
            phase: Phase::Scanning,
            quit: false,
            toast: note.map(|n| (n, Instant::now())),
            tick: 0,
            tx,
            scan_total: 0,
            scan_done: 0,
            scan_current: String::new(),
            scan_errors: Vec::new(),
            scan_elapsed: 0.0,
            scanning: false,
            rows: Vec::new(),
            visible: Vec::new(),
            table_state: TableState::default(),
            show_all: false,
            sort: SortMode::Size,
            enc: None,
            control: None,
            coordinator: None,
            confirm: false,
            report_path: None,
            log: RunLog::disabled(),
            settings: false,
            settings_idx: 0,
            edit: None,
        }
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Attach the run log created by `main`. Safe to skip (tests do).
    pub fn attach_log(&mut self, log: RunLog) {
        if let Some(err) = log.error() {
            let err = err.to_string();
            self.toast(err);
        }
        self.log = log;
        let started = self.log.started().clock();
        self.log.line(&format!("window opened at {started}"));
    }

    pub fn log_path(&self) -> Option<&std::path::Path> {
        self.log.path()
    }

    pub fn toast_text(&self) -> Option<&str> {
        match &self.toast {
            Some((msg, at)) if at.elapsed() < Duration::from_secs(6) => Some(msg),
            _ => None,
        }
    }

    // ---------------------------------------------------------------- scanning

    pub fn start_scan(&mut self) {
        self.rows.clear();
        self.visible.clear();
        self.scan_errors.clear();
        self.scan_done = 0;
        self.scan_total = 0;
        self.scan_elapsed = 0.0;
        self.scanning = true;
        self.phase = Phase::Scanning;
        self.table_state = TableState::default();
        if !self.cfg.root.is_dir() {
            self.scanning = false;
            self.phase = Phase::Review;
            self.toast(format!("root folder not found: {}", self.cfg.root.display()));
            self.log.line(&format!("scan ABORTED: not a folder: {}", self.cfg.root.display()));
            return;
        }
        if !self.cfg.tools_ok() {
            self.scanning = false;
            self.phase = Phase::Review;
            self.toast("ffmpeg/ffprobe not found — check settings (t)");
            self.log.line("scan ABORTED: ffmpeg/ffprobe not found");
            return;
        }
        self.log.line(&format!("scan started: {}", self.cfg.root.display()));
        crate::scan::start(self.cfg.clone(), self.tx.clone());
    }

    // ------------------------------------------------------------------ events

    pub fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::ScanStart { total } => {
                self.scan_total = total;
                self.scanning = true;
            }
            Ev::ScanProgress { done, total, current } => {
                self.scan_done = done;
                self.scan_total = total;
                self.scan_current = current;
            }
            Ev::ScanFile(info) => {
                let info = *info;
                let candidate = is_candidate(&self.cfg, &info);
                self.rows.push(RowItem { info, candidate, selected: candidate, status: RowStatus::Pending });
            }
            Ev::ScanError { path, msg } => {
                if self.scan_errors.len() < 500 {
                    self.scan_errors.push(format!("{}: {msg}", path.display()));
                }
            }
            Ev::ScanDone { scanned, errors, elapsed_secs } => {
                self.scanning = false;
                self.scan_elapsed = elapsed_secs;
                self.refresh_visible();
                self.phase = Phase::Review;
                let cands = self.rows.iter().filter(|r| r.candidate).count();
                self.toast(format!("scanned {scanned} files in {:.1}s · {cands} candidates · {errors} unreadable", elapsed_secs));
                self.log.line(&format!("scan done: {scanned} files probed, {cands} candidates, {} total, {errors} unreadable ({elapsed_secs:.1}s)", crate::fmt::bytes(self.candidate_bytes())));
                for err in &self.scan_errors {
                    self.log.line(&format!("scan error: {err}"));
                }
            }
            Ev::JobStarted { id, path, duration_secs, size_bytes } => {
                if let Some(enc) = &mut self.enc {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if let Some(slot) = enc.slots.iter_mut().find(|s| s.id.is_none()) {
                        *slot = Slot { id: Some(id), name, frac: 0.0, speed: 0.0, out_bytes: 0, dur: duration_secs, src_bytes: size_bytes, started: Some(Instant::now()) };
                    }
                }
                if let Some(row) = self.rows.iter_mut().find(|r| r.info.path == path) {
                    row.status = RowStatus::Running;
                }
                self.log.line(&format!("start: {}", path.display()));
            }
            Ev::JobProgress { id, out_secs, speed, out_bytes } => {
                if let Some(slot) = self.enc.as_mut().and_then(|enc| enc.slots.iter_mut().find(|s| s.id == Some(id))) {
                    slot.frac = if slot.dur > 0.0 { (out_secs / slot.dur).clamp(0.0, 1.0) } else { 0.0 };
                    slot.speed = speed;
                    slot.out_bytes = out_bytes;
                }
            }
            Ev::JobFinished(res) => {
                let path = res.path.clone();
                let outcome = res.outcome;
                let id = encode::job_id(&path);
                if let Some(enc) = &mut self.enc {
                    enc.completed += 1;
                    if let Some(slot) = enc.slots.iter_mut().find(|s| s.id == Some(id)) {
                        *slot = Slot::empty();
                    }
                    enc.results.push(*res);
                }
                if let Some(row) = self.rows.iter_mut().find(|r| r.info.path == path) {
                    row.status = RowStatus::Done(outcome);
                }
                if let Some(res) = self.enc.as_ref().and_then(|e| e.results.last()) {
                    let copy = res.clone();
                    self.log.job_result(&copy);
                }
            }
            Ev::EncodeDone { elapsed_secs } => {
                if let Some(enc) = &mut self.enc {
                    enc.elapsed = elapsed_secs;
                    enc.finished = true;
                    enc.slots.iter_mut().for_each(|s| *s = Slot::empty());
                    let results = enc.results.clone();
                    let path = report::write(&results, &self.cfg);
                    self.report_path = Some(path);
                }
                self.log_encode_summary(elapsed_secs);
                self.phase = Phase::Summary;
            }
        }
    }

    fn log_encode_summary(&mut self, elapsed_secs: f64) {
        let Some(enc) = &self.enc else { return };
        let count = |o: Outcome| enc.results.iter().filter(|r| r.outcome == o).count();
        let ok: u64 = enc.results.iter().filter(|r| r.outcome.is_success()).map(|r| r.new_size).sum();
        let before: u64 = enc.results.iter().filter(|r| r.outcome.is_success()).map(|r| r.old_size).sum();
        let line = format!(
            "encode done: {} replaced, {} dry-run, {} rejected, {} failed, {} cancelled · {} -> {} (freed {}) · {:.1}s",
            count(Outcome::Replaced),
            count(Outcome::DryRun),
            count(Outcome::Rejected),
            count(Outcome::Failed),
            count(Outcome::Cancelled),
            crate::fmt::bytes(before),
            crate::fmt::bytes(ok),
            crate::fmt::bytes(before.saturating_sub(ok)),
            elapsed_secs
        );
        self.log.line(&line);
        if let Some(p) = &self.report_path {
            self.log.line(&format!("report written: {}", p.display()));
        }
        self.log.raw("\n");
    }

    // ------------------------------------------------------------------- input

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        if self.settings {
            self.on_key_settings(key);
            return;
        }
        if self.confirm {
            self.on_key_confirm(key);
            return;
        }
        match self.phase {
            Phase::Scanning => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('t') => self.open_settings(),
                _ => {}
            },
            Phase::Review => self.on_key_review(key),
            Phase::Encoding => self.on_key_encoding(key),
            Phase::Summary => self.on_key_summary(key),
        }
    }

    fn on_key_review(&mut self, key: KeyEvent) {
        let len = self.visible.len();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1, len),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1, len),
            KeyCode::PageDown => self.move_cursor(10, len),
            KeyCode::PageUp => self.move_cursor(-10, len),
            KeyCode::Home | KeyCode::Char('g') => {
                if len > 0 {
                    self.table_state.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
            }
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Char('a') => self.toggle_all(),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.refresh_visible();
                self.toast(format!("sorted by {}", self.sort.label()));
            }
            KeyCode::Char('f') => {
                self.show_all = !self.show_all;
                self.refresh_visible();
                self.toast(if self.show_all { "showing all scanned files" } else { "showing candidates only" });
            }
            KeyCode::Char('d') => {
                self.cfg.dry_run = !self.cfg.dry_run;
                self.toast(if self.cfg.dry_run { "dry run ON — originals will not be touched" } else { "dry run OFF — originals will be replaced" });
            }
            KeyCode::Char('r') => {
                self.start_scan();
                self.toast("rescanning…");
            }
            KeyCode::Char('t') => self.open_settings(),
            KeyCode::Enter | KeyCode::Char('e') => {
                let n = self.selected_count();
                if n == 0 {
                    self.toast("nothing selected — press space to select files");
                } else {
                    self.confirm = true;
                }
            }
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.confirm = false;
                self.start_encoding();
            }
            KeyCode::Esc | KeyCode::Char('q') => self.confirm = false,
            KeyCode::Char('d') => {
                self.cfg.dry_run = !self.cfg.dry_run;
            }
            _ => {}
        }
    }

    fn on_key_encoding(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('p') => {
                if let Some(control) = &self.control {
                    let paused = control.toggle_pause();
                    if let Some(enc) = &mut self.enc {
                        enc.paused = paused;
                    }
                    self.toast(if paused { "paused — running jobs finish, no new ones start" } else { "resumed" });
                }
            }
            KeyCode::Char('c') => {
                if let Some(control) = &self.control {
                    control.cancel();
                }
                if let Some(enc) = &mut self.enc {
                    enc.cancelling = true;
                }
                self.toast("cancelling — killing running encodes…");
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                if let Some(control) = &self.control {
                    control.cancel();
                }
                self.quit = true;
            }
            _ => {}
        }
    }

    fn on_key_summary(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Enter | KeyCode::Char('r') => {
                self.enc = None;
                self.control = None;
                self.phase = Phase::Review;
                self.start_scan();
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------------- settings

    fn open_settings(&mut self) {
        self.settings = true;
        self.settings_idx = 0;
        self.edit = None;
    }

    fn on_key_settings(&mut self, key: KeyEvent) {
        if let Some(buf) = &mut self.edit {
            match key.code {
                KeyCode::Enter => {
                    let value = buf.clone();
                    let field = Field::ALL[self.settings_idx];
                    match field.apply(&mut self.cfg, &value) {
                        Ok(()) => {
                            self.edit = None;
                            self.toast(format!("{} = {}", field.label(), field.value(&self.cfg)));
                        }
                        Err(e) => self.toast(format!("invalid value: {e}")),
                    }
                }
                KeyCode::Esc => self.edit = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.settings = false;
                let _ = self.cfg.save(&self.cfg_path);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_idx = (self.settings_idx + 1) % Field::ALL.len();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_idx = (self.settings_idx + Field::ALL.len() - 1) % Field::ALL.len();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let field = Field::ALL[self.settings_idx];
                if field.is_bool() {
                    field.toggle(&mut self.cfg);
                } else {
                    self.edit = Some(field.value(&self.cfg));
                }
            }
            KeyCode::Char('s') => {
                let _ = self.cfg.save(&self.cfg_path);
                self.settings = false;
                self.start_scan();
                self.toast("settings saved — rescanning");
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ helpers

    fn move_cursor(&mut self, delta: i64, len: usize) {
        if len == 0 {
            return;
        }
        let cur = self.table_state.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, len as i64 - 1);
        self.table_state.select(Some(next as usize));
    }

    fn toggle_current(&mut self) {
        let Some(sel) = self.table_state.selected() else {
            return;
        };
        let Some(&row_idx) = self.visible.get(sel) else {
            return;
        };
        let row = &mut self.rows[row_idx];
        if !row.candidate {
            self.toast("not a candidate (below threshold) — nothing to do");
            return;
        }
        row.selected = !row.selected;
    }

    pub fn toggle_all(&mut self) {
        let any_unselected = self.visible.iter().any(|&i| self.rows[i].candidate && !self.rows[i].selected);
        for &i in &self.visible {
            if self.rows[i].candidate {
                self.rows[i].selected = any_unselected;
            }
        }
        self.toast(if any_unselected { "selected all candidates" } else { "deselected all" });
    }

    pub fn selected_count(&self) -> usize {
        self.rows.iter().filter(|r| r.candidate && r.selected).count()
    }

    pub fn selected_bytes(&self) -> u64 {
        self.rows.iter().filter(|r| r.candidate && r.selected).map(|r| r.info.size_bytes).sum()
    }

    pub fn candidate_count(&self) -> usize {
        self.rows.iter().filter(|r| r.candidate).count()
    }

    pub fn candidate_bytes(&self) -> u64 {
        self.rows.iter().filter(|r| r.candidate).map(|r| r.info.size_bytes).sum()
    }

    pub fn refresh_visible(&mut self) {
        let mut idx: Vec<usize> = self.rows.iter().enumerate().filter(|(_, r)| self.show_all || r.candidate).map(|(i, _)| i).collect();
        let rows = &self.rows;
        match self.sort {
            SortMode::Size => idx.sort_by_key(|&i| std::cmp::Reverse(rows[i].info.size_bytes)),
            SortMode::Bitrate => idx.sort_by_key(|&i| std::cmp::Reverse(rows[i].info.bitrate_bps)),
            SortMode::Duration => {
                idx.sort_by(|&a, &b| rows[b].info.duration_secs.partial_cmp(&rows[a].info.duration_secs).unwrap_or(std::cmp::Ordering::Equal));
            }
            SortMode::Name => idx.sort_by(|&a, &b| rows[a].info.path.to_string_lossy().to_lowercase().cmp(&rows[b].info.path.to_string_lossy().to_lowercase())),
        }
        self.visible = idx;
        if self.visible.is_empty() {
            self.table_state.select(None);
        } else {
            let cur = self.table_state.selected().unwrap_or(0);
            self.table_state.select(Some(cur.min(self.visible.len() - 1)));
        }
    }

    /// The files that will be encoded, in the order they are shown in the
    /// table (so the current sort decides the processing order).
    pub fn selected_jobs(&self) -> Vec<MediaInfo> {
        self.visible.iter().filter(|&&i| self.rows[i].candidate && self.rows[i].selected).map(|&i| self.rows[i].info.clone()).collect()
    }

    fn start_encoding(&mut self) {
        let jobs = self.selected_jobs();
        if jobs.is_empty() {
            self.toast("nothing selected");
            return;
        }
        let total = jobs.len();
        let slots = (0..self.cfg.parallelism.max(1)).map(|_| Slot::empty()).collect();
        self.enc = Some(EncodeState { total, completed: 0, started: Instant::now(), slots, results: Vec::new(), paused: false, cancelling: false, elapsed: 0.0, finished: false });
        for row in self.rows.iter_mut() {
            if row.candidate && row.selected {
                row.status = RowStatus::Pending;
            }
        }
        let opts = EncodeOpts::from_config(&self.cfg);
        self.log.line(&format!("encode start: {total} files, {} workers, dry_run={}, keep_backup={}", self.cfg.parallelism, self.cfg.dry_run, self.cfg.keep_backup));
        let (control, handle) = encode::start(jobs, opts, self.tx.clone());
        self.control = Some(control);
        self.coordinator = Some(handle);
        self.phase = Phase::Encoding;
        self.toast(format!("encoding {total} files with {} workers{}", self.cfg.parallelism, if self.cfg.dry_run { " (dry run)" } else { "" }));
    }

    /// Called on exit: stop workers and wait for them so no ffmpeg is orphaned.
    pub fn shutdown(&mut self) {
        if let Some(control) = &self.control {
            control.cancel();
        }
        if let Some(handle) = self.coordinator.take() {
            let _ = handle.join();
        }
    }
}

pub fn is_candidate(cfg: &Config, info: &MediaInfo) -> bool {
    info.has_video && info.duration_secs > 0.0 && info.bitrate_bps > cfg.threshold_bps() && info.size_bytes >= cfg.min_size_bytes() && !(cfg.skip_av1 && info.is_av1())
}

// --------------------------------------------------------------------- settings

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Root,
    Threshold,
    MinSize,
    Crf,
    MaxBitrate,
    TwoPass,
    Preset,
    AudioKbps,
    AudioChannels,
    Parallel,
    ProbeParallel,
    Tolerance,
    SkipAv1,
    Faststart,
    DryRun,
    KeepBackup,
}

impl Field {
    pub const ALL: [Field; 16] = [
        Field::Root,
        Field::Threshold,
        Field::MinSize,
        Field::Crf,
        Field::MaxBitrate,
        Field::TwoPass,
        Field::Preset,
        Field::AudioKbps,
        Field::AudioChannels,
        Field::Parallel,
        Field::ProbeParallel,
        Field::Tolerance,
        Field::SkipAv1,
        Field::Faststart,
        Field::DryRun,
        Field::KeepBackup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Root => "Root folder",
            Field::Threshold => "Bitrate threshold (kbps)",
            Field::MinSize => "Min file size (MB)",
            Field::Crf => "AV1 CRF (quality)",
            Field::MaxBitrate => "Max output bitrate (kbps)",
            Field::TwoPass => "Two-pass VBR encode",
            Field::Preset => "SVT-AV1 preset (0-13)",
            Field::AudioKbps => "Audio bitrate (kbps)",
            Field::AudioChannels => "Audio channels",
            Field::Parallel => "Concurrent encodes",
            Field::ProbeParallel => "Concurrent probes",
            Field::Tolerance => "Duration tolerance (s)",
            Field::SkipAv1 => "Skip already-AV1 files",
            Field::Faststart => "mp4 faststart",
            Field::DryRun => "Dry run (no changes)",
            Field::KeepBackup => "Keep .vqc-backup originals",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Field::Root => "scanned recursively",
            Field::Threshold => "total bitrate above this = candidate",
            Field::MinSize => "smaller files are ignored",
            Field::Crf => "lower = better quality, bigger file",
            Field::MaxBitrate => "0 = no cap · crf mode only",
            Field::TwoPass => "vbr at 75% of thr; no crf",
            Field::Preset => "higher = faster, bigger file",
            Field::AudioKbps => "AAC target; 0 = copy as-is",
            Field::AudioChannels => "2 = stereo; unused when copying",
            Field::Parallel => "ffmpeg processes at once",
            Field::ProbeParallel => "ffprobe processes at once",
            Field::Tolerance => "max allowed length difference",
            Field::SkipAv1 => "re-encoding AV1 only loses quality",
            Field::Faststart => "moves moov atom to the front",
            Field::DryRun => "encode + verify, keep originals",
            Field::KeepBackup => "originals moved to .vqc-backup/",
        }
    }

    pub fn is_bool(self) -> bool {
        matches!(self, Field::SkipAv1 | Field::Faststart | Field::TwoPass | Field::DryRun | Field::KeepBackup)
    }

    pub fn value(self, cfg: &Config) -> String {
        match self {
            Field::Root => cfg.root.display().to_string(),
            Field::Threshold => cfg.bitrate_threshold_kbps.to_string(),
            Field::MinSize => format!("{}", cfg.min_size_mb),
            Field::Crf => cfg.crf.to_string(),
            Field::MaxBitrate => cfg.max_bitrate_kbps.to_string(),
            Field::TwoPass => yes_no(cfg.two_pass),
            Field::Preset => cfg.preset.to_string(),
            Field::AudioKbps => cfg.audio_bitrate_kbps.to_string(),
            Field::AudioChannels => cfg.audio_channels.to_string(),
            Field::Parallel => cfg.parallelism.to_string(),
            Field::ProbeParallel => cfg.probe_parallelism.to_string(),
            Field::Tolerance => format!("{}", cfg.duration_tolerance_secs),
            Field::SkipAv1 => yes_no(cfg.skip_av1),
            Field::Faststart => yes_no(cfg.faststart),
            Field::DryRun => yes_no(cfg.dry_run),
            Field::KeepBackup => yes_no(cfg.keep_backup),
        }
    }

    pub fn toggle(self, cfg: &mut Config) {
        match self {
            Field::SkipAv1 => cfg.skip_av1 = !cfg.skip_av1,
            Field::Faststart => cfg.faststart = !cfg.faststart,
            Field::TwoPass => cfg.two_pass = !cfg.two_pass,
            Field::DryRun => cfg.dry_run = !cfg.dry_run,
            Field::KeepBackup => cfg.keep_backup = !cfg.keep_backup,
            _ => {}
        }
    }

    pub fn apply(self, cfg: &mut Config, raw: &str) -> Result<(), String> {
        let s = raw.trim();
        match self {
            Field::Root => {
                let p = PathBuf::from(s);
                if !p.is_dir() {
                    return Err(format!("not a folder: {s}"));
                }
                cfg.root = p;
            }
            Field::Threshold => {
                cfg.bitrate_threshold_kbps = parse_num::<u32>(s, 100, 200_000)?;
            }
            Field::MinSize => {
                cfg.min_size_mb = parse_num::<f64>(s, 0.0, 1_000_000.0)?;
            }
            Field::Crf => cfg.crf = parse_num::<u8>(s, 0, 63)?,
            Field::MaxBitrate => cfg.max_bitrate_kbps = parse_num::<u32>(s, 0, 200_000)?,
            Field::Preset => cfg.preset = parse_num::<u8>(s, 0, 13)?,
            Field::AudioKbps => cfg.audio_bitrate_kbps = parse_num::<u32>(s, 0, 512)?,
            Field::AudioChannels => cfg.audio_channels = parse_num::<u16>(s, 1, 8)?,
            Field::Parallel => cfg.parallelism = parse_num::<usize>(s, 1, 32)?,
            Field::ProbeParallel => cfg.probe_parallelism = parse_num::<usize>(s, 1, 32)?,
            Field::Tolerance => cfg.duration_tolerance_secs = parse_num::<f64>(s, 0.0, 60.0)?,
            _ => {}
        }
        Ok(())
    }
}

fn parse_num<T: std::str::FromStr + PartialOrd + std::fmt::Display>(s: &str, min: T, max: T) -> Result<T, String> {
    let v: T = s.parse().map_err(|_| format!("not a number: {s}"))?;
    if v < min || v > max {
        return Err(format!("must be between {min} and {max}"));
    }
    Ok(v)
}

fn yes_no(v: bool) -> String {
    if v { "yes".into() } else { "no".into() }
}
