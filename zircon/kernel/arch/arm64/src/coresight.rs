// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The `coresight` console command, which describes the components a CoreSight ROM table names.

use core::ffi::{c_int, c_void};

use coresight::{
    CIDR1, ClassId, DEVAFF, DEVARCH, DEVTYPE, MINIMUM_COMPONENT_SIZE, RomTable, arm,
    class_id_to_string, device_type_to_string, get_designer, get_part_id,
};
use kprint::kprint;
use regio::{MmioBank, MmioPtr, Offset, Ro};

use super::exceptions::alt_hex_prefix;
use super::mp::arm64_mpidr_to_cpu_num;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::types::{INVALID_CPU, PAddr};
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_UNCACHED_DEVICE};
use crate::vm::vm::vaddr_to_paddr;
use crate::vm::vm_aspace::VmAspace;

/// At the time of writing this, all observed ROM tables on the supported ARM
/// boards fit within an 8MiB address range. Increase as needed.
const VIEW_SIZE: u32 = 0x80_0000;

/// Prints information about a generic CoreSight component.
///
/// # Safety
///
/// `component` must be 4-byte aligned and point to a component's first `MINIMUM_COMPONENT_SIZE`
/// bytes, which must be mapped device memory that stays mapped for the call, and reads within
/// them must never trap.
unsafe fn dump_component_info(component: *const u32) {
    let mmio = MmioBank::new(
        // SAFETY: The caller's guarantee.
        unsafe { MmioPtr::<u32, Ro>::new(component) },
        MINIMUM_COMPONENT_SIZE,
    );
    let paddr = vaddr_to_paddr(component.cast::<c_void>());
    kprint!("address: {:cs}{:x}\n", alt_hex_prefix(paddr), paddr.0);

    // SAFETY: `at` panics unless the register is aligned and in the window that the caller
    // vouched for.
    let classid = unsafe { mmio.at(CIDR1) }.read().try_classid();
    // SAFETY: `mmio` is the component's first `MINIMUM_COMPONENT_SIZE` bytes, which the caller
    // vouched for.
    let partid = unsafe { get_part_id(&mmio) };

    // Morally a CoreSight component, if not one technically, ARM puts them in ROM tables.
    if matches!(classid, Ok(ClassId::NonStandard)) && partid == arm::partid::TIMESTAMP_GENERATOR {
        kprint!("type: N/A\n");
        kprint!("affinity: cluster\n");
        kprint!("architect: ARM\n");
        kprint!("architecture: Timestamp Generator\n");
        return;
    }
    if !matches!(classid, Ok(ClassId::CoreSight)) {
        let (classid, classid_str) = match classid {
            Ok(classid) => (classid as u8, class_id_to_string(classid)),
            Err(invalid) => (invalid.0, "unknown"),
        };
        kprint!(
            "unexpected component found; (class, part number) = ({:cs}{:x} ({:s}), {:cs}{:x})\n",
            alt_hex_prefix(classid),
            classid,
            classid_str,
            alt_hex_prefix(partid),
            partid
        );
        return;
    }

    // SAFETY: As for `CIDR1`.
    let device_type = unsafe { mmio.at(DEVTYPE) }.read().device_type();
    let type_str = device_type_to_string(device_type);
    kprint!("type: {:s}\n", type_str);

    // A 32-bit access: this reads DEVAFF0 alone, zero-extended.
    // SAFETY: As for `CIDR1`.
    let affinity = u64::from(unsafe { mmio.at(Offset::<u32, Ro>::new(DEVAFF.value)) }.read());
    kprint!("affinity: ");
    if affinity == 0 {
        kprint!("cluster\n");
    } else {
        let cpu_num = arm64_mpidr_to_cpu_num(affinity);
        if cpu_num != INVALID_CPU {
            kprint!("CPU #{:u} ({:cs}{:x})\n", cpu_num, alt_hex_prefix(affinity), affinity);
        } else {
            kprint!("{:cs}{:x}\n", alt_hex_prefix(affinity), affinity);
        }
    }

    // SAFETY: As for `CIDR1`.
    let arch_reg = unsafe { mmio.at(DEVARCH) }.read();
    let archid = arch_reg.archid();
    let revision = arch_reg.revision();

    // The device architecture register might not be populated; in this case, we
    // consult the designer designation.
    let architect = if arch_reg.architect() != 0 {
        arch_reg.architect()
    } else {
        // SAFETY: As for `get_part_id`.
        unsafe { get_designer(&mmio) }
    };

    if architect != arm::ARCHITECT {
        kprint!("architect: unknown ({:cs}{:x})\n", alt_hex_prefix(architect), architect);
        kprint!("archid: {:cs}{:x}\n", alt_hex_prefix(archid), archid);
        kprint!("part number: {:cs}{:x}\n", alt_hex_prefix(partid), partid);
        return; // Not much more we can say.
    }

    kprint!("architect: ARM\n");
    kprint!("architecture: ");
    match archid {
        arm::archid::CTI => {
            kprint!("Cross-Trigger Matrix (CTI)\n");
            return;
        }
        arm::archid::ETM3 => {
            kprint!("Embedded Trace Monitor (ETM) v3.{:u}\n", revision);
            return;
        }
        arm::archid::ETM4 => {
            kprint!("Embedded Trace Monitor (ETM) v4.{:u}\n", revision);
            return;
        }
        arm::archid::PMU2 => {
            kprint!("Performance Monitor Unit (PMU) v2.{:u}\n", revision);
            return;
        }
        arm::archid::PMU3 => {
            kprint!("Performance Monitor Unit (PMU) v3.{:u}\n", revision);
            return;
        }
        arm::archid::ROM_TABLE => {
            kprint!("0x9 ROM Table\n");
            return;
        }
        arm::archid::CORE_DEBUG_INTERFACE_8_0A => {
            kprint!("ARM v8.0-A Core Debug Interface\n");
            return;
        }
        arm::archid::CORE_DEBUG_INTERFACE_8_1A => {
            kprint!("ARM v8.1-A Core Debug Interface\n");
            return;
        }
        arm::archid::CORE_DEBUG_INTERFACE_8_2A => {
            kprint!("ARM v8.2-A Core Debug Interface\n");
            return;
        }
        _ => {}
    }

    // Sometimes no architecture ID is populated; fall back to part ID.
    match partid {
        arm::partid::ETB => {
            kprint!("Embedded Trace Buffer (ETB)\n");
            return;
        }
        arm::partid::CTI_400 => {
            kprint!("Cross-Trigger Matrix (CTI) (SoC400 generation)\n");
            return;
        }
        arm::partid::CTI_600 => {
            kprint!("Cross-Trigger Matrix (CTI) (SoC600 generation)\n");
            return;
        }
        arm::partid::TMC => {
            kprint!("Trace Memory Controller (TMC) (SoC400 generation)\n");
            return;
        }
        arm::partid::TPIU => {
            kprint!("Trace Port Interface Unit (TPIU)\n");
            return;
        }
        arm::partid::TRACE_FUNNEL => {
            kprint!("Trace Funnel (SoC400 generation)\n");
            return;
        }
        arm::partid::TRACE_REPLICATOR => {
            kprint!("Trace Replicator (SoC400 generation)\n");
            return;
        }
        _ => {}
    }
    kprint!(
        "unknown: (archid, part number) = ({:cs}{:x}, {:cs}{:x})\n",
        alt_hex_prefix(archid),
        archid,
        alt_hex_prefix(partid),
        partid
    );
}

