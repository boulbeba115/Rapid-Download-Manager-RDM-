# Rapid Download Manager (RDM) — Product & Engineering Specification

A native Windows download accelerator written in Rust, functionally modelled on
Internet Download Manager (IDM): same feature set, same window layout, same
workflow. RDM is an independent implementation. It does not use IDM's name,
icons, binaries or any other IDM asset.

Status legend used throughout: **M1…M6** = milestone in which the item ships
(see §12). Anything not yet built is tracked in §13, which is kept current.

---

## 1. Goals

1. **Maximum throughput.** Saturate the user's link on any server that allows
   parallel range requests; never be slower than a single browser connection on
   servers that do not.
2. **Never lose progress.** Every download is resumable after pause, crash,
   power loss, network change or reboot, with no corruption.
3. **IDM-equivalent features and workflow** (§3), with a GUI a long-time IDM
   user can operate without relearning anything (§8).
4. **Small and fast.** One self-contained `.exe`, no runtime to install, cold
   start under 300 ms, idle CPU ~0 %, UI stays at 60 fps with 10 000 list rows.

### Non-goals
- Circumventing DRM or paywalls, or downloading encrypted (Widevine/PlayReady) streams.
- BitTorrent / peer-to-peer.
- macOS / Linux packaging in v1 (the core is portable; only the shell is Windows-first).

---

## 2. Technology choices

| Concern | Choice | Reason |
|---|---|---|
| Language | Rust (stable, edition 2021) | No GC pauses, zero-cost async, memory safety in a network-facing program |
| Target | `x86_64-pc-windows-gnu` | Builds with rustup alone; no Visual Studio install required |
| Async runtime | `tokio` multi-thread | Thousands of sockets on a handful of threads |
| HTTP | `reqwest` on `hyper`, **HTTP/1.1 forced for segmented transfers** | See §5.7 — HTTP/2 multiplexing defeats parallelism |
| TLS | Windows SChannel via `native-tls` | OS certificate store, AES-NI, no C toolchain needed |
| GUI | `egui` / `eframe` (GPU-accelerated immediate mode) | Single binary, 60 fps tables, real OS windows for progress dialogs |
| Persistence | Atomic JSON files (`serde_json`, write-temp-then-rename) | Human-readable, crash-safe, no native DB dependency |
| Browser integration | WebExtension (MV3) → loopback HTTP API | No native-messaging host to register; works in Chrome/Edge/Firefox |

Constraint that follows from the target: **no dependency may need a C compiler.**

---

## 3. Feature matrix (IDM parity)

| # | IDM feature | RDM behaviour | Milestone |
|---|---|---|---|
| 1 | Dynamic file segmentation, up to 32 connections | §5.2–5.4 | M1 |
| 2 | Connection reuse without re-login | Keep-alive pool, §5.7 | M1 |
| 3 | Resume broken / paused downloads | §5.6 | M1 |
| 4 | Automatic retry on error | Exponential back-off with jitter, §5.5 | M1 |
| 5 | Speed limiter (global and per download) | Token bucket, §5.8 | M1 |
| 6 | Download categories with per-category folders | §6.3 | M2 |
| 7 | Main list with status, size, time left, rate | §8.1 | M2 |
| 8 | Add URL / Download File Info dialog | §8.2 | M2 |
| 9 | Per-download progress window with segment map | §8.3 | M2 |
| 10 | Download-complete dialog (Open / Open folder) | §8.4 | M2 |
| 11 | Queues (main + custom), N files at a time | §7 | M3 |
| 12 | Scheduler (start/stop time, days, on-finish actions) | §7.2 | M3 |
| 13 | Options dialog (all tabs) | §8.5 | M3 |
| 14 | Browser integration (capture downloads) | §9 | M4 |
| 15 | Command-line interface (`/d /p /f /q /n /a /s`) | §10 | M4 |
| 16 | Clipboard URL monitoring | §8.6 | M4 |
| 17 | System-tray icon, minimise to tray | §8.6 | M4 |
| 18 | Batch download (URL templates `file[001-100].zip`) | §8.7 | M4 |
| 19 | Proxy (HTTP/HTTPS/SOCKS5) and site logins | §5.9 | M3 |
| 20 | Refresh download address (expired links) | §8.7 | M4 |
| 21 | Import / export download list | §8.7 | M4 |
| 22 | Video stream capture (HLS `.m3u8`, DASH) | §11.1 | M5 |
| 23 | Site grabber / spider | §11.2 | M5 |
| 24 | FTP / FTPS downloads | §11.3 | M5 |
| 25 | Checksum verification (MD5 / SHA-1 / SHA-256) | §5.10 | M3 |
| 26 | Virus scan on completion (run configured scanner) | §8.5 | M5 |
| 27 | Sounds on events | §8.5 | M5 |
| 28 | Drag & drop of links onto the window | §8.6 | M4 |
| 29 | Installer, file-type association, auto-start | §11.4 | M6 |

