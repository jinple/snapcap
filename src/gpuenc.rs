//! GPU frame conversion for the recorder.
//!
//! The old path read every frame back to the CPU, converted it (10-bit -> 8-bit, then RGB ->
//! NV12 inside Media Foundation) and uploaded it again to the encoder: ~1.3 CPU cores at 1080p60.
//! Here the D3D11 *video processor* does the colour conversion (and composites the overlay) on the
//! GPU, and the NV12 texture goes straight to the hardware encoder through a DXGI device manager.
//! No readback, no CPU conversion.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{BOOL, Interface, Result};

struct Overlay {
    tex: ID3D11Texture2D,
    view: ID3D11VideoProcessorInputView,
    w: u32,
    h: u32,
    x: i32,
    y: i32,
}

pub struct GpuConv {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    venum: ID3D11VideoProcessorEnumerator,
    vproc: ID3D11VideoProcessor,
    pub mgr: IMFDXGIDeviceManager,
    src_view: Option<ID3D11VideoProcessorInputView>,
    ovl: Option<Overlay>,
    /// draw the overlay into frames (the "show overlay in recordings" setting, live)
    pub overlay_on: bool,
    w: u32,
    h: u32,
}

// BT.709, studio range (16-235) for the NV12 output; full-range RGB in.
const OUT_COLOR_SPACE: u32 = (1 << 2) | (1 << 4); // YCbCr_Matrix = 1 (709), Nominal_Range = 1 (16-235)
const IN_COLOR_SPACE: u32 = 0; // RGB, full range

