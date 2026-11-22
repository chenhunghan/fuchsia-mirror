// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::Args;
use crate::capability::{Capability, ExtendedCapability};
use crate::config::{CommandRegister, StatusRegister, Type00Config};
use crate::db::PciDb;
use crate::util::{Hexdumper, format_bytes};
use fidl_fuchsia_hardware_pci::{BASE_CONFIG_SIZE, DeviceSnapshot};
use std::fmt;
use zerocopy::Ref;

pub struct Device<'a> {
    pub device: &'a DeviceSnapshot,
    pub class: Option<String>,
    pub name: Option<String>,
    pub cfg: Ref<&'a [u8], Type00Config>,
    pub args: &'a Args,
}

// Extended capabilities start immediately after the conventional configuration space.
const EXT_CAP_START: usize = 0x100;
const EXT_CAP_END: usize = 0xffc;

impl<'a> Device<'a> {
    /// Returns None if the device's config space is too short to hold a header.
    pub fn new(
        device: &'a DeviceSnapshot,
        id_db: &Option<PciDb<'_>>,
        args: &'a Args,
    ) -> Option<Self> {
        let cfg = Type00Config::new(&device.config)?;
        let (class, name) = if let Some(db) = id_db {
            (
                db.find_class(cfg.base_class, cfg.sub_class, Some(cfg.program_interface)),
                db.find_device(cfg.vendor_id, cfg.device_id),
            )
        } else {
            (None, None)
        };
        Some(Device { device, class, name, cfg, args })
    }

    fn is_bridge(&self) -> bool {
        (self.cfg.header_type & 0x7f) == 0x01
    }

    fn print_bars(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Bridges only have two BARs; the rest of their header holds bus numbers and windows.
        let max_bars = if self.is_bridge() { 2 } else { 6 };
        let base_address = self.cfg.base_address;
        let mut i = 0;
        while i < max_bars {
            let size = self.device.bar_sizes[i];
            if size == 0 {
                i += 1;
                continue;
            }
            let raw_low = base_address[i];
            let (is_memory, is_64bit, is_prefetchable, address, step) = if raw_low & 0x1 != 0 {
                (false, false, false, (raw_low & 0xffff_fffc) as u64, 1)
            } else {
                let is_64bit = (raw_low & 0x6) == 0x4 && i + 1 < max_bars;
                let low_addr = (raw_low & 0xffff_fff0) as u64;
                let address = if is_64bit {
                    low_addr | ((base_address[i + 1] as u64) << 32)
                } else {
                    low_addr
                };
                (true, is_64bit, (raw_low & 0x8) != 0, address, if is_64bit { 2 } else { 1 })
            };
            write!(
                f,
                "\tRegion {}: {} at {:#x} ({}-bit, {}) [size={}]\n",
                i,
                if is_memory { "Memory" } else { "I/O ports" },
                address,
                if is_64bit { 64 } else { 32 },
                if is_prefetchable { "prefetchable" } else { "non-prefetchable" },
                format_bytes(size)
            )?;
            i += step;
        }
        Ok(())
    }

    fn print_capabilities(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !StatusRegister(self.cfg.status).has_cap_list() {
            return Ok(());
        }
        let config = &self.device.config[..];
        let mut ptr = (self.cfg.capabilities_ptr & 0xfc) as usize;
        let mut visited = [false; 256];
        while ptr >= 0x40 && ptr <= 0xfc && ptr + 2 <= config.len() && !visited[ptr] {
            visited[ptr] = true;
            let id = config[ptr];
            let next = (config[ptr + 1] & 0xfc) as usize;
            writeln!(f, "\t{}", Capability::new(id, ptr, config))?;
            ptr = next;
        }
        Ok(())
    }