---

## 4. Architecture

```
rdm/                       Cargo workspace
├─ crates/
│  ├─ rdm-core/            Engine library. No GUI code. Fully testable headless.
│  │   ├─ probe.rs         First request: size, range support, filename, validators
│  │   ├─ segment.rs       Segment table, split / steal logic
│  │   ├─ worker.rs        One connection: fetch → clip → buffer → writer
│  │   ├─ writer.rs        Dedicated disk thread, positional writes
│  │   ├─ task.rs          One download: supervisor, adaptive concurrency, state machine
│  │   ├─ limiter.rs       Token-bucket rate limiter
│  │   ├─ state.rs         `.rdmstate` resume file
│  │   ├─ manager.rs       All downloads, queues, scheduler, persistence, events
│  │   ├─ category.rs      Extension → category → folder
│  │   ├─ filename.rs      Content-Disposition / URL / MIME → safe file name
│  │   ├─ config.rs        Settings (serde)
│  │   └─ api.rs           Loopback HTTP API (browser extension, CLI forwarding)
│  ├─ rdm-app/             `rdm.exe` — GUI shell (eframe)
│  └─ rdm-cli/             `rdmc.exe` — headless client, used for benchmarks and CI
├─ extension/              WebExtension (Chrome / Edge / Firefox)
└─ SPEC.md
```

Rules:
- `rdm-core` exposes a `Manager` handle (cheaply cloneable) plus a snapshot API.
  The GUI never touches engine internals; it reads immutable snapshots and
  sends commands.
- The engine runs on its own tokio runtime; the GUI thread never blocks on I/O.
- The engine wakes the GUI (`request_repaint`) at most 10×/s while transfers
  are active and never when idle.

---

## 5. Download engine

### 5.1 Probe (zero wasted round-trips)
The first request is not `HEAD`. It is a real `GET` with `Range: bytes=0-`:

- `206` + `Content-Range: bytes 0-N/TOTAL` → ranges supported, size known. **The
  response body is kept and becomes the first segment's data stream**, so
  bytes start flowing on the very first round-trip.
- `200` → no range support. Continue as a single-connection download on that
  same response. Marked non-resumable unless the server later proves otherwise.
- Size unknown (chunked) → single connection, streamed until EOF.

Captured from the probe: final URL after redirects, `ETag`, `Last-Modified`,
`Content-Type`, `Content-Disposition`, `Accept-Ranges`.

### 5.2 Segment model
A download is a sorted, gap-free table of segments covering `[0, size)`.

```
Segment { start, end (exclusive, can shrink), pos (next byte to fetch),
          written (bytes durably handed to disk), owner (worker id or none) }
```

`pos` and `end` are guarded by one small per-segment lock so that a worker
advancing `pos` and a thief shrinking `end` can never overlap. `written` trails
`pos` and is the only value persisted for resume.

### 5.3 Dynamic segmentation (work stealing)
Initial plan: `n` equal segments, each at least `min_segment` (default 512 KiB).
Files smaller than `2 × min_segment` use one connection.

