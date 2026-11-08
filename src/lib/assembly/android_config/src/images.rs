// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Extraction of Android fastboot partition images from a Fuchsia `ProductBundle`.

use crate::AndroidConfigError;
use assembled_system::Image;
use assembly_partitions_config::{Partition, PartitionImageMapper, Slot};
use assembly_util::fast_copy;
use camino::{Utf8Path, Utf8PathBuf};
use product_bundle::ProductBundle;
use std::collections::BTreeMap;

/// Partition name used by non-Android Fuchsia boards for the primary ZBI.
const ZIRCON_PARTITION_BASE_NAME: &str = "zircon";

/// Android fastboot image name used for the primary Fuchsia volume (Fxfs or FVM).
const SUPER_IMAGE_FILENAME: &str = "super.img";

/// Android fastboot image name used for system VBMeta.
const VBMETA_SYSTEM_BASE_NAME: &str = "vbmeta_system";

/// Standard AOSP fastboot partition base names that are extracted from
/// `bootloader_partitions` when a board declares them.
const ANDROID_BOOTLOADER_FASTBOOT_PARTITIONS: &[&str] = &[
    "boot",
    "vendor_boot",
    "init_boot",
    "vendor_kernel_boot",
    "dtbo",
    "vbmeta",
    "vbmeta_system",
    "vbmeta_vendor",
    "pvmfw",
];

/// Extract Android-compatible `.img` partition files from `product_bundle` into
/// `out_dir`, returning a map from output filename (e.g. `"super.img"`) to its
/// written path.
pub fn extract_android_images(
    product_bundle: &ProductBundle,
    out_dir: impl AsRef<Utf8Path>,
) -> Result<BTreeMap<String, Utf8PathBuf>, AndroidConfigError> {
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)
        .map_err(|source| AndroidConfigError::CreateDir { path: out_dir.to_path_buf(), source })?;

    let ProductBundle::V2(pb) = product_bundle;

    let mut mapper = PartitionImageMapper::new(pb.partitions.clone())
        .map_err(AndroidConfigError::PartitionMapping)?;
    if let Some(system_a) = &pb.system_a {
        mapper
            .map_images_to_slot(system_a, Slot::A)
            .map_err(AndroidConfigError::PartitionMapping)?;
    }
    if let Some(system_b) = &pb.system_b {
        mapper
            .map_images_to_slot(system_b, Slot::B)
            .map_err(AndroidConfigError::PartitionMapping)?;
    }
    if let Some(system_r) = &pb.system_r {
        mapper
            .map_images_to_slot(system_r, Slot::R)
            .map_err(AndroidConfigError::PartitionMapping)?;
    }

    let has_zircon_zbi = pb.partitions.partitions.iter().any(|partition| match partition {
        Partition::ZBI { name, slot: Slot::A, .. } => {
            strip_slot_a_suffix(name) == ZIRCON_PARTITION_BASE_NAME
        }
        _ => false,
    });

    let mut source_by_filename: BTreeMap<String, Utf8PathBuf> = BTreeMap::new();

    for mapping in mapper.map() {
        match &mapping.partition {
            Partition::Fxfs { .. } | Partition::FVM { .. } => {
                source_by_filename.entry(SUPER_IMAGE_FILENAME.to_string()).or_insert(mapping.path);
            }
            Partition::ZBI { name, slot: Slot::A, .. }
            | Partition::RecoveryZBI { name, slot: Slot::A, .. } => {
                let base = strip_slot_a_suffix(name);
                if base != ZIRCON_PARTITION_BASE_NAME {
                    source_by_filename.insert(format!("{base}.img"), mapping.path);
                }
            }
            Partition::Dtbo { name, slot: Slot::A, .. } => {
                let base = strip_slot_a_suffix(name);
                source_by_filename.insert(format!("{base}.img"), mapping.path);
            }
            Partition::VBMeta { name, slot: Slot::A, .. }
            | Partition::RecoveryVBMeta { name, slot: Slot::A, .. } => {
                let base = strip_slot_a_suffix(name);
                if base == VBMETA_SYSTEM_BASE_NAME || !has_zircon_zbi {
                    source_by_filename.insert(format!("{base}.img"), mapping.path);
                }
            }
            _ => {}
        }
    }

    // Fallback: if the board's partitions config did not map a `Fxfs` or `FVM`
    // partition (or only an unmapped image exists in `system_a`), check `system_a`
    // directly for a sparse/fastboot/full filesystem image to expose as `super.img`.
    if !source_by_filename.contains_key(SUPER_IMAGE_FILENAME)
        && let Some(system_a) = &pb.system_a
        && let Some(storage_path) = find_fallback_super_image(system_a)
    {
        source_by_filename.insert(SUPER_IMAGE_FILENAME.to_string(), storage_path);
    }

    // Also inspect `system_a` for an explicit `VBMetaSystem` image if not already mapped.
    if !source_by_filename.contains_key("vbmeta_system.img")
        && let Some(system_a) = &pb.system_a
    {
        for image in system_a {
            if let Image::VBMetaSystem(path) = image {
                source_by_filename.insert("vbmeta_system.img".to_string(), path.clone());
                break;
            }
        }
    }

    // Map standard Android fastboot partitions that a board carries as
    // `bootloader_partitions` (e.g. `vbmeta`, `dtbo`, `init_boot`, `pvmfw`),
    // without overriding images assembled into slot A.
    for bootloader in &pb.partitions.bootloader_partitions {
        let Some(candidate_base) =
            android_bootloader_base_name(&bootloader.partition_type, bootloader.name.as_deref())
        else {
            continue;
        };
        let filename = format!("{candidate_base}.img");
        source_by_filename.entry(filename).or_insert_with(|| bootloader.image.clone());
    }

    let mut written_images = BTreeMap::new();
    for (filename, src_path) in source_by_filename {
        let dest_path = out_dir.join(&filename);
        copy_replacing_existing(&src_path, &dest_path)?;
        written_images.insert(filename, dest_path);
    }

    Ok(written_images)
}

