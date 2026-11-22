// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/lib/acpi/pci-root-windows.h"

#include <lib/fake-resource/resource.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/resource.h>
#include <lib/zx/time.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include <cstdint>
#include <optional>

#include <zxtest/zxtest.h>

namespace acpi {
namespace {

using Space = PciRootWindows::Space;

constexpr uint64_t kLowBase = 0x8000'0000;
constexpr uint64_t kLowSize = 0x1000'0000;
constexpr uint64_t kHighBase = 0x1'0000'0000;
constexpr uint64_t kHighSize = 0x1000'0000;

class PciRootWindowsTest : public zxtest::Test {
 protected:
  void SetUp() override {
    ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_MMIO, mmio_.reset_and_get_address()));
    ASSERT_OK(fake_resource_create(ZX_RSRC_KIND_IOPORT, ioport_.reset_and_get_address()));
    windows_.emplace(mmio_.borrow(), ioport_.borrow());
    page_size_ = zx_system_get_page_size();
  }

  PciRootWindows& windows() { return *windows_; }
  uint64_t page_size() const { return page_size_; }
  zx::unowned_resource mmio() const { return mmio_.borrow(); }
  zx::unowned_resource ioport() const { return ioport_.borrow(); }

  static zx_info_resource_t ResourceInfo(const zx::resource& resource) {
    zx_info_resource_t info{};
    EXPECT_OK(resource.get_info(ZX_INFO_RESOURCE, &info, sizeof(info), nullptr, nullptr));
    return info;
  }

 private:
  zx::resource mmio_;
  zx::resource ioport_;
  std::optional<PciRootWindows> windows_;
  uint64_t page_size_ = 0;
};

TEST_F(PciRootWindowsTest, AnyBaseIsNaturallyAligned) {
  ASSERT_OK(windows().AddWindow(Space::kMemory, kLowBase + page_size(), kLowSize - page_size()));

  const size_t size = page_size() * 4;
  zx::result allocation = windows().Allocate(Space::kMemory, 0, size, /*low=*/true);
  ASSERT_OK(allocation);
  EXPECT_EQ(allocation->base % size, 0u);
  EXPECT_GE(allocation->base, kLowBase + page_size());
}

TEST_F(PciRootWindowsTest, MemoryIsPageAligned) {
  ASSERT_OK(windows().AddWindow(Space::kMemory, kLowBase, kLowSize));

  // Unaligned bases are rejected so the bus driver will pick a new one.
  EXPECT_STATUS(windows().Allocate(Space::kMemory, kLowBase + 0x10, 0x10, /*low=*/true),
                ZX_ERR_INVALID_ARGS);

  // Sub-page sizes are rounded up to a page, so two small allocations don't share a page.
  zx::result first = windows().Allocate(Space::kMemory, 0, 0x10, /*low=*/true);
  ASSERT_OK(first);
  EXPECT_EQ(ResourceInfo(first->resource).size, page_size());
  zx::result second = windows().Allocate(Space::kMemory, 0, 0x10, /*low=*/true);
  ASSERT_OK(second);
  EXPECT_NE(first->base, second->base);
  EXPECT_EQ(second->base % page_size(), 0u);
}

