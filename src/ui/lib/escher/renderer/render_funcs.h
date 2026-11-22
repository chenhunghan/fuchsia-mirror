// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_RENDERER_RENDER_FUNCS_H_
#define SRC_UI_LIB_ESCHER_RENDERER_RENDER_FUNCS_H_

#include "src/ui/lib/escher/forward_declarations.h"
#include "src/ui/lib/escher/vk/image.h"
#include "src/ui/lib/escher/vk/texture.h"

#include <vulkan/vulkan.hpp>

namespace escher {

class RenderFuncs {
 public:
  // Updates or replaces the passed in depth texture (depth_texture_in_out) based on the provided
  // ImageInfo and vk::Format. If the texture pointer is null, a new texture will be allocated.
  static void ObtainDepthTexture(Escher* escher, const bool use_protected_memory,
                                 const ImageInfo& info, vk::Format depth_stencil_format,
                                 TexturePtr& depth_texture_in_out);
};

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_RENDERER_RENDER_FUNCS_H_