fn strip_slot_a_suffix(name: &str) -> &str {
    name.strip_suffix("_a").unwrap_or(name)
}

fn find_fallback_super_image(images: &[Image]) -> Option<Utf8PathBuf> {
    for image in images {
        if let Image::FxfsSparse { path, .. } = image {
            return Some(path.clone());
        }
    }
    for image in images {
        if let Image::Fxfs(path) = image {
            return Some(path.clone());
        }
    }
    for image in images {
        if let Image::FVMFastboot(path) = image {
            return Some(path.clone());
        }
    }
    for image in images {
        if let Image::FVMSparse(path) = image {
            return Some(path.clone());
        }
    }
    for image in images {
        if let Image::FVM(path) = image {
            return Some(path.clone());
        }
    }
    None
}

fn android_bootloader_base_name<'a>(
    partition_type: &'a str,
    fastboot_name: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(name) = fastboot_name {
        // Skip explicit B-slot or R-slot bootloader entries if any exist; we want
        // unslotted or A-slot fastboot partition images.
        if name.ends_with("_b") || name.ends_with("_r") {
            return None;
        }
        let base = strip_slot_a_suffix(name);
        if ANDROID_BOOTLOADER_FASTBOOT_PARTITIONS.contains(&base) {
            return Some(base);
        }
    }

    let type_base = strip_slot_a_suffix(partition_type);
    if ANDROID_BOOTLOADER_FASTBOOT_PARTITIONS.contains(&type_base) {
        // If `fastboot_name` was `None`, only include unflashable firmware entries
        // when their `type` itself names an Android fastboot image.
        return Some(type_base);
    }

    None
}

