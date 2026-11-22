// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMPORTED_COLLECTION_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMPORTED_COLLECTION_H_

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>
#include <fidl/fuchsia.math/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>

#include <atomic>
#include <cstdint>
#include <memory>
#include <vector>

#include "src/ui/scenic/lib/allocation/id.h"

namespace flatland {

class ImportedImage;
class ImageImporterImpl;
class ImageImportTestPeer;

// Per-collection sysmem and renderer state shared by an importer's collection
// map and each live `ImportedImage` of that collection (DESIGN.md#ownership).
class ImportedCollection final {
 public:
  // Opaque renderer-owned per-collection state (DESIGN.md#renderer-contract).
  class GpuCollection {
   public:
    virtual ~GpuCollection() = default;
  };

  enum class State : uint8_t {
    // Initial state after registration, awaiting sysmem allocation response.
    kRegistered,
    // Sysmem allocated with display constraints; images can be imported to display or GPU.
    kDisplayCapable,
    // Sysmem allocated without display constraints; images can only be imported to GPU.
    kGpuOnly,
    // Sysmem allocation or collection registration failed.
    kFailed,
    // Collection has been torn down and its sysmem/Vulkan handles released.
    kReleased,
  };

  explicit ImportedCollection(allocation::GlobalBufferCollectionId id);

  // `ImageImporterImpl` tears down sysmem and Vulkan state explicitly on the
  // main thread before the last `shared_ptr` can drop on any thread
  // (DESIGN.md#ownership).
  ~ImportedCollection();

  ImportedCollection(const ImportedCollection&) = delete;
  ImportedCollection& operator=(const ImportedCollection&) = delete;

  allocation::GlobalBufferCollectionId id() const { return id_; }
  State state() const { return state_.load(std::memory_order_acquire); }

  // Valid once `state()` is `kDisplayCapable`.
  uint32_t tiling_type() const;

  // Valid once `state()` leaves `kRegistered` for `kDisplayCapable` or `kGpuOnly`. The size of each
  // buffer's VMO (see `VmoBytesPerBuffer()`), which bounds the memory a GPU image may bind.
  uint64_t per_buffer_bytes() const;

  // Returns the size of each buffer VMO in a sysmem collection whose `buffer_settings.size_bytes`
  // is `sysmem_size_bytes`: the most memory an image in that buffer may bind. Sysmem VMOs are whole
  // pages and `vmo_usable_start` is always 0, so this is `sysmem_size_bytes` rounded up to the page
  // size. Vulkan's memory requirements for a small image can exceed `size_bytes` (e.g. 256 bytes
  // for a 1x1 RGBA image whose `size_bytes` is 64) while still fitting in the VMO.
  static uint64_t VmoBytesPerBuffer(uint64_t sysmem_size_bytes);

  // Returns the renderer's per-collection slot. Valid from main-thread
  // registration (`kRegistered`, `kDisplayCapable`, `kGpuOnly`, or `kFailed`
  // when `CreateGpuCollection()` succeeded) until main-thread teardown resets it
  // on entering `kReleased` (once `live_images_` reaches zero and the
  // collection is unregistered, or during `StopOnMain()`, which joins all GPU
  // workers before resetting collection slots). Safe to call on main
  // or from a GPU import job while its `ImportedImage` keeps `live_images_ > 0`
  // (DESIGN.md#ownership).
  GpuCollection* gpu_collection() const { return gpu_collection_.get(); }

 private:
  friend class ImageImporterImpl;
  friend class ImageImportTestPeer;

  const allocation::GlobalBufferCollectionId id_;
  std::atomic<State> state_{State::kRegistered};

  // Main-thread-only sysmem participants and two-reply join flags. Each client
  // sends `Release()` and is reset on main as soon as its allocation reply
  // arrives, or during teardown if torn down while still `kRegistered`.
  fidl::Client<fuchsia_sysmem2::BufferCollection> allocation_participant_;
  fidl::Client<fuchsia_sysmem2::BufferCollection> display_participant_;
  bool allocation_reply_received_ = false;
  bool display_reply_received_ = false;
  bool allocation_succeeded_ = false;
  bool display_won_ = false;

  // Written on main before `state_` leaves `kRegistered` with release ordering.
  uint32_t tiling_type_ = fuchsia_hardware_display_types::wire::kImageTilingTypeLinear;
  uint32_t buffer_count_ = 0;
  uint64_t per_buffer_bytes_ = 0;
  fuchsia_math::SizeU min_size_{0, 0};
  fuchsia_math::SizeU max_size_{UINT32_MAX, UINT32_MAX};
  std::unique_ptr<GpuCollection> gpu_collection_;

  // Claimed once by whichever thread runs the collection's first GPU import so
  // duration estimates separate cold first imports from cached later imports.
  std::atomic<bool> first_gpu_import_claimed_{false};

  // Guarded by `ImageImporterImpl::collections_mutex_`.
  uint32_t live_images_ = 0;
  bool released_by_allocator_ = false;
  std::vector<std::shared_ptr<ImportedImage>> waiting_for_allocation_;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMPORTED_COLLECTION_H_
