//! Rewrite the DPB-shaping fields of an H.264 / HEVC SPS NAL so the decoder
//! sees a 1-deep DPB and 0 reorder frames, no matter what the encoder
//! actually produced.
//!
//! ## Why this exists
//!
//! Adreno's `c2.qti.{avc,hevc}.decoder[.low_latency]` paces its output
//! against the SPS-declared DPB depth — not against what the encoder
//! actually USES. NVIDIA's NVENC MFT writes `max_num_ref_frames = 4`
//! (H.264) / `sps_max_dec_pic_buffering_minus1 = 3` (HEVC) into the SPS
//! by default, so Adreno bumps its `output_delay` to 24 frames after
//! parsing the first SPS, which `CCodecBufferChannel` translates into
//! a 28-deep BufferQueue and ~6 ms of additional per-frame latency
//! (measured: dec_us 9 ms → 15 ms steady, with periodic spikes to
//! 100 ms+ under content change).
//!
//! `CODECAPI_AVEncVideoMaxNumRefFrame = 1` on NVIDIA's MFT is silently
//! ignored, and the `MF_LOW_LATENCY = 1` knob doesn't touch the
//! bitstream. The well-trodden workaround for the same problem on
//! ExoPlayer / Snapdragon (issue
//! [#8514](https://github.com/google/ExoPlayer/issues/8514)) is to
//! rewrite the SPS in place before feeding the decoder. Since the
//! H.264 / HEVC bitstream formats are ITU-T standards, the fix works
//! against any compliant encoder (NVIDIA / AMD / Intel / software) and
//! is harmless against decoders that don't care about DPB sizing.
//!
//! ## What it changes
//!
//! H.264 SPS:
//!   - `max_num_ref_frames` → 1
//!   - VUI `bitstream_restriction.max_num_reorder_frames` → 0
//!   - VUI `bitstream_restriction.max_dec_frame_buffering` → 1
//!   - if VUI was absent, a minimal one is synthesised so the
//!     `bitstream_restriction` flag can be carried.
//!
//! HEVC SPS, for every sub-layer reported in
//! `sps_sub_layer_ordering_info_present_flag`:
//!   - `sps_max_dec_pic_buffering_minus1[i]` → 0  (DPB depth = 1)
//!   - `sps_max_num_reorder_pics[i]` → 0
//!   - `sps_max_latency_increase_plus1[i]` → 0  (= "no additional latency")
//!
//! ## What it does NOT touch
//!
//! profile/level fields, conformance window, VUI timing/colour info,
//! HEVC's profile_tier_level: parsed and copied byte-for-byte (well,
//! bit-for-bit) without modification. We re-emit the entire SPS
//! because Exp-Golomb coding means even modifying one field shifts
//! every subsequent bit; the rewriter walks the whole structure
//! whether or not we change a particular field.

use crate::encoder::Codec;
use crate::error::{EngineError, EngineResult};

/// Scan an Annex-B coded packet for SPS NAL(s) and patch them in place.
/// Non-SPS NALs (VPS / PPS / VCL slices / SEI / AUD / etc.) are copied
/// through unchanged.
pub fn patch_packet_for_low_latency_dpb(codec: Codec, bytes: &[u8]) -> EngineResult<Vec<u8>> {
    // Walk the start codes; for each NAL, decide whether it's an SPS
    // and if so, replace its body with a patched copy. Non-SPS NALs
    // are copied byte-for-byte (start code included).
    let mut out = Vec::with_capacity(bytes.len() + 16);
    let mut i = 0;
    while i < bytes.len() {
        let Some((sc_len, payload_off)) = scan_start_code(bytes, i) else {
            // No more start codes — copy the trailer (rare; usually the
            // bitstream is start-code-prefixed everywhere).
            out.extend_from_slice(&bytes[i..]);
            break;
        };
        // Find the end of this NAL = the next start code or end-of-buffer.
        let mut j = payload_off;
        let nal_end = loop {
            if j + 3 > bytes.len() {
                break bytes.len();
            }
            if bytes[j..].starts_with(&[0, 0, 0, 1]) || bytes[j..].starts_with(&[0, 0, 1]) {
                break j;
            }
            j += 1;
        };

        let header_len = match codec {
            Codec::H264 => 1,
            Codec::Hevc => 2,
        };
        if payload_off + header_len > nal_end {
            // Truncated NAL — copy as-is and stop.
            out.extend_from_slice(&bytes[i..]);
            break;
        }
        let nal_type = match codec {
            Codec::H264 => bytes[payload_off] & 0x1F,
            Codec::Hevc => (bytes[payload_off] >> 1) & 0x3F,
        };
        let is_sps = match codec {
            Codec::H264 => nal_type == 7,
            Codec::Hevc => nal_type == 33,
        };

        if !is_sps {
            // Pass through unchanged.
            out.extend_from_slice(&bytes[i..nal_end]);
            i = nal_end;
            continue;
        }

        // Patch this SPS: keep start code + NAL header, rewrite RBSP.
        let header_end = payload_off + header_len;
        let escaped = &bytes[header_end..nal_end];
        let rbsp = rbsp_unescape(escaped);
        let patched_rbsp = match codec {
            Codec::H264 => patch_h264_sps_rbsp(&rbsp)?,
            Codec::Hevc => patch_hevc_sps_rbsp(&rbsp)?,
        };
        let patched_escaped = rbsp_escape(&patched_rbsp);
        out.extend_from_slice(&bytes[i..header_end]);
        out.extend_from_slice(&patched_escaped);

        i = nal_end;
        let _ = sc_len; // start-code length not used outside this block
    }
    Ok(out)
}

/// Locate a start code (`00 00 00 01` or `00 00 01`) at or after `from`.
/// Returns `(start_code_len, first_payload_offset)`. None if none found.
fn scan_start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= bytes.len() {
        if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            return Some((4, i + 4));
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            return Some((3, i + 3));
        }
        i += 1;
    }
    None
}

// ===================================================================
// RBSP <-> EBSP (emulation-prevention bytes)
// ===================================================================

/// Remove emulation-prevention bytes from an EBSP byte stream:
/// every `00 00 03` sequence becomes `00 00`. The `03` is inserted by
/// the encoder to prevent the bit pattern of a start code appearing in
/// payload data.
fn rbsp_unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut i = 0;
    while i < ebsp.len() {
        if i + 2 < ebsp.len() && ebsp[i] == 0x00 && ebsp[i + 1] == 0x00 && ebsp[i + 2] == 0x03 {
            out.push(0x00);
            out.push(0x00);
            i += 3;
        } else {
            out.push(ebsp[i]);
            i += 1;
        }
    }
    out
}

/// Re-insert emulation-prevention bytes into an RBSP byte stream so the
/// resulting EBSP is safe to embed inside an Annex-B framed bitstream.
/// Inverse of [`rbsp_unescape`].
fn rbsp_escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 32);
    let mut i = 0;
    while i < rbsp.len() {
        // If we're about to write `00 00` followed by 00/01/02/03, we
        // must insert an emulation-prevention `03` between them.
        if i + 2 < rbsp.len() && rbsp[i] == 0x00 && rbsp[i + 1] == 0x00 && rbsp[i + 2] <= 0x03 {
            out.push(0x00);
            out.push(0x00);
            out.push(0x03);
            i += 2;
        } else {
            out.push(rbsp[i]);
            i += 1;
        }
    }
    out
}

