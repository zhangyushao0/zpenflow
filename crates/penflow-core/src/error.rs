//! Engine-wide error type.
//!
//! Most errors fan in from `windows::core::Error` (the underlying Windows /
//! D3D11 / DXGI / MF HRESULT). The variants below capture the smaller set of
//! engine-level conditions where the caller actually has a different recovery
//! action than "log the HRESULT and bail".

use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    /// Pass-through for any Windows API failure that doesn't need
    /// engine-specific handling. Holds the original HRESULT + message.
    #[error("Windows API call failed: {0}")]
    Win32(#[from] windows::core::Error),

    /// `IDXGIFactory6::EnumAdapterByGpuPreference` (or similar) returned no
    /// adapter. Should never happen on a real Windows install — typically
    /// surfaces if the process is running headless under WACK / WinPE.
    #[error("no DXGI adapter matched the requested preference")]
    NoAdapter,

    /// The picked adapter has no display outputs of its own. Modern NVIDIA
    /// drivers expose multiple logical adapters per physical GPU; only one
    /// of them carries the desktop outputs (gate-2 finding, HANDOFF §4.4b).
    /// The engine must pick a different adapter or fail loudly.
    #[error("adapter '{name}' (LUID 0x{luid:016x}) has no display outputs")]
    AdapterHasNoOutputs { name: String, luid: i64 },

    /// User picked an output that lives on an adapter different from the one
    /// the D3D11 device was created on. `IDXGIOutputDuplication` rejects that
    /// configuration. Cross-adapter capture is out of scope for v1.0
    /// (design §6.1).
    #[error(
        "selected output (adapter LUID 0x{output_luid:016x}) is not on the engine's D3D11 device \
         (adapter LUID 0x{device_luid:016x}); cross-adapter capture is not supported"
    )]
    AdapterMismatch { output_luid: i64, device_luid: i64 },

    /// `IDXGIOutputDuplication::AcquireNextFrame` returned `DXGI_ERROR_ACCESS_LOST`
    /// or `DXGI_ERROR_ACCESS_DENIED`. The capturer should reinit transparently;
    /// this variant exists for cases where reinit also fails.
    #[error("DXGI Output Duplication access lost; reinit failed too")]
    AccessLostUnrecoverable,

    /// `MFTEnumEx` produced HEVC encoder MFTs but none of them accepted our
    /// D3D11 device under the configured media types. Either all MFTs are
    /// vendor-mismatched (gate-1 finding) or a driver bug is in play.
    #[error("no hardware HEVC encoder MFT compatible with the chosen D3D11 device")]
    NoCompatibleEncoder,

    /// The capture loop went `dur` without seeing a frame. Distinct from
    /// `Win32` because the caller handles it (typically: re-emit the keepalive
    /// texture so the encoder doesn't go cold).
    #[error("capture timed out (no frame in {0:?})")]
    CaptureTimeout(Duration),

    /// The engine wasn't started, or has already been torn down.
    #[error("engine is not in a usable state")]
    NotInitialized,
}

pub type EngineResult<T> = Result<T, EngineError>;
