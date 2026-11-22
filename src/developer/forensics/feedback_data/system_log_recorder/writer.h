// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_WRITER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_WRITER_H_

#include <lib/fit/result.h>
#include <lib/zx/time.h>
#include <lib/zx/vmo.h>

#include <cstdint>
#include <deque>
#include <memory>
#include <optional>
#include <string>

#include <fbl/unique_fd.h>

#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/decoder.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/log_message_store.h"
#include "src/developer/forensics/utils/storage_size.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {

// Consumes the full content of a store on request, writing it to a rotating set of files.
//
// This class is thread-unsafe. All functions must be called on the same dispatcher.
class SystemLogWriter {
 public:
  enum class WriteResult : std::uint8_t {
    kOk,
    kCachePurge,
  };

  enum class WriterError : std::uint8_t {
    kIoError,
    kDecompressionError,
    kVmoError,
    kInsufficientCoverage,
  };

  struct Logs {
    zx::vmo vmo;
    std::optional<zx::time_boot> first_timestamp;
    std::optional<zx::time_boot> last_timestamp;
  };

  struct FlushAndReadLogsResult {
    ::fit::result<WriterError, Logs> logs;
    bool cache_purged;
  };

  static constexpr size_t kMinFilesForSufficientCoverage = 5u;

  // If |previous_logs_dir| is std::nullopt, only the current boot's files count against
  // |max_num_files|.
  //
  // Otherwise, |previous_logs_dir| is where Feedback holds logs from previous boots. The current
  // boot and the previous boots all share the |max_num_files| budget: if starting a new file would
  // exceed the limit, the oldest file of the oldest boot will be deleted. Files from the current
  // boot won't be pruned until no previous boot files remain.
  SystemLogWriter(const std::string& logs_dir, size_t max_num_files, StorageSize total_log_size,
                  std::unique_ptr<Decoder> decoder,
                  const std::optional<std::string>& previous_logs_dir);

  // Writes the consumed result to disk and returns whether a cache purge was detected.
  WriteResult Write(const LogMessageStore::ConsumeResult& result);

  // Instructs the class to call `fsync` on the currently open file to ensure data makes it to
  // disk. Returns true if successful.
  bool Fsync();

  // Flushes the given consume result to disk, reads all persisted logs and metadata, and returns
  // the logs (or error) along with whether a cache purge was detected.
  FlushAndReadLogsResult FlushAndReadLogs(const LogMessageStore::ConsumeResult& result);

 private:
  // Truncates the first file to start anew, evicting the oldest files across (potentially) previous
  // boots and the current boot until within |max_num_files_|.
  void StartNewFile();

  // Returns the path the |file_num|'th file created.
  std::string Path(size_t file_num) const;

  // Returns true if a cache purge was detected.
  bool RecreateDirectoryIfCachePurged();

  bool HasSufficientCoverage() const;

  const std::string logs_dir_;
  const std::optional<std::string> previous_logs_dir_;
  const size_t max_num_files_;
  const StorageSize total_log_size_;
  std::unique_ptr<Decoder> decoder_;

  std::deque<size_t> file_queue_;
  std::deque<std::string> previous_boot_file_queue_;

  bool recovering_from_purge_;
  fbl::unique_fd current_file_descriptor_;
};

}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_WRITER_H_
