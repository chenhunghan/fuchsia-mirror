// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::NtbError;
use crate::wire::{
    DPE16_LENGTH, NDP16_HEADER_LENGTH, NDP16_MIN_LENGTH, NDP16_SIGNATURE_NO_CRC, NTB_ALIGNMENT,
    NTH16_HEADER_LENGTH, NTH16_SIGNATURE, NcmDpe16, NcmNdp16Header, NcmNth16,
};
use packet::{BufferView, BufferViewMut, SliceBufViewMut};
use zerocopy::byteorder::little_endian::{U16, U32};

/// Stack `DPE16` capacity per NTB (including the `(0, 0)` terminator), avoiding TX heap allocation
/// while covering practical frame mixes (up to 511 datagrams per block).
const MAX_STACK_DPE16_ENTRIES: usize = 512;

/// USB 2.0 High-Speed bulk `wMaxPacketSize`, used to detect exact packet-boundary transfers.
const USB_HIGH_SPEED_BULK_MPS: usize = 512;

fn align_up_ntb(val: usize) -> Result<usize, NtbError> {
    const { assert!(NTB_ALIGNMENT.is_power_of_two()) };
    val.checked_add(NTB_ALIGNMENT - 1)
        .map(|v| v & !(NTB_ALIGNMENT - 1))
        .ok_or(NtbError::ArithmeticOverflow)
}

/// 16-bit wrapping NTB sequence counter (`wSequence`, CDC NCM 1.0 Table 3-1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct NtbSequence(u16);

impl NtbSequence {
    /// Creates a sequence counter initialized to `value`.
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns the raw 16-bit sequence value.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Advances the sequence counter by one, wrapping at `u16::MAX`.
    pub fn increment(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

/// Result of encoding an NTB-16 block into a destination buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ntb16BuildResult {
    /// Total encoded NTB-16 block length in bytes (including headers, datagrams, and padding).
    pub block_len: usize,
    /// Number of datagrams packed into the NTB-16 block.
    pub packed_count: usize,
}

/// Single-pass NTB-16 (`NDP-at-end`) encoder writing directly into a caller-provided slice.
///
/// Placing `NDP16` after all datagrams fixes frame 0 at offset 12 (`NTH16_HEADER_LENGTH`),
/// allowing greedy packing without knowing `packed_count` upfront or monomorphizing over
/// caller frame sources.
pub struct Ntb16Builder<'a> {
    view: SliceBufViewMut<'a>,
    capped_max_size: usize,
    nth: zerocopy::Ref<&'a mut [u8], NcmNth16>,
    dpes: [NcmDpe16; MAX_STACK_DPE16_ENTRIES],
    packed_count: usize,
}

