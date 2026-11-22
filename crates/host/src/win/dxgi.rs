//! Screen capture with DXGI Desktop Duplication: the GPU hands over the desktop image and
//! which parts of it changed since the last look, so only those are converted (GDI copied
//! and converted the whole screen every time, 20–40 ms at 1080p). It is what other remote
//! desktops use on Windows.
//!
//! When duplication is not possible (no GPU driver for it, a rotated or HDR output, the
//! secure desktop refusing it), the caller falls back to GDI.

use anyhow::{Context, Result, anyhow, bail};
use rdesk_common::tiles::Frame;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

/// One monitor's duplication.
pub struct Duplication {
    /// Where the monitor is on the virtual desktop (to find it again).
    pub rect: (i32, i32, u32, u32),
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    /// The picture so far (RGBA), kept up to date with the changed regions.
    last: Option<Frame>,
}

/// The output (and its adapter) showing the monitor at `rect` (x, y, width, height).
fn find_output(rect: (i32, i32, u32, u32)) -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    // SAFETY: COM calls on interfaces we own; the loop stops at DXGI_ERROR_NOT_FOUND.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        for a in 0.. {
            let adapter = match factory.EnumAdapters1(a) {
                Ok(x) => x,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            for o in 0.. {
                let output = match adapter.EnumOutputs(o) {
                    Ok(x) => x,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e.into()),
                };
                let d = output.GetDesc()?;
                let c = d.DesktopCoordinates;
                let here = (c.left, c.top, (c.right - c.left) as u32, (c.bottom - c.top) as u32);
                if here == rect && d.AttachedToDesktop.as_bool() {
                    if d.Rotation != DXGI_MODE_ROTATION_IDENTITY && d.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                        bail!("rotated monitor");
                    }
                    return Ok((adapter, output.cast()?));
                }
            }
        }
    }
    Err(anyhow!("no DXGI output at {rect:?}"))
}

impl Duplication {
    pub fn new(rect: (i32, i32, u32, u32)) -> Result<Self> {
        let (adapter, output) = find_output(rect)?;
        // SAFETY: COM calls; out-parameters are initialized by the callee on success.
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device: ID3D11Device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
            let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;
            let dup = output.DuplicateOutput(&device).context("DuplicateOutput")?;
            let desc = dup.GetDesc();
            if desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                bail!("desktop format {:?} (HDR?)", desc.ModeDesc.Format);
            }
            Ok(Self { rect, device, context, dup, staging: None, last: None })
        }
    }

    /// The current picture. Waits up to `wait_ms` for a change; with none, the last one again.
    pub fn grab(&mut self, wait_ms: u32) -> Result<Frame> {
        let wait = if self.last.is_none() { wait_ms.max(100) } else { wait_ms };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        // SAFETY: AcquireNextFrame/ReleaseFrame are paired; the mapped staging texture is read
        // within its row pitch and unmapped before returning.
        unsafe {
            match self.dup.AcquireNextFrame(wait, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    return self.last.clone().ok_or_else(|| anyhow!("no picture yet"));
                }
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => bail!(Lost),
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
            let r = self.take(&info, res);
            let _ = self.dup.ReleaseFrame();
            r?;
        }
        self.last.clone().ok_or_else(|| anyhow!("no picture yet"))
    }

    /// Bring `last` up to date with an acquired frame.
    unsafe fn take(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<IDXGIResource>) -> Result<()> {
        // Only the pointer moved: the picture is the same.
        if info.LastPresentTime == 0 && self.last.is_some() {
            return Ok(());
        }
        let tex: ID3D11Texture2D = res.ok_or_else(|| anyhow!("no desktop texture"))?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);
        if self.staging.is_none() || self.last.as_ref().is_some_and(|f| (f.width, f.height) != (w, h)) {
            let sd = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                MipLevels: 1,
                ArraySize: 1,
                ..desc
            };
            let mut t = None;
            unsafe { self.device.CreateTexture2D(&sd, None, Some(&mut t)) }.context("staging texture")?;
            self.staging = t;
            self.last = None;
        }
        let staging = self.staging.clone().expect("just made");
        let regions = if self.last.is_some() { unsafe { self.changed(info) } } else { None };
        unsafe {
            self.context.CopyResource(&staging, &tex);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map)).context("Map")?;
            let pitch = map.RowPitch as usize;
            let src = std::slice::from_raw_parts(map.pData as *const u8, pitch * h as usize);
            let frame = self.last.get_or_insert_with(|| Frame::new(w, h));
            match regions {
                Some(rects) => {
                    for r in rects {
                        convert(src, pitch, frame, r);
                    }
                }
                None => convert(src, pitch, frame, RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }),
            }
            self.context.Unmap(&staging, 0);
        }
        Ok(())
    }

    /// The regions that changed in the acquired frame (dirty rectangles and where moved ones
    /// landed), or `None` if they cannot be told (convert everything).
    unsafe fn changed(&self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Option<Vec<RECT>> {
        let size = info.TotalMetadataBufferSize as usize;
        if size == 0 {
            return Some(Vec::new());
        }
        let mut out = Vec::new();
        let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); size / size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1];
        let mut need = 0u32;
        let bytes = (moves.len() * size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32;
        unsafe { self.dup.GetFrameMoveRects(bytes, moves.as_mut_ptr(), &mut need) }.ok()?;
        out.extend(moves[..need as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>()].iter().map(|m| m.DestinationRect));
        let mut dirty = vec![RECT::default(); size / size_of::<RECT>() + 1];
        let bytes = (dirty.len() * size_of::<RECT>()) as u32;
        unsafe { self.dup.GetFrameDirtyRects(bytes, dirty.as_mut_ptr(), &mut need) }.ok()?;
        out.extend_from_slice(&dirty[..need as usize / size_of::<RECT>()]);
        Some(out)
    }
}

