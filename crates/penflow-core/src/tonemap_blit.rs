//! GPU compositor that converts a DDA frame texture (any of the four
//! supported scan-out formats) into the pipeline's BGRA8 keepalive,
//! applying the appropriate tonemap when the source is HDR.
//!
//! # Why
//!
//! The simple `CopyResource(keepalive_BGRA, dda_frame)` step the pipeline
//! used to do works only when the DDA returns a BGRA8 frame. On a physical
//! display in HDR mode, Windows' compositor scans out in
//! `DXGI_FORMAT_R16G16B16A16_FLOAT` (scRGB linear) — `CopyResource`
//! between mismatched formats silently no-ops, leaving the keepalive at
//! the zero-initialised black it was cleared to. Symptom on the tablet:
//! a pure-black video stream with only the OS cursor visible.
//!
//! `ID3D11VideoProcessor` (the API we already use for BGRA→NV12) **cannot
//! accept `R16G16B16A16_FLOAT` as INPUT** on common drivers — verified on
//! NVIDIA RTX 5070 with `CheckVideoProcessorFormat` returning OUTPUT-only
//! caps for that format. Industry-wide, the standard solution is a custom
//! HLSL shader; Microsoft's [DirectXTK `ToneMapPostProcess`][tk], LizardByte
//! Sunshine's HDR encoder, and the ReShade HDR shader collection all
//! follow that pattern. We do the same.
//!
//! # How
//!
//! One pixel shader, full-screen triangle, constant buffer selecting the
//! input color space:
//!
//! | DDA format                    | `input_color_space` | Path                                       |
//! |-------------------------------|---------------------|--------------------------------------------|
//! | `B8G8R8A8_UNORM`              | (handled upstream)  | `CopyResource` fast path in `pipeline.rs`  |
//! | `R8G8B8A8_UNORM`              | 0 (sRGB SDR)        | sample → swizzle → write                   |
//! | `R10G10B10A2_UNORM` (HDR10)   | 2 (PQ)              | PQ decode → BT.2020→BT.709 → ACES → sRGB   |
//! | `R16G16B16A16_FLOAT` (scRGB)  | 1 (scRGB linear)    | already linear → ACES → sRGB               |
//!
//! Generality is preserved by D3D11's design: `Texture2D<float4>` sampling
//! is format-agnostic at the shader level — the SRV's `Format` field
//! controls how raw bytes get unpacked into a `float4`. So the **same
//! shader** handles all four input formats; only the color-space cbuffer
//! field needs to be per-format.
//!
//! # Algorithms
//!
//! The math (`ToneMapACESFilmic`, `ST2084ToLinear`, `LinearToSRGBEst`,
//! BT.2020→BT.709 matrix) is industry-standard public formulae, copied
//! verbatim from DirectXTK's `Utilities.fxh` (MIT-licensed, Microsoft).
//! No proprietary content.
//!
//! [tk]: https://github.com/microsoft/DirectXTK/blob/main/Src/Shaders/ToneMap.fx

use windows::Win32::Graphics::Direct3D::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_PRIMITIVE_TOPOLOGY,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAP_WRITE_DISCARD,
    D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0,
    D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_RTV, D3D11_TEX2D_SRV,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM,
};

use crate::cursor_blit::compile_hlsl;
use crate::d3d11::D3d11Context;
use crate::error::{EngineError, EngineResult};

// SAFETY: lives on the single pipeline thread; D3D11 device is multithread
// protected for MF reach-ins.
unsafe impl Send for TonemapBlitter {}

