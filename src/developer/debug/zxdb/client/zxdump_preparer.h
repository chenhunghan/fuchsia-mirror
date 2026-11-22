// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_PREPARER_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_PREPARER_H_

#include <cstdint>
#include <span>
#include <string>

#include <fbl/unique_fd.h>

#include "src/developer/debug/zxdb/common/err_or.h"

namespace zxdb {

// The container formats that the zxdump reader accepts, by leading bytes.
enum class DumpFormat : uint8_t {
  kUnknown,
  kElfCore,  // ELF file with e_type == ET_CORE.
  kArchive,  // "!<arch>\n" job archive or plain archive of dumps.
  kZstd,     // zstd frame (magic 28 B5 2F FD) that wraps one of the formats above.
};

// Classifies a dump from its leading bytes (at least 18 bytes are needed to detect an ELF core
// file). Exposed for unit tests so header classification can be tested in memory without creating
// temporary files.
DumpFormat DetectDumpFormat(std::span<const uint8_t> header);

// Returns true when the leading magic bytes of |path| match a format that the zxdump reader
// accepts (ET_CORE, "!<arch>\n", or a zstd frame).
//
// Note: This checks only the leading 18-byte magic header so callers (such as Session::OpenDump)
// can route the file to ZxdumpRemoteAPI instead of MinidumpRemoteAPI. It does not validate the
// rest of the file.
bool IsZxdumpFile(const std::string& path);

// Returns a file descriptor positioned at offset 0 and ready for random-access reading (mmap) by
// zxdump::TaskHolder.
//
// Although zxdump::TaskHolder::Insert accepts .zst streams directly, zxdump's built-in zstd reader
// is forward-only and rejects random-access process memory reads. PrepareDump therefore opens
// uncompressed inputs in place (O_RDONLY | O_CLOEXEC) and decompresses zstd-compressed inputs into
// an immediately unlinked temporary file (O_RDWR | O_CLOEXEC) so the OS kernel reclaims the disk
// space automatically as soon as the returned file descriptor and any mappings of it are closed.
ErrOr<fbl::unique_fd> PrepareDump(const std::string& path);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_PREPARER_H_
