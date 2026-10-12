//! Packaging helper.
//!
//! ```text
//! cargo run -p xtask -- icon  <out.ico>            write the application icon
//! cargo run -p xtask -- png   <out.png> <size>     write the icon as a PNG
//! cargo run -p xtask -- stamp <exe> <description>  embed icon + version info into an .exe
//! ```
//!
//! The GNU toolchain this project builds with has no resource compiler, so
//! instead of linking a `.res` the resources are written into the finished
//! executable with the Windows resource-update API.

#[path = "../../rdm-app/src/icon.rs"]
#[allow(dead_code)]
mod icon;

use std::path::Path;

const ICON_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];
const VERSION: &str = env!("CARGO_PKG_VERSION");
const PRODUCT: &str = "Rapid Download Manager";
const COPYRIGHT: &str = "Copyright (c) 2026 boulbeba115. MIT License.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["icon", out] => std::fs::write(out, ico_file()).map_err(|e| e.to_string()),
        ["png", out, size] => size.parse::<u32>().map_err(|e| e.to_string()).and_then(|s| write_png(Path::new(out), s)),
        ["stamp", exe, description] => stamp(Path::new(exe), description),
        _ => Err("usage: xtask icon <out.ico> | png <out.png> <size> | stamp <exe> <description>".into()),
    };
    if let Err(e) = result {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

fn write_png(out: &Path, size: u32) -> Result<(), String> {
    image::save_buffer(out, &icon::render(size), size, size, image::ColorType::Rgba8).map_err(|e| e.to_string())
}

fn png_bytes(size: u32) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&icon::render(size), size, size, image::ExtendedColorType::Rgba8)
        .expect("encoding a PNG into memory cannot fail");
    out
}

/// One icon image in the classic DIB form: header, bottom-up BGRA pixels, 1-bit mask.
fn dib_bytes(size: u32) -> Vec<u8> {
    let rgba = icon::render(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // colour + mask
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(size * size * 4 + mask_row * size).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution, palette
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // Transparency comes from the alpha channel; the mask stays empty.
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}

/// The images that make up the icon: `(size, bytes)`. Large sizes are PNG-compressed.
fn icon_images() -> Vec<(u32, Vec<u8>)> {
    ICON_SIZES.iter().map(|&s| (s, if s >= 256 { png_bytes(s) } else { dib_bytes(s) })).collect()
}

fn ico_file() -> Vec<u8> {
    let images = icon_images();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// `RT_GROUP_ICON` data: the directory that points at the `RT_ICON` resources 1..=n.
fn group_icon(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, (size, data)) in images.iter().enumerate() {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u16 + 1).to_le_bytes());
    }
    out
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// One node of a version resource: header, key, value, then children, each 32-bit aligned.
fn node(key: &str, value: &[u8], value_len: u16, is_text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0u8; 2];
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&(is_text as u16).to_le_bytes());
    out.extend_from_slice(&utf16z(key));
    pad4(&mut out);
    out.extend_from_slice(value);
    for child in children {
        pad4(&mut out);
        out.extend_from_slice(child);
    }
    let len = out.len() as u16;
    out[..2].copy_from_slice(&len.to_le_bytes());
    out
}

fn text(key: &str, value: &str) -> Vec<u8> {
    let v = utf16z(value);
    node(key, &v, (v.len() / 2) as u16, true, &[])
}

/// `RT_VERSION` data (`VS_VERSIONINFO`): what Explorer shows under Properties, Details tab.
fn version_info(description: &str, file_name: &str) -> Vec<u8> {
    let mut parts = VERSION.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let (ms, ls) = ((major << 16) | minor, patch << 16);
    let mut fixed = Vec::new();
    for v in [0xFEEF_04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&v.to_le_bytes());
    }
    let strings = node(
        "040904B0",
        &[],
        0,
        true,
        &[
            text("CompanyName", "boulbeba115"),
            text("FileDescription", description),
            text("FileVersion", VERSION),
            text("InternalName", file_name),
            text("LegalCopyright", COPYRIGHT),
            text("OriginalFilename", file_name),
            text("ProductName", PRODUCT),
            text("ProductVersion", VERSION),
        ],
    );
    let string_file_info = node("StringFileInfo", &[], 0, true, &[strings]);
    let translation = node("Translation", &[0x09, 0x04, 0xB0, 0x04], 4, false, &[]);
    let var_file_info = node("VarFileInfo", &[], 0, true, &[translation]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[string_file_info, var_file_info])
}

#[cfg(windows)]
fn stamp(exe: &Path, description: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    const RT_ICON: usize = 3;
    const RT_GROUP_ICON: usize = 14;
    const RT_VERSION: usize = 16;
    const LANG_EN_US: u16 = 0x0409;

    let file_name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let images = icon_images();
    let wide: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let last_error = || std::io::Error::last_os_error().to_string();

    // SAFETY: plain Win32 calls with pointers to live, correctly sized buffers;
    // integer resource types/ids are passed the way MAKEINTRESOURCE does.
    unsafe {
        let handle = BeginUpdateResourceW(wide.as_ptr(), 0);
        if handle.is_null() {
            return Err(format!("cannot open {} for resource update: {}", exe.display(), last_error()));
        }
        let put = |kind: usize, id: usize, data: &[u8]| -> Result<(), String> {
            let ok = UpdateResourceW(handle, kind as *const u16, id as *const u16, LANG_EN_US, data.as_ptr() as *const _, data.len() as u32);
            if ok == 0 {
                Err(format!("UpdateResource({kind}, {id}) failed: {}", last_error()))
            } else {
                Ok(())
            }
        };
        let result = (|| {
            for (i, (_, data)) in images.iter().enumerate() {
                put(RT_ICON, i + 1, data)?;
            }
            put(RT_GROUP_ICON, 1, &group_icon(&images))?;
            put(RT_VERSION, 1, &version_info(description, &file_name))
        })();
        // Discard everything if any step failed, so a half-stamped file is never written.
        let discard = result.is_err() as i32;
        if EndUpdateResourceW(handle, discard) == 0 && result.is_ok() {
            return Err(format!("cannot write resources to {}: {}", exe.display(), last_error()));
        }
        result
    }
}

#[cfg(not(windows))]
fn stamp(_exe: &Path, _description: &str) -> Result<(), String> {
    Err("stamping executables is only supported on Windows".into())
}
