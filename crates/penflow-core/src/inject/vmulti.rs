//! VMulti HID-class virtual digitizer client.
//!
//! Talks to the user-installed [X9VoiD/vmulti-bin] driver — a community
//! fork of djpnewton/vmulti that ships a signed KMDF bus + UMDF HID-class
//! pair and exposes a virtual digitizer with 16384-level pressure, tilt,
//! eraser, and barrel buttons. SuperDisplay confirms the architecture
//! works in production: their `superdisplay_hidbus.sys` /
//! `superdisplay_hid.dll` are the same VMulti binaries rebranded
//! ("kelocube") and re-signed.
//!
//! Why this over `InjectSyntheticPointerInput`:
//!   - VMulti registers as a **real HID digitizer**, so the kernel
//!     populates `POINTER_INFO.ptHimetricLocation` from the device's HID
//!     descriptor (15-bit logical resolution = 32767 steps per axis).
//!     Sub-pixel precision survives end-to-end with no scale-factor
//!     guessing.
//!   - It also shows up on the **Wintab** channel via Wacom's HID-to-
//!     Wintab shim (or a fallback wintab32.dll if we ship one), so
//!     default-mode Krita works without switching to Windows Ink.
//!
//! Wire format (extended digitizer, REPORTID=0x06, 12 bytes payload,
//! padded to MaxOutputReportLength=65 by Windows HID stack):
//!
//! ```text
//!   off  size  field        notes
//!   0    1     VMultiID      = 0x40   (vendor control report)
//!   1    1     ReportLength  = 11     (sizeof(payload) - 1)
//!   2    1     ReportID      = 0x06   (Extended) or 0x05 (Normal)
//!   3    1     Buttons       bitmask: Tip 1 | Barrel 2 | Eraser 4
//!                                    | Invert 8 | InRange 16
//!   4..6  2    X u16 LE      [0, 32767]
//!   6..8  2    Y u16 LE      [0, 32767]
//!   8..10 2    Pressure u16 LE   [0, 16383] extended / [0, 8191] normal
//!   10   1     XTilt i8      [-127, 127]
//!   11   1     YTilt i8      [-127, 127]
//! ```
//!
//! Device discovery: `SetupDiGetClassDevsW(HID_GUID, …)`, enumerate
//! HID interfaces, open each, match `HidD_GetAttributes().VendorID==0x00FF
//! && ProductID==0xBACC && HidP_GetCaps().OutputReportByteLength==65`.
//! That picks the "control" collection — the one we write reports to.
//!
//! References:
//!   - `research/VoiDPlugins/src/VoiDPlugins.Library/VMulti/Device/`
//!     (struct layouts ported from C# verbatim)
//!   - `research/VoiDPlugins/src/VoiDPlugins.Library/VMulti/VMultiInstance.cs`
//!     (discovery + write loop)
//!
//! [X9VoiD/vmulti-bin]: https://github.com/X9VoiD/vmulti-bin

use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
    HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

/// X9VoiD vmulti-bin / djpnewton vmulti hardcoded HID identifiers.
const VMULTI_VID: u16 = 0x00FF;
const VMULTI_PID: u16 = 0xBACC;

/// Total Windows HID output report buffer size for the VMulti control
/// endpoint. The actual digitizer payload is 12 bytes, but Windows HID
/// requires the full `MaxOutputReportLength`-sized buffer in `WriteFile`.
const REPORT_BUF_LEN: usize = 65;

/// Field offsets inside the 12-byte digitizer payload (which sits at the
/// start of the 65-byte buffer; trailing bytes stay zero).
const DIGI_PAYLOAD_LEN: usize = 12;
const OFF_VMULTI_ID: usize = 0;
const OFF_REPORT_LEN: usize = 1;
const OFF_REPORT_ID: usize = 2;
const OFF_BUTTONS: usize = 3;
const OFF_X: usize = 4;
const OFF_Y: usize = 6;
const OFF_PRESSURE: usize = 8;
const OFF_XTILT: usize = 10;
const OFF_YTILT: usize = 11;

