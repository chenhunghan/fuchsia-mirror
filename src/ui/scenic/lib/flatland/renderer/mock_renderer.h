// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_MOCK_RENDERER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_MOCK_RENDERER_H_

#include <lib/fpromise/promise.h>

#include <gmock/gmock.h>

#include "src/ui/scenic/lib/allocation/buffer_collection_importer.h"
#include "src/ui/scenic/lib/flatland/renderer/renderer.h"

namespace flatland {

// Mock class of the Flatland Renderer for API testing.
class MockRenderer : public Renderer {
 public:
  MOCK_METHOD((fit::result<ImportImageError, std::unique_ptr<GpuCollection>>), CreateGpuCollection,
              (allocation::GlobalBufferCollectionId,
               fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>),
              (override));

  MOCK_METHOD((fit::result<ImportImageError, std::unique_ptr<GpuImage>>), CreateGpuImage,
              (const allocation::ImageMetadata&, GpuCollection&, uint64_t), (override));

  MOCK_METHOD(fpromise::promise<>, ImportBufferCollection,
              (allocation::GlobalBufferCollectionId, fidl::WireClient<fuchsia_sysmem2::Allocator>&,
               fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>,
               allocation::BufferCollectionUsage, std::optional<fuchsia_math::SizeU> size),
              (override));

  MOCK_METHOD(void, ReleaseBufferCollection,
              (allocation::GlobalBufferCollectionId, allocation::BufferCollectionUsage),
              (override));

  MOCK_METHOD(fpromise::promise<>, ImportBufferImage,
              (const allocation::ImageMetadata&, allocation::BufferCollectionUsage), (override));

  MOCK_METHOD(void, ReleaseBufferImage, (allocation::GlobalImageId image_id), (override));

  MOCK_METHOD(void, Render,
              (const allocation::ImageMetadata&, std::span<const ResolvedLayer>,
               const RenderArgs& render_args),
              (override));

  MOCK_METHOD(void, SetColorConversionValues,
              ((const fidl::Array<float, 9>&), (const fidl::Array<float, 3>&),
               (const fidl::Array<float, 3>&)),
              (override));

  MOCK_METHOD(fuchsia_images2::PixelFormat, ChoosePreferredRenderTargetFormat,
              (const std::vector<fuchsia_images2::PixelFormat>&), (const, override));

  MOCK_METHOD(bool, SupportsRenderInProtected, (), (const, override));

  MOCK_METHOD(bool, RequiresRenderInProtected, (std::span<const ResolvedLayer>), (const, override));
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_RENDERER_MOCK_RENDERER_H_