// ===================================================================
// Bit reader / writer + Exp-Golomb codecs
// ===================================================================

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize, // 0 = MSB of byte 0
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn read_bits(&mut self, n: u32) -> EngineResult<u32> {
        debug_assert!(n <= 32);
        let mut v: u32 = 0;
        for _ in 0..n {
            let byte_idx = self.bit_pos >> 3;
            if byte_idx >= self.bytes.len() {
                return Err(EngineError::NotInitialized);
            }
            let bit_idx = 7 - (self.bit_pos & 7);
            let bit = (self.bytes[byte_idx] >> bit_idx) & 1;
            v = (v << 1) | bit as u32;
            self.bit_pos += 1;
        }
        Ok(v)
    }

    /// Unsigned Exp-Golomb (ITU-T H.264 §9.1 / H.265 §9.2).
    /// Read a run of leading zeros, count them; then read 1 + that many
    /// bits as an unsigned integer; the value is `(2^k + bits) - 1`.
    fn read_ue(&mut self) -> EngineResult<u32> {
        let mut leading_zeros = 0u32;
        loop {
            let b = self.read_bits(1)?;
            if b == 1 {
                break;
            }
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(EngineError::NotInitialized);
            }
        }
        let suffix = if leading_zeros == 0 {
            0
        } else {
            self.read_bits(leading_zeros)?
        };
        Ok((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb. Codes `0, 1, -1, 2, -2, …` as ue(v) `0, 1, 2, 3, 4, …`.
    fn read_se(&mut self) -> EngineResult<i32> {
        let code = self.read_ue()?;
        if code & 1 != 0 {
            Ok(((code + 1) >> 1) as i32)
        } else {
            Ok(-((code >> 1) as i32))
        }
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    bit_pos: usize, // total bits written
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit_pos: 0,
        }
    }

    fn write_bits(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            let byte_idx = self.bit_pos >> 3;
            let bit_idx = 7 - (self.bit_pos & 7);
            if byte_idx >= self.bytes.len() {
                self.bytes.push(0);
            }
            self.bytes[byte_idx] |= bit << bit_idx;
            self.bit_pos += 1;
        }
    }

    fn write_ue(&mut self, value: u32) {
        // Number of leading zeros = floor(log2(value + 1)).
        let v_plus_1 = value as u64 + 1;
        let leading_zeros = 63 - v_plus_1.leading_zeros();
        // Write `leading_zeros` zero bits then a 1 bit then `leading_zeros`
        // bits of `(value + 1) - 2^leading_zeros`.
        for _ in 0..leading_zeros {
            self.write_bits(0, 1);
        }
        self.write_bits(1, 1);
        if leading_zeros > 0 {
            let suffix = (value + 1) - (1u32 << leading_zeros);
            self.write_bits(suffix, leading_zeros);
        }
    }

    fn write_se(&mut self, value: i32) {
        let code = if value > 0 {
            (value as u32 * 2) - 1
        } else {
            (-value) as u32 * 2
        };
        self.write_ue(code);
    }

    fn write_rbsp_trailing_bits(&mut self) {
        // Bit 1 then zero-pad to next byte boundary.
        self.write_bits(1, 1);
        while self.bit_pos & 7 != 0 {
            self.write_bits(0, 1);
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

// ===================================================================
// H.264 SPS rewriter (ITU-T H.264 §7.3.2.1.1)
// ===================================================================

/// Reads an H.264 SPS RBSP, copies every field bit-for-bit through the
/// writer EXCEPT `max_num_ref_frames` (forced to 1) and the VUI's
/// `bitstream_restriction` block (forced to declare 0 reorder + 1-deep
/// DPB). If the input has no VUI, a minimal one is synthesised that
/// only carries `bitstream_restriction`.
fn patch_h264_sps_rbsp(rbsp: &[u8]) -> EngineResult<Vec<u8>> {
    let mut r = BitReader::new(rbsp);
    let mut w = BitWriter::new();

    // profile_idc(8) + constraint_set_flags & reserved_zero_2bits(8) + level_idc(8)
    let profile_idc = r.read_bits(8)?;
    w.write_bits(profile_idc, 8);
    let constraint_flags = r.read_bits(8)?;
    w.write_bits(constraint_flags, 8);
    let level_idc = r.read_bits(8)?;
    w.write_bits(level_idc, 8);

    // seq_parameter_set_id ue(v)
    let sps_id = r.read_ue()?;
    w.write_ue(sps_id);

    // chroma stuff if profile_idc indicates High / High 10 / High 4:2:2 / High 4:4:4 / etc.
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.read_ue()?;
        w.write_ue(chroma_format_idc);
        if chroma_format_idc == 3 {
            let v = r.read_bits(1)?;
            w.write_bits(v, 1); // separate_colour_plane_flag
        }
        let v = r.read_ue()?;
        w.write_ue(v); // bit_depth_luma_minus8
        let v = r.read_ue()?;
        w.write_ue(v); // bit_depth_chroma_minus8
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // qpprime_y_zero_transform_bypass_flag
        let scaling_present = r.read_bits(1)?;
        w.write_bits(scaling_present, 1);
        if scaling_present == 1 {
            // Scaling matrices are rare in low-latency streams (NVENC
            // ULL doesn't emit them). If we ever see one we'd need to
            // walk the scaling lists; for now refuse to rewrite rather
            // than silently corrupt the bitstream.
            return Err(EngineError::NotInitialized);
        }
    }

    // log2_max_frame_num_minus4 ue(v)
    let v = r.read_ue()?;
    w.write_ue(v);

    // pic_order_cnt_type ue(v)
    let pic_order_cnt_type = r.read_ue()?;
    w.write_ue(pic_order_cnt_type);
    match pic_order_cnt_type {
        0 => {
            let v = r.read_ue()?;
            w.write_ue(v); // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            let v = r.read_bits(1)?;
            w.write_bits(v, 1); // delta_pic_order_always_zero_flag
            let v = r.read_se()?;
            w.write_se(v); // offset_for_non_ref_pic
            let v = r.read_se()?;
            w.write_se(v); // offset_for_top_to_bottom_field
            let n = r.read_ue()?;
            w.write_ue(n); // num_ref_frames_in_pic_order_cnt_cycle
            for _ in 0..n {
                let v = r.read_se()?;
                w.write_se(v);
            }
        }
        _ => {} // type 2 has no extra fields
    }

    // **max_num_ref_frames** — patch.
    let _ = r.read_ue()?;
    w.write_ue(1);

    // gaps_in_frame_num_value_allowed_flag (1)
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);
    // pic_width_in_mbs_minus1 ue(v)
    let v = r.read_ue()?;
    w.write_ue(v);
    // pic_height_in_map_units_minus1 ue(v)
    let v = r.read_ue()?;
    w.write_ue(v);
    // frame_mbs_only_flag (1)
    let frame_mbs_only = r.read_bits(1)?;
    w.write_bits(frame_mbs_only, 1);
    if frame_mbs_only == 0 {
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // mb_adaptive_frame_field_flag
    }
    // direct_8x8_inference_flag (1)
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);
    // frame_cropping
    let crop = r.read_bits(1)?;
    w.write_bits(crop, 1);
    if crop == 1 {
        for _ in 0..4 {
            let v = r.read_ue()?;
            w.write_ue(v);
        }
    }

    // VUI: present? We always emit a VUI in the output, populated with
    // either the input's data + our patched bitstream_restriction, or a
    // minimal one if the input didn't have any.
    let vui_present = r.read_bits(1)?;
    w.write_bits(1, 1);
    if vui_present == 1 {
        copy_h264_vui_with_patched_bitstream_restriction(&mut r, &mut w)?;
    } else {
        emit_minimal_h264_vui(&mut w);
    }

    w.write_rbsp_trailing_bits();
    Ok(w.into_bytes())
}

/// Copy an H.264 VUI byte-for-bit, replacing whatever
/// `bitstream_restriction` block it contained with our 1-deep DPB +
/// 0-reorder values. Per ITU-T H.264 §E.1.1.
fn copy_h264_vui_with_patched_bitstream_restriction(
    r: &mut BitReader,
    w: &mut BitWriter,
) -> EngineResult<()> {
    let aspect_ratio_info = r.read_bits(1)?;
    w.write_bits(aspect_ratio_info, 1);
    if aspect_ratio_info == 1 {
        let aspect_ratio_idc = r.read_bits(8)?;
        w.write_bits(aspect_ratio_idc, 8);
        if aspect_ratio_idc == 255 {
            // Extended SAR — sar_width(16), sar_height(16).
            let v = r.read_bits(16)?;
            w.write_bits(v, 16);
            let v = r.read_bits(16)?;
            w.write_bits(v, 16);
        }
    }
    let overscan_info_present = r.read_bits(1)?;
    w.write_bits(overscan_info_present, 1);
    if overscan_info_present == 1 {
        let v = r.read_bits(1)?;
        w.write_bits(v, 1);
    }
    // video_signal_type — we DROP whatever the input had and emit our own
    // (full-range BT.709) so the decoder doesn't apply a 16-235 → 0-255
    // expansion on samples that are already 0-255. Issue #1: NVIDIA's
    // HEVC/H.264 MFTs are reported to silently ignore
    // `MF_MT_VIDEO_NOMINAL_RANGE = MFNominalRange_0_255` on the input
    // type, leaving `video_full_range_flag = 0` in the bitstream while
    // the actual NV12 bytes are full-range — visible as blown-out
    // highlights on greyscale ramps (Y >= 235 clip to 255 after the
    // decoder's range-expansion).
    let video_signal_type_present = r.read_bits(1)?;
    if video_signal_type_present == 1 {
        let _ = r.read_bits(3)?; // video_format — discard
        let _ = r.read_bits(1)?; // video_full_range_flag — discard
        let colour_desc = r.read_bits(1)?;
        if colour_desc == 1 {
            let _ = r.read_bits(8)?; // colour_primaries — discard
            let _ = r.read_bits(8)?; // transfer_characteristics — discard
            let _ = r.read_bits(8)?; // matrix_coefficients — discard
        }
    }
    write_full_range_bt709_video_signal_type(w);
    let chroma_loc_info_present = r.read_bits(1)?;
    w.write_bits(chroma_loc_info_present, 1);
    if chroma_loc_info_present == 1 {
        let v = r.read_ue()?;
        w.write_ue(v);
        let v = r.read_ue()?;
        w.write_ue(v);
    }
    let timing_info_present = r.read_bits(1)?;
    w.write_bits(timing_info_present, 1);
    if timing_info_present == 1 {
        let v = r.read_bits(32)?;
        w.write_bits(v, 32); // num_units_in_tick
        let v = r.read_bits(32)?;
        w.write_bits(v, 32); // time_scale
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // fixed_frame_rate_flag
    }
    let nal_hrd = r.read_bits(1)?;
    w.write_bits(nal_hrd, 1);
    if nal_hrd == 1 {
        copy_h264_hrd_parameters(r, w)?;
    }
    let vcl_hrd = r.read_bits(1)?;
    w.write_bits(vcl_hrd, 1);
    if vcl_hrd == 1 {
        copy_h264_hrd_parameters(r, w)?;
    }
    if nal_hrd == 1 || vcl_hrd == 1 {
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // low_delay_hrd_flag
    }
    let pic_struct_present = r.read_bits(1)?;
    w.write_bits(pic_struct_present, 1);

    // bitstream_restriction — DROP whatever was here and emit our patched
    // version. We don't even read the input's, just skip past it. But
    // since we have to consume the same number of bits to keep parsing
    // any trailing data consistent, we read first then overwrite with
    // our values.
    let bitstream_restriction = r.read_bits(1)?;
    if bitstream_restriction == 1 {
        // Read & discard.
        let _ = r.read_bits(1)?; // motion_vectors_over_pic_boundaries_flag
        let _ = r.read_ue()?; // max_bytes_per_pic_denom
        let _ = r.read_ue()?; // max_bits_per_mb_denom
        let _ = r.read_ue()?; // log2_max_mv_length_horizontal
        let _ = r.read_ue()?; // log2_max_mv_length_vertical
        let _ = r.read_ue()?; // max_num_reorder_frames
        let _ = r.read_ue()?; // max_dec_frame_buffering
    }
    // Always emit our patched bitstream_restriction.
    w.write_bits(1, 1); // bitstream_restriction_flag
    w.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag = 1 (default)
    w.write_ue(0); // max_bytes_per_pic_denom = 0 (no constraint)
    w.write_ue(0); // max_bits_per_mb_denom = 0
    w.write_ue(16); // log2_max_mv_length_horizontal = 16 (default)
    w.write_ue(16); // log2_max_mv_length_vertical = 16
    w.write_ue(0); // max_num_reorder_frames = 0
    w.write_ue(1); // max_dec_frame_buffering = 1

    Ok(())
}