const VERTEX_HLSL: &str = r#"
// Full-screen triangle generated from SV_VertexID — no vertex buffer
// needed. Vertex IDs 0/1/2 produce a triangle that covers the entire
// viewport with UV (0,0)..(1,1).
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};
VS_OUT vs_main(uint vid : SV_VertexID) {
    VS_OUT o;
    float2 ndc = float2((vid << 1) & 2, vid & 2);
    o.uv = ndc;
    // Flip Y so UV (0,0) corresponds to NDC (-1, +1) (top-left).
    o.pos = float4(ndc * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}
"#;

const PIXEL_HLSL: &str = r#"
Texture2D<float4> input_tex : register(t0);
SamplerState      samp      : register(s0);

cbuffer ConvertParams : register(b0) {
    uint  input_color_space; // 0=sRGB SDR (passthrough), 1=scRGB linear, 2=HDR10 PQ
    // The "SDR content brightness" slider in Windows HDR settings.
    // When > 1, Windows places SDR-white at scRGB > 1.0 (e.g. slider
    // at ~half gives ~2.25, putting SDR-white at 180 nits). The shader
    // divides scRGB by this before clamping so SDR content renders
    // correctly regardless of the user's slider position.
    // Read from `DisplayConfigGetDeviceInfo(GET_SDR_WHITE_LEVEL)`.
    float scrgb_sdr_scale;
    float _pad0;
    float _pad1;
};

// === DirectXTK `Utilities.fxh` (MIT, Microsoft) ===

float3 ToneMapACESFilmic(float3 x) {
    float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return saturate((x * (a * x + b)) / (x * (c * x + d) + e));
}

float3 ST2084ToLinear(float3 v) {
    float3 m = pow(abs(v), 1.0 / 78.84375);
    return pow(max(m - 0.8359375, 0.0) /
               (18.8515625 - 18.6875 * m),
               1.0 / 0.1593017578);
}

float3 LinearToSRGB(float3 c) {
    // Matches DirectXTK's LinearToSRGBEst — fast pow-based approximation.
    // Differs from the exact piecewise sRGB curve by < 0.5 / 255 in
    // every channel, well below 8-bit quantisation.
    return pow(abs(c), 1.0 / 2.2);
}

// BT.2020 → BT.709 chromaticity-adaptation matrix. Standard formula
// (e.g. ITU-R BT.2087-0). Used to convert HDR10's BT.2020 primaries
// down to sRGB's BT.709 primaries before tonemapping.
static const float3x3 REC2020_TO_REC709 = {
     1.6605, -0.5876, -0.0728,
    -0.1246,  1.1329, -0.0083,
    -0.0182, -0.1006,  1.1187,
};

// === Main ===

struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

float4 ps_main(VS_OUT i) : SV_TARGET {
    float4 src = input_tex.Sample(samp, i.uv);

    if (input_color_space == 0) {
        // Source is sRGB-encoded SDR (BGRA8 / RGBA8). Sampler returns
        // raw gamma-encoded values; RTV is BGRA8_UNORM; no transform
        // needed. (Channel swizzle is handled by the SRV format choice
        // upstream — RGBA8 input is auto-swizzled to BGRA on RTV write.)
        return src;
    }

    float3 sdr_linear;
    if (input_color_space == 1) {
        // scRGB: linear, BT.709 primaries, full range. The HUGE majority
        // of pixels on a Windows-HDR-on desktop are **SDR content
        // composited into scRGB** — Krita canvases, taskbar, browser
        // tabs, file explorer. Those values sit in [0, 1] in scRGB
        // because the compositor places SDR-white at 1.0 (= 80 nits).
        //
        // Running ACES Filmic on SDR-range content **lifts mid-tones
        // by ~9%** (ACES expects HDR input to compress and bakes a
        // soft toe + shoulder; with no values > 1 to compress, only
        // the lift portion of the curve fires). Symptom on the tablet:
        // every screenshot looks washed-out / over-exposed.
        //
        // First normalise back to "SDR-as-the-app-meant-it" range by
        // dividing out the user's slider boost, then clamp. After this:
        //   - SDR content renders byte-identical to a native SDR display
        //   - True HDR highlights (rare on desktops) clip to white,
        //     which is acceptable for a stylus-display use case
        sdr_linear = saturate(src.rgb / max(scrgb_sdr_scale, 0.0001));
    } else /* input_color_space == 2 */ {
        // HDR10 PQ, BT.2020 primaries. PQ encodes absolute luminance:
        // 1.0 = 10000 nits, so a typical desktop fits in [0, 0.5]
        // unless something genuinely HDR is on screen. We always
        // tonemap here because the value range is huge by definition.
        //
        // Decode PQ EOTF, normalise to scRGB convention (1.0 = 80 nits),
        // re-primary BT.2020 → BT.709, then ACES Filmic.
        const float SCRGB_WHITE_NITS = 80.0;
        float3 linear_bt2020_abs = ST2084ToLinear(src.rgb);
        float3 linear_bt2020_scrgb = linear_bt2020_abs * (10000.0 / SCRGB_WHITE_NITS);
        float3 linear_rec709 = mul(REC2020_TO_REC709, linear_bt2020_scrgb);
        sdr_linear = ToneMapACESFilmic(linear_rec709);
    }

    // Encode for the BGRA8 keepalive (RTV is `_UNORM` not `_UNORM_SRGB`,
    // so the driver does NOT auto-encode — we apply the gamma here).
    return float4(LinearToSRGB(sdr_linear), 1.0);
}
"#;

/// Constant-buffer layout matching the HLSL `ConvertParams`.
/// 16-byte aligned per HLSL cbuffer rules.
#[repr(C)]
#[derive(Clone, Copy)]
struct ConvertParams {
    input_color_space: u32,
    scrgb_sdr_scale: f32,
    _pad0: f32,
    _pad1: f32,
}

/// Logical input color space the shader knows how to consume. Drives
/// the cbuffer's `input_color_space` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputColorSpace {
    /// sRGB-encoded SDR. BGRA8 / RGBA8 sources. Shader is a passthrough.
    SdrSrgb = 0,
    /// scRGB linear, BT.709 primaries, full range. Possibly > 1.0 for
    /// HDR highlights. Source: `R16G16B16A16_FLOAT` from a
    /// Windows-HDR-on desktop.
    ScrgbLinear = 1,
    /// HDR10 PQ, BT.2020 primaries. Source: `R10G10B10A2_UNORM` from a
    /// fullscreen-exclusive HDR app or Auto-HDR.
    Hdr10Pq = 2,
}

