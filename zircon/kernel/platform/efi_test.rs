// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// EFI service tests.
#[cfg(ktest)]
#[unittest::suite(name = "efi")]
mod tests {
    use crate::kernel::thread;
    use crate::platform_rs::efi::{
        EfiServicesActivation, for_each_memory_attribute_entry_safe, try_activate_efi_services,
    };
    use crate::platform_rs::efi_ffi::is_efi_expected;
    use core::mem::size_of;
    use core::ptr;
    use efi::{
        EFI_INVALID_PARAMETER, EFI_RUNTIME_SERVICES_CODE, EFI_RUNTIME_SERVICES_DATA, EFI_SUCCESS,
        EfiGuid, EfiMemoryAttributesTableHeader, EfiMemoryDescriptor, EfiTime,
    };
    use kprint::kprint;
    use unittest::{
        expect_eq, expect_err, expect_false, expect_gt, expect_lt, expect_ok, expect_true,
    };
    use zerocopy::IntoBytes;
    use zx_status::Status;

    /// Ensure EFI is present on platforms we know should have it.
    #[test]
    #[sanitize(address = "off")]
    fn test_efi_present() {
        // This test aims to prevent EFI support from being silently dropped.

        // Grab our current aspace.
        // SAFETY: The reference is only compared by address below, after the guard restores it.
        let old_aspace = unsafe { thread::current_active_aspace() };

        // Attempt to fetch EFI services.
        // SAFETY: `services` is scoped to this stack frame (strictly LIFO), the thread's previous
        // `VmAspace` is not switched or destroyed before `services` is reset, and this function is
        // tagged `#[sanitize(address = "off")]`.
        let mut services = unsafe { try_activate_efi_services() };

        // Ensure we got back a valid result if EFI meant to be present.
        // SAFETY: Takes no arguments and reads platform state set up during boot.
        if unsafe { is_efi_expected() } {
            expect_true!(services.valid());
            // This should switch back to the old aspace.
            // SAFETY: No inner `EfiServicesActivation` guard is active, and `services` is not
            // accessed again after `reset`.
            unsafe { services.reset() };

            // Make sure it actually did.
            // SAFETY: The reference is only compared by address.
            expect_eq!(
                old_aspace.map_or(ptr::null(), ptr::from_ref),
                unsafe { thread::current_active_aspace() }.map_or(ptr::null(), ptr::from_ref)
            );
        } else {
            kprint!("Unknown if EFI is expected to be supported on platform. Skipping test.\n");
        }
    }

    /// Tests that `GetTime` returns a reasonable year when EFI services are available.
    #[test]
    #[sanitize(address = "off", cfi = "off")]
    fn test_efi_services() {
        // Fetch EFI services.
        // SAFETY: `services` is scoped to this stack frame (strictly LIFO), the thread's previous
        // `VmAspace` is not switched or destroyed before `services` is dropped, and this function
        // is tagged `#[sanitize(address = "off")]`.
        let services = unsafe { try_activate_efi_services() };
        // We may not have EFI services.
        if let Some(rt) = services.get() {
            // Ensure we can call `GetTime` and get a reasonable year (between 2000 and 2100).
            let mut time = EfiTime::default();
            // SAFETY: `rt` points at the firmware's runtime services table, mapped 1:1 in the
            // aspace the guard activated.
            if let Some(get_time) = unsafe { (*rt.as_ptr()).GetTime } {
                // SAFETY: `get_time` is the firmware's `GetTime`, and `time` is a live local.
                let result = unsafe { get_time(&mut time, ptr::null_mut()) };
                expect_eq!(result, EFI_SUCCESS);
                expect_gt!(time.Year, 2000);
                expect_lt!(time.Year, 2100);
            } else {
                expect_true!(false, "EFI RuntimeServices->GetTime is null");
            }
        }
    }

