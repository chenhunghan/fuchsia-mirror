// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Utilities for GCE disk image packaging, serial number generation, and product bundle hashing.

use crate::error::{GceError, IoContext as _, Result};
use camino::Utf8Path;
use discovery::gce_watcher::write_file_atomically;
use flate2::Compression;
use flate2::write::GzEncoder;
use nix::sys::statvfs::statvfs;
use product_bundle::ProductBundle;
use rand::RngExt as _;
use sdk_metadata::CpuArchitecture;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Default disk size for GCE full disk images (20 GiB, matching `DEFAULT_EMU_DISK_SIZE`).
pub const DEFAULT_GCE_DISK_SIZE: u64 = ffx_uefi_disk::DEFAULT_EMU_DISK_SIZE;

/// Length of the hexadecimal prefix returned by [`compute_bundle_hash`].
pub const BUNDLE_HASH_HEX_LEN: usize = 12;

/// Approximate scratch space needed to synthesize a GCE image: the raw disk plus its archive.
const SCRATCH_SPACE_BYTES: u64 = DEFAULT_GCE_DISK_SIZE + 3 * (1 << 30);

/// 48-bit mask for the final node identifier segment of a generated serial number.
const SERIAL_NODE_MASK: u64 = 0xffff_ffff_ffff;

/// Buffer size (64 KiB) used when streaming files into the SHA-256 hasher.
const HASH_BUFFER_SIZE: usize = 64 * 1024;

/// Paths to a custom AVB signing key and its matching public key metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VbmetaKeys {
    pub key: PathBuf,
    pub metadata: PathBuf,
}

