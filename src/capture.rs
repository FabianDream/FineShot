use xcap::Monitor;

use crate::tools::PixelRect;

/// Per-monitor capture slice stored in native `0x00RRGGBB` XRGB format for instant
/// `softbuffer` blits and zero stitched bounding-box padding on multi-monitor layouts.
pub struct CapturedMonitor {
    /// OS physical top-left X coordinate (used to position the per-monitor window).
    pub screen_x: i32,
    /// OS physical top-left Y coordinate (used to position the per-monitor window).
    pub screen_y: i32,
    /// Rectangle in unified virtual-desktop coordinates (origin at `0, 0`).
    pub bounds: PixelRect,
    /// Native `0x00RRGGBB` pixels (`bounds.w * bounds.h`).
    pub pixels_xrgb: Vec<u32>,
}

/// Multi-monitor virtual desktop capture holding separate per-monitor XRGB buffers.
pub struct VirtualCapture {
    /// Virtual desktop bounding-box width in physical pixels.
    pub width: u32,
    /// Virtual desktop bounding-box height in physical pixels.
    pub height: u32,
    /// Per-monitor captures with zero dead-space padding between mismatched monitors.
    pub monitors: Vec<CapturedMonitor>,
}

impl VirtualCapture {
    /// Copies pixels inside `sel` (unified virtual-desktop coordinates) into a tightly packed
    /// `[R, G, B, 255]` `RgbaImage` buffer of size `sel.w * sel.h`.
    pub fn copy_rect_to_rgba(&self, sel: PixelRect, dst_rgba: &mut [u8]) {
        let sw = sel.w as usize;
        for mon in &self.monitors {
            let ix0 = sel.x.max(mon.bounds.x);
            let iy0 = sel.y.max(mon.bounds.y);
            let ix1 = sel.right().min(mon.bounds.right());
            let iy1 = sel.bottom().min(mon.bounds.bottom());
            if ix0 >= ix1 || iy0 >= iy1 {
                continue;
            }

            let mon_w = mon.bounds.w as usize;
            let cw = (ix1 - ix0) as usize;
            let src_x0 = (ix0 - mon.bounds.x) as usize;
            let dst_x0 = (ix0 - sel.x) as usize;

            for abs_y in iy0..iy1 {
                let src_y = (abs_y - mon.bounds.y) as usize;
                let dst_y = (abs_y - sel.y) as usize;
                let src_row = &mon.pixels_xrgb[src_y * mon_w + src_x0..];
                let dst_row = &mut dst_rgba[(dst_y * sw + dst_x0) * 4..];
                for col in 0..cw {
                    let px = src_row[col];
                    let dp = col * 4;
                    dst_row[dp] = ((px >> 16) & 0xFF) as u8;
                    dst_row[dp + 1] = ((px >> 8) & 0xFF) as u8;
                    dst_row[dp + 2] = (px & 0xFF) as u8;
                    dst_row[dp + 3] = 255;
                }
            }
        }
    }
}

struct MonitorMeta {
    monitor: Monitor,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// Enumerates all active monitors and captures each into its own `0x00RRGGBB` XRGB buffer.
pub fn capture_virtual_desktop() -> Result<VirtualCapture, String> {
    let raw_monitors =
        Monitor::all().map_err(|e| format!("Failed to enumerate monitors: {e}"))?;
    if raw_monitors.is_empty() {
        return Err("No monitors detected".to_string());
    }

    let mut metas = Vec::with_capacity(raw_monitors.len());
    for monitor in raw_monitors {
        let raw_x = monitor.x().map_err(|e| format!("Monitor x failed: {e}"))?;
        let raw_y = monitor.y().map_err(|e| format!("Monitor y failed: {e}"))?;
        let raw_w = monitor.width().map_err(|e| format!("Monitor width failed: {e}"))?;
        let raw_h = monitor.height().map_err(|e| format!("Monitor height failed: {e}"))?;

        #[cfg(target_os = "macos")]
        let (x, y, width, height) = {
            let sf = monitor.scale_factor().unwrap_or(1.0) as f64;
            (
                (raw_x as f64 * sf).round() as i32,
                (raw_y as f64 * sf).round() as i32,
                ((raw_w as f64 * sf).round() as u32).max(1),
                ((raw_h as f64 * sf).round() as u32).max(1),
            )
        };
        #[cfg(not(target_os = "macos"))]
        let (x, y, width, height) = (raw_x, raw_y, raw_w.max(1), raw_h.max(1));

        metas.push(MonitorMeta {
            monitor,
            x,
            y,
            width,
            height,
        });
    }

    let min_x = metas.iter().map(|m| m.x).min().unwrap_or(0);
    let min_y = metas.iter().map(|m| m.y).min().unwrap_or(0);
    let max_x = metas
        .iter()
        .map(|m| m.x + m.width as i32)
        .max()
        .unwrap_or(0);
    let max_y = metas
        .iter()
        .map(|m| m.y + m.height as i32)
        .max()
        .unwrap_or(0);

    let total_w = (max_x - min_x).max(1) as u32;
    let total_h = (max_y - min_y).max(1) as u32;

    let mut monitors = Vec::with_capacity(metas.len());
    for meta in metas {
        let image = meta
            .monitor
            .capture_image()
            .map_err(|e| format!("Monitor capture failed: {e}"))?;
        let w = image.width().max(1);
        let h = image.height().max(1);
        let raw = image.into_raw();
        let mut pixels_xrgb = Vec::with_capacity((w as usize) * (h as usize));
        for px in raw.chunks_exact(4) {
            pixels_xrgb.push(((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32));
        }

        monitors.push(CapturedMonitor {
            screen_x: meta.x,
            screen_y: meta.y,
            bounds: PixelRect {
                x: meta.x - min_x,
                y: meta.y - min_y,
                w: w as i32,
                h: h as i32,
            },
            pixels_xrgb,
        });
    }

    Ok(VirtualCapture {
        width: total_w,
        height: total_h,
        monitors,
    })
}
