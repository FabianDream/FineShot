use std::borrow::Cow;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use image::codecs::bmp::BmpEncoder;
use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::qoi::QoiEncoder;
use image::codecs::tga::TgaEncoder;
use image::codecs::tiff::TiffEncoder;
use image::codecs::webp::WebPEncoder;
use image::{ColorType, ImageEncoder, RgbaImage};

use crate::config::SaveFormat;

/// Copies an `RgbaImage` directly to the OS clipboard.
pub fn copy_to_clipboard(image: &RgbaImage) -> Result<(), String> {
    copy_rgba_to_clipboard(image.as_raw(), image.width(), image.height())
}

/// Copies a raw RGBA8 slice directly to the OS clipboard.
pub fn copy_rgba_to_clipboard(rgba: &[u8], w: u32, h: u32) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("Clipboard init failed: {e}"))?;
    let img_data = arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: Cow::Borrowed(rgba),
    };
    clipboard
        .set_image(img_data)
        .map_err(|e| format!("Clipboard copy failed: {e}"))
}

/// Converts packed `0x00RRGGBB` pixels directly to RGBA bytes and pushes to clipboard
/// without calling `composite_region` when no annotations exist on a full capture.
pub fn copy_xrgb_to_clipboard(xrgb: &[u32], w: u32, h: u32) -> Result<(), String> {
    let mut rgba = Vec::with_capacity(xrgb.len() * 4);
    for &px in xrgb {
        rgba.push(((px >> 16) & 0xFF) as u8);
        rgba.push(((px >> 8) & 0xFF) as u8);
        rgba.push((px & 0xFF) as u8);
        rgba.push(255);
    }
    copy_rgba_to_clipboard(&rgba, w, h)
}

/// Generates a timestamped default filename such as `FineShot_2026-11-26_17-30-05.png`.
pub fn default_filename(ext: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, min, sec) = unix_secs_to_utc_ymdhms(secs);
    format!("FineShot_{year:04}-{month:02}-{day:02}_{hour:02}-{min:02}-{sec:02}.{ext}")
}

/// Runs inside the isolated `--save-dialog <default_name> <default_ext>` child process so
/// Windows Explorer shell extensions never remain loaded in the background tray process.
pub fn run_save_dialog_subprocess(default_name: &str, default_ext: &str) {
    let default_format = SaveFormat::from_ext(default_ext).unwrap_or(SaveFormat::Png);
    let mut dialog = rfd::FileDialog::new()
        .set_title("Save Screenshot")
        .set_file_name(default_name)
        .add_filter(default_format.filter_name(), default_format.filter_exts());

    for &fmt in &SaveFormat::ALL {
        if fmt != default_format {
            dialog = dialog.add_filter(fmt.filter_name(), fmt.filter_exts());
        }
    }

    if let Some(mut path) = dialog.save_file() {
        if path.extension().is_none() {
            path.set_extension(default_format.ext());
        }
        println!("{}", path.to_string_lossy());
    }
}

/// Spawns `current_exe() --save-dialog <default_name> <default_ext>` to pick a destination
/// path without loading Explorer shell DLLs into the tray daemon process.
pub fn pick_save_path_isolated(default_format: SaveFormat) -> Result<Option<PathBuf>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let default_name = default_filename(default_format.ext());
    let output = Command::new(exe)
        .args(["--save-dialog", &default_name, default_format.ext()])
        .output()
        .map_err(|e| format!("Failed to launch save dialog process: {e}"))?;

    if !output.status.success() {
        return Err(format!("Save dialog process exited with {}", output.status));
    }

    let chosen = String::from_utf8_lossy(&output.stdout);
    let trimmed = chosen.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(trimmed)))
    }
}

/// Encodes and writes an `RgbaImage` to `path` based on its file extension.
pub fn save_image_to_path(
    image: &RgbaImage,
    path: &Path,
    fallback: SaveFormat,
) -> Result<(), String> {
    let (w, h) = image.dimensions();
    save_raw_to_path(image.as_raw(), w, h, ColorType::Rgba8, path, fallback)
}

fn rgba_to_rgb24(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((rgba.len() / 4) * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&[px[0], px[1], px[2]]);
    }
    rgb
}

fn rgb24_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((rgb.len() / 3) * 4);
    for px in rgb.chunks_exact(3) {
        rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    rgba
}

