// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::util::BoringSslSha256;
use crate::{BLOCK_SIZE, Hash};

struct CollectorBridge<T>(T);

impl<T: LeafHashCollector> fuchsia_merkle_core::LeafCollector for CollectorBridge<T> {
    #[inline]
    fn add_leaf_hash(&mut self, hash: fuchsia_merkle_core::Hash) {
        self.0.add_leaf_hash(Hash::from_array(hash.0));
    }
}

/// Calculates the merkle root for a given set of data. The leaf hashes of the merkle tree can
/// optionally be collected for use with [`crate::MerkleVerifier`].
///
/// [`MerkleRootBuilder`] only accepts complete blocks of data except for the last block. This
/// avoids buffering data internally. If complete blocks of data can't be guaranteed then
/// [`BufferedMerkleRootBuilder`] should be used instead.
pub struct MerkleRootBuilder<T> {
    inner: fuchsia_merkle_core::MerkleRootBuilder<BoringSslSha256, CollectorBridge<T>>,
}

impl<T: LeafHashCollector> MerkleRootBuilder<T> {
    /// Creates a new `MerkleRootBuilder` with a `LeafHashCollector`.
    ///
    /// Use `MerkleRootBuilder::default()` if a `LeafHashCollector` isn't needed.
    #[inline]
    pub fn new(leaf_hash_collector: T) -> Self {
        Self {
            inner: fuchsia_merkle_core::MerkleRootBuilder::new(CollectorBridge(
                leaf_hash_collector,
            )),
        }
    }

    /// Appends a full block of data to the merkle tree.
    ///
    /// `MerkleRootBuilder` doesn't buffer any data so only full blocks can be added until
    /// [`Self::complete`] is called. Use [`BufferedMerkleRootBuilder`] if buffering is required.
    #[inline]
    pub fn add_block(&mut self, data: &[u8; BLOCK_SIZE]) {
        self.inner.add_block(&data[..]);
    }

    /// Appends a final amount of data to the merkle tree and returns the merkle root.
    #[inline]
    pub fn complete(self, data: &[u8]) -> T::Output {
        let (root, CollectorBridge(collector)) = self.inner.complete(data);
        collector.complete(Hash::from_array(root.0))
    }

    /// Adds a hash of a single block of data to the merkle tree.
    #[cfg(test)]
    pub(crate) fn push_hash(&mut self, digest: Hash) {
        self.inner.push_hash(fuchsia_merkle_core::Hash(digest.into()));
    }
}

impl Default for MerkleRootBuilder<NoopLeafHashCollector> {
    fn default() -> Self {
        Self::new(NoopLeafHashCollector)
    }
}

/// Calculates the merkle root for a given set of data. The leaf hashes of the merkle tree can
/// optionally be collected for use with a `MerkleVerifier`.
///
/// [`BufferedMerkleRootBuilder`] is able to accept data that isn't a multiple of [`BLOCK_SIZE`] by
/// internally buffering the data.
///
/// If all of the data is already in memory then `MerkleRootBuilder::default().complete(data)`
/// should be used instead as it avoids buffering the tail end of the data if it's not a complete
/// block.
pub struct BufferedMerkleRootBuilder<T> {
    inner: fuchsia_merkle_core::BufferedMerkleRootBuilder<BoringSslSha256, CollectorBridge<T>>,
}

impl<T: LeafHashCollector> BufferedMerkleRootBuilder<T> {
    /// Creates a new `BufferedMerkleRootBuilder` with a `LeafHashCollector`.
    ///
    /// Use `BufferedMerkleRootBuilder::default()` if a `LeafHashCollector` isn't needed.
    #[inline]
    pub fn new(leaf_hash_collector: T) -> Self {
        Self {
            inner: fuchsia_merkle_core::BufferedMerkleRootBuilder::new(CollectorBridge(
                leaf_hash_collector,
            )),
        }
    }

    /// Appends data to the merkle tree.
    #[inline]
    pub fn write(&mut self, data: &[u8]) {
        self.inner.write(data);
    }

    /// Finishes building the merkle tree and returns the merkle root.
    #[inline]
    pub fn complete(self) -> T::Output {
        let (root, CollectorBridge(collector)) = self.inner.complete();
        collector.complete(Hash::from_array(root.0))
    }
}

