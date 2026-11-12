# vid-quality-controller

A terminal UI (Rust + ratatui) that finds videos whose bitrate is far higher than
their content needs, re-encodes them in place to AV1, and only keeps the result
when it is **verified** to be the same length and smaller on disk.

```powershell
cargo build --release
.\target\release\vid-quality-controller.exe
```

## Install

Prebuilt binaries are attached to every release:

| platform | file |
|---|---|
| Windows x64 | `vid-quality-controller-<ver>-windows-x64.zip` |
| Windows x32 | `vid-quality-controller-<ver>-windows-x86.zip` |
| macOS (Apple Silicon) | `vid-quality-controller-<ver>-macos-arm64.tar.gz` |
| macOS (Intel) | `vid-quality-controller-<ver>-macos-x64.tar.gz` |
| Linux (generic) | `vid-quality-controller-<ver>-linux-x64.tar.gz` |
| Debian / Ubuntu | `vid-quality-controller_<ver>_amd64.deb` |
| Arch Linux | `vid-quality-controller-<ver>-1-x86_64.pkg.tar.zst` |

```bash
sudo dpkg -i vid-quality-controller_0.1.0_amd64.deb   # Debian/Ubuntu
sudo pacman -U vid-quality-controller-0.1.0-1-x86_64.pkg.tar.zst   # Arch
```

`ffmpeg` (with `libsvtav1`) is required at runtime and is declared as a package
dependency: `apt install ffmpeg`, `pacman -S ffmpeg`, `brew install ffmpeg`.
The app finds it on `PATH` automatically.

## What it does

1. **Scan** — walks the target folder recursively and probes every video with
   `ffprobe` (in parallel) for bitrate, resolution, framerate, codec, duration.
2. **Review** — lists every file whose *total* bitrate is above the threshold
   (default 2000 kbit/s), sorted by size, with checkboxes. You pick what to
   process. `f` also shows files that are below the threshold.
3. **Encode** — runs N ffmpeg jobs in parallel with live per-job progress bars,
   overall progress (`done/total`), speed, ETA and bytes saved so far.
4. **Verify** — each result must still probe cleanly, keep a video stream, be the
   same length within tolerance, and be strictly smaller. Anything else is
   discarded and the original kept.
5. **Replace** — the original is renamed aside, the new file is moved into place,
   and the backup is deleted (or kept with `keep_backup`). Temp folders are always
   removed, even on failure.
6. **Report** — a before/after table plus `reports/vqc-<stamp>.csv` and `.txt`.
7. **Log** — a full run log next to the executable, `log-YYYY-MM-DD-HHMMSS.txt`
   (named after the start time). It records the config, every scan, every job
   with sizes/bitrates/durations/timings, and for failures the exit code, how
   far the encode got, the exact ffmpeg command line and the ffmpeg stderr.
   The path is shown on the summary screen.

## Encode preset (defaults, all configurable in-app with `t`)

| setting | value |
|---|---|
| video encoder | `libsvtav1` (SVT-AV1) |
| quality | `-crf 35` |
| speed preset | `-preset 6` (0 = slowest/best, 13 = fastest) |
| resolution / framerate | untouched — no scale, no fps filter |
| audio | AAC 160 kbit/s, 2 channels |
| container | mp4/m4v/mov stay mp4, mkv/webm stay mkv, everything else becomes mkv |

ffmpeg is used as the encoder backend (`C:\P\ffmpeg\bin`, auto-detected, also
found on `PATH`).

## Keys

| screen | keys |
|---|---|
| review | `↑↓`/`jk` move · `space` toggle · `a` all · `s` sort · `f` filter · `d` dry-run · `r` rescan · `t` settings · `enter` encode · `q` quit |
| confirm | `enter` start · `d` dry-run · `esc` cancel |
| encoding | `p` pause · `c` cancel · `q` quit (kills children) |
| summary | `enter` rescan · `q` quit |
| settings | `↑↓` field · `enter` edit/toggle · `s` save + rescan · `esc` close |

## CLI

```
--root <PATH>        folder to scan
--threshold <KBPS>   bitrate threshold (default 2000)
--min-size <MB>      ignore smaller files (default 20)
--preset <0-13>      SVT-AV1 preset (default 6)
--jobs <N>           concurrent encodes (default 1)
--dry-run            encode + verify, never touch originals
--scan-only          no TUI: list candidates and exit
--headless           no TUI: encode every candidate, print report
--config <PATH>      config file (default ./config.json)
```

`--scan-only` and `--headless` are handy for scripting and for sanity checks
before letting the TUI loose on a big library.

## Config file

`config.json` is created on first run next to the executable's working
directory and rewritten when you change settings in-app. Everything the TUI
exposes lives there, including `duration_tolerance_secs`, `keep_backup`,
`skip_av1` (re-encoding AV1 only loses quality), `probe_parallelism` and
`ffmpeg_extra_args`.

## Failures

A job that fails is reported in three places:

- the row in the terminal ends with the reason, e.g.
  `[failed] clip.mp4  kept 900M  ·  at 71% of 10:00 · Error while decoding stream #0:0: …`
- the summary table's status column repeats it,
- the run log gets the full detail block (exit code, position reached, command
  line, complete ffmpeg stderr).

A file that decodes only partially usually does **not** make ffmpeg exit non-zero;
it produces a short output, which the duration check then rejects
(`rejected · duration mismatch: 20.87s vs 60.00s`). Both cases leave the original
untouched and clean up the temp folder.

## Notes and caveats

- Audio is re-encoded (AAC 160 kbps stereo) and **only the first video stream and
  audio streams are mapped** — subtitle/data streams are dropped.
- SVT-AV1 requires dimensions aligned to its block size, so a 404 px wide source
  becomes 408 px. Normal resolutions (720p/1080p/4K) are unaffected.
- Already-AV1 files are skipped by default: re-encoding AV1 at CRF 35 mostly
  throws away quality for a modest size win.
- Replacing a file rewrites it in place; use `--dry-run` or `keep_backup` for a
  safety net on a first large run.
- Measured on a real sample (404x720, 26 s, 6.5 Mbit/s): 20.1 MiB → 1.5 MiB
  (−92.6%), SSIM 0.982 vs the source.

## Tests

```powershell
cargo test    # renders every screen through ratatui's TestBackend + candidate rules
```
