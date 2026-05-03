//! BGRA → NV12 colour conversion via D3D11 VideoProcessor.
//!
//! Reference: design.md §6.2.
//!
//! We avoid Sunshine's ~1000 LOC of HLSL+C++ for the colour shader path —
//! `ID3D11VideoProcessor` does scan-out-format → NV12 with full-range
//! BT.709 tagging in roughly the same budget, plus the driver writer
//! handles edge cases for us. The output texture is OWNED by the converter
//! and reused across calls so the encoder's NV12 input is always the same
//! `ID3D11Texture2D*`. That stability matters for NVENC's
//! `nvEncRegisterResource` cache (HANDOFF §2.3 #6), and is harmless
//! otherwise.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_TEX2D_RTV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::d3d11::D3d11Context;
use crate::error::{EngineError, EngineResult};

// SAFETY: video processor + view + textures live on the same single-threaded
// pipeline owner; D3D11 device has SetMultithreadProtected(true) for the
// rare case MF reaches into shared device state.
unsafe impl Send for ColorConverter {}

/// BGRA → NV12 converter. Holds a stable output NV12 texture; call
/// `output_texture()` once after construction to register it with the
/// downstream encoder.
pub struct ColorConverter {
    width: u32,
    height: u32,

    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,

    output_texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

impl ColorConverter {
    /// Build a converter for `width × height` frames running at `fps`. The
    /// frame rate is mostly informational for the VideoProcessor — it picks
    /// rate-conversion paths from it — but we set it to the encoder's fps for
    /// honesty. The D3D11 device must support the video pipeline (created
    /// with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, which `D3d11Context` does).
    pub fn new(ctx: &D3d11Context, width: u32, height: u32, fps: u32) -> EngineResult<Self> {
        if width == 0 || height == 0 {
            return Err(EngineError::NotInitialized);
        }
        let video_device: ID3D11VideoDevice = ctx.device.cast()?;
        let immediate: ID3D11DeviceContext = ctx.immediate_context.clone();
        let video_context: ID3D11VideoContext1 = immediate.cast()?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator =
            unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };
        // Rate-conversion index 0 is always the simplest "no rate change"
        // path; we don't do framerate conversion.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        // Tell the processor what colour space it's converting between.
        // Stream input: RGB full range, BT.709 (DXGI scan-out is sRGB primaries
        // and full range when the desktop is SDR). Output: YCbCr full range
        // BT.709, matching the MF encoder's input VUI (HANDOFF §4.5 finding 4).
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            // LEFT chroma siting matches the HEVC spec for 4:2:0 NV12. The
            // "_NONE_" siting variant for YCbCr doesn't exist in DXGI; LEFT
            // is the right choice for a video-encode pipeline.
            video_context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
            );
        }

        // Create the stable NV12 output texture and its view.
        let output_texture = create_nv12_texture(&ctx.device, width, height)?;
        let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &output_texture,
                &enumerator,
                &view_desc,
                Some(&mut out_view),
            )?;
        }
        let output_view = out_view.ok_or(EngineError::NotInitialized)?;

        let _ = fps; // recorded above in the content desc; not held on the struct
        Ok(Self {
            width,
            height,
            video_device,
            video_context,
            enumerator,
            processor,
            output_texture,
            output_view,
        })
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Stable NV12 output texture. Same `ID3D11Texture2D*` for the converter's
    /// lifetime — register once with the encoder and reuse.
    pub fn output_texture(&self) -> &ID3D11Texture2D {
        &self.output_texture
    }

    /// Convert one BGRA input texture into the cached NV12 output. Caller is
    /// responsible for ensuring `input` was produced on the same D3D11 device.
    pub fn convert(&self, input: &ID3D11Texture2D) -> EngineResult<()> {
        let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
        let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                input,
                &self.enumerator,
                &view_desc,
                Some(&mut input_view),
            )?;
        }
        let input_view = input_view.ok_or(EngineError::NotInitialized)?;

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )?;
        }
        // The input view inside `stream` is dropped via ManuallyDrop in the
        // struct teardown when `stream` goes out of scope. We must explicitly
        // take it back so the COM refcount drops to zero.
        let _ = ManuallyDrop::into_inner(stream.pInputSurface);
        let _ = ManuallyDrop::into_inner(stream.pInputSurfaceRight);
        Ok(())
    }
}

fn create_nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> EngineResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or(EngineError::NotInitialized)
}

/// Helper: create a stable BGRA "keepalive" texture matching `width × height`.
/// Used by the pipeline to copy DDA frames into a fixed pointer the encoder
/// can re-use across frames (HANDOFF §2.3 #6).
///
/// **Important:** the texture is **not** zero-initialised by D3D11.
/// Callers that may submit it to a hardware encoder (MF HEVC MFT etc.)
/// before any DDA frame overwrites it MUST clear it explicitly via
/// [`clear_bgra_texture_to_black`] right after creation. The MF HEVC
/// MFT rejects samples wrapping textures whose contents are
/// "undefined" with `MF_E_UNSUPPORTED_D3D_TYPE` (HRESULT 0xC00D6D76 —
/// localised as "D3D 设备不支持此输入类型" but the real meaning is
/// "the content is not supported for the current Direct3D device").
pub fn create_bgra_keepalive_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> EngineResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or(EngineError::NotInitialized)
}

/// Clear a BGRA texture to opaque black (`B=G=R=0, A=255`). Creates a
/// transient `ID3D11RenderTargetView` against the texture, calls
/// `ClearRenderTargetView`, drops the view. The clear is GPU-side and
/// cheap (~tens of microseconds for 4K).
///
/// Used by the pipeline immediately after `create_bgra_keepalive_texture`
/// so the encoder always sees a valid input even when DDA hasn't
/// produced its first frame yet (e.g. capturing a freshly-attached VDD
/// extend monitor that has no content drawn on it).
pub fn clear_bgra_texture_to_black(
    ctx: &D3d11Context,
    texture: &ID3D11Texture2D,
) -> EngineResult<()> {
    let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
        },
    };
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    unsafe {
        ctx.device.CreateRenderTargetView(texture, Some(&rtv_desc), Some(&mut rtv))?;
    }
    let rtv = rtv.ok_or(EngineError::NotInitialized)?;
    let black: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
    unsafe {
        ctx.immediate_context.ClearRenderTargetView(&rtv, &black);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: build a converter against the high-perf adapter and convert one
    /// black BGRA frame into the cached NV12 texture. Verifies the API path
    /// compiles and the device supports the video pipeline.
    #[test]
    fn converter_processes_one_frame() {
        let ctx = D3d11Context::create_high_perf().expect("d3d11 ctx");
        let conv = ColorConverter::new(&ctx, 256, 144, 60).expect("color converter");
        let input = create_bgra_keepalive_texture(&ctx.device, 256, 144).expect("bgra texture");
        conv.convert(&input).expect("convert");
        // Output texture pointer is stable.
        let p1 = conv.output_texture() as *const _;
        let p2 = conv.output_texture() as *const _;
        assert_eq!(p1, p2);
    }
}
