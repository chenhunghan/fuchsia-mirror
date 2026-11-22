// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pure safe-Rust core of `fuchsia-merkle` structured for mechanical extraction to Lean 4 via
//! `Charon` and `Aeneas`.
//!
//! `rust_lean_verification_test` compiles this crate directly with `charon rustc` and `aeneas` with
//! zero external crate dependencies (`core` and `alloc` only), isolating external libraries such as
//! `fuchsia-hash` (`serde`, `hex`, `thiserror`), BoringSSL, `zx-status`, and `ptr_slice` in the
//! outer `fuchsia-merkle` crate.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

/// Size of a SHA-256 / Fuchsia Merkle digest in bytes.
pub const HASH_SIZE: usize = 32;

/// Size of a single Fuchsia Merkle data or hash block in bytes.
pub const BLOCK_SIZE: usize = 8192;

/// Minimum supported VM page size in bytes for aligned verification buffers.
pub const PAGE_SIZE_MIN: usize = 4096;

/// Number of 32-byte child hashes packed into a single 8 KiB internal tree block
/// (`BLOCK_SIZE / HASH_SIZE`).
pub const HASHES_PER_BLOCK: usize = 256;

/// A 32-byte Fuchsia Merkle digest.
///
/// Defined locally rather than shared with `fuchsia-hash` so this crate stays free of external
/// crate dependencies for `Charon`/`Aeneas` extraction while `fuchsia-hash` retains ownership of
/// its digest wrapper for `serde`, hex parsing, and formatting trait implementations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hash(pub [u8; HASH_SIZE]);

/// Error returned by Merkle tree verifier constructors and verification routines.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// Offset, length, alignment, or size arguments violate verifier preconditions.
    InvalidArgs,
    /// Provided leaf hashes, block data, or trailing page padding failed cryptographic checks.
    IoDataIntegrity,
}

/// Canonical Merkle root of the 0-byte empty blob (`SHA256([0u8; 12])`).
pub const EMPTY_MERKLE_TREE_ROOT: Hash = Hash([
    0x15, 0xec, 0x7b, 0xf0, 0xb5, 0x07, 0x32, 0xb4, 0x9f, 0x82, 0x28, 0xe0, 0x7d, 0x24, 0x36, 0x53,
    0x38, 0xf9, 0xe3, 0xab, 0x99, 0x4b, 0x00, 0xaf, 0x08, 0xe5, 0xa3, 0xbf, 0xfe, 0x55, 0xfd, 0x8b,
]);

/// Safe streaming SHA-256 interface isolating BoringSSL FFI and architecture-specific SIMD/assembly
/// outside the verified core crate.
pub trait Sha256State {
    /// Creates a fresh SHA-256 state initialized to the standard IV.
    fn new() -> Self;

    /// Appends `bytes` to the message being hashed.
    fn update(&mut self, bytes: &[u8]);

    /// Consumes the state and returns the 32-byte SHA-256 digest.
    fn finalize(self) -> Hash;
}

/// Safe streaming sink receiving each level-0 Merkle leaf hash in block order.
pub trait LeafCollector {
    /// Called once for each level-0 leaf hash in block order (or once with the empty-blob root hash
    /// when completing a 0-byte blob).
    fn add_leaf_hash(&mut self, hash: Hash);
}

/// Zero-sized [`LeafCollector`] that discards leaf hashes when only the Merkle root is needed.
pub struct NoopLeafCollector;

impl LeafCollector for NoopLeafCollector {
    #[inline]
    fn add_leaf_hash(&mut self, _hash: Hash) {}
}

/// Safe abstraction over a page-aligned byte buffer (such as `PtrByteSlice<'_>`) for aligned
/// level-0 Merkle block hashing.
pub trait AlignedByteSource {
    /// Total length of the aligned buffer in bytes.
    fn buffer_len(&self) -> usize;

    /// Computes the level-0 Merkle hash of the aligned sub-buffer
    /// `byte_pos..byte_pos + buffer_len` (`buffer_len` is `4096` or `8192`) with logical data
    /// length `unaligned_len` (`0 < unaligned_len <= buffer_len`) at blob `offset`.
    fn hash_aligned_block(
        &self,
        offset: u64,
        byte_pos: usize,
        buffer_len: usize,
        unaligned_len: usize,
    ) -> Hash;
}

/// Encoded 12-byte little-endian Fuchsia Merkle block identity header.
pub type BlockIdentity = [u8; 12];

