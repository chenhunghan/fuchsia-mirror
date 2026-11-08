// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod zerocopy_serialization;

use delivery_blob::compression::{ChunkedArchiveError, CompressionAlgorithm, CompressionInfo};
use fprint::TypeFingerprint;
use fuchsia_merkle::{Hash, MerkleVerifier};
use fxfs_macros::{Migrate, migrate_nodefault};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, TypeFingerprint)]
pub struct BlobMetadataUnversioned {
    pub hashes: Vec<[u8; 32]>,
    pub chunk_size: u64,
    pub compressed_offsets: Vec<u64>,
    pub uncompressed_size: u64,
}

pub type BlobMetadata = BlobMetadataV64;
pub type BlobFormat = BlobFormatV64;
pub type MerkleLeaves = Vec<[u8; 32]>;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, TypeFingerprint)]
pub struct BlobMetadataV64 {
    #[serde(with = "zerocopy_serialization")]
    pub merkle_leaves: MerkleLeaves,
    pub format: BlobFormatV64,
}

impl BlobMetadata {
    pub fn empty() -> Self {
        Self { merkle_leaves: Vec::new(), format: BlobFormat::Uncompressed }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::empty()
    }

    pub fn into_merkle_verifier(self, root: Hash) -> Result<MerkleVerifier, anyhow::Error> {
        let hashes = if self.merkle_leaves.is_empty() {
            Box::new([root])
        } else {
            self.merkle_leaves.into_iter().map(Into::into).collect::<Box<[Hash]>>()
        };
        Ok(MerkleVerifier::new(root, hashes)?)
    }
}

