// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

pub use efi_bindings::{
    EFI_VARIABLE_BOOTSERVICE_ACCESS, EFI_VARIABLE_NON_VOLATILE, EFI_VARIABLE_RUNTIME_ACCESS,
    ZIRCON_CRASHLOG_EFIATTR, kEfiZirconCrashlogEfivar as kZirconCrashlogEfivar,
    kEfiZirconVendorGuid as kZirconVendorGuid,
};

pub type EfiGuid = efi_bindings::efi_guid;
pub type EfiStatus = efi_bindings::efi_status;
pub type EfiMemoryAttributesTableHeader = efi_bindings::efi_memory_attributes_table_header;
pub type EfiMemoryDescriptor = efi_bindings::efi_memory_descriptor;
pub type EfiMemoryType = efi_bindings::efi_memory_type;
pub type EfiRuntimeServices = efi_bindings::efi_runtime_services;
pub type EfiSystemTable = efi_bindings::efi_system_table;
pub type EfiTime = efi_bindings::efi_time;

pub const EFI_SUCCESS: EfiStatus = efi_bindings::kEfiSuccess;
pub const EFI_INVALID_PARAMETER: EfiStatus = efi_bindings::kEfiInvalidParameter;
pub const EFI_NOT_FOUND: EfiStatus = efi_bindings::kEfiNotFound;
pub const EFI_UNSUPPORTED: EfiStatus = efi_bindings::kEfiUnsupported;

pub const EFI_MEMORY_RUNTIME: u64 = efi_bindings::kEfiMemoryRuntime;
pub const EFI_MEMORY_RO: u64 = efi_bindings::kEfiMemoryRo;
pub const EFI_MEMORY_XP: u64 = efi_bindings::kEfiMemoryXp;

pub const EFI_RUNTIME_SERVICES_CODE: EfiMemoryType =
    efi_bindings::efi_memory_type_EfiRuntimeServicesCode;
pub const EFI_RUNTIME_SERVICES_DATA: EfiMemoryType =
    efi_bindings::efi_memory_type_EfiRuntimeServicesData;
pub const EFI_MEMORY_MAPPED_IO: EfiMemoryType = efi_bindings::efi_memory_type_EfiMemoryMappedIO;