/// Overflow-free `ceil(a / b)` for `b > 0` when the rounded quotient fits in `usize`, avoiding
/// `usize::div_ceil` which is not yet modeled in Aeneas's Lean standard library.
#[inline]
#[allow(clippy::manual_div_ceil)]
pub fn div_ceil_usize(a: usize, b: usize) -> usize {
    let q = a / b;
    let r = a % b;
    if r != 0 { q + 1 } else { q }
}

/// Generates the 12-byte little-endian identity header `u64(offset | level) || u32(length)`.
#[inline]
pub fn make_identity(length: u32, level: u64, offset: u64) -> BlockIdentity {
    let [o0, o1, o2, o3, o4, o5, o6, o7] = (offset | level).to_le_bytes();
    let [l0, l1, l2, l3] = length.to_le_bytes();
    [o0, o1, o2, o3, o4, o5, o6, o7, l0, l1, l2, l3]
}

/// Updates `hasher` with `count` zero bytes in 512-byte stack chunks.
pub fn update_with_zeros<H: Sha256State>(hasher: &mut H, mut count: usize) {
    const BUF_SIZE: usize = 512;
    const ZEROS: [u8; BUF_SIZE] = [0u8; BUF_SIZE];
    while count >= BUF_SIZE {
        count -= BUF_SIZE;
        hasher.update(&ZEROS);
    }
    if count > 0 {
        hasher.update(&ZEROS[0..count]);
    }
}

/// Computes the Fuchsia Merkle hash of a level-0 data block (`block.len() <= BLOCK_SIZE`).
///
/// A Merkle hash is the SHA-256 hash of a block of data with a 12-byte header built from the
/// length of the data, the level of the tree (`0` for data blocks), and the offset into the level.
/// The block is zero-filled if its length is less than [`BLOCK_SIZE`], except when the first data
/// block is completely empty.
///
/// # Panics
///
/// Panics if `block.len()` exceeds [`BLOCK_SIZE`] or if `offset` is not aligned to [`BLOCK_SIZE`].
pub fn hash_block<H: Sha256State>(block: &[u8], offset: u64) -> Hash {
    assert!(block.len() <= BLOCK_SIZE);
    assert!(offset.is_multiple_of(BLOCK_SIZE as u64));

    let mut hasher = H::new();
    let identity = make_identity(block.len() as u32, 0, offset);
    hasher.update(&identity);
    hasher.update(block);
    // Zero fill block up to BLOCK_SIZE. As a special case, if the first data block is completely
    // empty, it is not zero filled.
    if block.len() != BLOCK_SIZE && !(block.is_empty() && offset == 0) {
        update_with_zeros(&mut hasher, BLOCK_SIZE - block.len());
    }
    hasher.finalize()
}

/// Computes the Fuchsia Merkle hash of a 4096- or 8192-byte page-aligned buffer slice whose first
/// `unaligned_len` bytes are logical blob data (`0 < unaligned_len <= buffer_len`) and whose
/// remaining bytes up to `buffer_len` must be zeroed to match [`hash_block`].
///
/// # Panics
///
/// Panics if `buffer_len` is not [`PAGE_SIZE_MIN`] or [`BLOCK_SIZE`], if `unaligned_len == 0` or
/// `unaligned_len > buffer_len`, if `offset` is not aligned to [`BLOCK_SIZE`], or if
/// `data.len() < buffer_len`.
pub fn hash_block_aligned<H: Sha256State>(
    offset: u64,
    data: &[u8],
    buffer_len: usize,
    unaligned_len: usize,
) -> Hash {
    assert!(buffer_len == BLOCK_SIZE || buffer_len == PAGE_SIZE_MIN);
    assert!(unaligned_len > 0 && unaligned_len <= buffer_len);
    assert!(offset.is_multiple_of(BLOCK_SIZE as u64));
    assert!(data.len() >= buffer_len);

    let mut hasher = H::new();
    let identity = make_identity(unaligned_len as u32, 0, offset);
    hasher.update(&identity);
    hasher.update(&data[0..buffer_len]);
    if buffer_len < BLOCK_SIZE {
        update_with_zeros(&mut hasher, BLOCK_SIZE - buffer_len);
    }
    hasher.finalize()
}

