//! Frame capture.
//!
//! Today: Windows DXGI Output Duplication only. The macOS path
//! (`screencapturekit`) lands post-v1.0; when it does, lift `Capturer` to a
//! trait and have both backends implement it. Until then the engine talks to
//! `dxgi::DxgiCapturer` directly — premature trait abstraction is the
//! enemy of clarity.

#[cfg(windows)]
pub mod cursor_shape;
#[cfg(windows)]
pub mod dxgi;
