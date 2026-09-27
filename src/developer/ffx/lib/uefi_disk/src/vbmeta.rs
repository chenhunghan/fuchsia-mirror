// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Utilities for signing VBMeta descriptors for custom ZBIs.

pub use assembled_system::vbmeta::FUCHSIA_HASH_DESCRIPTOR_NAME;
use camino::Utf8Path;
use fho::{Result, user_error};
use std::path::Path;
use vbmeta::VBMeta;

// Note: While the bootloader (`bootx64.efi`) and the AVB public key metadata (`atx_metadata.bin`)
// are embedded in the product bundle (`fuchsia.esp.blk` and `fuchsia.vbmeta` respectively), the
// product bundle only contains the public verification key (RSA modulus `n`), not the RSA private
// signing key (`d`) required to compute a new signature after modifying the ZBI with SSH keys and
// a serial number. When custom VBMeta signing keys are not provided, we extract the public key
// metadata directly from the product bundle's `fuchsia.vbmeta` image and sign with the standard
// in-tree AVB development private key (`testkey_atx_psk.pem`).
const DEFAULT_VBMETA_KEY: &str = include_str!(
    "../../../../../../third_party/android/platform/external/avb/test/data/testkey_atx_psk.pem"
);

const AVB_HEADER_SIZE: usize = 256;
const AVB_MAGIC: &[u8; 4] = b"AVB0";

// Byte ranges of the big-endian `u64` header fields that locate the embedded
// `public_key_metadata` blob, per `AvbVBMetaImageHeader` in
// `//third_party/android/platform/external/avb/libavb/avb_vbmeta_image.h`. The blob starts at
// `AVB_HEADER_SIZE + authentication_data_block_size + public_key_metadata_offset`.
const AVB_AUTH_DATA_SIZE_RANGE: std::ops::Range<usize> = 12..20;
const AVB_PKMD_OFFSET_RANGE: std::ops::Range<usize> = 80..88;
const AVB_PKMD_SIZE_RANGE: std::ops::Range<usize> = 88..96;

fn read_be_u64(bytes: &[u8], range: std::ops::Range<usize>) -> usize {
    u64::from_be_bytes(bytes[range].try_into().expect("8-byte slice")) as usize
}

/// Extracts the `public_key_metadata` (`atx_metadata.bin`) byte slice from an existing AVB
/// `.vbmeta` image file in a product bundle.
pub fn extract_public_key_metadata(vbmeta_path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(vbmeta_path).map_err(|e| {
        user_error!("Failed to read product bundle VBMeta at {}: {e}", vbmeta_path.display())
    })?;
    if bytes.len() < AVB_HEADER_SIZE || &bytes[0..4] != AVB_MAGIC {
        return Err(user_error!("Invalid AVB VBMeta header in {}", vbmeta_path.display()));
    }
    let auth_data_size = read_be_u64(&bytes, AVB_AUTH_DATA_SIZE_RANGE);
    let pkmd_offset = read_be_u64(&bytes, AVB_PKMD_OFFSET_RANGE);
    let pkmd_size = read_be_u64(&bytes, AVB_PKMD_SIZE_RANGE);

    let start = AVB_HEADER_SIZE
        .checked_add(auth_data_size)
        .and_then(|v| v.checked_add(pkmd_offset))
        .ok_or_else(|| user_error!("Overflow in VBMeta public_key_metadata offset"))?;
    let end = start
        .checked_add(pkmd_size)
        .ok_or_else(|| user_error!("Overflow in VBMeta public_key_metadata size"))?;

    if pkmd_size == 0 || end > bytes.len() {
        return Err(user_error!(
            "VBMeta image at {} does not contain valid public_key_metadata",
            vbmeta_path.display()
        ));
    }
    Ok(bytes[start..end].to_vec())
}

