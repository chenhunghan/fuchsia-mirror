// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_IMPL_NAIVE_IMAGE_H_
#define SRC_UI_LIB_ESCHER_IMPL_NAIVE_IMAGE_H_

#include "src/ui/lib/escher/vk/image.h"

namespace escher {
namespace impl {

class NaiveImage : public Image {
 public:
  // Constructor. Claims ownership of the vk::Image, and binds it to the
  // provided GpuMemPtr.
  static ImagePtr AdoptVkImage(ResourceManager* image_owner, ImageInfo info, vk::Image image,
                               GpuMemPtr mem, vk::ImageLayout initial_layout);

  // Takes ownership of `image`, a `vk::Image` already created on
  // `image_owner->vk_device()` and bound to `mem->base()` at `mem->offset()`,
  // and destroys it on that device when the returned `NaiveImage` is destroyed.
  // Makes no Vulkan calls and never returns `nullptr`. Aborts via `FX_CHECK` on
  // null `image_owner`, `image_owner->vk_device()`, `image`, `mem`, or
  // `mem->base()`, or when `info.is_transient()` does not match
  // `vk::MemoryPropertyFlagBits::eLazilyAllocated` in `info.memory_flags`.
  static ImagePtr AdoptBoundVkImage(ResourceManager* image_owner, ImageInfo info, vk::Image image,
                                    GpuMemPtr mem, vk::ImageLayout initial_layout);

  // Destroys image_ and releases the reference to mem_.
  ~NaiveImage() override;

  // |Image|
  vk::DeviceSize GetDeviceMemoryCommitment() override;

  const GpuMemPtr& memory() const { return mem_; }

 private:
  // Constructor. `mem` must be non-null and own the memory backing `image`.
  NaiveImage(ResourceManager* image_owner, ImageInfo info, vk::Image image, GpuMemPtr mem,
             vk::ImageLayout initial_layout);

  const GpuMemPtr mem_;
};

}  // namespace impl
}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_IMPL_NAIVE_IMAGE_H_