/// Creates a new SHA-256 state for hashing a block of child hashes at `level >= 1`, initialized
/// with the block's identity header.
///
/// # Panics
///
/// Panics if `level == 0`.
pub fn make_hash_hasher<H: Sha256State>(bytes_per_hash: u32, level: u64, offset: u64) -> H {
    assert!(level > 0);
    let mut hasher = H::new();
    let identity = make_identity(bytes_per_hash, level, offset);
    hasher.update(&identity);
    hasher
}

/// Hashes blocks of hashes.
struct LevelHasher<H> {
    hasher: H,
    offset: u64,
}

/// Calculates the Fuchsia Merkle root over whole [`BLOCK_SIZE`] blocks plus a final tail slice.
///
/// Level-0 leaf hashes are passed to `collector` for optional use with [`MerkleVerifier`]. Only
/// complete blocks may be passed to [`Self::add_block`], avoiding internal data buffering; use
/// [`BufferedMerkleRootBuilder`] when arbitrary chunk sizes are needed.
pub struct MerkleRootBuilder<H, C> {
    levels: Vec<LevelHasher<H>>,
    offset: u64,
    collector: C,
    /// Holds the root of the tree when the offset of every active level is a multiple of
    /// [`BLOCK_SIZE`]. This only happens when `offset` is equal to `8192 * 256^N` for `N >= 0`.
    ///
    /// If [`Self::push_hash`] immediately turned the root into a new level then [`Self::complete`]
    /// wouldn't be able to recover the root if no more data was added.
    root: Option<Hash>,
}

impl<H: Sha256State, C: LeafCollector> MerkleRootBuilder<H, C> {
    /// Creates a new streaming Merkle root builder feeding level-0 leaf hashes to `collector`.
    #[inline]
    pub fn new(collector: C) -> Self {
        Self { levels: Vec::new(), offset: 0, collector, root: None }
    }

    /// Returns the current byte offsets at each active internal level of the tree.
    pub fn level_offsets(&self) -> Vec<u64> {
        let mut out = Vec::with_capacity(self.levels.len());
        let mut i: usize = 0;
        while i < self.levels.len() {
            out.push(self.levels[i].offset);
            i += 1;
        }
        out
    }

    /// Returns `true` if every active level of the tree is currently complete (`root` is set).
    #[inline]
    pub fn has_root(&self) -> bool {
        self.root.is_some()
    }

    /// Appends a single pre-hashed level-0 block digest to the tree.
    pub fn push_hash(&mut self, mut digest: Hash) {
        self.offset = self.offset.checked_add(BLOCK_SIZE as u64).unwrap();
        self.collector.add_leaf_hash(digest);

        if let Some(root) = self.root.take() {
            // The root was set but more data has now been added. Create a new level containing the
            // root.
            let next_level = (self.levels.len() as u64) + 1;
            let mut hasher = make_hash_hasher::<H>(BLOCK_SIZE as u32, next_level, 0);
            hasher.update(&root.0);
            self.levels.push(LevelHasher { hasher, offset: HASH_SIZE as u64 });
        }

        let mut level_idx: usize = 0;
        while level_idx < self.levels.len() {
            let level_num = (level_idx as u64) + 1;
            let level_hasher = &mut self.levels[level_idx];
            level_hasher.hasher.update(&digest.0);
            level_hasher.offset += HASH_SIZE as u64;

            // Only bubble the hash up to the next level when this level completes a block.
            if !level_hasher.offset.is_multiple_of(BLOCK_SIZE as u64) {
                return;
            }
            // Initialize a new hasher for this level and bubble the old hasher up to the next
            // level.
            let new_hasher =
                make_hash_hasher::<H>(BLOCK_SIZE as u32, level_num, level_hasher.offset);
            let old_hasher = core::mem::replace(&mut level_hasher.hasher, new_hasher);
            digest = old_hasher.finalize();
            level_idx += 1;
        }

        self.root = Some(digest);
    }

    /// Hashes and appends a full [`BLOCK_SIZE`] data block to the Merkle tree.
    ///
    /// `MerkleRootBuilder` doesn't buffer any data so only full blocks can be added until
    /// [`Self::complete`] is called. Use [`BufferedMerkleRootBuilder`] if buffering is required.
    #[inline]
    pub fn add_block(&mut self, data: &[u8]) {
        assert!(data.len() == BLOCK_SIZE);
        let h = hash_block::<H>(data, self.offset);
        self.push_hash(h);
    }

