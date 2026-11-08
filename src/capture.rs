//! Screen capture via GDI BitBlt, downscale + JPEG encode in memory. Single
//! windows (also ones Apeiron parked off-screen or hid as a tab) are captured
//! with Windows Graphics Capture, falling back to `PrintWindow`.

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
    /// Apeiron windows on screen when this was taken, topmost first (empty
    /// without Apeiron), so drawings can be anchored to them.
    pub windows: Vec<crate::apeiron::Placed>,
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
    /// See `RawShot::windows`.
    pub windows: Vec<crate::apeiron::Placed>,
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

/// If Apeiron's overview (stage) is open, its thumbnails aren't the real
/// windows: drop it to 1:1 first (waits for it to settle, capped).
pub fn leave_stage_if_open() {
    if crate::apeiron::stage_open() && crate::apeiron::leave_stage().is_ok() {
        // The settled reply can race the compositor's last frame.
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}

/// Grabs the monitor under the cursor.
pub fn grab_monitor_under_cursor() -> anyhow::Result<RawShot> {
    leave_stage_if_open();
    let cursor = cursor_pos();
    let monitor = monitor_at(cursor.0, cursor.1);
    let r = monitor.rect;
    let bgra = unsafe { bitblt(r)? };
    Ok(RawShot {
        monitor,
        cursor,
        bgra,
        windows: crate::apeiron::placed(),
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
        windows: crate::apeiron::placed(),
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
        windows: shot.windows.clone(),
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

// ---------------------------------------------------------------- one window

/// Captures one window by handle, even when it is covered, parked off-screen
/// or hidden as an Apeiron tab: Windows Graphics Capture first, then
/// `PrintWindow(PW_RENDERFULLCONTENT)`. The shot's `monitor.rect` starts at the
/// window's visible frame and has the image's size, so `encode(&shot, None)`
/// encodes the whole window. Use for @window mentions and agents asking for a
/// specific window.
pub fn window(hwnd: isize) -> anyhow::Result<RawShot> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow};
    let h = HWND(hwnd as *mut _);
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            anyhow::bail!("that window is gone");
        }
        if IsIconic(h).as_bool() {
            anyhow::bail!("that window is minimized");
        }
    }
    let frame = frame_bounds(h);
    let (w, ht, bgra) = match wgc_on_worker(hwnd) {
        Ok(v) => v,
        Err(e) => {
            crate::config::log(&format!("graphics capture failed ({e:#}); using PrintWindow"));
            unsafe { print_window(h, frame)? }
        }
    };
    let mut monitor = monitor_at(frame.x + frame.w / 2, frame.y + frame.h / 2);
    monitor.rect = Rect { x: frame.x, y: frame.y, w, h: ht };
    Ok(RawShot {
        monitor,
        cursor: cursor_pos(),
        bgra,
        windows: Vec::new(),
    })
}

/// `window` + `encode`.
pub fn window_capture(hwnd: isize) -> anyhow::Result<Capture> {
    encode(&window(hwnd)?, None)
}

/// The visible frame (no invisible resize borders), in screen pixels.
fn frame_bounds(h: windows::Win32::Foundation::HWND) -> Rect {
    use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r = RECT::default();
    unsafe {
        let ok = DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, (&mut r as *mut RECT).cast(), std::mem::size_of::<RECT>() as u32);
        if ok.is_err() || r.right <= r.left {
            let _ = GetWindowRect(h, &mut r);
        }
    }
    Rect::from_win(r)
}

/// Runs Graphics Capture on its own MTA thread, so the caller's COM apartment
/// doesn't matter and a stuck capture can't hold it up for long.
fn wgc_on_worker(hwnd: isize) -> anyhow::Result<(i32, i32, Vec<u8>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(unsafe { wgc(hwnd) });
    });
    rx.recv_timeout(std::time::Duration::from_millis(1500))
        .map_err(|_| anyhow::anyhow!("graphics capture timed out"))?
}

