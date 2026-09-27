// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Utilities for inspecting Zircon Boot Image (ZBI) containers.

use fho::{Result, user_error};
use sdk_metadata::CpuArchitecture;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Size of a ZBI container or item header, per
/// `//sdk/lib/zbi-format/include/lib/zbi-format/zbi.h`.
const ZBI_HEADER_SIZE: usize = 32;

/// `ZBI_TYPE_KERNEL_*` values, which encode the target architecture in the high byte of the
/// `KRN\0` kernel item type prefix.
const ZBI_TYPE_KERNEL_ARM64: u32 = 0x384e_524b;
const ZBI_TYPE_KERNEL_X64: u32 = 0x4c4e_524b;
const ZBI_TYPE_KERNEL_RISCV64: u32 = 0x564e_524b;

/// Returns the CPU architecture targeted by the bootable ZBI at `zbi_path`.
///
/// A bootable ZBI always begins with a container header immediately followed by the kernel item,
/// whose type identifies the architecture.
pub fn zbi_architecture(zbi_path: &Path) -> Result<CpuArchitecture> {
    let mut header = [0u8; 2 * ZBI_HEADER_SIZE];
    File::open(zbi_path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|e| user_error!("Failed to read ZBI header from {}: {e}", zbi_path.display()))?;

    let kernel_type = u32::from_le_bytes(
        header[ZBI_HEADER_SIZE..ZBI_HEADER_SIZE + 4].try_into().expect("4-byte slice"),
    );
    match kernel_type {
        ZBI_TYPE_KERNEL_ARM64 => Ok(CpuArchitecture::Arm64),
        ZBI_TYPE_KERNEL_X64 => Ok(CpuArchitecture::X64),
        ZBI_TYPE_KERNEL_RISCV64 => Ok(CpuArchitecture::Riscv64),
        other => Err(user_error!(
            "{} does not start with a kernel item (found item type {other:#010x}), so it is not a bootable ZBI.",
            zbi_path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_zbi(dir: &Path, name: &str, kernel_type: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut bytes = vec![0u8; 2 * ZBI_HEADER_SIZE];
        bytes[ZBI_HEADER_SIZE..ZBI_HEADER_SIZE + 4].copy_from_slice(&kernel_type.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[fuchsia::test]
    fn test_zbi_architecture() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            zbi_architecture(&write_zbi(dir.path(), "x64.zbi", ZBI_TYPE_KERNEL_X64)).unwrap(),
            CpuArchitecture::X64
        );
        assert_eq!(
            zbi_architecture(&write_zbi(dir.path(), "arm64.zbi", ZBI_TYPE_KERNEL_ARM64)).unwrap(),
            CpuArchitecture::Arm64
        );
        assert_eq!(
            zbi_architecture(&write_zbi(dir.path(), "riscv64.zbi", ZBI_TYPE_KERNEL_RISCV64))
                .unwrap(),
            CpuArchitecture::Riscv64
        );
    }

    #[fuchsia::test]
    fn test_zbi_architecture_rejects_non_kernel_zbi() {
        let dir = tempfile::tempdir().unwrap();
        assert!(zbi_architecture(&write_zbi(dir.path(), "data.zbi", 0x4d5f_4d52)).is_err());
        assert!(zbi_architecture(&dir.path().join("missing.zbi")).is_err());
    }
}
