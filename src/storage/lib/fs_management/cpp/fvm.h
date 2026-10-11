// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_FS_MANAGEMENT_CPP_FVM_H_
#define SRC_STORAGE_LIB_FS_MANAGEMENT_CPP_FVM_H_

#include <fidl/fuchsia.device/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <lib/zx/result.h>
#include <stdint.h>
#include <stdlib.h>
#include <zircon/types.h>

#include <cstdint>
#include <string_view>
#include <vector>

#include <fbl/unique_fd.h>
#include <src/lib/uuid/uuid.h>

#include "src/storage/lib/fs_management/cpp/format.h"

namespace fs_management {

// Format a block device to be an empty FVM.
zx_status_t FvmInit(fidl::UnownedClientEnd<fuchsia_storage_block::Block> device, size_t slice_size);

// Format a block device to be an empty FVM of |disk_size| size.
zx_status_t FvmInitWithSize(fidl::UnownedClientEnd<fuchsia_storage_block::Block> device,
                            uint64_t disk_size, size_t slice_size);

// Format a block device to be an empty FVM. The FVM will initially be formatted as if the block
// device had |initial_volume_size| and leave gap for metadata extension up to |max_volume_size|.
// Note: volume sizes are assumed to be multiples of the underlying block device block size.
zx_status_t FvmInitPreallocated(fidl::UnownedClientEnd<fuchsia_storage_block::Block> device,
                                uint64_t initial_volume_size, uint64_t max_volume_size,
                                size_t slice_size);

// Query the volume manager for info.
zx::result<fuchsia_storage_block::wire::VolumeManagerInfo> FvmQuery(
    fidl::UnownedClientEnd<fuchsia_storage_block::VolumeManager> fvm);

// Marks one partition as active and optionally another as inactive in one atomic operation.
// If both partition GUID are the same, the partition will be activated and
// no partition will be marked inactive.
zx_status_t FvmActivate(int fvm_fd, fuchsia_storage_block::wire::Guid deactivate,
                        fuchsia_storage_block::wire::Guid activate);

}  // namespace fs_management

#endif  // SRC_STORAGE_LIB_FS_MANAGEMENT_CPP_FVM_H_
