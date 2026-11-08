// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/system_log_recorder/writer.h"

#include <fcntl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/vmo.h>
#include <unistd.h>

#include <algorithm>
#include <string>

#include "src/developer/forensics/feedback_data/system_log_recorder/reader.h"
#include "src/developer/forensics/utils/vmo.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/path.h"
#include "src/lib/fxl/strings/string_number_conversions.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {

SystemLogWriter::SystemLogWriter(const std::string& logs_dir, size_t max_num_files,
                                 StorageSize total_log_size, std::unique_ptr<Decoder> decoder)
    : logs_dir_(logs_dir),
      max_num_files_(max_num_files),
      total_log_size_(total_log_size),
      decoder_(std::move(decoder)),
      recovering_from_purge_(false) {
  FX_CHECK(max_num_files_ > 0);
  if (!files::CreateDirectory(logs_dir)) {
    FX_LOGS(WARNING) << "Failed to create logs directory, will re-try on the next block, no logs "
                        "persisted until then";
    return;
  }

  std::vector<std::string> current_log_files;
  files::ReadDirContents(logs_dir_, &current_log_files);

  // Get the numbers the previous writer assigned to the files – there should only be previous
  // files in case of a component restart.
  for (const std::string& fname : current_log_files) {
    size_t file_num = 0;
    if (fxl::StringToNumberWithError(fname, &file_num)) {
      file_queue_.push_back(file_num);
    }
  }

  // Sort the files such that the oldest files will be deleted first.
  std::sort(file_queue_.begin(), file_queue_.end());

  StartNewFile();
}

bool SystemLogWriter::RecreateDirectoryIfCachePurged() {
  if (!files::IsDirectory(logs_dir_)) {
    recovering_from_purge_ = true;
    file_queue_.clear();

    // Invalidate the current file descriptor. The current in-flight write will be discarded because
    // it was encoded against a pre-purge dictionary. A new file will be started at the end of
    // Write() and the recorder will reset the store.
    current_file_descriptor_.reset();

    if (files::CreateDirectory(logs_dir_)) {
      FX_LOGS(INFO)
          << "Re-created logs directory. Disk was most likely full at some earlier point in time";
    } else {
      FX_LOGS_FIRST_N(WARNING, 10)
          << "Still cannot re-create logs directory. Disk still most likely full";
    }

    return true;
  }

  return false;
}

bool SystemLogWriter::HasSufficientCoverage() const {
  // If the writer is not recovering from a purge, then there's no reason for the system log
  // recorder to fallback to a snapshot from Archivist.
  return !recovering_from_purge_;
}

void SystemLogWriter::StartNewFile() {
  if (!files::IsDirectory(logs_dir_)) {
    FX_LOGS_FIRST_N(WARNING, 10) << "Cannot start new file: logs directory does not exist";
    return;
  }

  const size_t next_file_num = (file_queue_.empty()) ? 0u : file_queue_.back() + 1;
  if (file_queue_.size() >= max_num_files_) {
    TRACE_DURATION("feedback:io", "SystemLogWriter::RemoveFile");
    remove(Path(file_queue_.front()).c_str());
    file_queue_.pop_front();
  }

  file_queue_.push_back(next_file_num);

  if (recovering_from_purge_ &&
      file_queue_.size() >= std::min(max_num_files_, kMinFilesForSufficientCoverage)) {
    recovering_from_purge_ = false;
  }

  TRACE_DURATION("feedback:io", "SystemLogWriter::OpenFile");
  current_file_descriptor_.reset(
      open(Path(next_file_num).c_str(), O_WRONLY | O_CREAT | O_TRUNC, S_IRUSR | S_IWUSR));
}

SystemLogWriter::WriteResult SystemLogWriter::Write(const LogMessageStore::ConsumeResult& result) {
  TRACE_DURATION("feedback:io", "SystemLogWriter::Write");

  const bool cache_purged = RecreateDirectoryIfCachePurged();

  // The file descriptor could be negative if the file failed to open.
  if (current_file_descriptor_.is_valid()) {
    // Overcommit, i.e. write everything we consumed before starting a new file for the next
    // block as we cannot have a block spanning multiple files.
    write(current_file_descriptor_.get(), result.log.c_str(), result.log.size());
  }

  if (result.end_of_block || cache_purged) {
    StartNewFile();
  }

  return cache_purged ? WriteResult::kCachePurge : WriteResult::kOk;
}

bool SystemLogWriter::Fsync() {
  if (!current_file_descriptor_.is_valid()) {
    return false;
  }

  return fsync(current_file_descriptor_.get()) == 0;
}

SystemLogWriter::FlushAndReadLogsResult SystemLogWriter::FlushAndReadLogs(
    const LogMessageStore::ConsumeResult& result) {
  const bool cache_purged = (Write(result) == WriteResult::kCachePurge);

  if (!HasSufficientCoverage()) {
    return {
        .logs = fit::error(WriterError::kInsufficientCoverage),
        .cache_purged = cache_purged,
    };
  }

  const fit::result<ReaderError, ConcatenateResult> uncompressed_log =
      Concatenate(logs_dir_, total_log_size_, decoder_.get());

  if (uncompressed_log.is_error()) {
    switch (uncompressed_log.error_value()) {
      case ReaderError::kIoError:
        return {
            .logs = fit::error(WriterError::kIoError),
            .cache_purged = cache_purged,
        };
      case ReaderError::kDecompressionError:
        return {
            .logs = fit::error(WriterError::kDecompressionError),
            .cache_purged = cache_purged,
        };
    }
  }

  zx::result<zx::vmo> vmo = VmoFromString(uncompressed_log->logs);
  if (vmo.is_error()) {
    return {
        .logs = fit::error(WriterError::kVmoError),
        .cache_purged = cache_purged,
    };
  }

  return {
      .logs = fit::ok(Logs{
          .vmo = std::move(*vmo),
          .first_timestamp = uncompressed_log->first_timestamp,
          .last_timestamp = uncompressed_log->last_timestamp,
      }),
      .cache_purged = cache_purged,
  };
}

std::string SystemLogWriter::Path(const size_t file_num) const {
  return files::JoinPath(logs_dir_, std::to_string(file_num));
}

}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics
