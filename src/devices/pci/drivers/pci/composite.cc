// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pci/composite.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/legacy-bind-constants/legacy-bind-constants.h>

#include <bind/fuchsia/acpi/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>

namespace pci {

fuchsia_driver_framework::CompositeNodeSpec CreateCompositeNodeSpec(std::string_view name,
                                                                    const CompositeInfo& info) {
  uint32_t kPciBindTopo = BIND_PCI_TOPO_PACK(info.bus_id, info.dev_id, info.func_id);

  const std::vector<fuchsia_driver_framework::BindRule2> kPciBindRules = {
      fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.pci.Service"),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_VID, info.vendor_id),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_DID, info.device_id),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_CLASS, info.class_id),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_SUBCLASS, info.subclass),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_INTERFACE, info.program_interface),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_REVISION, info.revision_id),
      fdf::MakeAcceptBindRule(bind_fuchsia::PCI_TOPO, kPciBindTopo),
      fdf::MakeRejectBindRule(bind_fuchsia::COMPOSITE, 1u),
  };

  const std::vector<fuchsia_driver_framework::NodeProperty2> kPciBindProperties = {
      fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.pci.Service"),
      fdf::MakeProperty2(bind_fuchsia::PCI_VID, info.vendor_id),
      fdf::MakeProperty2(bind_fuchsia::PCI_DID, info.device_id),
      fdf::MakeProperty2(bind_fuchsia::PCI_CLASS, info.class_id),
      fdf::MakeProperty2(bind_fuchsia::PCI_SUBCLASS, info.subclass),
      fdf::MakeProperty2(bind_fuchsia::PCI_INTERFACE, info.program_interface),
      fdf::MakeProperty2(bind_fuchsia::PCI_REVISION, info.revision_id),
      fdf::MakeProperty2(bind_fuchsia::PCI_TOPO, kPciBindTopo),
  };

  std::vector<fuchsia_driver_framework::ParentSpec2> parents;
  parents.push_back(fuchsia_driver_framework::ParentSpec2{{
      .bind_rules = kPciBindRules,
      .properties = kPciBindProperties,
  }});

  if (info.has_acpi) {
    const std::vector<fuchsia_driver_framework::BindRule2> kAcpiBindRules = {
        fdf::MakeAcceptBindRule(bind_fuchsia::PROTOCOL, bind_fuchsia_acpi::BIND_PROTOCOL_DEVICE),
        fdf::MakeAcceptBindRule(bind_fuchsia::ACPI_BUS_TYPE,
                                bind_fuchsia_acpi::BIND_ACPI_BUS_TYPE_PCI),
        fdf::MakeAcceptBindRule(bind_fuchsia::PCI_TOPO, kPciBindTopo),
    };

    const std::vector<fuchsia_driver_framework::NodeProperty2> kAcpiBindProperties = {
        fdf::MakeProperty2(bind_fuchsia::PROTOCOL, bind_fuchsia_acpi::BIND_PROTOCOL_DEVICE),
        fdf::MakeProperty2(bind_fuchsia::ACPI_BUS_TYPE, bind_fuchsia_acpi::BIND_ACPI_BUS_TYPE_PCI),
        fdf::MakeProperty2(bind_fuchsia::PCI_TOPO, kPciBindTopo),
    };

    parents.push_back(fuchsia_driver_framework::ParentSpec2{{
        .bind_rules = kAcpiBindRules,
        .properties = kAcpiBindProperties,
    }});
  }

  return fuchsia_driver_framework::CompositeNodeSpec{{
      .name = std::string(name),
      .parents2 = std::move(parents),
  }};
}

}  // namespace pci
