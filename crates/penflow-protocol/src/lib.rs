//! Wire-format constants and types shared between PC server and Android client.
//!
//! See `docs/design.md` §7 for the full message catalogue and framing format.

#![deny(missing_docs)]

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

/// Connection-readiness probe byte (scrcpy-inspired) — both sides exchange
/// this single byte before any framed message, to distinguish a real
/// connection from an ADB tunnel that accepted the TCP handshake while the
/// peer is still initialising.
pub const READY_BYTE: u8 = 0xA5;

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
            MSG_ANDROID_GOODBYE,
        ] {
            assert!(
                id & 0x80 != 0,
                "client→server id 0x{id:02x} missing high bit"
            );
        }
    }

    #[test]
    fn codec_ids_are_stable() {
        assert_eq!(CODEC_H264, 1);
        assert_eq!(CODEC_HEVC, 2);
        assert_eq!(CODEC_AV1, 3);
    }
}