Whenever a worker has nothing to do it:
1. claims an **unowned** unfinished segment if one exists (after errors/resume);
2. otherwise picks the owned segment with the largest **estimated time
   remaining** (`remaining_bytes / worker_speed`, not just bytes — a big
   segment on a fast connection is a worse victim than a medium one on a slow
   connection), and splits it: the victim keeps the first half, the thief takes
   the second half as a new segment;
3. a split is refused when the victim's remainder is under
   `max(2 × min_segment, 1.5 s of the victim's current speed)` — opening a
   connection for less than a handshake's worth of data is a net loss.

This is repeated until the file is complete, so all connections stay busy
until the very end instead of finishing early and idling.

### 5.4 Adaptive connection count
Setting `connections` is either a fixed number (1–32) or **Auto** (default,
ceiling 32, IDM-compatible default of 8 as the starting point):

- Start with 8 workers (fewer if the file is small).
- After each change, give the new connections 0.75 s to ramp up, then measure
  throughput for 1.25 s. If the step bought ≥ 8 % more, double the workers (up
  to the ceiling) and repeat: 8 → 16 → 32 within about 4 s.
- If a step did not help, it is undone: the extra workers retire as they finish
  their segments, so RDM does not hold connections it has no use for.
- Growth is suspended while a speed limit is active (the measurement would be
  meaningless).

### 5.4a Server push-back (429 / 503 / connection caps)
Found necessary in testing against real CDNs; a naive retry loop gets the
client's IP blocked.

- The **first burst** of rejections teaches the ceiling: it drops to the number
  of connections the server actually accepted. Later rejections never shrink it
  further (they are rate limits or slots not yet freed, not a smaller cap).
- `Retry-After` is honoured (seconds or HTTP-date, capped at 120 s); without
  it the quiet period doubles from 1 s to 30 s per rejected round.
- After a rejection, new requests are issued **one at a time** and only when
  the quiet period is over; a segment is claimed only when it is that
  connection's turn, so waiting connections never sit on work.
- **Segment absorption:** a connection that is already streaming keeps going
  into the next untouched segment instead of needing a new request. Under
  pressure, requests are made open-ended over the run of untouched segments
  that follow, so the connections the server did accept can carry the whole
  download on their own if nothing else is admitted.
- Sites known to dislike parallel connections can be pinned to N connections
  in Options → Connection → Exceptions.

### 5.5 Failure handling
- Per-chunk stall timeout (default 30 s without a byte) and connect timeout (15 s).
- A failed worker releases its segment (`owner = none`); bytes already written stay.
- Retry delay `min(30 s, 0.5 s × 2^attempt)` ± 20 % jitter; the attempt counter
  resets whenever the download makes progress. Default limit: 20 consecutive
  failures → status **Error** (resumable manually).
- **Slow-connection eviction:** a worker running below 20 % of the median
  worker speed for 10 s while holding > 4 MiB is dropped and its segment
  re-requested on a fresh connection (escapes a congested route / bad CDN node).
- Mid-download `200` in reply to a range request, or a changed
  `Content-Range` total → the remote file changed; restart cleanly from zero.

### 5.6 Resume
- Data is written straight into its final position in `<name>.rdmpart`, sized
  up front (sparse where the filesystem allows). There is **no merge step** —
  completion is a rename, not a copy of the whole file.
- `<name>.rdmstate` (JSON) holds URL, validators, size and the segment table
  using `written` offsets. Rewritten atomically every 2 s while active and on
  every pause/stop.
- On resume every range request carries `If-Range: <ETag | Last-Modified>`, so
  a changed file is detected by the server and never spliced.
- After a crash at most the last unflushed buffer per connection is re-fetched.

### 5.7 Connection handling
- **HTTP/1.1 is forced when more than one connection is used.** HTTP/2 puts all
  streams on one TCP connection and therefore one congestion window; N
  independent TCP connections get N windows, which is the whole reason
  segmented downloading is faster on lossy or rate-shaped paths.
- Keep-alive pool per host: a worker that finishes its range exactly reuses its
  TCP/TLS session for the next segment — no handshake, congestion window still open.
- `TCP_NODELAY` on; redirects followed (max 10); cookies, referer, user-agent
  and arbitrary extra headers carried per download.
