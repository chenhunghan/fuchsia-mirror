// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "pciroot.h"

#include <lib/driver/fake-resource/cpp/fake-resource.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/pci/root_host.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>
#include <zircon/compiler.h>
#include <zircon/rights.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/resource.h>

#include <string_view>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace {

struct RecordedInterruptCreate {
  zx_handle_t resource = ZX_HANDLE_INVALID;
  uint32_t vector = 0;
  uint32_t options = 0;
};

std::vector<RecordedInterruptCreate> g_created_interrupts;

}  // namespace

__BEGIN_CDECLS

// Interpose zx_interrupt_create so VirtPciroot::CreateInterrupts() exercises its production
// resource, vector, and kVirtPcieIrqOptions arguments while backing the returned handles with
// kernel virtual interrupts.
__EXPORT
zx_status_t zx_interrupt_create(zx_handle_t src_obj, uint32_t src_num, uint32_t options,
                                zx_handle_t* out_handle) {
  zx_info_resource_t info{};
  zx_status_t status =
      zx_object_get_info(src_obj, ZX_INFO_RESOURCE, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    return status;
  }
  if (info.kind != ZX_RSRC_KIND_IRQ) {
    return ZX_ERR_WRONG_TYPE;
  }
  g_created_interrupts.push_back({
      .resource = src_obj,
      .vector = src_num,
      .options = options,
  });
  return _zx_interrupt_create(ZX_HANDLE_INVALID, src_num, ZX_INTERRUPT_VIRTUAL, out_handle);
}

__END_CDECLS

namespace board_virt {
namespace {

zx_koid_t GetKoid(zx_handle_t handle) {
  zx_info_handle_basic_t info{};
  zx_status_t status =
      zx_object_get_info(handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    return ZX_KOID_INVALID;
  }
  return info.koid;
}

TEST(VirtPcirootTest, GetPciPlatformInfoDuplicatesHandlesAndPopulatesRouting) {
  fdf_testing::ScopedGlobalLogger logger;
  g_created_interrupts.clear();

  zx::resource fake_msi_resource;
  zx::resource fake_mmio_resource;
  zx::resource fake_irq_resource;
  zx::resource fake_iommu_resource;
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, fake_msi_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_MMIO, fake_mmio_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_IRQ, fake_irq_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, fake_iommu_resource.reset_and_get_address()));
  const zx_handle_t expected_irq_resource = fake_irq_resource.get();

  PciRootHost root_host(fake_msi_resource.borrow(), fake_mmio_resource.borrow(),
                        zx::unowned_resource(), PCI_ADDRESS_SPACE_MEMORY);

  zx::vmo ecam;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &ecam));
  const zx_koid_t expected_ecam_koid = GetKoid(ecam.get());
  ASSERT_NE(expected_ecam_koid, ZX_KOID_INVALID);

  VirtPciroot pciroot(&root_host, std::move(ecam), std::move(fake_irq_resource),
                      std::move(fake_iommu_resource));
  ASSERT_OK(pciroot.CreateInterrupts().status_value());

  ASSERT_EQ(g_created_interrupts.size(), kVirtPcieIrqCount);
  for (size_t i = 0; i < g_created_interrupts.size(); i++) {
    EXPECT_EQ(g_created_interrupts[i].resource, expected_irq_resource);
    EXPECT_EQ(g_created_interrupts[i].vector, static_cast<uint32_t>(kVirtPcieIrqBase + i));
    EXPECT_EQ(g_created_interrupts[i].options, kVirtPcieIrqOptions);
  }

  pci_platform_info_t info1{
      .acpi_bdfs_count = 3,
      .devicetree_bdfs_count = 2,
  };
  ASSERT_OK(pciroot.PcirootGetPciPlatformInfo(&info1));

  EXPECT_EQ(info1.acpi_bdfs_count, 0u);
  EXPECT_EQ(info1.devicetree_bdfs_count, 0u);
  EXPECT_EQ(info1.start_bus_num, kVirtPcieMcfg.start_bus_number);
  EXPECT_EQ(info1.end_bus_num, kVirtPcieMcfg.end_bus_number);
  EXPECT_EQ(info1.segment_group, kVirtPcieMcfg.pci_segment);
  EXPECT_EQ(std::string_view(info1.name), kPcirootName);
  EXPECT_TRUE(info1.cam.is_extended);
  EXPECT_EQ(GetKoid(info1.cam.vmo), expected_ecam_koid);

  ASSERT_EQ(info1.legacy_irqs_count, kVirtPcieIrqCount);
  std::vector<zx_handle_t> first_handles(info1.legacy_irqs_count);
  std::vector<zx_koid_t> irq_koids(info1.legacy_irqs_count);
  for (size_t i = 0; i < info1.legacy_irqs_count; i++) {
    EXPECT_EQ(info1.legacy_irqs_list[i].vector, static_cast<uint32_t>(kVirtPcieIrqBase + i));
    first_handles[i] = info1.legacy_irqs_list[i].interrupt;
    ASSERT_NE(first_handles[i], ZX_HANDLE_INVALID);
    irq_koids[i] = GetKoid(first_handles[i]);
    ASSERT_NE(irq_koids[i], ZX_KOID_INVALID);
  }

  ASSERT_EQ(info1.irq_routing_count, static_cast<size_t>(DEVICES_PER_BUS));
  for (uint8_t device_id = 0; device_id < DEVICES_PER_BUS; device_id++) {
    const pci_irq_routing_entry_t& entry = info1.irq_routing_list[device_id];
    EXPECT_EQ(entry.port_device_id, PCI_IRQ_ROUTING_NO_PARENT);
    EXPECT_EQ(entry.port_function_id, PCI_IRQ_ROUTING_NO_PARENT);
    EXPECT_EQ(entry.device_id, device_id);
    for (uint32_t pin = 0; pin < PINS_PER_FUNCTION; pin++) {
      EXPECT_EQ(entry.pins[pin],
                static_cast<uint32_t>(kVirtPcieIrqBase + ((pin + device_id) % kVirtPcieIrqCount)));
    }
  }

  // Closing the handles returned by the first call must not invalidate the owned interrupts.
  zx_handle_close(info1.cam.vmo);
  for (zx_handle_t h : first_handles) {
    zx_handle_close(h);
  }

  pci_platform_info_t info2{};
  ASSERT_OK(pciroot.PcirootGetPciPlatformInfo(&info2));
  const zx_handle_t second_ecam_handle = info2.cam.vmo;
  EXPECT_EQ(GetKoid(second_ecam_handle), expected_ecam_koid);
  ASSERT_EQ(info2.legacy_irqs_count, kVirtPcieIrqCount);
  std::vector<zx_handle_t> second_handles(info2.legacy_irqs_count);
  for (size_t i = 0; i < info2.legacy_irqs_count; i++) {
    EXPECT_EQ(info2.legacy_irqs_list[i].vector, static_cast<uint32_t>(kVirtPcieIrqBase + i));
    second_handles[i] = info2.legacy_irqs_list[i].interrupt;
    EXPECT_NE(second_handles[i], ZX_HANDLE_INVALID);
    EXPECT_EQ(GetKoid(second_handles[i]), irq_koids[i]);
  }

  // Calling a third time while info2's handles are still open must return distinct handle values.
  pci_platform_info_t info3{};
  ASSERT_OK(pciroot.PcirootGetPciPlatformInfo(&info3));
  EXPECT_NE(info3.cam.vmo, second_ecam_handle);
  EXPECT_EQ(GetKoid(info3.cam.vmo), expected_ecam_koid);
  ASSERT_EQ(info3.legacy_irqs_count, kVirtPcieIrqCount);
  for (size_t i = 0; i < info3.legacy_irqs_count; i++) {
    EXPECT_NE(info3.legacy_irqs_list[i].interrupt, second_handles[i]);
    EXPECT_EQ(GetKoid(info3.legacy_irqs_list[i].interrupt), irq_koids[i]);
  }

  zx_handle_close(second_ecam_handle);
  for (zx_handle_t h : second_handles) {
    zx_handle_close(h);
  }
  zx_handle_close(info3.cam.vmo);
  for (size_t i = 0; i < info3.legacy_irqs_count; i++) {
    zx_handle_close(info3.legacy_irqs_list[i].interrupt);
  }
}

