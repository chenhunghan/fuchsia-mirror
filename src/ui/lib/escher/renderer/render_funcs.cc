// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/renderer/render_funcs.h"

#include "src/ui/lib/escher/escher.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/util/trace_macros.h"
#include "src/ui/lib/escher/vk/render_pass.h"
#include "src/ui/lib/escher/vk/texture.h"

#include <vulkan/vulkan.hpp>

namespace escher {

void RenderFuncs::ObtainDepthTexture(Escher* escher, const bool use_protected_memory,
                                     const ImageInfo& info, vk::Format depth_stencil_format,
                                     TexturePtr& depth_texture) {
  // Support for other sample_counts should fairly easy to add, if necessary.
  FX_DCHECK(info.sample_count == 1);

  const bool realloc_textures =
      !depth_texture || (depth_texture->image()->use_protected_memory() != use_protected_memory) ||
      info.width != depth_texture->width() || info.height != depth_texture->height();

  // If the depth buffer does not exist, or if the depth buffer has a different
  // size than the output buffer, recreate it.
  if (realloc_textures) {
    // Need to generate a new depth buffer.
    {
      TRACE_DURATION("gfx", "RenderFuncs::ObtainDepthAndMsaaTextures (new depth)");
      depth_texture = escher->NewAttachmentTexture(
          depth_stencil_format, info.width, info.height, 1, vk::Filter::eLinear,
          vk::ImageUsageFlags(), /*use_unnormalized_coordinates=*/false,
          use_protected_memory ? vk::MemoryPropertyFlagBits::eProtected
                               : vk::MemoryPropertyFlags());
    }
  }
}
}  // namespace escher