- **Honest User-Agent by default** (`RapidDownloadManager/<version>`). Claiming
  to be Chrome without Chrome's TLS/header fingerprint gets connections reset
  by bot filters (observed in testing). Downloads captured from a browser carry
  that browser's own User-Agent, which matches the cookies they come with.

### 5.8 Disk I/O
- One dedicated writer thread per active download receives `(offset, buffer)`
  over a bounded channel and performs positional writes (`seek_write`), so the
  async network threads never block on the disk and a slow disk applies
  back-pressure to the sockets instead of growing memory.
- Each worker coalesces network chunks into a 512 KiB buffer before handing it
  over (flushed early on a 500 ms timer for slow links), cutting write syscalls
  by ~30× compared with writing every TCP read.
- Rate limiting: hierarchical token buckets (global → per download), refilled
  continuously; workers await tokens before reading more, which lets TCP flow
  control slow the sender smoothly.

### 5.9 Network options
Proxy (system / none / manual HTTP, HTTPS, SOCKS5 with credentials), per-site
logins (HTTP Basic, matched by host), custom user-agent, per-download referer
and cookies (supplied by the browser extension).

### 5.10 Integrity
Optional expected hash (MD5 / SHA-1 / SHA-256) entered in the add dialog;
verified after completion on a blocking thread and shown in properties.
File modification time is set from `Last-Modified` (option).

---

## 6. Data model

### 6.1 Download record
`id, url, final_url, file_name, save_dir, category, queue_id, description,
referer, cookies, headers, size, downloaded, status, resumable, connections,
speed_limit, added_at, last_try_at, completed_at, error, expected_hash`

Status: `Queued | Connecting | Downloading | Paused | Complete | Error`.

### 6.2 Files on disk
`%APPDATA%\RDM\config.json`, `downloads.json`, `queues.json`, `rdm.log`.
All written via temp-file + atomic rename.

### 6.3 Categories (IDM defaults)
| Category | Extensions | Default folder |
|---|---|---|
| Compressed | zip rar 7z gz bz2 xz tar iso z arj ace lzh sit sitx sea | `Downloads\Compressed` |
| Documents | pdf doc docx xls xlsx ppt pptx pps txt rtf odt epub | `Downloads\Documents` |
| Music | mp3 wav wma flac aac m4a ogg aif ra mpa | `Downloads\Music` |
| Programs | exe msi msu apk dmg bin | `Downloads\Programs` |
| Video | mp4 mkv avi mov wmv flv webm mpg mpeg m4v 3gp rm rmvb asf ogv | `Downloads\Video` |
| General | everything else | `Downloads` |

Users can add, edit and remove categories.

---

## 7. Queues and scheduler

### 7.1 Queues
- Built-in **Main download queue**; users can create more.
- Per queue: ordered list of downloads, `max_concurrent` (1–10), running flag.
- Starting a queue starts the first `max_concurrent` unfinished items; when one
  finishes or errors, the next starts. Items can be reordered.
- **Download Later** in the add dialog puts the file in a queue without starting it.

### 7.2 Scheduler (per queue)
- Start: once at date/time, or daily / selected weekdays at a time.
- Optional stop time.
- Optional retry count per file before moving on.
- On completion: nothing | exit RDM | sleep | hibernate | shut down (with a
  60-second cancellable countdown).

---

## 8. User interface

Classic desktop look (light theme by default, dark optional), not a web-style UI.

### 8.1 Main window
```
┌──────────────────────────────────────────────────────────────────────────┐
│ Tasks  File  Downloads  View  Help                                       │
├──────────────────────────────────────────────────────────────────────────┤
│ [Add URL][Resume][Stop][Stop All][Delete][Delete Compl.][Options]        │
│ [Scheduler][Start Queue][Stop Queue]                                     │
├───────────────┬──────────────────────────────────────────────────────────┤
│ Categories    │ File Name      Q  Size   Status  Time left  Rate  Last try│
│ ▾ All         │ ubuntu.iso        4.1GB  37.2%   2m 10s  11.2MB/s  Oct 12 │
│    Compressed │ setup.exe         52MB   Complete                 Oct 11 │
│    Documents  │ …                                                        │
│    Music      │                                                          │
│    Programs   │                                                          │
│    Video      │                                                          │
│ ▸ Unfinished  │                                                          │
│ ▸ Finished    │                                                          │
│ ▸ Queues      │                                                          │
├───────────────┴──────────────────────────────────────────────────────────┤
│ 3 active · ↓ 23.4 MB/s · limiter off                                     │
└──────────────────────────────────────────────────────────────────────────┘
```
- Sortable, resizable columns; multi-select (Ctrl/Shift); double-click opens the
  progress window (active) or the file (complete).