TEST(VirtPcirootTest, GetPciPlatformInfoFailsWhenEcamCannotBeDuplicated) {
  fdf_testing::ScopedGlobalLogger logger(FUCHSIA_LOG_FATAL);
  g_created_interrupts.clear();

  zx::resource fake_msi_resource;
  zx::resource fake_mmio_resource;
  zx::resource fake_irq_resource;
  zx::resource fake_iommu_resource;
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, fake_msi_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_MMIO, fake_mmio_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_IRQ, fake_irq_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, fake_iommu_resource.reset_and_get_address()));

  PciRootHost root_host(fake_msi_resource.borrow(), fake_mmio_resource.borrow(),
                        zx::unowned_resource(), PCI_ADDRESS_SPACE_MEMORY);

  zx::vmo ecam;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &ecam));
  ASSERT_OK(ecam.replace(ZX_RIGHT_READ, &ecam));

  VirtPciroot pciroot(&root_host, std::move(ecam), std::move(fake_irq_resource),
                      std::move(fake_iommu_resource));
  ASSERT_OK(pciroot.CreateInterrupts().status_value());

  pci_platform_info_t info{};
  EXPECT_STATUS(pciroot.PcirootGetPciPlatformInfo(&info), ZX_ERR_ACCESS_DENIED);
  EXPECT_EQ(info.cam.vmo, ZX_HANDLE_INVALID);
  EXPECT_EQ(info.legacy_irqs_count, 0u);
}

TEST(VirtPcirootTest, CreateInterruptsFailsWithWrongResourceKind) {
  fdf_testing::ScopedGlobalLogger logger;
  g_created_interrupts.clear();

  zx::resource fake_msi_resource;
  zx::resource fake_mmio_resource;
  zx::resource wrong_irq_resource;
  zx::resource fake_iommu_resource;
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, fake_msi_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_MMIO, fake_mmio_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_MMIO, wrong_irq_resource.reset_and_get_address()));
  ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_SYSTEM, fake_iommu_resource.reset_and_get_address()));

  PciRootHost root_host(fake_msi_resource.borrow(), fake_mmio_resource.borrow(),
                        zx::unowned_resource(), PCI_ADDRESS_SPACE_MEMORY);

  zx::vmo ecam;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &ecam));

  VirtPciroot pciroot(&root_host, std::move(ecam), std::move(wrong_irq_resource),
                      std::move(fake_iommu_resource));
  EXPECT_STATUS(pciroot.CreateInterrupts().status_value(), ZX_ERR_WRONG_TYPE);
  EXPECT_TRUE(g_created_interrupts.empty());
}

}  // namespace
}  // namespace board_virt
