//! Wire-format constants, typed messages, and framing codec shared between PC
//! server and Android client.
//!
//! See `docs/design.md` §7 for the full message catalogue. Byte layouts MUST
//! match `android/app/src/main/java/dev/penflow/Protocol.kt` exactly — the
//! tests in this file only verify the Rust side; cross-version drift is caught
//! by the integration tests in `penflow-server` against a real client.

#![deny(missing_docs)]

use std::io;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// =====================================================================
// Message-id constants (mirrored in android/.../Protocol.kt).
// =====================================================================

/// Server → client: handshake reply with negotiated stream parameters.
pub const MSG_HELLO_PC: u8 = 0x01;
/// Server → client: codec-specific decoder configuration (csd-0).
pub const MSG_VIDEO_CONFIG: u8 = 0x02;
/// Server → client: encoded video frame.
pub const MSG_VIDEO_FRAME: u8 = 0x03;
/// Server → client: brush-stroke prediction hint (reserved, post-v1.0).
pub const MSG_BRUSH_HINT: u8 = 0x04;
/// Server → client: server-side telemetry sample.
pub const MSG_TELEMETRY: u8 = 0x05;
/// Server → client: NTP-style time-sync response.
pub const MSG_TIME_SYNC_RESP: u8 = 0x06;
/// Server → client: clean shutdown notice.
pub const MSG_PC_GOODBYE: u8 = 0x7F;

/// Client → server: handshake announcing device capabilities.
pub const MSG_HELLO_ANDROID: u8 = 0x81;
/// Client → server: pen sample (position, pressure, tilt, buttons).
pub const MSG_PEN_EVENT: u8 = 0x82;
/// Client → server: multi-finger touch snapshot.
pub const MSG_TOUCH_EVENT: u8 = 0x83;
/// Client → server: NTP-style time-sync request.
pub const MSG_TIME_SYNC_REQ: u8 = 0x84;
/// Client → server: ask the encoder to emit an IDR ASAP (decoder-recovery
/// signal — design.md §10.3 codec recovery ladder).
pub const MSG_REQUEST_IDR: u8 = 0x85;
/// Client → server: clean shutdown notice.
pub const MSG_ANDROID_GOODBYE: u8 = 0xFF;

/// Codec identifier: H.264 / AVC.
pub const CODEC_H264: u8 = 1;
/// Codec identifier: H.265 / HEVC.
pub const CODEC_HEVC: u8 = 2;
/// Codec identifier: AV1.
pub const CODEC_AV1: u8 = 3;

/// `VIDEO_FRAME` flag: this packet carries an IDR / key frame.
pub const FRAME_FLAG_KEYFRAME: u8 = 0x01;
/// `VIDEO_FRAME` flag: extended capture/encode timing fields are present.
pub const FRAME_FLAG_EXTENDED: u8 = 0x80;

/// `HELLO_ANDROID.codec_caps` bitmask: client decodes H.264.
pub const CODEC_CAPS_H264: u8 = 1 << 0;
/// `HELLO_ANDROID.codec_caps` bitmask: client decodes HEVC.
pub const CODEC_CAPS_HEVC: u8 = 1 << 1;
/// `HELLO_ANDROID.codec_caps` bitmask: client decodes AV1.
pub const CODEC_CAPS_AV1: u8 = 1 << 2;

/// Largest payload we'll accept on a single framed message. Sized for
/// generous-but-bounded; any realistic VIDEO_FRAME at 50 Mbps × 1 frame is
/// well under 1 MB.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024 * 1024;

// =====================================================================
// Errors.
// =====================================================================

/// Error returned by codec helpers.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Underlying I/O failure (connection closed, network error, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A length-prefix exceeded `MAX_PAYLOAD_LEN`. Indicates either a peer
    /// bug or a desynchronised stream — caller should drop the connection.
    #[error("framed message length {0} exceeds {} bytes", MAX_PAYLOAD_LEN)]
    PayloadTooLarge(u32),

    /// A typed payload was shorter or longer than expected for its message.
    #[error("malformed payload for msg 0x{msg:02x}: {detail}")]
    Malformed {
        /// The message id whose payload could not be decoded.
        msg: u8,
        /// Human-readable description of what went wrong.
        detail: &'static str,
    },
}

