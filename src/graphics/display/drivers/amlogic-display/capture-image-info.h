// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_CAPTURE_IMAGE_INFO_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_CAPTURE_IMAGE_INFO_H_

#include <fidl/fuchsia.hardware.amlogiccanvas/cpp/wire.h>

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>

#include <fbl/intrusive_hash_table.h>
#include <fbl/intrusive_single_list.h>

#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"

namespace amlogic_display {

// Driver-side state for an image imported for display capture.
struct CaptureImageInfo : public fbl::SinglyLinkedListable<std::unique_ptr<CaptureImageInfo>> {
  using IdType = display::DriverCaptureImageId;
  using HashTable = fbl::HashTable<IdType, std::unique_ptr<CaptureImageInfo>>;

  // Trait implementation for `fbl::HashTable`.
  HashTable::KeyType GetKey() const { return id; }
  static size_t GetHash(HashTable::KeyType key) { return std::hash<HashTable::KeyType>()(key); }

  display::DriverCaptureImageId id = display::kInvalidDriverCaptureImageId;
  std::optional<fidl::UnownedClientEnd<fuchsia_hardware_amlogiccanvas::Device>> canvas;
  uint8_t canvas_idx = 0;
  uint32_t image_height = 0;
  uint32_t image_width = 0;

  ~CaptureImageInfo();
};

}  // namespace amlogic_display

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_AMLOGIC_DISPLAY_CAPTURE_IMAGE_INFO_H_