    /// Adds all of the whole blocks from `data` and returns the remaining bytes.
    fn add_blocks<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        let mut pos: usize = 0;
        while data.len() - pos >= BLOCK_SIZE {
            let block = &data[pos..pos + BLOCK_SIZE];
            self.add_block(block);
            pos += BLOCK_SIZE;
        }
        &data[pos..]
    }

    /// Appends `data` as the final bytes of the blob and returns `(root, collector)`.
    pub fn complete(mut self, data: &[u8]) -> (Hash, C) {
        // Add all of the complete blocks from `data`.
        let remaining = self.add_blocks(data);

        // Handle the last partial block when `data` isn't a block multiple.
        if !remaining.is_empty() {
            let h = hash_block::<H>(remaining, self.offset);
            self.push_hash(h);
        }

        // Special case for the merkle tree of no data.
        if self.offset == 0 {
            let empty_root = hash_block::<H>(&[], 0);
            self.collector.add_leaf_hash(empty_root);
            return (empty_root, self.collector);
        }

        // Special case for when every level of the tree is already complete.
        if let Some(root) = self.root {
            return (root, self.collector);
        }

        // Each row of the tree is padded with zeros to fill the last block and the resulting hash
        // is bubbled up to the next level. Since `root` wasn't set, there's at least 1 level that
        // needs padding so `bubbling_digest` is guaranteed to be set by the end of the loop.
        let mut bubbling_digest: Option<Hash> = None;
        let mut i: usize = 0;
        while i < self.levels.len() {
            let mut level_hasher = core::mem::replace(
                &mut self.levels[i],
                LevelHasher { hasher: H::new(), offset: 0 },
            );
            if level_hasher.offset.is_multiple_of(BLOCK_SIZE as u64) && bubbling_digest.is_none() {
                i += 1;
                continue;
            }
            if let Some(digest) = bubbling_digest.take() {
                level_hasher.hasher.update(&digest.0);
                level_hasher.offset += HASH_SIZE as u64;
            }
            let rem = (level_hasher.offset % (BLOCK_SIZE as u64)) as usize;
            if rem != 0 {
                update_with_zeros(&mut level_hasher.hasher, BLOCK_SIZE - rem);
            }
            bubbling_digest = Some(level_hasher.hasher.finalize());
            i += 1;
        }

        match bubbling_digest {
            Some(root) => (root, self.collector),
            None => unreachable!(),
        }
    }
}

/// Arbitrary-chunk buffered Fuchsia Merkle root builder.
///
/// [`BufferedMerkleRootBuilder`] accepts data that isn't a multiple of [`BLOCK_SIZE`] by internally
/// buffering incomplete blocks. If all of the data is already in memory then
/// [`MerkleRootBuilder::complete`] avoids buffering the tail end of the data when it isn't a
/// complete block.
pub struct BufferedMerkleRootBuilder<H, C> {
    buffer: Vec<u8>,
    builder: MerkleRootBuilder<H, C>,
}

impl<H: Sha256State, C: LeafCollector> BufferedMerkleRootBuilder<H, C> {
    /// Creates a buffered builder feeding level-0 leaf hashes to `collector`.
    #[inline]
    pub fn new(collector: C) -> Self {
        Self { buffer: Vec::new(), builder: MerkleRootBuilder::new(collector) }
    }

    /// Appends an arbitrary-length byte chunk to the buffered builder.
    #[expect(
        clippy::len_zero,
        clippy::vec_resize_to_zero,
        reason = "Aeneas.Std models Vec::len and Vec::resize transparently, whereas Vec::is_empty and Vec::clear emit external axioms"
    )]
    pub fn write(&mut self, mut data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.buffer.len().saturating_add(data.len()) < BLOCK_SIZE {
            // The current buffer and the new data won't complete a block. Buffer the data and
            // return.
            self.buffer.extend_from_slice(data);
            return;
        }
        if self.buffer.len() > 0 {
            // If the buffered data isn't empty then take bytes from the start of the new data to
            // complete a block. The above case handles when a block wouldn't be completed so this
            // is guaranteed to complete a block.
            let needed = BLOCK_SIZE - self.buffer.len();
            let (head, tail) = data.split_at(needed);
            self.buffer.extend_from_slice(head);
            self.builder.add_block(&self.buffer);
            self.buffer.resize(0, 0);
            data = tail;
        }
        // Add all remaining whole blocks.
        let remainder = self.builder.add_blocks(data);
        if !remainder.is_empty() {
            // Buffer any remaining data.
            self.buffer.extend_from_slice(remainder);
        }
    }

    /// Finishes hashing any buffered tail bytes and returns `(root, collector)`.
    #[inline]
    pub fn complete(self) -> (Hash, C) {
        self.builder.complete(&self.buffer)
    }
}