// =====================================================================
// Framing codec (matches `Protocol.kt` `sendMsg` / `recvMsg`).
//
// Frame layout: [u8 msg_id][u32 BE length][payload].
// =====================================================================

/// Encode a single framed message into a freshly-allocated `Vec<u8>`.
/// Convenience for "encode then write" callers; for hot paths prefer
/// [`write_frame`] which streams directly into the `AsyncWrite`.
pub fn encode_frame(msg_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(msg_id);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Write one framed message to the given `AsyncWrite`. Does NOT flush —
/// callers wanting immediate delivery should call `.flush().await` themselves
/// (or use `BufWriter` and let it drain on drop).
///
/// **Single allocation, single `write_all`**. The earlier implementation
/// did three separate writes (`write_u8` + `write_u32` + `write_all`),
/// which TCP / ADB-localabstract handled fine via byte streaming, but
/// USB bulk endpoints translated each write into a separate USB
/// transfer — and Android's USB-accessory file descriptor doesn't always
/// aggregate packet boundaries across reads, so the receiving side
/// could misalign on the next length field. Buffering into one Vec
/// produces exactly one `bulk_out` per message and keeps both
/// transports happy.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg_id: u8,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    let bytes = encode_frame(msg_id, payload);
    w.write_all(&bytes).await?;
    Ok(())
}

/// Read one framed message from the given `AsyncRead`. Returns `(msg_id, payload)`.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<(u8, Vec<u8>), ProtocolError> {
    let msg_id = r.read_u8().await?;
    let len = r.read_u32().await?;
    if len as usize > MAX_PAYLOAD_LEN {
        return Err(ProtocolError::PayloadTooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    Ok((msg_id, payload))
}

// =====================================================================
// Typed payloads.
// =====================================================================

/// Server → client handshake payload. Sent right after `HELLO_ANDROID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloPc {
    /// Wire-protocol version. Currently 0.
    pub protocol_version: u8,
    /// Negotiated stream width in pixels.
    pub width: u16,
    /// Negotiated stream height in pixels.
    pub height: u16,
    /// One of `CODEC_H264` / `CODEC_HEVC` / `CODEC_AV1`.
    pub codec: u8,
    /// Bitrate the encoder is configured for.
    pub bitrate_bps: u32,
    /// Frames per second (0–255).
    pub fps: u8,
}

impl HelloPc {
    /// Wire size in bytes.
    pub const SIZE: usize = 1 + 2 + 2 + 1 + 4 + 1; // 11

    /// Encode to a heap-allocated `Vec` matching `Protocol.kt::decodeHelloPc`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SIZE);
        buf.push(self.protocol_version);
        buf.extend_from_slice(&self.width.to_be_bytes());
        buf.extend_from_slice(&self.height.to_be_bytes());
        buf.push(self.codec);
        buf.extend_from_slice(&self.bitrate_bps.to_be_bytes());
        buf.push(self.fps);
        buf
    }

    /// Decode the payload of a `MSG_HELLO_PC` frame.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != Self::SIZE {
            return Err(ProtocolError::Malformed {
                msg: MSG_HELLO_PC,
                detail: "HELLO_PC must be 11 bytes",
            });
        }
        Ok(Self {
            protocol_version: payload[0],
            width: u16::from_be_bytes([payload[1], payload[2]]),
            height: u16::from_be_bytes([payload[3], payload[4]]),
            codec: payload[5],
            bitrate_bps: u32::from_be_bytes([payload[6], payload[7], payload[8], payload[9]]),
            fps: payload[10],
        })
    }
}