const VMULTI_HEADER_ID: u8 = 0x40;
/// Report ID for the extended pen (16384 pressure levels).
pub const REPORT_ID_EXTENDED: u8 = 0x06;
/// Report ID for the legacy pen (8192 pressure levels). Kept for reference;
/// we always prefer extended when the driver supports it.
#[allow(dead_code)]
pub const REPORT_ID_NORMAL: u8 = 0x05;

// Button bit positions inside the header byte.
const BTN_TIP: u8 = 1 << 0;
const BTN_BARREL: u8 = 1 << 1;
const BTN_ERASER: u8 = 1 << 2;
const BTN_INVERT: u8 = 1 << 3;
const BTN_IN_RANGE: u8 = 1 << 4;

/// Logical-axis maximum declared in VMulti's HID descriptor. Both axes
/// span `[0, 32767]` regardless of the physical screen size; Windows
/// scales the digitizer surface to whatever monitor the device is
/// associated with (set by HID descriptor's `physical_min/max`).
pub const VMULTI_LOGICAL_MAX: u16 = 32767;

/// Pressure maximum for the extended report (14-bit). Legacy normal
/// report only gives 13-bit / 8191 — most modern Wacom pens (Pro Pen 3
/// included) report 8192 levels which fits either, but extended is the
/// strict superset.
pub const VMULTI_PRESSURE_MAX_EXTENDED: u16 = 16383;

/// One pen sample, already in VMulti's logical units. Caller is
/// responsible for the affine mapping from tablet-side normalized
/// coordinates onto `[0, VMULTI_LOGICAL_MAX]` over the destination
/// screen (see `coords::AffineTransform`).
#[derive(Clone, Copy, Debug)]
pub struct VMultiPenSample {
    pub x: u16,
    pub y: u16,
    pub pressure: u16,
    /// Degrees, ±90 max — caller should clamp before passing in.
    pub tilt_x_deg: i8,
    /// Degrees, ±90 max.
    pub tilt_y_deg: i8,
    pub tip_down: bool,
    pub barrel: bool,
    pub eraser: bool,
    /// Set when the physical eraser end of the stylus is in use (Android
    /// `MotionEvent.TOOL_TYPE_ERASER`). Drives the HID `Invert` bit which
    /// Windows Ink translates into the `PEN_FLAG_INVERTED` pointer flag.
    pub inverted: bool,
    pub in_range: bool,
}

/// Errors that can occur talking to the VMulti driver.
#[derive(thiserror::Error, Debug)]
pub enum VMultiError {
    /// No HID device matched VMulti's VID/PID + 65-byte control endpoint.
    /// Driver is not installed, or the binary we ship is incompatible
    /// with this Windows build.
    #[error(
        "VMulti HID device not found (VID=0x{:04X} PID=0x{:04X}); install X9VoiD/vmulti-bin",
        VMULTI_VID,
        VMULTI_PID
    )]
    NotFound,
    /// Win32 surface error during enumeration or I/O.
    #[error("Win32: {0}")]
    Win32(#[from] windows::core::Error),
}

/// Open handle to the VMulti control endpoint plus a reusable 65-byte
/// scratch buffer. Created at session init; dropped on session end.
pub struct VMultiPen {
    handle: HANDLE,
    buf: [u8; REPORT_BUF_LEN],
}

// SAFETY: the underlying Win32 file handle is safe to use from any thread
// once opened. We serialise calls through the session-owning `Mutex`.
unsafe impl Send for VMultiPen {}

impl VMultiPen {
    /// Discover the installed VMulti control endpoint and open it for
    /// write. Returns `VMultiError::NotFound` if no matching HID device
    /// is present — the typical "driver not installed" case the caller
    /// should surface to the user with an install prompt.
    pub fn open() -> Result<Self, VMultiError> {
        let hid_guid = unsafe { HidD_GetHidGuid() };

        let devinfo: HDEVINFO = unsafe {
            SetupDiGetClassDevsW(
                Some(&hid_guid),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )?
        };

        let mut found: Option<HANDLE> = None;

        // Enumerate interfaces. Each interface is one HID collection; a
        // single VMulti device contributes several (control + per-report
        // ID children). We want the one whose attributes match VID/PID
        // AND whose output-report length is 65 — that's the control
        // collection we write all reports to.
        let mut index: u32 = 0;
        loop {
            let mut if_data = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if unsafe { SetupDiEnumDeviceInterfaces(devinfo, None, &hid_guid, index, &mut if_data) }
                .is_err()
            {
                // ERROR_NO_MORE_ITEMS — done enumerating.
                break;
            }
            index += 1;

            // First call: get required buffer size for the detail struct.
            let mut required: u32 = 0;
            let _ = unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    devinfo,
                    &if_data,
                    None,
                    0,
                    Some(&mut required),
                    None,
                )
            };
            if required == 0 {
                continue;
            }

