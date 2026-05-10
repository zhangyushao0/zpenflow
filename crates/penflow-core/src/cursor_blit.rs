//! GPU compositor: blit one cursor sprite onto the captured BGRA keepalive
//! before colour conversion. Lets us keep `HardwareCursor=true` on the VDD
//! (so DWM does NOT compose the cursor into the framebuffer at all) and
//! still ship a cursor-bearing video stream — saving the one-frame DWM
//! compositor delay that the `HardwareCursor=false` workaround pays for.
//!
//! Path is: VS pass-through (NDC pos + UV) → PS samples a BGRA cursor
//! texture → blend state SRC=ONE, DEST=INV_SRC_ALPHA (premultiplied alpha).
//! DDA delivers `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR` in PMA already; the
//! masked-color and monochrome paths in `cursor_shape.rs` produce PMA-shaped
//! output too (alpha=255 for opaque, alpha=0 for transparent), so the same
//! blend state covers all three.
//!
//! State pollution: this module unconditionally re-binds VS, PS, IA, RS,
//! OM, blend, and viewport on every `composite()` call. The pipeline's
//! `ColorConverter::convert` runs through `ID3D11VideoContext1::VideoProcessorBlt`,
//! which doesn't share state with the regular render pipeline, so stomping
//! the immediate context is safe.

use std::ffi::CString;

use windows::core::PCSTR;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D_PRIMITIVE_TOPOLOGY,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11InputLayout, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_VERTEX_BUFFER, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE,
    D3D11_BLEND_OP_ADD, D3D11_BLEND_ZERO, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA,
    D3D11_MAP_WRITE_DISCARD, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT, DXGI_SAMPLE_DESC,
};

use crate::capture::cursor_shape::CursorShape;
use crate::d3d11::D3d11Context;
use crate::error::{EngineError, EngineResult};

const VERTEX_HLSL: &str = r#"
struct VS_IN {
    float2 pos : POSITION;
    float2 uv  : TEXCOORD0;
};
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};
VS_OUT vs_main(VS_IN i) {
    VS_OUT o;
    o.pos = float4(i.pos, 0.0, 1.0);
    o.uv = i.uv;
    return o;
}
"#;

