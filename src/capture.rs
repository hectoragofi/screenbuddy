//! Screen capture via GDI BitBlt, downscale + JPEG encode in memory.

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// Longest edge and pixel budget the image is reduced to, so the model sees
/// exactly the pixels we send (no server-side resize → coordinates stay exact).
const MAX_EDGE: u32 = 1568;
const MAX_PIXELS: u64 = 1_150_000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn from_win(r: RECT) -> Self {
        Self {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        }
    }
}

/// Monitor geometry in physical (virtual-screen) pixels.
#[derive(Clone, Copy, Debug)]
pub struct MonitorInfo {
    pub rect: Rect,
    pub work: Rect,
    pub scale: f32,
}

/// Raw BGRA pixels of one monitor.
pub struct RawShot {
    pub monitor: MonitorInfo,
    pub cursor: (i32, i32),
    pub bgra: Vec<u8>,
}

/// Encoded screenshot plus the mapping back to screen coordinates.
pub struct Capture {
    pub jpeg: Vec<u8>,
    pub img_w: u32,
    pub img_h: u32,
    /// Captured area in virtual-screen pixels (whole monitor or a region).
    pub src: Rect,
    pub monitor: MonitorInfo,
    pub cursor: (i32, i32),
}

impl Capture {
    /// Screenshot pixel → virtual-screen pixel.
    pub fn to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        let sx = self.src.w as f32 / self.img_w as f32;
        let sy = self.src.h as f32 / self.img_h as f32;
        (self.src.x as f32 + x * sx, self.src.y as f32 + y * sy)
    }

    pub fn length_scale(&self) -> f32 {
        self.src.w as f32 / self.img_w as f32
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn monitor_at(x: i32, y: i32) -> MonitorInfo {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        MonitorInfo {
            rect: Rect::from_win(mi.rcMonitor),
            work: Rect::from_win(mi.rcWork),
            scale: dx as f32 / 96.0,
        }
    }
}

/// Grabs the monitor under the cursor.
pub fn grab_monitor_under_cursor() -> anyhow::Result<RawShot> {
    let cursor = cursor_pos();
    let monitor = monitor_at(cursor.0, cursor.1);
    let r = monitor.rect;
    let bgra = unsafe { bitblt(r)? };
    Ok(RawShot {
        monitor,
        cursor,
        bgra,
    })
}

/// Grabs an arbitrary virtual-screen rectangle at native resolution. The
/// returned shot's `monitor.rect` is the rectangle itself, so `encode(&shot,
/// None)` encodes exactly that area.
pub fn grab_rect(r: Rect) -> anyhow::Result<RawShot> {
    let mut monitor = monitor_at(r.x + r.w / 2, r.y + r.h / 2);
    monitor.rect = r;
    let bgra = unsafe { bitblt(r)? };
    Ok(RawShot {
        monitor,
        cursor: cursor_pos(),
        bgra,
    })
}

/// Tiny grayscale thumbnail of a screen area (GDI does the averaging), used
/// for cheap change detection. Returns (width, height, luma).
pub fn thumb(r: Rect, max_edge: u32) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let s = (max_edge as f64 / r.w.max(r.h).max(1) as f64).min(1.0);
    let tw = ((r.w as f64 * s).round() as i32).max(1);
    let th = ((r.h as f64 * s).round() as i32).max(1);
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, tw, th);
        let old = SelectObject(mem, bmp.into());
        SetStretchBltMode(mem, HALFTONE);
        let ok = StretchBlt(mem, 0, 0, tw, th, Some(screen), r.x, r.y, r.w, r.h, SRCCOPY | CAPTUREBLT);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (tw * th * 4) as usize];
        SelectObject(mem, old);
        let lines = GetDIBits(mem, bmp, 0, th as u32, Some(buf.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !ok.as_bool() || lines == 0 {
            anyhow::bail!("thumbnail capture failed");
        }
        let luma = buf
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| ((p[2] as u32 * 77 + p[1] as u32 * 150 + p[0] as u32 * 29) >> 8) as u8)
            .collect();
        Ok((tw as u32, th as u32, luma))
    }
}

unsafe fn bitblt(r: Rect) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, r.w, r.h);
        let old = SelectObject(mem, bmp.into());
        let blt = BitBlt(
            mem,
            0,
            0,
            r.w,
            r.h,
            Some(screen),
            r.x,
            r.y,
            SRCCOPY | CAPTUREBLT,
        );
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: r.w,
                biHeight: -r.h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (r.w * r.h * 4) as usize];
        SelectObject(mem, old);
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            r.h as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        blt?;
        if lines == 0 {
            anyhow::bail!("GetDIBits failed");
        }
        Ok(buf)
    }
}

/// Crops (optional, in virtual-screen px), downsizes and JPEG-encodes a shot.
pub fn encode(shot: &RawShot, region: Option<Rect>) -> anyhow::Result<Capture> {
    let m = shot.monitor.rect;
    let src = match region {
        Some(r) => {
            let x0 = r.x.clamp(m.x, m.x + m.w - 1);
            let y0 = r.y.clamp(m.y, m.y + m.h - 1);
            let x1 = (r.x + r.w).clamp(x0 + 1, m.x + m.w);
            let y1 = (r.y + r.h).clamp(y0 + 1, m.y + m.h);
            Rect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            }
        }
        None => m,
    };
    // BGRA -> RGB for the cropped area.
    let mut rgb = Vec::with_capacity((src.w * src.h * 3) as usize);
    for row in 0..src.h {
        let start = (((src.y - m.y + row) * m.w + (src.x - m.x)) * 4) as usize;
        let line = &shot.bgra[start..start + (src.w * 4) as usize];
        for px in line.chunks_exact(4) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let img = image::RgbImage::from_raw(src.w as u32, src.h as u32, rgb)
        .ok_or_else(|| anyhow::anyhow!("bad buffer"))?;
    let (w, h) = fit(src.w as u32, src.h as u32);
    let img = if (w, h) != (img.width(), img.height()) {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut jpeg = Vec::with_capacity(256 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&img)?;
    Ok(Capture {
        jpeg,
        img_w: w,
        img_h: h,
        src,
        monitor: shot.monitor,
        cursor: shot.cursor,
    })
}

fn fit(w: u32, h: u32) -> (u32, u32) {
    let mut s = (MAX_EDGE as f64 / w.max(h) as f64).min(1.0);
    let px = (w as f64 * s) * (h as f64 * s);
    if px > MAX_PIXELS as f64 {
        s *= (MAX_PIXELS as f64 / px).sqrt();
    }
    (
        ((w as f64 * s).round() as u32).max(1),
        ((h as f64 * s).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn fit_limits() {
        let (w, h) = super::fit(3840, 2160);
        assert!(w <= 1568 && (w as u64 * h as u64) <= 1_150_000, "{w}x{h}");
        assert_eq!(super::fit(800, 600), (800, 600));
    }
}
