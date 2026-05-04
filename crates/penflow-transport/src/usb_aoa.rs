//! Android Open Accessory (AOA) v2 bulk-endpoint transport.
//!
//! Bypasses ADB entirely. The host (PC) negotiates AOA mode with an
//! attached Android device via standard USB control transfers, the device
//! re-enumerates as a Google-vendor "accessory" device with two bulk
//! endpoints, and we exchange protocol-level frames over those endpoints.
//!
//! ## Why
//!
//! ADB's `localabstract:` reverse tunnel adds 2-4 ms typical latency / much
//! more tail jitter on top of raw USB. Parsec, Astropad, SuperDisplay all
//! avoid ADB for this reason. AOA gives us direct USB bulk endpoints with
//! no daemon in the middle.
//!
//! ## Wire compatibility
//!
//! The protocol-level wire format (`[u8 type | u32 BE len | payload]`) is
//! unchanged. AOA replaces only the byte-transport substrate. Existing
//! `read_frame` / `write_frame` work as-is over the bulk endpoints.
//!
//! ## Lifecycle
//!
//! 1. Find an Android device via `nusb::list_devices()`. Two cases:
//!    - Already in accessory mode (vendor 0x18D1, product 0x2D00 / 0x2D01).
//!    - Otherwise: probe with the AOA `GET_PROTOCOL` (51) control transfer.
//! 2. If not yet in accessory mode: send the six accessory-identification
//!    strings via `SEND_STRING` (52), then `START_ACCESSORY` (53). The
//!    device USB-disconnects.
//! 3. Poll for the re-enumerated 0x18D1:0x2D00/0x2D01 device.
//! 4. Claim interface 0, find the bulk IN and bulk OUT endpoints, build
//!    async tokio reader/writer wrappers around the per-transfer
//!    [`Interface::bulk_in`] / [`Interface::bulk_out`] futures.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nusb::transfer::{
    Completion, Control, ControlType, Recipient, RequestBuffer, ResponseBuffer, TransferFuture,
};
use nusb::{Device, DeviceInfo, Interface};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;

use crate::{Transport, TransportStream};

// ===================================================================
// AOA protocol constants (Android Open Accessory v2 spec)
// ===================================================================

const AOA_GET_PROTOCOL: u8 = 51;
const AOA_SEND_STRING: u8 = 52;
const AOA_START_ACCESSORY: u8 = 53;

const AOA_STRING_MANUFACTURER: u16 = 0;
const AOA_STRING_MODEL: u16 = 1;
const AOA_STRING_DESCRIPTION: u16 = 2;
const AOA_STRING_VERSION: u16 = 3;
const AOA_STRING_URI: u16 = 4;
const AOA_STRING_SERIAL: u16 = 5;

const GOOGLE_VID: u16 = 0x18D1;
const ACCESSORY_PID: u16 = 0x2D00;
const ACCESSORY_ADB_PID: u16 = 0x2D01;

// ===================================================================
// Configuration
// ===================================================================

/// Six accessory-identification strings the Android side filters on in
/// its `res/xml/accessory_filter.xml`. Must match exactly between host
/// and device or Android won't dispatch the `USB_ACCESSORY_ATTACHED`
/// intent to our app.
#[derive(Clone, Debug)]
pub struct AccessoryStrings {
    /// Manufacturer name. Filter: `<usb-accessory manufacturer="…" />`.
    pub manufacturer: String,
    /// Model name. Filter: `model="…"`.
    pub model: String,
    /// Free-form human-readable description.
    pub description: String,
    /// Accessory version. Filter: `version="…"`.
    pub version: String,
    /// URI shown to user if no app filter matches (Play Store link
    /// or similar). May be empty.
    pub uri: String,
    /// Optional serial number. May be empty.
    pub serial: String,
}

impl AccessoryStrings {
    /// Default strings the Penflow Android client filters on. Keep in
    /// sync with `android/app/src/main/res/xml/accessory_filter.xml`.
    pub fn default_penflow() -> Self {
        Self {
            manufacturer: "Penflow".into(),
            model: "Penflow Server".into(),
            description: "Low-latency PC desktop streaming for pen displays".into(),
            version: "1.0".into(),
            uri: "https://github.com/zhangyushaow/zpenflow".into(),
            serial: String::new(),
        }
    }
}

