// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#include "src/ui/scenic/lib/flatland/image_import/imported_collection.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/syscalls.h>

namespace flatland {

ImportedCollection::ImportedCollection(allocation::GlobalBufferCollectionId id) : id_(id) {}

ImportedCollection::~ImportedCollection() {
  FX_CHECK(!gpu_collection_);
  FX_CHECK(!allocation_participant_.is_valid());
  FX_CHECK(!display_participant_.is_valid());
  FX_CHECK(waiting_for_allocation_.empty());
  FX_CHECK(live_images_ == 0);
}

uint32_t ImportedCollection::tiling_type() const {
  FX_CHECK(state() == State::kDisplayCapable);
  return tiling_type_;
}

uint64_t ImportedCollection::per_buffer_bytes() const {
  const State s = state();
  FX_CHECK(s == State::kDisplayCapable || s == State::kGpuOnly);
  return per_buffer_bytes_;
}

// static
uint64_t ImportedCollection::VmoBytesPerBuffer(uint64_t sysmem_size_bytes) {
  const uint64_t page_size = zx_system_get_page_size();
  return (sysmem_size_bytes + page_size - 1) / page_size * page_size;
}

}  // namespace flatland