fn save_raw_to_path(
    raw: &[u8],
    w: u32,
    h: u32,
    color_type: ColorType,
    path: &Path,
    fallback: SaveFormat,
) -> Result<(), String> {
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(SaveFormat::from_ext)
        .unwrap_or(fallback);

    let file = File::create(path).map_err(|e| format!("Failed to create file: {e}"))?;
    let mut writer = BufWriter::with_capacity(64 * 1024, file);

    if format == SaveFormat::Gif {
        let owned_rgba;
        let rgba_slice = match color_type {
            ColorType::Rgba8 => raw,
            _ => {
                owned_rgba = rgb24_to_rgba(raw);
                &owned_rgba
            }
        };
        let mut encoder = GifEncoder::new(writer);
        encoder
            .encode(rgba_slice, w, h, ColorType::Rgba8.into())
            .map_err(|e| format!("GIF encoding failed: {e}"))?;
        return Ok(());
    }

    // All opaque screenshot formats strip alpha -> Rgb8 to cut encode CPU & file size by ~20%.
    let owned_rgb;
    let rgb_slice = match color_type {
        ColorType::Rgb8 => raw,
        _ => {
            owned_rgb = rgba_to_rgb24(raw);
            &owned_rgb
        }
    };

    match format {
        SaveFormat::Png => {
            let encoder =
                PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::Sub);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("PNG encoding failed: {e}"))?;
        }
        SaveFormat::Jpeg => {
            let encoder = JpegEncoder::new_with_quality(writer, 100);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("JPEG encoding failed: {e}"))?;
        }
        SaveFormat::Webp => {
            let encoder = WebPEncoder::new_lossless(writer);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("WebP encoding failed: {e}"))?;
        }
        SaveFormat::Bmp => {
            let encoder = BmpEncoder::new(&mut writer);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("BMP encoding failed: {e}"))?;
        }
        SaveFormat::Tiff => {
            let encoder = TiffEncoder::new(writer);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("TIFF encoding failed: {e}"))?;
        }
        SaveFormat::Qoi => {
            let encoder = QoiEncoder::new(writer);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("QOI encoding failed: {e}"))?;
        }
        SaveFormat::Tga => {
            let encoder = TgaEncoder::new(writer);
            encoder
                .write_image(rgb_slice, w, h, ColorType::Rgb8.into())
                .map_err(|e| format!("TGA encoding failed: {e}"))?;
        }
        SaveFormat::Gif => unreachable!(),
    }
    Ok(())
}

/// Converts Unix epoch seconds to (year, month, day, hour, minute, second) in UTC
/// using Howard Hinnant's civil-from-days algorithm without external date crates.
fn unix_secs_to_utc_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;

    (year, m as u32, d as u32, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn epoch_zero_formats_correctly() {
        assert_eq!(unix_secs_to_utc_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(
            unix_secs_to_utc_ymdhms(1_700_000_000),
            (2023, 11, 14, 22, 13, 20)
        );
    }

    #[test]
    fn default_filename_has_expected_prefix_and_extension() {
        let name = default_filename("webp");
        assert!(name.starts_with("FineShot_"));
        assert!(name.ends_with(".webp"));
    }

    #[test]
    fn encodes_all_supported_formats_to_disk() {
        let img = RgbaImage::from_pixel(16, 16, Rgba([40, 120, 200, 255]));
        let tmp = std::env::temp_dir().join("fineshot_format_tests");
        let _ = std::fs::create_dir_all(&tmp);

        for &fmt in &SaveFormat::ALL {
            let path = tmp.join(format!("test_out.{}", fmt.ext()));
            save_image_to_path(&img, &path, fmt)
                .unwrap_or_else(|e| panic!("Failed to encode {:?}: {e}", fmt));
            let meta = std::fs::metadata(&path).unwrap();
            assert!(meta.len() > 0, "Empty output for {:?}", fmt);
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn encodes_rgb24_formats_directly_without_rgba_conversion() {
        let mut rgb = Vec::with_capacity(16 * 16 * 3);
        for _ in 0..(16 * 16) {
            rgb.extend_from_slice(&[30, 140, 220]);
        }
        let tmp = std::env::temp_dir().join("fineshot_rgb24_tests");
        let _ = std::fs::create_dir_all(&tmp);

        for &fmt in &SaveFormat::ALL {
            let path = tmp.join(format!("test_rgb.{}", fmt.ext()));
            save_raw_to_path(&rgb, 16, 16, ColorType::Rgb8, &path, fmt)
                .unwrap_or_else(|e| panic!("Failed RGB24 encode {:?}: {e}", fmt));
            assert!(std::fs::metadata(&path).unwrap().len() > 0);
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir(&tmp);
    }
}
