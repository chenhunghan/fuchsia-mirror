// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/lib/acpi/test/mock-pci.h"

#include <vector>

#include <acpica/acpi.h>

#include "src/devices/board/lib/acpi/pci.h"

namespace acpi::test {
namespace {
std::vector<ACPI_PCI_ID> stored_acpi_bdfs;

}

std::vector<ACPI_PCI_ID> GetAcpiBdfs() { return std::move(stored_acpi_bdfs); }
}  // namespace acpi::test

zx_status_t pci_init(zx_device_t* parent, async_dispatcher_t* dispatcher, ACPI_HANDLE object,
                     acpi::UniquePtr<ACPI_DEVICE_INFO> info, acpi::Manager* manager,
                     std::vector<ACPI_PCI_ID> acpi_bdfs) {
  acpi::test::stored_acpi_bdfs = std::move(acpi_bdfs);
  return ZX_OK;
}