- Context menu: Open, Open with, Open folder, Move/rename, Redownload, Resume,
  Stop, Remove, Move to queue, Delete from queue, Properties.
- Search box filters the list as you type.

### 8.2 Add URL → Download File Info
URL field (pre-filled from clipboard when it holds a link), optional login.
After the probe returns: URL, category (auto-detected, editable), Save As with
browse button, description, file size, resumable yes/no.
Buttons: **Download Later**, **Start Download**, **Cancel**.

### 8.3 Progress window (one real OS window per download)
Tabs **Download status** / **Speed Limiter** / **Options on completion**.
Shows URL, status, file size, downloaded (with %), transfer rate, time left,
resume capability; overall progress bar; **segment map** drawn from the live
segment table; connection table (N., Downloaded, Info).
Buttons: Show/Hide details, Pause/Resume, Cancel.

### 8.4 Download complete
Address, size, saved path; **Open**, **Open with…**, **Open folder**, **Close**;
"don't show this dialog again".

### 8.5 Options
Tabs: **General** (start with Windows, clipboard monitor, browser integration
status) · **File types** (extensions to capture, sites to ignore) · **Save to**
(category folders, temp dir) · **Downloads** (show dialogs, on-duplicate
behaviour, set file time) · **Connection** (connection type/speed, default max
connections 1–32 or Auto, per-site exceptions, timeouts, retries) · **Proxy** ·
**Site logins** · **Sounds** · **Scheduler/limits** (global speed limit).

### 8.6 Shell integration
Tray icon with menu (restore, add URL, start/stop main queue, speed limiter
toggle, exit); close-to-tray; clipboard monitoring for URLs whose extension is
in the capture list; drag & drop of links/text onto the window.

### 8.7 Tools
Batch download from a numeric/alphabetic template with preview; refresh
download address; import/export (`.json`, plain URL list); find; delete all
completed.

---

## 9. Browser integration

- `extension/` (Manifest V3): watches `downloads.onCreated`; when the file
  extension/MIME matches the capture list and the size exceeds a threshold, it
  cancels the browser's download and POSTs `{url, referer, cookies, userAgent,
  fileName}` to RDM. Adds a "Download with RDM" context-menu item for links.
  Holding **Alt** bypasses capture (IDM convention).
- RDM listens on `127.0.0.1:<port>` (loopback only). Requests must carry
  `Content-Type: application/json` and an `Origin` of `chrome-extension://…` or
  `moz-extension://…`; CORS pre-flights from ordinary web pages are refused, so
  a web page cannot drive RDM. Captured downloads always show the Download File
  Info dialog unless the user turned it off.
- If RDM is not running the extension lets the browser handle the download.

---

## 10. Command line

`rdm.exe [url]` and IDM-compatible switches, forwarded to the running instance
via the loopback API (single-instance application):

`/d URL` download · `/p path` save dir · `/f name` file name · `/q` quit when
done · `/n` silent (no dialogs) · `/a` add to queue without starting ·
`/s` start main queue · `/h` hang up → not applicable, ignored.

`rdmc.exe` is a headless client: `rdmc get <url> [-o file] [-c N] [--limit RATE]`
with a live progress line; used for benchmarks and automated tests.

---

## 11. Later milestones

1. **HLS/DASH**: parse master/media playlists, quality picker, concurrent
   segment fetch through the same worker pool, AES-128 decryption, concatenate
   to `.ts`/`.mp4`. Extension detects media requests and shows a "Download this
   video" panel. DRM-protected streams are out of scope.