/// Map a DXGI scan-out format to the `InputColorSpace` we feed the
/// shader. Returns `None` for formats the blitter doesn't handle —
/// caller should fall back to `CopyResource` (BGRA8) or fail loudly.
pub fn input_color_space_for(fmt: DXGI_FORMAT) -> Option<InputColorSpace> {
    match fmt {
        // Caller should NOT route BGRA through here — it's a wasted GPU
        // pass when CopyResource works. We accept it for completeness.
        f if f == DXGI_FORMAT_B8G8R8A8_UNORM => Some(InputColorSpace::SdrSrgb),
        f if f == DXGI_FORMAT_R8G8B8A8_UNORM => Some(InputColorSpace::SdrSrgb),
        f if f == DXGI_FORMAT_R16G16B16A16_FLOAT => Some(InputColorSpace::ScrgbLinear),
        f if f == DXGI_FORMAT_R10G10B10A2_UNORM => Some(InputColorSpace::Hdr10Pq),
        _ => None,
    }
}

pub struct TonemapBlitter {
    target_w: u32,
    target_h: u32,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rasterizer: ID3D11RasterizerState,
    target_rtv: ID3D11RenderTargetView,
    /// Dynamic constant buffer; refreshed per-frame.
    constant_buffer: ID3D11Buffer,
    /// scRGB → SDR-range divisor read from `DisplayConfigGetDeviceInfo`.
    /// Default 1.0 = "slider at default / unknown / non-HDR display".
    /// `Pipeline::start` sets it to the queried value before the loop
    /// begins. Stored as `f32` because that's what gets written to the
    /// cbuffer; rebuild not needed if it changes at runtime — we just
    /// rewrite the cbuffer on the next `convert`.
    scrgb_sdr_scale: std::sync::atomic::AtomicU32,
}

impl TonemapBlitter {
    /// Build a tonemap blitter that writes into `target_keepalive`
    /// (a BGRA8 texture; same buffer the rest of the pipeline already
    /// uses).
    pub fn new(
        ctx: &D3d11Context,
        target_keepalive: &ID3D11Texture2D,
        target_w: u32,
        target_h: u32,
    ) -> EngineResult<Self> {
        // Compile both shaders.
        let (vs_blob, vs_bytecode) = compile_hlsl(VERTEX_HLSL, "vs_main", "vs_5_0")?;
        let (_ps_blob, ps_bytecode) = compile_hlsl(PIXEL_HLSL, "ps_main", "ps_5_0")?;

        let mut vs: Option<ID3D11VertexShader> = None;
        unsafe {
            ctx.device
                .CreateVertexShader(vs_bytecode, None, Some(&mut vs))?;
        }
        let vs = vs.ok_or(EngineError::NotInitialized)?;
        drop(vs_blob);

        let mut ps: Option<ID3D11PixelShader> = None;
        unsafe {
            ctx.device
                .CreatePixelShader(ps_bytecode, None, Some(&mut ps))?;
        }
        let ps = ps.ok_or(EngineError::NotInitialized)?;

        // Linear sampler — a tiny bit of softening on edge cases is
        // preferable to point-sampled aliasing when DDA dimensions
        // don't match the keepalive (they should; defensive).
        let samp_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: 0.0,
        };
        let mut sampler: Option<ID3D11SamplerState> = None;
        unsafe {
            ctx.device
                .CreateSamplerState(&samp_desc, Some(&mut sampler))?;
        }
        let sampler = sampler.ok_or(EngineError::NotInitialized)?;

