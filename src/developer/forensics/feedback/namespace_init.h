// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_NAMESPACE_INIT_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_NAMESPACE_INIT_H_

#include <lib/zx/time.h>

#include <optional>
#include <string>

#include "src/developer/forensics/feedback/constants.h"
#include "src/developer/forensics/utils/cobalt/logger.h"
#include "src/developer/forensics/utils/storage_size.h"

namespace forensics::feedback {

// Reboot reporting

// Return whether |not_a_fdr_path| existed in the file system and create it otherwise.
bool TestAndSetNotAFdr(const std::string& not_a_fdr_file = kNotAFdrFile);

// Moves the file to |to| from |from|.
void MoveFile(const std::string& from, const std::string& to);

// Moves the file at |from| to |to|, falling back to moving |legacy_from| if |from| does not exist.
// Deletes |legacy_from| if |from| exists.
void MoveFileWithFallback(const std::string& from, const std::string& legacy_from,
                          const std::string& to);

// Feedback data
//
// Decompress and concatenate the logs from the previous boot in |dir| and store them at
// |write_path|. Also logs metrics about the previous boot logs to |cobalt|.
//
// Returns the timestamp of the last message in the previous boot logs, if any, even if writing
// |write_path| fails.
std::optional<zx::time_boot> CreatePreviousLogsFile(
    cobalt::Logger* cobalt, StorageSize max_decompressed_size,
    const std::string& dir = kCurrentLogsDir,
    const std::string& write_path = kPreviousLogsFilePath);

// Logs to |cobalt| the difference between |last_boot_uptime| and |last_log_timestamp|, the
// timestamp of the last message in the previous boot logs.
void LogPreviousBootLogLastTimestampDelta(cobalt::Logger* cobalt,
                                          std::optional<zx::time_boot> last_log_timestamp,
                                          std::optional<zx::duration> last_boot_uptime);

// Move the boot id stored at |current_boot_id_path| to |previous_boot_id_path| and write a new
// boot id to |current_boot_id_path|.
//
// Appends the new boot id to the timeline stored at |timeline_path| and truncates the timeline
// to the last 10 boot ids.
void MoveAndRecordBootId(const std::string& new_boot_id,
                         const std::string& previous_boot_id_path = kPreviousBootIdPath,
                         const std::string& current_boot_id_path = kCurrentBootIdPath,
                         const std::string& timeline_path = kBootIdTimelinePath);

// Move the build version stored at |current_build_version_path| to |previous_build_version_path|
// and write the current build version to |current_build_version_path|.
void MoveAndRecordBuildVersion(const std::string& current_build_version,
                               const std::string& previous_build_version_path,
                               const std::string& current_build_version_path);

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_NAMESPACE_INIT_H_