2. **Site grabber**: crawl from a start URL with depth, domain and file-type
   filters; review list; enqueue.
3. **FTP/FTPS** with `REST` resume and parallel data connections.
4. **Packaging**: installer (per-user), executable icon and version resource,
   start with Windows, `rdm://` protocol handler, code signing.

---

## 12. Milestones and acceptance criteria

| M | Scope | Done when |
|---|---|---|
| **M1 Engine** | §5.1–5.8, `rdmc` | Integration tests pass against a local fault-injecting server: byte-exact output with 1/8/32 connections; pause→resume; kill→resume; mid-stream disconnects; no-range server; changed-file detection. Segmented download is ≥ 5× single-connection on a per-connection-throttled server. |
| **M2 GUI core** | §8.1–8.4, §6 | Add, watch, pause, resume, delete from the GUI; list survives restart. |
| **M3 Management** | §7, §8.5, §5.9, §5.10 | Queues run N at a time; scheduler fires; options persist; limiter holds within ±5 % of target. |
| **M4 Integration** | §9, §10, §8.6, §8.7 | Browser download is captured end-to-end; CLI switches work; tray works. |
| **M5 Media & grabber** | §11.1–11.3 | HLS VOD stream saved and playable. |
| **M6 Packaging** | §11.4 | Clean-machine install/uninstall. |

### Performance targets (measured, not assumed)
| Metric | Target | How |
|---|---|---|
| Throughput vs. single connection, per-connection-capped server | ≥ 0.9 × N × cap, up to link speed | Local throttled test server |
| Loopback raw throughput | ≥ 1 GB/s (disk permitting) | `rdmc` against local server, 1 GiB file |
| Engine CPU at 100 MB/s | < 1 core | Task Manager during benchmark |
| Memory per active download | < 40 MiB at 32 connections | Bounded by writer channel × buffer size |
| Resume loss after kill | ≤ 512 KiB × connections | Kill test |

A head-to-head against IDM itself requires an IDM licence on the same link and
server; until that is run, "matches IDM" is a design goal, not a measured claim.

---

## 13. Implementation status

_As of 2026-10-12, version 0.1.0. "Verified" names how it was checked;
anything not listed as verified has been written but not exercised._

| Milestone | State |
|---|---|
| M1 Engine | **Done** |
| M2 GUI core | **Done** |
| M3 Management | **Done** |
| M4 Integration | **Done** |
| M5 Media & grabber | **Partly done** — HLS and a single-page link grabber; no DASH, no FTP, no multi-level grabbing |
| M6 Packaging | **Done** except code signing |

### Verification suites
| Suite | What it is | Size |
|---|---|---|
| `cargo test --workspace` | Unit tests, engine tests against the fault-injecting server, manager tests, HLS tests, click-through GUI tests | 82 tests |
| `scripts/test-installer.ps1` | Silent install into a temp folder, run, uninstall | 34 checks |
| `scripts/test-integration.ps1` | Clipboard monitoring, the extension in a real Edge, unattended `/n /q` | 10 checks |

