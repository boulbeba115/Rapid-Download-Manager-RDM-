# Rapid Download Manager (RDM)

A Windows download accelerator written in Rust, modelled on the workflow of
Internet Download Manager: dynamic file segmentation over up to 32
connections, resume, queues, a scheduler, categories and browser capture.

RDM is an independent implementation. It contains no IDM code or artwork.

- **Design and status:** [SPEC.md](SPEC.md) — §13 lists exactly what is done,
  how each part was verified, and what is still missing.
- **Engine:** [crates/rdm-core](crates/rdm-core) · **GUI:** [crates/rdm-app](crates/rdm-app) ·
  **CLI:** [crates/rdm-cli](crates/rdm-cli) · **Browser extension:** [extension](extension)

## Run

```powershell
cargo build --release
.\target\release\rdm.exe          # the application
.\target\release\rdmc.exe --help  # command-line downloader, benchmark, test server
```

Settings and the download list live in `%APPDATA%\RDM`.

### Command line (IDM-compatible switches)

```powershell
rdm.exe https://example.com/file.zip           # opens the Download File Info dialog
rdm.exe /d <url> /p D:\Downloads /f name.zip   # same, with folder and name
rdm.exe /n /d <url>                            # start without asking
rdm.exe /a /d <url>                            # add to the main queue, do not start
rdm.exe /s                                     # start the main queue
```

If RDM is already running, these are handed to the running window.

```powershell
rdmc get <url> -c auto            # download in the terminal; re-run to resume
rdmc get <url> -c 8 -l 2M -o D:\x # 8 connections, 2 MB/s limit
rdmc bench -s 256M -r 2M -c 16    # 1 vs 16 connections against a throttled local server
rdmc get <url> -v                 # show the engine's decisions
```

### Browser integration

1. Start RDM.
2. Open `chrome://extensions` (or `edge://extensions`), enable **Developer
   mode**, choose **Load unpacked** and select the [extension](extension) folder.
3. Downloads of the file types listed in *Options → File types* now open in
   RDM. Right-click any link → **Download with RDM**.

The extension talks to RDM on `127.0.0.1` only. Web pages cannot use that
interface: requests are accepted only from browser-extension origins.

## Build environment

Rust stable, target `x86_64-pc-windows-gnu`. No Visual Studio or MinGW install
is required, but that bare toolchain needs two adjustments, both in the
*user-level* `%USERPROFILE%\.cargo\config.toml`:

```toml
[target.x86_64-pc-windows-gnu]
rustflags = ["-C", "dlltool=C:/Users/<you>/.cargo/tools/llvm-dlltool.exe",
             "-C", "linker-flavor=ld.lld", "-C", "linker=rust-lld"]
```

- `llvm-dlltool.exe` is a copy of `llvm-ar.exe` from the `llvm-tools` rustup
  component (`rustup component add llvm-tools`); it is a multi-call binary and
  acts as dlltool under that name. The bundled GNU `dlltool` cannot run without
  an assembler that rustup does not ship.
- Linking must use Rust's bundled LLD. GNU `ld` mis-links LLVM-style import
  libraries and the resulting programs crash at start-up with an access violation.

With a full MSVC or MinGW-w64 toolchain installed, none of this is needed.

## Tests

```powershell
cargo test --workspace
```

46 tests: unit tests, 19 end-to-end engine tests against a fault-injecting
HTTP server (throttling, connection caps, rate limits, dropped connections,
changed files, missing range support), and manager tests (naming, persistence,
queues, removal).