unsafe fn wgc(hwnd: isize) -> anyhow::Result<(i32, i32, Vec<u8>)> {
    use std::time::{Duration, Instant};
    use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
    use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Win32::Foundation::{HMODULE, HWND};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::Interface;
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        if !GraphicsCaptureSession::IsSupported()? {
            anyhow::bail!("not supported on this system");
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForWindow(HWND(hwnd as *mut _))?;
        let size = item.Size()?;
        let (mut device, mut ctx) = (None, None);
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device: ID3D11Device = device.ok_or_else(|| anyhow::anyhow!("no D3D device"))?;
        let ctx: ID3D11DeviceContext = ctx.ok_or_else(|| anyhow::anyhow!("no D3D context"))?;
        let dxgi: IDXGIDevice = device.cast()?;
        let d3d: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 1, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(false);
        session.StartCapture()?;
        let deadline = Instant::now() + Duration::from_millis(1000);
        let frame = loop {
            if let Ok(f) = pool.TryGetNextFrame() {
                break Some(f);
            }
            if Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(8));
        };
        let result = (|| -> anyhow::Result<(i32, i32, Vec<u8>)> {
            let frame = frame.as_ref().ok_or_else(|| anyhow::anyhow!("no frame arrived"))?;
            let content = frame.ContentSize()?;
            let tex: ID3D11Texture2D = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            desc.MipLevels = 1;
            desc.ArraySize = 1;
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.ok_or_else(|| anyhow::anyhow!("no staging texture"))?;
            ctx.CopyResource(&staging, &tex);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            let w = content.Width.clamp(1, desc.Width as i32);
            let h = content.Height.clamp(1, desc.Height as i32);
            let mut buf = Vec::with_capacity((w * h * 4) as usize);
            for row in 0..h as usize {
                let line = std::slice::from_raw_parts((map.pData as *const u8).add(row * map.RowPitch as usize), w as usize * 4);
                buf.extend_from_slice(line);
            }
            ctx.Unmap(&staging, 0);
            Ok((w, h, buf))
        })();
        if let Some(f) = frame {
            let _ = f.Close();
        }
        let _ = session.Close();
        let _ = pool.Close();
        result
    }
}

/// `PrintWindow(PW_RENDERFULLCONTENT)` cropped to the visible frame.
unsafe fn print_window(h: windows::Win32::Foundation::HWND, frame: Rect) -> anyhow::Result<(i32, i32, Vec<u8>)> {
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, PW_RENDERFULLCONTENT};
    unsafe {
        let mut wr = RECT::default();
        GetWindowRect(h, &mut wr)?;
        let full = Rect::from_win(wr);
        if full.w <= 0 || full.h <= 0 {
            anyhow::bail!("the window has no size");
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, full.w, full.h);
        let old = SelectObject(mem, bmp.into());
        let ok = PrintWindow(h, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: full.w,
                biHeight: -full.h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (full.w * full.h * 4) as usize];
        SelectObject(mem, old);
        let lines = GetDIBits(mem, bmp, 0, full.h as u32, Some(buf.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !ok.as_bool() || lines == 0 {
            anyhow::bail!("PrintWindow failed");
        }
        let c = crop_to(full, frame);
        if c == full {
            return Ok((full.w, full.h, buf));
        }
        let mut out = Vec::with_capacity((c.w * c.h * 4) as usize);
        for row in 0..c.h {
            let start = (((c.y - full.y + row) * full.w + (c.x - full.x)) * 4) as usize;
            out.extend_from_slice(&buf[start..start + (c.w * 4) as usize]);
        }
        Ok((c.w, c.h, out))
    }
}

/// `inner` clipped to `outer` (falls back to `outer` if they don't overlap).
fn crop_to(outer: Rect, inner: Rect) -> Rect {
    let x0 = inner.x.max(outer.x);
    let y0 = inner.y.max(outer.y);
    let x1 = (inner.x + inner.w).min(outer.x + outer.w);
    let y1 = (inner.y + inner.h).min(outer.y + outer.h);
    if x1 <= x0 || y1 <= y0 {
        return outer;
    }
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fit_limits() {
        let (w, h) = super::fit(3840, 2160);
        assert!(w <= 1568 && (w as u64 * h as u64) <= 1_150_000, "{w}x{h}");
        assert_eq!(super::fit(800, 600), (800, 600));
    }

    #[test]
    fn crops_invisible_borders() {
        use super::Rect;
        let full = Rect { x: -30007, y: 0, w: 814, h: 607 };
        let frame = Rect { x: -30000, y: 0, w: 800, h: 600 };
        assert_eq!(super::crop_to(full, frame), frame);
        let far = Rect { x: 0, y: 0, w: 10, h: 10 };
        assert_eq!(super::crop_to(full, far), full);
    }
}
