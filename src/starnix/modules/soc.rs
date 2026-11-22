// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, bail};
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{ArgNameAndValue, Kernel};
use starnix_core::vfs::pseudo::simple_file::BytesFile;
use starnix_core::vfs::pseudo::stub_empty_file::StubEmptyFile;
use starnix_logging::bug_ref;
use starnix_uapi::file_mode::mode;

#[derive(Default)]
struct SocConfig<'a> {
    machine: Option<&'a str>,
    family: Option<&'a str>,
    soc_id: Option<&'a str>,
    revision: Option<&'a str>,
    serial_number: Option<&'a str>,
}

fn parse_args<'a>(args: &'a [&'a str]) -> Result<SocConfig<'a>, Error> {
    let mut config = SocConfig::default();
    for &arg in args {
        match ArgNameAndValue::from(arg) {
            ArgNameAndValue { name, value: Some(value) } if !value.is_empty() => match name {
                "machine" => config.machine = Some(value),
                "family" => config.family = Some(value),
                "soc_id" => config.soc_id = Some(value),
                "revision" => config.revision = Some(value),
                "serial_number" => config.serial_number = Some(value),
                _ => bail!("invalid soc module argument: {arg}"),
            },
            _ => bail!("invalid soc module argument: {arg}"),
        }
    }
    Ok(config)
}

/// Validates `soc` module configuration `args`.
pub fn validate_args(args: &[&str]) -> Result<(), Error> {
    parse_args(args).map(|_| ())
}

/// Registers the `/sys/devices/soc0` identification device on the `soc` bus.
pub fn module_init(kernel: &Kernel, args: &[&str]) -> Result<(), Error> {
    let config = parse_args(args)?;
    let registry = &kernel.device_registry;
    registry.add_bus_device(
        "soc0".into(),
        /* parent = */ None,
        registry.objects.get_or_create_bus("soc".into()),
        |device, dir| {
            build_device_directory(device, dir);
            for (name, value) in [
                ("machine", config.machine),
                ("family", config.family),
                ("soc_id", config.soc_id),
                ("revision", config.revision),
                ("serial_number", config.serial_number),
            ] {
                if let Some(value) = value {
                    dir.entry(
                        name,
                        BytesFile::new_node(format!("{value}\n").into_bytes()),
                        mode!(IFREG, 0o444),
                    );
                } else if matches!(name, "revision" | "serial_number") {
                    // TODO(https://fxbug.dev/452096300): Remove stub fallback once containers
                    // configure explicit SoC identification attributes.
                    dir.entry(
                        name,
                        StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/452096300")),
                        mode!(IFREG, 0o444),
                    );
                }
            }
        },
    )?;
    Ok(())
}
