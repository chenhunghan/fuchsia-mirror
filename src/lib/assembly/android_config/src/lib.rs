// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

//! Library for producing Android-compatible partition images and streaming
//! `ota.zip` archives from a Fuchsia `ProductBundle`.

mod images;
mod ota;

use camino::{Utf8Path, Utf8PathBuf};
use product_bundle::ProductBundle;
use std::collections::BTreeMap;
use thiserror::Error;

pub use images::extract_android_images;
pub use ota::{OtaMetadataSource, create_ota_zip};

/// Android artifacts generated from a Fuchsia `ProductBundle`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AndroidArtifacts {
    /// Path to the unsigned streaming `ota.zip` written to the output directory.
    pub ota_zip: Utf8PathBuf,
    /// Map from Android image filename (for example `"super.img"`, `"boot.img"`)
    /// to its written path inside the output directory.
    pub partition_images: BTreeMap<String, Utf8PathBuf>,
}

/// Errors returned while generating Android artifacts from a `ProductBundle`.
#[derive(Debug, Error)]
pub enum AndroidConfigError {
    /// Failed to create an output directory.
    #[error("Failed to create directory {path}")]
    CreateDir {
        /// Directory path that could not be created.
        path: Utf8PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to read a file on disk.
    #[error("Failed to read file {path}")]
    ReadFile {
        /// File path that could not be read.
        path: Utf8PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to write a file on disk.
    #[error("Failed to write file {path}")]
    WriteFile {
        /// File path that could not be written.
        path: Utf8PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to remove an existing destination file before copying.
    #[error("Failed to remove existing file {path}")]
    RemoveFile {
        /// Destination file path that could not be removed.
        path: Utf8PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to copy or hardlink a partition image.
    #[error("Failed to copy image from {src} to {dest}")]
    CopyImage {
        /// Source image path.
        src: Utf8PathBuf,
        /// Destination image path.
        dest: Utf8PathBuf,
        /// Underlying error.
        #[source]
        source: anyhow::Error,
    },

    /// Failed to map product bundle images to hardware partitions.
    #[error("Failed to map product bundle partitions")]
    PartitionMapping(#[source] anyhow::Error),

    /// The product bundle does not contain an `ota_manifest` in its repositories.
    #[error(
        "Product bundle is missing an OTA manifest in its repositories; ensure TUF keys and update metadata are configured"
    )]
    MissingOtaManifest,

    /// Failed to read or write a ZIP archive.
    #[error("ZIP archive error")]
    Zip(#[from] zip::result::ZipError),

    /// Failed to read an entry from a ZIP archive.
    #[error("Failed to read ZIP entry {entry}")]
    ReadZipEntry {
        /// Name of the entry inside the ZIP archive.
        entry: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to write an entry into a ZIP archive.
    #[error("Failed to write ZIP entry {entry}")]
    WriteZipEntry {
        /// Name of the entry inside the ZIP archive.
        entry: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Base OTA ZIP did not contain `META-INF/com/android/metadata.pb` or `metadata`.
    #[error("Base OTA archive {path} is missing META-INF/com/android/metadata(.pb)")]
    MissingBaseOtaMetadata {
        /// Path to the base OTA archive.
        path: Utf8PathBuf,
    },

    /// Failed to decode protobuf OTA metadata.
    #[error("Failed to decode OTA metadata protobuf")]
    DecodeOtaMetadata(#[from] prost::DecodeError),

    /// OTA property-files metadata exceeded reserved placeholder space or shifted offsets.
    #[error("Invalid OTA property-files layout: {0}")]
    InvalidPropertyFilesLayout(String),

    /// A build property required for the OTA metadata is missing or empty.
    #[error("Android build properties are missing required property {key}")]
    MissingBuildProp {
        /// Name of the missing build property.
        key: String,
    },

    /// A build property required for the OTA metadata has an invalid value.
    #[error("Android build property {key} has invalid value {value:?}")]
    InvalidBuildProp {
        /// Name of the build property.
        key: String,
        /// Invalid value of the build property.
        value: String,
    },
}

/// Generate Android partition images and an unsigned streaming `ota.zip` in
/// `out_dir` from the given `ProductBundle`.
///
/// The Android device and build metadata of the `ota.zip` comes from
/// `metadata_source`. If `otacert` is provided, that certificate file is stored
/// in the `ota.zip` as `META-INF/com/android/otacert`. See [`create_ota_zip`]
/// for how the result can be signed afterwards.
pub fn create_android_artifacts(
    product_bundle: &ProductBundle,
    out_dir: impl AsRef<Utf8Path>,
    metadata_source: OtaMetadataSource<'_>,
    otacert: Option<&Utf8Path>,
) -> Result<AndroidArtifacts, AndroidConfigError> {
    let out_dir = out_dir.as_ref();
    let otacert = otacert
        .map(|path| {
            std::fs::read(path)
                .map_err(|source| AndroidConfigError::ReadFile { path: path.to_path_buf(), source })
        })
        .transpose()?;
    std::fs::create_dir_all(out_dir)
        .map_err(|source| AndroidConfigError::CreateDir { path: out_dir.to_path_buf(), source })?;

    let partition_images = extract_android_images(product_bundle, out_dir)?;
    let ota_zip = out_dir.join("ota.zip");
    create_ota_zip(product_bundle, &ota_zip, metadata_source, otacert.as_deref())?;

    Ok(AndroidArtifacts { ota_zip, partition_images })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembled_system::{BlobfsContents, Image};
    use assembly_partitions_config::{Partition, PartitionsConfig, Slot};
    use product_bundle::{ProductBundleV2, Repository};
    use tempfile::TempDir;

    #[test]
    fn create_android_artifacts_writes_images_and_ota_zip() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let zbi_path = dir.join("in/fuchsia.zbi");
        let fxfs_path = dir.join("in/fxfs.sparse.blk");
        let manifest_path = dir.join("in/ota_manifest");
        std::fs::create_dir_all(dir.join("in")).unwrap();
        std::fs::write(&zbi_path, b"boot-zbi").unwrap();
        std::fs::write(&fxfs_path, b"fxfs-super").unwrap();
        std::fs::write(&manifest_path, b"{\"version\":1}").unwrap();

        let pb = ProductBundle::V2(ProductBundleV2 {
            product_name: "test_product.test_board".to_string(),
            product_version: "1.0.0".to_string(),
            partitions: PartitionsConfig {
                hardware_revision: "test_board".to_string(),
                partitions: vec![
                    Partition::ZBI { name: "boot_a".to_string(), slot: Slot::A, size: None },
                    Partition::Fxfs { name: "super".to_string(), size: None },
                ],
                ..Default::default()
            },
            sdk_version: "sdk".to_string(),
            system_a: Some(vec![
                Image::ZBI { path: zbi_path, signed: true },
                Image::FxfsSparse { path: fxfs_path, contents: BlobfsContents::default() },
            ]),
            platform_tools_a: vec![],
            system_b: None,
            platform_tools_b: vec![],
            system_r: None,
            platform_tools_r: vec![],
            release_info: None,
            repositories: vec![Repository {
                name: "fuchsia.com".to_string(),
                metadata_path: dir.join("repo"),
                blobs_path: dir.join("blobs"),
                delivery_blob_type: 1,
                root_private_key_path: None,
                targets_private_key_path: None,
                snapshot_private_key_path: None,
                timestamp_private_key_path: None,
                ota_manifest_signature_path: None,
                ota_manifest_path: Some(manifest_path),
            }],
            update_package_hash: None,
            virtual_devices_path: None,
        });

        let out_dir = dir.join("android_out");
        let build_prop = dir.join("in/build.prop");
        std::fs::write(
            &build_prop,
            "ro.product.device=test_board\n\
             ro.build.fingerprint=vendor/test_product/test_board:16/MAIN/1:userdebug/dev-keys\n\
             ro.build.version.incremental=1\n\
             ro.build.version.sdk=36\n\
             ro.build.version.security_patch=2025-11-05\n\
             ro.build.date.utc=1759863069\n",
        )
        .unwrap();
        let build_props = [build_prop];
        let artifacts = create_android_artifacts(
            &pb,
            &out_dir,
            OtaMetadataSource::BuildProps(&build_props),
            None,
        )
        .unwrap();
        assert!(artifacts.ota_zip.is_file());
        assert_eq!(artifacts.partition_images.len(), 2);
        assert_eq!(std::fs::read(&artifacts.partition_images["boot.img"]).unwrap(), b"boot-zbi");
        assert_eq!(std::fs::read(&artifacts.partition_images["super.img"]).unwrap(), b"fxfs-super");
    }
}