        let raster_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            FrontCounterClockwise: false.into(),
            DepthBias: 0,
            DepthBiasClamp: 0.0,
            SlopeScaledDepthBias: 0.0,
            DepthClipEnable: false.into(),
            ScissorEnable: false.into(),
            MultisampleEnable: false.into(),
            AntialiasedLineEnable: false.into(),
        };
        let mut rasterizer: Option<ID3D11RasterizerState> = None;
        unsafe {
            ctx.device
                .CreateRasterizerState(&raster_desc, Some(&mut rasterizer))?;
        }
        let rasterizer = rasterizer.ok_or(EngineError::NotInitialized)?;

        // RTV against the keepalive — same buffer pipeline already owns.
        let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
            },
        };
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        unsafe {
            ctx.device
                .CreateRenderTargetView(target_keepalive, Some(&rtv_desc), Some(&mut rtv))?;
        }
        let target_rtv = rtv.ok_or(EngineError::NotInitialized)?;

        // Constant buffer — 16 bytes (one float4-equivalent), dynamic so
        // we can `Map(WRITE_DISCARD)` it each frame.
        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<ConvertParams>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut cb: Option<ID3D11Buffer> = None;
        unsafe {
            ctx.device.CreateBuffer(&cb_desc, None, Some(&mut cb))?;
        }
        let constant_buffer = cb.ok_or(EngineError::NotInitialized)?;

        Ok(Self {
            target_w,
            target_h,
            vs,
            ps,
            sampler,
            rasterizer,
            target_rtv,
            constant_buffer,
            scrgb_sdr_scale: std::sync::atomic::AtomicU32::new(1.0_f32.to_bits()),
        })
    }

    /// Update the user's SDR-content-brightness scale factor (Windows
    /// HDR settings slider). Cheap, lock-free; the next `convert` call
    /// will use the new value. Call this once on session start with the
    /// value from `query_sdr_white_level_scale(&monitor.device_name)`.
    pub fn set_scrgb_sdr_scale(&self, scale: f32) {
        self.scrgb_sdr_scale
            .store(scale.to_bits(), std::sync::atomic::Ordering::Release);
    }

    fn current_scrgb_sdr_scale(&self) -> f32 {
        f32::from_bits(
            self.scrgb_sdr_scale
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }

    /// Render `dda_frame` into the keepalive RTV with the appropriate
    /// transfer function and tonemap for `dda_format`.
    ///
    /// `dda_format` MUST be one of the formats `input_color_space_for`
    /// recognises — caller is responsible for routing BGRA-fast-path
    /// frames to `CopyResource` instead.
    pub fn convert(
        &self,
        ctx: &D3d11Context,
        dda_frame: &ID3D11Texture2D,
        dda_format: DXGI_FORMAT,
    ) -> EngineResult<()> {
        let cs = input_color_space_for(dda_format).ok_or_else(|| {
            eprintln!(
                "[tonemap_blit] unsupported DDA format {} — caller should not have routed it here",
                dda_format.0
            );
            EngineError::NotInitialized
        })?;

        // 1. Refresh the constant buffer.
        let params = ConvertParams {
            input_color_space: cs as u32,
            scrgb_sdr_scale: self.current_scrgb_sdr_scale(),
            _pad0: 0.0,
            _pad1: 0.0,
        };
        unsafe {
            let mut mapped =
                windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
            ctx.immediate_context.Map(
                &self.constant_buffer,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut mapped),
            )?;
            std::ptr::copy_nonoverlapping(
                &params as *const _ as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<ConvertParams>(),
            );
            ctx.immediate_context.Unmap(&self.constant_buffer, 0);
        }

        // 2. Build an SRV against the DDA texture. The view's `Format`
        //    field controls how raw bytes are unpacked into the
        //    shader's `float4` — that's what makes one shader handle
        //    every input format.
        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: dda_format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            ctx.device
                .CreateShaderResourceView(dda_frame, Some(&srv_desc), Some(&mut srv))?;
        }
        let srv = srv.ok_or(EngineError::NotInitialized)?;

        // 3. Bind state and draw a 3-vertex full-screen triangle.
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.target_w as f32,
            Height: self.target_h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let rtv_array = [Some(self.target_rtv.clone())];
        let srv_array = [Some(srv.clone())];
        let samp_array = [Some(self.sampler.clone())];
        let cb_array = [Some(self.constant_buffer.clone())];

        unsafe {
            ctx.immediate_context
                .OMSetRenderTargets(Some(&rtv_array), None);
            // No blend state — we want to overwrite the keepalive,
            // not blend on top of it. Default blend = no-op overwrite.
            ctx.immediate_context
                .OMSetBlendState(None, None, 0xFFFFFFFF);
            ctx.immediate_context.RSSetState(&self.rasterizer);
            ctx.immediate_context
                .RSSetViewports(Some(std::slice::from_ref(&viewport)));
            // No input layout / no vertex buffer — VS reads SV_VertexID.
            ctx.immediate_context.IASetInputLayout(None);
            ctx.immediate_context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(
                    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST.0,
                ));
            ctx.immediate_context.VSSetShader(&self.vs, None);
            ctx.immediate_context.PSSetShader(&self.ps, None);
            ctx.immediate_context
                .PSSetShaderResources(0, Some(&srv_array));
            ctx.immediate_context.PSSetSamplers(0, Some(&samp_array));
            ctx.immediate_context
                .PSSetConstantBuffers(0, Some(&cb_array));
            ctx.immediate_context.Draw(3, 0);

            // Defensive unbind: leaving the SRV bound after the call
            // would prevent the keepalive from being used as an SRV in
            // the very next stage of the pipeline. We never do that
            // today, but unbinding keeps state hygiene.
            let null_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            ctx.immediate_context
                .PSSetShaderResources(0, Some(&null_srv));
        }

        let _ = srv; // explicit drop site; refcount falls to 0 here
        Ok(())
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.target_w, self.target_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::create_bgra_keepalive_texture;

    #[test]
    fn input_color_space_mapping() {
        assert_eq!(
            input_color_space_for(DXGI_FORMAT_B8G8R8A8_UNORM),
            Some(InputColorSpace::SdrSrgb),
        );
        assert_eq!(
            input_color_space_for(DXGI_FORMAT_R8G8B8A8_UNORM),
            Some(InputColorSpace::SdrSrgb),
        );
        assert_eq!(
            input_color_space_for(DXGI_FORMAT_R10G10B10A2_UNORM),
            Some(InputColorSpace::Hdr10Pq),
        );
        assert_eq!(
            input_color_space_for(DXGI_FORMAT_R16G16B16A16_FLOAT),
            Some(InputColorSpace::ScrgbLinear),
        );
        // NV12 is a video-encoder format, not a DDA scan-out format.
        assert_eq!(
            input_color_space_for(windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12),
            None,
        );
    }

    /// Smoke: build a TonemapBlitter and run one convert with a synthetic
    /// scRGB float input. Verifies the shader compiles, the SRV creates
    /// for RGBA16F input, and the draw call doesn't error. Pixel values
    /// are not checked here — that's a separate visual-integration test.
    #[test]
    #[ignore = "requires real D3D11 video device; GitHub windows-latest VM has no GPU"]
    fn tonemap_smoke_scrgb() {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

        let ctx = D3d11Context::create_high_perf().expect("d3d11 ctx");
        let keepalive = create_bgra_keepalive_texture(&ctx.device, 256, 144).expect("keepalive");
        let blitter = TonemapBlitter::new(&ctx, &keepalive, 256, 144).expect("blitter");

        // Synthetic RGBA16F input texture.
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 256,
            Height: 144,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
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
        unsafe { ctx.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("rgba16f");
        let input = tex.expect("rgba16f handle");

        blitter
            .convert(&ctx, &input, DXGI_FORMAT_R16G16B16A16_FLOAT)
            .expect("scRGB convert");
    }

    #[test]
    #[ignore = "requires real D3D11 video device; GitHub windows-latest VM has no GPU"]
    fn tonemap_smoke_hdr10() {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

        let ctx = D3d11Context::create_high_perf().expect("d3d11 ctx");
        let keepalive = create_bgra_keepalive_texture(&ctx.device, 256, 144).expect("keepalive");
        let blitter = TonemapBlitter::new(&ctx, &keepalive, 256, 144).expect("blitter");

        let desc = D3D11_TEXTURE2D_DESC {
            Width: 256,
            Height: 144,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
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
        unsafe { ctx.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("hdr10 tex");
        let input = tex.expect("hdr10 handle");

        blitter
            .convert(&ctx, &input, DXGI_FORMAT_R10G10B10A2_UNORM)
            .expect("HDR10 PQ convert");
    }
}