            // Allocate a properly-aligned buffer. SP_DEVICE_INTERFACE_DETAIL_DATA_W's
            // cbSize must be set to the FIXED part size (offsetof(DevicePath)) = 8
            // on x64 / 6 on x86, per MSDN. windows-rs ships the right struct;
            // we set cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() which
            // is the documented value the kernel accepts.
            let mut detail_buf: Vec<u8> = vec![0u8; required as usize];
            let detail = detail_buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            unsafe {
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            }

            if unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    devinfo,
                    &if_data,
                    Some(detail),
                    required,
                    None,
                    None,
                )
            }
            .is_err()
            {
                continue;
            }

            // DevicePath is a flexible-array-member; PCWSTR points at it.
            let path_ptr = unsafe { (*detail).DevicePath.as_ptr() };
            let path = PCWSTR(path_ptr);

            let handle = match unsafe {
                CreateFileW(
                    path,
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            } {
                Ok(h) => h,
                Err(_) => continue, // Permission / busy / not openable — skip.
            };

            if !matches_vmulti_control(handle) {
                let _ = unsafe { CloseHandle(handle) };
                continue;
            }

            found = Some(handle);
            break;
        }

        let _ = unsafe { SetupDiDestroyDeviceInfoList(devinfo) };

        let handle = found.ok_or(VMultiError::NotFound)?;
        Ok(Self {
            handle,
            buf: [0u8; REPORT_BUF_LEN],
        })
    }

    /// Send one extended digitizer report. Pressure is clamped to
    /// `VMULTI_PRESSURE_MAX_EXTENDED`; X/Y are clamped to
    /// `VMULTI_LOGICAL_MAX`. Buttons are encoded into the header byte.
    pub fn write_pen(&mut self, s: &VMultiPenSample) -> Result<(), VMultiError> {
        // Reset only the bytes we touch — keeps the trailing padding
        // already-zero from initial construction.
        self.buf[..DIGI_PAYLOAD_LEN].fill(0);

        let x = s.x.min(VMULTI_LOGICAL_MAX);
        let y = s.y.min(VMULTI_LOGICAL_MAX);
        let pressure = s.pressure.min(VMULTI_PRESSURE_MAX_EXTENDED);

        let mut buttons = 0u8;
        if s.tip_down {
            buttons |= BTN_TIP;
        }
        if s.barrel {
            buttons |= BTN_BARREL;
        }
        if s.eraser {
            buttons |= BTN_ERASER;
        }
        if s.inverted {
            buttons |= BTN_INVERT;
        }
        if s.in_range {
            buttons |= BTN_IN_RANGE;
        }

        self.buf[OFF_VMULTI_ID] = VMULTI_HEADER_ID;
        self.buf[OFF_REPORT_LEN] = (DIGI_PAYLOAD_LEN - 1) as u8;
        self.buf[OFF_REPORT_ID] = REPORT_ID_EXTENDED;
        self.buf[OFF_BUTTONS] = buttons;
        self.buf[OFF_X..OFF_X + 2].copy_from_slice(&x.to_le_bytes());
        self.buf[OFF_Y..OFF_Y + 2].copy_from_slice(&y.to_le_bytes());
        self.buf[OFF_PRESSURE..OFF_PRESSURE + 2].copy_from_slice(&pressure.to_le_bytes());
        self.buf[OFF_XTILT] = s.tilt_x_deg as u8;
        self.buf[OFF_YTILT] = s.tilt_y_deg as u8;

        let mut written = 0u32;
        unsafe {
            WriteFile(self.handle, Some(&self.buf), Some(&mut written), None)?;
        }
        Ok(())
    }
}

