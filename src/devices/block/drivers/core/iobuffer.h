// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_CORE_IOBUFFER_H_
#define SRC_DEVICES_BLOCK_DRIVERS_CORE_IOBUFFER_H_

#include <fuchsia/hardware/block/driver/c/banjo.h>
#include <lib/zx/vmo.h>

#include <fbl/intrusive_wavl_tree.h>
#include <fbl/ref_counted.h>

// Represents the mapping of "vmoid --> VMO"
class IoBuffer : public fbl::WAVLTreeContainable<fbl::RefPtr<IoBuffer>>,
                 public fbl::RefCounted<IoBuffer> {
 public:
  vmoid_t GetKey() const { return vmoid_; }

  // Validates that the requested range is within the bounds of the VMO.
  // The units of length and vmo_offset are bytes.
  zx_status_t ValidateVmo(uint64_t length, uint64_t vmo_offset) const;

  zx_handle_t vmo() const { return io_vmo_.get(); }

  IoBuffer(zx::vmo vmo, vmoid_t vmoid, uint64_t vmo_size);
  ~IoBuffer();

 private:
  friend struct TypeWAVLTraits;
  DISALLOW_COPY_ASSIGN_AND_MOVE(IoBuffer);

  const zx::vmo io_vmo_;
  const vmoid_t vmoid_;
  const uint64_t vmo_size_;
};

#endif  // SRC_DEVICES_BLOCK_DRIVERS_CORE_IOBUFFER_H_