/// Client → server handshake payload. Sent right after the socket connects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloAndroid {
    /// Wire-protocol version. Currently 0.
    pub protocol_version: u8,
    /// Display width in physical pixels.
    pub display_width: u16,
    /// Display height in physical pixels.
    pub display_height: u16,
    /// Pen pressure resolution (e.g. 16383 for the MovinkPad).
    pub pen_max_pressure: u16,
    /// Pen tilt minimum in degrees (signed).
    pub pen_tilt_min_deg: i16,
    /// Pen tilt maximum in degrees (signed).
    pub pen_tilt_max_deg: i16,
    /// Number of pen barrel buttons.
    pub pen_buttons_count: u8,
    /// `CODEC_CAPS_*` bitmask.
    pub codec_caps: u8,
}

impl HelloAndroid {
    /// Wire size in bytes.
    pub const SIZE: usize = 1 + 2 + 2 + 2 + 2 + 2 + 1 + 1; // 13

    /// Encode to bytes matching `Protocol.kt::encodeHelloAndroid`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SIZE);
        buf.push(self.protocol_version);
        buf.extend_from_slice(&self.display_width.to_be_bytes());
        buf.extend_from_slice(&self.display_height.to_be_bytes());
        buf.extend_from_slice(&self.pen_max_pressure.to_be_bytes());
        buf.extend_from_slice(&self.pen_tilt_min_deg.to_be_bytes());
        buf.extend_from_slice(&self.pen_tilt_max_deg.to_be_bytes());
        buf.push(self.pen_buttons_count);
        buf.push(self.codec_caps);
        buf
    }

    /// Decode the payload of a `MSG_HELLO_ANDROID` frame.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != Self::SIZE {
            return Err(ProtocolError::Malformed {
                msg: MSG_HELLO_ANDROID,
                detail: "HELLO_ANDROID must be 13 bytes",
            });
        }
        Ok(Self {
            protocol_version: payload[0],
            display_width: u16::from_be_bytes([payload[1], payload[2]]),
            display_height: u16::from_be_bytes([payload[3], payload[4]]),
            pen_max_pressure: u16::from_be_bytes([payload[5], payload[6]]),
            pen_tilt_min_deg: i16::from_be_bytes([payload[7], payload[8]]),
            pen_tilt_max_deg: i16::from_be_bytes([payload[9], payload[10]]),
            pen_buttons_count: payload[11],
            codec_caps: payload[12],
        })
    }
}

/// Encoded video frame header + payload.
///
/// Wire layout: `u64 pts_ns | u8 flags | (if FRAME_FLAG_EXTENDED: u32 capture_us, u32 encode_us) | bytes...`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    /// Server-side capture-instant PTS in nanoseconds.
    pub pts_ns: i64,
    /// `FRAME_FLAG_*` bits.
    pub flags: u8,
    /// Capture stage cost in microseconds (Some iff `FRAME_FLAG_EXTENDED`).
    pub capture_us: Option<u32>,
    /// Encode stage cost in microseconds (Some iff `FRAME_FLAG_EXTENDED`).
    pub encode_us: Option<u32>,
    /// Annex-B coded NAL bytes.
    pub coded: Vec<u8>,
}

impl VideoFrame {
    /// Encode header + bytes into one heap allocation.
    pub fn encode(&self) -> Vec<u8> {
        let extended = self.flags & FRAME_FLAG_EXTENDED != 0;
        let header = 8 + 1 + if extended { 8 } else { 0 };
        let mut buf = Vec::with_capacity(header + self.coded.len());
        buf.extend_from_slice(&self.pts_ns.to_be_bytes());
        buf.push(self.flags);
        if extended {
            buf.extend_from_slice(&self.capture_us.unwrap_or(0).to_be_bytes());
            buf.extend_from_slice(&self.encode_us.unwrap_or(0).to_be_bytes());
        }
        buf.extend_from_slice(&self.coded);
        buf
    }

