//! Video encoder abstraction.
//!
//! Sunshine-inspired two-level shape (design.md §6.3): an `EncoderBackend`
//! enumerates and instantiates `EncodeSession`s, then the session does the
//! per-frame work. v1.0 ships one backend (`mf` for Windows Media Foundation
//! HEVC). The macOS `videotoolbox` backend will plug in here post-v1.0
//! without changing the trait surface.

#[cfg(windows)]
pub mod mf;
pub mod sps_patcher;

use crate::error::EngineResult;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    /// AVC / H.264. Default for low-latency streaming on Qualcomm Snapdragon
    /// targets — `c2.qti.avc.decoder.low_latency` exists as a dedicated
    /// low-latency variant on Adreno (HEVC has no such variant), and
    /// moonlight-android #1471 documents specific HEVC corruption /
    /// runaway-latency on Snapdragon 8s Gen 3 that doesn't affect H.264.
    /// Encode cost on Blackwell NVENC is roughly equal to HEVC (NVENC AppNote
    /// 13.0); bandwidth is ~1.5× HEVC at equivalent quality but irrelevant
    /// over USB ADB / USB bulk.
    H264,
    /// HEVC / H.265. Lower bandwidth; kept as opt-in for Wi-Fi or other
    /// bandwidth-constrained transports, or for clients without a stable
    /// `avc.decoder.low_latency` path.
    Hevc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    /// 4:2:0 semi-planar; the canonical hardware-encoder input on Windows.
    Nv12,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub codec: Codec,
    pub input_format: PixelFormat,
}

/// One encoded packet as it leaves the encoder. Bytes are Annex-B (the MF
/// HEVC encoder emits with start codes) and the parameter sets (VPS/SPS/PPS)
/// are repeated on every IDR (HANDOFF §4.5 finding #5).
pub struct EncodedPacket {
    pub bytes: Vec<u8>,
    /// Caller-supplied PTS in nanoseconds (we forward the value passed to
    /// `submit_frame`). The MF encoder doesn't reorder, so packet PTS == input
    /// frame PTS.
    pub pts_ns: i64,
    pub is_keyframe: bool,
    /// Wall-clock time the encoder MFT held onto this frame
    /// (`ProcessInput` → packet popped via `try_packet`). Includes the small
    /// async-event drain in between but is dominated by the actual encode
    /// step (~3 ms NVENC HEVC P1 ULL on RTX 5070). `None` if the backend
    /// doesn't measure.
    pub encode_us: Option<u32>,
}

#[cfg(windows)]
pub trait EncoderBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_codecs(&self) -> &[Codec];
    fn supported_input_formats(&self) -> &[PixelFormat];
    /// Live-probe + instantiate a session. The backend is responsible for
    /// rejecting MFTs that don't bind to `ctx.device` (gate-1 finding —
    /// vendor MFTs are registered system-wide and the wrong one fails with
    /// `E_OUTOFMEMORY` at SET_D3D_MANAGER).
    fn make_session(
        &self,
        ctx: &crate::d3d11::D3d11Context,
        cfg: SessionConfig,
    ) -> EngineResult<Box<dyn EncodeSession>>;
}

#[cfg(windows)]
pub trait EncodeSession: Send {
    fn input_format(&self) -> PixelFormat;

    /// Submit one input frame. MF hardware MFTs are async — internally this
    /// drains pending output events first, then waits for `METransformNeedInput`,
    /// then `ProcessInput`. `force_idr` requests an IDR via
    /// `CODECAPI_AVEncVideoForceKeyFrame` (gate-1 verified to work on NVIDIA;
    /// the call is a no-op on backends that don't support it, which would
    /// fall back to periodic IDR per design §6.4.1).
    ///
    /// `tex` is captured by reference inside an `IMFSample` (zero-copy via
    /// `MFCreateDXGISurfaceBuffer`); the caller MUST pass the SAME stable
    /// texture every call (typically `ColorConverter::output_texture()`) so
    /// MF can keep its driver-side resource cache warm.
    fn submit_frame(
        &mut self,
        tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        pts_ns: i64,
        force_idr: bool,
    ) -> EngineResult<()>;

    /// Pop the next encoded packet, if one is ready. Non-blocking — drains
    /// any pending `METransformHaveOutput` events first and then returns the
    /// front of the internal queue.
    fn try_packet(&mut self) -> EngineResult<Option<EncodedPacket>>;
}