fn copy_h264_hrd_parameters(r: &mut BitReader, w: &mut BitWriter) -> EngineResult<()> {
    let cpb_cnt_minus1 = r.read_ue()?;
    w.write_ue(cpb_cnt_minus1);
    let v = r.read_bits(4)?;
    w.write_bits(v, 4); // bit_rate_scale
    let v = r.read_bits(4)?;
    w.write_bits(v, 4); // cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        let v = r.read_ue()?;
        w.write_ue(v); // bit_rate_value_minus1
        let v = r.read_ue()?;
        w.write_ue(v); // cpb_size_value_minus1
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // cbr_flag
    }
    let v = r.read_bits(5)?;
    w.write_bits(v, 5); // initial_cpb_removal_delay_length_minus1
    let v = r.read_bits(5)?;
    w.write_bits(v, 5); // cpb_removal_delay_length_minus1
    let v = r.read_bits(5)?;
    w.write_bits(v, 5); // dpb_output_delay_length_minus1
    let v = r.read_bits(5)?;
    w.write_bits(v, 5); // time_offset_length
    Ok(())
}

/// Synthesise a VUI that carries only `bitstream_restriction`. Used
/// when the encoder didn't emit a VUI at all (rare for NVENC, but
/// possible for software encoders).
fn emit_minimal_h264_vui(w: &mut BitWriter) {
    w.write_bits(0, 1); // aspect_ratio_info_present_flag
    w.write_bits(0, 1); // overscan_info_present_flag
    write_full_range_bt709_video_signal_type(w);
    w.write_bits(0, 1); // chroma_loc_info_present_flag
    w.write_bits(0, 1); // timing_info_present_flag
    w.write_bits(0, 1); // nal_hrd_parameters_present_flag
    w.write_bits(0, 1); // vcl_hrd_parameters_present_flag
    w.write_bits(0, 1); // pic_struct_present_flag
    w.write_bits(1, 1); // bitstream_restriction_flag
    w.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag
    w.write_ue(0); // max_bytes_per_pic_denom
    w.write_ue(0); // max_bits_per_mb_denom
    w.write_ue(16); // log2_max_mv_length_horizontal
    w.write_ue(16); // log2_max_mv_length_vertical
    w.write_ue(0); // max_num_reorder_frames
    w.write_ue(1); // max_dec_frame_buffering
}

/// Emit a `video_signal_type` block declaring full-range BT.709 colour.
/// Used by both the H.264 and HEVC patchers so a single source of truth
/// owns the override values. The bit layout is identical between the
/// two specs (H.264 §E.1.1 / H.265 §E.2.1).
fn write_full_range_bt709_video_signal_type(w: &mut BitWriter) {
    w.write_bits(1, 1); // video_signal_type_present_flag = 1
    w.write_bits(5, 3); // video_format = 5 (Unspecified)
    w.write_bits(1, 1); // video_full_range_flag = 1
    w.write_bits(1, 1); // colour_description_present_flag = 1
    w.write_bits(1, 8); // colour_primaries = 1 (BT.709)
    w.write_bits(1, 8); // transfer_characteristics = 1 (BT.709)
    w.write_bits(1, 8); // matrix_coefficients = 1 (BT.709)
}

// ===================================================================
// HEVC SPS rewriter (ITU-T H.265 §7.3.2.2.1)
// ===================================================================

/// Top-level HEVC SPS patcher. Tries the comprehensive walk
/// (`patch_hevc_sps_full`) which patches DPB depth AND overrides VUI
/// colour info to full-range BT.709. If the SPS uses HEVC features
/// the parser doesn't implement (e.g. an in-band scaling list, or a
/// `st_ref_pic_set` predictive form we haven't covered), falls back
/// to the original DPB-only patch. The legacy fallback always
/// succeeds on any SPS our DPB patcher used to handle, so the engine
/// never regresses on encoders / driver versions we don't yet cover.
fn patch_hevc_sps_rbsp(rbsp: &[u8]) -> EngineResult<Vec<u8>> {
    if let Ok(v) = patch_hevc_sps_full(rbsp) {
        return Ok(v);
    }
    patch_hevc_sps_dpb_only(rbsp)
}

/// Original behaviour: walk to `sps_max_latency_increase_plus1`,
/// patch the DPB triplet, copy the rest of the SPS verbatim. Used as
/// the fallback when `patch_hevc_sps_full` doesn't recognise some
/// downstream SPS feature.
fn patch_hevc_sps_dpb_only(rbsp: &[u8]) -> EngineResult<Vec<u8>> {
    let mut r = BitReader::new(rbsp);
    let mut w = BitWriter::new();
    walk_hevc_sps_through_dpb(&mut r, &mut w)?;

    // Everything from here to the end of the SPS is COPIED bit-for-bit.
    // The remainder includes log2_min_luma_coding_block_size_minus3 and
    // a dozen more fields, ending at the SPS extensions + RBSP trailing
    // bits. Rather than re-implement the entire parser, copy the
    // unread tail of `rbsp` directly into the writer at the current
    // bit offset.
    copy_remaining_bits(&mut r, &mut w);

    // The trailing `1` + zero-pad is already inside the copied tail
    // (the original SPS ended with rbsp_trailing_bits()), so we don't
    // emit our own.
    Ok(w.into_bytes())
}

/// Full HEVC SPS walk. Patches the DPB triplet AND continues all the
/// way to the VUI to override `video_signal_type_present_flag`,
/// `video_full_range_flag`, and the colour description (issue #1 —
/// blown-out highlights when the encoder writes the bytes in 0-255
/// but the bitstream's VUI declares 16-235). On failure (HEVC features
/// the parser doesn't implement) returns Err and the caller falls
/// back to the DPB-only patch.
fn patch_hevc_sps_full(rbsp: &[u8]) -> EngineResult<Vec<u8>> {
    let mut r = BitReader::new(rbsp);
    let mut w = BitWriter::new();
    walk_hevc_sps_through_dpb(&mut r, &mut w)?;
    let log2_max_poc = walk_hevc_sps_dpb_log2_max_poc(rbsp)?;

    // log2_min_luma_coding_block_size_minus3 ue
    let v = r.read_ue()?;
    w.write_ue(v);
    // log2_diff_max_min_luma_coding_block_size ue
    let v = r.read_ue()?;
    w.write_ue(v);
    // log2_min_luma_transform_block_size_minus2 ue
    let v = r.read_ue()?;
    w.write_ue(v);
    // log2_diff_max_min_luma_transform_block_size ue
    let v = r.read_ue()?;
    w.write_ue(v);
    // max_transform_hierarchy_depth_inter ue
    let v = r.read_ue()?;
    w.write_ue(v);
    // max_transform_hierarchy_depth_intra ue
    let v = r.read_ue()?;
    w.write_ue(v);

    // scaling_list_enabled_flag u(1) [+ optional scaling_list_data]
    let scaling_list_enabled = r.read_bits(1)?;
    w.write_bits(scaling_list_enabled, 1);
    if scaling_list_enabled == 1 {
        let sps_scaling_list_data_present = r.read_bits(1)?;
        w.write_bits(sps_scaling_list_data_present, 1);
        if sps_scaling_list_data_present == 1 {
            // scaling_list_data() — non-trivial, NVENC ULL doesn't emit
            // it. Bail so the caller falls back to the legacy DPB-only
            // patch (the bitstream still works, just no VUI override).
            return Err(EngineError::NotInitialized);
        }
    }

    // amp_enabled_flag u(1), sample_adaptive_offset_enabled_flag u(1)
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);

    // pcm_enabled_flag u(1)
    let pcm_enabled = r.read_bits(1)?;
    w.write_bits(pcm_enabled, 1);
    if pcm_enabled == 1 {
        let v = r.read_bits(4)?;
        w.write_bits(v, 4); // pcm_sample_bit_depth_luma_minus1
        let v = r.read_bits(4)?;
        w.write_bits(v, 4); // pcm_sample_bit_depth_chroma_minus1
        let v = r.read_ue()?;
        w.write_ue(v); // log2_min_pcm_luma_coding_block_size_minus3
        let v = r.read_ue()?;
        w.write_ue(v); // log2_diff_max_min_pcm_luma_coding_block_size
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // pcm_loop_filter_disabled_flag
    }

    // num_short_term_ref_pic_sets ue + array of st_ref_pic_set(i).
    let num_short_term_ref_pic_sets = r.read_ue()?;
    w.write_ue(num_short_term_ref_pic_sets);
    let mut num_delta_pocs = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for st_rps_idx in 0..num_short_term_ref_pic_sets {
        let n = copy_hevc_st_ref_pic_set(&mut r, &mut w, st_rps_idx, &num_delta_pocs)?;
        num_delta_pocs.push(n);
    }

    // long_term_ref_pics_present_flag u(1) + table
    let long_term_present = r.read_bits(1)?;
    w.write_bits(long_term_present, 1);
    if long_term_present == 1 {
        let num_long_term = r.read_ue()?;
        w.write_ue(num_long_term);
        let lt_lsb_bits = log2_max_poc + 4;
        for _ in 0..num_long_term {
            let v = r.read_bits(lt_lsb_bits)?;
            w.write_bits(v, lt_lsb_bits); // lt_ref_pic_poc_lsb_sps[i]
            let v = r.read_bits(1)?;
            w.write_bits(v, 1); // used_by_curr_pic_lt_sps_flag[i]
        }
    }

    // sps_temporal_mvp_enabled_flag u(1), strong_intra_smoothing_enabled_flag u(1)
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);

    // vui_parameters_present_flag — always force = 1 in output, then
    // either rewrite the existing VUI's video_signal_type or emit a
    // fresh one.
    let vui_present = r.read_bits(1)?;
    w.write_bits(1, 1);
    if vui_present == 1 {
        copy_hevc_vui_with_full_range_bt709(&mut r, &mut w)?;
    } else {
        emit_minimal_hevc_vui(&mut w);
    }

    // sps_extension_present_flag u(1) and any extensions. We don't
    // know which extensions are in use, so if the flag is set we bail
    // and let the fallback path copy the SPS through. This is rare
    // for ULL streams but harmless to be safe.
    let sps_extension_present = r.read_bits(1)?;
    if sps_extension_present == 1 {
        return Err(EngineError::NotInitialized);
    }
    w.write_bits(0, 1); // sps_extension_present_flag = 0

    // rbsp_trailing_bits() — not in source SPS yet at our cursor,
    // append our own. (We deliberately don't read trailing bits from
    // input because we may have rewritten preceding fields to
    // different bit lengths; the input's trailing bits no longer line
    // up.)
    w.write_rbsp_trailing_bits();
    Ok(w.into_bytes())
}