    /// Decode the payload of a `MSG_VIDEO_FRAME` frame.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() < 9 {
            return Err(ProtocolError::Malformed {
                msg: MSG_VIDEO_FRAME,
                detail: "VIDEO_FRAME header truncated",
            });
        }
        let pts_ns = i64::from_be_bytes(payload[0..8].try_into().unwrap());
        let flags = payload[8];
        if flags & FRAME_FLAG_EXTENDED != 0 {
            if payload.len() < 17 {
                return Err(ProtocolError::Malformed {
                    msg: MSG_VIDEO_FRAME,
                    detail: "VIDEO_FRAME extended header truncated",
                });
            }
            let capture_us = u32::from_be_bytes(payload[9..13].try_into().unwrap());
            let encode_us = u32::from_be_bytes(payload[13..17].try_into().unwrap());
            Ok(Self {
                pts_ns,
                flags,
                capture_us: Some(capture_us),
                encode_us: Some(encode_us),
                coded: payload[17..].to_vec(),
            })
        } else {
            Ok(Self {
                pts_ns,
                flags,
                capture_us: None,
                encode_us: None,
                coded: payload[9..].to_vec(),
            })
        }
    }
}

/// One pen sample from the Android client.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenEvent {
    /// Android `eventTime` in nanoseconds (monotonic-since-boot).
    pub ts_ns: i64,
    /// Phase: 0=hover, 1=down, 2=move, 3=up, 4=leave.
    pub phase: u8,
    /// Normalized X in `[0, 1]` over the Android display.
    pub x_norm: f32,
    /// Normalized Y in `[0, 1]` over the Android display.
    pub y_norm: f32,
    /// `[0, 1]`.
    pub pressure: f32,
    /// Tilt-X in degrees (rough signed range -60..+60 on the MovinkPad).
    pub tilt_x: f32,
    /// Tilt-Y in degrees.
    pub tilt_y: f32,
    /// Bit 0=barrel1, bit 1=barrel2, bit 2=tertiary.
    pub buttons: u8,
    /// 0=tip, 1=eraser end.
    pub tool: u8,
}

impl PenEvent {
    /// Wire size in bytes.
    pub const SIZE: usize = 8 + 1 + 4 + 4 + 4 + 4 + 4 + 1 + 1; // 31

    /// Encode to bytes matching `Protocol.kt::encodePenEvent`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SIZE);
        buf.extend_from_slice(&self.ts_ns.to_be_bytes());
        buf.push(self.phase);
        buf.extend_from_slice(&self.x_norm.to_be_bytes());
        buf.extend_from_slice(&self.y_norm.to_be_bytes());
        buf.extend_from_slice(&self.pressure.to_be_bytes());
        buf.extend_from_slice(&self.tilt_x.to_be_bytes());
        buf.extend_from_slice(&self.tilt_y.to_be_bytes());
        buf.push(self.buttons);
        buf.push(self.tool);
        buf
    }

    /// Decode the payload of a `MSG_PEN_EVENT` frame.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != Self::SIZE {
            return Err(ProtocolError::Malformed {
                msg: MSG_PEN_EVENT,
                detail: "PEN_EVENT must be 31 bytes",
            });
        }
        Ok(Self {
            ts_ns: i64::from_be_bytes(payload[0..8].try_into().unwrap()),
            phase: payload[8],
            x_norm: f32::from_be_bytes(payload[9..13].try_into().unwrap()),
            y_norm: f32::from_be_bytes(payload[13..17].try_into().unwrap()),
            pressure: f32::from_be_bytes(payload[17..21].try_into().unwrap()),
            tilt_x: f32::from_be_bytes(payload[21..25].try_into().unwrap()),
            tilt_y: f32::from_be_bytes(payload[25..29].try_into().unwrap()),
            buttons: payload[29],
            tool: payload[30],
        })
    }
}

/// One contact in a multi-finger touch snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchContact {
    /// Stable contact id (Android `MotionEvent` pointer id, 0..255).
    pub pointer_id: u8,
    /// Normalized X in `[0, 1]`.
    pub x_norm: f32,
    /// Normalized Y in `[0, 1]`.
    pub y_norm: f32,
    /// `[0, 1]`.
    pub pressure: f32,
}

