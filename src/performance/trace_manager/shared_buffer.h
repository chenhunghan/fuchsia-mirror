// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_TRACE_MANAGER_SHARED_BUFFER_H_
#define SRC_PERFORMANCE_TRACE_MANAGER_SHARED_BUFFER_H_

#include <fidl/fuchsia.tracing/cpp/common_types.h>
#include <lib/trace-engine/types.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

#include <array>
#include <optional>

#include <trace-reader/reader_internal.h>

#include "src/performance/trace_manager/buffer_forwarder.h"
#include "src/performance/trace_manager/util.h"

const char* ModeName(fuchsia_tracing::BufferingMode mode);

struct TransferStats {
  uint8_t durable_used_percent;
  uint32_t wrapped_count;
  uint64_t records_dropped;
  uint64_t non_durable_bytes_written;
};

class SharedBuffer {
 public:
  static zx::result<SharedBuffer> Create(size_t buffer_size,
                                         fuchsia_tracing::BufferingMode buffering_mode,
                                         std::string provider_name, uint32_t provider_id);
  zx::unowned_vmo Vmo() const;
  fuchsia_tracing::BufferingMode BufferingMode() const;

  // Transfer records according to the streaming mode algorithm
  //
  // Transfers not yet transferred durable records up to the new durable_data_end.
  // Transfers not yet transferred records up the buffer given by wrapped count.
  bool StreamingTransfer(const std::shared_ptr<const tracing::BufferForwarder>& output,
                         uint32_t wrapped_count, uint64_t durable_data_end);

  // Transfer all records in the buffer.
  std::pair<tracing::TransferStatus, TransferStats> TransferAll(
      const std::shared_ptr<const tracing::BufferForwarder>& output) const;

  // Reset streaming state when tracing starts according to buffer disposition.
  void OnStart(fuchsia_tracing::BufferDisposition disposition);

  SharedBuffer(SharedBuffer&&) = default;
  SharedBuffer& operator=(SharedBuffer&&) = default;

 private:
  // The number of bytes at the start of a rolling buffer that we record after forwarding it in
  // streaming mode. TransferAll() compares them to tell whether the provider has written to the
  // buffer since.
  //
  // Timestamped rolling-buffer records (Event, Scheduling, Log, etc.) put an 8-byte header at
  // bytes 0..7 and a monotonic 64-bit zx_ticks_t timestamp at bytes 8..15, so 16 bytes already
  // changes whenever one of those is written at offset 0. 64 bytes (8 FXT words) is a best-effort
  // prefix that also spans past a small non-timestamped record (such as an 8-byte Metadata,
  // 16-byte Initialization, or 24-byte Thread record) when followed by a timestamped record, while
  // fitting in a small stack read well below trace-engine's minimum rolling buffer size (1856
  // bytes; note >=64-byte non-timestamped records such as Perfetto blobs with an identical prefix
  // can still collide on an unfinalized exit).
  static constexpr size_t kRollingBufferFingerprintSize = 64;
  using RollingBufferFingerprint = std::array<uint8_t, kRollingBufferFingerprintSize>;

  SharedBuffer(zx::vmo vmo, std::string provider_name,
               fuchsia_tracing::BufferingMode buffering_mode, size_t buffer_size,
               uint32_t provider_id);

  bool VerifyBufferHeader(const trace::internal::BufferHeaderReader* header) const;
  tracing::TransferStatus WriteProviderIdRecord(
      const std::shared_ptr<const tracing::BufferForwarder>& output) const;
  tracing::TransferStatus WriteChunk(const std::shared_ptr<const tracing::BufferForwarder>& output,
                                     uint64_t offset, uint64_t last, uint64_t end,
                                     uint64_t buffer_size) const;
  // Reads the first bytes of the rolling buffer at |buffer_offset|. Returns std::nullopt if the
  // buffer is too small or the read fails.
  std::optional<RollingBufferFingerprint> ReadRollingBufferFingerprint(uint64_t buffer_offset,
                                                                       uint64_t buffer_size) const;
  // Returns true if, in streaming mode, rolling buffer |buffer_number| was previously forwarded
  // and its first bytes haven't changed since, i.e. the provider hasn't written to it again.
  bool IsRollingBufferUnchangedSinceSaved(int32_t buffer_number, uint64_t buffer_offset,
                                          uint64_t buffer_size) const;

  zx::vmo vmo_;
  // Used only for providing debugging information.
  std::string provider_name_;
  fuchsia_tracing::BufferingMode buffering_mode_;
  size_t buffer_size_;

  mutable bool provider_info_record_written_ = false;

  uint32_t last_wrapped_count_ = 0u;
  uint64_t last_durable_data_end_ = 0;
  // Streaming mode only: the first bytes of each rolling buffer as of the last time it was
  // forwarded. They describe the physical contents of the VMO, so they stay valid across OnStart().
  mutable std::array<std::optional<RollingBufferFingerprint>, 2> saved_rolling_buffer_fingerprints_;
  uint32_t provider_id_;
};
#endif  // SRC_PERFORMANCE_TRACE_MANAGER_SHARED_BUFFER_H_
