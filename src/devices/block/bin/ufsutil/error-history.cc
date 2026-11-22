// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/bin/ufsutil/error-history.h"

#include <endian.h>
#include <lib/fzl/owned-vmo-mapper.h>
#include <zircon/status.h>

#include <algorithm>
#include <cstring>
#include <format>
#include <string>
#include <vector>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"

namespace ufsutil {
namespace {
// As specified in UFS 3.1, Section 11.3.27.3 (Error History Mode), the host must allocate at
// least 2088 bytes to retrieve the full error history directory without truncation.
constexpr uint32_t kErrorHistoryModeDataLength = 2088;
// Buffer ID range as defined in the UFS spec (10h - EFh)
constexpr uint8_t kMinBufferID = 0x10;
constexpr uint8_t kMaxBufferID = 0xEF;
constexpr size_t kMaxErrorHistoryEntries = kMaxBufferID - kMinBufferID + 1;
// READ BUFFER carries the buffer offset in 24 bits, so larger entries cannot be read.
constexpr uint32_t kMaxEntryLength = 1u << 24u;

// Returns the FIDL transport error or the driver error of a ReadBuffer call, or ZX_OK.
zx_status_t ReadBufferStatus(
    const fidl::WireResult<fuchsia_hardware_ufs::Ufs::ReadBuffer>& result) {
  if (!result.ok()) {
    return result.status();
  }
  return result->is_error() ? result->error_value() : ZX_OK;
}

struct ErrorHistoryDirectory {
  ErrorDirectoryHeader header;
  std::vector<ErrorDirectoryEntry> entries;
};

zx::result<ErrorHistoryDirectory> ReadErrorDirectory(
    const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client, std::ostream& err) {
  zx::vmo vmo;
  if (zx_status_t status = zx::vmo::create(kErrorHistoryModeDataLength, 0, &vmo); status != ZX_OK) {
    err << std::format("error: failed to create VMO: {}\n", zx_status_get_string(status));
    return zx::error(status);
  }

  zx::vmo dup;
  if (zx_status_t status = vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup); status != ZX_OK) {
    err << std::format("error: failed to duplicate VMO: {}\n", zx_status_get_string(status));
    return zx::error(status);
  }

  const fidl::WireResult result =
      client->ReadBuffer(0, fuchsia_hardware_scsi::wire::ReadBufferMode::kErrorHistory, 0, 0,
                         kErrorHistoryModeDataLength, std::move(dup));
  if (zx_status_t status = ReadBufferStatus(result); status != ZX_OK) {
    err << std::format("error: failed to read error history directory: {}\n",
                       zx_status_get_string(status));
    return zx::error(status);
  }
  std::vector<uint8_t> buffer(kErrorHistoryModeDataLength);
  if (zx_status_t status = vmo.read(buffer.data(), 0, buffer.size()); status != ZX_OK) {
    err << std::format("error: failed to read VMO data: {}\n", zx_status_get_string(status));
    return zx::error(status);
  }

  ErrorHistoryDirectory dir;
  std::memcpy(&dir.header, buffer.data(), sizeof(ErrorDirectoryHeader));
  uint16_t length = be16toh(dir.header.directory_length);
  // Validate the length
  if (!length || length % sizeof(ErrorDirectoryEntry)) {
    err << std::format("error: illegal directory length 0x{:x}\n", length);
    return zx::error(ZX_ERR_IO);
  }

  size_t entry_count = length / sizeof(ErrorDirectoryEntry);
  // Validate the entry count
  if (entry_count > kMaxErrorHistoryEntries) {
    err << std::format("error: illegal buffer count {}\n", entry_count);
    return zx::error(ZX_ERR_IO);
  }

  dir.entries.resize(entry_count);
  std::memcpy(dir.entries.data(), buffer.data() + sizeof(ErrorDirectoryHeader),
              entry_count * sizeof(ErrorDirectoryEntry));