impl TouchContact {
    /// Wire size in bytes.
    pub const SIZE: usize = 1 + 4 + 4 + 4; // 13
}

/// Multi-finger touch snapshot. Wire layout: `u64 ts_ns | u8 count | count × TouchContact`.
#[derive(Clone, Debug, PartialEq)]
pub struct TouchEvent {
    /// Android `eventTime` in nanoseconds.
    pub ts_ns: i64,
    /// Currently-down contacts (full snapshot, not delta).
    pub contacts: Vec<TouchContact>,
}

impl TouchEvent {
    /// Encode to bytes matching `Protocol.kt::encodeTouchEvent`.
    pub fn encode(&self) -> Vec<u8> {
        let n = self.contacts.len().min(255);
        let mut buf = Vec::with_capacity(9 + n * TouchContact::SIZE);
        buf.extend_from_slice(&self.ts_ns.to_be_bytes());
        buf.push(n as u8);
        for c in &self.contacts[..n] {
            buf.push(c.pointer_id);
            buf.extend_from_slice(&c.x_norm.to_be_bytes());
            buf.extend_from_slice(&c.y_norm.to_be_bytes());
            buf.extend_from_slice(&c.pressure.to_be_bytes());
        }
        buf
    }

    /// Decode the payload of a `MSG_TOUCH_EVENT` frame.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() < 9 {
            return Err(ProtocolError::Malformed {
                msg: MSG_TOUCH_EVENT,
                detail: "TOUCH_EVENT header truncated",
            });
        }
        let ts_ns = i64::from_be_bytes(payload[0..8].try_into().unwrap());
        let count = payload[8] as usize;
        let expected = 9 + count * TouchContact::SIZE;
        if payload.len() != expected {
            return Err(ProtocolError::Malformed {
                msg: MSG_TOUCH_EVENT,
                detail: "TOUCH_EVENT contact count mismatch",
            });
        }
        let mut contacts = Vec::with_capacity(count);
        for i in 0..count {
            let off = 9 + i * TouchContact::SIZE;
            contacts.push(TouchContact {
                pointer_id: payload[off],
                x_norm: f32::from_be_bytes(payload[off + 1..off + 5].try_into().unwrap()),
                y_norm: f32::from_be_bytes(payload[off + 5..off + 9].try_into().unwrap()),
                pressure: f32::from_be_bytes(payload[off + 9..off + 13].try_into().unwrap()),
            });
        }
        Ok(Self { ts_ns, contacts })
    }
}

/// Server → client telemetry sample (1 Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Telemetry {
    /// Frames produced since last sample.
    pub frames: u32,
    /// Frames the queue dropped (overflow) since last sample.
    pub dropped: u32,
    /// Average capture stage cost in microseconds.
    pub capture_us_avg: u32,
    /// Average encode stage cost in microseconds.
    pub encode_us_avg: u32,
    /// p99 encode stage cost in microseconds.
    pub encode_us_p99: u32,
    /// Current packet queue depth at the server.
    pub queue_depth: u8,
}

impl Telemetry {
    /// Wire size in bytes.
    pub const SIZE: usize = 4 * 5 + 1; // 21

    /// Encode to bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SIZE);
        buf.extend_from_slice(&self.frames.to_be_bytes());
        buf.extend_from_slice(&self.dropped.to_be_bytes());
        buf.extend_from_slice(&self.capture_us_avg.to_be_bytes());
        buf.extend_from_slice(&self.encode_us_avg.to_be_bytes());
        buf.extend_from_slice(&self.encode_us_p99.to_be_bytes());
        buf.push(self.queue_depth);
        buf
    }
}

/// Client → server NTP-style ping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeSyncReq {
    /// Android `System.nanoTime()` at send.
    pub android_t1_ns: i64,
}

impl TimeSyncReq {
    /// Wire size in bytes.
    pub const SIZE: usize = 8;