const PIXEL_HLSL: &str = r#"
Texture2D<float4>  cursor_tex : register(t0);
SamplerState       samp       : register(s0);
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};
float4 ps_main(VS_OUT i) : SV_TARGET {
    // BGRA premultiplied; matched by blend state SRC=ONE, DEST=INV_SRC_ALPHA.
    return cursor_tex.Sample(samp, i.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

const VERTEX_SIZE: u32 = std::mem::size_of::<Vertex>() as u32;
const VERTEX_BUFFER_BYTES: u32 = VERTEX_SIZE * 4;

/// GPU compositor for one render target.
///
/// Tied to one BGRA target texture (the pipeline's keepalive) — we cache
/// its render-target view. If the keepalive is ever swapped (it isn't,
/// today), recreate the blitter.
pub struct CursorBlitter {
    target_w: u32,
    target_h: u32,

    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    vertex_buffer: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_state: ID3D11BlendState,
    rasterizer: ID3D11RasterizerState,
    target_rtv: ID3D11RenderTargetView,

    // Cached cursor sprite. `None` until the first shape arrives.
    cursor: Option<CachedCursor>,
}

struct CachedCursor {
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
    /// Generation counter incremented when `update_shape` actually rebuilds
    /// the GPU texture. Lets the pipeline cheaply skip redundant uploads.
    generation: u64,
    /// The texture itself, sized to fit the current shape.
    _texture: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

// SAFETY: like the rest of the engine, the blitter lives on the single
// pipeline thread; D3D11 device is multithread-protected for the rare
// case Media Foundation reaches in.
unsafe impl Send for CursorBlitter {}

impl CursorBlitter {
    /// Build a blitter pointed at `target` (the pipeline's BGRA keepalive).
    /// `target_w`/`target_h` MUST match the texture's actual dimensions —
    /// the same values the keepalive was created with — so NDC math lands
    /// on the right pixels.
    pub fn new(
        ctx: &D3d11Context,
        target: &ID3D11Texture2D,
        target_w: u32,
        target_h: u32,
    ) -> EngineResult<Self> {
        let (vs_blob, vs_bytecode) = compile_hlsl(VERTEX_HLSL, "vs_main", "vs_5_0")?;
        let (_ps_blob, ps_bytecode) = compile_hlsl(PIXEL_HLSL, "ps_main", "ps_5_0")?;

        // Compile shaders.
        let mut vs: Option<ID3D11VertexShader> = None;
        unsafe {
            ctx.device
                .CreateVertexShader(vs_bytecode, None, Some(&mut vs))?;
        }
        let vs = vs.ok_or(EngineError::NotInitialized)?;

        let mut ps: Option<ID3D11PixelShader> = None;
        unsafe {
            ctx.device
                .CreatePixelShader(ps_bytecode, None, Some(&mut ps))?;
        }
        let ps = ps.ok_or(EngineError::NotInitialized)?;

        // Input layout: position (float2), uv (float2). Names must match
        // the HLSL VS_IN semantics. Hold the C strings in locals so the
        // PCSTR pointers we hand to D3D stay valid for the call.
        let pos_name = CString::new("POSITION").unwrap();
        let uv_name = CString::new("TEXCOORD").unwrap();
        let elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(pos_name.as_ptr() as *const u8),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(uv_name.as_ptr() as *const u8),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let mut input_layout: Option<ID3D11InputLayout> = None;
        unsafe {
            ctx.device
                .CreateInputLayout(&elements, vs_bytecode, Some(&mut input_layout))?;
        }
        let input_layout = input_layout.ok_or(EngineError::NotInitialized)?;
        // vs_blob holds the bytecode buffer alive while we used it.
        drop(vs_blob);

        // Dynamic vertex buffer for 4 verts; rewritten per `composite()`.
        let vb_desc = D3D11_BUFFER_DESC {
            ByteWidth: VERTEX_BUFFER_BYTES,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut vb: Option<ID3D11Buffer> = None;
        unsafe {
            ctx.device.CreateBuffer(&vb_desc, None, Some(&mut vb))?;
        }
        let vertex_buffer = vb.ok_or(EngineError::NotInitialized)?;

        // Sampler: point + clamp. Cursor sprites are pixel-art-perfect at
        // 1:1; bilinear would smear the alpha edge.
        let samp_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
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

        // Blend: premultiplied-alpha over.
        let mut blend_desc = D3D11_BLEND_DESC::default();
        blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_ZERO,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut blend_state: Option<ID3D11BlendState> = None;
        unsafe {
            ctx.device
                .CreateBlendState(&blend_desc, Some(&mut blend_state))?;
        }
        let blend_state = blend_state.ok_or(EngineError::NotInitialized)?;

        // Rasterizer: cull none (4-vert strip can wind either way), no
        // scissor (we clip in CPU before computing verts).
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

        // Render target view against the keepalive.
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
                .CreateRenderTargetView(target, Some(&rtv_desc), Some(&mut rtv))?;
        }
        let target_rtv = rtv.ok_or(EngineError::NotInitialized)?;

        Ok(Self {
            target_w,
            target_h,
            vs,
            ps,
            input_layout,
            vertex_buffer,
            sampler,
            blend_state,
            rasterizer,
            target_rtv,
            cursor: None,
        })
    }

    /// Replace the cached cursor sprite with `shape`. Allocates a fresh
    /// BGRA texture sized to the shape and uploads the pixels in one
    /// `UpdateSubresource` call. Returns the new generation counter so
    /// callers can invalidate any draw-side state if needed (today the
    /// pipeline doesn't track this — every `composite()` references the
    /// current cached texture).
    pub fn update_shape(&mut self, ctx: &D3d11Context, shape: &CursorShape) -> EngineResult<u64> {
        let next_gen = self.cursor.as_ref().map(|c| c.generation + 1).unwrap_or(1);
        let row_pitch = shape.width * 4;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: shape.width,
            Height: shape.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: shape.pixels.as_ptr() as *const _,
            SysMemPitch: row_pitch,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe {
            ctx.device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
        }
        let tex = tex.ok_or(EngineError::NotInitialized)?;
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            ctx.device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        }
        let srv = srv.ok_or(EngineError::NotInitialized)?;
        self.cursor = Some(CachedCursor {
            width: shape.width,
            height: shape.height,
            hot_x: shape.hot_x,
            hot_y: shape.hot_y,
            generation: next_gen,
            _texture: tex,
            srv,
        });
        let _ = D3D11_BIND_RENDER_TARGET; // silence unused (kept for symmetry)
        Ok(next_gen)
    }

    /// Composite the cached cursor onto the bound target at screen position
    /// `(pos_x, pos_y)`. Position is in target-local pixels, with the OS
    /// reporting it WITHOUT the hotspot applied — we subtract the hotspot
    /// here to get the bitmap's top-left.
    ///
    /// Returns silently when there's no cached shape, when the cursor lies
    /// fully off-target, or when DDA reported `visible == false` (caller
    /// should already have filtered that, but defensive).
    pub fn composite(&self, ctx: &D3d11Context, pos_x: i32, pos_y: i32) -> EngineResult<()> {
        let cursor = match &self.cursor {
            Some(c) => c,
            None => return Ok(()),
        };
        let bitmap_x = pos_x - cursor.hot_x;
        let bitmap_y = pos_y - cursor.hot_y;
        let quad = match compute_clipped_quad(
            bitmap_x,
            bitmap_y,
            cursor.width,
            cursor.height,
            self.target_w,
            self.target_h,
        ) {
            Some(q) => q,
            None => return Ok(()),
        };

        // Update the dynamic vertex buffer.
        let verts = [
            Vertex {
                pos: [quad.ndc_l, quad.ndc_t],
                uv: [quad.u_l, quad.v_t],
            },
            Vertex {
                pos: [quad.ndc_r, quad.ndc_t],
                uv: [quad.u_r, quad.v_t],
            },
            Vertex {
                pos: [quad.ndc_l, quad.ndc_b],
                uv: [quad.u_l, quad.v_b],
            },
            Vertex {
                pos: [quad.ndc_r, quad.ndc_b],
                uv: [quad.u_r, quad.v_b],
            },
        ];
        unsafe {
            let mut mapped =
                windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
            ctx.immediate_context.Map(
                &self.vertex_buffer,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut mapped),
            )?;
            std::ptr::copy_nonoverlapping(
                verts.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of_val(&verts),
            );
            ctx.immediate_context.Unmap(&self.vertex_buffer, 0);
        }

        // Bind state and draw.
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.target_w as f32,
            Height: self.target_h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let strides = [VERTEX_SIZE];
        let offsets = [0u32];
        let vb_array = [Some(self.vertex_buffer.clone())];
        let rtv_array = [Some(self.target_rtv.clone())];
        let srv_array = [Some(cursor.srv.clone())];
        let samp_array = [Some(self.sampler.clone())];

        unsafe {
            ctx.immediate_context
                .OMSetRenderTargets(Some(&rtv_array), None);
            ctx.immediate_context.OMSetBlendState(
                &self.blend_state,
                Some(&[1.0, 1.0, 1.0, 1.0]),
                0xFFFFFFFF,
            );
            ctx.immediate_context.RSSetState(&self.rasterizer);
            ctx.immediate_context
                .RSSetViewports(Some(std::slice::from_ref(&viewport)));
            ctx.immediate_context.IASetInputLayout(&self.input_layout);
            ctx.immediate_context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(
                    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP.0,
                ));
            ctx.immediate_context.IASetVertexBuffers(
                0,
                1,
                Some(vb_array.as_ptr()),
                Some(strides.as_ptr()),
                Some(offsets.as_ptr()),
            );
            ctx.immediate_context.VSSetShader(&self.vs, None);
            ctx.immediate_context.PSSetShader(&self.ps, None);
            ctx.immediate_context
                .PSSetShaderResources(0, Some(&srv_array));
            ctx.immediate_context.PSSetSamplers(0, Some(&samp_array));
            ctx.immediate_context.Draw(4, 0);

            // Unbind the SRV so the same texture can be re-used as an RTV
            // later if needed (defensive; we only ever use it as SRV).
            let null_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            ctx.immediate_context
                .PSSetShaderResources(0, Some(&null_srv));
        }
        Ok(())
    }

    pub fn current_shape_dims(&self) -> Option<(u32, u32)> {
        self.cursor.as_ref().map(|c| (c.width, c.height))
    }
}

