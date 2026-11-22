// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/amlogic-display/capture-image-info.h"

#include <fidl/fuchsia.hardware.amlogiccanvas/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/result.h>

namespace amlogic_display {

CaptureImageInfo::~CaptureImageInfo() {
  fdf::info("Destroying capture image on canvas {}", canvas_idx);
  if (canvas.has_value()) {
    fidl::WireResult result = fidl::WireCall(canvas.value())->Free(canvas_idx);
    if (!result.ok()) {
      fdf::warn("Failed to call Canvas Free: {}", result.error().FormatDescription());
    } else if (result->is_error()) {
      fdf::warn("Canvas Free failed: {}", zx::make_result(result->error_value()));
    }
  }
}

}  // namespace amlogic_display