/// The duplication must be made again (another desktop: sign-in, lock, UAC; a mode change).
#[derive(Debug, thiserror::Error)]
#[error("desktop duplication lost")]
pub struct Lost;

/// Copy rectangle `r` of a BGRA image (`pitch` bytes per row) into `frame` as RGBA.
fn convert(src: &[u8], pitch: usize, frame: &mut Frame, r: RECT) {
    let (w, h) = (frame.width as i32, frame.height as i32);
    let (x0, y0) = (r.left.clamp(0, w) as usize, r.top.clamp(0, h) as usize);
    let (x1, y1) = (r.right.clamp(0, w) as usize, r.bottom.clamp(0, h) as usize);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let stride = frame.width as usize * 4;
    for y in y0..y1 {
        let s = &src[y * pitch + x0 * 4..y * pitch + x1 * 4];
        let d = &mut frame.data[y * stride + x0 * 4..y * stride + x1 * 4];
        for (dp, sp) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
            *dp = [sp[2], sp[1], sp[0], 255];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_only_the_rectangle() {
        let (w, h) = (4usize, 3usize);
        let pitch = w * 4 + 8;
        let mut src = vec![0u8; pitch * h];
        for y in 0..h {
            for x in 0..w {
                src[y * pitch + x * 4..y * pitch + x * 4 + 4].copy_from_slice(&[1, 2, 3, 0]);
            }
        }
        let mut f = Frame::new(w as u32, h as u32);
        convert(&src, pitch, &mut f, RECT { left: 1, top: 1, right: 3, bottom: 9 });
        let px = |x: usize, y: usize| f.data[(y * w + x) * 4..(y * w + x) * 4 + 4].to_vec();
        assert_eq!(px(1, 1), vec![3, 2, 1, 255]);
        assert_eq!(px(2, 2), vec![3, 2, 1, 255]);
        assert_eq!(px(0, 1), vec![0, 0, 0, 255], "left of the rectangle");
        assert_eq!(px(1, 0), vec![0, 0, 0, 255], "above it");
    }

    /// Needs a real screen: `cargo test -p rdesk-host --release dxgi -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a screen"]
    fn duplicates_the_primary_monitor() {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32) };
        let mut d = Duplication::new((0, 0, w, h)).expect("duplication");
        let t = std::time::Instant::now();
        let f = d.grab(500).expect("first picture");
        eprintln!("first {}x{} in {:?}", f.width, f.height, t.elapsed());
        let t = std::time::Instant::now();
        for _ in 0..30 {
            d.grab(0).unwrap();
        }
        eprintln!("30 more in {:?}", t.elapsed());
        assert_eq!((f.width, f.height), (w, h));
    }
}