fn hash_file(path: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut file = File::open(path).io_context(|| format!("Failed to open {}", path.display()))?;
    let mut buffer = [0u8; HASH_BUFFER_SIZE];
    loop {
        let bytes_read =
            file.read(&mut buffer).io_context(|| format!("Failed to read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(())
}

/// Reinterprets `path` as a UTF-8 path, which the product bundle APIs require.
fn utf8_path(path: &Path) -> Result<&Utf8Path> {
    Utf8Path::from_path(path)
        .ok_or_else(|| GceError::NonUtf8Path { path: path.to_string_lossy().into_owned() })
}

/// Loads the v2 product bundle rooted at `path`.
fn load_product_bundle(path: &Utf8Path) -> Result<product_bundle::ProductBundleV2> {
    let ProductBundle::V2(pb) = ProductBundle::try_load_from(path)
        .map_err(|source| GceError::ProductBundle { path: path.to_string(), source })?;
    Ok(pb)
}

/// Generates a random GCE instance name (`fuchsia-gce-<8 hex chars>`).
pub fn generate_instance_name() -> String {
    let mut rng = rand::rng();
    format!("fuchsia-gce-{:08x}", rng.random::<u32>())
}

/// Generates a random serial number formatted for GCE instances (`GC-...`).
pub fn generate_serial_number() -> String {
    let mut rng = rand::rng();
    format!(
        "GC-{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        rng.random::<u32>(),
        rng.random::<u16>(),
        rng.random::<u16>(),
        rng.random::<u16>(),
        rng.random::<u64>() & SERIAL_NODE_MASK
    )
}

/// Computes a deterministic content hash of a product bundle and everything else baked into the
/// synthesized disk image, to serve as a GCE image tag.
///
/// The SSH public key and any custom VBMeta signing keys must be included, because they are
/// embedded by [`prepare_gce_disk_archive`]; otherwise changing them would silently reuse a
/// previously registered image.
pub fn compute_bundle_hash(
    product_bundle_path: &Path,
    ssh_pubkey: &str,
    vbmeta_keys: Option<&VbmetaKeys>,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(ssh_pubkey.as_bytes());
    if let Some(keys) = vbmeta_keys {
        hash_file(&keys.key, &mut hasher)?;
        hash_file(&keys.metadata, &mut hasher)?;
    }
    let manifest_path = product_bundle_path.join("product_bundle.json");
    if manifest_path.exists() {
        hash_file(&manifest_path, &mut hasher)?;
        let pb = load_product_bundle(utf8_path(product_bundle_path)?)?;
        if let Some(system_a) = &pb.system_a {
            for image in system_a {
                let path = image.source().as_std_path();
                if path.exists() {
                    hash_file(path, &mut hasher)?;
                }
            }
        }
        for part in &pb.partitions.bootloader_partitions {
            let path = part.image.as_std_path();
            if path.exists() {
                hash_file(path, &mut hasher)?;
            }
        }
    } else {
        hasher.update(product_bundle_path.to_string_lossy().as_bytes());
    }
    let result = hasher.finalize();
    let mut hex_str = String::with_capacity(BUNDLE_HASH_HEX_LEN);
    for &b in &result[..BUNDLE_HASH_HEX_LEN / 2] {
        let _ = write!(hex_str, "{:02x}", b);
    }
    Ok(hex_str)
}

/// Packages a raw disk file into a GCE-compatible `.tar.gz` archive containing `disk.raw`.
pub fn package_gce_tar_gz(raw_disk_path: &Path, output_tar_gz: &Path) -> Result<()> {
    let mut disk_file = File::open(raw_disk_path)
        .io_context(|| format!("Failed to open raw disk file at {}", raw_disk_path.display()))?;

    write_file_atomically(output_tar_gz, |writer| -> Result<()> {
        let enc = GzEncoder::new(writer, Compression::fast());
        let mut tar = tar::Builder::new(enc);
        tar.append_file("disk.raw", &mut disk_file)
            .io_context(|| format!("Failed to append disk.raw to {}", output_tar_gz.display()))?;
        let enc = tar
            .into_inner()
            .io_context(|| format!("Failed to finalize {}", output_tar_gz.display()))?;
        enc.finish().io_context(|| format!("Failed to compress {}", output_tar_gz.display()))?;
        Ok(())
    })
}

/// Returns the path of the system image in `pb` whose file name ends in `extension`.
fn find_system_image<'a>(
    pb: &'a product_bundle::ProductBundleV2,
    pb_path: &Utf8Path,
    extension: &str,
) -> Result<&'a Path> {
    pb.system_a
        .as_ref()
        .and_then(|imgs| {
            imgs.iter()
                .map(|img| img.source().as_std_path())
                .find(|path| path.extension() == Some(std::ffi::OsStr::new(extension)))
        })
        .ok_or_else(|| GceError::MissingSystemImage {
            extension: extension.to_string(),
            path: pb_path.to_string(),
        })
}

/// Returns the CPU architecture that the product bundle at `product_bundle_path` boots.
///
/// The disk image, the GCE image registration, and the VM's machine type all have to agree on
/// the architecture, so callers resolve it once from the bundle's ZBI.
pub fn product_bundle_architecture(product_bundle_path: &Path) -> Result<CpuArchitecture> {
    let utf8_pb = utf8_path(product_bundle_path)?;
    let pb = load_product_bundle(utf8_pb)?;
    let zbi = find_system_image(&pb, utf8_pb, "zbi")?;
    zbi_architecture(zbi)
}

/// Returns the CPU architecture encoded in the ZBI at `zbi`.
fn zbi_architecture(zbi: &Path) -> Result<CpuArchitecture> {
    ffx_uefi_disk::zbi_architecture(zbi)
        .map_err(|e| GceError::dependency("Failed to determine product bundle architecture", e))
}

/// The GCE VM settings implied by a guest architecture.
///
/// GCE only boots an image on a CPU matching the image's architecture, so image registration,
/// machine type, and network interface all have to be derived from the same architecture. Keeping
/// them in one table means adding an architecture is a single, exhaustively-checked change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GceVmShape {
    /// Value of the GCE image `architecture` field.
    pub image_architecture: &'static str,
    /// Machine type used when the caller does not choose one.
    pub default_machine_type: &'static str,
    /// Network interface type.
    pub nic_type: &'static str,
}

/// Returns the GCE VM settings for `arch`, or an error if GCE cannot host it.
pub fn gce_vm_shape(arch: CpuArchitecture) -> Result<GceVmShape> {
    match arch {
        // N2 is Intel; VirtIO keeps the widest machine type compatibility.
        CpuArchitecture::X64 => Ok(GceVmShape {
            image_architecture: "X86_64",
            default_machine_type: "n2-standard-4",
            nic_type: "VIRTIO_NET",
        }),
        // T2A is Ampere Altra, which only supports gVNIC.
        CpuArchitecture::Arm64 => Ok(GceVmShape {
            image_architecture: "ARM64",
            default_machine_type: "t2a-standard-4",
            nic_type: "GVNIC",
        }),
        CpuArchitecture::Riscv64 | CpuArchitecture::Unsupported => {
            Err(GceError::UnsupportedArchitecture { arch })
        }
    }
}

