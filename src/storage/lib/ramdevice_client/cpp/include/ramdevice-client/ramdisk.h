// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_RAMDEVICE_CLIENT_CPP_INCLUDE_RAMDEVICE_CLIENT_RAMDISK_H_
#define SRC_STORAGE_LIB_RAMDEVICE_CLIENT_CPP_INCLUDE_RAMDEVICE_CLIENT_RAMDISK_H_

#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <stdlib.h>
#include <zircon/compiler.h>
#include <zircon/hw/gpt.h>
#include <zircon/types.h>

#include <array>
#include <memory>
#include <optional>
#include <string>

// A struct containing the number of write operations transmitted to the ramdisk
// since the last invocation of "wake" or "sleep_after".
typedef struct ramdisk_block_write_counts {
  uint64_t received;
  uint64_t successful;
  uint64_t failed;
} ramdisk_block_write_counts_t;

namespace ramdevice_client {

// Manages a ramdisk instance.
class Ramdisk {
 public:
  struct Options {
    // If set, the ram-disk will report this type guid using the partition protocol.
    std::optional<std::array<uint8_t, GPT_GUID_LEN>> type_guid;
  };
  static constexpr Options kDefaultOptions;

  // Creates a ram-disk with |block_count| blocks of |block_size| bytes.
  // `svc_root_fd` can be overridden if desired; otherwise, "/svc" is opened to find
  // fuchsia.hardware.ramdisk.Service.
  static zx::result<Ramdisk> Create(int block_size, uint64_t block_count,
                                    std::optional<int> svc_root_fd = std::nullopt,
                                    const Options& options = kDefaultOptions);

  // Creates a ram-disk with the given VMO.  If block_size is zero, a default block size is used.
  // `svc_root_fd` can be overridden if desired; otherwise, "/svc" is opened to find
  // fuchsia.hardware.ramdisk.Service.
  static zx::result<Ramdisk> CreateWithVmo(zx::vmo vmo, uint64_t block_size = 0,
                                           std::optional<int> svc_root_fd = std::nullopt,
                                           const Options& options = kDefaultOptions);

  Ramdisk();
  Ramdisk(Ramdisk&& other) noexcept;
  Ramdisk& operator=(Ramdisk&& other) noexcept;
  ~Ramdisk();

  // Frees the resources associated with the Ramdisk.  It is an error to call any methods after this
  // has been called.
  void Reset();

  bool is_valid() const { return client_ != nullptr; }

  // Returns the path to the device.
  std::string path() const;

  // Creates a new connection to the Block protocol served by the ramdisk.
  zx::result<fidl::ClientEnd<fuchsia_storage_block::Block>> ConnectBlock() const;

  // Puts the ramdisk to sleep after |blk_count| blocks written.  After this, transactions will no
  // longer be immediately persisted to disk.
  // If the |RAMDISK_FLAG_RESUME_ON_WAKE| flag has been set, transactions will be processed when
  // |Wake| is called, otherwise they will fail immediately.
  zx::result<> SleepAfter(uint64_t block_count) const;

  // Wakes up the ramdisk.
  zx::result<> Wake() const;

  // Sets flags on the ramdisk (see fuchsia.hardware.ramdisk.RamdiskFlag).
  zx::result<> SetFlags(uint32_t flags) const;

  // Returns the ramdisk's current failed, successful, and total block counts.
  zx::result<ramdisk_block_write_counts_t> GetBlockCounts() const;

 private:
  struct Client;
  explicit Ramdisk(std::unique_ptr<Client> client);

  std::unique_ptr<Client> client_;
};

}  // namespace ramdevice_client

#endif  // SRC_STORAGE_LIB_RAMDEVICE_CLIENT_CPP_INCLUDE_RAMDEVICE_CLIENT_RAMDISK_H_