/// Rebuilds the Merkle root from a slice of level-0 leaf hashes.
pub fn rebuild_root<H: Sha256State>(leaf_hashes: &[Hash]) -> Hash {
    let mut builder = MerkleRootBuilder::<H, NoopLeafCollector>::new(NoopLeafCollector);
    let mut i: usize = 0;
    while i < leaf_hashes.len() {
        builder.push_hash(leaf_hashes[i]);
        i += 1;
    }
    builder.complete(&[]).0
}

/// Computes a single level-1 grouped hash over `hashes` (`hashes.len() <= hashes_per_hash`) at
/// chunk `index`, zero-padding unused hash slots up to `hashes_per_hash`.
pub fn hash_hashes_slice<H: Sha256State>(
    hashes_per_hash: usize,
    bytes_per_hash: u32,
    index: usize,
    hashes: &[Hash],
) -> Hash {
    assert!(hashes.len() <= hashes_per_hash);
    let level_offset = (index as u64) * (bytes_per_hash as u64);
    let mut hasher = make_hash_hasher::<H>(bytes_per_hash, 1, level_offset);
    let mut i: usize = 0;
    while i < hashes.len() {
        hasher.update(&hashes[i].0);
        i += 1;
    }
    if hashes.len() != hashes_per_hash {
        update_with_zeros(&mut hasher, (hashes_per_hash - hashes.len()) * HASH_SIZE);
    }
    hasher.finalize()
}

fn clone_boxed_hashes(hashes: &[Hash]) -> Box<[Hash]> {
    let mut copy = Vec::with_capacity(hashes.len());
    copy.extend_from_slice(hashes);
    <Box<[Hash]> as From<Vec<Hash>>>::from(copy)
}

/// Leaf-granularity (8 KiB) Merkle tree verifier.
pub struct MerkleVerifier {
    hashes: Box<[Hash]>,
}

impl Clone for MerkleVerifier {
    fn clone(&self) -> Self {
        Self { hashes: clone_boxed_hashes(&self.hashes) }
    }
}

impl MerkleVerifier {
    /// Validates `hashes` against `root` and constructs a [`MerkleVerifier`].
    #[allow(clippy::boxed_local)]
    pub fn new<H: Sha256State>(root: Hash, hashes: Box<[Hash]>) -> Result<Self, VerifyError> {
        if hashes.is_empty() {
            return Err(VerifyError::IoDataIntegrity);
        }
        let len_u64 = hashes.len() as u64;
        if len_u64.checked_mul(BLOCK_SIZE as u64).is_none() {
            return Err(VerifyError::InvalidArgs);
        }
        if rebuild_root::<H>(&hashes) != root {
            Err(VerifyError::IoDataIntegrity)
        } else {
            Ok(Self { hashes })
        }
    }

    /// Returns the number of level-0 leaf hashes stored by this verifier.
    #[inline]
    pub fn hash_count(&self) -> usize {
        self.hashes.len()
    }

    /// Returns the verified level-0 leaf hashes stored by this verifier.
    #[inline]
    pub fn hashes(&self) -> &[Hash] {
        &self.hashes
    }

    fn verify_blocks<H: Sha256State>(
        &self,
        offset: usize,
        data: &[u8],
        base_block: usize,
    ) -> Result<(), VerifyError> {
        let mut pos: usize = 0;
        let mut i: usize = 0;
        while pos < data.len() {
            let chunk_len = core::cmp::min(BLOCK_SIZE, data.len() - pos);
            let chunk = &data[pos..pos + chunk_len];
            let block_offset = (offset as u64) + (pos as u64);
            let hash = hash_block::<H>(chunk, block_offset);
            if self.hashes[base_block + i] != hash {
                return Err(VerifyError::IoDataIntegrity);
            }
            pos += chunk_len;
            i += 1;
        }
        Ok(())
    }