    /// Decode.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() != Self::SIZE {
            return Err(ProtocolError::Malformed {
                msg: MSG_TIME_SYNC_REQ,
                detail: "TIME_SYNC_REQ must be 8 bytes",
            });
        }
        Ok(Self {
            android_t1_ns: i64::from_be_bytes(payload[0..8].try_into().unwrap()),
        })
    }
}

/// Server → client NTP-style reply. `pc_t2_ns` is the server's monotonic
/// timestamp at receive; `pc_t3_ns` is the server's monotonic timestamp at
/// send. The client computes `offset = ((t2 - t1) + (t3 - t4)) / 2` per
/// Cristian's algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeSyncResp {
    /// Echoed Android t1 from the request.
    pub android_t1_ns: i64,
    /// PC monotonic-ns at receive.
    pub pc_t2_ns: i64,
    /// PC monotonic-ns at send.
    pub pc_t3_ns: i64,
}

impl TimeSyncResp {
    /// Wire size in bytes.
    pub const SIZE: usize = 24;

    /// Encode.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SIZE);
        buf.extend_from_slice(&self.android_t1_ns.to_be_bytes());
        buf.extend_from_slice(&self.pc_t2_ns.to_be_bytes());
        buf.extend_from_slice(&self.pc_t3_ns.to_be_bytes());
        buf
    }
}

// =====================================================================
// Parameter-set extraction (HEVC + H.264) — used to derive csd-0 from the
// first keyframe packet.
//
// HEVC and H.264 use **different NAL header layouts**:
//   - H.264:  1-byte header. nal_unit_type = byte[0] & 0x1F.
//             Important: 7 = SPS, 8 = PPS, 5 = IDR slice.
//   - HEVC:   2-byte header. nal_unit_type = (byte[0] >> 1) & 0x3F.
//             Important: 32 = VPS, 33 = SPS, 34 = PPS, 19/20/21 = IDR/CRA.
//
// Start codes (`00 00 00 01` or `00 00 01`) are identical in both.
// =====================================================================

/// Walk an HEVC Annex-B byte stream and return the bytes from start codes
/// onwards for any NAL unit whose `nal_unit_type` is in `keep_types`.
///
/// Used by the server to extract VPS+SPS+PPS (types 32, 33, 34) from the
/// first keyframe packet for `MSG_VIDEO_CONFIG` (csd-0).
pub fn extract_hevc_nals(annex_b: &[u8], keep_types: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (start_off, end_off, nal_type) in split_hevc_nals(annex_b) {
        if keep_types.contains(&nal_type) {
            out.extend_from_slice(&annex_b[start_off..end_off]);
        }
    }
    out
}

/// H.264 counterpart to [`extract_hevc_nals`]. Used to extract SPS+PPS
/// (types 7, 8) from the first keyframe for `MSG_VIDEO_CONFIG`.
pub fn extract_h264_nals(annex_b: &[u8], keep_types: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (start_off, end_off, nal_type) in split_h264_nals(annex_b) {
        if keep_types.contains(&nal_type) {
            out.extend_from_slice(&annex_b[start_off..end_off]);
        }
    }
    out
}

/// Walk an HEVC Annex-B byte stream and return `(start_offset_inclusive,
/// end_offset_exclusive, nal_unit_type)` triples — `start_offset` points at
/// the leading start code (`00 00 00 01` or `00 00 01`), `end_offset` is the
/// next start code or `bytes.len()`.
pub fn split_hevc_nals(bytes: &[u8]) -> Vec<(usize, usize, u8)> {
    split_nals(bytes, |b| (b >> 1) & 0x3F)
}

/// H.264 counterpart to [`split_hevc_nals`]. The only difference is the
/// NAL-type extraction (`byte & 0x1F` for H.264 vs `(byte >> 1) & 0x3F`
/// for HEVC), since H.264 uses a 1-byte NAL header.
pub fn split_h264_nals(bytes: &[u8]) -> Vec<(usize, usize, u8)> {
    split_nals(bytes, |b| b & 0x1F)
}