/// # Safety
///
/// `addr` must be 8-byte aligned and point to `view_size` bytes of mapped device memory that
/// stays mapped for the call, and reads within it must never trap.
unsafe fn walk_rom_table(addr: *const u32, view_size: u32) -> c_int {
    // SAFETY: The caller's guarantee.
    let mmio = unsafe { MmioPtr::<u32, Ro>::new(addr) };
    let callback = |offset: u32| {
        kprint!("\n----------------------------------------\n");
        // SAFETY: The walk reports a component only when its first page is inside the
        // `view_size`-byte mapping that the caller vouched for, so `byte_add` stays inside that
        // mapping. The offset is a multiple of 4 KiB because table entry offsets are shifted by
        // 12, so the pointer stays aligned.
        unsafe { dump_component_info(addr.byte_add(offset as usize)) };
    };
    // SAFETY: `mmio` is the 8-byte aligned start of the `view_size` bytes that the caller vouched
    // for.
    let result = unsafe { RomTable::walk(mmio, view_size, callback) };
    if let Err(error) = result {
        kprint!("error at offset {:x}: {:s}\n", error.offset, error.reason);
        return 1;
    }
    0
}

/// # Safety
///
/// `argv` must point to `argc` initialized `CmdArgs` whose `arg_str` fields point to
/// NUL-terminated strings, and `argc` is at least 1.
unsafe extern "C" fn cmd_coresight(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The caller guarantees that `argv` points to `argc` initialized `CmdArgs`.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || {
        kprint!("usage:\n");
        kprint!("k {:cs} help\n", argv[0].arg_str);
        kprint!("k {:cs} walk <ROM table physical address>\n", argv[0].arg_str);
    };

    if argc < 2 {
        usage();
        return 1;
    }

    if argv[1].as_str() == "help" {
        usage();
        return 0;
    }

    if argv[1].as_str() == "walk" {
        if argc < 3 {
            kprint!("too few arguments\n");
            usage();
            return 1;
        }

        let paddr = PAddr::from(argv[2].arg_uint);
        kprint!("attempting to walk a ROM table at {:cs}{:x}...\n", alt_hex_prefix(paddr), paddr.0);
        // SAFETY: The cache policy in the flags is appropriate for all mappings of the physical
        // range, and creating a read-only kernel mapping of it is sound.
        let base = match unsafe {
            VmAspace::kernel_aspace().alloc_physical(
                b"k coresight walk",
                VIEW_SIZE as usize, // Range size
                None,               // Requested virtual address
                page::SHIFT as u8,  // Alignment log2
                paddr,              // Physical address
                0,                  // VMM flags
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_UNCACHED_DEVICE, // MMU flags
            )
        } {
            Ok(base) => base,
            Err(status) => {
                kprint!(
                    "failed to map address range starting at {:cs}{:x}: {:d}\n",
                    alt_hex_prefix(paddr),
                    paddr.0,
                    status.into_raw()
                );
                return 1;
            }
        };
        let virt: *const u32 = core::ptr::with_exposed_provenance(base);
        kprint!("virtual address: {:p}\n", virt);
        // SAFETY: `virt` is a fresh page-aligned device mapping of `VIEW_SIZE` bytes that is never
        // unmapped. The console user vouches that reading a ROM table at `paddr`, and the
        // components it names, does not trap.
        return unsafe { walk_rom_table(virt, VIEW_SIZE) };
    }

    kprint!("unrecognized command: {:cs}\n", argv[1].arg_str);
    usage();
    1
}

static_command!(
    CMD_CORESIGHT,
    c"coresight".as_ptr(),
    c"access information within a CoreSight system".as_ptr(),
    cmd_coresight,
    CMD_AVAIL_NORMAL
);