    /// Verifies `data` starting at byte `offset` against the level-0 leaf hashes.
    pub fn verify<H: Sha256State>(&self, offset: usize, data: &[u8]) -> Result<(), VerifyError> {
        if !offset.is_multiple_of(BLOCK_SIZE) {
            return Err(VerifyError::InvalidArgs);
        }
        if data.is_empty() {
            if offset != 0 || self.hashes.len() != 1 {
                return Err(VerifyError::InvalidArgs);
            }
            let empty_hash = hash_block::<H>(&[], 0);
            return if self.hashes[0] == empty_hash {
                Ok(())
            } else {
                Err(VerifyError::IoDataIntegrity)
            };
        }

        let ending = match data.len().checked_add(offset) {
            Some(v) => v,
            None => return Err(VerifyError::InvalidArgs),
        };
        let hash_end_index = div_ceil_usize(ending, BLOCK_SIZE);
        if hash_end_index > self.hashes.len() {
            return Err(VerifyError::InvalidArgs);
        }
        if !ending.is_multiple_of(BLOCK_SIZE) && hash_end_index != self.hashes.len() {
            return Err(VerifyError::InvalidArgs);
        }

        let base_block = offset / BLOCK_SIZE;
        self.verify_blocks::<H>(offset, data, base_block)
    }

    fn verify_aligned_blocks<B: AlignedByteSource>(
        &self,
        offset: usize,
        data: &B,
        len: usize,
        unaligned_len: usize,
        base_block: usize,
        num_blocks: usize,
    ) -> Result<(), VerifyError> {
        let mut i: usize = 0;
        while i < num_blocks {
            let byte_pos = i * BLOCK_SIZE;
            let block_buffer_len = core::cmp::min(BLOCK_SIZE, len - byte_pos);
            let block_unaligned_len = core::cmp::min(block_buffer_len, unaligned_len - byte_pos);
            let hash = data.hash_aligned_block(
                (offset as u64) + (byte_pos as u64),
                byte_pos,
                block_buffer_len,
                block_unaligned_len,
            );
            if self.hashes[base_block + i] != hash {
                return Err(VerifyError::IoDataIntegrity);
            }
            i += 1;
        }
        Ok(())
    }

    /// Verifies a page-aligned, zero-padded [`AlignedByteSource`] with logical byte length
    /// `unaligned_len` starting at `offset`.
    ///
    /// Rejects buffers that are not `PAGE_SIZE_MIN`-aligned or that extend into extra Merkle blocks
    /// beyond `unaligned_len`. Because every 4 KiB page of an accepted buffer lies inside one of
    /// the hashed blocks, non-zero tail bytes in `unaligned_len..data.buffer_len()` fail the block
    /// digest comparison directly.
    pub fn verify_aligned<B: AlignedByteSource>(
        &self,
        offset: usize,
        data: &B,
        unaligned_len: usize,
    ) -> Result<(), VerifyError> {
        let len = data.buffer_len();
        if offset.checked_add(len).is_none() {
            return Err(VerifyError::InvalidArgs);
        }
        if !offset.is_multiple_of(BLOCK_SIZE)
            || !len.is_multiple_of(PAGE_SIZE_MIN)
            || unaligned_len == 0
            || unaligned_len > len
        {
            return Err(VerifyError::InvalidArgs);
        }
        let num_blocks = div_ceil_usize(unaligned_len, BLOCK_SIZE);
        if div_ceil_usize(len, BLOCK_SIZE) > num_blocks {
            return Err(VerifyError::InvalidArgs);
        }
        let unaligned_end = offset + unaligned_len;
        let hash_end_index = div_ceil_usize(unaligned_end, BLOCK_SIZE);
        if hash_end_index > self.hashes.len() {
            return Err(VerifyError::InvalidArgs);
        }
        if !unaligned_end.is_multiple_of(BLOCK_SIZE) && hash_end_index != self.hashes.len() {
            return Err(VerifyError::InvalidArgs);
        }

        let base_block = offset / BLOCK_SIZE;
        self.verify_aligned_blocks(offset, data, len, unaligned_len, base_block, num_blocks)
    }
}

/// Read-size-granularity Merkle tree verifier.
pub struct ReadSizedMerkleVerifier {
    read_size: usize,
    bytes_per_hash: u32,
    hashes: Box<[Hash]>,
}

impl Clone for ReadSizedMerkleVerifier {
    fn clone(&self) -> Self {
        Self {
            read_size: self.read_size,
            bytes_per_hash: self.bytes_per_hash,
            hashes: clone_boxed_hashes(&self.hashes),
        }
    }
}