/// Clipped quad for one cursor-on-target draw. Coordinates are NDC for
/// position (so the vertex shader is a pass-through) and `[0,1]` for UV.
struct ClippedQuad {
    ndc_l: f32,
    ndc_r: f32,
    ndc_t: f32,
    ndc_b: f32,
    u_l: f32,
    u_r: f32,
    v_t: f32,
    v_b: f32,
}

fn compute_clipped_quad(
    cursor_x: i32,
    cursor_y: i32,
    cursor_w: u32,
    cursor_h: u32,
    target_w: u32,
    target_h: u32,
) -> Option<ClippedQuad> {
    let cw = cursor_w as i32;
    let ch = cursor_h as i32;
    let tw = target_w as i32;
    let th = target_h as i32;
    let dst_l = cursor_x.max(0);
    let dst_t = cursor_y.max(0);
    let dst_r = (cursor_x + cw).min(tw);
    let dst_b = (cursor_y + ch).min(th);
    if dst_l >= dst_r || dst_t >= dst_b {
        return None;
    }
    let u_l = (dst_l - cursor_x) as f32 / cw as f32;
    let v_t = (dst_t - cursor_y) as f32 / ch as f32;
    let u_r = (dst_r - cursor_x) as f32 / cw as f32;
    let v_b = (dst_b - cursor_y) as f32 / ch as f32;
    Some(ClippedQuad {
        ndc_l: (dst_l as f32 / tw as f32) * 2.0 - 1.0,
        ndc_r: (dst_r as f32 / tw as f32) * 2.0 - 1.0,
        ndc_t: 1.0 - (dst_t as f32 / th as f32) * 2.0,
        ndc_b: 1.0 - (dst_b as f32 / th as f32) * 2.0,
        u_l,
        u_r,
        v_t,
        v_b,
    })
}