  return zx::ok(std::move(dir));
}

void PrintErrorDirectorySummary(std::ostream& out, const ErrorDirectoryHeader& header,
                                cpp20::span<const ErrorDirectoryEntry> entries) {
  out << "Vendor ID  : ";
  for (uint8_t c : header.vendor_id) {
    out << static_cast<char>(c);
  }
  out << std::format("\nVersion    : {}\n", header.version);
  out << std::format("Entry Count: {}\n\n", entries.size());

  for (size_t i = 0; i < entries.size(); ++i) {
    out << std::format("Entry #{} | Buffer ID: 0x{:02X} | Length: {} bytes\n", i,
                       entries[i].buffer_id, be32toh(entries[i].length));
  }
}

// Retrieve actual error buffers and save to files
zx::result<> RetrieveErrorHistoryData(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                      cpp20::span<const ErrorDirectoryEntry> entries,
                                      const std::string& dir_path, std::ostream& err) {
  for (size_t i = 0; i < entries.size(); ++i) {
    const ErrorDirectoryEntry& entry = entries[i];
    uint32_t entry_length = be32toh(entry.length);
    if (entry_length == 0 || entry_length > kMaxEntryLength || entry.buffer_id < kMinBufferID ||
        entry.buffer_id > kMaxBufferID) {
      err << std::format("error: illegal entry #{} (buffer_id=0x{:02X} length={})\n", i,
                         entry.buffer_id, entry_length);
      return zx::error(ZX_ERR_IO);
    }

    fzl::OwnedVmoMapper mapper;
    if (zx_status_t status = mapper.CreateAndMap(entry_length, "error-history-entry");
        status != ZX_OK) {
      err << std::format("error: failed to create error history entry buffer: {}\n",
                         zx_status_get_string(status));
      return zx::error(status);
    }

    uint32_t offset = 0;
    while (offset < entry_length) {
      const uint32_t chunk_size = std::min(kMaxTransferSize, entry_length - offset);
      // ReadBuffer lengths must be a multiple of 4 (PRDT byte counts are dword granular).
      const uint32_t transfer_length = (chunk_size + 3u) & ~3u;

      zx::vmo data_vmo;
      if (zx_status_t status =
              mapper.vmo().create_child(ZX_VMO_CHILD_SLICE, offset, transfer_length, &data_vmo);
          status != ZX_OK) {
        err << std::format("error: failed to create VMO slice: {}\n", zx_status_get_string(status));
        return zx::error(status);
      }

      // Send the ReadBuffer request for this chunk
      const fidl::WireResult result =
          client->ReadBuffer(0, fuchsia_hardware_scsi::wire::ReadBufferMode::kErrorHistory,
                             entry.buffer_id, offset, transfer_length, std::move(data_vmo));
      if (zx_status_t status = ReadBufferStatus(result); status != ZX_OK) {
        err << std::format(
            "error: failed to read buffer (buffer_id=0x{:02X} offset={} size={}): {}\n",
            entry.buffer_id, offset, transfer_length, zx_status_get_string(status));
        return zx::error(status);
      }

      offset += chunk_size;
    }

    std::string file_path = files::AbsolutePath(
        files::JoinPath(dir_path, std::format("ufsutil_error_entry_{:02X}.bin", entry.buffer_id)));
    if (!files::WriteFile(file_path, reinterpret_cast<const char*>(mapper.start()), entry_length)) {
      err << std::format("error: failed to write {}\n", file_path);
      return zx::error(ZX_ERR_IO);
    }
  }
  return zx::ok();
}
}  // namespace

zx::result<> HandleErrorHistory(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                const std::unordered_map<uint32_t, OptionValue>& options,
                                std::ostream& out, std::ostream& err) {
  if (!CheckRequiredOption(options, 'o', "out-dir", err)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  const std::string& out_dir = std::get<std::string>(options.at('o'));
  // Fail before reading anything from the device.
  if (!files::IsDirectory(out_dir)) {
    err << std::format("error: output directory {} does not exist\n", out_dir);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  zx::result<ErrorHistoryDirectory> dir = ReadErrorDirectory(client, err);
  if (dir.is_error()) {
    return dir.take_error();
  }

  if (zx::result<> status = RetrieveErrorHistoryData(client, dir->entries, out_dir, err);
      status.is_error()) {
    return status;
  }

  PrintErrorDirectorySummary(out, dir->header, dir->entries);
  return zx::ok();
}
}  // namespace ufsutil
