// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_LIB_ACPI_TEST_MOCK_PCI_H_
#define SRC_DEVICES_BOARD_LIB_ACPI_TEST_MOCK_PCI_H_

#include <vector>

#include <acpica/acpi.h>

#include "src/devices/board/lib/acpi/pci.h"

namespace acpi::test {

// Returns the list of BDFs passed to pci_init().
std::vector<ACPI_PCI_ID> GetAcpiBdfs();
}  // namespace acpi::test

#endif  // SRC_DEVICES_BOARD_LIB_ACPI_TEST_MOCK_PCI_H_