/// Walk the leading bytes of an HEVC SPS RBSP up to (and including)
/// the DPB-shaping triplet. Patches `sps_max_dec_pic_buffering_minus1`,
/// `sps_max_num_reorder_pics`, `sps_max_latency_increase_plus1` to 0.
/// Shared by the DPB-only fallback path and the full walk.
fn walk_hevc_sps_through_dpb(r: &mut BitReader, w: &mut BitWriter) -> EngineResult<()> {
    // sps_video_parameter_set_id u(4)
    let v = r.read_bits(4)?;
    w.write_bits(v, 4);
    // sps_max_sub_layers_minus1 u(3)
    let sps_max_sub_layers_minus1 = r.read_bits(3)?;
    w.write_bits(sps_max_sub_layers_minus1, 3);
    // sps_temporal_id_nesting_flag u(1)
    let v = r.read_bits(1)?;
    w.write_bits(v, 1);

    // profile_tier_level(profilePresentFlag=1, maxNumSubLayersMinus1=sps_max_sub_layers_minus1)
    copy_hevc_profile_tier_level(r, w, sps_max_sub_layers_minus1)?;

    // sps_seq_parameter_set_id ue(v)
    let v = r.read_ue()?;
    w.write_ue(v);
    let chroma_format_idc = r.read_ue()?;
    w.write_ue(chroma_format_idc);
    if chroma_format_idc == 3 {
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // separate_colour_plane_flag
    }
    let v = r.read_ue()?;
    w.write_ue(v); // pic_width_in_luma_samples
    let v = r.read_ue()?;
    w.write_ue(v); // pic_height_in_luma_samples

    let conformance_window = r.read_bits(1)?;
    w.write_bits(conformance_window, 1);
    if conformance_window == 1 {
        for _ in 0..4 {
            let v = r.read_ue()?;
            w.write_ue(v);
        }
    }

    let v = r.read_ue()?;
    w.write_ue(v); // bit_depth_luma_minus8
    let v = r.read_ue()?;
    w.write_ue(v); // bit_depth_chroma_minus8
    let v = r.read_ue()?;
    w.write_ue(v); // log2_max_pic_order_cnt_lsb_minus4

    let sub_layer_info_present = r.read_bits(1)?;
    w.write_bits(sub_layer_info_present, 1);

    let i_start = if sub_layer_info_present == 1 {
        0
    } else {
        sps_max_sub_layers_minus1
    };
    for _i in i_start..=sps_max_sub_layers_minus1 {
        // **sps_max_dec_pic_buffering_minus1[i]** — patch to 0 (DPB = 1).
        let _ = r.read_ue()?;
        w.write_ue(0);
        // **sps_max_num_reorder_pics[i]** — patch to 0.
        let _ = r.read_ue()?;
        w.write_ue(0);
        // **sps_max_latency_increase_plus1[i]** — 0 means "no constraint",
        // which is exactly what we want (decoder doesn't need to wait).
        let _ = r.read_ue()?;
        w.write_ue(0);
    }
    Ok(())
}

/// Re-parse the SPS just to recover `log2_max_pic_order_cnt_lsb_minus4`,
/// which `long_term_ref_pics_present_flag = 1` needs to know in order
/// to read fixed-width `lt_ref_pic_poc_lsb_sps[i]` fields. We do this
/// out-of-band rather than threading the value through `walk_hevc_sps_through_dpb`
/// because the latter is shared with the DPB-only path that doesn't
/// need it.
fn walk_hevc_sps_dpb_log2_max_poc(rbsp: &[u8]) -> EngineResult<u32> {
    let mut r = BitReader::new(rbsp);
    let _ = r.read_bits(4)?; // sps_video_parameter_set_id
    let sps_max_sub_layers_minus1 = r.read_bits(3)?;
    let _ = r.read_bits(1)?; // sps_temporal_id_nesting_flag

    // Skip profile_tier_level by re-running it through a throwaway writer.
    let mut throwaway = BitWriter::new();
    copy_hevc_profile_tier_level(&mut r, &mut throwaway, sps_max_sub_layers_minus1)?;

    let _ = r.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.read_ue()?;
    if chroma_format_idc == 3 {
        let _ = r.read_bits(1)?;
    }
    let _ = r.read_ue()?; // pic_width
    let _ = r.read_ue()?; // pic_height
    let conformance_window = r.read_bits(1)?;
    if conformance_window == 1 {
        for _ in 0..4 {
            let _ = r.read_ue()?;
        }
    }
    let _ = r.read_ue()?; // bit_depth_luma_minus8
    let _ = r.read_ue()?; // bit_depth_chroma_minus8
    let log2_max_poc = r.read_ue()?;
    Ok(log2_max_poc)
}

/// Copy `st_ref_pic_set(stRpsIdx)` per H.265 §7.3.7. Returns the
/// `NumDeltaPocs[stRpsIdx]` value the decoder will compute, so the
/// caller can use it when subsequent rps's predict from this one.
///
/// SPS context only — `stRpsIdx < num_short_term_ref_pic_sets` always,
/// so the slice-header-only `delta_idx_minus1` branch is never taken.
fn copy_hevc_st_ref_pic_set(
    r: &mut BitReader,
    w: &mut BitWriter,
    st_rps_idx: u32,
    num_delta_pocs: &[u32],
) -> EngineResult<u32> {
    let inter_ref_pic_set_prediction_flag = if st_rps_idx != 0 {
        let f = r.read_bits(1)?;
        w.write_bits(f, 1);
        f
    } else {
        0
    };
    if inter_ref_pic_set_prediction_flag == 1 {
        // delta_rps_sign u(1)
        let v = r.read_bits(1)?;
        w.write_bits(v, 1);
        // abs_delta_rps_minus1 ue
        let v = r.read_ue()?;
        w.write_ue(v);
        // RIdx = stRpsIdx - (delta_idx_minus1 + 1); in SPS context
        // delta_idx_minus1 isn't coded so it's implicitly 0, RIdx = stRpsIdx - 1.
        let r_idx = st_rps_idx
            .checked_sub(1)
            .ok_or(EngineError::NotInitialized)? as usize;
        let n_dp = *num_delta_pocs
            .get(r_idx)
            .ok_or(EngineError::NotInitialized)?;
        let mut new_num_delta_pocs = 0u32;
        // Loop bound is INCLUSIVE per spec: j in [0, NumDeltaPocs[RIdx]].
        for _ in 0..=n_dp {
            let used_by_curr_pic_flag = r.read_bits(1)?;
            w.write_bits(used_by_curr_pic_flag, 1);
            if used_by_curr_pic_flag == 0 {
                let use_delta_flag = r.read_bits(1)?;
                w.write_bits(use_delta_flag, 1);
                if use_delta_flag == 1 {
                    new_num_delta_pocs += 1;
                }
            } else {
                new_num_delta_pocs += 1;
            }
        }
        Ok(new_num_delta_pocs)
    } else {
        let num_neg = r.read_ue()?;
        w.write_ue(num_neg);
        let num_pos = r.read_ue()?;
        w.write_ue(num_pos);
        for _ in 0..num_neg {
            let v = r.read_ue()?;
            w.write_ue(v); // delta_poc_s0_minus1[i]
            let v = r.read_bits(1)?;
            w.write_bits(v, 1); // used_by_curr_pic_s0_flag[i]
        }
        for _ in 0..num_pos {
            let v = r.read_ue()?;
            w.write_ue(v); // delta_poc_s1_minus1[i]
            let v = r.read_bits(1)?;
            w.write_bits(v, 1); // used_by_curr_pic_s1_flag[i]
        }
        Ok(num_neg + num_pos)
    }
}

/// Copy the HEVC VUI (H.265 §E.2.1) bit-for-bit, except replace the
/// `video_signal_type` block with our full-range BT.709 override. The
/// rest of the VUI (timing, HRD, bitstream restriction) is sensitive
/// to the encoder's actual configuration, so we keep it.
fn copy_hevc_vui_with_full_range_bt709(r: &mut BitReader, w: &mut BitWriter) -> EngineResult<()> {
    let aspect_ratio_info_present = r.read_bits(1)?;
    w.write_bits(aspect_ratio_info_present, 1);
    if aspect_ratio_info_present == 1 {
        let aspect_ratio_idc = r.read_bits(8)?;
        w.write_bits(aspect_ratio_idc, 8);
        if aspect_ratio_idc == 255 {
            let v = r.read_bits(16)?;
            w.write_bits(v, 16);
            let v = r.read_bits(16)?;
            w.write_bits(v, 16);
        }
    }
    let overscan_info_present = r.read_bits(1)?;
    w.write_bits(overscan_info_present, 1);
    if overscan_info_present == 1 {
        let v = r.read_bits(1)?;
        w.write_bits(v, 1);
    }

    // === video_signal_type — DROP whatever the input had, emit ours.
    let video_signal_type_present = r.read_bits(1)?;
    if video_signal_type_present == 1 {
        let _ = r.read_bits(3)?; // video_format
        let _ = r.read_bits(1)?; // video_full_range_flag
        let colour_desc = r.read_bits(1)?;
        if colour_desc == 1 {
            let _ = r.read_bits(8)?; // colour_primaries
            let _ = r.read_bits(8)?; // transfer_characteristics
            let _ = r.read_bits(8)?; // matrix_coeffs
        }
    }
    write_full_range_bt709_video_signal_type(w);

    let chroma_loc_info_present = r.read_bits(1)?;
    w.write_bits(chroma_loc_info_present, 1);
    if chroma_loc_info_present == 1 {
        let v = r.read_ue()?;
        w.write_ue(v);
        let v = r.read_ue()?;
        w.write_ue(v);
    }
    let v = r.read_bits(1)?;
    w.write_bits(v, 1); // neutral_chroma_indication_flag
    let v = r.read_bits(1)?;
    w.write_bits(v, 1); // field_seq_flag
    let v = r.read_bits(1)?;
    w.write_bits(v, 1); // frame_field_info_present_flag
    let default_display_window = r.read_bits(1)?;
    w.write_bits(default_display_window, 1);
    if default_display_window == 1 {
        for _ in 0..4 {
            let v = r.read_ue()?;
            w.write_ue(v);
        }
    }

    let vui_timing_info_present = r.read_bits(1)?;
    w.write_bits(vui_timing_info_present, 1);
    if vui_timing_info_present == 1 {
        let v = r.read_bits(32)?;
        w.write_bits(v, 32); // vui_num_units_in_tick
        let v = r.read_bits(32)?;
        w.write_bits(v, 32); // vui_time_scale
        let poc_proportional = r.read_bits(1)?;
        w.write_bits(poc_proportional, 1);
        if poc_proportional == 1 {
            let v = r.read_ue()?;
            w.write_ue(v);
        }
        let vui_hrd_present = r.read_bits(1)?;
        if vui_hrd_present == 1 {
            // hrd_parameters() is non-trivial. NVENC ULL with
            // `outputBufferingPeriodSEI = 0` doesn't emit one, so this
            // is a rare path. Bail and let the DPB-only fallback ship
            // the bitstream unmodified.
            return Err(EngineError::NotInitialized);
        }
        w.write_bits(0, 1); // vui_hrd_parameters_present_flag = 0
    }

    let bitstream_restriction = r.read_bits(1)?;
    w.write_bits(bitstream_restriction, 1);
    if bitstream_restriction == 1 {
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // tiles_fixed_structure_flag
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // motion_vectors_over_pic_boundaries_flag
        let v = r.read_bits(1)?;
        w.write_bits(v, 1); // restricted_ref_pic_lists_flag
        let v = r.read_ue()?;
        w.write_ue(v); // min_spatial_segmentation_idc
        let v = r.read_ue()?;
        w.write_ue(v); // max_bytes_per_pic_denom
        let v = r.read_ue()?;
        w.write_ue(v); // max_bits_per_min_cu_denom
        let v = r.read_ue()?;
        w.write_ue(v); // log2_max_mv_length_horizontal
        let v = r.read_ue()?;
        w.write_ue(v); // log2_max_mv_length_vertical
    }
    Ok(())
}

