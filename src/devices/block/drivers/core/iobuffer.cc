// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "iobuffer.h"

IoBuffer::IoBuffer(zx::vmo vmo, vmoid_t id, uint64_t vmo_size)
    : io_vmo_(std::move(vmo)), vmoid_(id), vmo_size_(vmo_size) {}

IoBuffer::~IoBuffer() = default;

zx_status_t IoBuffer::ValidateVmo(uint64_t length, uint64_t vmo_offset) const {
  if ((vmo_offset > vmo_size_) || (vmo_size_ - vmo_offset < length)) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  return ZX_OK;
}