/// Shared start-code walker. The `nal_type_from_header_byte` closure pulls
/// the codec-specific NAL type bits out of the first header byte.
fn split_nals<F: Fn(u8) -> u8>(bytes: &[u8], nal_type_from_header_byte: F) -> Vec<(usize, usize, u8)> {
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (start_off, payload_off)
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            starts.push((i, i + 4));
            i += 4;
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for k in 0..starts.len() {
        let (start_off, payload_off) = starts[k];
        let end_off = if k + 1 < starts.len() {
            starts[k + 1].0
        } else {
            bytes.len()
        };
        if payload_off >= bytes.len() {
            continue;
        }
        out.push((start_off, end_off, nal_type_from_header_byte(bytes[payload_off])));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_to_client_message_ids_are_stable() {
        assert_eq!(MSG_HELLO_PC, 0x01);
        assert_eq!(MSG_VIDEO_FRAME, 0x03);
        assert_eq!(MSG_PC_GOODBYE, 0x7F);
    }

    #[test]
    fn client_to_server_message_ids_have_high_bit_set() {
        for id in [
            MSG_HELLO_ANDROID,
            MSG_PEN_EVENT,
            MSG_TOUCH_EVENT,
            MSG_TIME_SYNC_REQ,
            MSG_REQUEST_IDR,
            MSG_ANDROID_GOODBYE,
        ] {
            assert!(id & 0x80 != 0, "client→server id 0x{id:02x} missing high bit");
        }
    }

    #[test]
    fn codec_ids_are_stable() {
        assert_eq!(CODEC_H264, 1);
        assert_eq!(CODEC_HEVC, 2);
        assert_eq!(CODEC_AV1, 3);
    }

    #[test]
    fn frame_round_trip() {
        let raw = encode_frame(MSG_VIDEO_FRAME, b"hello");
        assert_eq!(raw[0], MSG_VIDEO_FRAME);
        assert_eq!(raw[1..5], 5u32.to_be_bytes());
        assert_eq!(&raw[5..], b"hello");
    }

    #[tokio::test]
    async fn write_then_read_frame_round_trip() {
        use tokio::io::duplex;
        let (mut a, mut b) = duplex(64 * 1024);
        write_frame(&mut a, MSG_PEN_EVENT, &vec![7u8; 31]).await.unwrap();
        let (id, payload) = read_frame(&mut b).await.unwrap();
        assert_eq!(id, MSG_PEN_EVENT);
        assert_eq!(payload.len(), 31);
        assert!(payload.iter().all(|&b| b == 7));
    }

    #[test]
    fn hello_pc_round_trip() {
        let h = HelloPc {
            protocol_version: 0,
            width: 2880,
            height: 1800,
            codec: CODEC_HEVC,
            bitrate_bps: 50_000_000,
            fps: 60,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), HelloPc::SIZE);
        assert_eq!(HelloPc::decode(&bytes).unwrap(), h);
    }

    #[test]
    fn hello_android_round_trip() {
        let h = HelloAndroid {
            protocol_version: 0,
            display_width: 2880,
            display_height: 1800,
            pen_max_pressure: 16383,
            pen_tilt_min_deg: -60,
            pen_tilt_max_deg: 60,
            pen_buttons_count: 3,
            codec_caps: CODEC_CAPS_HEVC,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), HelloAndroid::SIZE);
        assert_eq!(HelloAndroid::decode(&bytes).unwrap(), h);
    }

    #[test]
    fn pen_event_round_trip() {
        let p = PenEvent {
            ts_ns: 1_234_567_890,
            phase: 2,
            x_norm: 0.42,
            y_norm: 0.88,
            pressure: 0.7,
            tilt_x: 12.5,
            tilt_y: -7.0,
            buttons: 0b101,
            tool: 0,
        };
        let bytes = p.encode();
        assert_eq!(bytes.len(), PenEvent::SIZE);
        let decoded = PenEvent::decode(&bytes).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn touch_event_round_trip() {
        let t = TouchEvent {
            ts_ns: 555,
            contacts: vec![
                TouchContact { pointer_id: 0, x_norm: 0.1, y_norm: 0.2, pressure: 0.5 },
                TouchContact { pointer_id: 1, x_norm: 0.9, y_norm: 0.7, pressure: 0.3 },
            ],
        };
        let bytes = t.encode();
        let decoded = TouchEvent::decode(&bytes).unwrap();
        assert_eq!(decoded, t);
    }

    #[test]
    fn video_frame_extended_round_trip() {
        let vf = VideoFrame {
            pts_ns: 1_000_000_000,
            flags: FRAME_FLAG_KEYFRAME | FRAME_FLAG_EXTENDED,
            capture_us: Some(900),
            encode_us: Some(2800),
            coded: vec![0, 0, 0, 1, 0x46, 0x01, 0x10],
        };
        let bytes = vf.encode();
        let decoded = VideoFrame::decode(&bytes).unwrap();
        assert_eq!(decoded, vf);
    }

    #[test]
    fn split_hevc_nals_separates_units() {
        // Realistic shape: AUD, VPS, SPS, PPS, IDR (the predecessor's
        // gate-1 probe captured exactly this sequence on NVIDIA).
        let mut bytes = Vec::new();
        for (sc4, header_byte) in [
            (true, 0x46u8),  // AUD (35)
            (true, 0x40),    // VPS (32)
            (true, 0x42),    // SPS (33)
            (true, 0x44),    // PPS (34)
            (true, 0x26),    // IDR_W_RADL (19)
        ] {
            if sc4 {
                bytes.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                bytes.extend_from_slice(&[0, 0, 1]);
            }
            bytes.push(header_byte);
            bytes.push(0x01); // 2-byte NAL header
            bytes.push(0xff); // dummy payload
        }
        let nals = split_hevc_nals(&bytes);
        let types: Vec<u8> = nals.iter().map(|(_, _, t)| *t).collect();
        assert_eq!(types, vec![35, 32, 33, 34, 19]);

        // extract_hevc_nals should give us VPS+SPS+PPS only, byte-for-byte.
        let csd0 = extract_hevc_nals(&bytes, &[32, 33, 34]);
        // Three NAL units of 4 (start code) + 3 (header + payload byte) = 21 bytes.
        assert_eq!(csd0.len(), 3 * 7);
        // First byte of each retained NAL is a start code.
        assert_eq!(&csd0[0..4], &[0, 0, 0, 1]);
    }

    #[test]
    fn split_h264_nals_separates_units() {
        // Synthesize an Annex-B stream with the NAL types we care about for
        // H.264 csd-0 + IDR detection. H.264's 1-byte NAL header packs the
        // type into the low 5 bits.
        //   type 7 (SPS) → header byte 0x67 (forbidden=0, ref=3, type=7)
        //   type 8 (PPS) → header byte 0x68
        //   type 5 (IDR slice) → header byte 0x65
        //   type 1 (non-IDR slice) → header byte 0x41
        let mut bytes: Vec<u8> = Vec::new();
        for (sc4, hdr) in [
            (true, 0x67u8),  // SPS
            (false, 0x68),   // PPS
            (true, 0x65),    // IDR
            (false, 0x41),   // non-IDR slice
        ] {
            if sc4 {
                bytes.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                bytes.extend_from_slice(&[0, 0, 1]);
            }
            bytes.push(hdr);
            bytes.push(0xff); // dummy payload
        }
        let nals = split_h264_nals(&bytes);
        let types: Vec<u8> = nals.iter().map(|(_, _, t)| *t).collect();
        assert_eq!(types, vec![7, 8, 5, 1]);

        let csd0 = extract_h264_nals(&bytes, &[7, 8]);
        // SPS: 4 (start code) + 1 (header) + 1 (payload) = 6
        // PPS: 3 (start code) + 1 + 1 = 5
        // Total 11 bytes.
        assert_eq!(csd0.len(), 6 + 5);
        assert_eq!(&csd0[0..4], &[0, 0, 0, 1]);
    }
}
