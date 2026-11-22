// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::NtbError;
use crate::wire::{
    DPE16_LENGTH, NDP16_HEADER_LENGTH, NDP16_MIN_LENGTH, NDP16_SIGNATURE_NO_CRC,
    NTH16_HEADER_LENGTH, NTH16_SIGNATURE, NcmDpe16, NcmNdp16Header, NcmNth16,
};
use core::iter::FusedIterator;
use packet::BufferView;
use zerocopy::Ref;

/// Maximum chained `NDP16` tables (`wNextNdpIndex`) traversed per NTB.
///
/// CDC NCM 1.0 leaves chain length unbounded; hosts normally emit one `NDP16` per NTB, so 64
/// bounds traversal length (including self-referential loops) without rejecting multi-NDP encoders.
pub const MAX_NDP_CHAIN_STEPS: usize = 64;

/// Zero-allocation iterator over datagram slices within a validated NTB-16 block, returned by
/// [`parse_ntb16`].
#[must_use = "iterators are lazy and do nothing unless consumed"]
#[derive(Clone, Debug)]
pub struct Ntb16Iter<'a> {
    block: &'a [u8],
    current_dpes: &'a [NcmDpe16],
    next_ndp_offset: usize,
    remaining: usize,
}

impl<'a> Iterator for Ntb16Iter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        loop {
            if let Some((dpe, rest)) = self.current_dpes.split_first() {
                self.current_dpes = rest;
                let dg_offset = usize::from(dpe.datagram_index.get());
                let dg_len = usize::from(dpe.datagram_length.get());
                if dg_offset != 0 && dg_len != 0 {
                    self.remaining -= 1;
                    return self.block.get(dg_offset..dg_offset + dg_len);
                }
                // CDC NCM 1.0 Section 3.7: any DPE16 with zero index or zero length terminates the
                // current `NDP16` table; ignore trailing entries and advance to `next_ndp_offset`.
                self.current_dpes = &[];
            }

            if self.next_ndp_offset == 0 {
                self.remaining = 0;
                return None;
            }

            let parsed = parse_ndp16(self.block, self.next_ndp_offset).ok()?;
            self.current_dpes = parsed.dpes;
            self.next_ndp_offset = parsed.next_ndp_offset;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Ntb16Iter<'_> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl FusedIterator for Ntb16Iter<'_> {}

/// Validated `NDP16` table slices and chain link extracted by [`parse_ndp16`].
struct ParsedNdp16<'a> {
    /// Raw `DPE16` entries inside this `NDP16` table (terminated by the first null entry).
    dpes: &'a [NcmDpe16],
    /// Byte offset of the next chained `NDP16` table within the NTB, or `0` if this is the last.
    next_ndp_offset: usize,
    /// Total byte length of this `NDP16` table (`wLength`, header + `dpes`).
    ndp_len: usize,
}

fn parse_ndp16(block: &[u8], ndp_offset: usize) -> Result<ParsedNdp16<'_>, NtbError> {
    let block_len = block.len();
    if ndp_offset < usize::from(NTH16_HEADER_LENGTH) || ndp_offset >= block_len {
        return Err(NtbError::NdpOffsetOutOfBounds { ndp_offset, block_len });
    }

    let mut ndp_tail =
        block.get(ndp_offset..).ok_or(NtbError::NdpOffsetOutOfBounds { ndp_offset, block_len })?;
    let mut ndp_view = &mut ndp_tail;
    let ndp_hdr = ndp_view
        .take_obj_front::<NcmNdp16Header>()
        .ok_or(NtbError::NdpOffsetOutOfBounds { ndp_offset, block_len })?;

    let ndp_sig = ndp_hdr.signature.get();
    if ndp_sig != NDP16_SIGNATURE_NO_CRC {
        return Err(NtbError::InvalidNdpSignature(ndp_sig));
    }

    let ndp_len = ndp_hdr.length.get();
    let ndp_hdr_len = const { NDP16_HEADER_LENGTH as u16 };
    let dpe_len = const { DPE16_LENGTH as u16 };
    if ndp_len < NDP16_MIN_LENGTH || !(ndp_len - ndp_hdr_len).is_multiple_of(dpe_len) {
        return Err(NtbError::InvalidNdpLength(ndp_len));
    }

    let dpe_count = usize::from((ndp_len - ndp_hdr_len) / dpe_len);
    let dpes = Ref::into_ref(
        ndp_view
            .take_slice_front::<NcmDpe16>(dpe_count)
            .ok_or(NtbError::NdpOffsetOutOfBounds { ndp_offset, block_len })?,
    );

    let next_ndp_offset = usize::from(ndp_hdr.next_ndp_index.get());
    Ok(ParsedNdp16 { dpes, next_ndp_offset, ndp_len: usize::from(ndp_len) })
}

