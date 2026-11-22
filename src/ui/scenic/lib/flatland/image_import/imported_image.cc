// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#include "src/ui/scenic/lib/flatland/image_import/imported_image.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>

#include <algorithm>
#include <cstdint>
#include <functional>
#include <unordered_set>
#include <utility>

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/pmr_stack_arena.h"

namespace flatland {

ImportedImage::ImportedImage(const allocation::ImageMetadata& metadata, ImageReadinessPolicy policy,
                             const std::shared_ptr<utils::DispatcherHolder>& reply_dispatcher,
                             ImageImporter::FailureCallback on_failure)
    : metadata_(metadata),
      created_(zx::clock::get_monotonic()),
      policy_(policy),
      on_failure_(std::move(on_failure)),
      reply_dispatcher_(reply_dispatcher) {
  const auto word_addr = reinterpret_cast<uintptr_t>(&word_);
  const auto stamp_last_byte =
      reinterpret_cast<uintptr_t>(&frame_stamp_) + sizeof(frame_stamp_) - 1;
  FX_DCHECK(word_addr % kCacheLineBytes == 0);
  FX_DCHECK(word_addr / kCacheLineBytes == stamp_last_byte / kCacheLineBytes);
}

ImportedImage::~ImportedImage() {
  FX_CHECK(!gpu_image_);
  FX_CHECK(pending_.empty());
}

uint32_t ImportedImage::tiling_type() const {
  FX_DCHECK(utils::IsOnMainThread());
  FX_CHECK(display_state() == ImportState::kReady);
  FX_CHECK(collection_);
  return collection_->tiling_type();
}

ImportedImage::GpuImage* ImportedImage::gpu_image() const {
  FX_DCHECK(utils::IsOnMainThread());
  FX_CHECK(gpu_state() == ImportState::kReady);
  return gpu_image_.get();
}

namespace {
// Linear pointer scan stays faster than hashing up to 32 layer images.
constexpr size_t kLinearDedupMaxImages = 32;
}  // namespace

void DeduplicateImages(std::span<ImportedImage* const> images,
                       std::pmr::vector<ImportedImage*>& out) {
  TRACE_DURATION("gfx", "DeduplicateImages", "count", images.size());
  FX_CHECK(
      images.empty() || out.capacity() == 0 ||
      std::less_equal<const ImportedImage* const*>{}(images.data() + images.size(), out.data()) ||
      std::less_equal<const ImportedImage* const*>{}(out.data() + out.capacity(), images.data()))
      << "DeduplicateImages(): `images` must not overlap `out`'s buffer";
  out.clear();
  out.reserve(images.size());
  for (size_t i = 0; i < images.size(); ++i) {
    ImportedImage* img = images[i];
    if (img == nullptr) {
      continue;
    }
    if (std::ranges::find(out, img) != out.end()) {
      continue;
    }
    if (out.size() < kLinearDedupMaxImages) {
      out.push_back(img);
      continue;
    }
    utils::PmrStackArena<2048> set_arena(out.get_allocator().resource());
    std::pmr::unordered_set<ImportedImage*> seen(
        out.begin(), out.end(), images.size() - i + out.size(), set_arena.resource());
    seen.insert(img);
    out.push_back(img);
    for (++i; i < images.size(); ++i) {
      ImportedImage* rem = images[i];
      if (rem != nullptr && seen.insert(rem).second) {
        out.push_back(rem);
      }
    }
    return;
  }
}

}  // namespace flatland
