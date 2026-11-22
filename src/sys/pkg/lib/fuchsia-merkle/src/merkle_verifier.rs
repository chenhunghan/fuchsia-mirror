// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Hash;
use crate::util::{BoringSslSha256, hash_block_aligned};
use fuchsia_merkle_core::{AlignedByteSource, VerifyError};
use std::mem::{align_of, size_of};
use storage_ptr_slice::PtrByteSlice;
use zx_status::Status;

#[inline]
fn status_from_verify_error(err: VerifyError) -> Status {
    match err {
        VerifyError::InvalidArgs => Status::INVALID_ARGS,
        VerifyError::IoDataIntegrity => Status::IO_DATA_INTEGRITY,
    }
}

struct PtrByteSliceSource<'a>(PtrByteSlice<'a>);

impl AlignedByteSource for PtrByteSliceSource<'_> {
    #[inline]
    fn buffer_len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn hash_aligned_block(
        &self,
        offset: u64,
        byte_pos: usize,
        buffer_len: usize,
        unaligned_len: usize,
    ) -> fuchsia_merkle_core::Hash {
        let block = self.0.subslice(byte_pos..byte_pos + buffer_len);
        fuchsia_merkle_core::Hash(
            hash_block_aligned(offset, block, buffer_len, unaligned_len).into(),
        )
    }
}

/// Verifies data against the leaf hashes of a merkle tree.
#[derive(Clone)]
pub struct MerkleVerifier {
    inner: fuchsia_merkle_core::MerkleVerifier,
}

// FxBlob embeds ReadSizedMerkleVerifier and enforces a strict per-blob size budget.
const _: () = assert!(size_of::<MerkleVerifier>() == 2 * size_of::<usize>());
// Matching size and alignment let `Vec`'s in-place iterator specialization convert `Box<[Hash]>`
// into `Box<[fuchsia_merkle_core::Hash]>` without allocating a second buffer.
const _: () = assert!(size_of::<Hash>() == size_of::<fuchsia_merkle_core::Hash>());
const _: () = assert!(align_of::<Hash>() == align_of::<fuchsia_merkle_core::Hash>());

impl MerkleVerifier {
    /// Constructs a [`MerkleVerifier`] from the root and leaf hashes of a merkle tree.
    ///
    /// Returns `INVALID_ARGS` if the leaf hash count would overflow a 64-bit byte offset, or
    /// `IO_DATA_INTEGRITY` if the leaf hashes are empty or inconsistent with the root.
    #[allow(clippy::boxed_local)]
    pub fn new(root: Hash, hashes: Box<[Hash]>) -> Result<Self, Status> {
        // Because `Hash` and `fuchsia_merkle_core::Hash` both have size 32, alignment 1, and no
        // `Drop` glue, `Box::into_vec` -> `into_iter().map().collect::<Vec<_>>()` ->
        // `into_boxed_slice()` reuses `hashes`'s heap allocation in place without reallocating (and
        // folds to a no-op in optimized builds) while avoiding `unsafe`.
        let core_hashes: Box<[fuchsia_merkle_core::Hash]> = hashes
            .into_vec()
            .into_iter()
            .map(|h| fuchsia_merkle_core::Hash(h.into()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        fuchsia_merkle_core::MerkleVerifier::new::<BoringSslSha256>(
            fuchsia_merkle_core::Hash(root.into()),
            core_hashes,
        )
        .map(|inner| Self { inner })
        .map_err(status_from_verify_error)
    }

    /// Verifies a `data` slice against the Merkle tree, assuming it corresponds to original data
    /// starting at `offset`.
    ///
    /// # Requirements:
    /// - The `offset` must be aligned to [`crate::BLOCK_SIZE`].
    /// - The length of `data` must be a multiple of [`crate::BLOCK_SIZE`], *except* if `data`
    ///   contains the final chunk of the original data source.
    #[inline]
    pub fn verify(&self, offset: usize, data: &[u8]) -> Result<(), Status> {
        self.inner.verify::<BoringSslSha256>(offset, data).map_err(status_from_verify_error)
    }

    /// Verifies aligned, zero-padded data against the Merkle tree.
    ///
    /// The buffer length (`data.len()`) must be a multiple of 4096 bytes and must not extend into
    /// blocks beyond `unaligned_len` (`data.len() <= unaligned_len.next_multiple_of(BLOCK_SIZE)`).
    /// `unaligned_len` specifies the actual valid data length within the buffer
    /// (`0 < unaligned_len <= data.len()`).
    /// Any buffer bytes beyond `unaligned_len` up to `data.len()` MUST be zeroed for verification
    /// to pass.
    ///
    /// Note: Does not support the null blob (empty 0-length blob, `unaligned_len == 0`).
    #[inline]
    pub fn verify_aligned(
        &self,
        offset: usize,
        data: PtrByteSlice<'_>,
        unaligned_len: usize,
    ) -> Result<(), Status> {
        self.inner
            .verify_aligned(offset, &PtrByteSliceSource(data), unaligned_len)
            .map_err(status_from_verify_error)
    }
}

impl std::fmt::Debug for MerkleVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The leaf hashes are unlikely to be useful for debugging and could be megabytes of data.
        // The root of the merkle tree uniquely identifies the blob and the hash count can quickly
        // give a rough idea of the size of the blob.
        let root = Hash::from_array(
            fuchsia_merkle_core::rebuild_root::<BoringSslSha256>(self.inner.hashes()).0,
        );
        f.debug_struct("MerkleVerifier")
            .field("root", &root)
            .field("hash_count", &self.inner.hash_count())
            .finish()
    }
}