/// Creates a signed VBMeta image file at `dest` for the given key, key metadata, and ZBI file.
pub fn generate_vbmeta(
    key_path: &Path,
    metadata_path: &Path,
    zbi_path: &Path,
    dest: &Path,
) -> Result<()> {
    let dest_utf8 =
        Utf8Path::from_path(dest).ok_or_else(|| user_error!("Non-UTF8 destination path"))?;
    if dest_utf8.extension() != Some("vbmeta") {
        return Err(user_error!("Destination path must have .vbmeta extension"));
    }
    let outdir = dest_utf8.parent().ok_or_else(|| user_error!("Invalid destination path"))?;
    let file_stem =
        dest_utf8.file_stem().ok_or_else(|| user_error!("Invalid destination file stem"))?;

    let key_path_utf8 =
        Utf8Path::from_path(key_path).ok_or_else(|| user_error!("Non-UTF8 key path"))?;
    let metadata_path_utf8 =
        Utf8Path::from_path(metadata_path).ok_or_else(|| user_error!("Non-UTF8 metadata path"))?;
    let zbi_path_utf8 =
        Utf8Path::from_path(zbi_path).ok_or_else(|| user_error!("Non-UTF8 ZBI path"))?;

    let output_path = VBMeta::builder(file_stem, key_path_utf8)
        .key_metadata(metadata_path_utf8)
        .hash_descriptor(FUCHSIA_HASH_DESCRIPTOR_NAME, zbi_path_utf8)
        .construct(outdir)
        .map_err(|e| user_error!("Failed to generate VBMeta image: {e}"))?;
    assert_eq!(output_path, dest_utf8);
    Ok(())
}

/// Creates a signed VBMeta image file at `dest` for `zbi_path` by extracting the public key
/// metadata (`atx_metadata.bin`) from `orig_vbmeta_path` in the product bundle and signing with
/// the default AVB development private key (`testkey_atx_psk.pem`).
pub fn generate_vbmeta_from_product_bundle(
    orig_vbmeta_path: &Path,
    zbi_path: &Path,
    dest: &Path,
) -> Result<()> {
    let metadata_bytes = extract_public_key_metadata(orig_vbmeta_path)?;
    let temp = tempfile::tempdir()
        .map_err(|e| user_error!("Failed to create temporary directory for VBMeta keys: {e}"))?;
    let key_path = temp.path().join("testkey_atx_psk.pem");
    let metadata_path = temp.path().join("atx_metadata.bin");
    std::fs::write(&key_path, DEFAULT_VBMETA_KEY)
        .map_err(|e| user_error!("Failed to write default VBMeta key: {e}"))?;
    std::fs::write(&metadata_path, &metadata_bytes)
        .map_err(|e| user_error!("Failed to write extracted VBMeta metadata: {e}"))?;
    generate_vbmeta(&key_path, &metadata_path, zbi_path, dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_VBMETA_KEY_METADATA: &[u8] = include_bytes!(
        "../../../../../../third_party/android/platform/external/avb/test/data/atx_metadata.bin"
    );

    #[fuchsia::test]
    fn test_generate_vbmeta_and_extract_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("atx_psk.pem");
        let metadata_path = dir.path().join("avb_atx_metadata.bin");
        let zbi_path = dir.path().join("test.zbi");
        let orig_vbmeta = dir.path().join("orig.vbmeta");
        let re_signed_vbmeta = dir.path().join("zircon.vbmeta");

        std::fs::write(&key_path, DEFAULT_VBMETA_KEY).unwrap();
        std::fs::write(&metadata_path, TEST_VBMETA_KEY_METADATA).unwrap();
        std::fs::write(&zbi_path, b"fake zbi contents").unwrap();

        generate_vbmeta(&key_path, &metadata_path, &zbi_path, &orig_vbmeta).unwrap();
        assert!(orig_vbmeta.exists());

        let extracted = extract_public_key_metadata(&orig_vbmeta).unwrap();
        assert_eq!(extracted, TEST_VBMETA_KEY_METADATA);

        generate_vbmeta_from_product_bundle(&orig_vbmeta, &zbi_path, &re_signed_vbmeta).unwrap();
        assert!(re_signed_vbmeta.exists());
        assert_eq!(
            extract_public_key_metadata(&re_signed_vbmeta).unwrap(),
            TEST_VBMETA_KEY_METADATA
        );
    }

    #[fuchsia::test]
    fn test_generate_vbmeta_invalid_dest() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("atx_psk.pem");
        let metadata_path = dir.path().join("avb_atx_metadata.bin");
        let zbi_path = dir.path().join("test.zbi");

        assert!(generate_vbmeta(&key_path, &metadata_path, &zbi_path, Path::new("/")).is_err());
        assert!(
            generate_vbmeta(&key_path, &metadata_path, &zbi_path, &dir.path().join("zircon.zbi"))
                .is_err()
        );
    }
}