    fn print_extended_capabilities(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let config = &self.device.config[..];
        let mut offset = EXT_CAP_START;
        let mut visited = [false; 1024];
        while offset >= EXT_CAP_START
            && offset <= EXT_CAP_END
            && offset + 4 <= config.len()
            && !visited[offset / 4]
        {
            visited[offset / 4] = true;
            let header = u32::from_le_bytes(config[offset..offset + 4].try_into().unwrap());
            let id = (header & 0xffff) as u16;
            let next = ((header >> 20) & 0xffc) as usize;
            if id == 0xffff || (id == 0 && next == 0) {
                break;
            }
            writeln!(f, "\t{}", ExtendedCapability::new(id, offset))?;
            if next == 0 {
                break;
            }
            offset = next;
        }
        Ok(())
    }

    pub fn print_common_header(&self, f: &'a mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}.{:1x}",
            self.device.bdf.bus_id, self.device.bdf.device_id, self.device.bdf.function_id
        )?;
        if let Some(class) = &self.class {
            if !self.args.only_print_numeric {
                write!(f, " {}", class)?;
            }
        }
        if self.class.is_none() || self.args.print_numeric || self.args.only_print_numeric {
            write!(f, " [{:02x}{:02x}]", self.cfg.base_class, self.cfg.sub_class)?;
        }
        write!(f, ":")?;

        if let Some(name) = &self.name {
            if !self.args.only_print_numeric {
                write!(f, " {}", name)?;
            }
        }

        if self.name.is_none() || self.args.print_numeric || self.args.only_print_numeric {
            write!(f, " [{:04x}:{:04x}]", { self.cfg.vendor_id }, { self.cfg.device_id })?;
        }
        writeln!(f, " (rev {:02x})", self.cfg.revision_id)?;
        if self.args.verbose {
            writeln!(f, "\tControl: {}", CommandRegister(self.cfg.command))?;
            writeln!(f, "\tStatus: {}", StatusRegister(self.cfg.status))?;
        };

        Ok(())
    }

    pub fn print_common_footer(&self, f: &'a mut fmt::Formatter<'_>) -> fmt::Result {
        if self.args.verbose {
            self.print_bars(f)?;
            self.print_capabilities(f)?;
            self.print_extended_capabilities(f)?;
        }

        if self.args.print_config || self.args.print_extended_config {
            let bytes = if self.args.print_extended_config {
                &self.device.config[..]
            } else {
                &self.device.config
                    [..std::cmp::min(BASE_CONFIG_SIZE as usize, self.device.config.len())]
            };
            write!(
                f,
                "{}",
                Hexdumper { bytes, show_header: true, show_ascii: false, offset: None }
            )?;
        }

        if self.args.verbose || self.args.print_config || self.args.print_extended_config {
            writeln!(f)?;
        }

        Ok(())
    }
}

