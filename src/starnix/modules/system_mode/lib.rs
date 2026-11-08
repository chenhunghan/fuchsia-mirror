// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exposes `/sys/power/fuchsia_system_mode` for reading and updating the container's active
//! system power/scheduling mode.
//!
//! Reads return the active mode identifier followed by a newline (defaulting to `"NOMINAL\n"`).
//! Writes accept an ASCII mode identifier matching `^[A-Z][A-Z0-9_]{0,63}$` (with optional leading
//! or trailing ASCII whitespace) and return `EINVAL` for invalid inputs.

#![recursion_limit = "256"]

use starnix_core::fs::sysfs::get_sysfs;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{errno, error};
use std::borrow::Cow;

/// Maximum length of a system mode identifier written to `/sys/power/fuchsia_system_mode`.
const MAX_SYSTEM_MODE_LEN: usize = 64;

/// Registers `/sys/power/fuchsia_system_mode` in the kernel's sysfs tree.
pub fn init_system_mode(kernel: &Kernel) {
    let fs = get_sysfs(kernel);
    let root = SimpleDirectoryMutator::new(fs, kernel.device_registry.objects.root.clone());
    root.subdir("power", 0o755, |dir| {
        dir.entry("fuchsia_system_mode", FuchsiaSystemModeFile::new_node(), mode!(IFREG, 0o644));
    });
}

/// Validates and trims an input byte buffer into an uppercase ASCII mode identifier
/// (`^[A-Z][A-Z0-9_]{0,63}$`).
fn parse_system_mode(data: &[u8]) -> Result<&str, Errno> {
    let mode = std::str::from_utf8(data).map_err(|_| errno!(EINVAL))?.trim_ascii();
    if mode.is_empty() || mode.len() > MAX_SYSTEM_MODE_LEN {
        return error!(EINVAL);
    }
    let bytes = mode.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_uppercase)
        || !bytes.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
    {
        return error!(EINVAL);
    }
    Ok(mode)
}

struct FuchsiaSystemModeFile;

impl FuchsiaSystemModeFile {
    fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self)
    }
}

impl BytesFileOps for FuchsiaSystemModeFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let mode = parse_system_mode(&data)?;
        let kernel = current_task.kernel();
        kernel.scheduler.set_system_mode(kernel, mode)
    }

    fn read(&self, current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let mode = current_task.kernel().scheduler.system_mode();
        Ok(format!("{mode}\n").into_bytes().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::spawn_kernel_and_run_sync;

    #[fuchsia::test]
    async fn init_system_mode_registers_sysfs_node() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            assert!(kernel.device_registry.objects.root.lookup("power/state".into()).is_some());
            assert!(
                kernel
                    .device_registry
                    .objects
                    .root
                    .lookup("power/fuchsia_system_mode".into())
                    .is_none()
            );

            init_system_mode(kernel);

            assert!(kernel.device_registry.objects.root.lookup("power/state".into()).is_some());
            assert!(
                kernel
                    .device_registry
                    .objects
                    .root
                    .lookup("power/fuchsia_system_mode".into())
                    .is_some()
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn fuchsia_system_mode_read_and_write() {
        spawn_kernel_and_run_sync(|current_task| {
            let kernel = current_task.kernel();
            let file = FuchsiaSystemModeFile;
            assert_eq!(file.read(current_task).unwrap().as_ref(), b"NOMINAL\n");
            assert_eq!(kernel.scheduler.system_mode(), "NOMINAL");

            // Transition to LAUNCH mode and verify the active system mode updates.
            file.write(current_task, b"LAUNCH\n".to_vec()).expect("write LAUNCH");
            assert_eq!(file.read(current_task).unwrap().as_ref(), b"LAUNCH\n");
            assert_eq!(kernel.scheduler.system_mode(), "LAUNCH");

            // Invalid mode string returns EINVAL and leaves the non-default mode unchanged.
            assert_eq!(file.write(current_task, b"invalid_lower\n".to_vec()), error!(EINVAL));
            assert_eq!(file.read(current_task).unwrap().as_ref(), b"LAUNCH\n");
            assert_eq!(kernel.scheduler.system_mode(), "LAUNCH");

            // Transition to an arbitrary valid mode identifier with surrounding whitespace.
            file.write(current_task, b" CUSTOM_BOOST_2 \n".to_vec()).expect("write CUSTOM_BOOST_2");
            assert_eq!(file.read(current_task).unwrap().as_ref(), b"CUSTOM_BOOST_2\n");
            assert_eq!(kernel.scheduler.system_mode(), "CUSTOM_BOOST_2");

            // Transition back to NOMINAL mode.
            file.write(current_task, b"NOMINAL".to_vec()).expect("write NOMINAL");
            assert_eq!(file.read(current_task).unwrap().as_ref(), b"NOMINAL\n");
            assert_eq!(kernel.scheduler.system_mode(), "NOMINAL");
        })
        .await;
    }

    #[fuchsia::test]
    fn parse_system_mode_validates_syntax() {
        let max_len_mode = "A".repeat(MAX_SYSTEM_MODE_LEN);
        for (input, expected) in [
            (&b"A"[..], "A"),
            (&b"NOMINAL\n"[..], "NOMINAL"),
            (&b"  LAUNCH_EXTEND_2 \t\n"[..], "LAUNCH_EXTEND_2"),
            (max_len_mode.as_bytes(), max_len_mode.as_str()),
        ] {
            assert_eq!(parse_system_mode(input), Ok(expected));
        }

        let too_long_mode = "A".repeat(MAX_SYSTEM_MODE_LEN + 1);
        for invalid in [
            &b""[..],
            &b"   \n"[..],
            &b"nominal"[..],
            &b"Launch"[..],
            &b"1LAUNCH"[..],
            &b"_LAUNCH"[..],
            &b"LAUNCH-EXTEND"[..],
            &b"LAUNCH MODE"[..],
            &b"LAUNCH/1"[..],
            &[0xff, 0xfe],
            too_long_mode.as_bytes(),
        ] {
            assert_eq!(parse_system_mode(invalid), error!(EINVAL), "input: {invalid:?}");
        }
    }
}