impl GpuConv {
    /// `crop` is (x, y, w, h) of the part of the source to encode; `src` its full size.
    pub unsafe fn new(device: &ID3D11Device, fps: u32, crop: (u32, u32, u32, u32), src: (u32, u32)) -> Result<Self> {
        unsafe {
            // Media Foundation's encoder threads use this device too.
            let mt: ID3D11Multithread = device.cast()?;
            let _ = mt.SetMultithreadProtected(true);

            let vdev: ID3D11VideoDevice = device.cast()?;
            let ctx = device.GetImmediateContext()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            let (x0, y0, w, h) = crop;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: src.0,
                InputHeight: src.1,
                OutputFrameRate: rate,
                OutputWidth: w,
                OutputHeight: h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let venum = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vproc = vdev.CreateVideoProcessor(&venum, 0)?;

            vctx.VideoProcessorSetStreamColorSpace(&vproc, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: IN_COLOR_SPACE });
            vctx.VideoProcessorSetOutputColorSpace(&vproc, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: OUT_COLOR_SPACE });
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vproc, 0, false);
            vctx.VideoProcessorSetStreamSourceRect(&vproc, 0, true, Some(&RECT { left: x0 as i32, top: y0 as i32, right: (x0 + w) as i32, bottom: (y0 + h) as i32 }));
            let full = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            vctx.VideoProcessorSetStreamDestRect(&vproc, 0, true, Some(&full));
            vctx.VideoProcessorSetOutputTargetRect(&vproc, true, Some(&full));

            let mut token = 0u32;
            let mut mgr = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            mgr.ResetDevice(device, token)?;

            Ok(Self { device: device.clone(), ctx, vdev, vctx, venum, vproc, mgr, src_view: None, ovl: None, overlay_on: false, w, h })
        }
    }

    /// The texture frames are converted from (the game's shared frame, or the desktop copy).
    pub unsafe fn set_source(&mut self, tex: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let d = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut v = None;
            if let Err(e) = self.vdev.CreateVideoProcessorInputView(tex, &self.venum, &d, Some(&mut v)) {
                let mut td = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut td);
                let sup = self.venum.CheckVideoProcessorFormat(td.Format).unwrap_or(0);
                eprintln!(
                    "input view failed: {e} | texture {}x{} fmt {} bind {:#x} misc {:#x} usage {} | processor format support {:#x} (1 = usable as input)",
                    td.Width, td.Height, td.Format.0, td.BindFlags, td.MiscFlags, td.Usage.0, sup
                );
                return Err(e);
            }
            self.src_view = v;
            Ok(())
        }
    }

    /// New overlay pixels: premultiplied RGBA (tiny-skia) drawn at (x, y) in the frame.
    pub unsafe fn set_overlay(&mut self, rgba_premul: &[u8], w: u32, h: u32, x: i32, y: i32) -> Result<()> {
        unsafe {
            // The video processor blends with straight alpha: un-premultiply, and RGBA -> BGRA.
            let mut bgra = vec![0u8; (w * h * 4) as usize];
            for (d, s) in bgra.chunks_exact_mut(4).zip(rgba_premul.chunks_exact(4)) {
                let a = s[3] as u32;
                let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
                d[0] = un(s[2]);
                d[1] = un(s[1]);
                d[2] = un(s[0]);
                d[3] = s[3];
            }
            if self.ovl.as_ref().is_none_or(|o| o.w != w || o.h != h) {
                let td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.device.CreateTexture2D(&td, None, Some(&mut t))?;
                let tex = t.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
                let d = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
                };
                let mut v = None;
                self.vdev.CreateVideoProcessorInputView(&tex, &self.venum, &d, Some(&mut v))?;
                let view = v.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
                self.ovl = Some(Overlay { tex, view, w, h, x, y });
            }
            let o = self.ovl.as_mut().unwrap();
            o.x = x;
            o.y = y;
            self.ctx.UpdateSubresource(&o.tex, 0, None, bgra.as_ptr() as *const _, w * 4, 0);
            Ok(())
        }
    }

    /// Converts the current source frame to a fresh NV12 texture (with the overlay on top).
    /// Queued on the GPU; the caller must hold the game-frame mutex while this runs.
    pub unsafe fn convert(&self) -> Result<ID3D11Texture2D> {
        unsafe {
            let src = self.src_view.as_ref().ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let td = D3D11_TEXTURE2D_DESC {
                Width: self.w,
                Height: self.h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut t = None;
            self.device.CreateTexture2D(&td, None, Some(&mut t))?;
            let out = t.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut ov = None;
            self.vdev.CreateVideoProcessorOutputView(&out, &self.venum, &od, Some(&mut ov))?;
            let ov = ov.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;

            let mut streams: Vec<D3D11_VIDEO_PROCESSOR_STREAM> = Vec::with_capacity(2);
            streams.push(D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                pInputSurface: ManuallyDrop::new(Some(src.clone())),
                ..Default::default()
            });
            if let (true, Some(o)) = (self.overlay_on, self.ovl.as_ref()) {
                let r = RECT { left: o.x, top: o.y, right: o.x + o.w as i32, bottom: o.y + o.h as i32 };
                self.vctx.VideoProcessorSetStreamColorSpace(&self.vproc, 1, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: IN_COLOR_SPACE });
                self.vctx.VideoProcessorSetStreamSourceRect(&self.vproc, 1, false, None);
                self.vctx.VideoProcessorSetStreamDestRect(&self.vproc, 1, true, Some(&r));
                self.vctx.VideoProcessorSetStreamAlpha(&self.vproc, 1, true, 1.0);
                streams.push(D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: BOOL(1),
                    pInputSurface: ManuallyDrop::new(Some(o.view.clone())),
                    ..Default::default()
                });
            }
            let r = self.vctx.VideoProcessorBlt(&self.vproc, &ov, 0, &streams);
            for s in streams.iter_mut() {
                ManuallyDrop::drop(&mut s.pInputSurface);
            }
            r?;
            Ok(out)
        }
    }

    /// Wraps a converted frame as a Media Foundation sample the encoder reads straight from the GPU.
    pub unsafe fn sample(&self, tex: &ID3D11Texture2D, time: i64, dur: i64) -> Result<IMFSample> {
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let len = buf.cast::<IMF2DBuffer>().and_then(|b| b.GetContiguousLength()).unwrap_or(self.w * self.h * 3 / 2);
            buf.SetCurrentLength(len)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(time)?;
            s.SetSampleDuration(dur)?;
            Ok(s)
        }
    }
}
