// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_COMPOSITE_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_COMPOSITE_H_

#include <fidl/fuchsia.driver.framework/cpp/natural_types.h>

#include <cstdint>

namespace pci {

struct CompositeInfo {
  uint32_t vendor_id;
  uint32_t device_id;
  uint32_t class_id;
  uint32_t subclass;
  uint32_t program_interface;
  uint32_t revision_id;

  uint32_t segment_group;
  uint32_t bus_id;
  uint32_t dev_id;
  uint32_t func_id;

  bool has_acpi;
};

// Builds the composite node spec for a PCI function. The spec name and PCI bind rules include
// the segment group so functions with the same BDF in different segments get distinct specs.
fuchsia_driver_framework::CompositeNodeSpec CreateCompositeNodeSpec(const CompositeInfo& info);

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_COMPOSITE_H_
