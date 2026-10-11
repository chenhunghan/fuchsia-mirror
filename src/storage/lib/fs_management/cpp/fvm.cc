// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/fs_management/cpp/fvm.h"

#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <lib/fdio/cpp/caller.h>
#include <string.h>
#include <zircon/compiler.h>

#include <memory>

#include "src/storage/fvm/fvm.h"
#include "src/storage/lib/block_client/cpp/remote_block_device.h"

namespace fs_management {

__EXPORT
zx_status_t FvmInitPreallocated(fidl::UnownedClientEnd<fuchsia_storage_block::Block> device,
                                uint64_t initial_volume_size, uint64_t max_volume_size,
                                size_t slice_size) {
  if (slice_size % fvm::kBlockSize != 0) {
    // Alignment
    return ZX_ERR_INVALID_ARGS;
  }
  if ((slice_size * fvm::kMaxVSlices) / fvm::kMaxVSlices != slice_size) {
    // Overflow
    return ZX_ERR_INVALID_ARGS;
  }
  if (initial_volume_size > max_volume_size || initial_volume_size == 0 || max_volume_size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  fvm::Header header = fvm::Header::FromGrowableDiskSize(
      fvm::kMaxUsablePartitions, initial_volume_size, max_volume_size, slice_size);
  if (header.pslice_count == 0) {
    return ZX_ERR_NO_SPACE;
  }

  // This buffer needs to hold both copies of the metadata.
  // TODO(https://fxbug.dev/42138919): Eliminate layout assumptions.
  size_t metadata_allocated_bytes = header.GetMetadataAllocatedBytes();
  size_t dual_metadata_bytes = metadata_allocated_bytes * 2;
  std::unique_ptr<uint8_t[]> mvmo(new uint8_t[dual_metadata_bytes]);
  // Clear entire primary copy of metadata
  memset(mvmo.get(), 0, metadata_allocated_bytes);

  // Save the header to our primary metadata.
  memcpy(mvmo.get(), &header, sizeof(fvm::Header));
  size_t metadata_used_bytes = header.GetMetadataUsedBytes();
  fvm::UpdateHash(mvmo.get(), metadata_used_bytes);

  // Copy the new primary metadata to the backup copy.
  void* backup = mvmo.get() + header.GetSuperblockOffset(fvm::SuperblockType::kSecondary);
  memcpy(backup, mvmo.get(), metadata_allocated_bytes);

  // Validate our new state.
  if (!fvm::PickValidHeader(mvmo.get(), backup, metadata_used_bytes)) {
    return ZX_ERR_BAD_STATE;
  }

  // Write to primary copy.
  auto status = block_client::SingleWriteBytes(device, mvmo.get(), metadata_allocated_bytes, 0);
  if (status != ZX_OK) {
    return status;
  }
  // Write to secondary copy, to overwrite any previous FVM metadata copy that
  // could be here.
  return block_client::SingleWriteBytes(device, mvmo.get(), metadata_allocated_bytes,
                                        metadata_allocated_bytes);
}

__EXPORT
zx_status_t FvmInitWithSize(fidl::UnownedClientEnd<fuchsia_storage_block::Block> device,
                            uint64_t volume_size, size_t slice_size) {
  return FvmInitPreallocated(device, volume_size, volume_size, slice_size);
}

__EXPORT
zx_status_t FvmInit(fidl::UnownedClientEnd<fuchsia_storage_block::Block> device,
                    size_t slice_size) {
  // The metadata layout of the FVM is dependent on the
  // size of the FVM's underlying partition.
  const fidl::WireResult result = fidl::WireCall(device)->GetInfo();
  if (!result.ok()) {
    return result.status();
  }
  const fit::result response = result.value();
  if (response.is_error()) {
    return response.error_value();
  }
  const fuchsia_storage_block::wire::BlockInfo& block_info = response.value()->info;
  if (slice_size == 0 || slice_size % block_info.block_size) {
    return ZX_ERR_BAD_STATE;
  }

  return FvmInitWithSize(device, block_info.block_count * block_info.block_size, slice_size);
}

__EXPORT
zx::result<fuchsia_storage_block::wire::VolumeManagerInfo> FvmQuery(
    fidl::UnownedClientEnd<fuchsia_storage_block::VolumeManager> fvm) {
  const fidl::WireResult result = fidl::WireCall(fvm)->GetInfo();
  if (!result.ok()) {
    return zx::error(result.status());
  }
  const fidl::WireResponse response = result.value();
  if (zx_status_t status = response.status; status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(*response.info);
}

__EXPORT
zx_status_t FvmActivate(int fvm_fd, fuchsia_storage_block::wire::Guid deactivate,
                        fuchsia_storage_block::wire::Guid activate) {
  fdio_cpp::UnownedFdioCaller caller(fvm_fd);
  fidl::UnownedClientEnd<fuchsia_storage_block::VolumeManager> client(caller.borrow_channel());
  auto response = fidl::WireCall(client)->Activate(deactivate, activate);
  if (response.status() != ZX_OK) {
    return response.status();
  }
  if (response.value().status != ZX_OK) {
    return response.value().status;
  }
  return ZX_OK;
}

}  // namespace fs_management