### Done and verified
| Item | How it was verified |
|---|---|
| Probe reuse, dynamic segmentation, work stealing, 1–32 connections | Engine tests; every output compared byte-for-byte |
| Pause / resume, crash / resume from checkpoint | Engine tests; plus a real process killed mid-download from a public server, resumed, SHA-256 identical to a `curl` copy |
| Retry with back-off, dropped connections, no-range and no-length servers, changed-file detection | Engine tests |
| Server connection caps, `429` rate limiting, `Retry-After`, segment absorption | Engine tests; plus two public hosts that rate-limit |
| Slow-connection eviction | Test: one of four connections throttled to 8 KB/s is replaced; download takes 7 s instead of minutes |
| Speed limiter | Test: 4 MiB at 1 MiB/s took 4 s ± 5 % |
| Auto connection count | Test and benchmark |
| Authenticating HTTP proxy | Test: every connection passes through a local proxy; a wrong password is reported as 407 |
| Site logins, per-site connection limits | Manager tests |
| Checksum verification (MD5 / SHA-1 / SHA-256) | Unit and manager tests (match, mismatch) |
| HLS streams: master playlist, best rendition, AES-128 (explicit and sequence IV), pause/resume, live refused | HLS tests against a playlist server |
| Link grabber | Unit tests on extraction; manager test against a served page |
| Import / export (JSON, text, `.url` shortcuts) | Unit and manager tests |
| Scheduler | Unit tests on the rule; a manager test where the queue starts itself at the scheduled minute |
| Queues, N files at a time, on-finish action event | Manager test |
| Changing a download's address keeps progress | Manager test |
| File naming, categories, unique names, list persistence, restart mid-download | Unit and manager tests |
| Add URL → Download File Info → start → complete → delete; stop / resume; search, sort, category filter, context menu; scheduler dialog; options saved; batch; paste a link; captured downloads wait for confirmation | GUI tests that click the real interface (`egui_kittest`) |
| Loopback API access rules, single-instance forwarding, `/d /p /f /n /a /s` | End-to-end checks against the release build |
| `/n /q` unattended download that exits when done | Integration script |
| Clipboard monitoring | Integration script: ordinary text and page links ignored, a file link picked up |
| Browser extension | Integration script: loaded into Microsoft Edge; a download created by the browser is handed to RDM and the browser's copy cancelled |
| Installer / uninstaller | Installer script: files, Apps & features entry, shortcuts, closing a running instance, clean removal |
| Tray: start hidden, restore, quit while hidden | Installer script (window visibility checked through Win32) |
| Icon and version information in the executables | Read back through Windows (`VersionInfo`, associated icon) |

### Measured (this machine, release build)
| Metric | Target | Measured |
|---|---|---|
| 16 connections vs 1, server capped at 2 MB/s per connection | ≥ 0.9 × N | **15.7×** (31.4 vs 2.0 MB/s) |
| Auto mode on the same server | — | 11.6× on a 96 MB file (reaches 16 connections in 2 s) |
| Raw loopback throughput incl. disk + fsync | ≥ 1 GB/s | **0.87–0.96 GB/s** — just under; the single disk-writer thread is the likely limit |
| Real internet (≈ 47 Mbit/s link) | — | 5.7 MB/s with 1, 4 or 8 connections: the link is the limit, so extra connections cannot help here |
| Engine CPU and memory under load | < 1 core, < 40 MiB | **not measured** |
| Head-to-head against IDM | parity | **not measured** — needs IDM on the same link and server |

### Written but not exercised
- Tray **menu items** and clicking the tray icon (the hide / restore / quit
  paths they call are tested; the icon itself has not been clicked by a test).
- Dropping files on the window (the import it calls is tested).
- Power actions after a queue (sleep / hibernate / shut down with a 60 s
  cancellable countdown) — deliberately not triggered on a development machine.
- SOCKS5 and HTTPS proxies (the HTTP proxy path is tested).
- The extension in Chrome (Chrome no longer loads unpacked extensions from the
  command line, so only Edge could be automated) and in Firefox (needs a
  manifest tweak).
- A real link click in the browser: the integration test starts the download
  through the browser's download API, which is the same path from
  `downloads.onCreated` onward.
- "Start with Windows" (the installer writes the `Run` entry; signing in again
  to see it start was not tested).

### Not implemented
- FTP / FTPS.
- DASH (`.mpd`) streams; a "download this video" panel in the browser.
- Multi-level site grabbing (the grabber reads one page).
- Code signing (Windows SmartScreen warns on first run).
- Remembering a learned per-host connection ceiling across sessions
  (per-site limits are set by hand).
- `/h` (hang up a modem connection) — obsolete, ignored.

### Known limitations
- Hosts that punish parallel connections (lower total speed with 2 than with 1)
  are handled correctly but not optimally; pin them to 1 connection in Options.
- Site-login and proxy passwords are stored unencrypted in `config.json`.
- The build targets `x86_64-pc-windows-gnu` and needs the linker setup in
  `README.md`; for the same reason the executable icon is stamped in after
  linking by `crates/xtask`.
