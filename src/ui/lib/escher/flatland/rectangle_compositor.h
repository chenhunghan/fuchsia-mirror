// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_FLATLAND_RECTANGLE_COMPOSITOR_H_
#define SRC_UI_LIB_ESCHER_FLATLAND_RECTANGLE_COMPOSITOR_H_

#include <initializer_list>
#include <span>
#include <vector>

#include "src/ui/lib/escher/flatland/flatland_static_config.h"
#include "src/ui/lib/escher/forward_declarations.h"
#include "src/ui/lib/escher/geometry/types.h"
#include "src/ui/lib/escher/vk/shader_program.h"
#include "src/ui/lib/escher/vk/texture.h"

namespace escher {

// |RectangleCompositor| provides an interface for rendering axis-aligned rectangles in 2D space,
// to support the implementation of the `fuchsia.ui.composition/Flatland` API.
//
// It composites opaque and (premultiplied/straight) images to generate a premultiplied output
// image.
class RectangleCompositor {
 public:
  static const vk::ImageUsageFlags kRenderTargetUsageFlags;
  static const vk::ImageUsageFlags kTextureUsageFlags;

  enum class Opacity {
    Opaque,
    Translucent,
    NonPremultipliedTranslucent,
  };

  struct ColorData {
    ColorData(vec4 in_color, Opacity opacity) : color(in_color), opacity(opacity) {
      FX_CHECK(glm::all(glm::greaterThanEqual(in_color, vec4(0.f))));
      FX_CHECK(glm::all(glm::lessThanEqual(in_color, vec4(1.f))));
    }

    // RGBA
    const vec4 color = vec4(1.f);
    const Opacity opacity = Opacity::Translucent;
  };

  explicit RectangleCompositor(EscherWeakPtr escher);
  ~RectangleCompositor() = default;

  static constexpr uint32_t kTransientTargetAttachmentIndex = 0;
  static constexpr uint32_t kOutputTargetAttachmentIndex = 1;

  // Draws a single batch of renderables into the provided output image.
  // Parameters:
  // - cmd_buf: The command buffer used to record commands.
  // - rectangles: geometry to be drawn.
  // - textures: must be 1-1 with rectangles, to which they are textured onto.
  // - color_data: must be 1-1 with rectangles and textures.
  //             |color| is multiply_color to the texture used in the shader.
  //             |opacity| determines use of opaque or transparent rendering.
  // - output_image: the render target the renderables will be rendered into.
  // - depth_buffer: The depth texture to be used for z-buffering.
  //
  // Creates a standard 1-subpass Framebuffer suitable for use with DrawBatch().
  impl::FramebufferPtr CreateFramebuffer(ImageViewPtr output_image_view, TexturePtr depth_texture);

  // Creates a 2-subpass Framebuffer for color conversion. If |transient_image| is provided, it is
  // reused as the intermediate color attachment for the first subpass; if null, a new transient
  // image matching |output_image_view| is allocated.
  impl::FramebufferPtr CreateColorConversionFramebuffer(ImageViewPtr output_image_view,
                                                        TexturePtr depth_texture,
                                                        ImagePtr transient_image = nullptr);

  // Returns the transient image from a color-conversion framebuffer (attachment 0).
  // The framebuffer *MUST* have been created via `CreateColorConversionFramebuffer()`.
  static ImagePtr GetTransientImage(const impl::FramebufferPtr& color_conversion_framebuffer);

  // Returns true if |transient_image| has matching properties (dimensions, format, color space,
  // protected memory) to serve as the intermediate target for rendering into |target_image|.
  static bool CanShareTransientImage(const ImagePtr& transient_image, const ImagePtr& target_image);

  // Draws a batch of renderables into |framebuffer|.  Whether color conversion is applied is
  // determined by |framebuffer|, i.e. by whether CreateColorConversionFramebuffer() was used.
  void DrawBatch(CommandBuffer* cmd_buf, std::span<const Rectangle2D> rectangles,
                 std::span<const TexturePtr> textures, std::span<const ColorData> color_data,
                 const impl::FramebufferPtr& framebuffer);

  // Helper to support braced-init-lists with Framebuffer.
  void DrawBatch(CommandBuffer* cmd_buf, std::initializer_list<Rectangle2D> rectangles,
                 std::initializer_list<TexturePtr> textures,
                 std::initializer_list<ColorData> color_data,
                 const impl::FramebufferPtr& framebuffer) {
    DrawBatch(cmd_buf, std::span<const Rectangle2D>(rectangles.begin(), rectangles.size()),
              std::span<const TexturePtr>(textures.begin(), textures.size()),
              std::span<const ColorData>(color_data.begin(), color_data.size()), framebuffer);
  }

  // This data is used to apply a color-conversion post processing effect over the entire
  // rendered output, when making a call to |DrawBatch|. The color conversion formula
  // used is matrix * (color + preoffsets) + postoffsets.
  void SetColorConversionParams(const ColorConversionParams& color_conversion_params);

  // Minimal image constraints to be set on textures and render targets passed into
  // DrawBatch. These are meant to also be compatible with AFBC
  // (Arm Framebuffer Compression).
  static vk::ImageCreateInfo GetDefaultImageConstraints(const vk::Format& vk_format,
                                                        vk::ImageUsageFlags usage);

  // Pre-generate and cache Vulkan render passes and pipelines at startup to avoid subsequent
  // jank at runtime: when DrawBatch() looks for the renderpasses and pipelines in their respective
  // caches, they will already be there.
  //
  // WarmPipelineCache() may be called several times with different parameters to accommodate
  // e.g. different framebuffer formats and layouts.
  //
  // Args:
  // - |output_format|           Describes the target framebuffer.
  // - |output_swapchain_layout| Describes the target framebuffer.
  // - |depth_format|            Describes the target framebuffer.
  // - |depth_swapchain_layout|  Describes the target framebuffer.
  // - |immutable_samplers|      Support various types of YUV video.
  // - |use_protected_memory|    If true, allows rendering protected (and unprotected) content into
  //                             a protected framebuffer.  If false, only unprotected content can be
  //                             rendered by the generated pipelines.
  void WarmPipelineCache(vk::Format output_format, vk::ImageLayout output_swapchain_layout,
                         vk::Format depth_format, const std::vector<SamplerPtr>& immutable_samplers,
                         bool use_protected_memory);

 private:
  RectangleCompositor(const RectangleCompositor&) = delete;
  ImagePtr CreateTransientImage(const ImagePtr& image);

  // Hold onto escher pointer.
  EscherWeakPtr escher_ = nullptr;

  // Default shader program that all renderables use.
  ShaderProgramPtr standard_program_ = nullptr;

  // Color conversion shader program used for post processing.
  ShaderProgramPtr color_conversion_program_ = nullptr;

  // Color conversion values.
  ColorConversionParams color_conversion_params_;
};

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_FLATLAND_RECTANGLE_COMPOSITOR_H_
