<div align="center">

<img src="docs/icon.png" width="96" alt="Rapid Download Manager icon">

# Rapid Download Manager

**A fast, open-source download accelerator for Windows, written in Rust.**

Splits every file across up to 32 connections, resumes after anything short of a
dead disk, and stays out of your way.

[![Latest release](https://img.shields.io/github/v/release/boulbeba115/Rapid-Download-Manager-RDM-?label=release&color=1e7fd6)](https://github.com/boulbeba115/Rapid-Download-Manager-RDM-/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/boulbeba115/Rapid-Download-Manager-RDM-/total?color=2e9e4f)](https://github.com/boulbeba115/Rapid-Download-Manager-RDM-/releases)
![Platform](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011%20x64-0078d4)
![Rust](https://img.shields.io/badge/built%20with-Rust-b7410e)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

[**Download**](#download) · [Features](#features) · [How it gets its speed](#how-it-gets-its-speed) · [Usage](#usage) · [Build from source](#build-from-source)

<br>

<img src="docs/screenshots/main.png" width="900" alt="Main window with a download in progress">

</div>

---

## Download

| | File | For |
|---|---|---|
| **Installer** | [`RDM-x.y.z-Setup.exe`](https://github.com/boulbeba115/Rapid-Download-Manager-RDM-/releases/latest) | Most people. Installs for your user only — **no administrator rights needed** — with Start-menu entry and a clean uninstaller. |
| **Portable** | [`RDM-x.y.z-portable-win64.zip`](https://github.com/boulbeba115/Rapid-Download-Manager-RDM-/releases/latest) | Unzip anywhere (a USB stick is fine). Settings stay in a `data` folder next to the program. |

Requires 64-bit Windows 10 or 11. Nothing else to install: one ~10 MB executable, no runtime.

> **First-run notice.** The builds are not code-signed yet, so Windows SmartScreen
> may show *"Windows protected your PC"*. Choose **More info → Run anyway**, or
> verify the file against `SHA256SUMS.txt` on the release page first.

## Features

**Speed**
- Dynamic file segmentation over **1–32 connections**, with idle connections taking work from busy ones
- **Auto** mode starts at 8 connections and keeps adding while it measurably helps
- Reuses the very first response instead of throwing it away, and reuses connections between segments
- Replaces a connection that is crawling far below the others

**Reliability**
- **Pause and resume** — including after a crash, power cut or reboot
- Detects a file that changed on the server and never splices old and new data together
- Automatic retry with back-off; honours `429` / `Retry-After` and adapts to per-site connection limits instead of hammering them
- Optional **MD5 / SHA-1 / SHA-256** verification

**Everyday use**
- Categories with their own folders (Compressed, Documents, Music, Programs, Video)
- **Queues** (run N files at a time) and a **scheduler** (start/stop times, weekdays, shut down when done)
- Global and per-download **speed limits**
- **Browser integration** for Chrome and Edge: downloads open in RDM, with cookies and referrer
- **Video streams**: paste an HLS (`.m3u8`) link and get one joined file, AES-128 handled
- **Link grabber**: list and pick the files a web page links to
- Batch downloads (`file*.zip`, 1–100), clipboard monitoring, drag & drop of link lists, import / export
- Notification-area icon, start with Windows, dark theme
- Proxy support (HTTP, HTTPS, SOCKS5), site logins, per-site connection limits
- IDM-compatible command line, plus `rdmc` for scripts and terminals

## How it gets its speed

Most servers cap each connection, not each visitor. RDM asks for the file in
several ranges at once and — the important part — keeps every connection busy
until the last byte:

1. **No wasted first request.** The request that finds out the file's size is a
   real download request; its response becomes the first segment's data.
2. **Work stealing.** When a connection finishes its part, it takes the back half
   of whichever remaining part will take the *longest* (measured by speed, not
   just size). Nothing sits idle while another connection still has work.
3. **Straight to its final place.** Data is written at its final offset in one
   pre-sized file. Finishing is a rename, not a second pass that merges parts.
4. **Polite under pressure.** If a server refuses extra connections, RDM learns
   the limit from the first refusal, waits as long as the server asks, and lets
   the connections that *were* accepted carry on through the rest of the file.

Measured on a release build (every result verified byte-for-byte):

| Scenario | 1 connection | RDM | |
|---|---|---|---|
| Server capped at 2 MB/s per connection, 16 connections | 2.0 MB/s | **31.4 MB/s** | 15.7× |
| Same server, Auto mode | 2.0 MB/s | 23.1 MB/s | 11.6× |
| Loopback, no cap (includes writing to disk) | 0.87 GB/s | 0.96 GB/s | disk-bound |

On a line that one connection already fills, more connections cannot add speed —
RDM then simply matches it. No side-by-side measurement against other download
managers has been made.

## Screenshots

| Download File Info | Progress and segment map |
|---|---|
| <img src="docs/screenshots/download-file-info.png" alt="Download File Info dialog"> | <img src="docs/screenshots/progress.png" alt="Progress window showing 16 connections"> |
| **Options** | **Dark theme** |
| <img src="docs/screenshots/options.png" alt="Options dialog"> | <img src="docs/screenshots/dark.png" alt="Main window in the dark theme"> |

## Usage

### Start a download

Click **Add URL**, paste the address, press **OK**. RDM asks the server about
the file, suggests a category and folder, and waits for **Start Download** or
**Download Later** (which puts it in the main queue). Copying a link and pressing
`Ctrl+V` in the window does the same.

### Browser integration (Chrome, Edge)

1. Open `chrome://extensions` or `edge://extensions` and switch on **Developer mode**.
2. Choose **Load unpacked** and select the `extension` folder — it is installed
   next to `rdm.exe` (or inside the portable zip).
3. Downloads of the file types listed under *Options → File types* now open in
   RDM. Right-click any link → **Download with RDM**.

The extension talks to RDM on `127.0.0.1` only, and RDM accepts those requests
only from browser extensions — a web page cannot use that channel. If RDM is not
running, the browser downloads the file itself as usual.

### Video streams

Paste the address of an HLS playlist (`.m3u8`). RDM picks the best quality,
downloads the segments in parallel and joins them into one `.ts` (or `.mp4`)
file. Live streams and DRM-protected streams are not supported.

### Command line

```powershell
rdm.exe https://example.com/file.zip           # opens the Download File Info dialog
rdm.exe /d <url> /p D:\Downloads /f name.zip   # the same, with folder and name
rdm.exe /n /d <url>                            # start without asking
rdm.exe /n /q /d <url>                         # …and exit when it has finished
rdm.exe /a /d <url>                            # add to the main queue, do not start
rdm.exe /s                                     # start the main queue
rdm.exe --minimized                            # start in the notification area
```

If RDM is already running, these are handed to the running window.

`rdmc` is the same engine without a window:

```powershell
rdmc get <url>                      # Auto connections; run again to resume
rdmc get <url> -c 8 -l 2M -o D:\x   # 8 connections, 2 MB/s limit, save to D:\x
rdmc get <url> -v                   # show what the engine decides and why
rdmc bench -s 256M -r 2M -c 16      # 1 vs 16 connections against a throttled local server
```

### Where things are stored

| | Installed | Portable |
|---|---|---|
| Settings, download list, `rdm.log` | `%APPDATA%\RDM` | `data\` next to `rdm.exe` |
| Downloads | your *Downloads* folder, one sub-folder per category (changeable in *Options → Save to*) | same |

## Build from source

```powershell
git clone https://github.com/boulbeba115/Rapid-Download-Manager-RDM-.git
cd Rapid-Download-Manager-RDM-
cargo build --release
.\target\release\rdm.exe
```

Any recent stable Rust works with the MSVC or a full MinGW-w64 toolchain.

<details>
<summary><b>Building with the bare <code>x86_64-pc-windows-gnu</code> toolchain (no Visual Studio, no MinGW)</b></summary>

<br>

That is how this project is developed. The bare toolchain needs two settings in
the user-level `%USERPROFILE%\.cargo\config.toml`:

```toml
[target.x86_64-pc-windows-gnu]
rustflags = ["-C", "dlltool=C:/Users/<you>/.cargo/tools/llvm-dlltool.exe",
             "-C", "linker-flavor=ld.lld", "-C", "linker=rust-lld"]
```

- `llvm-dlltool.exe` is a copy of `llvm-ar.exe` from `rustup component add llvm-tools`
  (a multi-call binary). The bundled GNU `dlltool` needs an assembler rustup does not ship.
- Linking must use Rust's bundled LLD. GNU `ld` mis-links LLVM-style import
  libraries; the resulting programs crash at start-up.

For the same reason no dependency may need a C compiler, and the icon and
version information are written into the finished executables by `crates/xtask`
rather than by a resource compiler.

</details>

### Tests

```powershell
cargo test --workspace             # 82 tests
.\scripts\package.ps1              # build the installer and the portable zip into dist\
.\scripts\test-installer.ps1       # install, run, uninstall — 34 checks
.\scripts\test-integration.ps1     # clipboard monitoring + the extension in a real Edge
```

The Rust tests run the engine against a local HTTP server that misbehaves on
purpose (throttling, connection caps, rate limits, dropped connections, changed
files, missing range support, an authenticating proxy), download HLS streams
with encrypted segments, and click through the real interface with
[egui_kittest](https://crates.io/crates/egui_kittest).

### Packaging

`scripts\package.ps1` needs [NSIS 3](https://nsis.sourceforge.io) — either on
`PATH`, or unzipped under `tools\`.

### Project layout

| Path | What |
|---|---|
| [`crates/rdm-core`](crates/rdm-core) | The engine: segmentation, resume, HLS, queues, scheduler, the local API. No GUI code. |
| [`crates/rdm-app`](crates/rdm-app) | The desktop application ([egui](https://github.com/emilk/egui)). |
| [`crates/rdm-cli`](crates/rdm-cli) | `rdmc`: terminal downloader, benchmark, test server. |
| [`crates/xtask`](crates/xtask) | Build helper: icon generation, stamping executables. |
| [`extension`](extension) | Browser extension (Manifest V3). |
| [`installer`](installer) | NSIS installer script. |
| [`SPEC.md`](SPEC.md) | Design, and an exact account of what is implemented and how it was verified. |

## Status and roadmap

Version 0.1 is a first release. It is tested as described above, but it has not
had many real-world users yet — please [open an issue](https://github.com/boulbeba115/Rapid-Download-Manager-RDM-/issues)
when something misbehaves, ideally with the relevant lines from `rdm.log`.

Not there yet:

- FTP downloads
- DASH (`.mpd`) streams; a "download this video" button in the browser
- Multi-level site grabbing (the link grabber works on one page)
- Firefox packaging of the extension, and store listings for Chrome / Edge
- Code signing
- Remembering a site's connection limit between sessions (set it by hand under *Options → Connection* for now)

## License

[MIT](LICENSE).

Rapid Download Manager is an independent project. It is not affiliated with,
and contains no code or artwork from, Internet Download Manager or Tonec Inc.