impl From<BlobMetadataUnversioned> for BlobMetadata {
    fn from(old: BlobMetadataUnversioned) -> Self {
        if old.compressed_offsets.is_empty() {
            Self { merkle_leaves: old.hashes, format: BlobFormat::Uncompressed }
        } else {
            Self {
                merkle_leaves: old.hashes,
                format: BlobFormat::ChunkedZstd {
                    uncompressed_size: old.uncompressed_size,
                    chunk_size: old.chunk_size,
                    compressed_offsets: old.compressed_offsets,
                },
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, TypeFingerprint)]
pub enum BlobFormatV64 {
    Uncompressed,
    ChunkedZstd {
        uncompressed_size: u64,
        chunk_size: u64,
        compressed_offsets: Vec<u64>,
    },
    /// Every chunk is LZ4 compressed, even if that makes it larger. Only written by older versions.
    ChunkedLz4 {
        uncompressed_size: u64,
        chunk_size: u64,
        compressed_offsets: Vec<u64>,
    },
    /// Chunks that LZ4 can't make smaller are stored raw (uncompressed). `compressed_offsets[i]` is
    /// where chunk `i - 1` ends (0 for chunk 0), and a raw chunk starts at the next 4 KiB boundary
    /// at or after that.
    ChunkedLz4WithRawChunks {
        uncompressed_size: u64,
        chunk_size: u64,
        compressed_offsets: Vec<u64>,
    },
}

impl BlobFormat {
    /// Returns the uncompressed size of the blob, or `None` if the blob is uncompressed.
    pub fn uncompressed_size(&self) -> Option<u64> {
        match self {
            Self::Uncompressed => None,
            Self::ChunkedZstd { uncompressed_size, .. }
            | Self::ChunkedLz4 { uncompressed_size, .. }
            | Self::ChunkedLz4WithRawChunks { uncompressed_size, .. } => Some(*uncompressed_size),
        }
    }

    /// Returns the `CompressionInfo` for a blob stored in this format with `stored_size` bytes of
    /// data, or `None` if the blob is uncompressed.
    pub fn compression_info(
        &self,
        stored_size: u64,
    ) -> Result<Option<CompressionInfo>, ChunkedArchiveError> {
        match self {
            Self::Uncompressed => Ok(None),
            Self::ChunkedZstd { uncompressed_size, chunk_size, compressed_offsets }
            | Self::ChunkedLz4 { uncompressed_size, chunk_size, compressed_offsets } => {
                let algorithm = match self {
                    Self::ChunkedZstd { .. } => CompressionAlgorithm::Zstd,
                    Self::ChunkedLz4 { .. } => CompressionAlgorithm::Lz4,
                    _ => unreachable!(),
                };
                let info =
                    CompressionInfo::new(*chunk_size, stored_size, compressed_offsets, algorithm)?;
                if *uncompressed_size == 0
                    || uncompressed_size.div_ceil(*chunk_size) != compressed_offsets.len() as u64
                {
                    return Err(ChunkedArchiveError::IntegrityError);
                }
                Ok(Some(info))
            }
            Self::ChunkedLz4WithRawChunks { uncompressed_size, chunk_size, compressed_offsets } => {
                Ok(Some(CompressionInfo::new_lz4_with_raw_chunks(
                    *chunk_size,
                    *uncompressed_size,
                    stored_size,
                    compressed_offsets,
                )?))
            }
        }
    }
}

#[derive(Serialize, Deserialize, TypeFingerprint, Migrate)]
#[migrate_nodefault]
pub struct BlobMetadataV53 {
    #[serde(with = "zerocopy_serialization")]
    merkle_leaves: MerkleLeaves,
    format: BlobFormatV53,
}

#[derive(Serialize, Deserialize, TypeFingerprint, Migrate)]
#[migrate_nodefault]
enum BlobFormatV53 {
    Uncompressed,
    ChunkedZstd { uncompressed_size: u64, chunk_size: u64, compressed_offsets: Vec<u64> },
    ChunkedLz4 { uncompressed_size: u64, chunk_size: u64, compressed_offsets: Vec<u64> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_info() {
        assert!(BlobFormat::Uncompressed.compression_info(1234).unwrap().is_none());

        let zstd = BlobFormat::ChunkedZstd {
            uncompressed_size: 65536,
            chunk_size: 32768,
            compressed_offsets: vec![0, 500],
        };
        let info = zstd.compression_info(1000).unwrap().unwrap();
        assert_eq!(info.chunk_size(), 32768);
        assert_eq!(info.compressed_size(), 1000);

        // Zero uncompressed size or mismatched chunk count is rejected for Zstd.
        assert!(matches!(
            BlobFormat::ChunkedZstd {
                uncompressed_size: 0,
                chunk_size: 32768,
                compressed_offsets: vec![0],
            }
            .compression_info(100),
            Err(ChunkedArchiveError::IntegrityError)
        ));
        assert!(matches!(
            BlobFormat::ChunkedZstd {
                uncompressed_size: 100000,
                chunk_size: 32768,
                compressed_offsets: vec![0, 500],
            }
            .compression_info(1000),
            Err(ChunkedArchiveError::IntegrityError)
        ));
        assert!(matches!(
            BlobFormat::ChunkedZstd {
                uncompressed_size: 32768,
                chunk_size: 32768,
                compressed_offsets: vec![0, 500],
            }
            .compression_info(1000),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // Valid and invalid legacy ChunkedLz4.
        let lz4 = BlobFormat::ChunkedLz4 {
            uncompressed_size: 33000,
            chunk_size: 32768,
            compressed_offsets: vec![0, 500],
        };
        let info = lz4.compression_info(1000).unwrap().unwrap();
        assert_eq!(info.chunk_size(), 32768);
        assert_eq!(info.compressed_size(), 1000);

        assert!(matches!(
            BlobFormat::ChunkedLz4 {
                uncompressed_size: 0,
                chunk_size: 32768,
                compressed_offsets: vec![0],
            }
            .compression_info(100),
            Err(ChunkedArchiveError::IntegrityError)
        ));
        assert!(matches!(
            BlobFormat::ChunkedLz4 {
                uncompressed_size: 32768,
                chunk_size: 32768,
                compressed_offsets: vec![0, 500],
            }
            .compression_info(1000),
            Err(ChunkedArchiveError::IntegrityError)
        ));

        // ChunkedLz4WithRawChunks.
        let lz4_raw = BlobFormat::ChunkedLz4WithRawChunks {
            uncompressed_size: 65536,
            chunk_size: 32768,
            compressed_offsets: vec![0, 32768],
        };
        let info = lz4_raw.compression_info(65536).unwrap().unwrap();
        assert_eq!(info.raw_offset_for_uncompressed_range(&(0..65536), 4096), Some(0));
    }
}