impl ReadSizedMerkleVerifier {
    /// Folds `verifier`'s leaf hashes into `read_size`-aligned level-1 hashes.
    pub fn new<H: Sha256State>(
        verifier: MerkleVerifier,
        read_size: usize,
    ) -> Result<Self, VerifyError> {
        if read_size == 0 || !read_size.is_multiple_of(BLOCK_SIZE) {
            return Err(VerifyError::InvalidArgs);
        }
        let hashes_per_hash = read_size / BLOCK_SIZE;
        let bytes_per_hash_usize = match hashes_per_hash.checked_mul(HASH_SIZE) {
            Some(v) => v,
            None => return Err(VerifyError::InvalidArgs),
        };
        if bytes_per_hash_usize > (u32::MAX as usize) {
            return Err(VerifyError::InvalidArgs);
        }
        let bytes_per_hash = bytes_per_hash_usize as u32;

        let cap = div_ceil_usize(verifier.hashes.len(), hashes_per_hash);
        let mut level_1_hashes = Vec::with_capacity(cap);
        let mut pos: usize = 0;
        let mut idx: usize = 0;
        while pos < verifier.hashes.len() {
            let chunk_len = core::cmp::min(hashes_per_hash, verifier.hashes.len() - pos);
            let slice = &verifier.hashes[pos..pos + chunk_len];
            level_1_hashes.push(hash_hashes_slice::<H>(
                hashes_per_hash,
                bytes_per_hash,
                idx,
                slice,
            ));
            pos += chunk_len;
            idx += 1;
        }

        let hashes = <Box<[Hash]> as From<Vec<Hash>>>::from(level_1_hashes);
        Ok(ReadSizedMerkleVerifier { read_size, bytes_per_hash, hashes })
    }

    /// Returns the configured read-alignment chunk size in bytes.
    #[inline]
    pub fn read_size(&self) -> usize {
        self.read_size
    }

    /// Returns the number of folded level-1 chunk hashes stored by this verifier.
    #[inline]
    pub fn hash_count(&self) -> usize {
        self.hashes.len()
    }

    fn verify_chunks<H: Sha256State>(
        &self,
        offset: usize,
        data: &[u8],
        hash_start_index: usize,
        hashes_per_hash: usize,
    ) -> Result<(), VerifyError> {
        let mut chunk_pos: usize = 0;
        let mut i: usize = 0;
        while chunk_pos < data.len() {
            let chunk_len = core::cmp::min(self.read_size, data.len() - chunk_pos);
            let chunk = &data[chunk_pos..chunk_pos + chunk_len];

            let level_offset = ((hash_start_index + i) as u64) * (self.bytes_per_hash as u64);
            let mut hasher = make_hash_hasher::<H>(self.bytes_per_hash, 1, level_offset);
            let mut sub_pos: usize = 0;
            let mut hash_count: usize = 0;
            while sub_pos < chunk.len() {
                let block_len = core::cmp::min(BLOCK_SIZE, chunk.len() - sub_pos);
                let block = &chunk[sub_pos..sub_pos + block_len];
                let block_offset = (offset as u64) + (chunk_pos as u64) + (sub_pos as u64);
                let leaf_hash = hash_block::<H>(block, block_offset);
                hasher.update(&leaf_hash.0);
                sub_pos += block_len;
                hash_count += 1;
            }
            if hash_count != hashes_per_hash {
                update_with_zeros(&mut hasher, (hashes_per_hash - hash_count) * HASH_SIZE);
            }
            if hasher.finalize() != self.hashes[hash_start_index + i] {
                return Err(VerifyError::IoDataIntegrity);
            }

            chunk_pos += chunk_len;
            i += 1;
        }

        Ok(())
    }