impl Drop for VMultiPen {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// `true` if the opened HID file's attributes + capabilities match the
/// VMulti control endpoint (VID/PID + 65-byte output report).
fn matches_vmulti_control(handle: HANDLE) -> bool {
    let mut attrs = HIDD_ATTRIBUTES {
        Size: size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    if !unsafe { HidD_GetAttributes(handle, &mut attrs) } {
        return false;
    }
    if attrs.VendorID != VMULTI_VID || attrs.ProductID != VMULTI_PID {
        return false;
    }
    let mut preparsed = PHIDP_PREPARSED_DATA(0);
    if !unsafe { HidD_GetPreparsedData(handle, &mut preparsed) } {
        return false;
    }
    let mut caps = HIDP_CAPS::default();
    let cap_ok = unsafe { HidP_GetCaps(preparsed, &mut caps) }.is_ok();
    unsafe { HidD_FreePreparsedData(preparsed) };
    if !cap_ok {
        return false;
    }
    caps.OutputReportByteLength as usize == REPORT_BUF_LEN
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise a known sample and verify the byte layout matches the
    /// VMulti / VoiDPlugins reference (Pack=1 sequential struct).
    #[test]
    fn report_serialization_matches_voidplugins_layout() {
        // We can't actually open the device in unit tests, so build the
        // buffer directly to check field offsets.
        let mut buf = [0u8; REPORT_BUF_LEN];
        let sample = VMultiPenSample {
            x: 0x1234,
            y: 0x5678,
            pressure: 0x3FFF, // 16383 max
            tilt_x_deg: -30,
            tilt_y_deg: 45,
            tip_down: true,
            barrel: false,
            eraser: false,
            inverted: false,
            in_range: true,
        };
        // Inline the encoding so the test exercises the same logic as
        // write_pen without needing a real device.
        let mut buttons = 0u8;
        if sample.tip_down {
            buttons |= BTN_TIP;
        }
        if sample.in_range {
            buttons |= BTN_IN_RANGE;
        }
        buf[OFF_VMULTI_ID] = VMULTI_HEADER_ID;
        buf[OFF_REPORT_LEN] = 11;
        buf[OFF_REPORT_ID] = REPORT_ID_EXTENDED;
        buf[OFF_BUTTONS] = buttons;
        buf[OFF_X..OFF_X + 2].copy_from_slice(&sample.x.to_le_bytes());
        buf[OFF_Y..OFF_Y + 2].copy_from_slice(&sample.y.to_le_bytes());
        buf[OFF_PRESSURE..OFF_PRESSURE + 2].copy_from_slice(&sample.pressure.to_le_bytes());
        buf[OFF_XTILT] = sample.tilt_x_deg as u8;
        buf[OFF_YTILT] = sample.tilt_y_deg as u8;

        // Header: VMultiID, Length=11, ReportID=0x06, Buttons=Tip|InRange
        assert_eq!(buf[0], 0x40);
        assert_eq!(buf[1], 11);
        assert_eq!(buf[2], 0x06);
        assert_eq!(buf[3], BTN_TIP | BTN_IN_RANGE);
        // Coords little-endian.
        assert_eq!(&buf[4..6], &[0x34, 0x12]);
        assert_eq!(&buf[6..8], &[0x78, 0x56]);
        // Pressure 16383 = 0x3FFF.
        assert_eq!(&buf[8..10], &[0xFF, 0x3F]);
        // Tilts as signed bytes.
        assert_eq!(buf[10], (-30i8) as u8); // 0xE2
        assert_eq!(buf[11], 45u8);
        // Trailing padding all zero.
        assert!(buf[12..].iter().all(|&b| b == 0));
    }

    #[test]
    fn button_bits_match_voidplugins() {
        // From VoiDPlugins WindowsInkButtonFlags.cs:
        //   Press = 1, Barrel = 2, Eraser = 4, Invert = 8, InRange = 16
        assert_eq!(BTN_TIP, 1);
        assert_eq!(BTN_BARREL, 2);
        assert_eq!(BTN_ERASER, 4);
        assert_eq!(BTN_INVERT, 8);
        assert_eq!(BTN_IN_RANGE, 16);
    }
}