fn copy_replacing_existing(src: &Utf8Path, dest: &Utf8Path) -> Result<(), AndroidConfigError> {
    if dest.exists() {
        std::fs::remove_file(dest).map_err(|source| AndroidConfigError::RemoveFile {
            path: dest.to_path_buf(),
            source,
        })?;
    }
    fast_copy(src, dest).map_err(|source| AndroidConfigError::CopyImage {
        src: src.to_path_buf(),
        dest: dest.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembled_system::BlobfsContents;
    use assembly_partitions_config::{BootloaderPartition, PartitionsConfig};
    use product_bundle::ProductBundleV2;
    use tempfile::TempDir;

    fn write_dummy_file(dir: &Utf8Path, rel: &str, contents: &[u8]) -> Utf8PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn empty_product_bundle(partitions: PartitionsConfig) -> ProductBundleV2 {
        ProductBundleV2 {
            product_name: "test.board".to_string(),
            product_version: "1.2.3".to_string(),
            partitions,
            sdk_version: "sdk".to_string(),
            system_a: None,
            platform_tools_a: vec![],
            system_b: None,
            platform_tools_b: vec![],
            system_r: None,
            platform_tools_r: vec![],
            release_info: None,
            repositories: vec![],
            update_package_hash: None,
            virtual_devices_path: None,
        }
    }

    #[test]
    fn extract_ab_recovery_and_filtered_bootloaders() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let boot_zbi = write_dummy_file(dir, "in/system_a/fuchsia.zbi", b"boot-zbi");
        let vbmeta_sys =
            write_dummy_file(dir, "in/system_a/vbmeta_system.vbmeta", b"vbmeta-system");
        let fxfs_sparse = write_dummy_file(dir, "in/system_a/fxfs.sparse.blk", b"fxfs");
        let recovery_zbi =
            write_dummy_file(dir, "in/system_r/recovery.zbi", b"vendor-boot-recovery");
        let bl_vbmeta = write_dummy_file(dir, "in/partitions/vbmeta.img", b"android-vbmeta");
        let bl_dtbo = write_dummy_file(dir, "in/partitions/dtbo.img", b"android-dtbo");
        let bl_abl = write_dummy_file(dir, "in/partitions/abl.elf", b"firmware-abl");
        let bl_bl2 = write_dummy_file(dir, "in/partitions/bl2.img", b"firmware-bl2");

        let partitions = PartitionsConfig {
            hardware_revision: "test_board".to_string(),
            bootloader_partitions: vec![
                BootloaderPartition {
                    partition_type: "vbmeta".to_string(),
                    name: Some("vbmeta_a".to_string()),
                    image: bl_vbmeta,
                },
                BootloaderPartition {
                    partition_type: "dtbo".to_string(),
                    name: Some("dtbo_a".to_string()),
                    image: bl_dtbo,
                },
                BootloaderPartition {
                    partition_type: "abl".to_string(),
                    name: Some("abl_a".to_string()),
                    image: bl_abl,
                },
                BootloaderPartition {
                    partition_type: "bl2".to_string(),
                    name: None,
                    image: bl_bl2,
                },
            ],
            partitions: vec![
                Partition::ZBI { name: "boot_a".to_string(), slot: Slot::A, size: None },
                Partition::ZBI { name: "boot_b".to_string(), slot: Slot::B, size: None },
                Partition::RecoveryZBI {
                    name: "vendor_boot_a".to_string(),
                    slot: Slot::A,
                    size: None,
                },
                Partition::RecoveryZBI {
                    name: "vendor_boot_b".to_string(),
                    slot: Slot::B,
                    size: None,
                },
                Partition::VBMeta {
                    name: "vbmeta_system_a".to_string(),
                    slot: Slot::A,
                    size: None,
                },
                Partition::VBMeta {
                    name: "vbmeta_system_b".to_string(),
                    slot: Slot::B,
                    size: None,
                },
                Partition::Fxfs { name: "super".to_string(), size: None },
            ],
            ..Default::default()
        };

        let mut pb = empty_product_bundle(partitions);
        pb.system_a = Some(vec![
            Image::ZBI { path: boot_zbi, signed: true },
            Image::VBMetaSystem(vbmeta_sys),
            Image::FxfsSparse { path: fxfs_sparse, contents: BlobfsContents::default() },
        ]);
        pb.system_r = Some(vec![Image::ZBI { path: recovery_zbi, signed: true }]);

        let out_dir = dir.join("out");
        let extracted = extract_android_images(&ProductBundle::V2(pb), &out_dir).unwrap();

        let keys: Vec<&str> = extracted.keys().map(String::as_str).collect();
        pretty_assertions::assert_eq!(
            keys,
            vec![
                "boot.img",
                "dtbo.img",
                "super.img",
                "vbmeta.img",
                "vbmeta_system.img",
                "vendor_boot.img"
            ]
        );
        pretty_assertions::assert_eq!(std::fs::read(&extracted["boot.img"]).unwrap(), b"boot-zbi");
        pretty_assertions::assert_eq!(
            std::fs::read(&extracted["vendor_boot.img"]).unwrap(),
            b"vendor-boot-recovery"
        );
        pretty_assertions::assert_eq!(
            std::fs::read(&extracted["vbmeta_system.img"]).unwrap(),
            b"vbmeta-system"
        );
        pretty_assertions::assert_eq!(std::fs::read(&extracted["super.img"]).unwrap(), b"fxfs");
        pretty_assertions::assert_eq!(
            std::fs::read(&extracted["vbmeta.img"]).unwrap(),
            b"android-vbmeta"
        );
        pretty_assertions::assert_eq!(
            std::fs::read(&extracted["dtbo.img"]).unwrap(),
            b"android-dtbo"
        );
    }

    #[test]
    fn extract_android_fastboot_bootloader_partitions() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let boot_zbi = write_dummy_file(dir, "in/system_a/fuchsia.zbi", b"boot");
        let dtbo_a = write_dummy_file(dir, "in/system_a/dtbo", b"assembled-dtbo");
        let fxfs_sparse = write_dummy_file(dir, "in/system_a/fxfs.sparse.blk", b"fxfs");
        let init_boot = write_dummy_file(dir, "in/partitions/init_boot.img", b"init-boot");
        let vendor_kernel_boot =
            write_dummy_file(dir, "in/partitions/vendor_kernel_boot.img", b"vk-boot");
        let vbmeta_vendor =
            write_dummy_file(dir, "in/partitions/vbmeta_vendor.img", b"vbmeta-vendor");
        let pvmfw = write_dummy_file(dir, "in/partitions/pvmfw.img", b"pvmfw");
        let bootloader_dtbo =
            write_dummy_file(dir, "in/partitions/bootloader_dtbo.img", b"should-not-override");

        let partitions = PartitionsConfig {
            hardware_revision: "test_board".to_string(),
            bootloader_partitions: vec![
                BootloaderPartition {
                    partition_type: "init_boot".to_string(),
                    name: Some("init_boot_a".to_string()),
                    image: init_boot,
                },
                BootloaderPartition {
                    partition_type: "vendor_kernel_boot".to_string(),
                    name: Some("vendor_kernel_boot_a".to_string()),
                    image: vendor_kernel_boot,
                },
                BootloaderPartition {
                    partition_type: "vbmeta_vendor".to_string(),
                    name: Some("vbmeta_vendor_a".to_string()),
                    image: vbmeta_vendor,
                },
                BootloaderPartition {
                    partition_type: "pvmfw".to_string(),
                    name: Some("pvmfw".to_string()),
                    image: pvmfw,
                },
                BootloaderPartition {
                    partition_type: "dtbo".to_string(),
                    name: Some("dtbo_a".to_string()),
                    image: bootloader_dtbo,
                },
            ],
            partitions: vec![
                Partition::ZBI { name: "boot_a".to_string(), slot: Slot::A, size: None },
                Partition::Dtbo { name: "dtbo_a".to_string(), slot: Slot::A, size: None },
                Partition::Fxfs { name: "super".to_string(), size: None },
            ],
            ..Default::default()
        };

        let mut pb = empty_product_bundle(partitions);
        pb.system_a = Some(vec![
            Image::ZBI { path: boot_zbi, signed: true },
            Image::Dtbo(dtbo_a),
            Image::FxfsSparse { path: fxfs_sparse, contents: BlobfsContents::default() },
        ]);

        let out_dir = dir.join("out");
        let extracted = extract_android_images(&ProductBundle::V2(pb), &out_dir).unwrap();

        let keys: Vec<&str> = extracted.keys().map(String::as_str).collect();
        pretty_assertions::assert_eq!(
            keys,
            vec![
                "boot.img",
                "dtbo.img",
                "init_boot.img",
                "pvmfw.img",
                "super.img",
                "vbmeta_vendor.img",
                "vendor_kernel_boot.img",
            ]
        );
        // Assembled slot-A dtbo takes precedence over bootloader_partitions dtbo.
        pretty_assertions::assert_eq!(
            std::fs::read(&extracted["dtbo.img"]).unwrap(),
            b"assembled-dtbo"
        );
        pretty_assertions::assert_eq!(std::fs::read(&extracted["pvmfw.img"]).unwrap(), b"pvmfw");
    }

    #[test]
    fn extract_x64_maps_super_without_clobbering_android_vbmeta() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let zircon_zbi = write_dummy_file(dir, "in/system_a/fuchsia.zbi", b"x64-zircon");
        let zircon_vbmeta = write_dummy_file(dir, "in/system_a/fuchsia.vbmeta", b"x64-vbmeta");
        let fvm_fastboot = write_dummy_file(dir, "in/system_a/fvm.fastboot.blk", b"x64-fvm");

        let partitions = PartitionsConfig {
            hardware_revision: "x64".to_string(),
            partitions: vec![
                Partition::ZBI { name: "zircon_a".to_string(), slot: Slot::A, size: None },
                Partition::VBMeta { name: "vbmeta_a".to_string(), slot: Slot::A, size: None },
                Partition::FVM { name: "fvm".to_string(), size: None },
            ],
            ..Default::default()
        };

        let mut pb = empty_product_bundle(partitions);
        pb.system_a = Some(vec![
            Image::ZBI { path: zircon_zbi, signed: false },
            Image::VBMeta(zircon_vbmeta),
            Image::FVMFastboot(fvm_fastboot),
        ]);

        let out_dir = dir.join("out");
        let extracted = extract_android_images(&ProductBundle::V2(pb), &out_dir).unwrap();

        let keys: Vec<&str> = extracted.keys().map(String::as_str).collect();
        pretty_assertions::assert_eq!(keys, vec!["super.img"]);
        pretty_assertions::assert_eq!(std::fs::read(&extracted["super.img"]).unwrap(), b"x64-fvm");
    }
}