impl Default for BufferedMerkleRootBuilder<NoopLeafHashCollector> {
    fn default() -> Self {
        Self::new(NoopLeafHashCollector)
    }
}

impl<T: LeafHashCollector> std::io::Write for BufferedMerkleRootBuilder<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A trait for collecting the leaf hashes of a merkle tree while constructing a merkle root.
pub trait LeafHashCollector {
    /// The output type of [`MerkleRootBuilder::complete`]. This allows for `complete` to return
    /// just the merkle root with `NoopLeafHashCollector` and also return the leaf hashes when a
    /// real `LeafHashCollector` is used.
    type Output;

    /// This method is called as each leaf hash in the merkle tree is created.
    ///
    /// If the merkle tree consists of just a root node then this method will be called for the root
    /// node.
    fn add_leaf_hash(&mut self, hash: Hash);

    /// Transforms the merkle root into Self::Output.
    fn complete(self, root: Hash) -> Self::Output;
}

/// A `LeafHashCollector` that doesn't collect leaf hashes.
pub struct NoopLeafHashCollector;
impl LeafHashCollector for NoopLeafHashCollector {
    type Output = Hash;

    fn add_leaf_hash(&mut self, _hash: Hash) {}

    fn complete(self, root: Hash) -> Self::Output {
        root
    }
}

impl LeafHashCollector for Vec<Hash> {
    type Output = (Hash, Vec<Hash>);

    fn add_leaf_hash(&mut self, hash: Hash) {
        self.push(hash);
    }