/// Verifies reads against a merkle tree.
///
/// [`MerkleVerifier`] verifies data at a granularity of [`crate::BLOCK_SIZE`]. Consequently, it
/// stores one hash (of size [`crate::HASH_SIZE`]) for each data block. This hash storage consumes
/// memory equal to 1/256th of the original data size.
///
/// [`ReadSizedMerkleVerifier`] optimizes memory usage when reads are always aligned and are
/// guaranteed to be a multiple of the [`crate::BLOCK_SIZE`]. Instead of storing a hash for every
/// [`crate::BLOCK_SIZE`] blocks like [`MerkleVerifier`], it stores only one hash for each read
/// sized chunk. For example, 128KiB aligned reads would require storing 1/16th the number of
/// hashes.
#[derive(Clone)]
pub struct ReadSizedMerkleVerifier {
    inner: fuchsia_merkle_core::ReadSizedMerkleVerifier,
}

const _: () = assert!(size_of::<ReadSizedMerkleVerifier>() == 4 * size_of::<usize>());

impl ReadSizedMerkleVerifier {
    /// Constructs a [`ReadSizedMerkleVerifier`] from an existing [`MerkleVerifier`] and a
    /// `read_size`.
    ///
    /// Returns an error if `read_size` is zero, not a multiple of [`crate::BLOCK_SIZE`], or so
    /// large that its level-1 hash block size exceeds `u32::MAX`.
    #[inline]
    pub fn new(verifier: MerkleVerifier, read_size: usize) -> Result<Self, Status> {
        fuchsia_merkle_core::ReadSizedMerkleVerifier::new::<BoringSslSha256>(
            verifier.inner,
            read_size,
        )
        .map(|inner| Self { inner })
        .map_err(status_from_verify_error)
    }

    /// Verifies a `data` slice against the Merkle tree, assuming it corresponds to original data
    /// starting at `offset`.
    ///
    /// # Requirements:
    /// - The `offset` must be aligned to the configured read size granularity.
    /// - The length of `data` must be a multiple of the read size, *except* if `data` represents
    ///   the final chunk of the original data source (in which case it can be shorter).
    #[inline]
    pub fn verify(&self, offset: usize, data: &[u8]) -> Result<(), Status> {
        self.inner.verify::<BoringSslSha256>(offset, data).map_err(status_from_verify_error)
    }

    /// Verifies aligned, zero-padded data against the Merkle tree.
    ///
    /// The buffer length (`data.len()`) must be a multiple of 4096 bytes and must not extend into
    /// blocks beyond `unaligned_len` (`data.len() <= unaligned_len.next_multiple_of(BLOCK_SIZE)`).
    /// `unaligned_len` specifies the actual valid data length within the buffer
    /// (`0 < unaligned_len <= data.len()`).
    /// Any buffer bytes beyond `unaligned_len` up to `data.len()` MUST be zeroed for verification
    /// to pass.
    ///
    /// Note: Does not support the null blob (empty 0-length blob, `unaligned_len == 0`).
    #[inline]
    pub fn verify_aligned(
        &self,
        offset: usize,
        data: PtrByteSlice<'_>,
        unaligned_len: usize,
    ) -> Result<(), Status> {
        self.inner
            .verify_aligned::<BoringSslSha256, _>(offset, &PtrByteSliceSource(data), unaligned_len)
            .map_err(status_from_verify_error)
    }
}