pub(crate) fn compile_hlsl(
    src: &str,
    entry: &str,
    target: &str,
) -> EngineResult<(ID3DBlob, &'static [u8])> {
    let entry_c = CString::new(entry).map_err(|_| EngineError::NotInitialized)?;
    let target_c = CString::new(target).map_err(|_| EngineError::NotInitialized)?;
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            None,
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut blob,
            Some(&mut errors),
        )
    };
    if let Err(e) = hr {
        if let Some(err_blob) = errors {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    err_blob.GetBufferPointer() as *const u8,
                    err_blob.GetBufferSize(),
                )
            };
            let msg = String::from_utf8_lossy(bytes);
            eprintln!("[cursor_blit] D3DCompile error: {msg}");
        }
        return Err(EngineError::Win32(e));
    }
    let blob =
        blob.ok_or_else(|| EngineError::Win32(windows::core::Error::from_hresult(E_FAIL)))?;
    let bytecode: &'static [u8] = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    Ok((blob, bytecode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_centered_inside_target() {
        let q = compute_clipped_quad(100, 50, 32, 32, 1920, 1080).unwrap();
        assert!((q.u_l - 0.0).abs() < 1e-6);
        assert!((q.u_r - 1.0).abs() < 1e-6);
        assert!((q.v_t - 0.0).abs() < 1e-6);
        assert!((q.v_b - 1.0).abs() < 1e-6);
        // NDC top is +1, dst_t = 50 → ndc_t = 1 - (50/1080)*2
        let expected_top = 1.0_f32 - (50.0 / 1080.0) * 2.0;
        assert!((q.ndc_t - expected_top).abs() < 1e-6);
    }

    #[test]
    fn quad_clipped_top_left() {
        // Cursor anchored at (-10, -10), 32×32 → only 22×22 of it is on-screen
        let q = compute_clipped_quad(-10, -10, 32, 32, 1920, 1080).unwrap();
        // Source UV starts at 10/32 (we skip the first 10 source rows/cols).
        assert!((q.u_l - 10.0 / 32.0).abs() < 1e-6);
        assert!((q.v_t - 10.0 / 32.0).abs() < 1e-6);
        // Dest left/top are clamped to 0, so NDC corners are -1 / +1.
        assert!((q.ndc_l + 1.0).abs() < 1e-6);
        assert!((q.ndc_t - 1.0).abs() < 1e-6);
    }

    #[test]
    fn quad_off_screen_returns_none() {
        assert!(compute_clipped_quad(2000, 50, 32, 32, 1920, 1080).is_none());
        assert!(compute_clipped_quad(50, -100, 32, 32, 1920, 1080).is_none());
    }
}