    /// Verifies `data` starting at `read_size`-aligned byte `offset`.
    pub fn verify<H: Sha256State>(&self, offset: usize, data: &[u8]) -> Result<(), VerifyError> {
        let end = match offset.checked_add(data.len()) {
            Some(v) => v,
            None => return Err(VerifyError::InvalidArgs),
        };
        if !offset.is_multiple_of(self.read_size) {
            // The offset must be read-aligned.
            return Err(VerifyError::InvalidArgs);
        }
        let hashes_per_hash = self.read_size / BLOCK_SIZE;
        if data.is_empty() {
            if offset != 0 || self.hashes.len() != 1 {
                return Err(VerifyError::InvalidArgs);
            }
            let empty_leaf = [hash_block::<H>(&[], 0)];
            let expected =
                hash_hashes_slice::<H>(hashes_per_hash, self.bytes_per_hash, 0, &empty_leaf);
            return if self.hashes[0] == expected {
                Ok(())
            } else {
                Err(VerifyError::IoDataIntegrity)
            };
        }

        let hash_start_index = offset / self.read_size;
        let hash_end_index = div_ceil_usize(end, self.read_size);

        if !end.is_multiple_of(self.read_size) && hash_end_index != self.hashes.len() {
            // The end is not aligned and it's not the end of the data.
            return Err(VerifyError::InvalidArgs);
        }
        if hash_end_index > self.hashes.len() {
            return Err(VerifyError::InvalidArgs);
        }

        self.verify_chunks::<H>(offset, data, hash_start_index, hashes_per_hash)
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_aligned_chunks<H: Sha256State, B: AlignedByteSource>(
        &self,
        offset: usize,
        data: &B,
        len: usize,
        unaligned_len: usize,
        hash_start_index: usize,
        hashes_per_hash: usize,
        num_chunks: usize,
    ) -> Result<(), VerifyError> {
        let mut i: usize = 0;
        while i < num_chunks {
            let chunk_pos = i * self.read_size;
            let chunk_unaligned_len = core::cmp::min(self.read_size, unaligned_len - chunk_pos);
            let num_blocks = div_ceil_usize(chunk_unaligned_len, BLOCK_SIZE);

            let level_offset = ((hash_start_index + i) as u64) * (self.bytes_per_hash as u64);
            let mut hasher = make_hash_hasher::<H>(self.bytes_per_hash, 1, level_offset);
            let mut j: usize = 0;
            while j < num_blocks {
                let byte_pos = chunk_pos + j * BLOCK_SIZE;
                let block_buffer_len = core::cmp::min(BLOCK_SIZE, len - byte_pos);
                let block_unaligned_len =
                    core::cmp::min(block_buffer_len, unaligned_len - byte_pos);
                let leaf_hash = data.hash_aligned_block(
                    (offset as u64) + (byte_pos as u64),
                    byte_pos,
                    block_buffer_len,
                    block_unaligned_len,
                );
                hasher.update(&leaf_hash.0);
                j += 1;
            }
            if num_blocks != hashes_per_hash {
                update_with_zeros(&mut hasher, (hashes_per_hash - num_blocks) * HASH_SIZE);
            }
            if hasher.finalize() != self.hashes[hash_start_index + i] {
                return Err(VerifyError::IoDataIntegrity);
            }
            i += 1;
        }
        Ok(())
    }

    /// Verifies a page-aligned, zero-padded [`AlignedByteSource`] with logical byte length
    /// `unaligned_len` starting at `read_size`-aligned `offset`.
    ///
    /// Rejects buffers that are not `PAGE_SIZE_MIN`-aligned or that extend into extra Merkle blocks
    /// beyond `unaligned_len`.
    pub fn verify_aligned<H: Sha256State, B: AlignedByteSource>(
        &self,
        offset: usize,
        data: &B,
        unaligned_len: usize,
    ) -> Result<(), VerifyError> {
        let len = data.buffer_len();
        if offset.checked_add(len).is_none() {
            return Err(VerifyError::InvalidArgs);
        }
        if !offset.is_multiple_of(self.read_size)
            || !len.is_multiple_of(PAGE_SIZE_MIN)
            || unaligned_len == 0
            || unaligned_len > len
            || div_ceil_usize(len, BLOCK_SIZE) > div_ceil_usize(unaligned_len, BLOCK_SIZE)
        {
            return Err(VerifyError::InvalidArgs);
        }

        let unaligned_end = offset + unaligned_len;
        let hash_start_index = offset / self.read_size;
        let hash_end_index = div_ceil_usize(unaligned_end, self.read_size);

        if !unaligned_end.is_multiple_of(self.read_size) && hash_end_index != self.hashes.len() {
            // The end is not aligned and it's not the end of the data.
            return Err(VerifyError::InvalidArgs);
        }
        if hash_end_index > self.hashes.len() {
            return Err(VerifyError::InvalidArgs);
        }

        let hashes_per_hash = self.read_size / BLOCK_SIZE;
        let num_chunks = div_ceil_usize(unaligned_len, self.read_size);
        self.verify_aligned_chunks::<H, B>(
            offset,
            data,
            len,
            unaligned_len,
            hash_start_index,
            hashes_per_hash,
            num_chunks,
        )
    }
}