impl std::fmt::Debug for ReadSizedMerkleVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The leaf hashes are unlikely to be useful for debugging and could be megabytes of data.
        // The root of the merkle tree can't be recovered either. The read size and hash count can
        // give a rough idea of the size of the blob.
        f.debug_struct("ReadAlignedMerkleVerifier")
            .field("read_size", &self.inner.read_size())
            .field("hash_count", &self.inner.hash_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::hash_block;
    use crate::{BLOCK_SIZE, HASH_SIZE, MerkleRootBuilder};
    use anyhow::{Context, Error, anyhow};
    use assert_matches::assert_matches;
    use test_case::test_case;

    fn rebuild_root(leaf_hashes: &[Hash]) -> Hash {
        let core_hashes: Vec<fuchsia_merkle_core::Hash> =
            leaf_hashes.iter().map(|h| fuchsia_merkle_core::Hash((*h).into())).collect();
        Hash::from_array(fuchsia_merkle_core::rebuild_root::<BoringSslSha256>(&core_hashes).0)
    }

    fn create_data(size: usize) -> Vec<u8> {
        const USIZE_SIZE: usize = size_of::<usize>();
        let mut data = vec![0xABu8; size];
        for (i, block) in data.chunks_mut(BLOCK_SIZE).enumerate() {
            // Place the index of the block at the start of the block to make every block unique.
            if block.len() < USIZE_SIZE {
                block.copy_from_slice(&i.to_le_bytes()[0..block.len()]);
            } else {
                block[0..USIZE_SIZE].copy_from_slice(&i.to_le_bytes());
            }
        }
        data
    }

    #[test_case(0; "0")]
    #[test_case(1; "1")]
    #[test_case(4096; "4096")]
    #[test_case(8191; "8191")]
    #[test_case(8192; "8192")]
    #[test_case(8193; "8193")]
    #[test_case(8192 * 2 - 1; "16383")]
    #[test_case(8192 * 2; "16384")]
    #[test_case(8192 * 2 + 1; "16385")]
    #[test_case(8192 * 256 - 1; "2097151")]
    #[test_case(8192 * 256; "2097152")]
    #[test_case(8192 * 256 + 1; "2097153")]
    #[test_case(8192 * 256 * 2 - 1; "4194303")]
    #[test_case(8192 * 256 * 2; "4194304")]
    #[test_case(8192 * 256 * 2 + 1; "4194305")]
    fn test_successfully_validate_root(size: usize) {
        let data = create_data(size);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);

        MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
    }

    #[test_case(8193; "8193")]
    #[test_case(8192 * 2 - 1; "16383")]
    #[test_case(8192 * 2; "16384")]
    #[test_case(8192 * 2 + 1; "16385")]
    #[test_case(8192 * 256 - 1; "2097151")]
    #[test_case(8192 * 256; "2097152")]
    #[test_case(8192 * 256 + 1; "2097153")]
    #[test_case(8192 * 256 * 2 - 1; "4194303")]
    #[test_case(8192 * 256 * 2; "4194304")]
    #[test_case(8192 * 256 * 2 + 1; "4194305")]
    fn test_fail_to_validate_root(size: usize) {
        let data = create_data(size);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);

        {
            let mut leaf_hashes = leaf_hashes.clone().into_boxed_slice();
            let mut first_hash: [u8; HASH_SIZE] = leaf_hashes[0].into();
            first_hash[0] ^= 0xFF;
            leaf_hashes[0] = Hash::from_array(first_hash);
            MerkleVerifier::new(root, leaf_hashes).expect_err("The merkle root shouldn't match");
        }

        {
            let mut leaf_hashes = leaf_hashes.into_boxed_slice();
            let mut last_hash: [u8; HASH_SIZE] = (*leaf_hashes.last().unwrap()).into();
            last_hash[31] ^= 0xFF;
            *leaf_hashes.last_mut().unwrap() = Hash::from_array(last_hash);
            MerkleVerifier::new(root, leaf_hashes).expect_err("The merkle root shouldn't match");
        }
    }

    #[test]
    fn test_verify_empty_data() {
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&[]);
        assert_matches!(MerkleVerifier::new(root, Box::new([])), Err(Status::IO_DATA_INTEGRITY));
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
        verifier.verify(0, &[]).unwrap();
        assert_matches!(verifier.verify(1, &[]), Err(Status::INVALID_ARGS));
        assert_matches!(verifier.verify(BLOCK_SIZE, &[]), Err(Status::INVALID_ARGS));
        assert_matches!(verifier.verify(0, &[0x00]), Err(Status::IO_DATA_INTEGRITY));
        assert_matches!(verifier.verify(0, &[0x01]), Err(Status::IO_DATA_INTEGRITY));

        let read_verifier = ReadSizedMerkleVerifier::new(verifier, 16 * 1024).unwrap();
        read_verifier.verify(0, &[]).unwrap();
        assert_matches!(read_verifier.verify(16 * 1024, &[]), Err(Status::INVALID_ARGS));
        assert_matches!(read_verifier.verify(0, &[0x00]), Err(Status::IO_DATA_INTEGRITY));
    }

    #[test]
    fn test_reject_empty_slice_on_non_empty_blobs() {
        for size in [1, BLOCK_SIZE, 16 * 1024, 16 * 1024 + 20] {
            let data = create_data(size);
            let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
            let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
            let read_verifier = ReadSizedMerkleVerifier::new(verifier.clone(), 16 * 1024).unwrap();

            let expected_zero_offset_status =
                if size <= BLOCK_SIZE { Status::IO_DATA_INTEGRITY } else { Status::INVALID_ARGS };
            assert_eq!(verifier.verify(0, &[]), Err(expected_zero_offset_status), "size={size}");
            assert_matches!(verifier.verify(BLOCK_SIZE, &[]), Err(Status::INVALID_ARGS));

            let expected_read_zero_offset_status =
                if size <= 16 * 1024 { Status::IO_DATA_INTEGRITY } else { Status::INVALID_ARGS };
            assert_eq!(
                read_verifier.verify(0, &[]),
                Err(expected_read_zero_offset_status),
                "read_verifier size={size}"
            );
            assert_matches!(read_verifier.verify(16 * 1024, &[]), Err(Status::INVALID_ARGS));
        }
    }

    #[test]
    fn test_verify_with_invalid_args() {
        let data = create_data(16 * 1024 + 20);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.clone().into_boxed_slice()).unwrap();
        // Offset isn't aligned.
        assert_matches!(verifier.verify(1, &data[1..]), Err(Status::INVALID_ARGS));
        // Non-final unaligned slice lengths must be rejected with INVALID_ARGS, even if the leaf
        // hash is forged from the short slice.
        assert_matches!(verifier.verify(0, &data[0..100]), Err(Status::INVALID_ARGS));
        assert_matches!(verifier.verify(0, &data[0..BLOCK_SIZE + 1]), Err(Status::INVALID_ARGS));
        assert_matches!(
            verifier.verify(BLOCK_SIZE, &data[BLOCK_SIZE..BLOCK_SIZE + 100]),
            Err(Status::INVALID_ARGS)
        );
        let mut forged_hashes = leaf_hashes;
        forged_hashes[0] = hash_block(&data[0..100], 0);
        let forged_root = rebuild_root(&forged_hashes);
        let forged_verifier =
            MerkleVerifier::new(forged_root, forged_hashes.into_boxed_slice()).unwrap();
        assert_matches!(
            forged_verifier.verify(BLOCK_SIZE, &data[BLOCK_SIZE..BLOCK_SIZE * 2]),
            Ok(())
        );
        assert_matches!(forged_verifier.verify(0, &data[0..100]), Err(Status::INVALID_ARGS));
        // Too much data.
        assert_matches!(verifier.verify(8192, &data), Err(Status::INVALID_ARGS));
        // Still too much data but it's within the same block as the original data it's detected
        // with the hashes.
        assert_matches!(verifier.verify(8192, &data[8191..]), Err(Status::IO_DATA_INTEGRITY));
        assert_matches!(verifier.verify(8192, &data[8192..]), Ok(()));
    }

    #[test]
    fn test_invalid_read_sizes() {
        let data = create_data(16 * 1024 + 20);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier.clone(), 0),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier.clone(), 1),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier.clone(), 8191),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier, 100 * 1024),
            Err(Status::INVALID_ARGS)
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn test_read_size_overflowing_u32_identity_length_rejected() {
        const READ_SIZE: usize = BLOCK_SIZE * 2;
        const MAX_BLOCK_ALIGNED: usize = (usize::MAX / BLOCK_SIZE) * BLOCK_SIZE;
        const MAX_READ_ALIGNED: usize = (usize::MAX / READ_SIZE) * READ_SIZE;
        let data = create_data(100);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();

        // At `read_size = 1 TiB` (`1 << 40`), `hashes_per_hash = 1 << 27` and
        // `bytes_per_hash = 1 << 32`, which previously wrapped to `0u32` inside `make_identity`.
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier.clone(), 1usize << 40),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier.clone(), (1usize << 40) + BLOCK_SIZE),
            Err(Status::INVALID_ARGS)
        );
        // Near `usize::MAX`, `hashes_per_hash * HASH_SIZE` overflows `usize`.
        assert_matches!(
            ReadSizedMerkleVerifier::new(verifier.clone(), MAX_BLOCK_ALIGNED),
            Err(Status::INVALID_ARGS)
        );

        let read_verifier = ReadSizedMerkleVerifier::new(verifier.clone(), READ_SIZE).unwrap();
        let block_buf = [0u8; BLOCK_SIZE];
        let read_buf = [0u8; READ_SIZE];
        // `MAX_BLOCK_ALIGNED` is `usize::MAX - (BLOCK_SIZE - 1)`, so adding `BLOCK_SIZE` overflows
        // `usize` in `checked_add`.
        assert_matches!(verifier.verify(MAX_BLOCK_ALIGNED, &block_buf), Err(Status::INVALID_ARGS));
        assert_matches!(verifier.verify(usize::MAX, &[0xab]), Err(Status::INVALID_ARGS));
        assert_matches!(
            verifier.verify_aligned(MAX_BLOCK_ALIGNED, PtrByteSlice::from(&block_buf[..]), 100),
            Err(Status::INVALID_ARGS)
        );
        // `MAX_READ_ALIGNED` is `usize::MAX - (READ_SIZE - 1)`, so adding `READ_SIZE` overflows
        // `usize` in `checked_add`.
        assert_matches!(
            read_verifier.verify(MAX_READ_ALIGNED, &read_buf),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(read_verifier.verify(usize::MAX, &[0xab]), Err(Status::INVALID_ARGS));
        assert_matches!(
            read_verifier.verify_aligned(
                MAX_READ_ALIGNED,
                PtrByteSlice::from(&read_buf[..]),
                BLOCK_SIZE + 100
            ),
            Err(Status::INVALID_ARGS)
        );
    }

    #[test]
    fn test_verify_reads_with_multiple_reads() {
        const READ_SIZE: usize = 16 * 1024;
        let data = create_data(READ_SIZE * 3 + 20);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
        let verifier = ReadSizedMerkleVerifier::new(verifier, READ_SIZE).unwrap();
        verifier.verify(0, &data).unwrap();
        verifier.verify(0, &data[0..READ_SIZE * 2]).unwrap();
        verifier.verify(READ_SIZE, &data[READ_SIZE..READ_SIZE * 3]).unwrap();
        verifier.verify(READ_SIZE, &data[READ_SIZE..READ_SIZE * 3 + 20]).unwrap();
        verifier.verify(READ_SIZE * 2, &data[READ_SIZE * 2..READ_SIZE * 3 + 20]).unwrap();
    }

    #[test]
    fn test_verify_reads_with_invalid_args() {
        const READ_SIZE: usize = 16 * 1024;
        let data = create_data(READ_SIZE * 3 + 20);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
        let verifier = ReadSizedMerkleVerifier::new(verifier, READ_SIZE).unwrap();
        // Offset isn't aligned.
        assert_matches!(verifier.verify(1, &data[1..READ_SIZE + 1]), Err(Status::INVALID_ARGS));
        // Read past the end.
        assert_matches!(
            verifier.verify(READ_SIZE * 4, &data[0..READ_SIZE]),
            Err(Status::INVALID_ARGS)
        );
        // Not the end of the data and the data is not a multiple of the read size.
        assert_matches!(verifier.verify(0, &data[0..READ_SIZE - 10]), Err(Status::INVALID_ARGS));
        // At end of the data and it's the wrong amount of data but it's within the last block so
        // it's detected by the hashes.
        assert_matches!(
            verifier.verify(READ_SIZE * 3, &data[READ_SIZE * 3..READ_SIZE * 3 + 19]),
            Err(Status::IO_DATA_INTEGRITY)
        );
        let mut last_block_with_an_extra_byte = data[READ_SIZE * 3..READ_SIZE * 3 + 20].to_vec();
        last_block_with_an_extra_byte.push(0xAB);
        assert_matches!(
            verifier.verify(READ_SIZE * 3, &last_block_with_an_extra_byte),
            Err(Status::IO_DATA_INTEGRITY)
        );
        assert_matches!(
            verifier.verify(READ_SIZE * 3, &data[READ_SIZE * 3..READ_SIZE * 3 + 20]),
            Ok(())
        );
    }

    fn verify_with_first_bit_flipped(
        verifier: &MerkleVerifier,
        offset: usize,
        data: &mut [u8],
    ) -> Result<(), Error> {
        data[0] ^= 0x01;
        match verifier.verify(offset, data) {
            Ok(()) => Err(anyhow!("verify_with_first_bit_flipped should have failed")),
            Err(Status::IO_DATA_INTEGRITY) => {
                data[0] ^= 0x01;
                Ok(())
            }
            Err(e) => Err(anyhow!("unexpected error in verify_with_first_bit_flipped: {e:?}")),
        }
    }

    fn verify_with_last_bit_flipped(
        verifier: &MerkleVerifier,
        offset: usize,
        data: &mut [u8],
    ) -> Result<(), Error> {
        *data.last_mut().unwrap() ^= 0x80;
        match verifier.verify(offset, data) {
            Ok(()) => Err(anyhow!("verify_with_last_bit_flipped should have failed")),
            Err(Status::IO_DATA_INTEGRITY) => {
                *data.last_mut().unwrap() ^= 0x80;
                Ok(())
            }
            Err(e) => Err(anyhow!("unexpected error in verify_with_last_bit_flipped: {e:?}")),
        }
    }

    fn run_verify_tests(data: &mut [u8], verifier: MerkleVerifier) -> Result<(), Error> {
        // Verify all of the data at once.
        verifier.verify(0, data).context("verify all data")?;
        verify_with_first_bit_flipped(&verifier, 0, data).context("verify all data")?;
        verify_with_last_bit_flipped(&verifier, 0, data).context("verify all data")?;

        // Verify 1 block at a time.
        for (i, block) in data.chunks_mut(BLOCK_SIZE).enumerate() {
            let offset = i * BLOCK_SIZE;
            let context = || format!("verify 1 block at a time: offset={offset}");
            verifier.verify(offset, block).with_context(context)?;
            verify_with_first_bit_flipped(&verifier, offset, block).with_context(context)?;
            verify_with_last_bit_flipped(&verifier, offset, block).with_context(context)?;
        }

        // Verify 4 blocks at a time.
        const BLOCK_COUNT: usize = 4;
        for (i, block) in data.chunks_mut(BLOCK_SIZE * BLOCK_COUNT).enumerate() {
            let offset = i * BLOCK_SIZE * BLOCK_COUNT;
            let context = || format!("verify {BLOCK_COUNT} blocks at a time: offset={offset}");
            verifier.verify(offset, block).with_context(context)?;
            verify_with_first_bit_flipped(&verifier, offset, block).with_context(context)?;
            verify_with_last_bit_flipped(&verifier, offset, block).with_context(context)?;
        }
        Ok(())
    }

    fn verify_reads_with_first_bit_flipped(
        verifier: &ReadSizedMerkleVerifier,
        offset: usize,
        data: &mut [u8],
    ) -> Result<(), Error> {
        data[0] ^= 0x01;
        match verifier.verify(offset, data) {
            Ok(()) => Err(anyhow!("verify_reads_with_first_bit_flipped should have failed")),
            Err(Status::IO_DATA_INTEGRITY) => {
                data[0] ^= 0x01;
                Ok(())
            }
            Err(e) => {
                Err(anyhow!("unexpected error in verify_reads_with_first_bit_flipped: {e:?}"))
            }
        }
    }

    fn verify_reads_with_last_bit_flipped(
        verifier: &ReadSizedMerkleVerifier,
        offset: usize,
        data: &mut [u8],
    ) -> Result<(), Error> {
        *data.last_mut().unwrap() ^= 0x80;
        match verifier.verify(offset, data) {
            Ok(()) => Err(anyhow!("verify_reads_with_last_bit_flipped should have failed")),
            Err(Status::IO_DATA_INTEGRITY) => {
                *data.last_mut().unwrap() ^= 0x80;
                Ok(())
            }
            Err(e) => Err(anyhow!("unexpected error in verify_reads_with_last_bit_flipped: {e:?}")),
        }
    }

    fn run_read_sized_verify_tests(
        data: &mut [u8],
        verifier: MerkleVerifier,
        read_size: usize,
    ) -> Result<(), Error> {
        let verifier = ReadSizedMerkleVerifier::new(verifier, read_size)
            .context("Failed to create read aligned verifier")?;

        for (i, chunk) in data.chunks_mut(read_size).enumerate() {
            let offset = i * read_size;
            let context = || format!("verify read: offset={offset}");
            verifier.verify(offset, chunk).with_context(context)?;
            verify_reads_with_first_bit_flipped(&verifier, offset, chunk).with_context(context)?;
            verify_reads_with_last_bit_flipped(&verifier, offset, chunk).with_context(context)?;
        }
        Ok(())
    }

    fn verify_test(data: &mut [u8]) -> Result<(), Error> {
        let hashes = Vec::with_capacity(data.len().div_ceil(BLOCK_SIZE));
        let (root, hashes) = MerkleRootBuilder::new(hashes).complete(data);
        let verifier = MerkleVerifier::new(root, hashes.into_boxed_slice())
            .with_context(|| format!("create verifier: data-size={}", data.len()))?;
        run_verify_tests(data, verifier.clone())
            .with_context(|| format!("verify data-size={}", data.len()))?;
        for read_size in [8 * 1024, 32 * 1024, 96 * 1024, 128 * 1024, 216 * 1024] {
            run_read_sized_verify_tests(data, verifier.clone(), read_size).with_context(|| {
                format!("verify read read-size={} data-size={}", read_size, data.len())
            })?;
        }
        Ok(())
    }

    #[test_case(1; "1")]
    #[test_case(4096; "4096")]
    #[test_case(8192 - 1; "8191")]
    #[test_case(8192; "8192")]
    #[test_case(8192 + 1; "8193")]
    #[test_case(8192 * 2 - 1; "16383")]
    #[test_case(8192 * 2; "16384")]
    #[test_case(8192 * 2 + 1; "16385")]
    #[test_case(8192 * 256 - 1; "2097151")]
    #[test_case(8192 * 256; "2097152")]
    #[test_case(8192 * 256 + 1; "2097153")]
    fn test_verification(size: usize) {
        verify_test(&mut create_data(size)).unwrap();
    }

    #[test]
    fn test_verify_aligned() {
        for size in [1, 100, 4096, 4097, 8192, 16384, 65536, 131072] {
            let data = create_data(size);
            let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
            let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
            let read_sized_verifier =
                ReadSizedMerkleVerifier::new(verifier.clone(), 128 * 1024).unwrap();

            let buffer_len = size.next_multiple_of(4096);
            let mut buf = vec![0u8; buffer_len];
            buf[..size].copy_from_slice(&data);

            // Test MerkleVerifier::verify_aligned
            verifier
                .verify_aligned(0, PtrByteSlice::from(&buf[..]), size)
                .expect("verify_aligned failed");

            // Test ReadSizedMerkleVerifier::verify_aligned
            read_sized_verifier
                .verify_aligned(0, PtrByteSlice::from(&buf[..]), size)
                .expect("read_sized_verifier.verify_aligned failed");

            // Test sub-chunk verification with non-zero offsets.
            for chunk_offset in (0..size).step_by(8192) {
                let chunk_unaligned_len = std::cmp::min(8192, size - chunk_offset);
                let chunk_buffer_len = chunk_unaligned_len.next_multiple_of(4096);
                let chunk_slice = &buf[chunk_offset..chunk_offset + chunk_buffer_len];
                verifier
                    .verify_aligned(
                        chunk_offset,
                        PtrByteSlice::from(chunk_slice),
                        chunk_unaligned_len,
                    )
                    .expect("verify_aligned failed for non-zero offset sub-chunk");
            }

            // Test corruption detection
            let mut corrupt_buf = buf.clone();
            corrupt_buf[0] ^= 0x01;
            assert_matches!(
                verifier.verify_aligned(0, PtrByteSlice::from(&corrupt_buf[..]), size),
                Err(Status::IO_DATA_INTEGRITY)
            );
            assert_matches!(
                read_sized_verifier.verify_aligned(0, PtrByteSlice::from(&corrupt_buf[..]), size),
                Err(Status::IO_DATA_INTEGRITY)
            );

            // Test corruption detection at non-zero offset
            if size > 8192 {
                let mut corrupt_nonzero_buf = buf.clone();
                corrupt_nonzero_buf[8192] ^= 0x01;
                let chunk_unaligned_len = std::cmp::min(8192, size - 8192);
                let chunk_buffer_len = chunk_unaligned_len.next_multiple_of(4096);
                assert_matches!(
                    verifier.verify_aligned(
                        8192,
                        PtrByteSlice::from(&corrupt_nonzero_buf[8192..8192 + chunk_buffer_len]),
                        chunk_unaligned_len
                    ),
                    Err(Status::IO_DATA_INTEGRITY)
                );
            }

            // Test unzeroed tail padding detection (if size < buffer_len)
            if size < buffer_len {
                let mut unzeroed_tail_buf = buf.clone();
                unzeroed_tail_buf[size] = 0xAA;
                assert_matches!(
                    verifier.verify_aligned(0, PtrByteSlice::from(&unzeroed_tail_buf[..]), size),
                    Err(Status::IO_DATA_INTEGRITY)
                );
                assert_matches!(
                    read_sized_verifier.verify_aligned(
                        0,
                        PtrByteSlice::from(&unzeroed_tail_buf[..]),
                        size
                    ),
                    Err(Status::IO_DATA_INTEGRITY)
                );
            }
        }

        // Test non-4096-aligned buffer length returns INVALID_ARGS
        let data = create_data(100);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
        let unaligned_buf = [0u8; 100];
        assert_matches!(
            verifier.verify_aligned(0, PtrByteSlice::from(&unaligned_buf[..]), 100),
            Err(Status::INVALID_ARGS)
        );
    }

    #[test]
    fn test_verify_aligned_block_bounds_and_non_final_alignment() {
        const READ_SIZE: usize = 16 * 1024;
        for page_size in [4096, 16 * 1024, 64 * 1024] {
            for size in [1, 100, 4096, 8192, 8193, 20000] {
                let data = create_data(size);
                let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
                let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
                let read_sized_verifier =
                    ReadSizedMerkleVerifier::new(verifier.clone(), READ_SIZE).unwrap();

                let buffer_len = size.next_multiple_of(page_size);
                let mut buf = vec![0u8; buffer_len];
                buf[..size].copy_from_slice(&data);

                let verify_len = std::cmp::min(buffer_len, size.next_multiple_of(BLOCK_SIZE));
                assert_matches!(
                    verifier.verify_aligned(0, PtrByteSlice::from(&buf[..verify_len]), size),
                    Ok(())
                );
                assert_matches!(
                    read_sized_verifier.verify_aligned(
                        0,
                        PtrByteSlice::from(&buf[..verify_len]),
                        size
                    ),
                    Ok(())
                );

                if size < verify_len {
                    for dirty_idx in [size, verify_len - 1] {
                        let mut dirty_buf = buf.clone();
                        dirty_buf[dirty_idx] = 0x5A;
                        assert_matches!(
                            verifier.verify_aligned(
                                0,
                                PtrByteSlice::from(&dirty_buf[..verify_len]),
                                size
                            ),
                            Err(Status::IO_DATA_INTEGRITY)
                        );
                        assert_matches!(
                            read_sized_verifier.verify_aligned(
                                0,
                                PtrByteSlice::from(&dirty_buf[..verify_len]),
                                size
                            ),
                            Err(Status::IO_DATA_INTEGRITY)
                        );
                    }
                }

                // Buffers extending into blocks beyond `size.next_multiple_of(BLOCK_SIZE)` must be
                // rejected with `INVALID_ARGS` rather than panicking in `hash_block_aligned`.
                if buffer_len > verify_len {
                    assert_matches!(
                        verifier.verify_aligned(0, PtrByteSlice::from(&buf[..]), size),
                        Err(Status::INVALID_ARGS)
                    );
                    assert_matches!(
                        read_sized_verifier.verify_aligned(0, PtrByteSlice::from(&buf[..]), size),
                        Err(Status::INVALID_ARGS)
                    );
                }
            }
        }

        // Non-zero-offset tail reads must accept buffers clamped to `tail_len.next_multiple_of(BLOCK_SIZE)`
        // and reject buffers extending into extra blocks.
        let data = create_data(20_000);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.into_boxed_slice()).unwrap();
        let read_sized_verifier =
            ReadSizedMerkleVerifier::new(verifier.clone(), READ_SIZE).unwrap();
        let tail_len = data.len() - READ_SIZE;
        let tail_verify_len = tail_len.next_multiple_of(BLOCK_SIZE);
        let mut tail_buf_64k = vec![0u8; 64 * 1024];
        tail_buf_64k[..tail_len].copy_from_slice(&data[READ_SIZE..]);
        assert_matches!(
            verifier.verify_aligned(
                READ_SIZE,
                PtrByteSlice::from(&tail_buf_64k[..tail_verify_len]),
                tail_len
            ),
            Ok(())
        );
        assert_matches!(
            read_sized_verifier.verify_aligned(
                READ_SIZE,
                PtrByteSlice::from(&tail_buf_64k[..tail_verify_len]),
                tail_len
            ),
            Ok(())
        );
        assert_matches!(
            verifier.verify_aligned(READ_SIZE, PtrByteSlice::from(&tail_buf_64k[..]), tail_len),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            read_sized_verifier.verify_aligned(
                READ_SIZE,
                PtrByteSlice::from(&tail_buf_64k[..]),
                tail_len
            ),
            Err(Status::INVALID_ARGS)
        );

        // Non-final unaligned `unaligned_len` must return INVALID_ARGS, even if leaf 0 is forged.
        let data = create_data(READ_SIZE + 20);
        let (root, leaf_hashes) = MerkleRootBuilder::new(Vec::new()).complete(&data);
        let verifier = MerkleVerifier::new(root, leaf_hashes.clone().into_boxed_slice()).unwrap();
        let read_sized_verifier =
            ReadSizedMerkleVerifier::new(verifier.clone(), READ_SIZE).unwrap();
        let mut buf = vec![0u8; READ_SIZE];
        buf[..BLOCK_SIZE + 100].copy_from_slice(&data[..BLOCK_SIZE + 100]);
        assert_matches!(
            verifier.verify_aligned(0, PtrByteSlice::from(&buf[..BLOCK_SIZE]), 100),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            verifier.verify_aligned(0, PtrByteSlice::from(&buf[..]), 100),
            Err(Status::INVALID_ARGS)
        );
        assert_matches!(
            read_sized_verifier.verify_aligned(0, PtrByteSlice::from(&buf[..]), BLOCK_SIZE + 100),
            Err(Status::INVALID_ARGS)
        );

        let mut forged_hashes = leaf_hashes;
        forged_hashes[0] = hash_block(&data[..100], 0);
        let forged_root = rebuild_root(&forged_hashes);
        let forged_verifier =
            MerkleVerifier::new(forged_root, forged_hashes.into_boxed_slice()).unwrap();
        assert_matches!(
            forged_verifier.verify(BLOCK_SIZE, &data[BLOCK_SIZE..BLOCK_SIZE * 2]),
            Ok(())
        );
        assert_matches!(
            forged_verifier.verify_aligned(0, PtrByteSlice::from(&buf[..BLOCK_SIZE]), 100),
            Err(Status::INVALID_ARGS)
        );
    }

    #[test]
    #[ignore]
    fn test_very_large_verification() {
        const MAX_BUF: usize = 256 * 1024 * 1024 + 8192;
        let parallelism = std::thread::available_parallelism().unwrap().get();
        std::thread::scope(|scope| {
            for thread in 0..parallelism {
                scope.spawn(move || {
                    let mut data = create_data(MAX_BUF + 1);
                    for size in ((8192 * (thread + 1))..MAX_BUF).step_by(8192 * parallelism) {
                        verify_test(&mut data[0..size - 1]).unwrap();
                        verify_test(&mut data[0..size]).unwrap();
                        verify_test(&mut data[0..size + 1]).unwrap();
                    }
                });
            }
        });
    }
}