impl<'a> Ntb16Builder<'a> {
    /// Starts encoding an NTB-16 block in `dst[..max_ntb_size]`, reserving space for `NTH16`.
    ///
    /// # Errors
    ///
    /// Returns [`NtbError::BufferTooSmall`] if `dst[..max_ntb_size]` cannot fit `NTH16` plus an
    /// empty `NDP16`.
    pub fn new(dst: &'a mut [u8], max_ntb_size: usize) -> Result<Self, NtbError> {
        let capped_max_size = max_ntb_size.min(dst.len()).min(usize::from(u16::MAX));
        let mut view = SliceBufViewMut::new(&mut dst[..capped_max_size]);
        if view.len() < usize::from(NTH16_HEADER_LENGTH) + usize::from(NDP16_MIN_LENGTH) {
            return Err(NtbError::BufferTooSmall);
        }

        // Reserve the 12-byte NTH16 slot at offset 0; populated in `finish` once `block_len` and
        // `ndp_offset` are finalized.
        let nth = view.take_obj_front_zero::<NcmNth16>().ok_or(NtbError::BufferTooSmall)?;
        Ok(Self {
            view,
            capped_max_size,
            nth,
            dpes: [NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::ZERO };
                MAX_STACK_DPE16_ENTRIES],
            packed_count: 0,
        })
    }

    fn written(&self) -> usize {
        self.capped_max_size - self.view.len()
    }

    /// Reserves a `frame_len`-byte slot in the NTB-16 buffer for the caller to populate.
    ///
    /// Returns `Ok(None)` if the NTB is full after at least one packed frame.
    ///
    /// # Errors
    ///
    /// Returns [`NtbError::EmptyInput`] if `frame_len` is zero,
    /// [`NtbError::BufferTooSmall`] if the first frame cannot fit, or
    /// [`NtbError::ArithmeticOverflow`] if any offset or length overflows.
    pub fn try_alloc_frame(&mut self, frame_len: usize) -> Result<Option<&mut [u8]>, NtbError> {
        if frame_len == 0 {
            return Err(NtbError::EmptyInput);
        }
        if self.packed_count + 1 >= MAX_STACK_DPE16_ENTRIES {
            return Ok(None);
        }

        let offset = self.written();
        let pad = align_up_ntb(offset)? - offset;
        let aligned_dg_offset = offset + pad;
        let after_dg =
            aligned_dg_offset.checked_add(frame_len).ok_or(NtbError::ArithmeticOverflow)?;
        let trailing_pad = align_up_ntb(after_dg)? - after_dg;
        let candidate_dpe_count = self
            .packed_count
            .checked_add(2) // +1 for this datagram, +1 for terminating (0, 0) entry
            .ok_or(NtbError::ArithmeticOverflow)?;
        let candidate_ndp_len = candidate_dpe_count
            .checked_mul(DPE16_LENGTH)
            .and_then(|n| n.checked_add(NDP16_HEADER_LENGTH))
            .ok_or(NtbError::ArithmeticOverflow)?;
        let needed = pad
            .checked_add(frame_len)
            .and_then(|n| n.checked_add(trailing_pad))
            .and_then(|n| n.checked_add(candidate_ndp_len))
            .ok_or(NtbError::ArithmeticOverflow)?;

        if needed > self.view.len() {
            if self.packed_count == 0 {
                return Err(NtbError::BufferTooSmall);
            }
            return Ok(None);
        }

        let _ = self.view.take_front_zero(pad).ok_or(NtbError::BufferTooSmall)?;
        let dst_slice = self.view.take_front(frame_len).ok_or(NtbError::BufferTooSmall)?;
        self.dpes[self.packed_count] = NcmDpe16 {
            datagram_index: U16::new(
                u16::try_from(aligned_dg_offset).map_err(|_| NtbError::ArithmeticOverflow)?,
            ),
            datagram_length: U16::new(
                u16::try_from(frame_len).map_err(|_| NtbError::ArithmeticOverflow)?,
            ),
        };
        self.packed_count += 1;
        Ok(Some(dst_slice))
    }

    /// Writes the trailing `NDP16` table, optional short-packet padding, and `NTH16` header using
    /// `sequence`. Callers are expected to advance their [`NtbSequence`] via
    /// [`NtbSequence::increment`] for each successfully encoded block.
    ///
    /// # Errors
    ///
    /// Returns [`NtbError::EmptyInput`] if no frames were added, [`NtbError::BufferTooSmall`] if
    /// writing the trailer fails, or [`NtbError::ArithmeticOverflow`] on `u16` overflow.
    pub fn finish(mut self, sequence: NtbSequence) -> Result<Ntb16BuildResult, NtbError> {
        if self.packed_count == 0 {
            return Err(NtbError::EmptyInput);
        }

        let pre_ndp = self.written();
        let ndp_pad = align_up_ntb(pre_ndp)? - pre_ndp;
        let _ = self.view.take_front_zero(ndp_pad).ok_or(NtbError::BufferTooSmall)?;
        let ndp_offset = self.written();

        // `dpes[packed_count]` is already initialized to the terminating `(0, 0)` entry.
        let total_dpes = self.packed_count + 1;
        let ndp_len = total_dpes
            .checked_mul(DPE16_LENGTH)
            .and_then(|n| n.checked_add(NDP16_HEADER_LENGTH))
            .ok_or(NtbError::ArithmeticOverflow)?;

        let ndp_hdr = NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(u16::try_from(ndp_len).map_err(|_| NtbError::ArithmeticOverflow)?),
            next_ndp_index: U16::ZERO,
        };
        self.view.write_obj_front(&ndp_hdr).ok_or(NtbError::BufferTooSmall)?;
        self.view.write_obj_front(&self.dpes[..total_dpes]).ok_or(NtbError::BufferTooSmall)?;

        // Bulk IN transfers smaller than `capped_max_size` end on a short packet (`< 512` B). On an
        // exact 512 B multiple, append 4 zero bytes so one DMA TRB terminates the URB without an
        // extra ZLP transaction; hosts stop at `NDP16`'s `(0, 0)` entry. Skip at `capped_max_size`
        // (`view.is_empty()`), where buffer-full already completes the URB.
        let cur_len = self.written();
        if self.view.len() >= NTB_ALIGNMENT && cur_len.is_multiple_of(USB_HIGH_SPEED_BULK_MPS) {
            let _ = self.view.take_front_zero(NTB_ALIGNMENT);
        }

        let final_len = self.written();
        let block_len = u16::try_from(final_len).map_err(|_| NtbError::ArithmeticOverflow)?;

        *self.nth = NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::new(sequence.get()),
            block_length: U16::new(block_len),
            ndp_index: U16::new(
                u16::try_from(ndp_offset).map_err(|_| NtbError::ArithmeticOverflow)?,
            ),
        };

        Ok(Ntb16BuildResult { block_len: final_len, packed_count: self.packed_count })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::parser::parse_ntb16;
    use std::vec;
    use std::vec::Vec;

    pub(crate) fn build_ntb16(
        frames: &[&[u8]],
        sequence: NtbSequence,
        max_ntb_size: usize,
    ) -> Result<(Vec<u8>, usize), NtbError> {
        if frames.is_empty() {
            return Err(NtbError::EmptyInput);
        }
        let capped_max_size = max_ntb_size.min(usize::from(u16::MAX));
        let mut payload_buf = vec![0u8; capped_max_size];
        let mut builder = Ntb16Builder::new(&mut payload_buf, capped_max_size)?;
        for &frame in frames {
            let Some(slot) = builder.try_alloc_frame(frame.len())? else {
                break;
            };
            slot.copy_from_slice(frame);
        }
        let result = builder.finish(sequence)?;
        payload_buf.truncate(result.block_len);
        Ok((payload_buf, result.packed_count))
    }

    #[test]
    fn test_ntb_sequence_wraps_on_increment() {
        let mut seq = NtbSequence::default();
        assert_eq!(seq.get(), 0);
        seq.increment();
        assert_eq!(seq.get(), 1);

        let mut max_seq = NtbSequence::new(u16::MAX);
        assert_eq!(max_seq.get(), u16::MAX);
        max_seq.increment();
        assert_eq!(max_seq.get(), 0);
    }

    #[test]
    fn test_ntb16_single_datagram_roundtrip() {
        let frame = [0x42u8; 64];
        let mut seq = NtbSequence::default();
        let (ntb, count) = build_ntb16(&[&frame[..]], seq, 16384).expect("build_ntb16");
        assert_eq!(count, 1);
        seq.increment();
        assert_eq!(seq.get(), 1);

        let mut view = &ntb[..];
        let nth = (&mut view).take_obj_front::<NcmNth16>().expect("read nth16");
        assert_eq!(nth.sequence.get(), 0);

        let parsed: Vec<_> = parse_ntb16(&ntb).expect("parse_ntb16").collect();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], &frame[..]);
    }

    #[test]
    fn test_ntb16_multi_datagram_roundtrip() {
        let f1 = [0x11u8; 14];
        let f2 = [0x22u8; 63];
        let f3 = [0x33u8; 128];
        let f4 = [0x44u8; 513];
        let f5 = [0x55u8; 1514];
        let frames = [&f1[..], &f2[..], &f3[..], &f4[..], &f5[..]];

        let seq = NtbSequence::new(7);
        let (ntb, count) = build_ntb16(&frames, seq, 16384).expect("build_ntb16");
        assert_eq!(count, 5);

        // Verify all datagram offsets and the NDP offset are 4-byte aligned.
        let mut nth_slice = &ntb[..];
        let nth = (&mut nth_slice).take_obj_front::<NcmNth16>().expect("read nth16");
        assert_eq!(nth.sequence.get(), 7);
        let ndp_idx = usize::from(nth.ndp_index.get());
        assert_eq!(ndp_idx % 4, 0);

        let mut ndp_slice = &ntb[ndp_idx..];
        let mut ndp_view = &mut ndp_slice;
        let _ndp_hdr = ndp_view.take_obj_front::<NcmNdp16Header>().expect("read ndp16");
        let dpes = ndp_view.take_slice_front::<NcmDpe16>(5).expect("read dpe16 slice");
        for dpe in dpes.iter() {
            assert_eq!(dpe.datagram_index.get() % 4, 0);
        }

        let parsed: Vec<_> = parse_ntb16(&ntb).expect("parse_ntb16").collect();
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0], &f1[..]);
        assert_eq!(parsed[1], &f2[..]);
        assert_eq!(parsed[2], &f3[..]);
        assert_eq!(parsed[3], &f4[..]);
        assert_eq!(parsed[4], &f5[..]);
    }

    #[test]
    fn test_ntb16_512_byte_boundary_padding() {
        // NTH16 (12B) + frame (484B) + NDP16 (16B) = 512B before short-packet padding.
        let frame = vec![0x77u8; 484];
        let mut seq = NtbSequence::default();
        let (ntb, count) = build_ntb16(&[&frame[..]], seq, 16384).expect("build_ntb16");
        seq.increment();
        assert_eq!(count, 1);
        assert_eq!(ntb.len(), 516);
        assert_ne!(ntb.len() % 512, 0);

        let mut nth_slice = &ntb[..];
        let nth = (&mut nth_slice).take_obj_front::<NcmNth16>().expect("read nth16");
        assert_eq!(nth.block_length.get(), 516);

        let parsed: Vec<_> = parse_ntb16(&ntb).expect("parse padded ntb16").collect();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], &frame[..]);

        // When the block equals `max_ntb_size` (512B), padding is skipped so the buffer does not
        // overflow.
        let (exact_ntb, exact_count) =
            build_ntb16(&[&frame[..]], seq, 512).expect("build exact 512B ntb");
        assert_eq!(exact_count, 1);
        assert_eq!(exact_ntb.len(), 512);
    }

    #[test]
    fn test_ntb16_empty_and_small_buffer_errors() {
        let seq = NtbSequence::default();
        let empty: [&[u8]; 0] = [];
        assert_eq!(build_ntb16(&empty, seq, 16384), Err(NtbError::EmptyInput));

        let frame = [0xAAu8; 64];
        assert_eq!(build_ntb16(&[&[][..]], seq, 16384), Err(NtbError::EmptyInput));
        assert_eq!(build_ntb16(&[&frame[..], &[][..]], seq, 16384), Err(NtbError::EmptyInput));

        assert_eq!(build_ntb16(&[&frame[..]], seq, 64), Err(NtbError::BufferTooSmall));
    }
}
