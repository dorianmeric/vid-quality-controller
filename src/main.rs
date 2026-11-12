mod app;
mod config;
mod encode;
mod event;
mod fmt;
mod headless;
mod probe;
mod report;
mod scan;
mod ui;
#[cfg(test)]
mod ui_tests;

use anyhow::Result;
use app::App;
use config::Config;
use crossbeam_channel::unbounded;
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<()> {
    let mut cfg_path = PathBuf::from(config::CONFIG_FILE);
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut headless = false;
    let mut scan_only = false;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "--config" if i + 1 < args.len() => {
                cfg_path = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--root" if i + 1 < args.len() => {
                overrides.push(("root".into(), args[i + 1].clone()));
                i += 2;
            }
            "--threshold" if i + 1 < args.len() => {
                overrides.push(("threshold".into(), args[i + 1].clone()));
                i += 2;
            }
            "--min-size" if i + 1 < args.len() => {
                overrides.push(("min_size".into(), args[i + 1].clone()));
                i += 2;
            }
            "--preset" if i + 1 < args.len() => {
                overrides.push(("preset".into(), args[i + 1].clone()));
                i += 2;
            }
            "--jobs" if i + 1 < args.len() => {
                overrides.push(("jobs".into(), args[i + 1].clone()));
                i += 2;
            }
            "--headless" => {
                headless = true;
                i += 1;
            }
            "--scan-only" => {
                headless = true;
                scan_only = true;
                i += 1;
            }
            "--dry-run" => {
                overrides.push(("dry_run".into(), "true".into()));
                i += 1;
            }
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
    }

    let (mut cfg, note) = Config::load_or_create(&cfg_path);
    for (key, value) in overrides {
        match key.as_str() {
            "root" => cfg.root = PathBuf::from(value),
            "threshold" => {
                if let Ok(v) = value.parse() {
                    cfg.bitrate_threshold_kbps = v;
                }
            }
            "min_size" => {
                if let Ok(v) = value.parse() {
                    cfg.min_size_mb = v;
                }
            }
            "preset" => {
                if let Ok(v) = value.parse() {
                    cfg.preset = v;
                }
            }
            "jobs" => {
                if let Ok(v) = value.parse() {
                    cfg.parallelism = v;
                }
            }
            "dry_run" => cfg.dry_run = true,
            _ => {}
        }
    }

    if headless {
        return headless::run(cfg, scan_only);
    }
    run(cfg, cfg_path, note)
}

fn run(cfg: Config, cfg_path: PathBuf, note: Option<String>) -> Result<()> {
    let (tx, rx) = unbounded();
    let mut app = App::new(cfg, cfg_path, note, tx);

    let mut terminal = ratatui::init();
    app.start_scan();

    let result = loop {
        if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
            break Err(e.into());
        }
        app.tick = app.tick.wrapping_add(1);

        while let Ok(ev) = rx.try_recv() {
            app.on_event(ev);
        }

        match crossterm::event::poll(Duration::from_millis(70)) {
            Ok(true) => match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => app.on_key(key),
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            },
            Ok(false) => {}
            Err(e) => break Err(e.into()),
        }

        if app.quit {
            break Ok(());
        }
    };

    app.shutdown();
    ratatui::restore();
    result
}

fn print_help() {
    println!(
        "vid-quality-controller — find over-bitrated videos and re-encode them in place

USAGE:
    vid-quality-controller [OPTIONS]

OPTIONS:
    --root <PATH>        folder to scan (default from config.json, e.g. E:\\Po)
    --threshold <KBPS>   bitrate threshold in kbit/s (default 2000)
    --min-size <MB>      ignore files smaller than this (default 20)
    --preset <0-13>      SVT-AV1 speed preset (default 6)
    --jobs <N>           concurrent encodes (default 4)
    --dry-run            encode and verify, but never touch the originals
    --headless           no TUI: scan, encode every candidate, print a report
    --scan-only          no TUI: scan and list candidates, then exit
    --config <PATH>      config file to use (default ./config.json)
    -h, --help           show this help

KEYS:
    review:   ↑↓/jk move · space toggle · a all · s sort · f filter · d dry-run
              r rescan · t settings · enter encode · q quit
    encoding: p pause · c cancel · q quit
    summary:  enter rescan · q quit

The encode preset is AV1 (SVT) CRF 35, resolution and framerate untouched,
audio re-encoded to AAC 160 kbit/s stereo. A result only replaces the original
when it probes cleanly, has the same length (within tolerance) and is smaller.
"
    );
}