/// Synthesizes a UEFI GPT full disk image from `product_bundle_path`, embeds SSH `authorized_keys`
/// from `ctx` and a newly generated `GC-...` serial number into the ZBI, re-signs VBMeta (using
/// `vbmeta_keys` if provided, or the public key metadata extracted from the product bundle's
/// `fuchsia.vbmeta` with the default AVB test private key otherwise), and packages the resulting
/// disk image into `output_tar_gz`.
///
/// Returns the generated `GC-...` serial number embedded in the image.
pub fn prepare_gce_disk_archive(
    ctx: &ffx_config::EnvironmentContext,
    product_bundle_path: &Path,
    vbmeta_keys: Option<&VbmetaKeys>,
    output_tar_gz: &Path,
) -> Result<String> {
    let utf8_pb = utf8_path(product_bundle_path)?;
    let pb = load_product_bundle(utf8_pb)?;

    let src_zbi = find_system_image(&pb, utf8_pb, "zbi")?;
    let src_vbmeta = find_system_image(&pb, utf8_pb, "vbmeta")?;

    // The EFI bootloader staged in the ESP is architecture specific.
    let arch = zbi_architecture(src_zbi)?;
    gce_vm_shape(arch)?;

    let serial = generate_serial_number();
    let work_dir = tempfile::tempdir()
        .io_context(|| "Failed to create temporary disk workspace".to_string())?;
    ensure_scratch_space(work_dir.path())?;

    let modified_zbi = work_dir.path().join("fuchsia.zbi");
    ffx_uefi_disk::embed_boot_data(ctx, src_zbi, &modified_zbi, None, Some(&serial))
        .map_err(|e| GceError::dependency("Failed to embed boot data into ZBI", e))?;

    let modified_vbmeta = work_dir.path().join("fuchsia.vbmeta");
    if let Some(keys) = vbmeta_keys {
        ffx_uefi_disk::generate_vbmeta(&keys.key, &keys.metadata, &modified_zbi, &modified_vbmeta)
            .map_err(|e| GceError::dependency("Failed to generate VBMeta with custom keys", e))?;
    } else {
        ffx_uefi_disk::generate_vbmeta_from_product_bundle(
            src_vbmeta,
            &modified_zbi,
            &modified_vbmeta,
        )
        .map_err(|e| GceError::dependency("Failed to generate VBMeta from product bundle", e))?;
    }

    let cmdline_path = work_dir.path().join("zedboot_cmdline");
    ffx_uefi_disk::write_zedboot_cmdline(&cmdline_path, None)
        .map_err(|e| GceError::dependency("Failed to write zedboot cmdline", e))?;

    let raw_disk_path = work_dir.path().join("disk.raw");
    ffx_uefi_disk::FuchsiaFullDiskImageBuilder::new()
        .arch(arch)
        .product_bundle(product_bundle_path)
        .output_path(&raw_disk_path)
        .cmdline(&cmdline_path)
        .zbi(Some(modified_zbi))
        .vbmeta(Some(modified_vbmeta))
        .resize(DEFAULT_GCE_DISK_SIZE)
        .build(ctx)
        .map_err(|e| GceError::dependency("Failed to assemble GCE UEFI GPT disk image", e))
        .map_err(add_scratch_space_hint)?;

    package_gce_tar_gz(&raw_disk_path, output_tar_gz).map_err(add_scratch_space_hint)?;
    Ok(serial)
}

/// Fails if `dir` does not have enough free space to synthesize a GCE disk image.
///
/// The raw disk and its compressed archive are both written under `TMPDIR`, which is frequently
/// a small `tmpfs`, so checking up front turns a confusing mid-build failure into an actionable
/// one before tens of GiB of work are done.
fn ensure_scratch_space(dir: &Path) -> Result<()> {
    let stat = statvfs(dir).map_err(|e| {
        GceError::dependency(format!("Failed to query free space in {}", dir.display()), e)
    })?;
    let available = stat.blocks_available() as u64 * stat.fragment_size() as u64;
    if available < SCRATCH_SPACE_BYTES {
        return Err(GceError::InsufficientScratchSpace {
            dir: dir.display().to_string(),
            needed_gib: SCRATCH_SPACE_BYTES >> 30,
            available_gib: available >> 30,
        });
    }
    Ok(())
}