    /// Tests that every descriptor of a well-formed table is visited.
    #[test]
    fn test_memory_attributes_table_valid() {
        const VALID_DATA: [u32; 24] = [
            0x2,                                     // header.version
            0x2,                                     // header.number_of_entries
            size_of::<EfiMemoryDescriptor>() as u32, // header.descriptor_size
            0,                                       // header.reserved
            // descriptor 0
            EFI_RUNTIME_SERVICES_CODE, // descriptor.Type
            0,                         // descriptor.Padding
            0,
            0x1000, // descriptor.PhysicalStart (64-bit)
            0,
            0, // descriptor.VirtualStart (64-bit)
            1,
            0, // descriptor.NumberOfPages (64-bit)
            0,
            0, // descriptor.Attribute (64-bit)
            // descriptor 1
            EFI_RUNTIME_SERVICES_CODE, // descriptor.Type
            0,                         // descriptor.Padding
            0,
            0x1000, // descriptor.PhysicalStart (64-bit)
            0,
            0, // descriptor.VirtualStart (64-bit)
            1,
            0, // descriptor.NumberOfPages (64-bit)
            0,
            0, // descriptor.Attribute (64-bit)
        ];

        let bytes: &[u8] = VALID_DATA.as_bytes();

        let mut callback_count: usize = 0;
        expect_ok!(for_each_memory_attribute_entry_safe(bytes, |_| {
            // The first descriptor is valid so this should be called at least once.
            callback_count += 1;
            Ok(())
        }));

        expect_eq!(2, callback_count);
    }

    /// Tests that a table shorter than the memory attributes header is rejected.
    #[test]
    fn test_memory_attributes_table_invalid() {
        const SHORT_DATA: [u8; 4] = [0xab, 0xab, 0xab, 0xab];
        let bytes: &[u8] = &SHORT_DATA;
        const _: () = assert!(
            SHORT_DATA.len() < size_of::<EfiMemoryAttributesTableHeader>(),
            "test data is not small enough"
        );

        expect_err!(for_each_memory_attribute_entry_safe(bytes, |_| Ok(())), Status::INVALID_ARGS);
    }

    /// Tests that a table truncated before its declared entry count is rejected.
    #[test]
    fn test_memory_attributes_table_truncated() {
        const TRUNCATED_DATA: [u32; 15] = [
            0x2,                                     // header.version
            0x10,                                    // header.number_of_entries
            size_of::<EfiMemoryDescriptor>() as u32, // header.descriptor_size
            0,                                       // header.reserved
            // descriptor 0
            EFI_RUNTIME_SERVICES_CODE, // descriptor.Type
            0,                         // descriptor.Padding
            0,
            0x1000, // descriptor.PhysicalStart (64-bit)
            0,
            0, // descriptor.VirtualStart (64-bit)
            1,
            0, // descriptor.NumberOfPages (64-bit)
            0,
            0, // descriptor.Attribute (64-bit)
            // descriptor 1
            EFI_RUNTIME_SERVICES_DATA,
        ];

        let bytes: &[u8] = TRUNCATED_DATA.as_bytes();

        let mut did_callback = false;
        expect_err!(
            for_each_memory_attribute_entry_safe(bytes, |_| {
                // The first descriptor is valid so this should be called at least once.
                did_callback = true;
                Ok(())
            }),
            Status::INVALID_ARGS
        );

        expect_true!(did_callback);
    }

    /// Tests that a header whose descriptor size is smaller than a descriptor is rejected.
    #[test]
    fn test_memory_attributes_table_short_descriptor() {
        const SHORT_DESCRIPTOR_DATA: [u32; 15] = [
            0x2,                                         // header.version
            0x10,                                        // header.number_of_entries
            size_of::<EfiMemoryDescriptor>() as u32 - 3, // header.descriptor_size
            0,                                           // header.reserved
            // descriptor 0
            EFI_RUNTIME_SERVICES_CODE, // descriptor.Type
            0,                         // descriptor.Padding
            0,
            0x1000, // descriptor.PhysicalStart (64-bit)
            0,
            0, // descriptor.VirtualStart (64-bit)
            1,
            0, // descriptor.NumberOfPages (64-bit)
            0,
            0, // descriptor.Attribute (64-bit)
            // descriptor 1
            EFI_RUNTIME_SERVICES_DATA,
        ];

        let bytes: &[u8] = SHORT_DESCRIPTOR_DATA.as_bytes();

        let mut did_callback = false;
        expect_err!(
            for_each_memory_attribute_entry_safe(bytes, |_| {
                // The descriptor size is too small, so this should never be called.
                did_callback = true;
                Ok(())
            }),
            Status::INVALID_ARGS
        );

        expect_false!(did_callback);
    }