/// Synthesise a minimal HEVC VUI carrying just the colour info we
/// care about. Used when the encoder didn't emit a VUI at all.
fn emit_minimal_hevc_vui(w: &mut BitWriter) {
    w.write_bits(0, 1); // aspect_ratio_info_present_flag
    w.write_bits(0, 1); // overscan_info_present_flag
    write_full_range_bt709_video_signal_type(w);
    w.write_bits(0, 1); // chroma_loc_info_present_flag
    w.write_bits(0, 1); // neutral_chroma_indication_flag
    w.write_bits(0, 1); // field_seq_flag
    w.write_bits(0, 1); // frame_field_info_present_flag
    w.write_bits(0, 1); // default_display_window_flag
    w.write_bits(0, 1); // vui_timing_info_present_flag
    w.write_bits(0, 1); // bitstream_restriction_flag
}

fn copy_hevc_profile_tier_level(
    r: &mut BitReader,
    w: &mut BitWriter,
    max_num_sub_layers_minus1: u32,
) -> EngineResult<()> {
    // general_profile_space(2) + general_tier_flag(1) + general_profile_idc(5)
    let v = r.read_bits(8)?;
    w.write_bits(v, 8);
    // general_profile_compatibility_flag[32]
    let v = r.read_bits(32)?;
    w.write_bits(v, 32);
    // general_progressive_source_flag(1) + general_interlaced_source_flag(1)
    // + general_non_packed_constraint_flag(1) + general_frame_only_constraint_flag(1)
    // + 43 reserved + general_inbld_flag(1) + general_level_idc(8)
    // = 1+1+1+1 + 43 + 1 + 8 = 56 bits, but easier to read in chunks.
    for _ in 0..7 {
        let v = r.read_bits(8)?;
        w.write_bits(v, 8);
    } // 56 bits

    // sub_layer_profile_present_flag[i] / sub_layer_level_present_flag[i] for each sub-layer.
    let mut sub_profile_present = [false; 7];
    let mut sub_level_present = [false; 7];
    for i in 0..max_num_sub_layers_minus1 as usize {
        let pp = r.read_bits(1)?;
        w.write_bits(pp, 1);
        let lp = r.read_bits(1)?;
        w.write_bits(lp, 1);
        sub_profile_present[i] = pp == 1;
        sub_level_present[i] = lp == 1;
    }
    // Reserved bits to byte-align to the next 8-byte boundary if
    // max_num_sub_layers_minus1 > 0.
    if max_num_sub_layers_minus1 > 0 {
        for _ in max_num_sub_layers_minus1..8 {
            let v = r.read_bits(2)?;
            w.write_bits(v, 2);
        }
    }
    for i in 0..max_num_sub_layers_minus1 as usize {
        if sub_profile_present[i] {
            // 2 + 1 + 5 + 32 + 4 + 43 + 1 = 88 bits.
            for _ in 0..11 {
                let v = r.read_bits(8)?;
                w.write_bits(v, 8);
            }
        }
        if sub_level_present[i] {
            let v = r.read_bits(8)?;
            w.write_bits(v, 8);
        }
    }
    Ok(())
}