// ===================================================================
// Transport
// ===================================================================

/// AOA-based USB bulk-endpoint transport. Build with
/// [`UsbAoaTransport::new`] and pass to the session orchestrator
/// like any other [`Transport`].
pub struct UsbAoaTransport {
    strings: AccessoryStrings,
    /// How long to poll for the re-enumerated accessory device after
    /// `START_ACCESSORY` before giving up.
    reenumerate_timeout: Duration,
    state: Mutex<State>,
}

enum State {
    Idle,
    /// Held to keep the interface claim alive for the stream's lifetime.
    Connected { _interface: Arc<Interface> },
}

impl UsbAoaTransport {
    /// Build a transport that will negotiate AOA with the first
    /// connected Android device and expose its bulk endpoints as a
    /// [`TransportStream`] from `accept`.
    pub fn new(strings: AccessoryStrings) -> Self {
        Self {
            strings,
            reenumerate_timeout: Duration::from_secs(8),
            state: Mutex::new(State::Idle),
        }
    }

    /// Override how long `accept` will poll for the AOA device to
    /// re-enumerate after `START_ACCESSORY`. Default 8 s.
    pub fn with_reenumerate_timeout(mut self, t: Duration) -> Self {
        self.reenumerate_timeout = t;
        self
    }
}

#[async_trait]
impl Transport for UsbAoaTransport {
    async fn accept(&self) -> io::Result<TransportStream> {
        let candidate = pick_candidate_device()?;
        let already_accessory = candidate.vendor_id() == GOOGLE_VID
            && (candidate.product_id() == ACCESSORY_PID
                || candidate.product_id() == ACCESSORY_ADB_PID);

        let (accessory_device, accessory_pid) = if already_accessory {
            let dev = candidate.open().map_err(io_other)?;
            (dev, candidate.product_id())
        } else {
            let dev = candidate.open().map_err(io_other)?;
            // Need to claim interface 0 to send vendor control transfers
            // on Windows. (On Linux/macOS it's allowed without claim, but
            // claiming is harmless.)
            let iface = dev.claim_interface(0).map_err(io_other)?;
            negotiate_aoa(&iface, &self.strings)?;
            // Drop the iface + dev so the device can disconnect cleanly.
            drop(iface);
            drop(dev);
            wait_for_accessory_reenum(self.reenumerate_timeout).await?
        };

        let (interface, ep_in, ep_out) = claim_bulk_interface(&accessory_device)?;

        // **Clear any stale endpoint state from a previous session.** When
        // the previous run died mid-stream (e.g. PFD finalized on Android
        // → bulk_in interrupted, or session crash) the kernel-side bulk
        // buffer might still hold un-read bytes — reading them on the new
        // connection produces "expected MSG_HELLO_ANDROID, got 0xff" (a
        // stale MSG_ANDROID_GOODBYE byte). `clear_halt` resets the data
        // toggle + stall state and Linux+Windows backends usually drop
        // pending buffered data along with it.
        if let Err(e) = interface.clear_halt(ep_in) {
            eprintln!("[usb_aoa] clear_halt(IN endpoint 0x{ep_in:02x}) failed: {e:?} (continuing)");
        }
        if let Err(e) = interface.clear_halt(ep_out) {
            eprintln!("[usb_aoa] clear_halt(OUT endpoint 0x{ep_out:02x}) failed: {e:?} (continuing)");
        }

        // Belt-and-braces drain: do a few short non-blocking-style reads
        // and discard whatever shows up before the protocol expects its
        // first byte. Bounded so we never sit here forever if the device
        // is genuinely stuck.
        drain_stale_bytes(&interface, ep_in).await;

        let interface = Arc::new(interface);

        let reader = UsbReader::new(Arc::clone(&interface), ep_in);
        let writer = UsbWriter::new(Arc::clone(&interface), ep_out);

        *self.state.lock().await = State::Connected {
            _interface: Arc::clone(&interface),
        };

        let peer_label = format!("usb:VID_{:04X}&PID_{:04X}", GOOGLE_VID, accessory_pid);
        Ok(TransportStream {
            reader: Box::new(reader),
            writer: Box::new(writer),
            peer_label,
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        *self.state.lock().await = State::Idle;
        Ok(())
    }
}

// ===================================================================
// AOA negotiation helpers
// ===================================================================

/// Find an Android device that's either already in AOA mode or
/// supports AOA negotiation. Logs each enumerated device and the
/// reason for skipping non-candidates so it's clear why a device the
/// operator expects to see was rejected.
fn pick_candidate_device() -> io::Result<DeviceInfo> {
    let devices: Vec<DeviceInfo> = nusb::list_devices().map_err(io_other)?.collect();
    eprintln!("[usb_aoa] enumerating {} USB device(s):", devices.len());
    for d in &devices {
        eprintln!(
            "  VID:PID = {:04X}:{:04X}  class={:#04x}  bus={} addr={}  manufacturer={:?} product={:?}",
            d.vendor_id(),
            d.product_id(),
            d.class(),
            d.bus_number(),
            d.device_address(),
            d.manufacturer_string(),
            d.product_string(),
        );
    }

    if let Some(d) = devices.iter().find(|d| {
        d.vendor_id() == GOOGLE_VID
            && (d.product_id() == ACCESSORY_PID || d.product_id() == ACCESSORY_ADB_PID)
    }) {
        eprintln!(
            "[usb_aoa] found device already in AOA mode: {:04X}:{:04X}",
            d.vendor_id(),
            d.product_id()
        );
        return Ok(d.clone());
    }

    // Probe each plausible device with GET_PROTOCOL.
    for d in &devices {
        if is_obviously_not_android(d) {
            eprintln!(
                "[usb_aoa]   skip {:04X}:{:04X}: class {:#04x} is not Android-class",
                d.vendor_id(),
                d.product_id(),
                d.class()
            );
            continue;
        }
        eprintln!(
            "[usb_aoa]   probing {:04X}:{:04X} for AOA support...",
            d.vendor_id(),
            d.product_id()
        );
        match probe_aoa_version(d) {
            Ok(v) => {
                eprintln!("[usb_aoa]     OK — AOA version {v}");
                return Ok(d.clone());
            }
            Err(e) => {
                eprintln!("[usb_aoa]     skip: {e}");
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no Android device with AOA support found. Common causes on Windows:\n\
         (1) `adb` server has the device claimed — run `adb kill-server` first;\n\
         (2) the device's interface 0 uses a vendor driver that blocks WinUSB \
             (you may need Zadig to bind WinUSB);\n\
         (3) USB debugging is disabled on the device (toggle it off then on \
             so the driver re-binds).",
    ))
}

fn probe_aoa_version(info: &DeviceInfo) -> io::Result<u16> {
    let dev = info.open().map_err(io_other)?;
    let iface = dev.claim_interface(0).map_err(io_other)?;
    let mut buf = [0u8; 2];
    let n = iface
        .control_in_blocking(
            Control {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: AOA_GET_PROTOCOL,
                value: 0,
                index: 0,
            },
            &mut buf,
            Duration::from_millis(500),
        )
        .map_err(io_other)?;
    if n < 2 {
        return Err(io_other("GET_PROTOCOL returned <2 bytes"));
    }
    let v = u16::from_le_bytes([buf[0], buf[1]]);
    if v < 1 {
        return Err(io_other(format!("AOA protocol version {v} unsupported")));
    }
    Ok(v)
}

/// Heuristic: skip devices that are obviously not Android phones (hubs,
/// HID, audio, etc.). Speeds up the probe scan.
fn is_obviously_not_android(d: &DeviceInfo) -> bool {
    matches!(d.class(), 0x09 | 0x03 | 0x01 | 0x07 | 0x08 | 0x02 | 0x0B | 0x0E | 0xE0)
}

fn negotiate_aoa(iface: &Interface, s: &AccessoryStrings) -> io::Result<()> {
    // Verify version on this claimed handle (the earlier probe was on a
    // throwaway claim). 1+ is required; we don't do v2-only features.
    let mut buf = [0u8; 2];
    iface
        .control_in_blocking(
            Control {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: AOA_GET_PROTOCOL,
                value: 0,
                index: 0,
            },
            &mut buf,
            Duration::from_millis(500),
        )
        .map_err(io_other)?;

    send_string(iface, AOA_STRING_MANUFACTURER, &s.manufacturer)?;
    send_string(iface, AOA_STRING_MODEL, &s.model)?;
    send_string(iface, AOA_STRING_DESCRIPTION, &s.description)?;
    send_string(iface, AOA_STRING_VERSION, &s.version)?;
    send_string(iface, AOA_STRING_URI, &s.uri)?;
    send_string(iface, AOA_STRING_SERIAL, &s.serial)?;

    iface
        .control_out_blocking(
            Control {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: AOA_START_ACCESSORY,
                value: 0,
                index: 0,
            },
            &[],
            Duration::from_millis(500),
        )
        .map_err(io_other)?;
    Ok(())
}

fn send_string(iface: &Interface, index: u16, s: &str) -> io::Result<()> {
    // AOA spec requires null-terminated strings.
    let mut data = Vec::with_capacity(s.len() + 1);
    data.extend_from_slice(s.as_bytes());
    data.push(0);
    iface
        .control_out_blocking(
            Control {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: AOA_SEND_STRING,
                value: 0,
                index,
            },
            &data,
            Duration::from_millis(500),
        )
        .map_err(io_other)?;
    Ok(())
}

async fn wait_for_accessory_reenum(timeout: Duration) -> io::Result<(Device, u16)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(devices) = nusb::list_devices() {
            for d in devices {
                if d.vendor_id() == GOOGLE_VID
                    && (d.product_id() == ACCESSORY_PID || d.product_id() == ACCESSORY_ADB_PID)
                {
                    let pid = d.product_id();
                    // Brief settle delay — some devices appear in the
                    // list before they're ready to accept interface
                    // claims.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let dev = d.open().map_err(io_other)?;
                    return Ok((dev, pid));
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "AOA device did not re-enumerate after START_ACCESSORY",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Drain any bytes the previous (probably-crashed) session left in the
/// kernel-side bulk-IN buffer for this endpoint. Wraps an in-flight
/// `bulk_in` future in a `tokio::time::timeout` so we don't block
/// forever — anything that arrives within the budget is discarded;
/// nothing arriving means the buffer is genuinely empty and we can
/// safely start the protocol handshake.
async fn drain_stale_bytes(interface: &Interface, ep_in: u8) {
    use nusb::transfer::RequestBuffer;
    // Drain budget: small enough that a healthy device with no stale
    // bytes barely notices, large enough that a couple of un-acked
    // bulk packets will surface within the window.
    let budget = Duration::from_millis(150);
    let deadline = Instant::now() + budget;
    let mut total = 0usize;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let req = RequestBuffer::new(4096);
        let fut = interface.bulk_in(ep_in, req);
        match tokio::time::timeout(remaining, fut).await {
            Ok(completion) => match completion.into_result() {
                Ok(data) if data.is_empty() => break,
                Ok(data) => {
                    total += data.len();
                    eprintln!(
                        "[usb_aoa] drained {} stale byte(s) from IN endpoint",
                        data.len()
                    );
                }
                Err(_e) => break,
            },
            Err(_) => {
                // Timeout — no stale data. The pending future is dropped,
                // which nusb should cancel.
                break;
            }
        }
    }
    if total > 0 {
        eprintln!("[usb_aoa] total drained: {total} byte(s)");
    }
}

/// Claim interface 0 of the accessory-mode device and return the
/// (interface, IN endpoint addr, OUT endpoint addr) triple.
fn claim_bulk_interface(dev: &Device) -> io::Result<(Interface, u8, u8)> {
    let interface = dev.claim_interface(0).map_err(io_other)?;

    let mut ep_in: Option<u8> = None;
    let mut ep_out: Option<u8> = None;
    for cfg in dev.configurations() {
        for iface in cfg.interfaces() {
            for alt in iface.alt_settings() {
                for ep in alt.endpoints() {
                    if ep.transfer_type() != nusb::transfer::EndpointType::Bulk {
                        continue;
                    }
                    let addr = ep.address();
                    let is_in = addr & 0x80 != 0;
                    if is_in && ep_in.is_none() {
                        ep_in = Some(addr);
                    } else if !is_in && ep_out.is_none() {
                        ep_out = Some(addr);
                    }
                }
            }
        }
    }
    let ep_in = ep_in.ok_or_else(|| io_other("no bulk IN endpoint on accessory interface"))?;
    let ep_out = ep_out.ok_or_else(|| io_other("no bulk OUT endpoint on accessory interface"))?;
    Ok((interface, ep_in, ep_out))
}

// ===================================================================
// AsyncRead / AsyncWrite over nusb bulk endpoints
// ===================================================================

const BULK_READ_SIZE: usize = 64 * 1024;

/// Tokio AsyncRead wrapper around an nusb bulk-IN endpoint.
struct UsbReader {
    interface: Arc<Interface>,
    ep: u8,
    /// In-flight transfer's future. Output is `Completion<Vec<u8>>`.
    pending: Option<TransferFuture<RequestBuffer>>,
    leftover: Vec<u8>,
    leftover_pos: usize,
}

impl UsbReader {
    fn new(interface: Arc<Interface>, ep: u8) -> Self {
        Self {
            interface,
            ep,
            pending: None,
            leftover: Vec::new(),
            leftover_pos: 0,
        }
    }
}

impl AsyncRead for UsbReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 1. Drain leftover from previous transfer.
        if self.leftover_pos < self.leftover.len() {
            let remaining = &self.leftover[self.leftover_pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.leftover_pos += n;
            return Poll::Ready(Ok(()));
        }

        // 2. Submit a new transfer if none in flight.
        if self.pending.is_none() {
            let req = RequestBuffer::new(BULK_READ_SIZE);
            self.pending = Some(self.interface.bulk_in(self.ep, req));
        }

        // 3. Poll.
        let pending = self.pending.as_mut().unwrap();
        match Pin::new(pending).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Completion { data, status }) => {
                self.pending = None;
                if let Err(e) = status {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)));
                }
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.leftover = data;
                    self.leftover_pos = n;
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// Tokio AsyncWrite wrapper around an nusb bulk-OUT endpoint.
struct UsbWriter {
    interface: Arc<Interface>,
    ep: u8,
    /// In-flight transfer's future. Output is `Completion<ResponseBuffer>`.
    pending: Option<TransferFuture<Vec<u8>>>,
    pending_len: usize,
}

impl UsbWriter {
    fn new(interface: Arc<Interface>, ep: u8) -> Self {
        Self {
            interface,
            ep,
            pending: None,
            pending_len: 0,
        }
    }
}

impl AsyncWrite for UsbWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // 1. If a previous transfer is in flight, drive it to completion
        //    first so we don't stack writes.
        if let Some(pending) = self.pending.as_mut() {
            match Pin::new(pending).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Completion { data: _resp, status }) => {
                    self.pending = None;
                    if let Err(e) = status {
                        return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)));
                    }
                    let n = self.pending_len;
                    self.pending_len = 0;
                    return Poll::Ready(Ok(n));
                }
            }
        }

        // 2. Submit a new transfer.
        let data = buf.to_vec();
        self.pending_len = data.len();
        self.pending = Some(self.interface.bulk_out(self.ep, data));

        // 3. Poll once immediately so we make progress in this call.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(pending) = self.pending.as_mut() {
            match Pin::new(pending).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Completion { data: _resp, status }) => {
                    self.pending = None;
                    self.pending_len = 0;
                    status
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                        .map(|()| ())?;
                    Poll::Ready(Ok(()))
                }
            }
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        // Bulk endpoints don't have a shutdown semantic beyond flushing
        // the last in-flight transfer.
        self.poll_flush(cx)
    }
}

// Silence "unused import" for ResponseBuffer — it appears in
// `Completion<ResponseBuffer>` which is the destructured shape of
// bulk_out's output, but Rust doesn't see the use through the pattern.
#[allow(dead_code)]
fn _unused_response_buffer_marker(_r: ResponseBuffer) {}

// ===================================================================
// Helpers
// ===================================================================

fn io_other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}
