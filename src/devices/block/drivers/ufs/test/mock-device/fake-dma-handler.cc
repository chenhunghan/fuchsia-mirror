// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fake-dma-handler.h"

#include <lib/zx/vmar.h>
#include <zircon/assert.h>

namespace ufs {
namespace ufs_mock_device {

FakeDmaHandler::FakeDmaHandler() {
  for (size_t i = 0; i < std::size(fake_bti_paddrs_); i++) {
    fake_bti_paddrs_[i] = FAKE_BTI_PHYS_ADDR * (i + 1);
  }

  zx::result bti = fake_bti::CreateFakeBtiWithPaddrs(fake_bti_paddrs_);
  ZX_ASSERT(bti.is_ok());
  fake_bti_ = std::move(*bti);
}

FakeDmaHandler::~FakeDmaHandler() { Reset(); }

void FakeDmaHandler::Reset() {
  for (const auto& reg : mapped_addrs_) {
    zx::vmar::root_self()->unmap(reg.vaddr, reg.size);
  }
  mapped_addrs_.clear();

  fake_bti_.reset();
  zx::result bti = fake_bti::CreateFakeBtiWithPaddrs(fake_bti_paddrs_);
  ZX_ASSERT(bti.is_ok());
  fake_bti_ = std::move(*bti);
}

zx::result<zx_vaddr_t> FakeDmaHandler::PhysToVirt(zx_paddr_t paddr) {
  uint64_t page_offset = paddr % zx_system_get_page_size();
  zx_paddr_t base_paddr = paddr - page_offset;

  for (const auto& reg : mapped_addrs_) {
    for (size_t k = 0; k < reg.paddrs.size(); ++k) {
      if (reg.paddrs[k] == base_paddr) {
        return zx::ok(reg.vaddr + k * zx_system_get_page_size() + page_offset);
      }
    }
  }

  if (base_paddr % FAKE_BTI_PHYS_ADDR != 0 || base_paddr == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (base_paddr > FAKE_BTI_PHYS_ADDR * kFakeBtiAddrsCount) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  zx::result vmo_infos = fake_bti::GetPinnedVmo(fake_bti_.borrow());
  if (vmo_infos.is_error()) {
    return vmo_infos.take_error();
  }

  for (ssize_t i = static_cast<ssize_t>(vmo_infos->size()) - 1; i >= 0; --i) {
    auto& vmo_info = (*vmo_infos)[i];
    zx::result paddrs = fake_bti::GetVmoPhysAddress(fake_bti_.borrow(), vmo_info);
    if (paddrs.is_error()) {
      return paddrs.take_error();
    }

    for (size_t paddr_index = 0; paddr_index < paddrs->size(); ++paddr_index) {
      if (base_paddr == (*paddrs)[paddr_index]) {
        zx_vaddr_t vaddr;
        if (zx_status_t status =
                zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo_info.vmo,
                                           vmo_info.offset, vmo_info.size, &vaddr);
            status != ZX_OK) {
          return zx::error(status);
        }
        mapped_addrs_.push_back({vaddr, vmo_info.size, std::move(*paddrs)});
        return zx::ok(vaddr + paddr_index * zx_system_get_page_size() + page_offset);
      }
    }
  }

  return zx::error(ZX_ERR_NOT_FOUND);
}

}  // namespace ufs_mock_device
}  // namespace ufs