/// Explains how to recover if the scratch filesystem filled up after [`ensure_scratch_space`] ran.
fn add_scratch_space_hint(err: GceError) -> GceError {
    if err.io_cause().map(|io| io.kind()) != Some(std::io::ErrorKind::StorageFull) {
        return err;
    }
    GceError::OutOfScratchSpace {
        tmpdir: std::env::temp_dir().display().to_string(),
        needed_gib: SCRATCH_SPACE_BYTES >> 30,
        source: Box::new(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_gce_vm_shape() {
        let x64 = gce_vm_shape(CpuArchitecture::X64).unwrap();
        assert_eq!(x64.image_architecture, "X86_64");
        assert_eq!(x64.default_machine_type, "n2-standard-4");
        assert_eq!(x64.nic_type, "VIRTIO_NET");

        let arm64 = gce_vm_shape(CpuArchitecture::Arm64).unwrap();
        assert_eq!(arm64.image_architecture, "ARM64");
        assert_eq!(arm64.default_machine_type, "t2a-standard-4");
        assert_eq!(arm64.nic_type, "GVNIC");

        assert!(gce_vm_shape(CpuArchitecture::Riscv64).is_err());
        assert!(gce_vm_shape(CpuArchitecture::Unsupported).is_err());
    }

    #[fuchsia::test]
    fn test_add_scratch_space_hint() {
        let full = GceError::Io {
            context: "Failed to write disk.raw".to_string(),
            source: std::io::Error::from(std::io::ErrorKind::StorageFull),
        };
        assert!(add_scratch_space_hint(full).to_string().contains("TMPDIR"));

        // Unrelated IO failures must not be mislabeled as an out-of-space condition.
        let other = GceError::Io {
            context: "Failed to write disk.raw".to_string(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert!(!add_scratch_space_hint(other).to_string().contains("TMPDIR"));
    }

    #[fuchsia::test]
    fn test_generate_instance_name() {
        let n1 = generate_instance_name();
        let n2 = generate_instance_name();
        assert!(n1.starts_with("fuchsia-gce-"));
        assert!(n2.starts_with("fuchsia-gce-"));
        assert_eq!(n1.len(), "fuchsia-gce-12345678".len());
        assert_ne!(n1, n2);
    }

    #[fuchsia::test]
    fn test_generate_serial_number() {
        let s1 = generate_serial_number();
        let s2 = generate_serial_number();
        assert!(s1.starts_with("GC-"));
        assert!(s2.starts_with("GC-"));
        assert_ne!(s1, s2);
    }

    #[fuchsia::test]
    fn test_compute_bundle_hash_determinism() {
        let temp = tempfile::tempdir().unwrap();
        const KEY: &str = "ssh-ed25519 AAAAC3... test_key";
        let hash1 = compute_bundle_hash(temp.path(), KEY, None).unwrap();
        let hash2 = compute_bundle_hash(temp.path(), KEY, None).unwrap();
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), BUNDLE_HASH_HEX_LEN);

        let other_key = compute_bundle_hash(temp.path(), "ssh-ed25519 AAAAC3... other", None)
            .expect("hash with other ssh key");
        assert_ne!(hash1, other_key);
    }

    #[fuchsia::test]
    fn test_compute_bundle_hash_covers_vbmeta_keys() {
        let temp = tempfile::tempdir().unwrap();
        const KEY: &str = "ssh-ed25519 AAAAC3... test_key";
        let keys = VbmetaKeys {
            key: temp.path().join("key.pem"),
            metadata: temp.path().join("metadata.bin"),
        };
        std::fs::write(&keys.key, b"private key").unwrap();
        std::fs::write(&keys.metadata, b"key metadata").unwrap();

        let without = compute_bundle_hash(temp.path(), KEY, None).unwrap();
        let with = compute_bundle_hash(temp.path(), KEY, Some(&keys)).unwrap();
        assert_ne!(without, with);

        std::fs::write(&keys.key, b"different private key").unwrap();
        assert_ne!(with, compute_bundle_hash(temp.path(), KEY, Some(&keys)).unwrap());
    }

    #[fuchsia::test]
    fn test_compute_bundle_hash_invalid_manifest_propagates_error() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("product_bundle.json"), b"not valid json").unwrap();
        let res = compute_bundle_hash(temp.path(), "ssh-ed25519 AAAAC3... test_key", None);
        assert!(res.is_err());
    }

    #[fuchsia::test]
    fn test_package_gce_tar_gz() {
        let temp = tempfile::tempdir().unwrap();
        let raw_disk = temp.path().join("disk.raw");
        std::fs::write(&raw_disk, b"fake raw disk content").unwrap();

        let tar_gz = temp.path().join("disk.tar.gz");
        package_gce_tar_gz(&raw_disk, &tar_gz).unwrap();
        assert!(tar_gz.exists());

        let tar_gz_file = File::open(&tar_gz).unwrap();
        let dec = flate2::read::GzDecoder::new(tar_gz_file);
        let mut archive = tar::Archive::new(dec);
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap(), Path::new("disk.raw"));
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
        assert_eq!(content, b"fake raw disk content");
    }

    #[fuchsia::test]
    fn test_package_gce_tar_gz_does_not_leave_corrupt_file_on_error() {
        let temp = tempfile::tempdir().unwrap();
        let missing_raw_disk = temp.path().join("missing.raw");
        let tar_gz = temp.path().join("disk.tar.gz");

        let res = package_gce_tar_gz(&missing_raw_disk, &tar_gz);
        assert!(res.is_err());
        assert!(!tar_gz.exists());
    }
}
