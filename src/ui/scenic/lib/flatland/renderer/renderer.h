// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_RENDERER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_RENDERER_H_

#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.math/cpp/fidl.h>
#include <lib/fit/result.h>
#include <lib/zx/event.h>

#include <memory>
#include <optional>

#include "src/ui/lib/escher/geometry/types.h"
#include "src/ui/scenic/lib/allocation/buffer_collection_importer.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/image_import/image_importer.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_collection.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_image.h"
#include "src/ui/scenic/lib/flatland/renderer/omitted_layer_tracker.h"

#include <glm/glm.hpp>
#include <glm/mat3x3.hpp>
#include <glm/vec2.hpp>
#include <glm/vec4.hpp>

namespace flatland {

// Typedef to a flatland type.
using Rectangle2D = escher::Rectangle2D;

// This is the main renderer interface used by the Flatland System. Since Flatland is
// agnostic to the implementation of the renderer, it is declared here as a virtual
// interface, whose concrete implementation is to be injected into Flatland.
class Renderer : public allocation::BufferCollectionImporter {
 public:
  using GpuImage = ImportedImage::GpuImage;
  using GpuCollection = ImportedCollection::GpuCollection;

  ~Renderer() override = default;

  // Main thread only. Binds `token` to a renderer buffer collection and sets
  // client-image constraints; destroying the returned slot on the main thread
  // destroys the underlying collection (image_import/DESIGN.md#renderer-contract).
  // Returns `ImportImageError::kCollectionFailed` when `collection_id` is
  // invalid, `token` is invalid, or binding or constraint setting fails.
  virtual fit::result<ImportImageError, std::unique_ptr<GpuCollection>> CreateGpuCollection(
      allocation::GlobalBufferCollectionId collection_id,
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) = 0;

  // Safe from any thread. Creates and memory-binds a client GPU image without
  // touching main-thread Escher state, rejecting images whose Vulkan memory
  // requirements exceed `per_buffer_bytes` before allocation. `per_buffer_bytes`
  // is the size of each buffer's VMO (`ImportedCollection::VmoBytesPerBuffer()`),
  // not sysmem's `size_bytes`.
  //
  // Preconditions: `collection` must come from this `Renderer`, sysmem
  // allocation for `collection` must be complete, `metadata.collection_id` must
  // equal `collection`'s ID when `metadata.collection_id != allocation::kInvalidId`,
  // and `collection` must outlive the `CreateGpuImage()` call (the returned
  // `GpuImage` does not reference `collection` after the call; the importer
  // keeps `collection` alive while any of its images is live
  // (image_import/DESIGN.md#ownership)). Destroying a returned slot with
  // `WasSampled() == true` runs on the main thread only, while a slot with
  // `WasSampled() == false` may be destroyed on any thread
  // (image_import/DESIGN.md#renderer-contract).
  //
  // Failure codes: `kInvalidVmoIndex` when `metadata.vmo_index` is out of
  // bounds, `kImageTooLarge` when `per_buffer_bytes == 0` or the image's memory
  // requirements exceed `per_buffer_bytes`, and `kGpuImportFailed` for invalid
  // metadata or any other Vulkan or driver import failure.
  virtual fit::result<ImportImageError, std::unique_ptr<GpuImage>> CreateGpuImage(
      const allocation::ImageMetadata& metadata, GpuCollection& collection,
      uint64_t per_buffer_bytes) = 0;

  // Optional arguments passed to Renderer::Render().
  struct RenderArgs {
    // Fences that will be signaled once rendering has completed. Clients can use these fences to
    // coordinate with other work that cannot be executed until rendering is completed.
    std::span<zx::event> release_fences;

    // If true, instructs the renderer to apply color conversion using the values specified by
    // |SetColorConversionValues|.
    bool apply_color_conversion = false;

    // If present, the renderer should render the frame number above the other content.
    std::optional<uint64_t> display_frame_number;

    // When non-null, `Render()` increments per-reason counts for scene-graph
    // layers skipped due to import failure (such as a GPU `kFailed` image);
    // layers skipped because `released` is set or `kReleased` are not counted.
    OmittedLayerTracker::ReasonCounts* omitted_layer_counts = nullptr;
  };

  // This function is responsible for rendering a single batch of Flatland rectangles into a
  // render target. This function is designed to be called on the render thread, not on any
  // Flatland instance thread. The specific behavior may differ depending on the specific subclass
  // implementation, which may also be a null renderer.
  //
  // This function assumes that clients have already validated their input data by comparing it
  // against the BufferCollectionMetadata received from calling |Validate| above. This function
  // will abort if invalid data has been passed in or if |Validate| has never been called.
  //
  // The size of the arrays |rectangles| and |images| must match or else this function will CHECK.
  // Entries in each array with the same index will be used together in rendering.
  //
  // The final argument |render_args| contains optional arguments that affect rendering.  See the
  // doc comments on each field of |struct RenderArgs|.
  //
  // If any of the |images| use protected memory but the |render_target| is not using protected
  // memory, the renderer replaces protected content with black solid color.
  //
  // Every non-null `ResolvedLayer::ImageContent::image` must already have
  // `released` set, or GPU state `kReady`, `kFailed`, or `kReleased`
  // (`EngineImageImporter::PrepareForGpuComposition()` must run first), and any
  // `kReady` `ImportedImage` in `layers` must hold a `GpuImage` created by this
  // `Renderer`.
  virtual void Render(const allocation::ImageMetadata& render_target,
                      std::span<const ResolvedLayer> layers, const RenderArgs& render_args) = 0;

  // Values needed to adjust the color of the framebuffer as a postprocessing effect.
  virtual void SetColorConversionValues(const fidl::Array<float, 9>& coefficients,
                                        const fidl::Array<float, 3>& preoffsets,
                                        const fidl::Array<float, 3>& postoffsets) = 0;

  // Returns the pixel format that the renderer prefers to use for render targets.
  // TODO(https://fxbug.dev/42065293): Rename this to reflect that it's for a render target.
  virtual fuchsia_images2::PixelFormat ChoosePreferredRenderTargetFormat(
      const std::vector<fuchsia_images2::PixelFormat>& available_formats) const = 0;

  // Returns true if the renderer is capable of switching to protected mode.
  virtual bool SupportsRenderInProtected() const = 0;

  // Returns true if the renderer has to switch to protected mode to render the given |images|. If
  // true, the caller is responsible for providing a render target using the protected memory when
  // calling Render(). Every non-null `ResolvedLayer::ImageContent::image` must
  // already have `released` set, or GPU state `kReady`, `kFailed`, or
  // `kReleased` (`EngineImageImporter::PrepareForGpuComposition()` must run
  // first), and any `kReady` `ImportedImage` in `layers` must hold a `GpuImage`
  // created by this `Renderer`.
  // TODO(https://fxbug.dev/42062402): The caller should be able to figure out if the images are
  // protected. Remove this after moving to prunable tokens in the callers.
  virtual bool RequiresRenderInProtected(std::span<const ResolvedLayer> layers) const = 0;

  // Implementations which use Vulkan or similar should implement this by generating render passes
  // and pipelines, so that there is no jank due to lazily generating them later.
  virtual void WarmPipelineCache() {}
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_RENDERER_H_