/// Copy all remaining bits from the reader to the writer verbatim. Used
/// after the patched fields when the rest of the SPS contains structure
/// we don't care about (and don't want to re-parse).
fn copy_remaining_bits(r: &mut BitReader, w: &mut BitWriter) {
    let total_bits = r.bytes.len() * 8;
    while r.bit_pos < total_bits {
        // Try to copy a whole byte at a time when aligned.
        if r.bit_pos & 7 == 0 && total_bits - r.bit_pos >= 8 {
            let b = r.bytes[r.bit_pos >> 3];
            w.write_bits(b as u32, 8);
            r.bit_pos += 8;
        } else {
            match r.read_bits(1) {
                Ok(b) => w.write_bits(b, 1),
                Err(_) => break,
            }
        }
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbsp_round_trip() {
        let rbsp: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x42];
        let escaped = rbsp_escape(rbsp);
        // First 00 00 00 → must become 00 00 03 00. Then 01, 02, 00 00 42 →
        // also escaped because 00 00 followed by ≤ 03.
        // We don't need to assert exact bytes, just that round-trip works.
        let unescaped = rbsp_unescape(&escaped);
        assert_eq!(unescaped, rbsp);
    }

    #[test]
    fn ue_round_trip_small_values() {
        for v in 0u32..32 {
            let mut w = BitWriter::new();
            w.write_ue(v);
            // Pad to byte boundary so reader doesn't run off.
            while w.bit_pos & 7 != 0 {
                w.write_bits(0, 1);
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            let got = r.read_ue().expect("read");
            assert_eq!(got, v, "ue round trip failed for {v}");
        }
    }

    #[test]
    fn ue_round_trip_larger_values() {
        for &v in &[100u32, 1023, 1024, 65535, 65536, 1_000_000] {
            let mut w = BitWriter::new();
            w.write_ue(v);
            while w.bit_pos & 7 != 0 {
                w.write_bits(0, 1);
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_ue().expect("read"), v);
        }
    }

    #[test]
    fn se_round_trip() {
        for v in [0i32, 1, -1, 2, -2, 100, -100, 16384, -16384] {
            let mut w = BitWriter::new();
            w.write_se(v);
            while w.bit_pos & 7 != 0 {
                w.write_bits(0, 1);
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_se().expect("read"), v);
        }
    }

    /// Build a synthetic Baseline H.264 SPS RBSP with a chosen
    /// `max_num_ref_frames` and NO VUI. Construction uses the same
    /// BitWriter as production code, so syntactic validity is
    /// guaranteed by the round-trip tests above.
    fn build_synthetic_h264_baseline_sps(max_num_ref_frames: u32, with_vui: bool) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(66, 8); // profile_idc = Baseline
        w.write_bits(0xC0, 8); // constraint flags + reserved
        w.write_bits(40, 8); // level_idc
        w.write_ue(0); // seq_parameter_set_id
                       // No chroma block for profile_idc = 66.
        w.write_ue(0); // log2_max_frame_num_minus4
        w.write_ue(0); // pic_order_cnt_type = 0
        w.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.write_ue(max_num_ref_frames); // max_num_ref_frames
        w.write_bits(0, 1); // gaps_in_frame_num_value_allowed_flag
        w.write_ue(119); // pic_width_in_mbs_minus1 (1920/16 - 1)
        w.write_ue(67); // pic_height_in_map_units_minus1 (1088/16 - 1)
        w.write_bits(1, 1); // frame_mbs_only_flag
        w.write_bits(1, 1); // direct_8x8_inference_flag
        w.write_bits(0, 1); // frame_cropping_flag
        w.write_bits(if with_vui { 1 } else { 0 }, 1); // vui_parameters_present_flag
        if with_vui {
            // Minimal VUI with bitstream_restriction declaring max_num_ref_frames=4.
            for _ in 0..7 {
                w.write_bits(0, 1); // aspect/overscan/video/chroma/timing/nal_hrd/vcl_hrd
            }
            w.write_bits(0, 1); // pic_struct_present_flag
            w.write_bits(1, 1); // bitstream_restriction_flag
            w.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag
            w.write_ue(0); // max_bytes_per_pic_denom
            w.write_ue(0); // max_bits_per_mb_denom
            w.write_ue(16); // log2_max_mv_length_horizontal
            w.write_ue(16); // log2_max_mv_length_vertical
            w.write_ue(2); // max_num_reorder_frames (deliberately non-zero)
            w.write_ue(4); // max_dec_frame_buffering (deliberately non-1)
        }
        w.write_rbsp_trailing_bits();
        w.into_bytes()
    }

    /// Reverse-parse just enough of an H.264 SPS RBSP to recover
    /// `max_num_ref_frames` and (if VUI present) the bitstream-restriction
    /// reorder/dpb-buffering values. Used by tests below.
    fn read_h264_sps_dpb_fields(rbsp: &[u8]) -> (u32, Option<(u32, u32)>) {
        let mut r = BitReader::new(rbsp);
        let _profile = r.read_bits(8).unwrap();
        let _ = r.read_bits(8).unwrap();
        let _ = r.read_bits(8).unwrap();
        let _ = r.read_ue().unwrap(); // sps_id
        let _ = r.read_ue().unwrap(); // log2_max_frame_num_minus4
        let pic_order_cnt_type = r.read_ue().unwrap();
        match pic_order_cnt_type {
            0 => {
                let _ = r.read_ue().unwrap();
            }
            1 => panic!("not testing pic_order_cnt_type=1 path"),
            _ => {}
        }
        let max_num_ref_frames = r.read_ue().unwrap();
        let _ = r.read_bits(1).unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let frame_mbs_only = r.read_bits(1).unwrap();
        if frame_mbs_only == 0 {
            let _ = r.read_bits(1).unwrap();
        }
        let _ = r.read_bits(1).unwrap();
        let crop = r.read_bits(1).unwrap();
        if crop == 1 {
            for _ in 0..4 {
                let _ = r.read_ue().unwrap();
            }
        }
        let vui = r.read_bits(1).unwrap();
        let restriction = if vui == 1 {
            // aspect_ratio_info_present_flag (0 in our synthetic VUI; the
            // patcher copies the input value through).
            let aspect_present = r.read_bits(1).unwrap();
            if aspect_present == 1 {
                let aspect_idc = r.read_bits(8).unwrap();
                if aspect_idc == 255 {
                    let _ = r.read_bits(16).unwrap();
                    let _ = r.read_bits(16).unwrap();
                }
            }
            // overscan_info_present_flag
            let overscan_present = r.read_bits(1).unwrap();
            if overscan_present == 1 {
                let _ = r.read_bits(1).unwrap();
            }
            // video_signal_type_present_flag — the patcher always forces
            // this to 1, so the patched output will go into the if.
            let video_signal_type_present = r.read_bits(1).unwrap();
            if video_signal_type_present == 1 {
                let _video_format = r.read_bits(3).unwrap();
                let _video_full_range = r.read_bits(1).unwrap();
                let colour_desc = r.read_bits(1).unwrap();
                if colour_desc == 1 {
                    let _ = r.read_bits(8).unwrap();
                    let _ = r.read_bits(8).unwrap();
                    let _ = r.read_bits(8).unwrap();
                }
            }
            // chroma_loc_info_present_flag
            let chroma_present = r.read_bits(1).unwrap();
            if chroma_present == 1 {
                let _ = r.read_ue().unwrap();
                let _ = r.read_ue().unwrap();
            }
            // timing_info_present_flag
            let timing_present = r.read_bits(1).unwrap();
            if timing_present == 1 {
                let _ = r.read_bits(32).unwrap();
                let _ = r.read_bits(32).unwrap();
                let _ = r.read_bits(1).unwrap();
            }
            let nal_hrd = r.read_bits(1).unwrap();
            if nal_hrd == 1 {
                panic!("test SPS doesn't carry nal_hrd; patcher would copy it through");
            }
            let vcl_hrd = r.read_bits(1).unwrap();
            if vcl_hrd == 1 {
                panic!("test SPS doesn't carry vcl_hrd");
            }
            let _pic_struct = r.read_bits(1).unwrap();
            let bs_restr = r.read_bits(1).unwrap();
            if bs_restr == 1 {
                let _ = r.read_bits(1).unwrap(); // motion_vectors_over_pic_boundaries_flag
                let _ = r.read_ue().unwrap();
                let _ = r.read_ue().unwrap();
                let _ = r.read_ue().unwrap();
                let _ = r.read_ue().unwrap();
                let reorder = r.read_ue().unwrap();
                let dpb = r.read_ue().unwrap();
                Some((reorder, dpb))
            } else {
                None
            }
        } else {
            None
        };
        (max_num_ref_frames, restriction)
    }

    /// Extract `(video_signal_type_present, video_full_range_flag,
    /// colour_description_present, primaries, transfer, matrix)` from
    /// an H.264 SPS RBSP. Used to verify the issue-#1 colour patch.
    #[allow(clippy::type_complexity)]
    fn read_h264_sps_video_signal_type(
        rbsp: &[u8],
    ) -> (bool, Option<(u32, bool, Option<(u32, u32, u32)>)>) {
        let mut r = BitReader::new(rbsp);
        let _ = r.read_bits(8).unwrap(); // profile
        let _ = r.read_bits(8).unwrap();
        let _ = r.read_bits(8).unwrap();
        let _ = r.read_ue().unwrap(); // sps_id
        let _ = r.read_ue().unwrap(); // log2_max_frame_num_minus4
        let pic_order_cnt_type = r.read_ue().unwrap();
        if pic_order_cnt_type == 0 {
            let _ = r.read_ue().unwrap();
        }
        let _ = r.read_ue().unwrap(); // max_num_ref_frames
        let _ = r.read_bits(1).unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let frame_mbs_only = r.read_bits(1).unwrap();
        if frame_mbs_only == 0 {
            let _ = r.read_bits(1).unwrap();
        }
        let _ = r.read_bits(1).unwrap();
        let crop = r.read_bits(1).unwrap();
        if crop == 1 {
            for _ in 0..4 {
                let _ = r.read_ue().unwrap();
            }
        }
        let vui = r.read_bits(1).unwrap();
        if vui == 0 {
            return (false, None);
        }
        let aspect_present = r.read_bits(1).unwrap();
        if aspect_present == 1 {
            let aspect_idc = r.read_bits(8).unwrap();
            if aspect_idc == 255 {
                let _ = r.read_bits(16).unwrap();
                let _ = r.read_bits(16).unwrap();
            }
        }
        let overscan_present = r.read_bits(1).unwrap();
        if overscan_present == 1 {
            let _ = r.read_bits(1).unwrap();
        }
        let vst_present = r.read_bits(1).unwrap();
        if vst_present == 0 {
            return (true, None);
        }
        let video_format = r.read_bits(3).unwrap();
        let full_range = r.read_bits(1).unwrap() == 1;
        let colour_desc = r.read_bits(1).unwrap();
        let cd = if colour_desc == 1 {
            let p = r.read_bits(8).unwrap();
            let t = r.read_bits(8).unwrap();
            let m = r.read_bits(8).unwrap();
            Some((p, t, m))
        } else {
            None
        };
        (true, Some((video_format, full_range, cd)))
    }

    #[test]
    fn patch_h264_sps_with_vui() {
        let rbsp = build_synthetic_h264_baseline_sps(4, true);
        let (orig_ref, orig_restr) = read_h264_sps_dpb_fields(&rbsp);
        assert_eq!(orig_ref, 4);
        assert_eq!(orig_restr, Some((2, 4)));

        // Wrap in NAL header + start code so patch_packet_for_low_latency_dpb
        // can consume it.
        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x67]; // SPS NAL header
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::H264, &nal).expect("patch");

        // Same start code + NAL header preserved.
        assert_eq!(&patched[0..5], &nal[0..5]);
        let patched_rbsp = rbsp_unescape(&patched[5..]);
        let (new_ref, new_restr) = read_h264_sps_dpb_fields(&patched_rbsp);
        assert_eq!(new_ref, 1, "max_num_ref_frames should be patched to 1");
        assert_eq!(
            new_restr,
            Some((0, 1)),
            "bitstream_restriction max_num_reorder_frames + max_dec_frame_buffering should be (0, 1)"
        );
    }

    #[test]
    fn patch_h264_sps_without_vui_synthesises_one() {
        let rbsp = build_synthetic_h264_baseline_sps(4, false);
        let (orig_ref, orig_restr) = read_h264_sps_dpb_fields(&rbsp);
        assert_eq!(orig_ref, 4);
        assert_eq!(orig_restr, None);

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::H264, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[5..]);
        let (new_ref, new_restr) = read_h264_sps_dpb_fields(&patched_rbsp);
        assert_eq!(new_ref, 1);
        assert_eq!(
            new_restr,
            Some((0, 1)),
            "bitstream_restriction was not synthesised when VUI was absent"
        );
    }

    /// Build a synthetic HEVC SPS RBSP with sps_max_sub_layers_minus1 = 0
    /// and a chosen `max_dec_pic_buffering_minus1[0]` /
    /// `max_num_reorder_pics[0]`. profile_tier_level is filled with
    /// zeros (94 bits total: 8 + 32 + (1+1+1+1+43+0+8) = 96, but we
    /// drop the inbld_flag which is the 44th of the 43-reserved block
    /// per spec — easier to just emit 96 bits of zeros and let the
    /// parser walk through them).
    fn build_synthetic_hevc_sps(
        sps_max_dec_pic_buffering_minus1: u32,
        sps_max_num_reorder_pics: u32,
        sps_max_latency_increase_plus1: u32,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(0, 4); // sps_video_parameter_set_id
        w.write_bits(0, 3); // sps_max_sub_layers_minus1
        w.write_bits(1, 1); // sps_temporal_id_nesting_flag
                            // profile_tier_level: 96 bits when profile_present_flag=1 and
                            // max_num_sub_layers_minus1=0.
        for _ in 0..12 {
            w.write_bits(0, 8); // 96 bits = 12 bytes of zeros
        }
        w.write_ue(0); // sps_seq_parameter_set_id
        w.write_ue(1); // chroma_format_idc = 1 (4:2:0)
        w.write_ue(1919); // pic_width_in_luma_samples = 1920... wait this is u(v) not ue
                          // Hmm wait, per spec pic_width_in_luma_samples is ue(v). Let me check.
                          // Actually it IS ue(v) per H.265 §7.3.2.2.1. So 1920 directly.
                          // Already wrote 1919 — let me fix to write 1920 below for a more
                          // realistic test, but actually since this is just synthetic, anything
                          // works. Keep it small.
        w.write_ue(1080);
        w.write_bits(0, 1); // conformance_window_flag = 0
        w.write_ue(0); // bit_depth_luma_minus8
        w.write_ue(0); // bit_depth_chroma_minus8
        w.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.write_bits(1, 1); // sps_sub_layer_ordering_info_present_flag
                            // Just one entry since max_sub_layers_minus1 = 0.
        w.write_ue(sps_max_dec_pic_buffering_minus1);
        w.write_ue(sps_max_num_reorder_pics);
        w.write_ue(sps_max_latency_increase_plus1);
        // Tail: any bits — the patcher copies this through verbatim.
        // Add a few sentinel bytes so we can verify they're preserved.
        for _ in 0..4 {
            w.write_bits(0xAB, 8);
        }
        w.write_rbsp_trailing_bits();
        w.into_bytes()
    }

    fn read_hevc_sps_dpb_fields(rbsp: &[u8]) -> (u32, u32, u32) {
        let mut r = BitReader::new(rbsp);
        let _ = r.read_bits(4).unwrap();
        let _ = r.read_bits(3).unwrap();
        let _ = r.read_bits(1).unwrap();
        for _ in 0..12 {
            let _ = r.read_bits(8).unwrap();
        }
        let _ = r.read_ue().unwrap(); // sps_seq_parameter_set_id
        let _ = r.read_ue().unwrap(); // chroma_format_idc
        let _ = r.read_ue().unwrap(); // pic_width
        let _ = r.read_ue().unwrap(); // pic_height
        let _ = r.read_bits(1).unwrap(); // conformance_window_flag
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_bits(1).unwrap();
        let dpb = r.read_ue().unwrap();
        let reorder = r.read_ue().unwrap();
        let latency = r.read_ue().unwrap();
        (dpb, reorder, latency)
    }

    #[test]
    fn patch_hevc_sps() {
        let rbsp = build_synthetic_hevc_sps(3, 2, 5);
        let (orig_dpb, orig_reorder, orig_lat) = read_hevc_sps_dpb_fields(&rbsp);
        assert_eq!((orig_dpb, orig_reorder, orig_lat), (3, 2, 5));

        // Wrap as HEVC SPS NAL (type 33 → header byte = 0x42, second byte = 0x01).
        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x42, 0x01];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::Hevc, &nal).expect("patch");

        assert_eq!(&patched[0..6], &nal[0..6]);
        let patched_rbsp = rbsp_unescape(&patched[6..]);
        let (new_dpb, new_reorder, new_lat) = read_hevc_sps_dpb_fields(&patched_rbsp);
        assert_eq!(
            (new_dpb, new_reorder, new_lat),
            (0, 0, 0),
            "HEVC DPB fields not all patched to 0"
        );
    }

    /// Patcher must pass non-SPS NALs through completely unchanged.
    /// (PPS, IDR slice, AUD, SEI, etc.)
    #[test]
    fn patch_passes_non_sps_nals_through_unchanged() {
        // PPS (NAL 8) followed by a slice (NAL 5, IDR).
        let bytes: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0xb8, 0x40, 0x12, // IDR slice
        ];
        let out = patch_packet_for_low_latency_dpb(Codec::H264, bytes).expect("patch");
        assert_eq!(out, bytes);
    }

    // ===================================================================
    // Issue #1 — full-range BT.709 VUI override tests
    // ===================================================================

    /// H.264 path: input SPS has no VUI at all. Patched output must
    /// synthesise a VUI with `video_signal_type_present_flag = 1`,
    /// `video_full_range_flag = 1`, and BT.709 colour description.
    #[test]
    fn patch_h264_sps_no_vui_synthesises_full_range_bt709() {
        let rbsp = build_synthetic_h264_baseline_sps(4, false);
        let (orig_vui, _) = read_h264_sps_video_signal_type(&rbsp);
        assert!(!orig_vui, "synthetic input had no VUI");

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::H264, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[5..]);
        let (vui, vsi) = read_h264_sps_video_signal_type(&patched_rbsp);
        assert!(vui, "patched output must carry a VUI");
        let (_video_format, full_range, cd) = vsi.expect("video_signal_type fields");
        assert!(full_range, "video_full_range_flag must be 1");
        assert_eq!(
            cd,
            Some((1, 1, 1)),
            "colour_description must be BT.709 (primaries=1, transfer=1, matrix=1)"
        );
    }

    /// H.264 path: input SPS already has a VUI (built by the helper
    /// without video_signal_type). Patched output must still set
    /// full-range BT.709, overriding the absent video_signal_type.
    #[test]
    fn patch_h264_sps_existing_vui_gets_full_range_bt709() {
        let rbsp = build_synthetic_h264_baseline_sps(4, true);
        let (orig_vui, orig_vst) = read_h264_sps_video_signal_type(&rbsp);
        assert!(orig_vui);
        assert!(
            orig_vst.is_none(),
            "synthetic VUI helper omits video_signal_type"
        );

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::H264, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[5..]);
        let (vui, vsi) = read_h264_sps_video_signal_type(&patched_rbsp);
        assert!(vui);
        let (_video_format, full_range, cd) = vsi.expect("video_signal_type fields");
        assert!(full_range);
        assert_eq!(cd, Some((1, 1, 1)));
    }

    /// H.264 path: input SPS already has a VUI carrying a WRONG
    /// video_signal_type (limited-range BT.601). Patched output must
    /// override it with full-range BT.709.
    #[test]
    fn patch_h264_sps_overrides_wrong_video_signal_type() {
        let rbsp = build_h264_sps_with_video_signal_type(/*limited_range*/ false, 6, 6, 6);
        let (orig_vst, orig_vsi) = read_h264_sps_video_signal_type(&rbsp);
        assert!(orig_vst);
        assert_eq!(
            orig_vsi,
            Some((5, false, Some((6, 6, 6)))),
            "synthetic input declared limited-range BT.601"
        );

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::H264, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[5..]);
        let (vst, vsi) = read_h264_sps_video_signal_type(&patched_rbsp);
        assert!(vst);
        let (_, full_range, cd) = vsi.expect("video_signal_type fields");
        assert!(full_range, "wrong limited-range flag must be overridden");
        assert_eq!(
            cd,
            Some((1, 1, 1)),
            "wrong BT.601 colour description must be overridden to BT.709"
        );
    }

    /// Build an H.264 SPS RBSP with a VUI carrying an explicit
    /// `video_signal_type` block. Used by the override test.
    fn build_h264_sps_with_video_signal_type(
        full_range: bool,
        primaries: u32,
        transfer: u32,
        matrix: u32,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(66, 8); // profile_idc
        w.write_bits(0xC0, 8);
        w.write_bits(40, 8);
        w.write_ue(0); // sps_id
        w.write_ue(0); // log2_max_frame_num_minus4
        w.write_ue(0); // pic_order_cnt_type
        w.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.write_ue(4); // max_num_ref_frames
        w.write_bits(0, 1); // gaps_in_frame_num_value_allowed_flag
        w.write_ue(119); // pic_width_in_mbs_minus1
        w.write_ue(67); // pic_height_in_map_units_minus1
        w.write_bits(1, 1); // frame_mbs_only_flag
        w.write_bits(1, 1); // direct_8x8_inference_flag
        w.write_bits(0, 1); // frame_cropping_flag
        w.write_bits(1, 1); // vui_parameters_present_flag
                            // VUI:
        w.write_bits(0, 1); // aspect_ratio_info_present_flag
        w.write_bits(0, 1); // overscan_info_present_flag
        w.write_bits(1, 1); // video_signal_type_present_flag = 1
        w.write_bits(5, 3); // video_format = Unspecified
        w.write_bits(if full_range { 1 } else { 0 }, 1); // video_full_range_flag
        w.write_bits(1, 1); // colour_description_present_flag
        w.write_bits(primaries, 8);
        w.write_bits(transfer, 8);
        w.write_bits(matrix, 8);
        w.write_bits(0, 1); // chroma_loc_info_present_flag
        w.write_bits(0, 1); // timing_info_present_flag
        w.write_bits(0, 1); // nal_hrd_parameters_present_flag
        w.write_bits(0, 1); // vcl_hrd_parameters_present_flag
        w.write_bits(0, 1); // pic_struct_present_flag
        w.write_bits(0, 1); // bitstream_restriction_flag
        w.write_rbsp_trailing_bits();
        w.into_bytes()
    }

    /// Build a *complete* HEVC SPS RBSP — every field present including
    /// the post-DPB block needed to reach the VUI. Configurable
    /// `video_full_range_flag` and colour-description triple so we can
    /// verify the patcher overrides whatever the encoder wrote.
    fn build_complete_hevc_sps(
        sps_max_dec_pic_buffering_minus1: u32,
        full_range_in: bool,
        primaries_in: u32,
        transfer_in: u32,
        matrix_in: u32,
        with_vui: bool,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(0, 4); // sps_video_parameter_set_id
        w.write_bits(0, 3); // sps_max_sub_layers_minus1 = 0
        w.write_bits(1, 1); // sps_temporal_id_nesting_flag
                            // profile_tier_level: 96 bits when max_num_sub_layers_minus1 = 0.
        for _ in 0..12 {
            w.write_bits(0, 8);
        }
        w.write_ue(0); // sps_seq_parameter_set_id
        w.write_ue(1); // chroma_format_idc = 4:2:0
        w.write_ue(1920); // pic_width_in_luma_samples
        w.write_ue(1080); // pic_height_in_luma_samples
        w.write_bits(0, 1); // conformance_window_flag
        w.write_ue(0); // bit_depth_luma_minus8
        w.write_ue(0); // bit_depth_chroma_minus8
        w.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
        w.write_bits(1, 1); // sps_sub_layer_ordering_info_present_flag
        w.write_ue(sps_max_dec_pic_buffering_minus1);
        w.write_ue(0); // sps_max_num_reorder_pics
        w.write_ue(0); // sps_max_latency_increase_plus1

        // Post-DPB block — sensible NVENC ULL defaults.
        w.write_ue(0); // log2_min_luma_coding_block_size_minus3
        w.write_ue(3); // log2_diff_max_min_luma_coding_block_size
        w.write_ue(0); // log2_min_luma_transform_block_size_minus2
        w.write_ue(3); // log2_diff_max_min_luma_transform_block_size
        w.write_ue(0); // max_transform_hierarchy_depth_inter
        w.write_ue(0); // max_transform_hierarchy_depth_intra
        w.write_bits(0, 1); // scaling_list_enabled_flag
        w.write_bits(1, 1); // amp_enabled_flag
        w.write_bits(1, 1); // sample_adaptive_offset_enabled_flag
        w.write_bits(0, 1); // pcm_enabled_flag
        w.write_ue(0); // num_short_term_ref_pic_sets
        w.write_bits(0, 1); // long_term_ref_pics_present_flag
        w.write_bits(1, 1); // sps_temporal_mvp_enabled_flag
        w.write_bits(0, 1); // strong_intra_smoothing_enabled_flag
        w.write_bits(if with_vui { 1 } else { 0 }, 1); // vui_parameters_present_flag

        if with_vui {
            w.write_bits(0, 1); // aspect_ratio_info_present_flag
            w.write_bits(0, 1); // overscan_info_present_flag
            w.write_bits(1, 1); // video_signal_type_present_flag
            w.write_bits(5, 3); // video_format = Unspecified
            w.write_bits(if full_range_in { 1 } else { 0 }, 1);
            w.write_bits(1, 1); // colour_description_present_flag
            w.write_bits(primaries_in, 8);
            w.write_bits(transfer_in, 8);
            w.write_bits(matrix_in, 8);
            w.write_bits(0, 1); // chroma_loc_info_present_flag
            w.write_bits(0, 1); // neutral_chroma_indication_flag
            w.write_bits(0, 1); // field_seq_flag
            w.write_bits(0, 1); // frame_field_info_present_flag
            w.write_bits(0, 1); // default_display_window_flag
            w.write_bits(0, 1); // vui_timing_info_present_flag
            w.write_bits(0, 1); // bitstream_restriction_flag
        }

        w.write_bits(0, 1); // sps_extension_present_flag
        w.write_rbsp_trailing_bits();
        w.into_bytes()
    }

    /// Walk an HEVC SPS RBSP all the way to the VUI's
    /// `video_signal_type` block and return its values. Returns
    /// `(vui_present, video_signal_type)` where the inner option is
    /// `(full_range, colour_description)`.
    #[allow(clippy::type_complexity)]
    fn read_hevc_sps_video_signal_type(
        rbsp: &[u8],
    ) -> (bool, Option<(bool, Option<(u32, u32, u32)>)>) {
        let mut r = BitReader::new(rbsp);
        let _ = r.read_bits(4).unwrap();
        let sps_max_sub_layers_minus1 = r.read_bits(3).unwrap();
        let _ = r.read_bits(1).unwrap();
        let mut throwaway = BitWriter::new();
        copy_hevc_profile_tier_level(&mut r, &mut throwaway, sps_max_sub_layers_minus1).unwrap();
        let _ = r.read_ue().unwrap();
        let chroma_format_idc = r.read_ue().unwrap();
        if chroma_format_idc == 3 {
            let _ = r.read_bits(1).unwrap();
        }
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let conformance = r.read_bits(1).unwrap();
        if conformance == 1 {
            for _ in 0..4 {
                let _ = r.read_ue().unwrap();
            }
        }
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let sub_layer_info_present = r.read_bits(1).unwrap();
        let i_start = if sub_layer_info_present == 1 {
            0
        } else {
            sps_max_sub_layers_minus1
        };
        for _ in i_start..=sps_max_sub_layers_minus1 {
            let _ = r.read_ue().unwrap();
            let _ = r.read_ue().unwrap();
            let _ = r.read_ue().unwrap();
        }
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let scaling_enabled = r.read_bits(1).unwrap();
        assert_eq!(scaling_enabled, 0);
        let _ = r.read_bits(1).unwrap(); // amp
        let _ = r.read_bits(1).unwrap(); // sao
        let pcm_enabled = r.read_bits(1).unwrap();
        assert_eq!(pcm_enabled, 0);
        let n_rps = r.read_ue().unwrap();
        assert_eq!(n_rps, 0, "test SPS doesn't model st_ref_pic_set");
        let lt_present = r.read_bits(1).unwrap();
        assert_eq!(lt_present, 0);
        let _ = r.read_bits(1).unwrap(); // sps_temporal_mvp_enabled_flag
        let _ = r.read_bits(1).unwrap(); // strong_intra_smoothing
        let vui_present = r.read_bits(1).unwrap();
        if vui_present == 0 {
            return (false, None);
        }
        let aspect = r.read_bits(1).unwrap();
        if aspect == 1 {
            let idc = r.read_bits(8).unwrap();
            if idc == 255 {
                let _ = r.read_bits(16).unwrap();
                let _ = r.read_bits(16).unwrap();
            }
        }
        let overscan = r.read_bits(1).unwrap();
        if overscan == 1 {
            let _ = r.read_bits(1).unwrap();
        }
        let vst = r.read_bits(1).unwrap();
        if vst == 0 {
            return (true, None);
        }
        let _ = r.read_bits(3).unwrap();
        let full_range = r.read_bits(1).unwrap() == 1;
        let cd_present = r.read_bits(1).unwrap();
        let cd = if cd_present == 1 {
            let p = r.read_bits(8).unwrap();
            let t = r.read_bits(8).unwrap();
            let m = r.read_bits(8).unwrap();
            Some((p, t, m))
        } else {
            None
        };
        (true, Some((full_range, cd)))
    }

    /// HEVC path: complete SPS with a wrong limited-range BT.601
    /// video_signal_type. Patched output must override to full-range
    /// BT.709 AND keep DPB triplet patched to (0, 0, 0).
    #[test]
    fn patch_hevc_sps_overrides_wrong_video_signal_type() {
        let rbsp = build_complete_hevc_sps(3, false, 6, 6, 6, true);
        let (vui, vst) = read_hevc_sps_video_signal_type(&rbsp);
        assert!(vui);
        assert_eq!(vst, Some((false, Some((6, 6, 6)))));

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x42, 0x01];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::Hevc, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[6..]);

        let (dpb, reorder, lat) = read_hevc_sps_dpb_fields_via_full_walk(&patched_rbsp);
        assert_eq!(
            (dpb, reorder, lat),
            (0, 0, 0),
            "DPB triplet must still be patched"
        );

        let (vui_p, vst_p) = read_hevc_sps_video_signal_type(&patched_rbsp);
        assert!(vui_p);
        let (full_range, cd) = vst_p.expect("video_signal_type present in patched output");
        assert!(full_range);
        assert_eq!(cd, Some((1, 1, 1)));
    }

    /// HEVC path: complete SPS without a VUI. Patcher must synthesise
    /// one declaring full-range BT.709.
    #[test]
    fn patch_hevc_sps_no_vui_synthesises_full_range_bt709() {
        let rbsp = build_complete_hevc_sps(3, false, 0, 0, 0, false);
        let (vui, _) = read_hevc_sps_video_signal_type(&rbsp);
        assert!(!vui, "input SPS had no VUI");

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x42, 0x01];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::Hevc, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[6..]);

        let (vui_p, vst_p) = read_hevc_sps_video_signal_type(&patched_rbsp);
        assert!(vui_p, "patcher must force vui_parameters_present_flag = 1");
        let (full_range, cd) = vst_p.expect("video_signal_type synthesised");
        assert!(full_range);
        assert_eq!(cd, Some((1, 1, 1)));
    }

    /// Read DPB triplet from a complete HEVC SPS (the existing
    /// `read_hevc_sps_dpb_fields` helper assumes the synthetic SPS
    /// shape; this one walks the realistic structure).
    fn read_hevc_sps_dpb_fields_via_full_walk(rbsp: &[u8]) -> (u32, u32, u32) {
        let mut r = BitReader::new(rbsp);
        let _ = r.read_bits(4).unwrap();
        let sps_max_sub_layers_minus1 = r.read_bits(3).unwrap();
        let _ = r.read_bits(1).unwrap();
        let mut throwaway = BitWriter::new();
        copy_hevc_profile_tier_level(&mut r, &mut throwaway, sps_max_sub_layers_minus1).unwrap();
        let _ = r.read_ue().unwrap();
        let chroma_format_idc = r.read_ue().unwrap();
        if chroma_format_idc == 3 {
            let _ = r.read_bits(1).unwrap();
        }
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let conformance = r.read_bits(1).unwrap();
        if conformance == 1 {
            for _ in 0..4 {
                let _ = r.read_ue().unwrap();
            }
        }
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_ue().unwrap();
        let _ = r.read_bits(1).unwrap(); // sub_layer_info_present
        let dpb = r.read_ue().unwrap();
        let reorder = r.read_ue().unwrap();
        let latency = r.read_ue().unwrap();
        (dpb, reorder, latency)
    }

    /// HEVC fallback path: an SPS that uses scaling_list_data must
    /// still get the DPB patch even though we bail on VUI override.
    #[test]
    fn patch_hevc_sps_with_scaling_list_falls_back_to_dpb_only() {
        let mut w = BitWriter::new();
        w.write_bits(0, 4); // sps_video_parameter_set_id
        w.write_bits(0, 3); // sps_max_sub_layers_minus1
        w.write_bits(1, 1); // sps_temporal_id_nesting_flag
        for _ in 0..12 {
            w.write_bits(0, 8);
        } // profile_tier_level
        w.write_ue(0); // sps_seq_parameter_set_id
        w.write_ue(1); // chroma_format_idc
        w.write_ue(1920);
        w.write_ue(1080);
        w.write_bits(0, 1); // conformance_window_flag
        w.write_ue(0);
        w.write_ue(0);
        w.write_ue(4);
        w.write_bits(1, 1); // sps_sub_layer_ordering_info_present_flag
        w.write_ue(3); // sps_max_dec_pic_buffering_minus1 (non-zero — should still get patched)
        w.write_ue(2); // sps_max_num_reorder_pics
        w.write_ue(5); // sps_max_latency_increase_plus1
        w.write_ue(0);
        w.write_ue(3);
        w.write_ue(0);
        w.write_ue(3);
        w.write_ue(0);
        w.write_ue(0);
        w.write_bits(1, 1); // scaling_list_enabled_flag = 1 → triggers fallback
        w.write_bits(1, 1); // sps_scaling_list_data_present_flag = 1
                            // Sentinel garbage where scaling_list_data should be — the full
                            // walk will return Err, the DPB-only fallback will copy through.
        for _ in 0..8 {
            w.write_bits(0xAB, 8);
        }
        w.write_rbsp_trailing_bits();
        let rbsp = w.into_bytes();

        let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x42, 0x01];
        nal.extend_from_slice(&rbsp_escape(&rbsp));
        let patched = patch_packet_for_low_latency_dpb(Codec::Hevc, &nal).expect("patch");
        let patched_rbsp = rbsp_unescape(&patched[6..]);

        // DPB triplet must be patched to (0, 0, 0) by the fallback.
        let (dpb, reorder, latency) = read_hevc_sps_dpb_fields_via_full_walk(&patched_rbsp);
        assert_eq!(
            (dpb, reorder, latency),
            (0, 0, 0),
            "fallback must still patch DPB"
        );
    }
}