    /// Tests that descriptors are visited with a stride larger than `EfiMemoryDescriptor`.
    #[test]
    fn test_memory_attributes_table_large_descriptor_size() {
        const LARGE_DESCRIPTOR_DATA: [u32; 28] = [
            0x2,                                         // header.version
            0x2,                                         // header.number_of_entries
            size_of::<EfiMemoryDescriptor>() as u32 + 8, // header.descriptor_size
            0,                                           // header.reserved
            // descriptor 0
            EFI_RUNTIME_SERVICES_CODE, // descriptor.Type
            0,                         // descriptor.Padding
            0,
            0x1000, // descriptor.PhysicalStart (64-bit)
            0,
            0, // descriptor.VirtualStart (64-bit)
            1,
            0, // descriptor.NumberOfPages (64-bit)
            0,
            0, // descriptor.Attribute (64-bit)
            0xdeadbeef,
            0xdeadbeef, // bytes past EfiMemoryDescriptor
            // descriptor 1
            EFI_RUNTIME_SERVICES_DATA, // descriptor.Type
            0,                         // descriptor.Padding
            0,
            0x1000, // descriptor.PhysicalStart (64-bit)
            0,
            0, // descriptor.VirtualStart (64-bit)
            1,
            0, // descriptor.NumberOfPages (64-bit)
            0,
            0, // descriptor.Attribute (64-bit)
            0xdeadbeef,
            0xdeadbeef, // bytes past EfiMemoryDescriptor
        ];

        let bytes: &[u8] = LARGE_DESCRIPTOR_DATA.as_bytes();

        let mut types = [0u32; 2];
        let mut callback_count: usize = 0;
        expect_ok!(for_each_memory_attribute_entry_safe(bytes, |desc| {
            if let Some(ty) = types.get_mut(callback_count) {
                *ty = desc.Type;
            }
            callback_count += 1;
            Ok(())
        }));

        expect_eq!(2, callback_count);
        expect_eq!(EFI_RUNTIME_SERVICES_CODE, types[0]);
        expect_eq!(EFI_RUNTIME_SERVICES_DATA, types[1]);
    }

    /// Tests that `set_variable` on an inactive guard returns `EFI_INVALID_PARAMETER`.
    #[test]
    fn test_set_variable_invalid_guard() {
        let services = EfiServicesActivation::default();
        expect_false!(services.valid());
        expect_eq!(EFI_INVALID_PARAMETER, services.set_variable(&[0], &EfiGuid::default(), 0, &[]));
    }

    /// Tests that `set_variable` rejects a `var_name` that is not NUL-terminated.
    #[test]
    #[sanitize(address = "off")]
    fn test_set_variable_invalid_var_name() {
        // SAFETY: `services` is scoped to this stack frame (strictly LIFO), the thread's previous
        // `VmAspace` is not switched or destroyed before `services` is dropped, and this function
        // is tagged `#[sanitize(address = "off")]`.
        let services = unsafe { try_activate_efi_services() };
        // We may not have EFI services.
        if services.valid() {
            expect_eq!(
                EFI_INVALID_PARAMETER,
                services.set_variable(&[], &EfiGuid::default(), 0, &[])
            );
            expect_eq!(
                EFI_INVALID_PARAMETER,
                services.set_variable(&[0x41], &EfiGuid::default(), 0, &[])
            );
        }
    }
}