/// Parses a 16-bit NTB (`NTH16` + chained `NDP16`s), returning an iterator over datagram slices
/// within `buf`.
///
/// Validates the entire `NDP16` chain and all `DPE16` bounds upfront without heap allocation so
/// malformed blocks are rejected atomically before any datagrams are yielded (CDC NCM 1.0
/// Section 3.2). Follows `ndp_index` and `next_ndp_index` anywhere in `[12..block_len)`,
/// supporting both `NDP-at-front` (Linux `cdc_ncm` default) and `NDP-at-end` hosts.
///
/// # Errors
///
/// Returns an [`NtbError`] if the `NTH16` header is truncated or has an invalid signature, header
/// length, or block length; if any `NDP16` in the chain is out of bounds, has an invalid signature
/// or length, or lacks a terminating null `DPE16`; if the `NDP16` chain exceeds
/// [`MAX_NDP_CHAIN_STEPS`]; if any `DPE16` datagram slice lies outside the NTB payload; or if
/// cumulative `NDP16` and datagram lengths exceed `block_len`.
pub fn parse_ntb16(buf: &[u8]) -> Result<Ntb16Iter<'_>, NtbError> {
    let mut header_slice = buf;
    let nth = (&mut header_slice).take_obj_front::<NcmNth16>().ok_or(NtbError::BufferTooShort)?;

    let nth_sig = nth.signature.get();
    if nth_sig != NTH16_SIGNATURE {
        return Err(NtbError::InvalidNthSignature(nth_sig));
    }

    let nth_hdr_len = nth.header_length.get();
    if nth_hdr_len != NTH16_HEADER_LENGTH {
        return Err(NtbError::InvalidNthHeaderLength(nth_hdr_len));
    }

    let raw_block_len = usize::from(nth.block_length.get());
    if raw_block_len > buf.len() {
        return Err(NtbError::BlockLengthOutOfBounds {
            block_len: raw_block_len,
            buf_len: buf.len(),
        });
    }
    let block_len = if raw_block_len == 0 { buf.len() } else { raw_block_len };
    if block_len < usize::from(NTH16_HEADER_LENGTH) {
        return Err(NtbError::BlockLengthOutOfBounds { block_len, buf_len: buf.len() });
    }
    let block = &buf[..block_len];

    let first_ndp_offset = usize::from(nth.ndp_index.get());
    let first_ndp = parse_ndp16(block, first_ndp_offset)?;

    let mut accounted_bytes = usize::from(NTH16_HEADER_LENGTH);
    let mut remaining = 0usize;
    let mut chain_steps = 0usize;
    let mut cur_ndp = ParsedNdp16 {
        dpes: first_ndp.dpes,
        next_ndp_offset: first_ndp.next_ndp_offset,
        ndp_len: first_ndp.ndp_len,
    };

    loop {
        chain_steps += 1;
        accounted_bytes =
            accounted_bytes.checked_add(cur_ndp.ndp_len).ok_or(NtbError::ArithmeticOverflow)?;
        if accounted_bytes > block_len {
            return Err(NtbError::TotalPayloadExceedsBlock { accounted_bytes, block_len });
        }

        let mut saw_null = false;
        for dpe in cur_ndp.dpes {
            let dg_offset = usize::from(dpe.datagram_index.get());
            let dg_len = usize::from(dpe.datagram_length.get());

            if dg_offset == 0 || dg_len == 0 {
                saw_null = true;
                break;
            }

            if dg_offset < usize::from(NTH16_HEADER_LENGTH) {
                return Err(NtbError::DatagramOutOfBounds { dg_offset, dg_len, block_len });
            }

            let dg_end = dg_offset.checked_add(dg_len).ok_or(NtbError::ArithmeticOverflow)?;
            if dg_end > block_len {
                return Err(NtbError::DatagramOutOfBounds { dg_offset, dg_len, block_len });
            }

            accounted_bytes =
                accounted_bytes.checked_add(dg_len).ok_or(NtbError::ArithmeticOverflow)?;
            if accounted_bytes > block_len {
                return Err(NtbError::TotalPayloadExceedsBlock { accounted_bytes, block_len });
            }

            remaining += 1;
        }
        if !saw_null {
            return Err(NtbError::MissingNdpNullTerminator);
        }

        if cur_ndp.next_ndp_offset == 0 {
            break;
        }
        if chain_steps >= MAX_NDP_CHAIN_STEPS {
            return Err(NtbError::NdpChainTooLong);
        }
        cur_ndp = parse_ndp16(block, cur_ndp.next_ndp_offset)?;
    }

    Ok(Ntb16Iter {
        block,
        current_dpes: first_ndp.dpes,
        next_ndp_offset: first_ndp.next_ndp_offset,
        remaining,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::NtbSequence;
    use crate::builder::tests::build_ntb16;
    use packet::{BufferViewMut, SliceBufViewMut};
    use std::vec;
    use std::vec::Vec;
    use zerocopy::byteorder::little_endian::{U16, U32};

    #[test]
    fn test_ntb16_ndp_at_front() {
        // Linux `cdc_ncm` places NDP16 immediately after NTH16 (aligned to 16) followed by frames.
        let f1 = [0xAAu8; 19];
        let f2 = [0xBBu8; 25];

        // Layout:
        // 0..12:   NTH16 (ndp_index = 16, block_length = 80)
        // 12..16:  pad
        // 16..36:  NDP16 (20B: header + DPE(36, 19) + DPE(55, 25) + DPE(0, 0))
        // 36..55:  f1 (19B)
        // 55..80:  f2 (25B, unaligned offset)
        let mut buf = [0u8; 80];
        let mut view = SliceBufViewMut::new(&mut buf);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(80),
            ndp_index: U16::new(16),
        })
        .unwrap();
        let _ = view.take_front_zero(4).unwrap();
        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(20),
            next_ndp_index: U16::ZERO,
        })
        .unwrap();
        view.write_obj_front(&[
            NcmDpe16 { datagram_index: U16::new(36), datagram_length: U16::new(19) },
            NcmDpe16 { datagram_index: U16::new(55), datagram_length: U16::new(25) },
            NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::ZERO },
        ])
        .unwrap();
        view.write_obj_front(&f1).unwrap();
        view.write_obj_front(&f2).unwrap();

        let iter = parse_ntb16(&buf).expect("parse NDP-at-front NTB-16");
        assert_eq!(iter.len(), 2);
        let parsed: Vec<_> = iter.collect();
        assert_eq!(parsed, vec![&f1[..], &f2[..]]);
    }

    #[test]
    fn test_ntb16_ndp_chain() {
        let f1 = [0xAAu8; 20];
        let f2 = [0xBBu8; 24];

        // Layout:
        // 0..12:   NTH16 (ndp_index = 56, block_length = 88)
        // 12..32:  f1 (20B)
        // 32..56:  f2 (24B)
        // 56..72:  NDP16 #1 (16B: header + DPE(12, 20) + DPE(0, 0), next_ndp_index = 72)
        // 72..88:  NDP16 #2 (16B: header + DPE(32, 24) + DPE(0, 0), next_ndp_index = 0)
        let mut buf = [0u8; 88];
        let mut view = SliceBufViewMut::new(&mut buf);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::new(1),
            block_length: U16::new(88),
            ndp_index: U16::new(56),
        })
        .unwrap();
        view.write_obj_front(&f1).unwrap();
        view.write_obj_front(&f2).unwrap();

        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(16),
            next_ndp_index: U16::new(72),
        })
        .unwrap();
        view.write_obj_front(&[
            NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(20) },
            NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::ZERO },
        ])
        .unwrap();

        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(16),
            next_ndp_index: U16::ZERO,
        })
        .unwrap();
        view.write_obj_front(&[
            NcmDpe16 { datagram_index: U16::new(32), datagram_length: U16::new(24) },
            NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::ZERO },
        ])
        .unwrap();

        let mut iter = parse_ntb16(&buf).expect("parse chained NTB-16");
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.next(), Some(&f1[..]));
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.next(), Some(&f2[..]));
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_ntb16_zero_datagrams() {
        let mut buf = [0u8; 32];
        let mut view = SliceBufViewMut::new(&mut buf);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(32),
            ndp_index: U16::new(16),
        })
        .unwrap();
        let _ = view.take_front_zero(4).unwrap();
        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(NDP16_MIN_LENGTH),
            next_ndp_index: U16::ZERO,
        })
        .unwrap();

        let mut iter = parse_ntb16(&buf).expect("parse empty NTB-16");
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_ntb16_malformed_signature() {
        let frame = [0x11u8; 64];
        let (mut ntb, _) =
            build_ntb16(&[&frame[..]], NtbSequence::default(), 16384).expect("build_ntb16");
        ntb[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());

        assert_eq!(parse_ntb16(&ntb).unwrap_err(), NtbError::InvalidNthSignature(0xDEAD_BEEF));
    }

    #[test]
    fn test_ntb16_malformed_ndp_signature() {
        let frame = [0x11u8; 64];
        let (mut ntb, _) =
            build_ntb16(&[&frame[..]], NtbSequence::default(), 16384).expect("build_ntb16");
        let mut nth_slice = &ntb[..];
        let nth = (&mut nth_slice).take_obj_front::<NcmNth16>().expect("read nth16");
        let ndp_idx = usize::from(nth.ndp_index.get());
        // Overwrite NDP16 signature with "NCM1" (0x314D_434E, CRC32 mode).
        ntb[ndp_idx..ndp_idx + 4].copy_from_slice(&0x314D_434Eu32.to_le_bytes());

        assert_eq!(parse_ntb16(&ntb).unwrap_err(), NtbError::InvalidNdpSignature(0x314D_434E));
    }

    #[test]
    fn test_ntb16_out_of_bounds_ndp_offset() {
        let mut buf = [0u8; 100];
        let mut view = SliceBufViewMut::new(&mut buf);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(100),
            ndp_index: U16::new(4096),
        })
        .unwrap();

        assert_eq!(
            parse_ntb16(&buf).unwrap_err(),
            NtbError::NdpOffsetOutOfBounds { ndp_offset: 4096, block_len: 100 }
        );

        // Valid `ndp_len` (16B) whose DPE slice extends past `block_len` (12 + 16 > 24).
        let mut truncated_ndp = [0u8; 24];
        let mut view = SliceBufViewMut::new(&mut truncated_ndp);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(24),
            ndp_index: U16::new(12),
        })
        .unwrap();
        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(NDP16_MIN_LENGTH),
            next_ndp_index: U16::ZERO,
        })
        .unwrap();

        assert_eq!(
            parse_ntb16(&truncated_ndp).unwrap_err(),
            NtbError::NdpOffsetOutOfBounds { ndp_offset: 12, block_len: 24 }
        );

        // `block_length` below `NTH16_HEADER_LENGTH` (12) is rejected as `BlockLengthOutOfBounds`.
        let mut short_block = [0u8; 32];
        let mut view = SliceBufViewMut::new(&mut short_block);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(8),
            ndp_index: U16::new(12),
        })
        .unwrap();

        assert_eq!(
            parse_ntb16(&short_block).unwrap_err(),
            NtbError::BlockLengthOutOfBounds { block_len: 8, buf_len: 32 }
        );
    }

    #[test]
    fn test_ntb16_out_of_bounds_datagram_rejects_whole_block() {
        // First DPE is valid (12..32), second DPE is out of bounds (80..130): `parse_ntb16` must
        // reject the entire block upfront rather than yielding a partial frame stream.
        let mut buf = [0u8; 100];
        let mut nth_view = SliceBufViewMut::new(&mut buf[..12]);
        nth_view
            .write_obj_front(&NcmNth16 {
                signature: U32::new(NTH16_SIGNATURE),
                header_length: U16::new(NTH16_HEADER_LENGTH),
                sequence: U16::ZERO,
                block_length: U16::new(100),
                ndp_index: U16::new(76),
            })
            .unwrap();

        let mut ndp_view = SliceBufViewMut::new(&mut buf[76..]);
        ndp_view
            .write_obj_front(&NcmNdp16Header {
                signature: U32::new(NDP16_SIGNATURE_NO_CRC),
                length: U16::new(20),
                next_ndp_index: U16::ZERO,
            })
            .unwrap();
        ndp_view
            .write_obj_front(&[
                NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(20) },
                NcmDpe16 { datagram_index: U16::new(80), datagram_length: U16::new(50) },
                NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::ZERO },
            ])
            .unwrap();

        assert_eq!(
            parse_ntb16(&buf).unwrap_err(),
            NtbError::DatagramOutOfBounds { dg_offset: 80, dg_len: 50, block_len: 100 }
        );
    }

    #[test]
    fn test_ntb16_ndp_chain_too_long() {
        // 65 non-overlapping 16-byte NDP16 tables fit within `block_len` (12 + 65 * 16 = 1052 B),
        // exercising the `MAX_NDP_CHAIN_STEPS` (64) limit rather than the byte budget.
        const CHAIN_LEN: usize = MAX_NDP_CHAIN_STEPS + 1;
        let block_len =
            usize::from(NTH16_HEADER_LENGTH) + CHAIN_LEN * usize::from(NDP16_MIN_LENGTH);
        let mut buf = vec![0u8; block_len];
        let mut view = SliceBufViewMut::new(&mut buf);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(u16::try_from(block_len).unwrap()),
            ndp_index: U16::new(NTH16_HEADER_LENGTH),
        })
        .unwrap();

        for i in 0..CHAIN_LEN {
            let next_offset = if i + 1 < CHAIN_LEN {
                u16::try_from(
                    usize::from(NTH16_HEADER_LENGTH) + (i + 1) * usize::from(NDP16_MIN_LENGTH),
                )
                .unwrap()
            } else {
                0
            };
            view.write_obj_front(&NcmNdp16Header {
                signature: U32::new(NDP16_SIGNATURE_NO_CRC),
                length: U16::new(NDP16_MIN_LENGTH),
                next_ndp_index: U16::new(next_offset),
            })
            .unwrap();
            let _ = view.take_front_zero(DPE16_LENGTH * 2).unwrap();
        }

        assert_eq!(parse_ntb16(&buf).unwrap_err(), NtbError::NdpChainTooLong);
    }

    #[test]
    fn test_ntb16_overlapping_dpe_and_cyclic_ndp_dos_rejected() {
        // Overlapping DPE16 entries pointing to the same [12..52) slice must be rejected once
        // cumulative accounted bytes exceed `block_len`.
        let mut buf = [0u8; 80];
        let mut view = SliceBufViewMut::new(&mut buf);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(80),
            ndp_index: U16::new(56),
        })
        .unwrap();
        let _ = view.take_front_zero(44).unwrap();
        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(24),
            next_ndp_index: U16::ZERO,
        })
        .unwrap();
        view.write_obj_front(&[
            NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(40) },
            NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(40) },
            NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(40) },
            NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::ZERO },
        ])
        .unwrap();

        assert_eq!(
            parse_ntb16(&buf).unwrap_err(),
            NtbError::TotalPayloadExceedsBlock { accounted_bytes: 116, block_len: 80 }
        );

        // Self-referential `next_ndp_index = 16` must also be bounded by `block_len` (64).
        let mut cyclic = [0u8; 64];
        let mut nth_view = SliceBufViewMut::new(&mut cyclic[..12]);
        nth_view
            .write_obj_front(&NcmNth16 {
                signature: U32::new(NTH16_SIGNATURE),
                header_length: U16::new(NTH16_HEADER_LENGTH),
                sequence: U16::ZERO,
                block_length: U16::new(64),
                ndp_index: U16::new(16),
            })
            .unwrap();
        let mut ndp_view = SliceBufViewMut::new(&mut cyclic[16..]);
        ndp_view
            .write_obj_front(&NcmNdp16Header {
                signature: U32::new(NDP16_SIGNATURE_NO_CRC),
                length: U16::new(NDP16_MIN_LENGTH),
                next_ndp_index: U16::new(16),
            })
            .unwrap();

        assert_eq!(
            parse_ntb16(&cyclic).unwrap_err(),
            NtbError::TotalPayloadExceedsBlock { accounted_bytes: 76, block_len: 64 }
        );
    }

    #[test]
    fn test_ntb16_null_dpe_variants_and_missing_terminator() {
        let frame = [0x55u8; 20];

        // CDC NCM 1.0 Section 3.7: `(0, non_zero)` and `(non_zero, 0)` are both Null DPEs that
        // terminate the NDP16 and ignore any subsequent entries.
        for null_dpe in [
            NcmDpe16 { datagram_index: U16::ZERO, datagram_length: U16::new(20) },
            NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::ZERO },
        ] {
            let mut buf = [0u8; 64];
            let mut view = SliceBufViewMut::new(&mut buf);
            view.write_obj_front(&NcmNth16 {
                signature: U32::new(NTH16_SIGNATURE),
                header_length: U16::new(NTH16_HEADER_LENGTH),
                sequence: U16::ZERO,
                block_length: U16::new(64),
                ndp_index: U16::new(32),
            })
            .unwrap();
            view.write_obj_front(&frame).unwrap();
            view.write_obj_front(&NcmNdp16Header {
                signature: U32::new(NDP16_SIGNATURE_NO_CRC),
                length: U16::new(20),
                next_ndp_index: U16::ZERO,
            })
            .unwrap();
            view.write_obj_front(&[
                NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(20) },
                null_dpe,
                // Trailing garbage after first Null DPE must be ignored.
                NcmDpe16 { datagram_index: U16::new(999), datagram_length: U16::new(999) },
            ])
            .unwrap();

            let parsed: Vec<_> = parse_ntb16(&buf).expect("parse null DPE variant").collect();
            assert_eq!(parsed, vec![&frame[..]]);
        }

        // Missing terminating Null DPE must fail with `MissingNdpNullTerminator`.
        let mut missing_null = [0u8; 64];
        let mut view = SliceBufViewMut::new(&mut missing_null);
        view.write_obj_front(&NcmNth16 {
            signature: U32::new(NTH16_SIGNATURE),
            header_length: U16::new(NTH16_HEADER_LENGTH),
            sequence: U16::ZERO,
            block_length: U16::new(64),
            ndp_index: U16::new(32),
        })
        .unwrap();
        view.write_obj_front(&frame).unwrap();
        view.write_obj_front(&NcmNdp16Header {
            signature: U32::new(NDP16_SIGNATURE_NO_CRC),
            length: U16::new(16),
            next_ndp_index: U16::ZERO,
        })
        .unwrap();
        view.write_obj_front(&[
            NcmDpe16 { datagram_index: U16::new(12), datagram_length: U16::new(10) },
            NcmDpe16 { datagram_index: U16::new(22), datagram_length: U16::new(10) },
        ])
        .unwrap();

        assert_eq!(parse_ntb16(&missing_null).unwrap_err(), NtbError::MissingNdpNullTerminator);
    }
}