impl<'a> fmt::Display for Device<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.print_common_header(f)?;
        self.print_common_footer(f)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;
    use fidl_fuchsia_hardware_pci::Bdf;

    fn args() -> Args {
        Args {
            filter: None,
            verbose: true,
            quiet: true,
            print_config: false,
            print_extended_config: false,
            print_numeric: true,
            only_print_numeric: false,
            command: None,
        }
    }

    fn device(config: Vec<u8>, bar_sizes: [u64; 6]) -> DeviceSnapshot {
        DeviceSnapshot { bdf: Bdf { bus_id: 1, device_id: 2, function_id: 3 }, bar_sizes, config }
    }

    fn put32(config: &mut [u8], offset: usize, value: u32) {
        config[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn footer(snapshot: &DeviceSnapshot) -> String {
        let args = args();
        let device = Device::new(snapshot, &None, &args).unwrap();
        std::fmt::from_fn(|f| device.print_common_footer(f)).to_string()
    }

    #[test]
    fn test_short_config_is_rejected() {
        let args = args();
        assert!(Device::new(&device(vec![0; 63], [0; 6]), &None, &args).is_none());
    }

    #[test]
    fn test_endpoint_bars() {
        let mut config = vec![0u8; 256];
        // BAR 0: 32-bit non-prefetchable memory.
        put32(&mut config, 0x10, 0xfe00_0000);
        // BARs 1 and 2: 64-bit prefetchable memory.
        put32(&mut config, 0x14, 0x8000_000c);
        put32(&mut config, 0x18, 0x0000_0001);
        // BAR 4: I/O space.
        put32(&mut config, 0x20, 0x0000_2001);
        let out = footer(&device(config, [4096, 1 << 20, 0, 0, 128, 0]));
        assert_eq!(
            out,
            "\tRegion 0: Memory at 0xfe000000 (32-bit, non-prefetchable) [size=4K]\n\
             \tRegion 1: Memory at 0x180000000 (64-bit, prefetchable) [size=1M]\n\
             \tRegion 4: I/O ports at 0x2000 (32-bit, non-prefetchable) [size=128B]\n\n"
        );
    }

    #[test]
    fn test_bridge_bars() {
        let mut config = vec![0u8; 256];
        config[0x0e] = 0x01;
        put32(&mut config, 0x10, 0xfe00_0000);
        // Bus numbers in a bridge header must not be decoded as a BAR.
        put32(&mut config, 0x18, 0x0001_0100);
        let out = footer(&device(config, [4096, 0, 4096, 0, 0, 0]));
        assert_eq!(
            out,
            "\tRegion 0: Memory at 0xfe000000 (32-bit, non-prefetchable) [size=4K]\n\n"
        );
    }

    #[test]
    fn test_capability_list() {
        let mut config = vec![0u8; 256];
        config[0x34] = 0x40;
        // Power management at 0x40, then vendor at 0x50, which points back to 0x40.
        config[0x40] = 0x01;
        config[0x41] = 0x50;
        config[0x50] = 0x09;
        config[0x51] = 0x40;
        config[0x52] = 0x08;

        // Without the capability list bit in the status register nothing is printed.
        assert_eq!(footer(&device(config.clone(), [0; 6])), "\n");

        config[0x06] = 0x10;
        assert_eq!(
            footer(&device(config, [0; 6])),
            "\tCapabilities: [0x40] PCI Power Management\n\
             \tCapabilities: [0x50] Vendor Specific Information: Len=0x8\n\n"
        );
    }

    #[test]
    fn test_extended_capability_list() {
        // Conventional config space has no extended capabilities.
        let mut config = vec![0u8; 256];
        assert_eq!(footer(&device(config.clone(), [0; 6])), "\n");

        config.resize(4096, 0);
        // Absent extended capability lists read as all zeros or all ones.
        assert_eq!(footer(&device(config.clone(), [0; 6])), "\n");
        put32(&mut config, 0x100, 0xffff_ffff);
        assert_eq!(footer(&device(config.clone(), [0; 6])), "\n");

        // AER at 0x100 -> ARI at 0x200 -> back to 0x100.
        put32(&mut config, 0x100, (0x200 << 20) | 0x0001);
        put32(&mut config, 0x200, (0x100 << 20) | 0x000e);
        assert_eq!(
            footer(&device(config, [0; 6])),
            "\tCapabilities: [0x100] Advanced Error Reporting\n\
             \tCapabilities: [0x200] Alternative Routing-ID Interpretation (ARI)\n\n"
        );
    }

    #[test]
    fn test_print_config_base_vs_extended() {
        let mut config = vec![0u8; 4096];
        put32(&mut config, 0x100, 0x1122_3344);
        put32(&mut config, 0xffc, 0xefbe_adde);
        let snapshot = device(config, [0; 6]);

        let base_args = Args::from_args(&["lspci"], &["-x"]).unwrap();
        let base_out = format!("{}", Device::new(&snapshot, &None, &base_args).unwrap());
        assert!(base_out.contains("  0000: "));
        assert!(base_out.contains("  00f0: "));
        assert!(!base_out.contains("  0100: "));

        let ext_args = Args::from_args(&["lspci"], &["-X"]).unwrap();
        let ext_out = format!("{}", Device::new(&snapshot, &None, &ext_args).unwrap());
        assert!(ext_out.contains("  00f0: "));
        assert!(ext_out.contains("  0100: 44 33 22 11 "));
        assert!(ext_out.contains("  0ff0: 00 00 00 00 00 00 00 00  00 00 00 00 de ad be ef "));
    }
}
