// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_LIB_ACPI_PCI_H_
#define SRC_DEVICES_BOARD_LIB_ACPI_PCI_H_

#include <lib/async/dispatcher.h>
#include <lib/ddk/device.h>

#include <vector>

#include <acpica/acpi.h>

#include "src/devices/board/lib/acpi/acpi.h"
namespace acpi {

class Manager;

}  // namespace acpi

// Publishes the PCI root described by |object|, serving it on |dispatcher|.
zx_status_t pci_init(zx_device_t* parent, async_dispatcher_t* dispatcher, ACPI_HANDLE object,
                     acpi::UniquePtr<ACPI_DEVICE_INFO> info, acpi::Manager* manager,
                     std::vector<ACPI_PCI_ID> acpi_bdfs);

#endif  // SRC_DEVICES_BOARD_LIB_ACPI_PCI_H_