    fn complete(self, root: Hash) -> Self::Output {
        (root, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HASH_SIZE;
    use crate::util::hash_block;
    use fuchsia_merkle_core::{
        EMPTY_MERKLE_TREE_ROOT, HASHES_PER_BLOCK, Sha256State, make_hash_hasher, update_with_zeros,
    };
    use test_case::test_case;

    const AB_BLOCK: [u8; BLOCK_SIZE] = [0xAB; BLOCK_SIZE];

    #[test]
    fn test_empty_merkle_tree_root_constant() {
        assert_eq!(Hash::from_array(EMPTY_MERKLE_TREE_ROOT.0), hash_block(&[], 0));
    }

    fn add_blocks<T: LeafHashCollector>(block_count: usize, builder: &mut MerkleRootBuilder<T>) {
        for _ in 0..block_count {
            builder.add_block(&AB_BLOCK);
        }
    }

    // Hashes and the length of repeated 0xAB bytes. These were generated from the C++
    // implementation in //src/lib/digest.
    #[test_case("15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b", 0; "0")]
    #[test_case("2fabda7adeac120f587ae7b159c39d1716758b2c65e25b856470bbbb8b94e25e", 1; "1")]
    #[test_case("fd2acae20c551eea1df052f39478b33536ebf3b0c6ceef649a010de06b71822b", 4096; "4096")]
    #[test_case("85a0918a035140e720f4a5f10b7768d5b04959ed526da401cf4893aac73f436a", 8191; "8191")]
    #[test_case("1dc023a7a88f56fab52e0b078a92cd566546d11aed479c66a14d6592e0f011eb", 8192; "8192")]
    #[test_case("b6eae68aa1ad1b4fc74618575135f0d1daedc6bb07dc10f8fa41916d0578c318", 8193; "8193")]
    #[test_case(
        "35689edb3c4c46e44bb4ff00a7f8ac628d97ba16e1233f78759a41da597ed8dd",
        8192 * 2 - 1; "16383"
    )]
    #[test_case(
        "a6a903830d911d7555f9243b748d27a5aec1e385f91d845c854f46e02339070a",
        8192 * 2; "16384"
    )]
    #[test_case(
        "54c9eb182fc5bfc2984ca45049cf8cf1d071f2165e5190605b7970f824e03d8f",
        8192 * 2 + 1; "16385"
    )]
    #[test_case(
        "c96631f69f0cfaf2626dc4230aeb7fa50a05cd7e897039d2f56cdec99dc0b3b1",
        8192 * 256 - 1; "2097151"
    )]
    #[test_case(
        "cb717b240ae7ddf1ad197a3272db3cb4175bb7b7242d5050c6317df2bb8ed4b4",
        8192 * 256; "2097152"
    )]
    #[test_case(
        "9942b1f926e3295053e97b2b5a1471b6dff7df3d130f589c2881f9b98eee2fd0",
        8192 * 256 + 1; "2097153"
    )]
    #[test_case(
        "59f9f6275ec8b672446bc8fc2da0a7ce625a4c55fcbb3891144ace7a145f23e2",
        8192 * 256 * 2 - 1; "4194303"
    )]
    #[test_case(
        "fa21580bb27be533d2846e75e7fb3acca3fa2605f3fd3a4d49db2d023b10324b",
        8192 * 256 * 2; "4194304"
    )]
    #[test_case(
        "aec444c86e0553ed4a2a5e49faa2db9e6c9f0bdb6b97a8bd2b84aebec30b4558",
        8192 * 256 * 2 + 1; "4194305"
    )]
    #[test_case(
        "a68780a9adba83fdc1180732d974ca73a56b41629b84ee8f6046767348c2356a",
        8192 * 256 * 255; "534773760"
    )]
    #[test_case(
        "966a09567323e8d368ca25d55e797ef887cd14a1eafaba57b7cafe80d458a7b1",
        8192 * 256 * 255 + 1; "534773761"
    )]
    #[test_case(
        "da4fc0125b29f5b652f66f545238dbed714f997c6dba9d823e9f9f41580cb784",
        8192 * (256 * 256 - 1); "536862720"
    )]
    #[test_case(
        "799bb12a5f633f9949f4619e4e0af447c980f895e7261521abf69a2dba67ce9e",
        8192 * 256 * 256 - 1; "536870911"
    )]
    #[test_case(
        "b3b1a0ceb8e5949a5ba3f6db646e037438fe15053113c1b50b290678596467e3",
        8192 * 256 * 256; "536870912"
    )]
    #[test_case(
        "53b85ef0285254b7bb32bc6cd5092fdf1a610d96446848f4dd2841e4c93a55d2",
        8192 * 256 * 256 + 1; "536870913"
    )]
    fn test_merkle_root_builder_matches_lib_digest(hash: &str, size: usize) {
        let expected: Hash = hash.parse().unwrap();
        let mut builder = MerkleRootBuilder::default();
        add_blocks(size / BLOCK_SIZE, &mut builder);
        let actual = builder.complete(&AB_BLOCK[0..(size % BLOCK_SIZE)]);
        assert_eq!(actual, expected, "size={size}");
    }

    #[test_case(0, &[], false; "0")]
    #[test_case(8192, &[], true; "8192")]
    #[test_case(8192 * 2, &[64], false; "16384")]
    #[test_case(8192 * 255, &[8192 - 32], false; "2088960")]
    #[test_case(8192 * 256, &[8192], true; "2097152")]
    #[test_case(8192 * 257, &[8192 + 32, 32], false; "2105344")]
    #[test_case(8192 * 256 * 2, &[8192 * 2, 64], false; "4194304")]
    #[test_case(8192 * 256 * 255, &[8192 * 255, 8192 - 32], false; "534773760")]
    #[test_case(8192 * 256 * 256, &[8192 * 256, 8192], true; "536870912")]
    fn test_merkle_tree_structure(size: usize, expected_offsets: &[u64], is_root_set: bool) {
        let mut builder = MerkleRootBuilder::default();
        add_blocks(size / BLOCK_SIZE, &mut builder);
        let actual_offsets = builder.inner.level_offsets();
        assert_eq!(&actual_offsets, expected_offsets);
        assert_eq!(builder.inner.has_root(), is_root_set);
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
    #[test_case(8192 * 256 * 255; "534773760")]
    #[test_case(8192 * 256 * 255 + 1; "534773761")]
    #[test_case(8192 * (256 * 256 - 1); "536862720")]
    #[test_case(8192 * 256 * 256 - 1; "536870911")]
    #[test_case(8192 * 256 * 256; "536870912")]
    #[test_case(8192 * 256 * 256 + 1; "536870913")]
    fn test_rebuild_from_leaf_hashes(size: usize) {
        let mut builder = MerkleRootBuilder::new(Vec::new());
        add_blocks(size / BLOCK_SIZE, &mut builder);
        let (expected_root, leaf_hashes) = builder.complete(&AB_BLOCK[0..(size % BLOCK_SIZE)]);

        // There should always be leaf hashes, even for the empty input.
        assert!(!leaf_hashes.is_empty());

        let mut builder = MerkleRootBuilder::default();
        for hash in leaf_hashes {
            builder.push_hash(hash);
        }

        assert_eq!(builder.complete(&[]), expected_root);
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
    fn test_buffered_merkle_root_builder(size: usize) {
        let data = vec![0; size];
        let expected_root = MerkleRootBuilder::default().complete(&data);

        let mut builder = BufferedMerkleRootBuilder::default();
        for chunk in data.chunks(6079) {
            builder.write(chunk);
        }
        assert_eq!(builder.complete(), expected_root);

        let mut builder = BufferedMerkleRootBuilder::default();
        builder.write(&data);
        assert_eq!(builder.complete(), expected_root);
    }

    #[test]
    fn test_buffered_merkle_root_builder_as_writer() {
        let data = vec![0; 8192 * 3 + 60];
        let expected_root = MerkleRootBuilder::default().complete(&data);

        let mut reader = std::io::Cursor::new(data);
        let mut builder = BufferedMerkleRootBuilder::default();
        std::io::copy(&mut reader, &mut builder).unwrap();

        assert_eq!(builder.complete(), expected_root);
    }

    #[test_case(HASHES_PER_BLOCK * (HASHES_PER_BLOCK - 1) + 1; "65281_leaves")]
    #[test_case(HASHES_PER_BLOCK * HASHES_PER_BLOCK - 1; "65535_leaves")]
    fn test_complete_carry_completes_upper_level_without_extra_padding(leaf_count: usize) {
        const INTERNAL_BYTES_PER_HASH: u32 = (HASHES_PER_BLOCK * HASH_SIZE) as u32;
        let leaf = hash_block(&AB_BLOCK, 0);

        let mut builder = MerkleRootBuilder::default();
        for _ in 0..leaf_count {
            builder.push_hash(leaf);
        }
        let actual_root = builder.complete(&[]);

        // Reference level-1 computation: 255 full blocks of 256 `leaf` hashes, plus 1 final
        // zero-padded level-1 block containing the remaining `leaf_count - 65280` leaves.
        let mut level_1_hashes = Vec::with_capacity(HASHES_PER_BLOCK);
        for block_idx in 0..(HASHES_PER_BLOCK - 1) {
            let mut h1 = make_hash_hasher::<BoringSslSha256>(
                INTERNAL_BYTES_PER_HASH,
                1,
                (block_idx * BLOCK_SIZE) as u64,
            );
            for _ in 0..HASHES_PER_BLOCK {
                h1.update(leaf.as_bytes());
            }
            level_1_hashes.push(Hash::from_array(h1.finalize().0));
        }
        let tail_leaves = leaf_count - (HASHES_PER_BLOCK - 1) * HASHES_PER_BLOCK;
        let mut h1_last = make_hash_hasher::<BoringSslSha256>(
            INTERNAL_BYTES_PER_HASH,
            1,
            ((HASHES_PER_BLOCK - 1) * BLOCK_SIZE) as u64,
        );
        for _ in 0..tail_leaves {
            h1_last.update(leaf.as_bytes());
        }
        update_with_zeros(&mut h1_last, (HASHES_PER_BLOCK - tail_leaves) * HASH_SIZE);
        level_1_hashes.push(Hash::from_array(h1_last.finalize().0));
        assert_eq!(level_1_hashes.len(), HASHES_PER_BLOCK);

        // Level 2 has exactly 256 hashes (8,192 bytes) and must NOT receive 8,192 extra zero bytes.
        let mut h2 = make_hash_hasher::<BoringSslSha256>(INTERNAL_BYTES_PER_HASH, 2, 0);
        for h in &level_1_hashes {
            h2.update(h.as_bytes());
        }
        let expected_root = Hash::from_array(h2.finalize().0);

        assert_eq!(actual_root, expected_root);
    }
}