TEST_F(PciRootWindowsTest, WindowsAreSplitAt4GiB) {
  constexpr uint64_t kBase = 0xE000'0000;
  ASSERT_OK(windows().AddWindow(Space::kMemory, kBase, kHighBase - kBase));
  ASSERT_OK(windows().AddWindow(Space::kMemory, kHighBase, kHighSize));

  EXPECT_OK(windows().Allocate(Space::kMemory, kBase, page_size(), /*low=*/true));

  // A high range isn't in the low window, and a low range isn't in the high window.
  EXPECT_STATUS(windows().Allocate(Space::kMemory, kHighBase, page_size(), /*low=*/true),
                ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(windows().Allocate(Space::kMemory, kBase, page_size(), /*low=*/false),
                ZX_ERR_NOT_FOUND);

  zx::result high = windows().Allocate(Space::kMemory, 0, page_size(), /*low=*/false);
  ASSERT_OK(high);
  EXPECT_GE(high->base, kHighBase);
}

TEST_F(PciRootWindowsTest, HighFallsBackToLowWithoutBase) {
  ASSERT_OK(windows().AddWindow(Space::kMemory, kLowBase, kLowSize));

  zx::result allocation = windows().Allocate(Space::kMemory, 0, page_size(), /*low=*/false);
  ASSERT_OK(allocation);
  EXPECT_GE(allocation->base, kLowBase);
  EXPECT_LT(allocation->base, kLowBase + kLowSize);
}

TEST_F(PciRootWindowsTest, SubtractedRangesAreUnavailable) {
  ASSERT_OK(windows().AddWindow(Space::kMemory, kLowBase, kLowSize));
  ASSERT_OK(windows().SubtractWindow(Space::kMemory, kLowBase, page_size()));
  // Ranges outside any window are ignored.
  ASSERT_OK(windows().SubtractWindow(Space::kMemory, 0x1000, page_size()));

  EXPECT_STATUS(windows().Allocate(Space::kMemory, kLowBase, page_size(), /*low=*/true),
                ZX_ERR_NOT_FOUND);
  EXPECT_OK(windows().Allocate(Space::kMemory, kLowBase + page_size(), page_size(), /*low=*/true));
}

TEST_F(PciRootWindowsTest, IoAllocation) {
  ASSERT_OK(windows().AddWindow(Space::kIo, 0x1000, 0x1000));

  // IO ranges have no page alignment requirement.
  zx::result allocation = windows().Allocate(Space::kIo, 0x1010, 0x10, /*low=*/false);
  ASSERT_OK(allocation);
  EXPECT_EQ(allocation->base, 0x1010u);
  zx_info_resource_t info = ResourceInfo(allocation->resource);
  EXPECT_EQ(info.kind, ZX_RSRC_KIND_IOPORT);
  EXPECT_EQ(info.size, 0x10u);

  // Memory and IO windows are separate.
  EXPECT_STATUS(windows().Allocate(Space::kMemory, 0, 0x10, /*low=*/true), ZX_ERR_NOT_FOUND);
}

TEST_F(PciRootWindowsTest, ClosingTokenReleasesAllocation) {
  ASSERT_OK(windows().AddWindow(Space::kMemory, kLowBase, kLowSize));

  zx::result allocation = windows().Allocate(Space::kMemory, kLowBase, page_size(), /*low=*/true);
  ASSERT_OK(allocation);
  EXPECT_EQ(allocation->base, kLowBase);
  zx_info_resource_t info = ResourceInfo(allocation->resource);
  EXPECT_EQ(info.kind, ZX_RSRC_KIND_MMIO);
  EXPECT_EQ(info.base, kLowBase);
  EXPECT_EQ(info.size, page_size());
  ASSERT_TRUE(allocation->token.is_valid());
  EXPECT_STATUS(
      allocation->token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), nullptr),
      ZX_ERR_TIMED_OUT);

  // While held, allocating the same range fails.
  EXPECT_STATUS(windows().Allocate(Space::kMemory, kLowBase, page_size(), /*low=*/true),
                ZX_ERR_NOT_FOUND);

  // Closing the token releases the allocation back to the window.
  allocation->token.reset();
  allocation->resource.reset();
  zx::result reallocated = windows().Allocate(Space::kMemory, kLowBase, page_size(), /*low=*/true);
  EXPECT_OK(reallocated);
}

TEST_F(PciRootWindowsTest, DestroyingWindowsWithLiveAllocationReleasesRegion) {
  std::optional<PciRootWindows::Allocation> live_allocation;
  {
    PciRootWindows inner_windows(mmio(), ioport());
    ASSERT_OK(inner_windows.AddWindow(Space::kMemory, kLowBase, kLowSize));
    zx::result allocation =
        inner_windows.Allocate(Space::kMemory, kLowBase, page_size(), /*low=*/true);
    ASSERT_OK(allocation);
    ASSERT_TRUE(allocation->token.is_valid());
    live_allocation = std::move(*allocation);
  }
  EXPECT_TRUE(live_allocation->token.is_valid());
  EXPECT_STATUS(
      live_allocation->token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(), nullptr),
      ZX_OK);
}

TEST_F(PciRootWindowsTest, MemoryWindowStraddling4GiB) {
  // A memory window spanning from below 4GiB to above 4GiB is split into low and high windows.
  constexpr uint64_t kBase = 0xE000'0000;
  constexpr uint64_t kSize = 0x4000'0000;  // Spans up to 0x1'2000'0000
  ASSERT_OK(windows().AddWindow(Space::kMemory, kBase, kSize));

  // The low part is available to low allocations.
  EXPECT_OK(windows().Allocate(Space::kMemory, kBase, page_size(), /*low=*/true));
  // The high part is available to high allocations.
  const uint64_t high_alloc = kHighBase + page_size() * 10;
  EXPECT_OK(windows().Allocate(Space::kMemory, high_alloc, page_size(), /*low=*/false));

  // Subtracting a range straddling 4GiB removes parts from both low and high windows.
  const uint64_t subtract_base = kHighBase - page_size() * 2;
  const uint64_t subtract_size = page_size() * 4;
  ASSERT_OK(windows().SubtractWindow(Space::kMemory, subtract_base, subtract_size));

  EXPECT_STATUS(windows().Allocate(Space::kMemory, subtract_base, page_size(), /*low=*/true),
                ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(windows().Allocate(Space::kMemory, kHighBase, page_size(), /*low=*/false),
                ZX_ERR_NOT_FOUND);
}

TEST_F(PciRootWindowsTest, OverflowRejection) {
  ASSERT_OK(windows().AddWindow(Space::kMemory, kLowBase, kLowSize));
  EXPECT_STATUS(windows().Allocate(Space::kMemory, 0, 0, /*low=*/true), ZX_ERR_INVALID_ARGS);

  EXPECT_STATUS(windows().AddWindow(Space::kMemory, UINT64_MAX - 0x10, 0x20), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(windows().SubtractWindow(Space::kMemory, UINT64_MAX - 0x10, 0x20),
                ZX_ERR_INVALID_ARGS);

  // Allocate with size near SIZE_MAX overflows page round-up.
  EXPECT_STATUS(windows().Allocate(Space::kMemory, 0, SIZE_MAX - page_size() + 2, /*low=*/true),
                ZX_ERR_INVALID_ARGS);
}

}  // namespace
}  // namespace acpi
